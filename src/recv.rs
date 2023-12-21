use std::{net::SocketAddr, thread};

use ffmpeg_next::codec;
use itertools::Itertools;
use mdns_sd::ServiceInfo;

use crate::{
    io::{
        frame::{
            audio,
            text::{self, Metadata},
            video, Frame,
        },
        Stream,
    },
    Result,
};

#[derive(Debug, Clone)]
pub struct Recv {
    video: flume::Receiver<video::Block>,
    audio: flume::Receiver<audio::Block>,
}

impl Recv {
    pub fn new(service: &ServiceInfo, queue: usize) -> Result<Self> {
        let port = service.get_port();
        let mut stream = Stream::connect(
            &*service
                .get_addresses()
                .iter()
                .map(|addr| SocketAddr::new(*addr, port))
                .collect::<Vec<_>>(),
        )?;

        tracing::debug!(
            "Connected to network source `{}@{}`",
            service.get_fullname(),
            stream.peer_addr()?
        );

        Self::identify(&mut stream)?;

        let (videotx, video) = flume::bounded(queue);
        let (audiotx, audio) = flume::bounded(queue);
        Self::task(stream, videotx, audiotx);

        Ok(Self { video, audio })
    }

    fn identify(stream: &mut Stream) -> Result<()> {
        stream.send(&Frame::Text(
            Metadata::Version(text::Version {
                video: 5,
                audio: 4,
                text: 3,
                sdk: crate::SDK_VERSION.into(),
                platform: crate::SDK_PLATFORM.into(),
            })
            .to_block()?,
        ))?;

        stream.send(&Frame::Text(
            Metadata::Identify(text::Identify {
                name: crate::name("receiver"),
            })
            .to_block()?,
        ))?;

        stream.send(&Frame::Text(
            Metadata::Video(text::Video {
                quality: text::VideoQuality::High,
            })
            .to_block()?,
        ))?;

        stream.send(&Frame::Text(
            Metadata::EnabledStreams(text::EnabledStreams {
                video: true,
                audio: true,
                text: true,
                shq_skip_block: true,
                shq_short_dc: true,
            })
            .to_block()?,
        ))?;

        Ok(())
    }

    fn task(
        mut stream: Stream,
        video: flume::Sender<video::Block>,
        audio: flume::Sender<audio::Block>,
    ) {
        let mut task = move || {
            loop {
                if video.is_disconnected() && audio.is_disconnected() {
                    tracing::trace!("All receivers dropped, disconnecting from peer");

                    break;
                }

                match stream.recv()? {
                    Frame::Video(block) => {
                        if let Err(err) = video.try_send(block) {
                            tracing::debug!("A video block was dropped: {err}");
                        }
                    }
                    Frame::Audio(block) => {
                        if let Err(err) = audio.try_send(block) {
                            tracing::debug!("An audio block was dropped: {err}");
                        }
                    }
                    Frame::Text(_) => {}
                }
            }

            Ok::<_, crate::Error>(())
        };

        thread::spawn(move || {
            if let Err(err) = task() {
                tracing::error!("Fatal error in the `Recv::task` thread: {err}");
            }
        });
    }

    /// Pop the next [`video::Block`] from the queue, if present.
    pub fn video(&self) -> Result<video::Block, flume::TryRecvError> {
        self.video.try_recv()
    }

    /// Iterate forever over the [`video::Block`] from the queue.
    pub fn iter_video(&self) -> impl Iterator<Item = Result<video::Block, flume::RecvError>> + '_ {
        std::iter::from_fn(move || Some(self.video.recv()))
    }

    //let codec = codec::decoder::find(codec::Id::SPEEDHQ)
    //    .expect("Unable to find the SpeedHQ decoder in the ffmpeg implementation");
    pub fn iter_video_frames(&self) -> Result<()> {
        let mut decoder = codec::Context::new().decoder().video()?;

        self.iter_video()
            .map_ok(|block| {
                decoder.send_packet(&codec::packet::Packet::borrow(&block.data));

                let mut frame = ffmpeg_next::util::frame::Video::empty();
                while decoder.receive_frame(&mut frame).is_ok() {
                    tracing::error!("FRAME @{:?}: {:?}", frame.timestamp(), frame.data(0));
                }
            })
            .collect::<Vec<_>>();

        Ok(())
    }

    /// Pop the next [`audio::Block`] from the queue, if present.
    pub fn audio(&self) -> Result<audio::Block, flume::TryRecvError> {
        self.audio.try_recv()
    }

    /// Iterate forever over the [`audio::Block`] from the queue.
    pub fn iter_audio(&self) -> impl Iterator<Item = Result<audio::Block, flume::RecvError>> + '_ {
        std::iter::from_fn(move || Some(self.audio.recv()))
    }
}
