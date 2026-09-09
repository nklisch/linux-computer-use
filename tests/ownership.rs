//! Disposable process recovery: no portal or real desktop access.
use linux_computer_use::desktop::{
    self, Descriptor,
    gate::Pending,
    ownership::{self, LifecycleLock, Ownership},
};
use std::{
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Command, Stdio},
    time::Duration,
};
fn descriptor(root: &std::path::Path) -> Descriptor {
    let d = Descriptor {
        name: None,
        id: "probe".into(),
        runtime: root.join("run/desktops/probe"),
        state: root.join("state/probe"),
        width: 100,
        height: 100,
        owned: true,
        ownership: Some(Ownership::new().unwrap()),
    };
    desktop::private_dir(&d.runtime).unwrap();
    desktop::private_dir(&d.state).unwrap();
    ownership::publish(&d).unwrap();
    d
}
fn pending(args: &[&str]) -> Pending {
    let helper = std::path::Path::new(env!("CARGO_BIN_EXE_lcu"));
    let mut cmd = Command::new(helper);
    cmd.env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Pending::spawn(
        helper,
        cmd,
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )
    .unwrap()
}
#[test]
fn gate_eof_before_and_after_publication_never_executes_payload() {
    for publish in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut d = descriptor(root.path());
        let marker = root.path().join("payload");
        let (channel, child_channel) = UnixStream::pair().unwrap();
        let fd = child_channel.as_raw_fd();
        let mut command = Command::new(env!("CARGO_BIN_EXE_lcu"));
        command
            .args([
                "desktop-exec-gate",
                "--gate-fd",
                &fd.to_string(),
                "--",
                "/usr/bin/touch",
            ])
            .arg(&marker);
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        drop(child_channel);
        if publish {
            ownership::remember(&mut d, [ownership::identity(child.id()).unwrap()]).unwrap();
        }
        drop(channel); // Exactly the EOF delivered when the worker crashes.
        assert!(child.wait().unwrap().success());
        assert!(!marker.exists());
    }
}
#[test]
fn publication_failure_and_target_exec_failure_are_not_launch_success() {
    let root = tempfile::tempdir().unwrap();
    let mut d = descriptor(root.path());
    let marker = root.path().join("payload");
    let p = pending(&["/usr/bin/touch", marker.to_str().unwrap()]);
    std::fs::remove_file(d.state.join("descriptor.json")).unwrap();
    assert!(ownership::remember(&mut d, [ownership::identity(p.pid()).unwrap()]).is_err());
    drop(p);
    assert!(!marker.exists());
    let error = pending(&["/definitely/no/lcu-test-executable"])
        .authorize()
        .unwrap_err();
    assert!(error.to_string().contains("Target exec failed"));
}
#[tokio::test]
async fn authorized_root_without_tag_is_recovered_and_payload_does_not_hold_lock() {
    let root = tempfile::tempdir().unwrap();
    let mut d = descriptor(root.path());
    let lock = LifecycleLock::acquire(&d).unwrap();
    assert!(LifecycleLock::acquire(&d).is_err());
    let p = pending(&["/usr/bin/sleep", "30"]);
    let i = ownership::identity(p.pid()).unwrap();
    ownership::remember(&mut d, [i.clone()]).unwrap();
    let mut child = p.authorize().unwrap();
    drop(lock);
    // Parallel tests can fork while this descriptor is open. CLOEXEC releases
    // those transient inherited copies at exec; the sleep payload must not hold
    // the lock for its 30-second lifetime. Pin bounded eventual release rather
    // than assuming other threads cannot be between fork and exec right now.
    let _recovery = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(lock) = LifecycleLock::acquire(&d) {
                break lock;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("executed payload must not retain the lifecycle lock");
    ownership::cleanup(&mut d, false).await.unwrap();
    child.wait().unwrap();
    assert!(ownership::identity(i.pid).is_err());
    ownership::cleanup(&mut d, false).await.unwrap(); // Interrupted/finished retry is harmless.
    desktop::remove_resources(&d).unwrap();
}
#[tokio::test]
async fn wrong_boot_never_signals_current_process_and_missing_records_fail() {
    let root = tempfile::tempdir().unwrap();
    let mut d = descriptor(root.path());
    d.ownership.as_mut().unwrap().boot = "not-this-boot".into();
    d.ownership
        .as_mut()
        .unwrap()
        .processes
        .push(ownership::identity(std::process::id()).unwrap());
    ownership::publish(&d).unwrap();
    ownership::cleanup(&mut d, false).await.unwrap();
    d.ownership = None;
    ownership::publish(&d).unwrap();
    assert!(ownership::cleanup(&mut d, false).await.is_err());
    assert!(d.state.join("descriptor.json").exists());
}
#[tokio::test]
async fn live_unreachable_worker_lock_excludes_force_recovery() {
    let root = tempfile::tempdir().unwrap();
    let d = descriptor(root.path());
    let lock = LifecycleLock::acquire(&d).unwrap();
    let marker = d.state.join("profile");
    std::fs::write(&marker, "keep").unwrap();
    let mut daemon = tokio::process::Command::new(env!("CARGO_BIN_EXE_lcu"))
        .args(["daemon", "run"])
        .env("LCU_RUNTIME_DIR", root.path().join("run"))
        .env("LCU_STATE_DIR", root.path().join("state"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let socket = root.path().join("run/daemon.sock");
    let client = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(c) = desktop::rpc::Client::connect(&socket).await {
                break c;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let request = desktop::Request::Desktop(desktop::DesktopOperation::Destroy {
        desktop_id: d.id.clone(),
        force: true,
    });
    assert!(
        client
            .call(request.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("starting/live worker")
    );
    assert!(marker.exists());
    drop(lock);
    assert!(matches!(
        client.call(request).await.unwrap(),
        desktop::Response::Destroyed { .. }
    ));
    assert!(!d.state.exists());
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

#[tokio::test]
async fn cached_dead_worker_transport_can_retry_force_recovery() {
    let root = tempfile::tempdir().unwrap();
    let d = descriptor(root.path());
    let lock = LifecycleLock::acquire(&d).unwrap();
    let listener = tokio::net::UnixListener::bind(d.socket()).unwrap();
    let life = tokio_util::sync::CancellationToken::new();
    let server_life = life.clone();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        desktop::rpc::serve(socket, server_life, |r, _, _| async move {
            Ok(match r {
                desktop::Request::Hello => desktop::Response::Hello {
                    wire_revision: desktop::WIRE_REVISION,
                    version: "test".into(),
                },
                _ => desktop::Response::Claimed {
                    desktop_id: "probe".into(),
                },
            })
        })
        .await
        .unwrap();
    });
    let mut daemon = tokio::process::Command::new(env!("CARGO_BIN_EXE_lcu"))
        .args(["daemon", "run"])
        .env("LCU_RUNTIME_DIR", root.path().join("run"))
        .env("LCU_STATE_DIR", root.path().join("state"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let socket = root.path().join("run/daemon.sock");
    let client = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(c) = desktop::rpc::Client::connect(&socket).await {
                break c;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    client
        .call(desktop::Request::Desktop(
            desktop::DesktopOperation::Claim {
                desktop_id: d.id.clone(),
                force: false,
            },
        ))
        .await
        .unwrap();
    life.cancel();
    server.await.unwrap();
    let destroy = desktop::Request::Desktop(desktop::DesktopOperation::Destroy {
        desktop_id: d.id.clone(),
        force: true,
    });
    assert!(
        client
            .call(destroy.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("starting/live worker")
    );
    assert!(d.state.join("descriptor.json").exists());
    drop(lock);
    assert!(matches!(
        client.call(destroy).await.unwrap(),
        desktop::Response::Destroyed { .. }
    ));
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}
