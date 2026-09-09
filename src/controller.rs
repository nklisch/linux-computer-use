//! One actor owns input state. Cancelling an MCP request cannot abandon a held key.
use crate::{
    accessibility::Accessibility,
    capture::{Capture, CapturedFrame},
    geometry::{FrameMapping, checked_crop, output_size},
    input::{Gesture as Prepared, InputState},
    keys,
    portal::Portal,
    state::StateStore,
    types::*,
};
use anyhow::{Context, Result, ensure};
use std::{
    io::Cursor,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

type Response = Result<Reply>;
fn stop_report(result: Result<()>) -> StopReport {
    StopReport {
        controller_halted: true,
        desktop_close_confirmed: result.is_ok(),
        error: result
            .err()
            .map(|e| format!("Desktop closure not confirmed: {e:#}")),
    }
}
struct Envelope {
    command: Command,
    read_only: bool,
    cancel: CancellationToken,
    reply: oneshot::Sender<Response>,
}
struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Clone)]
pub struct ControllerHandle {
    tx: mpsc::Sender<Envelope>,
    stop: CancellationToken,
    completion: watch::Receiver<Option<StopReport>>,
}
impl ControllerHandle {
    pub fn spawn() -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel::<Envelope>(8);
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let (completed, completion) = watch::channel(None);
        let worker = tokio::spawn(async move {
            let mut controller = Controller::new();
            loop {
                let envelope = tokio::select! {
                    biased;
                    _=shutdown.cancelled()=>break,
                    item=rx.recv()=>match item { Some(v)=>v, None=>break },
                };
                if envelope.cancel.is_cancelled() {
                    let _ = envelope
                        .reply
                        .send(Err(anyhow::anyhow!("Request cancelled before input")));
                    continue;
                }
                // Observer requests share capture but never replace actionable references.
                let saved = envelope
                    .read_only
                    .then(|| (controller.latest.take(), controller.accessibility.take()));
                let result = controller
                    .dispatch(envelope.command, &envelope.cancel)
                    .await;
                if let Some((latest, accessibility)) = saved {
                    controller.latest = latest;
                    controller.accessibility = accessibility;
                }
                let _ = envelope.reply.send(result);
            }
            let report = stop_report(controller.close().await);
            if let Some(error) = &report.error {
                eprintln!("{error}");
            }
            let _ = completed.send(Some(report));
        });
        (
            Self {
                tx,
                stop,
                completion,
            },
            worker,
        )
    }
    pub async fn call(&self, command: Command) -> Response {
        self.call_view(command, false).await
    }
    pub async fn call_view(&self, command: Command, read_only: bool) -> Response {
        if matches!(command, Command::Stop) {
            self.stop.cancel();
            return Ok(Reply::Stopped(self.stopped().await));
        }
        ensure!(
            !self.stop.is_cancelled(),
            "Controller stopped; restart lcu to authorize another session"
        );
        let cancel = self.stop.child_token();
        let _guard = CancelOnDrop(cancel.clone());
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Envelope {
                command,
                read_only,
                cancel,
                reply,
            })
            .await
            .context("Controller has stopped")?;
        rx.await
            .context("Controller stopped before replying; input outcome may be unknown")?
    }
    pub fn shutdown(&self) {
        self.stop.cancel();
    }
    pub async fn stopped(&self) -> StopReport {
        self.tx.closed().await;
        self.completion.borrow().clone().unwrap_or(StopReport {
            controller_halted: true,
            desktop_close_confirmed: false,
            error: Some("Controller exited without confirming desktop closure".into()),
        })
    }
}

struct Desktop {
    portal: Portal,
    captures: Vec<Arc<Capture>>,
}
struct LatestObservation {
    mapping: FrameMapping,
    sequence: u64,
    captured_at_ms: u64,
    observed_at: Instant,
}

struct Controller {
    desktop: Option<Desktop>,
    accessibility: Option<Accessibility>,
    latest: Option<LatestObservation>,
    observation_number: u64,
    instance: String,
    input: InputState,
    warnings: Vec<String>,
    permission_saved: bool,
}
impl Controller {
    fn new() -> Self {
        Self {
            desktop: None,
            accessibility: None,
            latest: None,
            observation_number: 0,
            instance: format!(
                "{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            input: InputState::new(),
            warnings: vec![],
            permission_saved: false,
        }
    }
    async fn dispatch(&mut self, command: Command, cancel: &CancellationToken) -> Response {
        match command {
            Command::Act(a) => Ok(Reply::Action(self.act(a, cancel).await)),
            Command::Start(a) => {
                let result = tokio::select! { r=self.start(a)=>r.map(Reply::Status), _=cancel.cancelled()=>Err(anyhow::anyhow!("Session start cancelled")) };
                if cancel.is_cancelled() {
                    let _ = self.close().await;
                }
                result
            }
            Command::Observe(a) => {
                tokio::select! { r=self.observe(a, false, cancel)=>r.map(Reply::Observation), _=cancel.cancelled()=>Err(anyhow::anyhow!("Observation cancelled")) }
            }
            Command::Inspect(args) => tokio::select! {
                result=self.inspect(args)=>result.map(Reply::Inspection),
                _=cancel.cancelled()=>Err(anyhow::anyhow!("Accessibility inspection cancelled")),
            },
            Command::Focus(args) => {
                self.desktop()?;
                self.latest = None;
                let accessibility = self
                    .accessibility
                    .as_ref()
                    .context("Inspect before focusing a control")?;
                let accepted = tokio::select! {
                    result=accessibility.focus(&args.node_id)=>result?,
                    _=cancel.cancelled()=>return Err(anyhow::anyhow!("Focus request cancelled; observe before deciding whether to retry")),
                };
                Ok(Reply::Focused(FocusReport {
                    request_accepted: accepted,
                    outcome_verified: false,
                }))
            }
            Command::Status => Ok(Reply::Status(self.status())),
            Command::CloseControl => Ok(Reply::Stopped(stop_report(self.close().await))),
            Command::Invalidate => {
                self.latest = None;
                self.accessibility = None;
                Ok(Reply::Status(self.status()))
            }
            Command::Stop => unreachable!("Stop bypasses the command queue"),
        }
    }
    async fn inspect(&mut self, args: InspectArgs) -> Result<crate::accessibility::Inspection> {
        if self.accessibility.is_none() {
            self.accessibility = Some(Accessibility::connect().await?);
        }
        self.accessibility
            .as_mut()
            .unwrap()
            .inspect(args.application.as_deref(), args.max_nodes.unwrap_or(100))
            .await
    }
    fn status(&self) -> Status {
        Status {
            active: self.desktop.as_ref().is_some_and(|d| !d.portal.is_closed()),
            displays: self
                .desktop
                .as_ref()
                .map(|d| {
                    d.portal
                        .streams
                        .iter()
                        .enumerate()
                        .map(|(index, s)| DisplayInfo {
                            index,
                            node_id: s.node_id,
                            logical_size: s.logical_size,
                            position: s.position,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            clipboard_available: self
                .desktop
                .as_ref()
                .is_some_and(|d| d.portal.clipboard_available),
            permission_saved: self.permission_saved,
            captures: self
                .desktop
                .as_ref()
                .map(|d| d.captures.iter().map(|c| c.diagnostics()).collect())
                .unwrap_or_default(),
            warnings: self.warnings.clone(),
        }
    }
    async fn start(&mut self, args: StartArgs) -> Result<Status> {
        if self.desktop.as_ref().is_some_and(|d| !d.portal.is_closed())
            && !args.fresh_permission
            && !args.restart
        {
            return Ok(self.status());
        }
        self.close().await?;
        self.warnings.clear();
        self.permission_saved = false;
        let store = StateStore::from_environment();
        let mut token = None;
        match &store {
            Ok(store) => {
                if args.fresh_permission {
                    if let Err(e) = store.forget() {
                        self.warnings
                            .push(format!("Cannot forget permission: {e:#}"));
                    }
                } else {
                    match store.load() {
                        Ok(t) => token = t,
                        Err(e) => self
                            .warnings
                            .push(format!("Saved permission unavailable; asking KDE: {e:#}")),
                    }
                }
            }
            Err(e) => self
                .warnings
                .push(format!("Permission persistence unavailable: {e:#}")),
        }
        if token.is_some() {
            eprintln!("Restoring KDE desktop access. KDE may ask you to approve again.");
        } else {
            eprintln!("Requesting KDE desktop access. Approve the portal dialog to continue.");
        }
        let portal = Portal::start(token.as_deref()).await?;
        // Keep the session owned here while capture setup awaits. Shutdown/cancellation can close it.
        self.desktop = Some(Desktop {
            portal,
            captures: vec![],
        });
        let setup: Result<()> = async {
            let desktop = self.desktop.as_mut().unwrap();
            ensure!(
                !desktop.portal.streams.is_empty(),
                "No display was selected"
            );
            if let (Ok(store), Some(token)) = (&store, &desktop.portal.restore_token) {
                match store.save(token) {
                    Ok(()) => self.permission_saved = true,
                    Err(e) => self.warnings.push(format!(
                        "Session works, but permission could not be saved: {e:#}"
                    )),
                }
            }
            for stream in &desktop.portal.streams {
                let fd = desktop.portal.open_pipewire().await?;
                let node = stream.node_id;
                let capture =
                    tokio::task::spawn_blocking(move || Capture::start(fd, node)).await??;
                desktop.captures.push(Arc::new(capture));
            }
            Ok(())
        }
        .await;
        if let Err(e) = setup {
            let _ = self.close().await;
            return Err(e);
        }
        Ok(self.status())
    }
    fn desktop(&self) -> Result<&Desktop> {
        let d = self
            .desktop
            .as_ref()
            .context("No desktop session; call computer_start first")?;
        ensure!(
            !d.portal.is_closed(),
            "KDE closed/revoked the desktop session; call computer_start again"
        );
        Ok(d)
    }
    fn capture(&self, display: usize) -> Result<Arc<Capture>> {
        self.desktop()?
            .captures
            .get(display)
            .cloned()
            .context("Unknown display index; inspect computer_status")
    }
    async fn observe(
        &mut self,
        args: ObserveArgs,
        post_input: bool,
        cancel: &CancellationToken,
    ) -> Result<Observation> {
        let timeout = capture_budget(args.timeout_ms, args.after_sequence.is_some())?;
        let capture = self.capture(args.display)?;
        let after = args.after_sequence;
        let token = cancel.clone();
        let waited =
            tokio::task::spawn_blocking(move || capture.wait_frame(after, timeout, &token))
                .await??;
        let frame = waited.frame;
        let stream = self
            .desktop()?
            .portal
            .streams
            .get(args.display)
            .context("Unknown display")?;
        let logical_size = stream.logical_size;
        let node_id = stream.node_id;
        let source_size = (frame.width, frame.height);
        let crop = checked_crop(source_size, args.crop)?;
        let max = args.max_dimension.unwrap_or(1600);
        let image_size = output_size(crop, max);
        let captured_at_ms = frame.captured_at_ms;
        let sequence = frame.sequence;
        let png = tokio::task::spawn_blocking(move || encode(frame, crop, image_size)).await??;
        self.observation_number += 1;
        let frame_id = format!("{}-{}", self.instance, self.observation_number);
        self.latest = Some(LatestObservation {
            sequence,
            captured_at_ms,
            observed_at: Instant::now(),
            mapping: FrameMapping {
                frame_id: frame_id.clone(),
                display: args.display,
                node_id,
                source_size,
                image_size,
                logical_size,
                crop,
            },
        });
        Ok(Observation {
            info: ObservationInfo {
                frame_id,
                display: args.display,
                captured_at_ms,
                age_ms: now_ms().saturating_sub(captured_at_ms),
                capture_sequence: sequence,
                image_width: image_size.0,
                image_height: image_size.1,
                source_width: source_size.0,
                source_height: source_size.1,
                crop,
                logical_size,
                new_frame_after_input: if post_input {
                    after.map(|s| sequence > s)
                } else {
                    None
                },
                freshness_met: after.map(|s| sequence > s),
                requested_after_sequence: after,
                wait_timed_out: waited.timed_out,
            },
            png,
        })
    }
    fn validate(&self, args: &ActArgs) -> Result<(FrameMapping, Prepared)> {
        self.desktop()?;
        let mapping = &self
            .latest
            .as_ref()
            .context("Observe before sending input")?
            .mapping;
        ensure!(
            mapping.frame_id == args.frame_id,
            "Frame is not the latest observation; observe again before acting"
        );
        Ok((
            mapping.clone(),
            prepare(mapping, args, self.desktop()?.portal.clipboard_available)?,
        ))
    }
    async fn act(&mut self, args: ActArgs, cancel: &CancellationToken) -> ActionReport {
        let mut report = ActionReport {
            input_status: InputStatus::NotSent,
            outcome_verified: false,
            cancelled: false,
            error: None,
            cleanup_errors: vec![],
            observation: None,
            capture_error: None,
            diagnostics: ActionDiagnostics::default(),
        };
        let (mapping, prepared) = match self.validate(&args) {
            Ok(v) => v,
            Err(e) => {
                report.error = Some(format!("{e:#}"));
                return report;
            }
        };
        let latest = self.latest.as_ref().unwrap();
        report.diagnostics.observation_age_ms =
            Some(latest.observed_at.elapsed().as_millis() as u64);
        report.diagnostics.referenced_capture_age_ms =
            Some(now_ms().saturating_sub(latest.captured_at_ms));
        // A new sample is only temporal evidence, never a focus-change diagnosis.
        match self.capture(mapping.display).and_then(|c| c.snapshot()) {
            Ok(Some(frame)) => {
                report.diagnostics.capture_advanced_before_input =
                    Some(frame.sequence > latest.sequence);
                if (frame.width, frame.height) != mapping.source_size {
                    self.latest = None;
                    report.error =
                        Some("Capture geometry changed; observe again before input".into());
                    return report;
                }
            }
            _ => {
                report.error = Some("Capture unavailable; observe before input".into());
                return report;
            }
        }
        let prepared = tokio::select! {
            biased;
            _=cancel.cancelled()=>{report.cancelled=true; return report;},
            result=prepared.resolve(&self.desktop.as_ref().unwrap().portal)=>match result {
                Ok(prepared)=>prepared,
                Err(error)=>{report.error=Some(format!("{error:#}")); return report;},
            },
        };
        if let Some(id) = &args.expected_focus_node_id {
            let check = if let Some(accessibility) = &self.accessibility {
                tokio::select! {
                    biased;
                    _=cancel.cancelled()=>{report.cancelled=true; return report;},
                    check=accessibility.check_focus(id)=>check,
                }
            } else {
                FocusCheck::unknown("Inspect before requesting a focus precondition")
            };
            let confirmed = check.state == FocusState::Confirmed;
            report.diagnostics.focus_check = Some(check);
            if !confirmed {
                report.error = Some("Focus precondition not confirmed; inspect/focus/observe again, or omit expected_focus_node_id for visual targeting. No input sent.".into());
                return report;
            }
        }
        if cancel.is_cancelled() {
            report.cancelled = true;
            return report;
        }
        self.latest = None;
        // A timeout can occur after D-Bus delivered an event; conservatively report possibly_partial.
        report.input_status = InputStatus::PossiblyPartial;
        let result = tokio::select! {
            r=self.input.send(&self.desktop.as_ref().unwrap().portal,mapping.node_id,prepared)=>r,
            _=cancel.cancelled()=>{report.cancelled=true;Err(anyhow::anyhow!("Input cancelled; inspect the desktop before retrying"))},
        };
        report.cleanup_errors = self.release_all().await;
        match result {
            Ok(()) => report.input_status = InputStatus::Sent,
            Err(e) => report.error = Some(format!("{e:#}")),
        }
        if !report.cleanup_errors.is_empty() {
            // Closing the remote device is the final way to release state after failed release calls.
            if let Err(e) = self.close().await {
                report
                    .cleanup_errors
                    .push(format!("Closing input session: {e:#}"));
            }
        }
        if args.observe && !cancel.is_cancelled() {
            let after = match self.capture(mapping.display).and_then(|c| c.snapshot()) {
                Ok(Some(frame)) => frame.sequence,
                result => {
                    report.capture_error = Some(format!(
                        "Input was not replayed. Post-input capture unavailable: {result:?}"
                    ));
                    return report;
                }
            };
            tokio::select! { _=tokio::time::sleep(Duration::from_millis(args.settle_ms.unwrap_or(150)))=>{}, _=cancel.cancelled()=>{report.cancelled=true;return report;} }
            // Keep the same view after an action, so zoomed/cropped interaction remains coherent.
            let view = ObserveArgs {
                display: mapping.display,
                crop: Some(mapping.crop),
                max_dimension: Some(mapping.image_size.0.max(mapping.image_size.1)),
                after_sequence: Some(after),
                timeout_ms: args.feedback_timeout_ms,
            };
            let observation = tokio::select! {
                biased;
                _=cancel.cancelled()=>{report.cancelled=true; return report;},
                result=self.observe(view, true, cancel)=>result,
            };
            match observation {
                Ok(o) => {
                    if o.info.freshness_met == Some(false) {
                        report.diagnostics.guidance.push("No newer post-gesture frame within the feedback budget. Reobserve with after_sequence before deciding whether more input is needed; do not blindly replay.".into());
                    }
                    report.observation = Some(o);
                }
                Err(e) => {
                    report.capture_error =
                        Some(format!("Input was not replayed. Observation failed: {e:#}"))
                }
            }
        }
        report
    }
    async fn release_all(&mut self) -> Vec<String> {
        if let Some(d) = &self.desktop {
            self.input.release_all(&d.portal).await
        } else {
            vec![]
        }
    }
    async fn close(&mut self) -> Result<()> {
        let cleanup = self.release_all().await;
        for e in cleanup {
            eprintln!("{e}");
        }
        self.latest = None;
        self.accessibility = None;
        // Retain the portal after failed closure so a worker can retry the barrier.
        if let Some(d) = &self.desktop {
            d.portal.close().await?;
        }
        self.desktop = None;
        Ok(())
    }
}

fn prepare(mapping: &FrameMapping, args: &ActArgs, clipboard_available: bool) -> Result<Prepared> {
    ensure!(
        args.settle_ms.unwrap_or(150) <= 2000,
        "settle_ms must be 0–2000"
    );
    capture_budget(args.feedback_timeout_ms, true)?;
    let point = |x, y| mapping.point(Point { x, y });
    let action = match &args.action {
        InputAction::Move { x, y } => Prepared::Move(point(*x, *y)?),
        InputAction::MoveRelative { dx, dy } => {
            ensure!(
                dx.is_finite() && dy.is_finite(),
                "Relative deltas must be finite"
            );
            Prepared::Relative(*dx, *dy)
        }
        InputAction::Hold {
            keys: k,
            keycodes,
            button,
            duration_ms,
            dx,
            dy,
        } => {
            ensure!(
                (1..=30000).contains(duration_ms),
                "Hold duration must be 1–30000 ms"
            );
            ensure!(
                dx.is_finite() && dy.is_finite(),
                "Relative deltas must be finite"
            );
            ensure!(
                !k.is_empty() || !keycodes.is_empty() || button.is_some(),
                "Hold needs keys, keycodes, or a button"
            );
            let mut keys = if k.is_empty() {
                vec![]
            } else {
                keys::chord(k)?
            };
            if !keycodes.is_empty() {
                keys.extend(keys::physical(keycodes)?);
            }
            keys::unique(&keys)?;
            Prepared::Hold {
                keys,
                button: button.map(Button::code),
                duration: *duration_ms,
                dx: *dx,
                dy: *dy,
            }
        }
        InputAction::Click {
            x,
            y,
            button,
            count,
        } => {
            ensure!((1..=2).contains(count), "Click count must be 1 or 2");
            Prepared::Click(point(*x, *y)?, button.code(), *count)
        }
        InputAction::Scroll { x, y, dx, dy } => {
            ensure!(
                dx.is_finite() && dy.is_finite(),
                "Scroll deltas must be finite"
            );
            Prepared::Scroll(point(*x, *y)?, *dx, *dy)
        }
        InputAction::Keypress { keys: k, keycodes } => Prepared::Keys(keys::choose(k, keycodes)?),
        InputAction::Type {
            text,
            paste_keys,
            paste_keycodes,
            at,
        } => {
            ensure!(
                clipboard_available,
                "Clipboard portal is unavailable; type/paste is not supported in this session"
            );
            ensure!(!text.is_empty(), "Text must not be empty");
            Prepared::Text(
                text.clone(),
                keys::paste(paste_keys.as_deref(), paste_keycodes.as_deref())?,
                at.map(|p| mapping.point(p)).transpose()?,
            )
        }
        InputAction::Drag {
            points,
            button,
            duration_ms,
            keys: k,
        } => {
            ensure!(
                (2..=1000).contains(&points.len()),
                "A drag needs 2–1000 points"
            );
            let duration = duration_ms.unwrap_or(500);
            ensure!(
                (50..=10000).contains(&duration),
                "Drag duration must be 50–10000 ms"
            );
            Prepared::Drag(
                points
                    .iter()
                    .map(|p| mapping.point(*p))
                    .collect::<Result<_>>()?,
                button.code(),
                duration,
                if k.is_empty() {
                    vec![]
                } else {
                    keys::chord(k)?
                },
            )
        }
    };
    Ok(action)
}

fn capture_budget(value: Option<u64>, strict: bool) -> Result<Duration> {
    let ms = value.unwrap_or(if strict { 700 } else { 5000 });
    ensure!(ms <= 5000, "Capture timeout must be 0–5000 ms");
    Ok(Duration::from_millis(ms))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn encode(frame: CapturedFrame, crop: Crop, size: (u32, u32)) -> Result<Vec<u8>> {
    let original = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba)
        .context("Invalid RGBA capture size")?;
    let mut image =
        image::imageops::crop_imm(&original, crop.x, crop.y, crop.width, crop.height).to_image();
    if image.dimensions() != size {
        image = image::imageops::resize(
            &image,
            size.0,
            size.1,
            image::imageops::FilterType::Triangle,
        );
    }
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image).write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shutdown_error_is_not_a_confirmed_close() {
        let report = stop_report(Err(anyhow::anyhow!("Close timed out")));
        assert!(report.controller_halted);
        assert!(!report.desktop_close_confirmed);
        assert!(report.error.unwrap().contains("Close timed out"));
    }
    #[tokio::test]
    async fn status_does_not_request_desktop_access() {
        let (h, worker) = ControllerHandle::spawn();
        let Reply::Status(s) = h.call(Command::Status).await.unwrap() else {
            panic!()
        };
        assert!(!s.active);
        h.shutdown();
        worker.await.unwrap();
    }
    #[tokio::test]
    async fn stop_is_terminal_and_never_reauthorizes() {
        let (h, worker) = ControllerHandle::spawn();
        h.call(Command::Stop).await.unwrap();
        assert!(h.call(Command::Start(StartArgs::default())).await.is_err());
        worker.await.unwrap();
    }
    #[tokio::test]
    async fn input_without_observation_is_not_sent() {
        let mut c = Controller::new();
        let report = c
            .act(
                ActArgs {
                    frame_id: "invented".into(),
                    action: InputAction::Keypress {
                        keys: vec!["ENTER".into()],
                        keycodes: vec![],
                    },
                    observe: true,
                    settle_ms: None,
                    feedback_timeout_ms: None,
                    expected_focus_node_id: None,
                },
                &CancellationToken::new(),
            )
            .await;
        assert!(matches!(report.input_status, InputStatus::NotSent));
        assert!(report.error.is_some());
    }
    #[test]
    fn complete_action_validation_precedes_any_input() {
        let mapping = FrameMapping {
            frame_id: "test".into(),
            display: 0,
            node_id: 1,
            source_size: (100, 100),
            image_size: (100, 100),
            logical_size: Some((100, 100)),
            crop: Crop {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
        };
        let args = |action| {
            serde_json::from_value::<ActArgs>(
                serde_json::json!({"frame_id":"test", "action":action}),
            )
            .unwrap()
        };
        for action in [
            serde_json::json!({"type":"keypress", "keys":["CTRL"], "keycodes":[29]}),
            serde_json::json!({"type":"type", "text":"test", "paste_keys":["CTRL","V"], "paste_keycodes":[29,47]}),
            serde_json::json!({"type":"type", "text":"test", "paste_keycodes":[768]}),
        ] {
            assert!(prepare(&mapping, &args(action), true).is_err());
        }
        assert!(
            prepare(
                &mapping,
                &args(serde_json::json!({"type":"keypress", "keycodes":[29,38]})),
                true
            )
            .is_ok()
        );
        assert!(
            prepare(
                &mapping,
                &args(serde_json::json!({"type":"type", "text":"test"})),
                false
            )
            .is_err()
        );
        assert_eq!(capture_budget(Some(0), true).unwrap(), Duration::ZERO);
        assert_eq!(
            capture_budget(Some(5000), true).unwrap(),
            Duration::from_secs(5)
        );
        assert!(capture_budget(Some(5001), true).is_err());
        assert_eq!(
            capture_budget(None, true).unwrap(),
            Duration::from_millis(700)
        );
    }

    #[test]
    fn encodes_valid_cropped_png() {
        let f = CapturedFrame {
            sequence: 1,
            captured_at_ms: 1,
            width: 4,
            height: 4,
            rgba: vec![255; 64],
        };
        let png = encode(
            f,
            Crop {
                x: 1,
                y: 1,
                width: 2,
                height: 2,
            },
            (1, 1),
        )
        .unwrap();
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1, 1));
    }
}
