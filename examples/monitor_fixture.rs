//! Typed GUI transport fixture. No portals, desktop processes, input or user state.
//! Run with an explicit socket in a disposable directory. Sibling marker files
//! `fail-list` / `fail-observe` / `delay-observe` exercise transport/UI failures.
use anyhow::{Result, bail, ensure};
use linux_computer_use::{
    desktop::{self, DesktopInfo, DesktopOperation, Request, Response, rpc},
    types::{Command, Crop, Observation, ObservationInfo, Reply},
};
use std::{collections::HashSet, io::Write, path::PathBuf, sync::Arc};
use tokio::{net::UnixListener, sync::Mutex};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<()> {
    let socket = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("explicit fixture socket required"),
    );
    let root = socket.parent().unwrap().to_path_buf();
    ensure!(root.is_dir(), "Create a disposable fixture directory first");
    let listener = UnixListener::bind(&socket)?;
    let removed = Arc::new(Mutex::new(HashSet::<String>::new()));
    let trace = Arc::new(std::sync::Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("requests.jsonl"))?,
    ));
    let image = image::RgbImage::from_fn(640, 360, |x, y| {
        if x > 40 && x < 600 && y > 40 && y < 320 {
            image::Rgb([220, 232, 240])
        } else {
            image::Rgb([40, 65, 88])
        }
    });
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png)?;
    let png = Arc::new(png.into_inner());
    loop {
        let (stream, _) = listener.accept().await?;
        let root = root.clone();
        let removed = removed.clone();
        let png = png.clone();
        let trace = trace.clone();
        tokio::spawn(async move {
            let _ = rpc::serve(stream, CancellationToken::new(), move |request, _, cancel| {
                let root = root.clone();
                let removed = removed.clone();
                let png = png.clone();
                let trace = trace.clone();
                async move {
                    // Concurrent RPC handlers must publish whole trace lines.
                    writeln!(trace.lock().unwrap(), "{}", serde_json::to_string(&request)?)?;
                    match request {
                        Request::Hello => Ok(Response::Hello {
                            wire_revision: desktop::WIRE_REVISION,
                            version: "monitor-fixture".into(),
                        }),
                        Request::Desktop(DesktopOperation::List) => {
                            ensure!(!root.join("fail-list").exists(), "Fixture list unavailable");
                            let removed = removed.lock().await;
                            let names = ["Browser checks", "Godot workspace", "Settings check", "Docs preview",
                                "Layout review", "Input fixture", "<b>Plain text name</b>", "Render check", "Main"];
                            let count = std::fs::read_to_string(root.join("count")).ok().and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(8);
                            let mut desktops = Vec::new();
                            for (i, name) in names.iter().enumerate() {
                                let id = if i == 8 { "main".into() } else { format!("fixture-{i}") };
                                if removed.contains(&id) || (i != 8 && i >= count) { continue; }
                                desktops.push(DesktopInfo {
                                    descriptor: desktop::Descriptor {
                                        id, name: Some((*name).into()), runtime: root.clone(), state: root.clone(),
                                        width: 640, height: 360, owned: i != 8, ownership: None,
                                    },
                                    available: i != 7, claimed: i % 2 == 0,
                                    claim_age_ms: Some(1000), last_activity_age_ms: Some(1000),
                                    control_error: (i == 7).then(|| "Fixture worker unavailable".into()),
                                    applications: Vec::new(), resources: Default::default(),
                                });
                            }
                            // Real daemon listing order is not stable.
                            desktops.reverse();
                            Ok(Response::Desktops(desktops))
                        }
                        Request::Control { desktop_id, command: Command::Observe(args) } => {
                            ensure!(desktop_id != "main", "Monitor must not observe main");
                            ensure!(args.timeout_ms == Some(0) && args.after_sequence.is_none(), "Monitor must not wait for capture");
                            ensure!(matches!(args.max_dimension, Some(640 | 1600)), "Unexpected preview size");
                            ensure!(!root.join("fail-observe").exists(), "Fixture preview unavailable");
                            if root.join("delay-observe").exists() {
                                tokio::select! {
                                    _ = cancel.cancelled() => bail!("Fixture observation cancelled"),
                                    _ = tokio::time::sleep(std::time::Duration::from_secs(4)) => {}
                                }
                            }
                            Ok(Response::Control(Reply::Observation(Observation {
                                png: (*png).clone(),
                                info: ObservationInfo {
                                    frame_id: "fixture-static-frame".into(), display: 0,
                                    captured_at_ms: desktop::now_ms().saturating_sub(60000), age_ms: 60000,
                                    capture_sequence: 1, image_width: 640, image_height: 360,
                                    source_width: 640, source_height: 360,
                                    crop: Crop { x: 0, y: 0, width: 640, height: 360 },
                                    logical_size: Some((640, 360)), new_frame_after_input: None,
                                    freshness_met: None, requested_after_sequence: None, wait_timed_out: false,
                                },
                            })))
                        }
                        Request::Desktop(DesktopOperation::Destroy { desktop_id, force }) => {
                            ensure!(desktop_id != "main" && force, "Only explicit owned destruction");
                            removed.lock().await.insert(desktop_id.clone());
                            Ok(Response::Destroyed { desktop_id })
                        }
                        _ => bail!("Forbidden monitor request"),
                    }
                }
            }).await;
        });
    }
}
