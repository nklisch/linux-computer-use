use anyhow::{Context, Result, ensure};
use base64::Engine;
use clap::{Parser, Subcommand};
use linux_computer_use::{
    desktop::{self, DesktopOperation, LaunchArgs, Request, Response, daemon, rpc::Client},
    mcp::ComputerServer,
    types::*,
};
use rmcp::ServiceExt;
use serde::Deserialize;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[derive(Parser)]
#[command(
    version,
    about = "Native KDE/Wayland desktop control; CLI and MCP share a retained daemon"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Mode>,
}
#[derive(Subcommand)]
enum Mode {
    Mcp,
    /// Open the native read-only monitor; never starts or restarts a daemon.
    Monitor,
    Session {
        #[arg(long, default_value = "main")]
        desktop: String,
        #[arg(long)]
        claim: bool,
        #[arg(long, requires = "claim")]
        force: bool,
    },
    Screenshot {
        #[arg(long, default_value = "main")]
        desktop: String,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long, default_value_t = 0)]
        display: usize,
        #[arg(long, default_value_t = 1600)]
        max_dimension: u32,
    },
    Doctor,
    Stop {
        #[arg(long)]
        desktop: Option<String>,
    },
    Daemon {
        #[command(subcommand)]
        command: DaemonMode,
    },
    Desktop {
        #[command(subcommand)]
        command: DesktopMode,
    },
    #[command(hide = true)]
    DesktopWorker {
        descriptor: PathBuf,
        #[arg(long)]
        lifecycle_fd: i32,
    },
    #[command(hide = true)]
    DesktopExecGate {
        #[arg(long)]
        gate_fd: i32,
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
}
#[derive(Subcommand)]
enum DaemonMode {
    Run,
    Start,
    Status,
    Stop,
}
#[derive(Subcommand)]
enum DesktopMode {
    Create {
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value_t = 1280)]
        width: u32,
        #[arg(long, default_value_t = 720)]
        height: u32,
    },
    List,
    Show {
        desktop: String,
    },
    Launch {
        desktop: String,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long)]
        webkit_alternate_buffers: bool,
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    Destroy {
        desktop: String,
        #[arg(long)]
        force: bool,
    },
}
#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum LineRequest {
    Start {
        #[serde(flatten)]
        args: StartArgs,
    },
    Observe {
        #[serde(flatten)]
        args: ObserveArgs,
        output: Option<PathBuf>,
    },
    Act {
        #[serde(flatten)]
        args: ActArgs,
        output: Option<PathBuf>,
    },
    Inspect {
        #[serde(flatten)]
        args: InspectArgs,
    },
    Focus {
        #[serde(flatten)]
        args: FocusArgs,
    },
    Status,
    Stop,
    Claim {
        #[serde(default)]
        force: bool,
    },
    Release,
    Launch {
        #[serde(flatten)]
        args: LaunchArgs,
    },
}
#[tokio::main]
async fn main() -> Result<()> {
    let mode = Cli::parse().command.unwrap_or(Mode::Mcp);
    match mode {
        Mode::Monitor => {
            let companion = std::env::current_exe()?.with_file_name("lcu-monitor");
            use std::os::unix::process::CommandExt;
            let error = std::process::Command::new(&companion)
                .arg("--socket")
                .arg(desktop::runtime_root()?.join("daemon.sock"))
                .exec();
            return Err(error).with_context(|| {
                format!(
                    "Launch {}; build/install the optional Qt monitor companion first",
                    companion.display()
                )
            });
        }
        Mode::Doctor => return doctor().await,
        Mode::DesktopExecGate { gate_fd, argv } => return desktop::gate::run(gate_fd, argv),
        Mode::DesktopWorker {
            descriptor,
            lifecycle_fd,
        } => {
            return desktop::worker::run(daemon::read_descriptor(&descriptor)?, lifecycle_fd).await;
        }
        Mode::Daemon {
            command: DaemonMode::Run,
        } => return daemon::run().await,
        Mode::Daemon {
            command: DaemonMode::Start,
        } => {
            daemon::start().await?;
            println!("Daemon ready; retained desktops survive frontend disconnects");
            return Ok(());
        }
        Mode::Stop { desktop } => {
            // Start both independent groups before inspecting either result. A
            // dead/stalled worker must not strand older controllers' held input.
            let (workers, legacy) =
                tokio::join!(daemon::emergency_stop(desktop.as_deref()), async {
                    if desktop.is_none() && std::env::var_os("LCU_RUNTIME_DIR").is_none() {
                        linux_computer_use::local_stop::stop_all().await.map(Some)
                    } else {
                        Ok(None)
                    }
                });
            let mut errors = Vec::new();
            match workers {
                Ok(results) => {
                    println!("{}", serde_json::to_string_pretty(&results)?);
                    if !results.iter().all(|r| matches!(r, Ok(Response::Control(Reply::Stopped(s))) if s.desktop_close_confirmed)) {
                        errors.push("Some input-session closures were not confirmed".to_string());
                    }
                }
                Err(error) => errors.push(format!("Worker cleanup: {error:#}")),
            }
            match legacy {
                Ok(Some(summary)) => {
                    println!("{}", serde_json::to_string(&summary)?);
                    errors.extend(summary.errors);
                }
                Ok(None) => {}
                Err(error) => errors.push(format!("Legacy cleanup: {error:#}")),
            }
            ensure!(errors.is_empty(), "{}", errors.join("; "));
            return Ok(());
        }
        _ => {}
    }
    // First-class clients do not call MCP. Startup is idempotent under a daemon lock.
    if !matches!(mode, Mode::Daemon { .. }) {
        daemon::start().await?;
    }
    let client = daemon::connect().await?;
    let run = async {
        match mode {
            Mode::Mcp => {
                let service = ComputerServer::new(client.clone())
                    .serve((
                        linux_computer_use::stdio::stdin()?,
                        linux_computer_use::stdio::stdout()?,
                    ))
                    .await?;
                service.waiting().await?;
                Ok(())
            }
            Mode::Session {
                desktop,
                claim,
                force,
            } => session(&client, &desktop, claim, force).await,
            Mode::Screenshot {
                desktop,
                output,
                display,
                max_dimension,
            } => {
                emit(
                    client
                        .call(Request::Control {
                            desktop_id: desktop,
                            command: Command::Observe(ObserveArgs {
                                display,
                                max_dimension: Some(max_dimension),
                                ..Default::default()
                            }),
                        })
                        .await?,
                    Some(output),
                )
                .await
            }
            Mode::Daemon { command } => {
                let request = match command {
                    DaemonMode::Status => Request::Hello,
                    DaemonMode::Stop => Request::ShutdownDaemon,
                    _ => unreachable!(),
                };
                emit(client.call(request).await?, None).await
            }
            Mode::Desktop { command } => match command {
                DesktopMode::Create {
                    width,
                    height,
                    name,
                } => {
                    emit(
                        client
                            .call(Request::Desktop(DesktopOperation::Create {
                                width,
                                height,
                                name,
                            }))
                            .await?,
                        None,
                    )
                    .await
                }
                DesktopMode::List => {
                    emit(
                        client
                            .call(Request::Desktop(DesktopOperation::List))
                            .await?,
                        None,
                    )
                    .await
                }
                DesktopMode::Show { desktop } => {
                    let Response::Desktops(mut all) = client
                        .call(Request::Desktop(DesktopOperation::List))
                        .await?
                    else {
                        unreachable!()
                    };
                    all.retain(|d| d.descriptor.id == desktop);
                    ensure!(!all.is_empty(), "Unknown desktop");
                    emit(Response::Desktops(all), None).await
                }
                DesktopMode::Destroy { desktop, force } => {
                    emit(
                        client
                            .call(Request::Desktop(DesktopOperation::Destroy {
                                desktop_id: desktop,
                                force,
                            }))
                            .await?,
                        None,
                    )
                    .await
                }
                DesktopMode::Launch {
                    desktop,
                    cwd,
                    argv,
                    webkit_alternate_buffers,
                } => {
                    client
                        .call(Request::Desktop(DesktopOperation::Claim {
                            desktop_id: desktop.clone(),
                            force: false,
                        }))
                        .await?;
                    let launched = client
                        .call(Request::Desktop(DesktopOperation::Launch {
                            desktop_id: desktop.clone(),
                            args: LaunchArgs {
                                argv,
                                cwd,
                                env: Default::default(),
                                webkit_alternate_buffers,
                            },
                        }))
                        .await;
                    let release = client
                        .call(Request::Desktop(DesktopOperation::Release {
                            desktop_id: desktop,
                        }))
                        .await;
                    let response = launched?;
                    release?;
                    write_json(
                        serde_json::json!({"reply":response,"temporary_claim_released":true}),
                    )
                    .await
                }
            },
            _ => unreachable!(),
        }
    };
    let result = tokio::select! {r=run=>r,r=shutdown_signal()=>r};
    client.disconnect();
    result
}
async fn shutdown_signal() -> Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {r=tokio::signal::ctrl_c()=>r?,_=term.recv()=>{}}
    Ok(())
}
async fn session(client: &Client, desktop: &str, claim: bool, force: bool) -> Result<()> {
    let mut lines = BufReader::new(linux_computer_use::stdio::stdin()?).lines();
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let eof = tokio_util::sync::CancellationToken::new();
    let done = eof.clone();
    let reader = tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            if send.send(line).is_err() {
                break;
            }
        }
        done.cancel();
    });
    let run = async {
        if claim {
            emit(
                client
                    .call(Request::Desktop(DesktopOperation::Claim {
                        desktop_id: desktop.into(),
                        force,
                    }))
                    .await?,
                None,
            )
            .await?;
        }
        session_lines(client, desktop, &mut receive).await
    };
    let result = tokio::select! { biased; _ = eof.cancelled() => Ok(()), result = run => result };
    reader.abort();
    result
}
async fn session_lines(
    client: &Client,
    desktop: &str,
    lines: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
) -> Result<()> {
    while let Some(line) = lines.recv().await {
        if line.trim().is_empty() {
            continue;
        }
        let result = async {
            let request: LineRequest =
                serde_json::from_str(&line).context("Invalid command JSON")?;
            let desktop_id = desktop.to_owned();
            let (command, output) = match request {
                LineRequest::Start { args } => (Command::Start(args), None),
                LineRequest::Observe { args, output } => (Command::Observe(args), output),
                LineRequest::Act { args, output } => (Command::Act(args), output),
                LineRequest::Inspect { args } => (Command::Inspect(args), None),
                LineRequest::Focus { args } => (Command::Focus(args), None),
                LineRequest::Status => (Command::Status, None),
                LineRequest::Stop => (Command::Stop, None),
                other => {
                    let operation = match other {
                        LineRequest::Claim { force } => {
                            DesktopOperation::Claim { desktop_id, force }
                        }
                        LineRequest::Release => DesktopOperation::Release { desktop_id },
                        LineRequest::Launch { args } => {
                            DesktopOperation::Launch { desktop_id, args }
                        }
                        _ => unreachable!(),
                    };
                    return emit(client.call(Request::Desktop(operation)).await?, None).await;
                }
            };
            emit(
                client
                    .call(Request::Control {
                        desktop_id,
                        command,
                    })
                    .await?,
                output,
            )
            .await
        }
        .await;
        if let Err(error) = result {
            write_json(serde_json::json!({"error":format!("{error:#}")})).await?;
        }
    }
    Ok(())
}
async fn emit(response: Response, output: Option<PathBuf>) -> Result<()> {
    let mut json = match &response {
        Response::Control(reply) => serde_json::to_value(reply)?,
        _ => serde_json::to_value(&response)?,
    };
    if let Response::Control(reply) = &response
        && let Some(o) = reply.observation()
    {
        if let Some(path) = output {
            tokio::fs::write(&path, &o.png)
                .await
                .context("Writing image; input was not replayed")?;
            json["image_path"] = serde_json::json!(path);
        } else {
            json["image_base64"] =
                serde_json::json!(base64::engine::general_purpose::STANDARD.encode(&o.png));
        }
    }
    write_json(json).await
}
async fn write_json(value: serde_json::Value) -> Result<()> {
    let mut out = linux_computer_use::stdio::stdout()?;
    out.write_all(serde_json::to_string(&value)?.as_bytes())
        .await?;
    out.write_all(b"\n").await?;
    out.flush().await?;
    Ok(())
}
async fn doctor() -> Result<()> {
    let mut result = serde_json::json!({"version":env!("CARGO_PKG_VERSION"),"session_type":std::env::var("XDG_SESSION_TYPE").ok(),"desktop":std::env::var("XDG_CURRENT_DESKTOP").ok(),"permissions_requested":false,"input_backend":"xdg-desktop-portal Notify methods","capture_backend":"GStreamer PipeWire"});
    match gstreamer::init() {
        Ok(()) => {
            result["gstreamer"] = serde_json::json!(gstreamer::version_string().as_str());
            for name in [
                "pipewiresrc",
                "videoconvert",
                "queue",
                "identity",
                "appsink",
            ] {
                result[name] = serde_json::json!(gstreamer::ElementFactory::find(name).is_some());
            }
        }
        Err(e) => result["gstreamer_error"] = serde_json::json!(e.to_string()),
    }
    let probe = async {
        let conn = zbus::Connection::session().await?;
        for interface in ["RemoteDesktop", "ScreenCast", "Clipboard"] {
            let proxy = zbus::Proxy::new(
                &conn,
                "org.freedesktop.portal.Desktop",
                "/org/freedesktop/portal/desktop",
                format!("org.freedesktop.portal.{interface}"),
            )
            .await?;
            result[interface] = match proxy.get_property::<u32>("version").await {
                Ok(v) => serde_json::json!({"version":v}),
                Err(e) => serde_json::json!({"error":e.to_string()}),
            };
        }
        Ok::<(), anyhow::Error>(())
    };
    match tokio::time::timeout(std::time::Duration::from_secs(5), probe).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => result["portal_error"] = serde_json::json!(e.to_string()),
        Err(_) => result["portal_error"] = serde_json::json!("Portal probe timed out"),
    }
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
