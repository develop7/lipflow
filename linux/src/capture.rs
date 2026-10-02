use crate::{
    config::Paths,
    engine::{Event, Record, Tracker},
    worker::Command,
};
use anyhow::{bail, Context, Result};
use gst::prelude::*;
use std::{
    os::fd::{AsRawFd, OwnedFd},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, Sender},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Active {
    pub token: u64,
    pub record: Record,
    pub started: Instant,
    pub practice: Option<String>,
}
#[derive(Default)]
pub struct CaptureState {
    pub generation: AtomicU64,
    pub active: Mutex<Option<Active>>,
    pub practice: AtomicBool,
    pub preview_enabled: AtomicBool,
    pub frame_pending: AtomicBool,
    pub preview_pending: AtomicBool,
}
pub enum Control {
    Open(OwnedFd, String),
    Close,
}

pub struct Stream {
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
    _fd: OwnedFd,
}
impl Stream {
    pub fn new(fd: OwnedFd, target: &str) -> Result<Self> {
        gst::init()?;
        let pipeline = gst::Pipeline::new();
        let source = gst::ElementFactory::make("pipewiresrc")
            .property("fd", fd.as_raw_fd())
            .property("do-timestamp", true)
            .build()
            .context("The GStreamer PipeWire camera plugin is missing")?;
        if !target.is_empty() && target != "auto" {
            source.set_property("target-object", target);
        }
        let convert = gst::ElementFactory::make("videoconvert").build()?;
        let sink = gst_app::AppSink::builder()
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "BGR")
                    .build(),
            )
            .max_buffers(2)
            .drop(true)
            .sync(false)
            .build();
        pipeline.add_many([&source, &convert, sink.upcast_ref()])?;
        gst::Element::link_many([&source, &convert, sink.upcast_ref()])?;
        let stream = Self {
            pipeline,
            sink,
            _fd: fd,
        };
        stream.pipeline.set_state(gst::State::Playing)?;
        Ok(stream)
    }
    pub fn frame(&self) -> Result<Option<(Vec<u8>, usize, usize)>> {
        let Some(sample) = self
            .sink
            .try_pull_sample(gst::ClockTime::from_mseconds(100))
        else {
            if let Some(message) = self
                .pipeline
                .bus()
                .unwrap()
                .pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos])
            {
                if let gst::MessageView::Error(error) = message.view() {
                    bail!("Camera stream failed: {}", error.error());
                }
                bail!("The camera stream ended");
            }
            return Ok(None);
        };
        let info = gst_video::VideoInfo::from_caps(sample.caps().context("No camera format")?)?;
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(
            sample.buffer().context("No camera buffer")?,
            &info,
        )?;
        let data = frame.plane_data(0)?;
        let width = info.width() as usize;
        let height = info.height() as usize;
        let stride = info.stride()[0] as usize;
        Ok(Some((
            pack_bgr(data, width, height, stride)?,
            width,
            height,
        )))
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

pub fn pack_bgr(data: &[u8], width: usize, height: usize, stride: usize) -> Result<Vec<u8>> {
    let row = width.checked_mul(3).context("Invalid camera width")?;
    if stride < row
        || height == 0
        || data.len()
            < (height - 1)
                .checked_mul(stride)
                .context("Invalid frame size")?
                + row
    {
        bail!("Invalid padded camera frame");
    }
    let mut output = Vec::with_capacity(row * height);
    for y in 0..height {
        output.extend_from_slice(&data[y * stride..y * stride + row]);
    }
    Ok(output)
}

pub fn run(
    paths: Paths,
    state: Arc<CaptureState>,
    controls: Receiver<Control>,
    model: Sender<Command>,
    events: Sender<Event>,
) {
    let mut stream = None;
    let mut tracker = None;
    let mut last_used = Instant::now();
    let mut last_preview = Instant::now();
    let mut first_frame = None;
    let mut maximum_sent = None;
    loop {
        let command = if stream.is_none() {
            controls.recv_timeout(Duration::from_millis(100)).ok()
        } else {
            controls.try_recv().ok()
        };
        if let Some(command) = command {
            match command {
                Control::Close => return,
                Control::Open(fd, target) => {
                    last_used = Instant::now();
                    if stream.is_none() {
                        let result = Stream::new(fd, &target).and_then(|capture| {
                            if tracker.is_none() {
                                tracker = Some(Tracker::new(&paths)?);
                            }
                            Ok(capture)
                        });
                        match result {
                            Ok(capture) => {
                                stream = Some(capture);
                                first_frame = Some(Instant::now());
                            }
                            Err(error) => {
                                let _ = events.send(Event::Error(format!("Camera: {error:#}")));
                                state.active.lock().unwrap().take();
                            }
                        }
                    }
                }
            }
        }
        let Some(capture) = &stream else {
            continue;
        };
        let active = state.active.lock().unwrap().clone();
        let practice = state.practice.load(Ordering::Relaxed);
        if active.is_some() || practice {
            last_used = Instant::now();
        } else if last_used.elapsed() > Duration::from_secs(45) {
            stream = None;
            continue;
        }
        match capture.frame() {
            Ok(Some((data, width, height))) => {
                first_frame = None;
                let thumbnail = (active.is_some() || practice)
                    && !state.frame_pending.swap(true, Ordering::Relaxed);
                let result = tracker.as_ref().unwrap().frame(
                    &data,
                    width,
                    height,
                    active.as_ref().map(|a| &a.record),
                    thumbnail,
                );
                match result {
                    Ok(Some(rgb)) => {
                        let _ = events.send(Event::Frame(rgb, 320, 200));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        state.frame_pending.store(false, Ordering::Relaxed);
                        let _ = events.send(Event::Error(format!("Camera tracking: {error:#}")));
                        stream = None;
                    }
                }
                if let Some(active) = active {
                    if active.started.elapsed() >= Duration::from_secs(60)
                        && maximum_sent != Some(active.token)
                    {
                        maximum_sent = Some(active.token);
                        let _ = events.send(Event::Stopped(active.token));
                    }
                    if state.preview_enabled.load(Ordering::Relaxed)
                        && active.practice.is_none()
                        && last_preview.elapsed() > Duration::from_millis(450)
                        && !state.preview_pending.swap(true, Ordering::Relaxed)
                    {
                        last_preview = Instant::now();
                        let _ = model.send(Command::Preview(active.token, active.record));
                    }
                }
            }
            Ok(None) if first_frame.is_some_and(|at| at.elapsed() > Duration::from_secs(6)) => {
                let _ = events.send(Event::Error(
                    "The camera is not sending frames. Check the camera and reconnect permissions."
                        .into(),
                ));
                stream = None;
                state.active.lock().unwrap().take();
            }
            Ok(None) => {}
            Err(error) => {
                let _ = events.send(Event::Error(format!("Camera: {error:#}")));
                stream = None;
                state.active.lock().unwrap().take();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn padded_camera_rows_are_copied_without_padding_or_overreads() {
        let rows = [1, 2, 3, 4, 5, 6, 99, 99, 7, 8, 9, 10, 11, 12, 99, 99];
        assert_eq!(
            pack_bgr(&rows, 2, 2, 8).unwrap(),
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
        assert!(pack_bgr(&rows[..9], 2, 2, 8).is_err());
        assert!(pack_bgr(&rows, 2, 2, 5).is_err());
    }
}
