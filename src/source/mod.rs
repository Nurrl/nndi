//! Everything related to NDI [`Source`]s, to send video.

use std::sync::{Arc, Weak};

use ffmpeg::codec;
use futures::{StreamExt, TryFutureExt};
use mdns_sd::{ServiceDaemon, ServiceInfo, UnregisterStatus};
use slab::Slab;
use tokio::{net::TcpListener, sync::RwLock};

use crate::{
    io::{
        frame::{text, video, Frame, FrameKind},
        Stream,
    },
    Error, Result,
};

mod config;
pub use config::Config;

mod peer;
pub use peer::Peer;

type Lock<T> = Arc<RwLock<T>>;
type WeakLock<T> = Weak<RwLock<T>>;

/// A _video_ and _audio_ source, that can send data to multiple sinks.
pub struct Source {
    name: String,
    mdns: ServiceDaemon,

    peers: Lock<Vec<WeakLock<Peer>>>,
    frames: flume::Sender<Frame>,
}

impl Source {
    /// Expose a new [`Source`] based on the provided `config` on the network.
    pub async fn new(config: Config) -> Result<Self> {
        let groups = config.groups.as_deref().unwrap_or(&["public"]).join(",");
        let listener = TcpListener::bind("[::]:0").await?;

        let mdns = ServiceDaemon::new()?;
        let service = ServiceInfo::new(
            super::SERVICE_TYPE,
            &crate::name(&config.name),
            &crate::hostname(),
            (),
            listener.local_addr()?.port(),
            [("groups", groups.as_str())].as_slice(),
        )?
        .enable_addr_auto();

        let name = service.get_fullname().into();
        mdns.register(service)?;

        tracing::debug!("Registered mDNS service `{}`", name);

        let peers = <Lock<Vec<WeakLock<Peer>>>>::default();
        let (frames, framesrx) = flume::bounded(1);

        tokio::spawn(
            Self::listen(listener, config, peers.clone(), framesrx)
                .inspect_err(|err| tracing::error!("Fatal error in `Source::listener`: {err}")),
        );

        Ok(Self {
            name,
            mdns,
            peers,
            frames,
        })
    }

    async fn listen(
        listener: tokio::net::TcpListener,
        config: Config,
        peers: Lock<Vec<WeakLock<Peer>>>,
        frames: flume::Receiver<Frame>,
    ) -> Result {
        let mut streams: Slab<(Lock<Peer>, Stream)> = Slab::with_capacity(32);

        loop {
            tokio::select! {
                // Accept new connections in the pool
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let mut stream = stream.into();

                    let peer = tokio::time::timeout(
                        crate::HANDSHAKE_TIMEOUT,
                        Peer::handshake(&mut stream, &config)
                    )
                    .await??;
                    let peer = Arc::from(RwLock::new(peer));

                    peers.write().await.push(Arc::downgrade(&peer));
                    streams.insert((peer, stream));
                }

                // Receive metadata from peers
                Some(mut entry) = async {
                    let mut readable = streams
                        .iter_mut()
                        .map(|(idx, entry)| async move { entry.1.readable().await.ok(); (idx, entry) })
                        .collect::<futures::stream::FuturesUnordered<_>>();

                    readable.next().await
                } => {
                    let (idx, (peer, stream)) = &mut entry;

                    match stream.metadata().await {
                        Ok(Some(text::Metadata::Tally(tally))) => {
                            peer.write().await.tally = tally;
                        }
                        Ok(other) => tracing::debug!("Ignored metadata from peer: {other:?}"),
                        Err(err) => {
                            tracing::error!("Peer handling failed: {err}");

                            streams.remove(*idx);
                        }
                    }
                }

                // Send frames to all peers
                Ok(frame) = frames.recv_async() => {
                    futures::future::join_all(
                        streams
                            .iter_mut()
                            .map(|(_, entry)| {
                                let frame = &frame;

                                async move {
                                    let (peer, stream) = entry;
                                    let peer = peer.read().await;

                                    if (peer.streams.text && matches!(frame, Frame::Text { .. }))
                                        || (peer.streams.video && matches!(frame, Frame::Video { .. }))
                                        || (peer.streams.audio && matches!(frame, Frame::Audio { .. })) {
                                        tracing::trace!("-> sending {:?} frame to `{}`", frame, peer.identify.name);

                                        drop(peer);
                                        stream.send(frame).await.ok();
                                    } else {
                                        tracing::trace!("-x-> skip sending {:?} frame to `{}`", FrameKind::from(frame), peer.identify.name);
                                    }
                                }
                            })
                    )
                    .await;
                }
            }
        }
    }

    /// List the peers currently connected to the [`Source`], with their parameters.
    pub async fn peers(&self) -> Vec<Peer> {
        let pointers: Vec<_> = self
            .peers
            .read()
            .await
            .iter()
            .filter_map(Weak::upgrade)
            .collect();

        let peers = futures::future::join_all(
            pointers
                .iter()
                .map(|peer| async { peer.read().await.clone() }),
        )
        .await;

        *self.peers.write().await = pointers.iter().map(Arc::downgrade).collect();

        peers
    }

    /// Get current _tally_ information computed from all the connected peers of the [`Source`].
    pub async fn tally(&self) -> text::Tally {
        self.peers()
            .await
            .into_iter()
            .fold(Default::default(), |current, peer| current | peer.tally)
    }

    /// Broadcast a [`ffmpeg::frame::Video`] to all the connected peers.
    pub async fn broadcast_video(
        &self,
        frame: &ffmpeg::frame::Video,
        framerate: ffmpeg::Rational,
    ) -> Result {
        assert!(
            frame.width() % 16 == 0,
            "SpeedHQ frame width must be a multiple of 16, was `{}`",
            frame.width()
        );

        let mut converted = ffmpeg::frame::Video::new(
            ffmpeg::format::Pixel::YUV422P,
            frame.width(),
            frame.height(),
        );

        frame
            .converter(converted.format())?
            .run(frame, &mut converted)?;

        let mut context = codec::Context::new().encoder().video()?;
        context.set_time_base(framerate);
        context.set_format(converted.format());
        context.set_width(converted.width());
        context.set_height(converted.height());

        let mut encoder = context.open_as(codec::encoder::find(codec::Id::SPEEDHQ))?;
        encoder.send_frame(&converted)?;
        encoder.send_eof()?;

        let mut packet = ffmpeg::Packet::empty();
        encoder.receive_packet(&mut packet)?;

        self.frames
            .send_async(Frame::video(
                video::Spec {
                    fourcc: video::FourCCVideoType::SHQ2,
                    width: converted.width(),
                    height: converted.height(),
                    fps_num: framerate.numerator() as u32,
                    fps_den: framerate.denominator() as u32,
                    aspect_ratio: converted.width() as f32 / converted.height() as f32,
                    frame_format: video::FrameFormat::Progressive,
                    timestamp: chrono::Utc::now().into(),
                    ..Default::default()
                },
                packet.data().expect("No packet data ??").to_vec(),
            ))
            .await
            .map_err(|_| Error::ClosedChannel)?;

        Ok(())
    }

    /// Broadcast a [`ffmpeg::frame::Audio`] to all the connected peers.
    pub fn broadcast_audio(&self, frame: &ffmpeg::frame::Audio) -> Result {
        todo!("Broadcast an audio frame")
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        match self.mdns.unregister(&self.name).map(|recv| recv.recv()) {
            Err(err) => tracing::error!(
                "Error while unregistering service `{}` from mDNS: {err}",
                self.name
            ),
            Ok(Err(err)) => tracing::error!(
                "Error while unregistering service `{}` from mDNS: {err}",
                self.name
            ),
            Ok(Ok(err @ UnregisterStatus::NotFound)) => tracing::error!(
                "Error while unregistering service `{}` from mDNS: {err:?}",
                self.name
            ),

            _ => tracing::debug!("Unregistered mDNS service `{}`", self.name),
        }

        if let Err(err) = self.mdns.shutdown() {
            tracing::error!("Error while shutting down the mDNS advertisement thread: {err}");
        }
    }
}
