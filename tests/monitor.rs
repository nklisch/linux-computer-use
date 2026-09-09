//! Monitor entrypoint and descriptor compatibility; no desktop or daemon access.
use linux_computer_use::desktop::{Descriptor, DesktopOperation};
use serde_json::json;
use std::{os::unix::fs::PermissionsExt, process::Command};

#[test]
fn optional_names_preserve_old_descriptors_and_creation_requests() {
    let old = json!({"id":"desktop-test","runtime":"/tmp/run","state":"/tmp/state",
        "width":1280,"height":720,"owned":true});
    let mut descriptor: Descriptor = serde_json::from_value(old).unwrap();
    assert!(descriptor.name.is_none());
    descriptor.name = Some("Browser checks".into());
    let decoded: Descriptor =
        serde_json::from_slice(&serde_json::to_vec(&descriptor).unwrap()).unwrap();
    assert_eq!(decoded.name.as_deref(), Some("Browser checks"));
    let create: DesktopOperation = serde_json::from_value(json!({"operation":"create"})).unwrap();
    assert!(matches!(
        create,
        DesktopOperation::Create { name: None, .. }
    ));
    let create: DesktopOperation =
        serde_json::from_value(json!({"operation":"create","name":"Scene review"})).unwrap();
    assert!(
        matches!(create, DesktopOperation::Create { name: Some(name), .. } if name == "Scene review")
    );
}

#[test]
fn monitor_launch_passes_private_socket_without_starting_any_daemon() {
    let root = tempfile::tempdir().unwrap();
    let binary = root.path().join("lcu");
    std::fs::copy(env!("CARGO_BIN_EXE_lcu"), &binary).unwrap();
    let companion = root.path().join("lcu-monitor");
    std::fs::write(&companion, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
    std::fs::set_permissions(&companion, std::fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = root.path().join("unused-runtime");
    let state = root.path().join("unused-state");
    let output = Command::new(&binary)
        .arg("monitor")
        .env("LCU_RUNTIME_DIR", &runtime)
        .env("LCU_STATE_DIR", &state)
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("--socket\n{}\n", runtime.join("daemon.sock").display())
    );
    assert!(!runtime.exists());
    assert!(!state.exists());
    std::fs::remove_file(companion).unwrap();
    let output = Command::new(binary)
        .arg("monitor")
        .env("LCU_RUNTIME_DIR", &runtime)
        .env("LCU_STATE_DIR", &state)
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!runtime.exists());
    assert!(!state.exists());
}
