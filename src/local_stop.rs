//! Out-of-band local stop remains reachable while a tool call waits for the desktop.
use crate::{controller::ControllerHandle, types::StopReport};
use anyhow::{Context, Result};
use serde::Serialize;
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    task::JoinSet,
    time::timeout,
};

fn directory() -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR unavailable")?)
            .join("linux-computer-use"),
    )
}
pub struct StopListener {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}
impl StopListener {
    pub fn bind(controller: ControllerHandle) -> Result<Self> {
        let dir = directory()?;
        std::fs::create_dir_all(&dir)?;
        // A desktop-control stop endpoint belongs only to this user, never the network.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let path = dir.join(format!("{}.sock", std::process::id()));
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path)?;
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let controller = controller.clone();
                tokio::spawn(async move {
                    let mut request = [0; 4];
                    if timeout(Duration::from_secs(2), stream.read_exact(&mut request))
                        .await
                        .is_ok_and(|r| r.is_ok())
                        && &request == b"stop"
                    {
                        controller.shutdown();
                        let report = controller.stopped().await;
                        if let Ok(mut response) = serde_json::to_vec(&report) {
                            response.push(b'\n');
                            let _ =
                                timeout(Duration::from_secs(2), stream.write_all(&response)).await;
                        }
                    }
                });
            }
        });
        Ok(Self { path, task })
    }
}
impl Drop for StopListener {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug, Default, Serialize)]
pub struct StopSummary {
    pub controllers_halted: usize,
    pub desktop_closures_confirmed: usize,
    pub errors: Vec<String>,
}

pub async fn stop_all() -> Result<StopSummary> {
    let entries = match std::fs::read_dir(directory()?) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(StopSummary::default()),
        Err(e) => return Err(e.into()),
    };
    let mut paths = vec![];
    let mut scan_errors = vec![];
    for entry in entries {
        match entry {
            Ok(entry) => {
                let path = entry.path();
                if path.extension().is_some_and(|s| s == "sock") {
                    paths.push(path);
                }
            }
            Err(error) => scan_errors.push(format!("Cannot inspect a stop endpoint: {error}")),
        }
    }
    let mut summary = stop_paths(paths, Duration::from_secs(30)).await;
    summary.errors.extend(scan_errors);
    Ok(summary)
}

async fn stop_paths(paths: Vec<PathBuf>, budget: Duration) -> StopSummary {
    let mut requests = JoinSet::new();
    // Every endpoint receives an independent request. One stalled controller must
    // never stop us asking all the others to release their real desktop inputs.
    for path in paths {
        requests.spawn(async move {
            let result = timeout(budget, request_stop(&path))
                .await
                .map_err(|_| {
                    anyhow::anyhow!("stop requested but shutdown acknowledgement timed out")
                })
                .and_then(|result| result);
            (path, result)
        });
    }
    let mut summary = StopSummary::default();
    while let Some(result) = requests.join_next().await {
        match result {
            Ok((path, Ok(Some(report)))) => {
                if report.controller_halted {
                    summary.controllers_halted += 1;
                }
                if report.desktop_close_confirmed {
                    summary.desktop_closures_confirmed += 1;
                }
                if !report.controller_halted || !report.desktop_close_confirmed {
                    summary.errors.push(format!(
                        "{}: {}",
                        path.display(),
                        report
                            .error
                            .unwrap_or_else(|| "Desktop closure not confirmed".into())
                    ));
                }
            }
            Ok((_, Ok(None))) => {} // stale endpoint from a process that is no longer listening
            Ok((path, Err(error))) => summary
                .errors
                .push(format!("{}: {error:#}", path.display())),
            Err(error) => summary
                .errors
                .push(format!("Stop request task failed: {error}")),
        }
    }
    summary
}
async fn request_stop(path: &std::path::Path) -> Result<Option<StopReport>> {
    let mut stream = match UnixStream::connect(path).await {
        Ok(s) => s,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e).context("Connecting to desktop controller"),
    };
    stream.write_all(b"stop").await?;
    let mut response = String::new();
    // The response is a small status record, not screen content.
    stream.take(16384).read_to_string(&mut response).await?;
    Ok(Some(serde_json::from_str(&response).context(
        "Controller did not return a valid shutdown confirmation",
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn unresponsive_controller_does_not_prevent_stopping_others() {
        let tmp = tempfile::tempdir().unwrap();
        let slow_path = tmp.path().join("slow.sock");
        let fast_path = tmp.path().join("fast.sock");
        let slow = UnixListener::bind(&slow_path).unwrap();
        let fast = UnixListener::bind(&fast_path).unwrap();
        let slow_worker = tokio::spawn(async move {
            let (mut stream, _) = slow.accept().await.unwrap();
            let mut request = [0; 4];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"stop");
            std::future::pending::<()>().await;
        });
        let fast_worker = tokio::spawn(async move {
            let (mut stream, _) = fast.accept().await.unwrap();
            let mut request = [0; 4];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"stop");
            stream
                .write_all(br#"{"controller_halted":true,"desktop_close_confirmed":true}"#)
                .await
                .unwrap();
        });
        let summary = stop_paths(vec![slow_path, fast_path], Duration::from_secs(1)).await;
        assert_eq!(summary.controllers_halted, 1);
        assert_eq!(summary.desktop_closures_confirmed, 1);
        assert_eq!(summary.errors.len(), 1);
        fast_worker.await.unwrap();
        slow_worker.abort();
    }
    #[tokio::test]
    async fn unconfirmed_closure_is_not_counted_as_confirmed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("failed.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let worker = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4];
            stream.read_exact(&mut request).await.unwrap();
            stream.write_all(br#"{"controller_halted":true,"desktop_close_confirmed":false,"error":"Close timed out"}"#).await.unwrap();
        });
        let summary = stop_paths(vec![path], Duration::from_secs(1)).await;
        assert_eq!(summary.controllers_halted, 1);
        assert_eq!(summary.desktop_closures_confirmed, 0);
        assert_eq!(summary.errors.len(), 1);
        assert!(summary.errors[0].contains("Close timed out"));
        worker.await.unwrap();
    }
}
