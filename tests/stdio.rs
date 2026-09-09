//! Real transports in a private discovery namespace; never authorize desktop input.
use linux_computer_use::desktop::{Descriptor, DesktopOperation, Request, Response, rpc::Client};
use serde_json::{Value, json};
use std::process::Stdio;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::{Duration, timeout},
};
fn binary() -> String {
    std::env::var("LCU_TEST_BINARY").unwrap_or_else(|_| env!("CARGO_BIN_EXE_lcu").into())
}
struct Rig {
    root: tempfile::TempDir,
    worker: Child,
    daemon: Child,
}
impl Rig {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("run");
        let state = root.path().join("state");
        let d = Descriptor {
            name: None,
            id: "main".into(),
            runtime: runtime.join("desktops/main"),
            state: state.join("main"),
            width: 1280,
            height: 720,
            owned: false,
            ownership: Some(linux_computer_use::desktop::ownership::Ownership::new().unwrap()),
        };
        std::fs::create_dir_all(&d.runtime).unwrap();
        std::fs::create_dir_all(&d.state).unwrap();
        let path = d.state.join("descriptor.json");
        std::fs::write(&path, serde_json::to_vec(&d).unwrap()).unwrap();
        let lock = linux_computer_use::desktop::ownership::LifecycleLock::acquire(&d).unwrap();
        let fd = lock.fd();
        let mut command = Command::new(binary());
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let worker = command
            .arg("desktop-worker")
            .arg(path)
            .arg("--lifecycle-fd")
            .arg(fd.to_string())
            .env("LCU_RUNTIME_DIR", &runtime)
            .env("LCU_STATE_DIR", &state)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        wait_socket(&d.socket()).await;
        let daemon = Command::new(binary())
            .args(["daemon", "run"])
            .env("LCU_RUNTIME_DIR", &runtime)
            .env("LCU_STATE_DIR", &state)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        wait_socket(&runtime.join("daemon.sock")).await;
        Self {
            root,
            worker,
            daemon,
        }
    }
    fn start(&self, mode: &str) -> Process {
        let mut child = Command::new(binary())
            .arg(mode)
            .env("LCU_RUNTIME_DIR", self.root.path().join("run"))
            .env("LCU_STATE_DIR", self.root.path().join("state"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        Process {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
        }
    }
    async fn client(&self) -> Client {
        Client::connect(&self.root.path().join("run/daemon.sock"))
            .await
            .unwrap()
    }
    async fn finish(mut self) {
        self.daemon.kill().await.unwrap();
        self.worker.kill().await.unwrap();
    }
}
async fn wait_socket(path: &std::path::Path) {
    timeout(Duration::from_secs(5), async {
        loop {
            if Client::connect(path).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
struct Process {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Process {
    async fn send(&mut self, value: Value) {
        self.input
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        self.input.flush().await.unwrap();
    }
    async fn line(&mut self) -> Value {
        let mut line = String::new();
        let bytes = timeout(Duration::from_secs(5), self.output.read_line(&mut line))
            .await
            .expect("process response timeout")
            .unwrap();
        assert!(bytes > 0, "process exited before response");
        serde_json::from_str(&line).expect("non-protocol stdout")
    }
    async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        loop {
            let reply = self.line().await;
            if reply.get("id") == Some(&json!(id)) {
                return reply;
            }
        }
    }
    async fn finish(mut self) {
        self.input.shutdown().await.unwrap();
        drop(self.input);
        assert!(
            timeout(Duration::from_secs(5), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}
#[tokio::test]
async fn redirected_replies_preserve_offsets_and_append() {
    let rig = Rig::new().await;
    for append in [false, true] {
        let path = rig.root.path().join("replies.jsonl");
        std::fs::write(
            &path,
            if append {
                b"existing\n".as_slice()
            } else {
                b""
            },
        )
        .unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .append(append)
            .open(&path)
            .unwrap();
        let mut child = Command::new(binary())
            .arg("session")
            .env("LCU_RUNTIME_DIR", rig.root.path().join("run"))
            .env("LCU_STATE_DIR", rig.root.path().join("state"))
            .stdin(Stdio::piped())
            .stdout(file)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        for (index, command) in ["first-invalid", "second-invalid"].iter().enumerate() {
            input
                .write_all(format!("{{\"command\":\"{command}\"}}\n").as_bytes())
                .await
                .unwrap();
            // Wait for completed output before another reply reopens stdout.
            timeout(Duration::from_secs(5), async {
                loop {
                    let text = std::fs::read_to_string(&path).unwrap();
                    if text.lines().count() == index + 1 + usize::from(append) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        drop(input);
        assert!(
            timeout(Duration::from_secs(5), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2 + usize::from(append));
        if append {
            assert_eq!(lines[0], "existing");
        }
        for line in &lines[usize::from(append)..] {
            assert!(serde_json::from_str::<Value>(line).unwrap()["error"].is_string());
        }
    }
    rig.finish().await;
}

#[tokio::test]
async fn global_stop_reaches_legacy_despite_unavailable_worker() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let state = root.path().join("state");
    let legacy_dir = runtime.join("linux-computer-use");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    std::fs::create_dir_all(state.join("missing")).unwrap();
    let descriptor = Descriptor {
        name: None,
        id: "missing".into(),
        runtime: root.path().join("absent"),
        state: state.join("missing"),
        width: 100,
        height: 100,
        owned: true,
        ownership: None,
    };
    std::fs::write(
        descriptor.state.join("descriptor.json"),
        serde_json::to_vec(&descriptor).unwrap(),
    )
    .unwrap();
    let listener = tokio::net::UnixListener::bind(legacy_dir.join("fixture.sock")).unwrap();
    let responder = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 4];
        stream.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"stop");
        stream
            .write_all(br#"{"controller_halted":true,"desktop_close_confirmed":true}"#)
            .await
            .unwrap();
    });
    let output = Command::new(binary())
        .arg("stop")
        .env_remove("LCU_RUNTIME_DIR")
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("LCU_STATE_DIR", &state)
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    timeout(Duration::from_secs(2), responder)
        .await
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("\"controllers_halted\":1")
    );
}

#[tokio::test]
async fn losing_daemon_child_waits_for_winner_readiness() {
    use std::os::fd::AsRawFd;
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let state = root.path().join("state");
    std::fs::create_dir_all(&runtime).unwrap();
    let lock = std::fs::File::create(runtime.join("daemon.lock")).unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let child = Command::new(binary())
        .args(["daemon", "start"])
        .env("LCU_RUNTIME_DIR", &runtime)
        .env("LCU_STATE_DIR", &state)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // A real losing daemon child must encounter the fixture-held startup lock.
    timeout(Duration::from_secs(5), async {
        loop {
            if std::fs::read_to_string(runtime.join("daemon.log"))
                .unwrap_or_default()
                .contains("LCU daemon already running or starting")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // Keep the winner's socket absent beyond the losing child's exit.
    tokio::time::sleep(Duration::from_millis(150)).await;
    drop(lock);
    let mut winner = Command::new(binary())
        .args(["daemon", "run"])
        .env("LCU_RUNTIME_DIR", &runtime)
        .env("LCU_STATE_DIR", &state)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let output = timeout(Duration::from_secs(7), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    winner.kill().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn concurrent_cold_daemon_start_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let state = root.path().join("state");
    let mut children = Vec::new();
    for _ in 0..16 {
        children.push(
            Command::new(binary())
                .args(["daemon", "start"])
                .env("LCU_RUNTIME_DIR", &runtime)
                .env("LCU_STATE_DIR", &state)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap(),
        );
    }
    let mut failures = Vec::new();
    for child in children {
        let output = timeout(Duration::from_secs(10), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        if !output.status.success() {
            failures.push(String::from_utf8_lossy(&output.stderr).into_owned());
        }
    }
    let client = Client::connect(&runtime.join("daemon.sock")).await.unwrap();
    client.call(Request::ShutdownDaemon).await.unwrap();
    assert!(failures.is_empty(), "{failures:?}");
}

#[tokio::test]
async fn mcp_handshake_tools_and_errors_over_real_stdio() {
    let rig = Rig::new().await;
    let mut p = rig.start("mcp");
    let init=p.request(1,"initialize",json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"lcu-test","version":"1"}})).await;
    assert_eq!(
        init["result"]["serverInfo"]["name"], "linux-computer-use",
        "{init}"
    );
    p.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .await;
    let list = p.request(2, "tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 8);
    for name in [
        "computer_start",
        "computer_observe",
        "computer_act",
        "computer_status",
        "computer_stop",
        "computer_inspect",
        "computer_focus",
        "computer_desktop",
    ] {
        assert!(tools.iter().any(|t| t["name"] == name));
    }
    let observe = tools
        .iter()
        .find(|t| t["name"] == "computer_observe")
        .unwrap();
    for name in ["desktop_id", "crop", "after_sequence", "timeout_ms"] {
        assert!(
            observe["inputSchema"]["properties"].get(name).is_some(),
            "{observe}"
        );
    }
    let act = tools.iter().find(|t| t["name"] == "computer_act").unwrap();
    let schema = act["inputSchema"].to_string();
    for name in [
        "keycodes",
        "paste_keycodes",
        "expected_focus_node_id",
        "feedback_timeout_ms",
    ] {
        assert!(schema.contains(name), "missing {name}");
    }
    let status = p
        .request(
            3,
            "tools/call",
            json!({"name":"computer_status","arguments":{"desktop_id":"main"}}),
        )
        .await;
    let status: Value =
        serde_json::from_str(status["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(status["result"]["active"], false);
    let action=p.request(4,"tools/call",json!({"name":"computer_act","arguments":{"desktop_id":"main","frame_id":"invented","action":{"type":"keypress","keys":["ENTER"]}}})).await;
    assert_eq!(action["result"]["isError"], true);
    assert!(
        action["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Claim")
    );
    let claimed = p.request(5, "tools/call", json!({"name":"computer_desktop","arguments":{"operation":"claim","desktop_id":"main"}})).await;
    assert_ne!(claimed["result"]["isError"], true, "{claimed}");
    let direct = rig.client().await;
    let claim = || {
        Request::Desktop(DesktopOperation::Claim {
            desktop_id: "main".into(),
            force: false,
        })
    };
    assert!(
        direct.call(claim()).await.is_err(),
        "MCP and direct clients must share one claim authority"
    );
    p.finish().await;
    timeout(Duration::from_secs(3), async {
        while direct.call(claim()).await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    direct.disconnect();
    rig.finish().await;
}
#[tokio::test]
async fn cli_bad_input_and_sigterm_with_stdin_open() {
    let rig = Rig::new().await;
    let mut p = rig.start("session");
    p.send(json!({"command":"not-a-command"})).await;
    assert!(p.line().await["error"].is_string());
    p.send(json!({"command":"status"})).await;
    assert_eq!(p.line().await["result"]["active"], false);
    p.send(json!({"command":"observe","display":0,"output":"/not-written.png"}))
        .await;
    assert!(
        p.line().await["error"]
            .as_str()
            .unwrap()
            .contains("No desktop session")
    );
    unsafe {
        libc::kill(p.child.id().unwrap() as i32, libc::SIGTERM);
    }
    assert!(
        timeout(Duration::from_secs(3), p.child.wait())
            .await
            .expect("SIGTERM hung with stdin open")
            .unwrap()
            .success()
    );
    drop(p);
    rig.finish().await;
}
#[tokio::test]
async fn competing_claims_force_fencing_disconnect_and_daemon_restart() {
    let mut rig = Rig::new().await;
    let a = rig.client().await;
    let b = rig.client().await;
    let claim = |force| {
        Request::Desktop(DesktopOperation::Claim {
            desktop_id: "main".into(),
            force,
        })
    };
    a.call(claim(false)).await.unwrap();
    assert!(b.call(claim(false)).await.is_err());
    b.call(claim(true)).await.unwrap();
    assert!(
        a.call(Request::Desktop(DesktopOperation::Release {
            desktop_id: "main".into()
        }))
        .await
        .is_err()
    );
    b.disconnect();
    timeout(Duration::from_secs(3), async {
        while a.call(claim(false)).await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    rig.daemon.kill().await.unwrap();
    let mut daemon = Command::new(binary())
        .args(["daemon", "run"])
        .env("LCU_RUNTIME_DIR", rig.root.path().join("run"))
        .env("LCU_STATE_DIR", rig.root.path().join("state"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_socket(&rig.root.path().join("run/daemon.sock")).await;
    let c = rig.client().await;
    timeout(Duration::from_secs(3), async {
        while c.call(claim(false)).await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let Response::Desktops(desktops) = c
        .call(Request::Desktop(DesktopOperation::List))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(desktops.len(), 1);
    assert!(desktops[0].available);
    c.disconnect();
    daemon.kill().await.unwrap();
    rig.worker.kill().await.unwrap();
}
