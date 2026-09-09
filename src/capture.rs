//! Persistent PipeWire capture. Only the newest owned RGBA frame is retained.
//!
//! `wait_frame(None, ..)` allows an existing frame; `Some(sequence)` requests
//! a newer sample. Timeout returns cached pixels with an explicit unmet flag;
//! terminal failures and cancellation never return a successful cached frame.

use std::{
    os::fd::{AsRawFd, OwnedFd},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use gstreamer::{self as gst, prelude::*};
use gstreamer_app as gst_app;
use gstreamer_video::{self as gst_video, prelude::*};
use serde::{Deserialize, Serialize};

// Coalesce surplus source buffers, but always deliver the final changed buffer.
const MAX_CAPTURE_FPS: u32 = 15;
const CANCEL_SLICE: Duration = Duration::from_millis(50);

fn capture_queue() -> Result<gst::Element> {
    gst::ElementFactory::make("queue")
        .property("max-size-buffers", 1u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .property_from_str("leaky", "downstream")
        .build()
        .context("create latest-buffer capture queue")
}

fn capture_pacer() -> Result<gst::Element> {
    gst::ElementFactory::make("identity")
        .property("sleep-time", 1_000_000u32 / MAX_CAPTURE_FPS)
        .build()
        .context("create capture pacer")
}

fn capture_caps() -> gst::Caps {
    gst::Caps::builder("video/x-raw")
        .field("format", "RGBA")
        .build()
}

#[derive(Debug)]
pub struct FrameWait {
    pub frame: CapturedFrame,
    pub timed_out: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct CaptureSnapshot {
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug)]
pub struct CapturedFrame {
    pub sequence: u64,
    /// Unix epoch milliseconds when this process received the sample, not when
    /// an application rendered it. Sequence is the ordering authority.
    pub captured_at_ms: u64,
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA, top row first, without row padding.
    pub rgba: Vec<u8>,
}

#[derive(Default)]
struct State {
    latest: Option<CapturedFrame>,
    failure: Option<String>,
    source_buffers: u64,
    source_seen: Option<Instant>,
    published_at: Option<Instant>,
    source_pts_ns: Option<u64>,
    source_caps: Option<String>,
}

/// Bounded metadata only: no pixels, window names or per-frame log history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureDiagnostics {
    pub source_buffers: u64,
    pub published_frames: u64,
    pub source_age_ms: Option<u64>,
    pub published_age_ms: Option<u64>,
    pub source_pts_ns: Option<u64>,
    pub source_caps: Option<String>,
    pub queued_buffers: u32,
    pub copies_producer_buffers: bool,
    pub failure: Option<String>,
}

#[derive(Default)]
struct Frames {
    state: Mutex<State>,
    changed: Condvar,
}

impl Frames {
    fn fail(&self, message: String) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Preserve the original error, not the downstream flow-error it causes.
        if state.failure.is_none() {
            state.failure = Some(message);
        }
        self.changed.notify_all();
    }

    fn publish(&self, mut frame: CapturedFrame) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(state.failure.is_none(), "capture has stopped");
        frame.sequence = state
            .latest
            .as_ref()
            .map_or(Some(1), |previous| previous.sequence.checked_add(1))
            .context("capture sequence exhausted")?;
        state.latest = Some(frame);
        state.published_at = Some(Instant::now());
        self.changed.notify_all();
        Ok(())
    }

    fn source_received(&self, pts_ns: Option<u64>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.source_buffers = state.source_buffers.saturating_add(1);
        state.source_seen = Some(Instant::now());
        state.source_pts_ns = pts_ns;
    }

    fn diagnostics(&self, queued_buffers: u32) -> CaptureDiagnostics {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let age = |at: Option<Instant>| at.map(|at| at.elapsed().as_millis() as u64);
        CaptureDiagnostics {
            source_buffers: state.source_buffers,
            published_frames: state.latest.as_ref().map_or(0, |f| f.sequence),
            source_age_ms: age(state.source_seen),
            published_age_ms: age(state.published_at),
            source_pts_ns: state.source_pts_ns,
            source_caps: state.source_caps.clone(),
            queued_buffers,
            copies_producer_buffers: true,
            failure: state.failure.clone(),
        }
    }

    fn snapshot(&self) -> Result<Option<CaptureSnapshot>> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(failure) = &state.failure {
            bail!("capture failed: {failure}");
        }
        Ok(state.latest.as_ref().map(|f| CaptureSnapshot {
            sequence: f.sequence,
            width: f.width,
            height: f.height,
        }))
    }

    fn wait(
        &self,
        after: Option<u64>,
        timeout: Duration,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<FrameWait> {
        let started = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            ensure!(!cancel.is_cancelled(), "Capture wait cancelled");
            if let Some(failure) = &state.failure {
                bail!("capture failed: {failure}");
            }
            if let Some(frame) = state
                .latest
                .as_ref()
                .filter(|f| after.is_none_or(|s| f.sequence > s))
            {
                return Ok(FrameWait {
                    frame: frame.clone(),
                    timed_out: false,
                });
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return state
                    .latest
                    .as_ref()
                    .map(|frame| FrameWait {
                        frame: frame.clone(),
                        timed_out: true,
                    })
                    .context("Timed out: no capture frame available");
            }
            (state, _) = self
                .changed
                .wait_timeout(state, remaining.min(CANCEL_SLICE))
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

pub struct Capture {
    pipeline: gst::Pipeline,
    queue: gst::Element,
    frames: Arc<Frames>,
    // pipewiresrc borrows this descriptor and duplicates it when connecting.
    // Keep our copy alive through the pipeline's transition to NULL.
    _fd: OwnedFd,
}

impl Capture {
    pub fn start(fd: OwnedFd, node_id: u32) -> Result<Self> {
        gst::init().context("initialize GStreamer")?;
        let source = gst::ElementFactory::make("pipewiresrc")
            .property("fd", fd.as_raw_fd())
            // Portal returns a node ID, not the object serial target-object uses.
            .property("path", node_id.to_string())
            // Return scarce compositor buffers before queue/pacer backpressure.
            // Older KWin drops a final repaint when its pool is exhausted;
            // copying here trades memory bandwidth for reliable source progress.
            .property("use-bufferpool", false)
            .build()
            .context("create pipewiresrc (install the GStreamer PipeWire plugin)")?;
        // Newer PipeWire plugins can explicitly report remote disconnection.
        // Older ones still report errors/EOS through the bus and appsink.
        if source.find_property("on-disconnect").is_some() {
            source.set_property_from_str("on-disconnect", "error");
        }
        // A leaky queue coalesces to the newest source buffer while pacing applies
        // backpressure before conversion. Unlike drop-only videorate, a trailing
        // update progresses even if no later source frame arrives.
        let queue = capture_queue()?;
        let pacer = capture_pacer()?;
        let convert = gst::ElementFactory::make("videoconvert")
            .build()
            .context("create videoconvert")?;
        let sink = gst_app::AppSink::builder()
            .caps(&capture_caps())
            .max_buffers(1)
            .drop(true)
            .sync(false)
            .enable_last_sample(false)
            .wait_on_eos(false)
            .build();
        let pipeline = gst::Pipeline::new();
        pipeline.add_many([&source, &queue, &pacer, &convert, sink.upcast_ref()])?;
        gst::Element::link_many([&source, &queue, &pacer, &convert, sink.upcast_ref()])
            .context("link PipeWire RGBA capture pipeline")?;

        let frames = Arc::new(Frames::default());
        let source_frames = frames.clone();
        source
            .static_pad("src")
            .context("PipeWire source has no output pad")?
            .add_probe(
                gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
                move |_, info| {
                    if let Some(buffer) = info.buffer() {
                        source_frames.source_received(buffer.pts().map(|pts| pts.nseconds()));
                    }
                    if let Some(event) = info.event()
                        && let gst::EventView::Caps(caps) = event.view()
                    {
                        source_frames
                            .state
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .source_caps = Some(caps.caps().to_string());
                    }
                    gst::PadProbeReturn::Ok
                },
            );
        let sample_frames = frames.clone();
        let eos_frames = frames.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let result = (|| -> Result<()> {
                        let sample = sink.pull_sample().context("pull capture sample")?;
                        sample_frames.publish(decode_sample(&sample)?)
                    })();
                    match result {
                        Ok(()) => Ok(gst::FlowSuccess::Ok),
                        Err(error) => {
                            sample_frames.fail(format!("{error:#}"));
                            Err(gst::FlowError::Error)
                        }
                    }
                })
                .eos(move |_| eos_frames.fail("PipeWire stream ended".into()))
                .build(),
        );

        let bus_frames = frames.clone();
        pipeline
            .bus()
            .context("capture pipeline has no bus")?
            .set_sync_handler(move |_, message| {
                match message.view() {
                    gst::MessageView::Error(error) => bus_frames.fail(format!(
                        "{}: {} ({:?})",
                        error
                            .src()
                            .map(|s| s.path_string().to_string())
                            .unwrap_or_default(),
                        error.error(),
                        error.debug(),
                    )),
                    gst::MessageView::Eos(_) => bus_frames.fail("PipeWire stream ended".into()),
                    _ => {}
                }
                // No GLib event loop or bus thread is needed, and messages must
                // not accumulate in an unread asynchronous bus queue.
                gst::BusSyncReply::Drop
            });
        let capture = Self {
            pipeline,
            queue,
            frames,
            _fd: fd,
        };
        capture
            .pipeline
            .set_state(gst::State::Playing)
            .context("start PipeWire capture pipeline")?;
        Ok(capture)
    }

    /// Blocks until a matching frame, terminal stream error, or timeout.
    /// Run on a blocking worker, not an async executor thread. A newer sample
    /// does not prove that an earlier input action has finished rendering.
    pub fn wait_frame(
        &self,
        after_sequence: Option<u64>,
        timeout: Duration,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<FrameWait> {
        self.frames.wait(after_sequence, timeout, cancel)
    }

    pub fn diagnostics(&self) -> CaptureDiagnostics {
        self.frames
            .diagnostics(self.queue.property::<u32>("current-level-buffers"))
    }

    /// Reads metadata only; never clones full-screen pixel storage.
    pub fn snapshot(&self) -> Result<Option<CaptureSnapshot>> {
        self.frames.snapshot()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.frames.fail("capture stopped".into());
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(bus) = self.pipeline.bus() {
            bus.unset_sync_handler();
        }
    }
}

fn decode_sample(sample: &gst::Sample) -> Result<CapturedFrame> {
    let captured_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?
        .as_millis()
        .try_into()
        .context("capture timestamp overflow")?;
    let caps = sample.caps().context("capture sample has no caps")?;
    let info = gst_video::VideoInfo::from_caps(caps).context("read capture video caps")?;
    ensure!(
        info.format() == gst_video::VideoFormat::Rgba,
        "capture sample is not RGBA"
    );
    let buffer = sample.buffer().context("capture sample has no buffer")?;
    // VideoFrame honors per-buffer VideoMeta offsets/strides, unlike mapping
    // the whole buffer and assuming the caps' default packed layout.
    let mapped = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
        .context("map capture video frame")?;
    let rgba = if mapped.plane_stride()[0] < 0 {
        // The Rust plane_data API assumes positive strides. Let GStreamer
        // normalize bottom-up frames before exposing their memory as a slice.
        let packed_info = gst_video::VideoInfo::builder(
            gst_video::VideoFormat::Rgba,
            mapped.width(),
            mapped.height(),
        )
        .build()
        .context("create normalized RGBA layout")?;
        let mut packed_buffer = gst::Buffer::with_size(packed_info.size())?;
        {
            let mut target = gst_video::VideoFrameRef::from_buffer_ref_writable(
                packed_buffer
                    .get_mut()
                    .context("normalized buffer is shared")?,
                &packed_info,
            )?;
            mapped
                .copy(&mut target)
                .context("normalize bottom-up RGBA frame")?;
        }
        let packed = packed_buffer.map_readable()?;
        copy_rgba_rows(
            packed.as_slice(),
            mapped.width(),
            mapped.height(),
            packed_info.stride()[0],
        )?
    } else {
        // gstreamer-video 0.24 calculates plane_data length using u32 math.
        // Reject an unrepresentable slice before calling that API.
        (mapped.plane_stride()[0] as u32)
            .checked_mul(mapped.height())
            .context("RGBA plane size exceeds the GStreamer slice API")?;
        copy_rgba_rows(
            mapped.plane_data(0)?,
            mapped.width(),
            mapped.height(),
            mapped.plane_stride()[0],
        )?
    };
    Ok(CapturedFrame {
        sequence: 0,
        captured_at_ms,
        width: mapped.width(),
        height: mapped.height(),
        rgba,
    })
}

fn copy_rgba_rows(data: &[u8], width: u32, height: u32, stride: i32) -> Result<Vec<u8>> {
    ensure!(width > 0 && height > 0, "capture has empty dimensions");
    let row = (width as usize)
        .checked_mul(4)
        .context("RGBA row size overflow")?;
    let stride = usize::try_from(stride).context("negative RGBA stride is unsupported")?;
    ensure!(stride >= row, "RGBA stride is smaller than its pixel row");
    let height = height as usize;
    let required = (height - 1)
        .checked_mul(stride)
        .and_then(|n| n.checked_add(row))
        .context("RGBA input size overflow")?;
    ensure!(
        data.len() >= required,
        "RGBA buffer is shorter than its dimensions and stride"
    );
    let size = row
        .checked_mul(height)
        .context("RGBA output size overflow")?;
    let mut packed = Vec::new();
    packed
        .try_reserve_exact(size)
        .map_err(|e| anyhow!("allocate RGBA frame: {e}"))?;
    for y in 0..height {
        packed.extend_from_slice(&data[y * stride..y * stride + row]);
    }
    Ok(packed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn frame() -> CapturedFrame {
        CapturedFrame {
            sequence: 0,
            captured_at_ms: 42,
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 4],
        }
    }

    #[test]
    fn coalescing_preserves_trailing_update_without_eos_or_duplicates() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let source = gst_app::AppSrc::builder()
            .format(gst::Format::Time)
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "RGBA")
                    .field("width", 1i32)
                    .field("height", 1i32)
                    .field("framerate", gst::Fraction::new(0, 1))
                    .build(),
            )
            .build();
        let queue = capture_queue().unwrap();
        let pacer = capture_pacer().unwrap();
        let convert = gst::ElementFactory::make("videoconvert").build().unwrap();
        let sink = gst_app::AppSink::builder()
            .caps(&capture_caps())
            .sync(false)
            .wait_on_eos(false)
            .build();
        pipeline
            .add_many([
                source.upcast_ref(),
                &queue,
                &pacer,
                &convert,
                sink.upcast_ref(),
            ])
            .unwrap();
        gst::Element::link_many([
            source.upcast_ref(),
            &queue,
            &pacer,
            &convert,
            sink.upcast_ref(),
        ])
        .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let push = |n: u8| {
            let mut buffer = gst::Buffer::from_mut_slice(vec![n, 0, 0, 255]);
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_mseconds(n as u64));
            source.push_buffer(buffer).unwrap();
        };
        push(0);
        assert!(sink.try_pull_sample(gst::ClockTime::SECOND).is_some());
        let start = Instant::now();
        for n in 1..=120 {
            push(n);
        }
        let mut outputs = 0;
        loop {
            let sample = sink
                .try_pull_sample(gst::ClockTime::SECOND)
                .expect("final update lost");
            outputs += 1;
            if decode_sample(&sample).unwrap().rgba[0] == 120 {
                break;
            }
            assert!(outputs < 10, "queue must coalesce the burst");
        }
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(queue.property::<u32>("max-size-buffers"), 1);
        assert!(
            sink.try_pull_sample(gst::ClockTime::from_mseconds(150))
                .is_none(),
            "no synthetic idle frames"
        );
        push(121);
        assert_eq!(
            decode_sample(&sink.try_pull_sample(gst::ClockTime::SECOND).unwrap())
                .unwrap()
                .rgba[0],
            121
        );
        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn copying_releases_finite_producer_pool_before_consumer_finishes() {
        gst::init().unwrap();
        let pool = gst::BufferPool::new();
        let mut config = pool.config();
        config.set_params(None, 4, 0, 1);
        pool.set_config(config).unwrap();
        pool.set_active(true).unwrap();
        let params =
            gst::BufferPoolAcquireParams::with_flags(gst::BufferPoolAcquireFlags::DONTWAIT);
        let mut borrowed = pool.acquire_buffer(Some(&params)).unwrap();
        borrowed
            .get_mut()
            .unwrap()
            .map_writable()
            .unwrap()
            .as_mut_slice()
            .copy_from_slice(&[1, 2, 3, 4]);
        assert!(
            pool.acquire_buffer(Some(&params)).is_err(),
            "a downstream borrowed buffer starves this producer"
        );
        let owned = borrowed.copy_deep().unwrap();
        drop(borrowed);
        // The consumer may keep its owned image throughout pacing/idle without
        // preventing the producer from recording the final repaint.
        let mut next = pool.acquire_buffer(Some(&params)).unwrap();
        next.get_mut()
            .unwrap()
            .map_writable()
            .unwrap()
            .as_mut_slice()
            .copy_from_slice(&[5, 6, 7, 8]);
        assert_eq!(owned.map_readable().unwrap().as_slice(), &[1, 2, 3, 4]);
        drop(next);
        drop(owned);
        pool.set_active(false).unwrap();
    }

    #[test]
    fn diagnostics_distinguish_source_progress_from_published_frames() {
        let frames = Frames::default();
        assert_eq!(frames.diagnostics(0).source_age_ms, None);
        for _ in 0..3 {
            frames.source_received(Some(7));
        }
        frames.publish(frame()).unwrap();
        let diagnostics = frames.diagnostics(1);
        assert_eq!(diagnostics.source_buffers, 3);
        assert_eq!(diagnostics.published_frames, 1);
        assert_eq!(diagnostics.source_pts_ns, Some(7));
        assert!(diagnostics.source_age_ms.is_some());
        assert!(diagnostics.published_age_ms.is_some());
        assert_eq!(diagnostics.queued_buffers, 1);
        frames.fail("source stopped".into());
        assert_eq!(
            frames.diagnostics(0).failure.as_deref(),
            Some("source stopped")
        );
    }

    #[test]
    fn copies_padded_rows_without_requiring_final_padding() {
        assert_eq!(
            copy_rgba_rows(&[1, 2, 3, 4, 99, 99, 99, 99, 5, 6, 7, 8], 1, 2, 8).unwrap(),
            vec![1, 2, 3, 4, 5, 6, 7, 8]
        );
    }

    #[test]
    fn sample_decode_honors_video_meta_offsets_and_signed_stride() {
        gst::init().unwrap();
        let caps = gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgba, 1, 2)
            .build()
            .unwrap()
            .to_caps()
            .unwrap();
        for (offset, stride, expected) in [
            (4, 8, vec![1, 2, 3, 4, 5, 6, 7, 8]),
            (12, -8, vec![5, 6, 7, 8, 1, 2, 3, 4]),
        ] {
            let mut buffer = gst::Buffer::from_mut_slice(vec![
                99, 99, 99, 99, 1, 2, 3, 4, 99, 99, 99, 99, 5, 6, 7, 8, 99, 99, 99, 99,
            ]);
            gst_video::VideoMeta::add_full(
                buffer.get_mut().unwrap(),
                gst_video::VideoFrameFlags::empty(),
                gst_video::VideoFormat::Rgba,
                1,
                2,
                &[offset],
                &[stride],
            )
            .unwrap();
            let sample = gst::Sample::builder().buffer(&buffer).caps(&caps).build();
            let decoded = decode_sample(&sample).unwrap();
            assert_eq!(decoded.rgba, expected);
            assert_eq!((decoded.width, decoded.height), (1, 2));
        }
    }

    #[test]
    fn copies_packed_rows() {
        assert_eq!(
            copy_rgba_rows(&[1, 2, 3, 4, 5, 6, 7, 8], 2, 1, 8).unwrap(),
            vec![1, 2, 3, 4, 5, 6, 7, 8]
        );
    }

    #[test]
    fn rejects_invalid_layouts() {
        for (width, height, stride) in [(0, 1, 4), (1, 0, 4), (2, 1, 4), (1, 1, -4), (1, 2, 8)] {
            assert!(copy_rgba_rows(&[0; 8], width, height, stride).is_err());
        }
    }

    #[test]
    fn latest_is_bounded_and_strict_next_does_not_return_stale_data() {
        let frames = Frames::default();
        frames.publish(frame()).unwrap();
        let first = frames
            .wait(None, Duration::ZERO, &CancellationToken::new())
            .unwrap();
        assert_eq!(first.frame.sequence, 1);
        assert!(
            frames
                .wait(
                    Some(first.frame.sequence),
                    Duration::ZERO,
                    &CancellationToken::new()
                )
                .unwrap()
                .timed_out
        );
        frames.publish(frame()).unwrap();
        assert_eq!(
            frames
                .wait(
                    Some(first.frame.sequence),
                    Duration::ZERO,
                    &CancellationToken::new()
                )
                .unwrap()
                .frame
                .sequence,
            2
        );
    }

    #[test]
    fn errors_take_precedence_over_cached_frames() {
        let frames = Frames::default();
        frames.publish(frame()).unwrap();
        frames.fail("remote disconnected".into());
        frames.fail("secondary failure".into());
        assert!(
            frames
                .wait(None, Duration::ZERO, &CancellationToken::new())
                .unwrap_err()
                .to_string()
                .contains("remote disconnected")
        );
    }

    #[test]
    fn waiting_is_woken_by_frames_and_errors() {
        for fail in [false, true] {
            let frames = Arc::new(Frames::default());
            let waiter = frames.clone();
            let thread = std::thread::spawn(move || {
                waiter.wait(None, Duration::from_secs(2), &CancellationToken::new())
            });
            if fail {
                frames.fail("disconnected".into());
            } else {
                frames.publish(frame()).unwrap();
            }
            assert_eq!(thread.join().unwrap().is_err(), fail);
        }
    }

    #[test]
    fn cancellation_interrupts_long_wait_even_with_cached_pixels() {
        let frames = Arc::new(Frames::default());
        frames.publish(frame()).unwrap();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let thread =
            std::thread::spawn(move || frames.wait(Some(1), Duration::from_secs(5), &token));
        std::thread::sleep(Duration::from_millis(20));
        let start = Instant::now();
        cancel.cancel();
        assert!(
            thread
                .join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        assert!(start.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn timeout_does_not_reset_on_notifications() {
        let frames = Arc::new(Frames::default());
        let notify = frames.clone();
        let thread = std::thread::spawn(move || {
            for _ in 0..40 {
                std::thread::sleep(Duration::from_millis(10));
                notify.changed.notify_all();
            }
        });
        let start = Instant::now();
        assert!(
            frames
                .wait(None, Duration::from_millis(25), &CancellationToken::new())
                .is_err()
        );
        assert!(start.elapsed() >= Duration::from_millis(25));
        assert!(start.elapsed() < Duration::from_millis(250));
        thread.join().unwrap();
    }
}
