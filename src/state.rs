//! Only the revocable portal restore token persists. Screenshots and typed text do not.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::PathBuf,
};

#[derive(Clone)]
pub struct StateStore {
    directory: PathBuf,
}
#[derive(Serialize, Deserialize)]
struct SavedPermission {
    restore_token: String,
}

impl StateStore {
    pub fn from_environment() -> Result<Self> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))
            .context("Neither XDG_STATE_HOME nor HOME is set; cannot locate permission storage")?;
        Ok(Self {
            directory: base.join("linux-computer-use"),
        })
    }
    pub fn load(&self) -> Result<Option<String>> {
        let path = self.directory.join("permission.json");
        let mut file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("Reading saved permission"),
        };
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        let saved: SavedPermission = serde_json::from_str(&text)
            .context("Invalid saved permission; start with fresh_permission=true to replace it")?;
        Ok(Some(saved.restore_token))
    }
    pub fn save(&self, token: &str) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&self.directory)?;
        let mut temp = tempfile::NamedTempFile::new_in(&self.directory)?;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer(
            &mut temp,
            &SavedPermission {
                restore_token: token.into(),
            },
        )?;
        temp.flush()?;
        temp.persist(self.directory.join("permission.json"))
            .map_err(|e| e.error)?;
        Ok(())
    }
    pub fn forget(&self) -> Result<()> {
        match std::fs::remove_file(self.directory.join("permission.json")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permission_roundtrip_and_private_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let store = StateStore {
            directory: tmp.path().join("state"),
        };
        assert!(store.load().unwrap().is_none());
        store.save("first").unwrap();
        store.save("second").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some("second"));
        assert_eq!(
            std::fs::metadata(store.directory.join("permission.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        store.forget().unwrap();
        assert!(store.load().unwrap().is_none());
    }
}
