//! Recovery identities belong to the desktop descriptor, not an application registry.
use super::Descriptor;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Write,
    os::fd::{AsRawFd, FromRawFd},
    path::Path,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub pid: u32,
    pub start: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ownership {
    pub boot: String,
    pub processes: Vec<Identity>,
}
impl Ownership {
    pub fn new() -> Result<Self> {
        Ok(Self {
            boot: boot()?,
            processes: vec![],
        })
    }
}
fn boot() -> Result<String> {
    Ok(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned())
}
#[derive(Clone)]
struct Process {
    identity: Identity,
    parent: u32,
}
fn read(pid: u32) -> Result<Option<Process>> {
    let text = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Read identity for PID {pid}")),
    };
    parse(pid, &text)
}
fn parse(pid: u32, text: &str) -> Result<Option<Process>> {
    let fields: Vec<_> = text
        .rsplit_once(')')
        .context("Invalid process stat")?
        .1
        .split_whitespace()
        .collect();
    let state = fields.first().context("Missing process state")?;
    let identity = Identity {
        pid,
        start: fields
            .get(19)
            .context("Missing process start time")?
            .parse()?,
    };
    let parent = fields.get(1).context("Missing process parent")?.parse()?;
    Ok(if *state == "Z" || *state == "X" {
        None
    } else {
        Some(Process { identity, parent })
    })
}
pub fn identity(pid: u32) -> Result<Identity> {
    Ok(read(pid)?
        .context("Process exited before identity publication")?
        .identity)
}
fn current(i: &Identity) -> Result<Option<Process>> {
    Ok(read(i.pid)?.filter(|p| &p.identity == i))
}
pub fn publish(d: &Descriptor) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(&d.state)?;
    file.write_all(&serde_json::to_vec(d)?)?;
    file.as_file().sync_all()?;
    file.persist(d.state.join("descriptor.json"))?;
    File::open(&d.state)?.sync_all()?;
    Ok(())
}
pub fn remember(d: &mut Descriptor, identities: impl IntoIterator<Item = Identity>) -> Result<()> {
    // Reload under the lifecycle lock: a prior rename may have succeeded even
    // if its durability acknowledgement failed. Never overwrite that evidence.
    let mut next = read_descriptor(&d.state.join("descriptor.json"))?;
    let record = next
        .ownership
        .as_mut()
        .context("Ownership record missing; cannot establish complete cleanup")?;
    ensure!(
        record.boot == boot()?,
        "Desktop ownership belongs to another boot"
    );
    let mut changed = false;
    for i in identities {
        if !record.processes.contains(&i) {
            record.processes.push(i);
            changed = true;
        }
    }
    if changed {
        publish(&next)?;
    }
    *d = next;
    Ok(())
}
/// The inode lives outside removable desktop resources and is never replaced.
/// Creation hands the same open-file description to the worker; payload execs do not.
pub struct LifecycleLock(File);
impl LifecycleLock {
    pub fn acquire(d: &Descriptor) -> Result<Self> {
        let dir = d
            .state
            .parent()
            .context("Desktop has no state parent")?
            .join(".locks");
        super::private_dir(&dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(format!("{}.lock", d.id)))?;
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "Desktop {} is still owned by a starting/live worker or another cleanup; retry after it exits",
            d.id
        );
        Ok(Self(file))
    }
    pub fn fd(&self) -> i32 {
        self.0.as_raw_fd()
    }
    /// Called only for the descriptor explicitly inherited from daemon creation.
    pub fn inherited(fd: i32) -> Result<Self> {
        ensure!(fd >= 3, "Invalid lifecycle descriptor");
        ensure!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
            "Cannot protect lifecycle descriptor from payload inheritance"
        );
        Ok(Self(unsafe { File::from_raw_fd(fd) }))
    }
}
fn discover(d: &Descriptor, worker: bool) -> Result<Vec<Identity>> {
    let record = d
        .ownership
        .as_ref()
        .context("Ownership record missing; explicit cleanup cannot prove all resources gone")?;
    let mut roots = Vec::new();
    for i in &record.processes {
        if let Some(p) = current(i)? {
            roots.push(p);
        }
    }
    if worker && let Some(p) = read(std::process::id())? {
        roots.push(p);
    }
    let expected = format!("LCU_DESKTOP_TAG={}", d.id);
    let mut all = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        // Unrelated /proc entries may be inaccessible. Known ownership is checked
        // strictly above; unreadable tags are not evidence that an owner has gone.
        let Ok(Some(p)) = read(pid) else { continue };
        if pid != std::process::id()
            && let Ok(env) = std::fs::read(entry.path().join("environ"))
            && env.split(|b| *b == 0).any(|v| v == expected.as_bytes())
            && current(&p.identity)?.is_some()
        {
            roots.push(p.clone());
        }
        all.push(p);
    }
    let mut result = Vec::new();
    while let Some(parent) = roots.pop() {
        if result.contains(&parent.identity) {
            continue;
        }
        // Bind ancestry to both incarnations, not merely a PID observed earlier.
        if current(&parent.identity)?.is_none() {
            continue;
        }
        for child in all.iter().filter(|p| p.parent == parent.identity.pid) {
            if current(&child.identity)?.is_some() && current(&parent.identity)?.is_some() {
                roots.push(child.clone());
            }
        }
        result.push(parent.identity);
    }
    result.retain(|i| i.pid != std::process::id());
    Ok(result)
}
pub(super) fn signal(i: &Identity, sig: i32) -> Result<()> {
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, i.pid, 0) } as i32;
    if raw < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(e).with_context(|| format!("Open pidfd for {}", i.pid));
    }
    let fd = unsafe { File::from_raw_fd(raw) };
    if current(i)?.is_none() {
        return Ok(());
    }
    let r = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            sig,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if r < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            return Err(e).with_context(|| format!("Signal PID {}", i.pid));
        }
    }
    Ok(())
}
pub(super) fn checkpoint(d: &mut Descriptor, worker: bool) -> Result<()> {
    let found = discover(d, worker)?;
    remember(d, found)
}
pub async fn cleanup(d: &mut Descriptor, worker: bool) -> Result<()> {
    cleanup_with(d, worker, |i| Ok(current(i)?.is_some()), signal).await
}
async fn cleanup_with(
    d: &mut Descriptor,
    worker: bool,
    probe: impl Fn(&Identity) -> Result<bool>,
    send: impl Fn(&Identity, i32) -> Result<()>,
) -> Result<()> {
    let record = d
        .ownership
        .as_ref()
        .context("Ownership record missing; cannot prove cleanup of this earlier desktop")?;
    if record.boot != boot()? {
        return Ok(());
    }
    for sig in [libc::SIGTERM, libc::SIGKILL] {
        for _ in 0..20 {
            let found = discover(d, worker)?;
            // Save descendants before terminating ancestors: retry after a crash
            // must not depend on ancestry that the first attempt has destroyed.
            remember(d, found)?;
            let mut live = Vec::new();
            for i in &d.ownership.as_ref().unwrap().processes {
                if probe(i)? {
                    live.push(i.clone());
                }
            }
            if live.is_empty() {
                return Ok(());
            }
            for i in live {
                send(&i, sig)?;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    let mut unresolved = Vec::new();
    for i in &d.ownership.as_ref().unwrap().processes {
        if probe(i)? {
            unresolved.push(i.clone());
        }
    }
    ensure!(
        unresolved.is_empty(),
        "Owned processes still alive: {unresolved:?}; descriptor and resources retained"
    );
    Ok(())
}
pub fn read_descriptor(path: &Path) -> Result<Descriptor> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(root: &Path) -> Descriptor {
        let d = Descriptor {
            name: None,
            id: "recovery-unit".into(),
            runtime: root.join("run"),
            state: root.join("state"),
            width: 100,
            height: 100,
            owned: true,
            ownership: Some(Ownership::new().unwrap()),
        };
        super::super::private_dir(&d.runtime).unwrap();
        super::super::private_dir(&d.state).unwrap();
        publish(&d).unwrap();
        d
    }
    #[tokio::test]
    async fn identity_and_signal_errors_preserve_evidence_for_retry() {
        let root = tempfile::tempdir().unwrap();
        let mut d = fixture(root.path());
        let mut child = std::process::Command::new("/usr/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let i = identity(child.id()).unwrap();
        remember(&mut d, [i.clone()]).unwrap();
        let marker = d.state.join("profile");
        std::fs::write(&marker, "keep").unwrap();
        assert!(
            cleanup_with(
                &mut d,
                false,
                |_| anyhow::bail!("identity read denied"),
                signal
            )
            .await
            .is_err()
        );
        assert!(marker.exists());
        assert!(
            read_descriptor(&d.state.join("descriptor.json"))
                .unwrap()
                .ownership
                .unwrap()
                .processes
                .contains(&i)
        );
        assert!(
            cleanup_with(
                &mut d,
                false,
                |i| Ok(current(i)?.is_some()),
                |_, _| anyhow::bail!("signal denied")
            )
            .await
            .is_err()
        );
        assert!(marker.exists());
        assert!(current(&i).unwrap().is_some());
        cleanup(&mut d, false).await.unwrap();
        child.wait().unwrap();
    }
    #[tokio::test]
    async fn discovered_descendant_survives_parent_exit_in_retry_record() {
        use std::io::BufRead;
        let root = tempfile::tempdir().unwrap();
        let mut d = fixture(root.path());
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let descendant = identity(line.trim().parse().unwrap()).unwrap();
        let parent = identity(child.id()).unwrap();
        remember(&mut d, [parent.clone()]).unwrap();
        checkpoint(&mut d, false).unwrap();
        assert!(
            d.ownership
                .as_ref()
                .unwrap()
                .processes
                .contains(&descendant)
        );
        signal(&parent, libc::SIGKILL).unwrap();
        child.wait().unwrap();
        let mut recovered = read_descriptor(&d.state.join("descriptor.json")).unwrap();
        cleanup(&mut recovered, false).await.unwrap();
        assert!(current(&descendant).unwrap().is_none());
    }
    #[tokio::test]
    async fn lifecycle_lock_inode_survives_resource_directory_recreation() {
        let root = tempfile::tempdir().unwrap();
        let d = fixture(root.path());
        let lock = LifecycleLock::acquire(&d).unwrap();
        std::fs::remove_dir_all(&d.state).unwrap();
        std::fs::create_dir(&d.state).unwrap();
        assert!(LifecycleLock::acquire(&d).is_err());
        drop(lock);
        // Other parallel tests may be between fork and CLOEXEC. Such temporary
        // inheritance legitimately delays lock release, but not beyond exec.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if LifecycleLock::acquire(&d).is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
    #[test]
    fn malformed_identity_is_not_absence() {
        assert!(parse(1, "broken").is_err());
    }
    #[test]
    fn stale_identity_does_not_signal() {
        let mut i = identity(std::process::id()).unwrap();
        i.start += 1;
        signal(&i, libc::SIGKILL).unwrap();
    }
}
