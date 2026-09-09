//! The worker is the sole live claim authority. Revocation fences before waiting.
use super::*;
use crate::controller::ControllerHandle;
use anyhow::{Result, ensure};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{net::UnixListener, sync::Mutex};
use tokio_util::sync::CancellationToken;

struct Claim {
    connection: u64,
    grant_order: u64,
    since: u64,
    activity: u64,
    cancel: CancellationToken,
}
struct State {
    claim: Option<Claim>,
    error: Option<String>,
    epoch: u64,
    destroying: bool,
    lifecycle: BTreeMap<u64, u64>,
}
impl State {
    fn admit_lifecycle(&mut self, connection: u64, order: u64) -> Result<()> {
        let fence = self.lifecycle.entry(connection).or_default();
        ensure!(
            order > *fence,
            "Stale lifecycle request predates a newer transition on this connection"
        );
        *fence = order;
        Ok(())
    }
    fn admit_operation(
        &mut self,
        connection: u64,
        order: u64,
        mutation: bool,
        cancel: &CancellationToken,
    ) -> Result<(bool, CancellationToken)> {
        ensure!(!self.destroying, "Desktop destruction underway");
        let owner = self.claim.as_ref().is_some_and(|c| {
            c.connection == connection && order > c.grant_order && !c.cancel.is_cancelled()
        });
        ensure!(
            !mutation || owner,
            "Claim this desktop before mutating it; this request may predate the current claim"
        );
        if owner {
            let claim = self.claim.as_mut().unwrap();
            claim.activity = now_ms();
            Ok((true, claim.cancel.clone()))
        } else {
            // Old owner reads are still useful, but cannot replace successor references.
            Ok((false, cancel.child_token()))
        }
    }
}
struct Worker {
    descriptor: Descriptor,
    controller: ControllerHandle,
    state: Mutex<State>,
    transition: Mutex<()>,
    operation: Mutex<()>,
    processes: Mutex<processes::Processes>,
    environment: BTreeMap<String, String>,
    exit: CancellationToken,
}
impl Worker {
    async fn close_control(&self) -> Result<()> {
        let _operation = self.operation.lock().await;
        let Reply::Stopped(report) = self.controller.call(Command::CloseControl).await? else {
            anyhow::bail!("Missing closure confirmation")
        };
        ensure!(
            report.desktop_close_confirmed,
            "{}",
            report
                .error
                .unwrap_or_else(|| "Portal closure unconfirmed".into())
        );
        Ok(())
    }
    async fn revoke(&self) -> Result<()> {
        {
            let mut state = self.state.lock().await;
            if let Some(claim) = state.claim.take() {
                claim.cancel.cancel();
            }
            state.error = Some("Control revocation in progress".into());
        }
        let result = self.close_control().await;
        self.state.lock().await.error = result.as_ref().err().map(|e| format!("{e:#}"));
        result
    }
    async fn release(&self) -> Result<()> {
        self.revoke().await?;
        // A retained owned desktop remains observable, but the old input session
        // must be confirmed closed before opening a clean, unclaimed session.
        if self.descriptor.owned {
            let _operation = self.operation.lock().await;
            let result = self
                .controller
                .call(Command::Start(StartArgs::default()))
                .await;
            self.state.lock().await.error = result.as_ref().err().map(|e| format!("{e:#}"));
            result?;
        }
        Ok(())
    }
    async fn disconnect(&self, id: u64) {
        // Cancel before waiting for another lifecycle transition.
        {
            let state = self.state.lock().await;
            if let Some(c) = &state.claim
                && c.connection == id
            {
                c.cancel.cancel();
            }
        }
        let _transition = self.transition.lock().await;
        if self
            .state
            .lock()
            .await
            .claim
            .as_ref()
            .is_some_and(|c| c.connection == id)
        {
            let _ = self.release().await;
        }
        self.state.lock().await.lifecycle.remove(&id);
    }
    async fn info(&self) -> DesktopInfo {
        let state = self.state.lock().await;
        let mut processes = self.processes.lock().await;
        let service_error = processes.failed_service();
        DesktopInfo {
            descriptor: processes.descriptor().unwrap_or(&self.descriptor).clone(),
            available: service_error.is_none() && !state.destroying,
            claimed: state.claim.is_some(),
            claim_age_ms: state
                .claim
                .as_ref()
                .map(|c| now_ms().saturating_sub(c.since)),
            last_activity_age_ms: state
                .claim
                .as_ref()
                .map(|c| now_ms().saturating_sub(c.activity)),
            control_error: state.error.clone().or(service_error),
            applications: processes.applications(),
            resources: processes::worker_resources(&self.descriptor.id),
        }
    }
    async fn claim(
        &self,
        id: u64,
        epoch: u64,
        order: u64,
        force: bool,
        cancel: &CancellationToken,
    ) -> Result<Response> {
        let _transition = self.transition.lock().await;
        {
            let mut state = self.state.lock().await;
            state.admit_lifecycle(id, order)?;
            ensure!(!cancel.is_cancelled(), "Claim cancelled before revocation");
            ensure!(!state.destroying, "Desktop destruction underway");
            ensure!(
                state.epoch == epoch,
                "Control halted for this connection; reconnect before claiming"
            );
            if let Some(c) = &state.claim {
                if c.connection == id && !c.cancel.is_cancelled() {
                    return Ok(Response::Claimed {
                        desktop_id: self.descriptor.id.clone(),
                    });
                }
                ensure!(force, "Desktop already claimed; explicit force required");
            }
        }
        self.revoke().await?;
        ensure!(
            !cancel.is_cancelled(),
            "Claim requester disconnected or cancelled; desktop left unclaimed"
        );
        if self.descriptor.owned {
            let opened = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(anyhow::anyhow!("Claim cancelled while opening control")),
                result = self.controller.call(Command::Start(StartArgs::default())) => result.map(|_| ()),
            };
            if let Err(error) = opened {
                let _ = self.revoke().await;
                return Err(error);
            }
        }
        let mut state = self.state.lock().await;
        if cancel.is_cancelled() {
            drop(state);
            self.revoke().await?;
            anyhow::bail!("Claim requester disappeared; desktop left unclaimed");
        }
        state.claim = Some(Claim {
            connection: id,
            grant_order: order,
            since: now_ms(),
            activity: now_ms(),
            cancel: CancellationToken::new(),
        });
        Ok(Response::Claimed {
            desktop_id: self.descriptor.id.clone(),
        })
    }
    async fn control(
        &self,
        id: u64,
        order: u64,
        command: Command,
        cancel: CancellationToken,
    ) -> Result<Response> {
        if matches!(command, Command::Stop) {
            let _transition = self.transition.lock().await;
            {
                let mut state = self.state.lock().await;
                state.admit_lifecycle(id, order)?;
                state.epoch += 1;
            }
            let result = self.revoke().await;
            return Ok(Response::Control(Reply::Stopped(StopReport {
                controller_halted: true,
                desktop_close_confirmed: result.is_ok(),
                error: result.err().map(|e| format!("{e:#}")),
            })));
        }
        ensure!(
            !matches!(command, Command::CloseControl | Command::Invalidate),
            "Internal control command"
        );
        let mutation = matches!(
            command,
            Command::Start(_) | Command::Act(_) | Command::Focus(_)
        );
        let (owner, generation) = self
            .state
            .lock()
            .await
            .admit_operation(id, order, mutation, &cancel)?;
        self.execute_control(command, cancel, owner, generation)
            .await
    }
    async fn execute_control(
        &self,
        command: Command,
        cancel: CancellationToken,
        owner: bool,
        generation: CancellationToken,
    ) -> Result<Response> {
        let _operation = tokio::select! { biased; _=generation.cancelled()=>anyhow::bail!("Claim revoked before operation"), _=cancel.cancelled()=>anyhow::bail!("Request cancelled"), lock=self.operation.lock()=>lock };
        tokio::select! { biased;
            _=generation.cancelled()=>anyhow::bail!("Claim revoked; input may be partial, never replay"),
            _=cancel.cancelled()=>anyhow::bail!("Request cancelled; input may be partial, never replay"),
            result=self.controller.call_view(command,!owner)=>result.map(Response::Control),
        }
    }
    async fn launch(
        &self,
        id: u64,
        order: u64,
        args: LaunchArgs,
        cancel: &CancellationToken,
    ) -> Result<Response> {
        let (_, generation) = self
            .state
            .lock()
            .await
            .admit_operation(id, order, true, cancel)?;
        let _operation = tokio::select! { biased;
            _ = generation.cancelled() => anyhow::bail!("Claim revoked before launch"),
            _ = cancel.cancelled() => anyhow::bail!("Launch cancelled before process creation"),
            lock = self.operation.lock() => lock,
        };
        ensure!(
            !generation.is_cancelled() && !cancel.is_cancelled(),
            "Launch cancelled before process creation"
        );
        ensure!(
            self.descriptor.owned,
            "LCU does not own main-desktop applications"
        );
        let (argv, env, routing) =
            processes::launch_plan(&self.descriptor, &args, &self.environment)?;
        self.controller.call(Command::Invalidate).await?;
        ensure!(
            !generation.is_cancelled() && !cancel.is_cancelled(),
            "Launch cancelled before process creation"
        );
        let app = self.processes.lock().await.spawn(
            &argv,
            &env,
            &args.cwd,
            &self.descriptor.state.join("applications.log"),
            true,
            routing,
        )?;
        Ok(Response::Launched(app))
    }
    async fn destroy(&self, id: u64, order: u64, force: bool) -> Result<Response> {
        let _transition = self.transition.lock().await;
        self.state.lock().await.admit_lifecycle(id, order)?;
        ensure!(
            self.descriptor.owned,
            "The main desktop cannot be destroyed"
        );
        {
            let mut state = self.state.lock().await;
            ensure!(
                force || state.claim.as_ref().is_none_or(|c| c.connection == id),
                "Another connection owns this desktop; force required"
            );
            state.destroying = true;
        }
        // Destruction keeps admission fenced. Even if the private portal is
        // unreachable, identity-checked termination of OUR session establishes
        // final cleanup; this path is never available for main.
        let _ = self.revoke().await;
        self.processes
            .lock()
            .await
            .cleanup(&self.descriptor.id)
            .await?;
        // remove_dir_all does not follow symlinks to shared project files.
        remove_resources(&self.descriptor)?;
        self.exit.cancel();
        Ok(Response::Destroyed {
            desktop_id: self.descriptor.id.clone(),
        })
    }
    async fn request(
        &self,
        id: u64,
        epoch: u64,
        order: u64,
        request: Request,
        cancel: CancellationToken,
    ) -> Result<Response> {
        match request {
            Request::Hello => Ok(Response::Hello {
                wire_revision: WIRE_REVISION,
                version: env!("CARGO_PKG_VERSION").into(),
            }),
            Request::Control {
                desktop_id,
                command,
            } => {
                ensure!(desktop_id == self.descriptor.id, "Wrong desktop endpoint");
                self.control(id, order, command, cancel).await
            }
            Request::Desktop(op) => {
                let target = match &op {
                    DesktopOperation::Claim { desktop_id, .. }
                    | DesktopOperation::Release { desktop_id }
                    | DesktopOperation::Launch { desktop_id, .. }
                    | DesktopOperation::Destroy { desktop_id, .. } => Some(desktop_id),
                    _ => None,
                };
                ensure!(
                    target.is_none_or(|t| t == &self.descriptor.id),
                    "Wrong desktop endpoint"
                );
                match op {
                    DesktopOperation::List => Ok(Response::Desktops(vec![self.info().await])),
                    DesktopOperation::Claim { force, .. } => {
                        self.claim(id, epoch, order, force, &cancel).await
                    }
                    DesktopOperation::Release { .. } => {
                        let _transition = self.transition.lock().await;
                        self.state.lock().await.admit_lifecycle(id, order)?;
                        ensure!(
                            self.state
                                .lock()
                                .await
                                .claim
                                .as_ref()
                                .is_none_or(|c| c.connection == id),
                            "Not the claimant"
                        );
                        let needs_release = {
                            let state = self.state.lock().await;
                            ensure!(
                                state.epoch == epoch,
                                "Control halted for this connection; reconnect before releasing"
                            );
                            state.claim.is_some() || state.error.is_some()
                        };
                        // Idempotent release must not reopen an emergency-halted
                        // session merely because an old connection sends it late.
                        if needs_release {
                            self.release().await?;
                        }
                        Ok(Response::Released {
                            desktop_id: self.descriptor.id.clone(),
                        })
                    }
                    DesktopOperation::Launch { args, .. } => {
                        self.launch(id, order, args, &cancel).await
                    }
                    DesktopOperation::Destroy { force, .. } => self.destroy(id, order, force).await,
                    _ => anyhow::bail!("Create is a daemon operation"),
                }
            }
            _ => anyhow::bail!("Not a worker operation"),
        }
    }
}
pub async fn run(descriptor: Descriptor, lifecycle_fd: i32) -> Result<()> {
    let _lock = ownership::LifecycleLock::inherited(lifecycle_fd)?;
    if descriptor.socket().exists() {
        std::fs::remove_file(descriptor.socket())?;
    }
    processes::Processes::subreaper()?;
    let mut processes = processes::Processes::for_desktop(descriptor.clone());
    let bootstrap = session::bootstrap(&descriptor, &mut processes).await;
    if let Err(error) = bootstrap {
        let _ = processes.cleanup(&descriptor.id).await;
        return Err(error);
    }
    let (controller, actor) = ControllerHandle::spawn();
    if descriptor.owned {
        let ready = tokio::time::timeout(Duration::from_secs(30), async {
            controller
                .call(Command::Start(StartArgs::default()))
                .await?;
            controller
                .call_view(Command::Observe(ObserveArgs::default()), true)
                .await?;
            Ok::<_, anyhow::Error>(())
        })
        .await;
        if !matches!(ready, Ok(Ok(()))) {
            controller.shutdown();
            let _ = actor.await;
            let _ = processes.cleanup(&descriptor.id).await;
            anyhow::bail!(
                "Owned desktop capture bootstrap failed: {ready:?}; inspect services.log"
            );
        }
    }
    let listener = UnixListener::bind(descriptor.socket())?;
    let worker = Arc::new(Worker {
        descriptor,
        controller,
        state: Mutex::new(State {
            claim: None,
            error: None,
            epoch: 0,
            destroying: false,
            lifecycle: BTreeMap::new(),
        }),
        transition: Mutex::new(()),
        operation: Mutex::new(()),
        processes: Mutex::new(processes),
        environment: std::env::vars().collect(),
        exit: CancellationToken::new(),
    });
    let next = AtomicU64::new(1);
    loop {
        tokio::select! {
            _=worker.exit.cancelled()=>break,
            accepted=listener.accept()=>{
                let (stream,_)=accepted?; let w=worker.clone(); let id=next.fetch_add(1,Ordering::Relaxed);
                let epoch=w.state.lock().await.epoch;
                tokio::spawn(async move {
                    let life=CancellationToken::new(); let handler=w.clone();
                    let _=rpc::serve_worker(stream,life,move |r,order,c| {let w=handler.clone();async move {w.request(id,epoch,order,r,c).await}}).await;
                    w.disconnect(id).await;
                });
            }
        }
    }
    // Give the destroy receipt writer an opportunity; worker exit never kills apps on ordinary EOF.
    tokio::time::sleep(Duration::from_millis(100)).await;
    worker.controller.shutdown();
    actor.await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Semaphore;

    fn fixture(root: &std::path::Path) -> (Arc<Worker>, tokio::task::JoinHandle<()>) {
        let (controller, actor) = ControllerHandle::spawn();
        (
            Arc::new(Worker {
                descriptor: Descriptor {
                    name: None,
                    id: "main".into(),
                    runtime: root.join("run"),
                    state: root.join("state"),
                    width: 100,
                    height: 100,
                    owned: false,
                    ownership: None,
                },
                controller,
                state: Mutex::new(State {
                    claim: None,
                    error: None,
                    epoch: 0,
                    destroying: false,
                    lifecycle: BTreeMap::new(),
                }),
                transition: Mutex::new(()),
                operation: Mutex::new(()),
                processes: Mutex::new(processes::Processes::new()),
                environment: BTreeMap::new(),
                exit: CancellationToken::new(),
            }),
            actor,
        )
    }
    fn claim() -> Request {
        Request::Desktop(DesktopOperation::Claim {
            desktop_id: "main".into(),
            force: false,
        })
    }
    fn release() -> Request {
        Request::Desktop(DesktopOperation::Release {
            desktop_id: "main".into(),
        })
    }
    fn action(frame: &str) -> Request {
        Request::Control {
            desktop_id: "main".into(),
            command: Command::Act(ActArgs {
                frame_id: frame.into(),
                action: InputAction::Keypress {
                    keys: vec!["ENTER".into()],
                    keycodes: vec![],
                },
                observe: false,
                settle_ms: None,
                feedback_timeout_ms: None,
                expected_focus_node_id: None,
            }),
        }
    }
    struct Gate {
        entered: Semaphore,
        resume: Semaphore,
        release_once: std::sync::atomic::AtomicBool,
    }
    impl Gate {
        fn new(release: bool) -> Arc<Self> {
            Arc::new(Self {
                entered: Semaphore::new(0),
                resume: Semaphore::new(0),
                release_once: std::sync::atomic::AtomicBool::new(release),
            })
        }
        async fn pause(&self, request: &Request, cancel: &CancellationToken) -> Result<()> {
            let pause = matches!(request, Request::Control { command: Command::Act(a), .. } if a.frame_id == "paused")
                || (matches!(request, Request::Desktop(DesktopOperation::Release { .. }))
                    && self.release_once.swap(false, Ordering::SeqCst));
            if pause {
                self.entered.add_permits(1);
                tokio::select! { _ = cancel.cancelled() => anyhow::bail!("Paused request cancelled"), permit = self.resume.acquire() => permit.unwrap().forget() }
            }
            Ok(())
        }
        async fn entered(&self) {
            tokio::time::timeout(Duration::from_secs(2), self.entered.acquire())
                .await
                .unwrap()
                .unwrap()
                .forget();
        }
    }
    // Exercise both asynchronous hops with real socket readers and the production
    // forwarding API. Barriers, not scheduling assumptions, establish the race.
    async fn scheduling_gap(pause_at_worker: bool, delayed_release: bool) {
        let root = tempfile::tempdir().unwrap();
        let (worker, actor) = fixture(root.path());
        let gate = Gate::new(delayed_release);
        let worker_path = root.path().join("worker.sock");
        let daemon_path = root.path().join("daemon.sock");
        let backend = UnixListener::bind(&worker_path).unwrap();
        let frontend = UnixListener::bind(&daemon_path).unwrap();
        let w = worker.clone();
        let g = gate.clone();
        let backend_task = tokio::spawn(async move {
            let (socket, _) = backend.accept().await.unwrap();
            let handler = w.clone();
            rpc::serve_worker(
                socket,
                CancellationToken::new(),
                move |request, order, cancel| {
                    let w = handler.clone();
                    let g = g.clone();
                    async move {
                        if pause_at_worker {
                            g.pause(&request, &cancel).await?;
                        }
                        w.request(1, 0, order, request, cancel).await
                    }
                },
            )
            .await
            .unwrap();
            w.disconnect(1).await;
        });
        let forwarding = rpc::Client::connect(&worker_path).await.unwrap();
        let g = gate.clone();
        let frontend_task = tokio::spawn(async move {
            let (socket, _) = frontend.accept().await.unwrap();
            let forward = forwarding.clone();
            rpc::serve(
                socket,
                CancellationToken::new(),
                move |request, order, cancel| {
                    let f = forward.clone();
                    let g = g.clone();
                    async move {
                        if !pause_at_worker {
                            g.pause(&request, &cancel).await?;
                        }
                        f.forward(request, order).await
                    }
                },
            )
            .await
            .unwrap();
            forwarding.disconnect();
        });
        let client = rpc::Client::connect(&daemon_path).await.unwrap();
        client.call(claim()).await.unwrap();
        let c = client.clone();
        let old = tokio::spawn(async move {
            c.call(if delayed_release {
                release()
            } else {
                action("paused")
            })
            .await
        });
        gate.entered().await;
        client.call(release()).await.unwrap();
        client.call(claim()).await.unwrap();
        let successor = worker
            .state
            .lock()
            .await
            .claim
            .as_ref()
            .unwrap()
            .cancel
            .clone();
        gate.resume.add_permits(1);
        assert!(
            old.await.unwrap().is_err(),
            "overtaken request must not acquire successor authority"
        );
        assert!(!successor.is_cancelled());
        // A genuinely new action reaches the inactive controller, rather than being
        // rejected by the claim fence. No native permission/input is requested.
        assert!(
            matches!(client.call(action("new")).await.unwrap(), Response::Control(Reply::Action(a)) if matches!(a.input_status, InputStatus::NotSent))
        );
        if !delayed_release {
            let c = client.clone();
            let same_generation = tokio::spawn(async move { c.call(action("paused")).await });
            gate.entered().await;
            client.call(claim()).await.unwrap();
            gate.resume.add_permits(1);
            assert!(matches!(
                same_generation.await.unwrap().unwrap(),
                Response::Control(Reply::Action(_))
            ));
            assert!(
                !successor.is_cancelled(),
                "idempotent claim must preserve generation"
            );
        }
        client.disconnect();
        tokio::time::timeout(Duration::from_secs(2), frontend_task)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), backend_task)
            .await
            .unwrap()
            .unwrap();
        worker.controller.shutdown();
        actor.await.unwrap();
    }
    #[tokio::test]
    async fn old_request_paused_before_daemon_forwarding_cannot_use_reclaimed_authority() {
        scheduling_gap(false, false).await;
    }
    #[tokio::test]
    async fn old_request_paused_before_worker_admission_cannot_use_reclaimed_authority() {
        scheduling_gap(true, false).await;
    }
    #[tokio::test]
    async fn delayed_release_cannot_revoke_new_claim() {
        scheduling_gap(false, true).await;
        scheduling_gap(true, true).await;
    }

    #[tokio::test]
    async fn old_reads_degrade_and_old_launch_lifecycle_requests_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let (worker, actor) = fixture(root.path());
        let cancel = CancellationToken::new();
        worker.claim(1, 0, 1, false, &cancel).await.unwrap();
        worker
            .request(1, 0, 3, release(), cancel.clone())
            .await
            .unwrap();
        worker.claim(1, 0, 4, false, &cancel).await.unwrap();
        let successor = worker
            .state
            .lock()
            .await
            .claim
            .as_ref()
            .unwrap()
            .cancel
            .clone();
        assert!(
            !worker
                .state
                .lock()
                .await
                .admit_operation(1, 2, false, &cancel)
                .unwrap()
                .0,
            "old observe/inspect must not update owner references"
        );
        assert!(
            worker
                .state
                .lock()
                .await
                .admit_operation(1, 5, false, &cancel)
                .unwrap()
                .0
        );
        let args = LaunchArgs {
            argv: vec!["must-not-spawn".into()],
            cwd: root.path().into(),
            env: BTreeMap::new(),
            webkit_alternate_buffers: false,
        };
        assert!(
            worker
                .launch(1, 2, args, &cancel)
                .await
                .unwrap_err()
                .to_string()
                .contains("predate")
        );
        for request in [
            release(),
            claim(),
            Request::Control {
                desktop_id: "main".into(),
                command: Command::Stop,
            },
            Request::Desktop(DesktopOperation::Destroy {
                desktop_id: "main".into(),
                force: true,
            }),
        ] {
            assert!(
                worker
                    .request(1, 0, 2, request, cancel.clone())
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("Stale lifecycle")
            );
            assert!(!successor.is_cancelled());
        }
        worker.controller.shutdown();
        actor.await.unwrap();
    }
    #[tokio::test]
    async fn emergency_stop_fences_late_release_and_reclaim() {
        let root = tempfile::tempdir().unwrap();
        let (worker, actor) = fixture(root.path());
        let cancel = CancellationToken::new();
        worker.claim(1, 0, 1, false, &cancel).await.unwrap();
        worker
            .control(1, 2, Command::Stop, cancel.clone())
            .await
            .unwrap();
        assert!(
            worker
                .request(1, 0, 3, release(), cancel.clone())
                .await
                .is_err()
        );
        assert!(worker.claim(1, 0, 4, false, &cancel).await.is_err());
        assert!(worker.state.lock().await.claim.is_none());
        worker.claim(2, 1, 1, false, &cancel).await.unwrap();
        worker.controller.shutdown();
        actor.await.unwrap();
    }
    #[tokio::test]
    async fn eof_during_claim_cleanup_cannot_leave_a_claimant() {
        let root = tempfile::tempdir().unwrap();
        let (worker, actor) = fixture(root.path());
        worker
            .claim(1, 0, 1, false, &CancellationToken::new())
            .await
            .unwrap();
        let old = worker
            .state
            .lock()
            .await
            .claim
            .as_ref()
            .unwrap()
            .cancel
            .clone();
        let barrier = worker.operation.lock().await;
        let path = root.path().join("eof.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let life = CancellationToken::new();
        let server_life = life.clone();
        let w = worker.clone();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let handler = w.clone();
            rpc::serve_worker(socket, server_life, move |request, order, cancel| {
                let w = handler.clone();
                async move { w.request(2, 0, order, request, cancel).await }
            })
            .await
            .unwrap();
            w.disconnect(2).await;
        });
        let client = rpc::Client::connect(&path).await.unwrap();
        let c = client.clone();
        let request = tokio::spawn(async move {
            c.call(Request::Desktop(DesktopOperation::Claim {
                desktop_id: "main".into(),
                force: true,
            }))
            .await
        });
        tokio::time::timeout(Duration::from_secs(1), old.cancelled())
            .await
            .unwrap();
        client.disconnect();
        tokio::time::timeout(Duration::from_secs(1), life.cancelled())
            .await
            .unwrap();
        assert!(!server.is_finished(), "cleanup must continue despite EOF");
        drop(barrier);
        assert!(request.await.unwrap().is_err());
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
        assert!(worker.state.lock().await.claim.is_none());
        worker.controller.shutdown();
        actor.await.unwrap();
    }
    #[tokio::test]
    async fn captured_generation_is_cancelled_before_cleanup_and_disappearing_successor_is_not_granted()
     {
        let root = tempfile::tempdir().unwrap();
        let (worker, actor) = fixture(root.path());
        worker
            .claim(1, 0, 1, false, &CancellationToken::new())
            .await
            .unwrap();
        let barrier = worker.operation.lock().await;
        // Explicit admission acknowledgement: capture the exact token through the
        // same state method used by control, before spawning execution behind the barrier.
        let (owner, old) = worker
            .state
            .lock()
            .await
            .admit_operation(1, 2, true, &CancellationToken::new())
            .unwrap();
        let w = worker.clone();
        let generation = old.clone();
        let request = tokio::spawn(async move {
            let Request::Control { command, .. } = action("inactive-frame") else {
                unreachable!()
            };
            w.execute_control(command, CancellationToken::new(), owner, generation)
                .await
        });
        let successor_life = CancellationToken::new();
        let life = successor_life.clone();
        let w = worker.clone();
        let successor = tokio::spawn(async move { w.claim(2, 0, 1, true, &life).await });
        tokio::time::timeout(Duration::from_secs(1), old.cancelled())
            .await
            .unwrap();
        assert!(!successor.is_finished());
        successor_life.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), request)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        drop(barrier);
        assert!(successor.await.unwrap().is_err());
        assert!(worker.state.lock().await.claim.is_none());
        worker
            .claim(3, 0, 1, false, &CancellationToken::new())
            .await
            .unwrap();
        worker.controller.shutdown();
        actor.await.unwrap();
    }
}
