//! Write-only portal clipboard ownership; the previous selection is never inspected.
use crate::portal::{CALL_TIMEOUT, DEST, Options, PATH, call};
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::{
    io::AsyncWriteExt,
    net::unix::pipe,
    sync::RwLock,
    task::{JoinHandle, JoinSet},
    time::{Duration, interval, timeout},
};
use zbus::{
    Connection, Proxy,
    zvariant::{OwnedObjectPath, Value},
};

const INTERFACE: &str = "org.freedesktop.portal.Clipboard";
const MIME_TYPES: [&str; 2] = ["text/plain;charset=utf-8", "text/plain"];

pub(crate) struct Clipboard {
    connection: Connection,
    session: OwnedObjectPath,
    text: Arc<RwLock<Option<Arc<str>>>>,
    closed: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl Clipboard {
    /// Must run before RemoteDesktop.Start, using the session's original connection.
    pub(crate) async fn prepare(
        connection: &Connection,
        session: &OwnedObjectPath,
        closed: Arc<AtomicBool>,
    ) -> Result<Self> {
        let proxy = Proxy::new(connection, DEST, PATH, INTERFACE).await?;
        let mut signals =
            timeout(CALL_TIMEOUT, proxy.receive_signal("SelectionTransfer")).await??;
        call::<_, ()>(
            connection,
            PATH,
            INTERFACE,
            "RequestClipboard",
            &(session, Options::new()),
        )
        .await?;
        let text = Arc::new(RwLock::new(None::<Arc<str>>));
        let worker_connection = connection.clone();
        let worker_session = session.clone();
        let worker_text = text.clone();
        let worker_closed = closed.clone();
        let task = tokio::spawn(async move {
            // A slow reader must not stop us serving other selection requests.
            // JoinSet owns all writers, so dropping/aborting this task cancels their I/O too.
            let mut writes = JoinSet::new();
            let mut closure_check = interval(Duration::from_millis(250));
            loop {
                tokio::select! {
                    _ = closure_check.tick() => {
                        if worker_closed.load(Ordering::Acquire) { break; }
                    }
                    _ = writes.join_next(), if !writes.is_empty() => {}
                    message = signals.next() => {
                        let Some(message) = message else { break; };
                        let Ok((session, mime, serial)) = message.body().deserialize::<(OwnedObjectPath, String, u32)>() else { continue; };
                        if session != worker_session { continue; }
                        let text = worker_text.read().await.clone();
                        let connection = worker_connection.clone();
                        let session = worker_session.clone();
                        writes.spawn(async move {
                            let result = async {
                                ensure!(MIME_TYPES.contains(&mime.as_str()), "unsupported clipboard MIME type");
                                let text = text.context("no text selection is retained")?;
                                let fd: zbus::zvariant::OwnedFd = call(&connection, PATH, INTERFACE, "SelectionWrite", &(&session, serial)).await?;
                                write_text(fd.into(), &text).await
                            }.await;
                            // Even unsupported requests and failed writes need a completion response.
                            let _ = call::<_, ()>(&connection, PATH, INTERFACE, "SelectionWriteDone", &(&session, serial, result.is_ok())).await;
                            if let Err(error) = result { eprintln!("Clipboard transfer failed: {error:#}"); }
                        });
                    }
                }
            }
        });
        Ok(Self {
            connection: connection.clone(),
            session: session.clone(),
            text,
            closed,
            task,
        })
    }

    pub(crate) async fn set_text(&self, text: &str) -> Result<()> {
        ensure!(
            !self.closed.load(Ordering::Acquire),
            "desktop portal session is closed"
        );
        ensure!(
            !self.task.is_finished(),
            "clipboard signal listener is unavailable"
        );
        // Lock across SetSelection so a fast SelectionTransfer cannot see the previous text.
        // This also serializes concurrent writers. We retain data until replacement or close.
        let mut current = self.text.write().await;
        let previous = current.replace(Arc::from(text));
        let options = Options::from([("mime_types", Value::from(MIME_TYPES.to_vec()))]);
        let result = call::<_, ()>(
            &self.connection,
            PATH,
            INTERFACE,
            "SetSelection",
            &(&self.session, options),
        )
        .await;
        if result.is_err() {
            *current = previous;
        }
        result
    }

    pub(crate) fn stop(&self) {
        self.task.abort();
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn write_text(fd: std::os::fd::OwnedFd, text: &str) -> Result<()> {
    // The portal supplies a pipe. Tokio sets O_NONBLOCK and owns the fd, unlike
    // spawn_blocking writes which cannot be cancelled when a paste reader stalls.
    let mut writer = pipe::Sender::from_owned_fd(fd)
        .context("clipboard SelectionWrite did not return a writable pipe")?;
    timeout(CALL_TIMEOUT, writer.write_all(text.as_bytes()))
        .await
        .context("clipboard reader stalled")??;
    // Drop closes the write end and supplies EOF before SelectionWriteDone.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn transfers_unicode_and_closes_pipe() {
        let (sender, mut receiver) = pipe::pipe().unwrap();
        let expected = "Hello, 世界 — café 🦀\n".repeat(10000);
        let fd = sender.into_blocking_fd().unwrap();
        let copy = expected.clone();
        let writer = tokio::spawn(async move { write_text(fd, &copy).await });
        let mut received = String::new();
        timeout(
            Duration::from_secs(5),
            receiver.read_to_string(&mut received),
        )
        .await
        .unwrap()
        .unwrap();
        writer.await.unwrap().unwrap();
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn reports_closed_reader() {
        let (sender, receiver) = pipe::pipe().unwrap();
        drop(receiver);
        assert!(
            write_text(sender.into_blocking_fd().unwrap(), "text")
                .await
                .is_err()
        );
    }
}
