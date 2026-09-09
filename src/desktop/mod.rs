//! Retained graphical sessions. Workers, not frontend connections, own applications.
pub mod daemon;
pub mod gate;
pub mod ownership;
pub mod processes;
pub mod rpc;
pub mod session;
pub mod worker;

use crate::types::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

pub const WIRE_REVISION: u32 = 1;
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum DesktopOperation {
    Create {
        /// Human-readable label; defaults to the desktop ID when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default = "width")]
        width: u32,
        #[serde(default = "height")]
        height: u32,
    },
    List,
    Claim {
        desktop_id: String,
        #[serde(default)]
        force: bool,
    },
    Release {
        desktop_id: String,
    },
    Launch {
        desktop_id: String,
        #[serde(flatten)]
        args: LaunchArgs,
    },
    Destroy {
        desktop_id: String,
        #[serde(default)]
        force: bool,
    },
}
fn width() -> u32 {
    1280
}
fn height() -> u32 {
    720
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchArgs {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Explicit WebKitGTK alternate-buffer workaround; never applied by default.
    #[serde(default)]
    pub webkit_alternate_buffers: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    Hello,
    Desktop(DesktopOperation),
    Control {
        desktop_id: String,
        command: Command,
    },
    ShutdownDaemon,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Descriptor {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub runtime: PathBuf,
    pub state: PathBuf,
    pub width: u32,
    pub height: u32,
    pub owned: bool,
    #[serde(default)]
    pub ownership: Option<ownership::Ownership>,
}
impl Descriptor {
    pub fn socket(&self) -> PathBuf {
        self.runtime.join("worker.sock")
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopInfo {
    pub descriptor: Descriptor,
    pub available: bool,
    pub claimed: bool,
    pub claim_age_ms: Option<u64>,
    pub last_activity_age_ms: Option<u64>,
    pub control_error: Option<String>,
    pub applications: Vec<processes::Application>,
    pub resources: processes::Resources,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "result", rename_all = "snake_case")]
// Responses are individual messages; the queued RPC packet already boxes its result.
#[allow(clippy::large_enum_variant)]
pub enum Response {
    Hello { wire_revision: u32, version: String },
    Desktops(Vec<DesktopInfo>),
    Created(Descriptor),
    Claimed { desktop_id: String },
    Released { desktop_id: String },
    Launched(processes::Application),
    Destroyed { desktop_id: String },
    Control(Reply),
    DaemonStopped,
}
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
pub fn runtime_root() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("LCU_RUNTIME_DIR") {
        return Ok(path.into());
    }
    Ok(PathBuf::from(
        std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| anyhow::anyhow!("XDG_RUNTIME_DIR unavailable"))?,
    )
    .join("lcu"))
}
pub fn state_root() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("LCU_STATE_DIR") {
        return Ok(p.into());
    }
    Ok(std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or(
            PathBuf::from(
                std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME unavailable"))?,
            )
            .join(".local/state"),
        )
        .join("lcu/desktops"))
}
/// Keep the rendezvous descriptor until all other owned resources are removed.
pub fn remove_resources(d: &Descriptor) -> anyhow::Result<()> {
    let latest = match std::fs::read(d.state.join("descriptor.json")) {
        Ok(bytes) => Some(serde_json::from_slice::<Descriptor>(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let d = latest.as_ref().unwrap_or(d);
    if d.runtime.exists() {
        std::fs::remove_dir_all(&d.runtime)?;
    }
    if !d.state.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&d.state)? {
        let entry = entry?;
        if entry.file_name() == "descriptor.json" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    let descriptor = d.state.join("descriptor.json");
    if descriptor.exists() {
        std::fs::remove_file(&descriptor)?;
    }
    if let Err(error) = std::fs::remove_dir(&d.state) {
        let _ = std::fs::write(descriptor, serde_json::to_vec(d)?);
        return Err(error.into());
    }
    Ok(())
}
pub fn private_dir(path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_resource_cleanup_does_not_follow_project_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let d = Descriptor {
            name: None,
            id: "test".into(),
            runtime: root.path().join("run"),
            state: root.path().join("private"),
            width: 100,
            height: 100,
            owned: true,
            ownership: None,
        };
        private_dir(&d.runtime).unwrap();
        private_dir(&d.state).unwrap();
        let shared = root.path().join("project");
        std::fs::create_dir(&shared).unwrap();
        std::fs::write(shared.join("marker"), "keep").unwrap();
        std::os::unix::fs::symlink(&shared, d.state.join("project-link")).unwrap();
        std::fs::write(
            d.state.join("descriptor.json"),
            serde_json::to_vec(&d).unwrap(),
        )
        .unwrap();
        remove_resources(&d).unwrap();
        assert_eq!(
            std::fs::read_to_string(shared.join("marker")).unwrap(),
            "keep"
        );
        assert!(!d.state.exists());
        assert!(!d.runtime.exists());
        remove_resources(&d).unwrap();
    }
}
