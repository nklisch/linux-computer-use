//! A RemoteDesktop session and its ScreenCast/Clipboard extensions on one bus connection.
use crate::clipboard::Clipboard;
use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::{HashMap, hash_map::RandomState},
    hash::BuildHasher,
    os::fd::OwnedFd,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Mutex, task::JoinHandle, time::timeout};
use zbus::{
    Connection, Proxy,
    zvariant::{OwnedObjectPath, OwnedValue, Type, Value},
};

pub(crate) const DEST: &str = "org.freedesktop.portal.Desktop";
pub(crate) const PATH: &str = "/org/freedesktop/portal/desktop";
const RD: &str = "org.freedesktop.portal.RemoteDesktop";
const SC: &str = "org.freedesktop.portal.ScreenCast";
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(15);
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) type Options<'a> = HashMap<&'a str, Value<'a>>;
type Results = HashMap<String, OwnedValue>;
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize)]
pub struct DesktopStream {
    pub node_id: u32,
    pub logical_size: Option<(u32, u32)>,
    pub position: Option<(i32, i32)>,
}

pub struct Portal {
    connection: Connection,
    session: OwnedObjectPath,
    pub streams: Vec<DesktopStream>,
    pub restore_token: Option<String>,
    pub clipboard_available: bool,
    closed: Arc<AtomicBool>,
    closed_task: Option<JoinHandle<()>>,
    remote_closed: Arc<AtomicBool>,
    // Serialize Close calls and remember only confirmed success. Cancellation or
    // a transport failure leaves Drop able to retry the remote cleanup.
    close_succeeded: Mutex<bool>,
    clipboard: Option<Clipboard>,
}

pub(crate) async fn call<B, R>(
    connection: &Connection,
    path: &str,
    interface: &str,
    method: &str,
    body: &B,
) -> Result<R>
where
    B: Serialize + zbus::zvariant::DynamicType,
    R: DeserializeOwned + Type,
{
    let reply = timeout(
        CALL_TIMEOUT,
        connection.call_method(Some(DEST), path, Some(interface), method, body),
    )
    .await
    .with_context(|| format!("{method} timed out"))??;
    reply
        .body()
        .deserialize()
        .with_context(|| format!("invalid {method} reply"))
}

fn token() -> String {
    format!(
        "lcu_{}_{:016x}",
        NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
        RandomState::new().hash_one(std::process::id())
    )
}
fn handle_path(connection: &Connection, kind: &str, token: &str) -> Result<OwnedObjectPath> {
    let sender = connection
        .unique_name()
        .context("session bus has no unique name")?
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");
    Ok(format!("{PATH}/{kind}/{sender}/{token}").try_into()?)
}

// Cancelling the Rust future must dismiss an outstanding approval dialog as well.
struct PendingRequest {
    connection: Connection,
    path: OwnedObjectPath,
    active: bool,
}
impl Drop for PendingRequest {
    fn drop(&mut self) {
        if self.active
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let connection = self.connection.clone();
            let path = self.path.clone();
            runtime.spawn(async move {
                let _ = call::<_, ()>(
                    &connection,
                    path.as_str(),
                    "org.freedesktop.portal.Request",
                    "Close",
                    &(),
                )
                .await;
            });
        }
    }
}

/// Install the match before sending the method: portals may emit Response before replying.
async fn request<B>(
    connection: &Connection,
    interface: &str,
    method: &str,
    token: &str,
    body: &B,
) -> Result<Results>
where
    B: Serialize + zbus::zvariant::DynamicType,
{
    let path = handle_path(connection, "request", token)?;
    let proxy = Proxy::new(
        connection,
        DEST,
        path.as_str(),
        "org.freedesktop.portal.Request",
    )
    .await?;
    let mut responses = timeout(CALL_TIMEOUT, proxy.receive_signal("Response")).await??;
    let mut pending = PendingRequest {
        connection: connection.clone(),
        path: path.clone(),
        active: true,
    };
    let result = async {
        let actual: OwnedObjectPath = call(connection, PATH, interface, method, body).await?;
        ensure!(
            actual == path,
            "portal returned an unexpected request handle for {method}"
        );
        let message = timeout(APPROVAL_TIMEOUT, responses.next())
            .await
            .with_context(|| format!("{method}: approval timed out after 120 seconds"))?
            .context("portal response stream ended")?;
        let (code, results): (u32, Results) = message.body().deserialize()?;
        match code {
            0 => Ok(results),
            1 => bail!("{method}: user cancelled portal approval"),
            _ => bail!("{method}: portal rejected request (response {code})"),
        }
    }
    .await;
    if result.is_err() {
        let _ = call::<_, ()>(
            connection,
            path.as_str(),
            "org.freedesktop.portal.Request",
            "Close",
            &(),
        )
        .await;
    }
    pending.active = false;
    result
}

impl Portal {
    pub async fn start(restore_token: Option<&str>) -> Result<Self> {
        let connection = timeout(CALL_TIMEOUT, Connection::session())
            .await
            .context("connecting to session bus timed out")??;
        let session_token = token();
        // Predict the session too, so cancellation during CreateSession still has a cleanup owner.
        let session = handle_path(&connection, "session", &session_token)?;
        let closed = Arc::new(AtomicBool::new(false));
        let mut portal = Self {
            connection,
            session,
            streams: vec![],
            restore_token: None,
            clipboard_available: false,
            closed,
            closed_task: None,
            remote_closed: Arc::new(AtomicBool::new(false)),
            close_succeeded: Mutex::new(false),
            clipboard: None,
        };
        let proxy = Proxy::new(
            &portal.connection,
            DEST,
            portal.session.as_str(),
            "org.freedesktop.portal.Session",
        )
        .await?;
        let mut signals = timeout(CALL_TIMEOUT, proxy.receive_signal("Closed")).await??;
        let closed = portal.closed.clone();
        let remote_closed = portal.remote_closed.clone();
        portal.closed_task = Some(tokio::spawn(async move {
            // End-of-stream is transport loss, not confirmation from the portal.
            if signals.next().await.is_some() {
                remote_closed.store(true, Ordering::Release);
            }
            closed.store(true, Ordering::Release);
        }));
        let setup = portal.setup(&session_token, restore_token).await;
        if let Err(error) = setup {
            let _ = portal.close().await;
            return Err(error);
        }
        Ok(portal)
    }

    async fn setup(&mut self, session_token: &str, restore_token: Option<&str>) -> Result<()> {
        let t = token();
        let opts = Options::from([
            ("handle_token", Value::from(t.as_str())),
            ("session_handle_token", Value::from(session_token)),
        ]);
        let mut result = request(&self.connection, RD, "CreateSession", &t, &(opts,)).await?;
        let actual = String::try_from(
            result
                .remove("session_handle")
                .context("CreateSession omitted session_handle")?,
        )?;
        ensure!(
            actual == self.session.as_str(),
            "portal returned an unexpected session handle"
        );
        let t = token();
        let mut opts = Options::from([
            ("handle_token", Value::from(t.as_str())),
            ("types", Value::from(3u32)),
            ("persist_mode", Value::from(2u32)),
        ]);
        if let Some(restore) = restore_token {
            opts.insert("restore_token", Value::from(restore));
        }
        request(
            &self.connection,
            RD,
            "SelectDevices",
            &t,
            &(&self.session, opts),
        )
        .await?;
        let t = token();
        // Persistence belongs to RemoteDesktop, not ScreenCast, for a combined session.
        let opts = Options::from([
            ("handle_token", Value::from(t.as_str())),
            ("types", Value::from(1u32)),
            ("multiple", Value::from(true)),
            ("cursor_mode", Value::from(2u32)),
        ]);
        request(
            &self.connection,
            SC,
            "SelectSources",
            &t,
            &(&self.session, opts),
        )
        .await?;
        match Clipboard::prepare(&self.connection, &self.session, self.closed.clone()).await {
            Ok(clipboard) => self.clipboard = Some(clipboard),
            Err(error) => {
                eprintln!("Clipboard portal unavailable; Unicode paste disabled: {error:#}")
            }
        }
        let t = token();
        let opts = Options::from([("handle_token", Value::from(t.as_str()))]);
        let mut result = request(
            &self.connection,
            RD,
            "Start",
            &t,
            &(&self.session, "", opts),
        )
        .await?;
        let devices = u32::try_from(
            result
                .remove("devices")
                .context("Start omitted granted devices")?,
        )?;
        ensure!(
            devices & 3 == 3,
            "portal did not grant both keyboard and pointer access"
        );
        self.clipboard_available = self.clipboard.is_some()
            && result
                .remove("clipboard_enabled")
                .and_then(|v| bool::try_from(v).ok())
                .unwrap_or(false);
        if !self.clipboard_available {
            self.clipboard = None;
        }
        self.restore_token = result
            .remove("restore_token")
            .map(String::try_from)
            .transpose()?;
        self.streams = parse_streams(
            result
                .remove("streams")
                .context("Start omitted screen streams")?,
        )?;
        ensure!(
            !self.streams.is_empty(),
            "portal did not grant a monitor stream"
        );
        self.ensure_open()
    }

    fn ensure_open(&self) -> Result<()> {
        ensure!(!self.is_closed(), "desktop portal session is closed");
        Ok(())
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    pub async fn open_pipewire(&self) -> Result<OwnedFd> {
        self.ensure_open()?;
        let fd: zbus::zvariant::OwnedFd = call(
            &self.connection,
            PATH,
            SC,
            "OpenPipeWireRemote",
            &(&self.session, Options::new()),
        )
        .await?;
        Ok(fd.into())
    }
    pub(crate) async fn resolve_keys(
        &self,
        keys: &[crate::keys::Key],
    ) -> Result<Vec<crate::keys::Key>> {
        crate::keyboard::resolve(&self.connection, keys).await
    }
    pub async fn move_to(&self, node: u32, x: f64, y: f64) -> Result<()> {
        self.ensure_open()?;
        ensure!(
            x.is_finite() && y.is_finite(),
            "pointer coordinates must be finite"
        );
        ensure!(
            self.streams.iter().any(|s| s.node_id == node),
            "unknown desktop stream {node}"
        );
        call(
            &self.connection,
            PATH,
            RD,
            "NotifyPointerMotionAbsolute",
            &(&self.session, Options::new(), node, x, y),
        )
        .await
    }
    /// Relative motion in compositor logical units; useful for captured game pointers.
    pub async fn move_relative(&self, dx: f64, dy: f64) -> Result<()> {
        self.ensure_open()?;
        ensure!(
            dx.is_finite() && dy.is_finite(),
            "pointer deltas must be finite"
        );
        call(
            &self.connection,
            PATH,
            RD,
            "NotifyPointerMotion",
            &(&self.session, Options::new(), dx, dy),
        )
        .await
    }
    pub async fn button(&self, code: i32, pressed: bool) -> Result<()> {
        self.ensure_open()?;
        call(
            &self.connection,
            PATH,
            RD,
            "NotifyPointerButton",
            &(&self.session, Options::new(), code, u32::from(pressed)),
        )
        .await
    }
    pub async fn key(&self, keysym: i32, pressed: bool) -> Result<()> {
        self.ensure_open()?;
        call(
            &self.connection,
            PATH,
            RD,
            "NotifyKeyboardKeysym",
            &(&self.session, Options::new(), keysym, u32::from(pressed)),
        )
        .await
    }
    /// Linux evdev keyboard code, not an X11 keycode (no +8 offset) or keysym.
    pub async fn keycode(&self, code: i32, pressed: bool) -> Result<()> {
        self.ensure_open()?;
        call(
            &self.connection,
            PATH,
            RD,
            "NotifyKeyboardKeycode",
            &(&self.session, Options::new(), code, u32::from(pressed)),
        )
        .await
    }
    pub async fn scroll(&self, dx: f64, dy: f64) -> Result<()> {
        self.ensure_open()?;
        ensure!(
            dx.is_finite() && dy.is_finite(),
            "scroll deltas must be finite"
        );
        let (dx, dy) = kde_scroll_deltas(dx, dy);
        call(
            &self.connection,
            PATH,
            RD,
            "NotifyPointerAxis",
            &(
                &self.session,
                Options::from([("finish", Value::from(true))]),
                dx,
                dy,
            ),
        )
        .await
    }
    /// Replaces the clipboard. This does not send Ctrl+V or confirm a paste.
    pub async fn set_text(&self, text: &str) -> Result<()> {
        self.ensure_open()?;
        self.clipboard
            .as_ref()
            .context("clipboard access was not granted; Unicode paste is unavailable")?
            .set_text(text)
            .await
    }
    pub async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::Release);
        if let Some(c) = &self.clipboard {
            c.stop();
        }
        let mut succeeded = self.close_succeeded.lock().await;
        if *succeeded || self.remote_closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let result = call::<_, ()>(
            &self.connection,
            self.session.as_str(),
            "org.freedesktop.portal.Session",
            "Close",
            &(),
        )
        .await;
        if !self.remote_closed.load(Ordering::Acquire) {
            result?;
        }
        *succeeded = true;
        Ok(())
    }
}

impl Drop for Portal {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        if let Some(c) = &self.clipboard {
            c.stop();
        }
        if let Some(task) = &self.closed_task {
            task.abort();
        }
        if *self.close_succeeded.get_mut() || self.remote_closed.load(Ordering::Acquire) {
            return;
        }
        // Explicit close is preferred. Drop cannot await; bus disconnection is the final fallback.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let connection = self.connection.clone();
            let session = self.session.clone();
            runtime.spawn(async move {
                let _ = call::<_, ()>(
                    &connection,
                    session.as_str(),
                    "org.freedesktop.portal.Session",
                    "Close",
                    &(),
                )
                .await;
            });
        }
    }
}

fn parse_streams(value: OwnedValue) -> Result<Vec<DesktopStream>> {
    let streams = Vec::<(u32, Results)>::try_from(value)?;
    streams
        .into_iter()
        .map(|(node_id, mut props)| {
            let position = props
                .remove("position")
                .map(<(i32, i32)>::try_from)
                .transpose()?;
            let size = props
                .remove("size")
                .map(<(i32, i32)>::try_from)
                .transpose()?;
            let logical_size = size.and_then(|(w, h)| {
                if w > 0 && h > 0 {
                    Some((w as u32, h as u32))
                } else {
                    None
                }
            });
            Ok(DesktopStream {
                node_id,
                logical_size,
                position,
            })
        })
        .collect()
}

// KDE's portal negates the vertical axis before forwarding to KWin. Adapt that
// endpoint once so LCU's positive-down contract reaches applications unchanged.
fn kde_scroll_deltas(dx: f64, dy: f64) -> (f64, f64) {
    (dx, -dy)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kde_axis_mapping_preserves_horizontal_and_reverses_vertical() {
        for (dx, dy) in [(2.5, 7.25), (-3.5, -0.5), (0., 0.)] {
            assert_eq!(kde_scroll_deltas(dx, dy), (dx, -dy));
        }
    }
    #[test]
    fn parses_logical_not_buffer_size() {
        let props = Options::from([
            ("size", Value::from((1920i32, 1080i32))),
            ("position", Value::from((-1920i32, 0i32))),
        ]);
        let value = Value::from(vec![(42u32, props)]).try_to_owned().unwrap();
        let streams = parse_streams(value).unwrap();
        assert_eq!(streams[0].logical_size, Some((1920, 1080)));
        assert_eq!(streams[0].position, Some((-1920, 0)));
    }
    #[test]
    fn invalid_size_is_unavailable() {
        let value = Value::from(vec![(
            1u32,
            Options::from([("size", Value::from((-1i32, 0i32)))]),
        )])
        .try_to_owned()
        .unwrap();
        assert_eq!(parse_streams(value).unwrap()[0].logical_size, None);
    }
}
