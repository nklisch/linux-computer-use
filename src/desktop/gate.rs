//! A short-lived exec gate closes the spawn/publication crash window.
use anyhow::{Context, Result, ensure};
use std::{
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command},
};

pub struct Pending {
    child: Option<Child>,
    channel: UnixStream,
}
impl Pending {
    pub fn spawn(helper: &std::path::Path, mut command: Command, argv: &[String]) -> Result<Self> {
        let (channel, child_channel) = UnixStream::pair()?;
        let fd = child_channel.as_raw_fd();
        command
            .arg("desktop-exec-gate")
            .arg("--gate-fd")
            .arg(fd.to_string())
            .arg("--")
            .args(argv);
        // Only the gate channel survives helper exec. All worker ownership locks
        // remain CLOEXEC, and the gate runs after exec (never waits in pre_exec).
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        ensure!(
            command.get_program() == helper.as_os_str(),
            "Wrong exec gate helper"
        );
        let child = command.spawn().context("Start exec gate")?;
        drop(child_channel);
        Ok(Self {
            child: Some(child),
            channel,
        })
    }
    pub fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }
    pub fn authorize(mut self) -> Result<Child> {
        self.channel.write_all(&[1])?;
        let mut error = Vec::new();
        self.channel.read_to_end(&mut error)?;
        ensure!(
            error.is_empty(),
            "Target exec failed: {}",
            String::from_utf8_lossy(&error)
        );
        Ok(self.child.take().unwrap())
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Publication failure/drop must not leave an unauthorized helper. EOF
            // is sufficient on parent crash; explicit teardown also reaps it.
            let _ = self.channel.shutdown(std::net::Shutdown::Both);
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
pub fn run(fd: i32, argv: Vec<String>) -> Result<()> {
    ensure!(
        fd >= 3 && !argv.is_empty(),
        "Invalid internal exec gate arguments"
    );
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut channel = UnixStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let mut authorization = [0];
    if channel.read(&mut authorization)? == 0 {
        return Ok(());
    }
    ensure!(authorization == [1], "Invalid exec authorization");
    // Successful target exec closes the status channel; failure carries the real
    // exec error. A helper launch alone is never reported as application launch.
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
        "Cannot close gate on exec"
    );
    let error = Command::new(&argv[0]).args(&argv[1..]).exec();
    channel.write_all(error.to_string().as_bytes())?;
    Err(error.into())
}
