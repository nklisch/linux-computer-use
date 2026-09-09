//! Cancellable stdio without changing the invoking process's descriptor flags.
use std::{
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{FileTypeExt, OpenOptionsExt},
    },
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, unix::AsyncFd};

pub struct Pollable {
    fd: AsyncFd<std::fs::File>,
    socket: bool,
}
pub enum Input {
    Pollable(Pollable),
    File(tokio::fs::File),
}
pub enum Output {
    Pollable(Pollable),
    File(tokio::fs::File),
}

fn open(number: i32, read: bool) -> io::Result<(std::fs::File, bool)> {
    let duplicate = unsafe { libc::fcntl(number, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    let inherited = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(duplicate) });
    let kind = inherited.metadata()?.file_type();
    // Regular files must share the caller's offset and O_APPEND semantics.
    if kind.is_file() || kind.is_socket() {
        return Ok((inherited, kind.is_socket()));
    }
    // Reopening pipes/terminals gives independent flags. Socket-backed stdio (used
    // by some execution harnesses) cannot be reopened via procfs: duplicate it,
    // leaving shared flags untouched, and use MSG_DONTWAIT for each operation.
    std::fs::OpenOptions::new()
        .read(read)
        .write(!read)
        .custom_flags(libc::O_NONBLOCK)
        .open(format!("/proc/self/fd/{number}"))
        .map(|file| (file, false))
}
fn input(number: i32) -> io::Result<Input> {
    let (file, socket) = open(number, true)?;
    match AsyncFd::new(file.try_clone()?) {
        Ok(fd) => Ok(Input::Pollable(Pollable { fd, socket })),
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => {
            Ok(Input::File(tokio::fs::File::from_std(file)))
        }
        Err(e) => Err(e),
    }
}
fn output(number: i32) -> io::Result<Output> {
    let (file, socket) = open(number, false)?;
    match AsyncFd::new(file.try_clone()?) {
        Ok(fd) => Ok(Output::Pollable(Pollable { fd, socket })),
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => {
            Ok(Output::File(tokio::fs::File::from_std(file)))
        }
        Err(e) => Err(e),
    }
}
pub fn stdin() -> io::Result<Input> {
    input(0)
}
pub fn stdout() -> io::Result<Output> {
    output(1)
}
impl AsyncRead for Input {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::File(file) => Pin::new(file).poll_read(cx, buf),
            Self::Pollable(p) => loop {
                let mut ready = std::task::ready!(p.fd.poll_read_ready(cx))?;
                match ready.try_io(|inner| {
                    let target = buf.initialize_unfilled();
                    let n = unsafe {
                        if p.socket {
                            libc::recv(
                                inner.as_raw_fd(),
                                target.as_mut_ptr().cast(),
                                target.len(),
                                libc::MSG_DONTWAIT,
                            )
                        } else {
                            libc::read(inner.as_raw_fd(), target.as_mut_ptr().cast(), target.len())
                        }
                    };
                    if n < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }) {
                    Ok(Ok(n)) => {
                        buf.advance(n);
                        return Poll::Ready(Ok(()));
                    }
                    Ok(Err(e)) => return Poll::Ready(Err(e)),
                    Err(_) => continue,
                }
            },
        }
    }
}
impl AsyncWrite for Output {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let p = match self.get_mut() {
            Self::File(file) => return Pin::new(file).poll_write(cx, buf),
            Self::Pollable(p) => p,
        };
        loop {
            let mut ready = std::task::ready!(p.fd.poll_write_ready(cx))?;
            match ready.try_io(|inner| {
                let n = unsafe {
                    if p.socket {
                        libc::send(
                            inner.as_raw_fd(),
                            buf.as_ptr().cast(),
                            buf.len(),
                            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                        )
                    } else {
                        libc::write(inner.as_raw_fd(), buf.as_ptr().cast(), buf.len())
                    }
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(r) => return Poll::Ready(r),
                Err(_) => continue,
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::File(f) => Pin::new(f).poll_flush(cx),
            _ => Poll::Ready(Ok(())),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::File(f) => Pin::new(f).poll_shutdown(cx),
            _ => Poll::Ready(Ok(())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn socket_stdio_is_cancellable_and_preserves_inherited_flags() {
        let (local, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = local.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        let mut reader = input(fd).unwrap();
        let mut writer = output(fd).unwrap();
        writer.write_all(b"out").await.unwrap();
        let mut bytes = [0; 3];
        peer.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"out");
        peer.write_all(b"in!").unwrap();
        reader.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"in!");
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                reader.read_exact(&mut bytes)
            )
            .await
            .is_err()
        );
        drop(reader);
        drop(writer);
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFL) }, flags);
    }
}
