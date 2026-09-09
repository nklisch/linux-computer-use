//! KWin/private-session bootstrap. Environment is installed before worker exec.
use super::{Descriptor, private_dir, processes::Processes};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, path::Path, time::Duration};

pub fn environment(d: &Descriptor) -> Result<BTreeMap<String, String>> {
    if !d.owned {
        return Ok(std::env::vars().collect());
    }
    let mut env = BTreeMap::new();
    for key in [
        "PATH",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TZ",
        "XDG_DATA_DIRS",
        "XDG_CONFIG_DIRS",
    ] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.into(), value);
        }
    }
    env.entry("PATH".into())
        .or_insert("/usr/local/bin:/usr/bin:/bin".into());
    for (key, path) in [
        ("HOME", d.state.join("home")),
        ("XDG_CONFIG_HOME", d.state.join("config")),
        ("XDG_DATA_HOME", d.state.join("data")),
        ("XDG_CACHE_HOME", d.state.join("cache")),
        ("XDG_STATE_HOME", d.state.join("state")),
        ("XDG_RUNTIME_DIR", d.runtime.join("session")),
    ] {
        private_dir(&path)?;
        env.insert(key.into(), path.to_string_lossy().into_owned());
    }
    let host_runtime =
        std::env::var("XDG_RUNTIME_DIR").context("Host PipeWire runtime unavailable")?;
    let pw = std::env::var("PIPEWIRE_REMOTE").unwrap_or_else(|_| "pipewire-0".into());
    let pw = if Path::new(&pw).is_absolute() {
        pw
    } else {
        format!(
            "{}/{pw}",
            std::env::var("PIPEWIRE_RUNTIME_DIR").unwrap_or(host_runtime)
        )
    };
    env.insert("PIPEWIRE_REMOTE".into(), pw);
    env.insert("WAYLAND_DISPLAY".into(), "lcu-wayland".into());
    env.insert(
        "DBUS_SESSION_BUS_ADDRESS".into(),
        format!("unix:path={}", d.runtime.join("session/bus").display()),
    );
    env.insert("XDG_CURRENT_DESKTOP".into(), "KDE".into());
    env.insert("XDG_SESSION_TYPE".into(), "wayland".into());
    env.insert("QT_QPA_PLATFORM".into(), "wayland".into());
    env.insert("GDK_BACKEND".into(), "wayland".into());
    env.insert("ATSPI_DBUS_IMPLEMENTATION".into(), "dbus-daemon".into());
    env.insert("LCU_DESKTOP_TAG".into(), d.id.clone());
    Ok(env)
}
async fn wait_path(path: &Path) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .with_context(|| format!("Timed out waiting for {}", path.display()))?;
    Ok(())
}
async fn wait_name(conn: &zbus::Connection, name: &str) -> Result<()> {
    let bus = zbus::Proxy::new(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if bus.call::<_, _, bool>("NameHasOwner", &(name,)).await? {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .with_context(|| format!("Private service {name} did not acquire its bus name"))??;
    Ok(())
}
pub async fn bootstrap(d: &Descriptor, processes: &mut Processes) -> Result<()> {
    if !d.owned {
        return Ok(());
    }
    ensure!(
        std::env::var("LCU_DESKTOP_TAG").ok().as_deref() == Some(&d.id),
        "Worker environment does not identify the owned desktop"
    );
    let env = std::env::vars().collect();
    let log = d.state.join("services.log");
    let spawn = |p: &mut Processes, argv: Vec<String>| {
        p.spawn(&argv, &env, &d.state, &log, false, "owned_service".into())
    };
    spawn(
        processes,
        vec![
            "dbus-daemon".into(),
            "--session".into(),
            "--nofork".into(),
            format!("--address={}", std::env::var("DBUS_SESSION_BUS_ADDRESS")?),
        ],
    )?;
    wait_path(&d.runtime.join("session/bus")).await?;
    spawn(
        processes,
        vec![
            "kwin_wayland".into(),
            "--virtual".into(),
            "--width".into(),
            d.width.to_string(),
            "--height".into(),
            d.height.to_string(),
            "--output-count".into(),
            "1".into(),
            "--socket".into(),
            "lcu-wayland".into(),
            "--no-lockscreen".into(),
        ],
    )?;
    wait_path(&d.runtime.join("session/lcu-wayland")).await?;
    // Authorization is written only on this private bus and private XDG data path.
    spawn(processes, vec!["/usr/libexec/xdg-permission-store".into()])?;
    let conn = zbus::Connection::session().await?;
    wait_name(&conn, "org.kde.KWin").await?;
    // Wait without activating a second copy while our foreground service starts.
    wait_name(&conn, "org.freedesktop.impl.portal.PermissionStore").await?;
    let proxy = zbus::Proxy::new(
        &conn,
        "org.freedesktop.impl.portal.PermissionStore",
        "/org/freedesktop/impl/portal/PermissionStore",
        "org.freedesktop.impl.portal.PermissionStore",
    )
    .await?;
    let mut permissions = BTreeMap::new();
    permissions.insert("", vec!["yes"]);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let result: std::result::Result<(), _> = proxy
                .call(
                    "Set",
                    &(
                        "kde-authorized",
                        true,
                        "remote-desktop",
                        &permissions,
                        zbus::zvariant::Value::from(0u32),
                    ),
                )
                .await;
            if result.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("Private permission-store authorization failed")?;
    // The KDE backend and portal frontend can activate each other during startup.
    // Let the private bus coalesce activation; spawning both independently races
    // their name acquisition and leaves a failed duplicate mistaken for lost service.
    let bus = zbus::Proxy::new(
        &conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await?;
    tokio::time::timeout(
        Duration::from_secs(25),
        bus.call::<_, _, u32>(
            "StartServiceByName",
            &("org.freedesktop.portal.Desktop", 0u32),
        ),
    )
    .await
    .context("Private portal activation timed out")??;
    wait_name(&conn, "org.freedesktop.portal.Desktop").await?;
    wait_name(&conn, "org.freedesktop.impl.portal.desktop.kde").await?;
    Ok(())
}
