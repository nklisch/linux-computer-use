//! Disposable routing state; descriptors locate authoritative, retained workers.
use super::*;
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashMap,
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Command as ProcessCommand, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::{net::UnixListener, sync::Mutex};
use tokio_util::sync::CancellationToken;

pub fn descriptors() -> Result<Vec<Descriptor>> {
    let root = state_root()?;
    let mut result = vec![];
    if !root.exists() {
        return Ok(result);
    }
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path().join("descriptor.json");
        if let Ok(bytes) = std::fs::read(path)
            && let Ok(d) = serde_json::from_slice(&bytes)
        {
            result.push(d);
        }
    }
    Ok(result)
}
fn descriptor(id: &str) -> Result<Descriptor> {
    descriptors()?
        .into_iter()
        .find(|d| d.id == id)
        .context("Unknown desktop ID; list desktops, never fall back to main")
}
async fn create(owned: bool, width: u32, height: u32, name: Option<String>) -> Result<Descriptor> {
    ensure!(
        (64..=8192).contains(&width) && (64..=8192).contains(&height),
        "Desktop dimensions must be 64–8192"
    );
    let id = if owned {
        format!("desktop-{}-{}", now_ms(), std::process::id())
    } else {
        "main".into()
    };
    let d = Descriptor {
        name: name.filter(|name| !name.trim().is_empty()),
        runtime: runtime_root()?.join("desktops").join(&id),
        state: state_root()?.join(&id),
        id,
        width,
        height,
        owned,
        ownership: Some(ownership::Ownership::new()?),
    };
    let lock = ownership::LifecycleLock::acquire(&d)?;
    private_dir(&d.runtime)?;
    private_dir(&d.state)?;
    let path = d.state.join("descriptor.json");
    ownership::publish(&d)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(d.state.join("worker.log"))?;
    // Retained daemons may outlive replacement of their executable pathname.
    let mut command = ProcessCommand::new("/proc/self/exe");
    command
        .arg("desktop-worker")
        .arg(&path)
        .arg("--lifecycle-fd")
        .arg(lock.fd().to_string())
        .env_clear()
        .envs(session::environment(&d)?)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    command.process_group(0);
    let fd = lock.fd();
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = tokio::process::Command::from(command).spawn()?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        if let Ok(client) = rpc::Client::connect(&d.socket()).await {
            client.handshake().await?;
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            return read_descriptor(&path);
        }
        if let Some(exit) = child.try_wait()? {
            anyhow::bail!(
                "Worker bootstrap exited {exit}; {} remains discoverable for explicit cleanup; inspect {}",
                d.id,
                d.state.join("worker.log").display()
            );
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "Worker readiness timed out; {} may still start. List before retrying create",
            d.id
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
async fn recover(id: &str) -> Result<Response> {
    let d = descriptor(id)?;
    ensure!(d.owned, "Cannot destroy main");
    let _lock = ownership::LifecycleLock::acquire(&d)?;
    let mut d = read_descriptor(&d.state.join("descriptor.json"))?;
    ownership::cleanup(&mut d, false).await?;
    remove_resources(&d)?;
    Ok(Response::Destroyed {
        desktop_id: id.into(),
    })
}
async fn list() -> Result<Response> {
    let mut jobs = tokio::task::JoinSet::new();
    for d in descriptors()? {
        jobs.spawn(async move {
        let live=tokio::time::timeout(Duration::from_secs(2),async {let c=rpc::Client::connect(&d.socket()).await?;c.call(Request::Desktop(DesktopOperation::List)).await}).await;
        if let Ok(Ok(Response::Desktops(mut values)))=live && let Some(info)=values.pop() {return info;}
        DesktopInfo {descriptor:d.clone(),available:false,claimed:false,claim_age_ms:None,last_activity_age_ms:None,control_error:Some("Worker unreachable; claim state unknown. Explicit force destruction requires exclusive lifecycle ownership and recorded process identities.".into()),applications:vec![],resources:processes::resources(&d.id)}
    });
    }
    let mut result = vec![];
    while let Some(info) = jobs.join_next().await {
        result.push(info?);
    }
    Ok(Response::Desktops(result))
}
struct Connection {
    workers: Mutex<HashMap<String, rpc::Client>>,
    life: CancellationToken,
}
impl Connection {
    async fn worker(&self, id: &str) -> Result<rpc::Client> {
        let mut workers = self.workers.lock().await;
        ensure!(!self.life.is_cancelled(), "Frontend disconnected");
        if let Some(client) = workers.get(id) {
            return Ok(client.clone());
        }
        let client = match rpc::Client::connect(&descriptor(id)?.socket()).await {
            Ok(client) => client,
            Err(_) if id == "main" => {
                // Main has no owned apps to resurrect. Its exclusive worker lock
                // prevents replacing a still-live controller; access remains unopened.
                let d = create(false, 1280, 720, None).await?;
                rpc::Client::connect(&d.socket()).await?
            }
            Err(error) => return Err(error),
        };
        tokio::select! { biased;
            _ = self.life.cancelled() => { client.disconnect(); anyhow::bail!("Frontend disconnected"); }
            result = client.handshake() => result?,
        }
        if self.life.is_cancelled() {
            client.disconnect();
            anyhow::bail!("Frontend disconnected");
        }
        workers.insert(id.into(), client.clone());
        Ok(client)
    }
    async fn request(
        &self,
        request: Request,
        ingress: u64,
        cancel: CancellationToken,
        creation: Arc<Mutex<()>>,
        stop: CancellationToken,
    ) -> Result<Response> {
        match request {
            Request::Hello => Ok(Response::Hello {
                wire_revision: WIRE_REVISION,
                version: env!("CARGO_PKG_VERSION").into(),
            }),
            Request::ShutdownDaemon => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    stop.cancel();
                });
                Ok(Response::DaemonStopped)
            }
            Request::Desktop(DesktopOperation::List) => list().await,
            Request::Desktop(DesktopOperation::Create {
                width,
                height,
                name,
            }) => {
                let _guard = creation.lock().await;
                ensure!(!cancel.is_cancelled(), "Create cancelled before allocation");
                Ok(Response::Created(create(true, width, height, name).await?))
            }
            request => {
                let id = match &request {
                    Request::Control { desktop_id, .. } => desktop_id,
                    Request::Desktop(
                        DesktopOperation::Claim { desktop_id, .. }
                        | DesktopOperation::Release { desktop_id }
                        | DesktopOperation::Launch { desktop_id, .. }
                        | DesktopOperation::Destroy { desktop_id, .. },
                    ) => desktop_id,
                    _ => unreachable!(),
                }
                .clone();
                if id == "main" && descriptor(&id).is_err() {
                    let _guard = creation.lock().await;
                    if descriptor(&id).is_err() {
                        create(false, 1280, 720, None).await?;
                    }
                }
                let client = match self.worker(&id).await {
                    Ok(client) => client,
                    Err(error) => {
                        if let Request::Desktop(DesktopOperation::Destroy { force: true, .. }) =
                            &request
                        {
                            return recover(&id).await;
                        }
                        return Err(error);
                    }
                };
                // Destruction runs to completion even after request cancellation.
                if matches!(request, Request::Desktop(DesktopOperation::Destroy { .. })) {
                    let forced = matches!(
                        &request,
                        Request::Desktop(DesktopOperation::Destroy { force: true, .. })
                    );
                    let result = client.forward(request, ingress).await;
                    // A retained frontend can still cache the dead worker transport.
                    // Only transport loss enters recovery; preserve live-worker errors.
                    if result.is_err() && forced && client.is_disconnected() {
                        return recover(&id).await;
                    }
                    return result;
                }
                tokio::select! { biased; _ = cancel.cancelled() => anyhow::bail!("Request cancelled; outcome may be unknown"), result = client.forward(request, ingress) => result }
            }
        }
    }
    async fn close(&self) {
        for (_, c) in self.workers.lock().await.drain() {
            c.disconnect();
        }
    }
}
pub async fn run() -> Result<()> {
    let root = runtime_root()?;
    private_dir(&root)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("daemon.lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "LCU daemon already running or starting"
    );
    let socket = root.join("daemon.sock");
    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    let exit = CancellationToken::new();
    let creation = Arc::new(Mutex::new(()));
    let mut connections = tokio::task::JoinSet::new();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _=exit.cancelled()=>break,
            _=term.recv()=>break,
            _=tokio::signal::ctrl_c()=>break,
            done=connections.join_next(),if !connections.is_empty()=>{let _=done;},
            accepted=listener.accept()=>{
                let (stream, _) = accepted?;
                connections.spawn(serve_frontend(stream, exit.child_token(), creation.clone(), exit.clone()));
            }
        }
    }
    exit.cancel();
    while connections.join_next().await.is_some() {}
    std::fs::remove_file(socket)?;
    Ok(())
}
async fn serve_frontend(
    stream: tokio::net::UnixStream,
    life: CancellationToken,
    creation: Arc<Mutex<()>>,
    stop: CancellationToken,
) -> Result<()> {
    let conn = Arc::new(Connection {
        workers: Mutex::new(HashMap::new()),
        life: life.clone(),
    });
    let close_conn = conn.clone();
    let close_life = life.clone();
    // EOF closes worker sockets independently of executing requests or blocked writes.
    let closer = tokio::spawn(async move {
        close_life.cancelled().await;
        close_conn.close().await;
    });
    let handler = conn.clone();
    let result = rpc::serve(stream, life, move |request, ingress, cancel| {
        let handler = handler.clone();
        let creation = creation.clone();
        let stop = stop.clone();
        async move {
            handler
                .request(request, ingress, cancel, creation, stop)
                .await
        }
    })
    .await;
    conn.close().await;
    let _ = closer.await;
    result
}
pub async fn connect() -> Result<rpc::Client> {
    let path = runtime_root()?.join("daemon.sock");
    let client = rpc::Client::connect(&path).await?;
    client.handshake().await?;
    Ok(client)
}
pub async fn start() -> Result<()> {
    if connect().await.is_ok() {
        return Ok(());
    }
    let root = runtime_root()?;
    private_dir(&root)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("daemon.log"))?;
    let mut command = ProcessCommand::new(std::env::current_exe()?);
    command
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0);
    let mut child = command.spawn()?;
    let mut child_exit = None;
    for _ in 0..100 {
        if connect().await.is_ok() {
            return Ok(());
        }
        // A competing startup can own the lock before it binds the socket.
        // Our child's exit is not proof that no daemon will become ready.
        if child_exit.is_none() {
            child_exit = child.try_wait()?;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!(
        "Daemon did not become ready (spawned child exit: {child_exit:?}); inspect {}",
        root.join("daemon.log").display()
    )
}
/// Direct worker control: independent of a stalled daemon or MCP adapter.
pub async fn emergency_stop(
    id: Option<&str>,
) -> Result<Vec<std::result::Result<Response, String>>> {
    let mut jobs = tokio::task::JoinSet::new();
    let desktops = descriptors()?;
    ensure!(
        id.is_none_or(|id| desktops.iter().any(|d| d.id == id)),
        "Unknown desktop; no stop request sent"
    );
    for d in desktops
        .into_iter()
        .filter(|d| id.is_none_or(|id| d.id == id))
    {
        jobs.spawn(async move {
            let result = tokio::time::timeout(Duration::from_secs(30), async {
                let c = rpc::Client::connect(&d.socket()).await?;
                c.call(Request::Control {
                    desktop_id: d.id,
                    command: Command::Stop,
                })
                .await
            })
            .await;
            match result {
                Ok(r) => r.map_err(|e| format!("{e:#}")),
                Err(e) => Err(format!("Stop confirmation timed out: {e}")),
            }
        });
    }
    let mut results = vec![];
    while let Some(r) = jobs.join_next().await {
        results.push(r?);
    }
    Ok(results)
}
pub fn read_descriptor(path: &Path) -> Result<Descriptor> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn forwarding_preserves_first_reader_order_even_when_requests_overtake() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("worker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            rpc::serve_worker(
                stream,
                CancellationToken::new(),
                |_, ingress, _| async move {
                    Ok(Response::Hello {
                        wire_revision: WIRE_REVISION,
                        version: ingress.to_string(),
                    })
                },
            )
            .await
            .unwrap();
        });
        let client = rpc::Client::connect(&path).await.unwrap();
        let connection = Arc::new(Connection {
            workers: Mutex::new(HashMap::from([("test".into(), client)])),
            life: CancellationToken::new(),
        });
        let request = || Request::Control {
            desktop_id: "test".into(),
            command: Command::Status,
        };
        let (paused, entered) = tokio::sync::oneshot::channel();
        let (resume, wait) = tokio::sync::oneshot::channel();
        let c = connection.clone();
        let old = tokio::spawn(async move {
            paused.send(()).unwrap();
            wait.await.unwrap();
            c.request(
                request(),
                2,
                CancellationToken::new(),
                Arc::new(Mutex::new(())),
                CancellationToken::new(),
            )
            .await
            .unwrap()
        });
        entered.await.unwrap();
        let newer = connection
            .request(
                request(),
                4,
                CancellationToken::new(),
                Arc::new(Mutex::new(())),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(newer, Response::Hello { version, .. } if version == "4"));
        resume.send(()).unwrap();
        assert!(matches!(old.await.unwrap(), Response::Hello { version, .. } if version == "2"));
        connection.close().await;
        server.await.unwrap();
    }
}
