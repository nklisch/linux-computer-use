//! Linux process ownership is recovery bookkeeping, not a sandbox.
use super::{Descriptor, LaunchArgs, now_ms};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::process::{Child, Command};
use std::{collections::BTreeMap, os::unix::process::CommandExt, path::Path, process::Stdio};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Application {
    pub pid: u32,
    pub executable: String,
    pub started_at_ms: u64,
    pub exit_code: Option<i32>,
    pub running: bool,
    pub routing: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Resources {
    pub process_count: usize,
    pub resident_bytes: u64,
    pub cpu_ticks: u64,
    pub note: String,
}
pub struct Processes {
    children: Vec<(Child, Option<Application>)>,
    descriptor: Option<Descriptor>,
}
impl Default for Processes {
    fn default() -> Self {
        Self::new()
    }
}
impl Processes {
    pub fn new() -> Self {
        Self {
            children: vec![],
            descriptor: None,
        }
    }
    pub fn for_desktop(d: Descriptor) -> Self {
        Self {
            children: vec![],
            descriptor: Some(d),
        }
    }
    pub fn subreaper() -> Result<()> {
        // Reparent ordinary daemonized grandchildren to this worker for reaping.
        ensure!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0,
            "Cannot establish child subreaper: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }
    pub fn spawn(
        &mut self,
        argv: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
        log: &Path,
        application: bool,
        routing: String,
    ) -> Result<Application> {
        ensure!(!argv.is_empty(), "argv must contain an executable");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        // Retained workers can outlive a binary replacement. Exec the mapped
        // inode, not current_exe's potentially deleted pathname.
        let helper = std::path::PathBuf::from("/proc/self/exe");
        let mut cmd = Command::new(&helper);
        cmd.env_clear()
            .envs(env)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(file.try_clone()?)
            .stderr(file);
        cmd.process_group(0);
        let pending = super::gate::Pending::spawn(&helper, cmd, argv)?;
        let identity = super::ownership::identity(pending.pid())?;
        super::ownership::remember(
            self.descriptor
                .as_mut()
                .context("Spawn requires desktop ownership")?,
            [identity],
        )?;
        let child = pending
            .authorize()
            .with_context(|| format!("Launch {}", argv[0]))?;
        let app = Application {
            pid: child.id(),
            executable: argv[0].clone(),
            started_at_ms: now_ms(),
            exit_code: None,
            running: true,
            routing,
        };
        self.children
            .push((child, application.then(|| app.clone())));
        Ok(app)
    }
    pub fn descriptor(&self) -> Option<&Descriptor> {
        self.descriptor.as_ref()
    }
    pub fn applications(&mut self) -> Vec<Application> {
        self.children
            .iter_mut()
            .filter_map(|(child, app)| {
                if let Ok(Some(exit)) = child.try_wait()
                    && let Some(a) = app
                {
                    a.running = false;
                    a.exit_code = exit.code();
                }
                app.clone()
            })
            .collect()
    }
    pub fn failed_service(&mut self) -> Option<String> {
        self.children.iter_mut().find_map(|(child, app)| {
            if app.is_none()
                && let Ok(Some(status)) = child.try_wait()
            {
                return Some(format!("Owned service exited: {status}"));
            }
            None
        })
    }
    pub async fn cleanup(&mut self, id: &str) -> Result<()> {
        // Persist discovered descendants BEFORE terminating application roots.
        // A retry after worker death must not lose the ancestry we sever here.
        if let Some(d) = self.descriptor.as_mut() {
            super::ownership::checkpoint(d, true)?;
        }
        let applications = self.applications();
        if let Some(record) = self.descriptor.as_ref().and_then(|d| d.ownership.as_ref()) {
            for app in applications.iter().filter(|a| a.running) {
                if let Some(identity) = record.processes.iter().find(|i| i.pid == app.pid) {
                    super::ownership::signal(identity, libc::SIGTERM)?;
                }
            }
        }
        for _ in 0..10 {
            if self.applications().iter().all(|a| !a.running) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let _ = id;
        if let Some(d) = self.descriptor.as_mut() {
            super::ownership::cleanup(d, true).await?;
        }
        for (child, _) in &mut self.children {
            let _ = child.try_wait();
        }
        // All established ownership targets have gone; reap adopted descendants too.
        loop {
            let pid = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
        }
        Ok(())
    }
}
#[derive(Clone)]
struct Process {
    pid: u32,
    parent: u32,
    rss: u64,
    cpu: u64,
}
fn process(pid: u32) -> Option<Process> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    if fields.first() == Some(&"Z") {
        return None;
    }
    Some(Process {
        pid,
        parent: fields.get(1)?.parse().ok()?,
        rss: fields
            .get(21)?
            .parse::<u64>()
            .ok()?
            .saturating_mul(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(0) as u64),
        cpu: fields
            .get(11)?
            .parse::<u64>()
            .ok()?
            .saturating_add(fields.get(12)?.parse().ok()?),
    })
}
fn tagged(id: &str) -> Vec<Process> {
    let expected = format!("LCU_DESKTOP_TAG={id}");
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let pid = e.ok()?.file_name().to_str()?.parse::<u32>().ok()?;
            if pid == std::process::id() {
                return None;
            }
            let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
            env.split(|b| *b == 0)
                .any(|v| v == expected.as_bytes())
                .then(|| process(pid))
                .flatten()
        })
        .collect()
}
fn owned(id: &str, include_children: bool) -> Vec<Process> {
    let mut result = tagged(id);
    if !include_children {
        return result;
    }
    // Subreaping keeps ordinary double-forking applications in this ancestry even
    // when they clear their environment or create new process groups.
    let all: Vec<_> = std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|entry| process(entry.ok()?.file_name().to_str()?.parse().ok()?))
        .collect();
    let mut parents = std::collections::HashSet::from([std::process::id()]);
    loop {
        let before = parents.len();
        for p in &all {
            if parents.contains(&p.parent) {
                parents.insert(p.pid);
            }
        }
        if before == parents.len() {
            break;
        }
    }
    for p in all {
        if p.pid != std::process::id()
            && parents.contains(&p.pid)
            && !result.iter().any(|old| old.pid == p.pid)
        {
            result.push(p);
        }
    }
    result
}
pub fn resources(id: &str) -> Resources {
    resource_summary(tagged(id))
}
pub fn worker_resources(id: &str) -> Resources {
    let mut ps = owned(id, true);
    if let Some(worker) = process(std::process::id()) {
        ps.push(worker);
    }
    resource_summary(ps)
}
fn resource_summary(ps: Vec<Process>) -> Resources {
    Resources { process_count:ps.len(), resident_bytes:ps.iter().map(|p|p.rss).sum(), cpu_ticks:ps.iter().map(|p|p.cpu).sum(), note:"Summed RSS double-counts shared pages; CPU in kernel ticks. Shared PipeWire, GPU and externally delegated work are not attributed.".into() }
}
pub fn launch_plan(
    d: &Descriptor,
    args: &LaunchArgs,
    baseline: &BTreeMap<String, String>,
) -> Result<(Vec<String>, BTreeMap<String, String>, String)> {
    ensure!(!args.argv.is_empty(), "argv must not be empty");
    ensure!(
        args.cwd.is_absolute() && args.cwd.is_dir(),
        "cwd must be an existing absolute directory"
    );
    let mut env = baseline.clone();
    for (key, value) in &args.env {
        ensure!(
            !matches!(
                key.as_str(),
                "HOME"
                    | "DISPLAY"
                    | "WAYLAND_DISPLAY"
                    | "WAYLAND_SOCKET"
                    | "DBUS_SESSION_BUS_ADDRESS"
                    | "XDG_RUNTIME_DIR"
                    | "XDG_CONFIG_HOME"
                    | "XDG_DATA_HOME"
                    | "XDG_STATE_HOME"
                    | "XDG_CACHE_HOME"
                    | "AT_SPI_BUS_ADDRESS"
                    | "PIPEWIRE_REMOTE"
                    | "PIPEWIRE_RUNTIME_DIR"
                    | "LCU_DESKTOP_TAG"
            ),
            "{key} is managed desktop routing; cannot override"
        );
        env.insert(key.clone(), value.clone());
    }
    if args.webkit_alternate_buffers {
        env.insert("WEBKIT_DISABLE_DMABUF_RENDERER".into(), "1".into());
    }
    let mut argv = args.argv.clone();
    let name = Path::new(&argv[0])
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let routing = match name {
        "google-chrome" | "google-chrome-stable" | "chromium" | "chromium-browser" | "chrome" => {
            ensure!(
                !argv.iter().skip(1).any(|a| a.starts_with("--user-data-dir")
                    || a.starts_with("--ozone-platform")
                    || a.starts_with("--display")
                    || a.starts_with("--profile-directory")),
                "Browser profile/display arguments are managed; omit conflicting arguments"
            );
            argv.push(format!(
                "--user-data-dir={}",
                d.state.join("chromium").display()
            ));
            argv.push("--ozone-platform=wayland".into());
            "chromium_private_profile"
        }
        name if is_godot4(name) => {
            let boundary = argv
                .iter()
                .position(|a| a == "--" || a == "++")
                .unwrap_or(argv.len());
            let mut explicit = false;
            for (index, arg) in argv[..boundary].iter().enumerate().skip(1) {
                if arg == "--display-driver" {
                    ensure!(
                        argv.get(index + 1)
                            .filter(|_| index + 1 < boundary)
                            .map(String::as_str)
                            == Some("wayland"),
                        "Owned Godot desktops require --display-driver wayland; omit the driver or specify wayland"
                    );
                    explicit = true;
                }
            }
            if !explicit {
                argv.splice(
                    boundary..boundary,
                    ["--display-driver".into(), "wayland".into()],
                );
            }
            "godot_wayland"
        }
        _ => "private_environment_only; application routing unverified",
    };
    Ok((argv, env, routing.into()))
}

fn is_godot4(name: &str) -> bool {
    if matches!(name, "godot" | "godot4") {
        return true;
    }
    let Some(release) = name.strip_prefix("Godot_v4.") else {
        return false;
    };
    let Some((version, arch)) = release.split_once("_linux.") else {
        return false;
    };
    let Some((version, channel)) = version.split_once('-') else {
        return false;
    };
    matches!(arch, "x86_64" | "x86_32" | "arm64" | "arm32")
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        && (channel == "stable"
            || ["dev", "beta", "rc"].iter().any(|prefix| {
                channel
                    .strip_prefix(prefix)
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plan(
        argv: &[&str],
        env: BTreeMap<String, String>,
    ) -> Result<(Vec<String>, BTreeMap<String, String>, String)> {
        let d = Descriptor {
            name: None,
            id: "test".into(),
            runtime: "/tmp/unused-runtime".into(),
            state: "/tmp/unused-state".into(),
            width: 1280,
            height: 720,
            owned: true,
            ownership: None,
        };
        launch_plan(
            &d,
            &LaunchArgs {
                argv: argv.iter().map(|s| (*s).into()).collect(),
                cwd: "/tmp".into(),
                env,
                webkit_alternate_buffers: false,
            },
            &BTreeMap::new(),
        )
    }
    #[test]
    fn godot_routing_recognizes_official_names_without_matching_wrappers() {
        for name in [
            "godot",
            "godot4",
            "/opt/Godot_v4.7.1-stable_linux.x86_64",
            "Godot_v4.8-beta2_linux.arm64",
        ] {
            let (argv, _, routing) = plan(&[name], BTreeMap::new()).unwrap();
            assert_eq!(routing, "godot_wayland");
            assert_eq!(&argv[1..], ["--display-driver", "wayland"]);
        }
        for name in [
            "godot-wrapper",
            "mygodot",
            "Godot_v3.6-stable_linux.x86_64",
            "Godot_v4.x-stable_linux.x86_64",
            "Godot_v4.7-stable_linux.x86_64-wrapper",
        ] {
            let (argv, _, routing) = plan(&[name], BTreeMap::new()).unwrap();
            assert!(routing.contains("unverified"));
            assert_eq!(argv, [name]);
        }
    }
    #[test]
    fn godot_driver_stays_before_user_arguments_and_preserves_explicit_wayland() {
        for separator in ["--", "++"] {
            let args = [
                "godot4",
                "--rendering-method",
                "gl_compatibility",
                "--fullscreen",
                separator,
                "--display-driver",
                "game-value",
            ];
            let (argv, _, _) = plan(&args, BTreeMap::new()).unwrap();
            assert_eq!(
                argv,
                [
                    "godot4",
                    "--rendering-method",
                    "gl_compatibility",
                    "--fullscreen",
                    "--display-driver",
                    "wayland",
                    separator,
                    "--display-driver",
                    "game-value"
                ]
            );
        }
        let args = [
            "godot",
            "--display-driver",
            "wayland",
            "--resolution",
            "900x600",
            "res://main.tscn",
            "--",
            "game-arg",
        ];
        assert_eq!(plan(&args, BTreeMap::new()).unwrap().0, args);
        for args in [
            vec!["godot", "--display-driver"],
            vec!["godot", "--display-driver", "--", "wayland"],
            vec!["godot", "--display-driver", "x11"],
        ] {
            assert!(
                plan(&args, BTreeMap::new())
                    .unwrap_err()
                    .to_string()
                    .contains("require --display-driver wayland")
            );
        }
    }
    #[test]
    fn webkit_override_remains_application_local() {
        let env = BTreeMap::from([("GDK_GL".into(), "always".into())]);
        let (_, first, _) = plan(&["webkit-app"], env.clone()).unwrap();
        assert_eq!(first["GDK_GL"], "always");
        assert!(!first.contains_key("WEBKIT_DISABLE_DMABUF_RENDERER"));
        assert_eq!(env.len(), 1);
        assert!(
            !plan(&["webkit-app"], BTreeMap::new())
                .unwrap()
                .1
                .contains_key("GDK_GL")
        );
    }
    #[test]
    fn launch_routing_is_explicit_and_profiles_are_not_overrideable() {
        let root = tempfile::tempdir().unwrap();
        let d = Descriptor {
            name: None,
            id: "test".into(),
            runtime: root.path().join("runtime"),
            state: root.path().join("state"),
            width: 100,
            height: 100,
            owned: true,
            ownership: None,
        };
        let mut args = LaunchArgs {
            argv: vec!["chromium".into(), "about:blank".into()],
            cwd: root.path().into(),
            env: BTreeMap::new(),
            webkit_alternate_buffers: false,
        };
        let (argv, env, routing) = launch_plan(&d, &args, &BTreeMap::new()).unwrap();
        assert!(argv.iter().any(|v| v == "--ozone-platform=wayland"));
        assert!(argv.iter().any(|v| v.starts_with("--user-data-dir=")));
        assert_eq!(routing, "chromium_private_profile");
        assert!(!env.contains_key("WEBKIT_DISABLE_DMABUF_RENDERER"));
        args.argv.push("--user-data-dir=/host".into());
        assert!(launch_plan(&d, &args, &BTreeMap::new()).is_err());
        args.argv = vec!["arbitrary-wrapper".into()];
        args.env.insert("WAYLAND_DISPLAY".into(), "host".into());
        assert!(launch_plan(&d, &args, &BTreeMap::new()).is_err());
        args.env.clear();
        args.webkit_alternate_buffers = true;
        let (_, env, routing) = launch_plan(&d, &args, &BTreeMap::new()).unwrap();
        assert_eq!(env["WEBKIT_DISABLE_DMABUF_RENDERER"], "1");
        assert!(routing.contains("unverified"));
    }
}
