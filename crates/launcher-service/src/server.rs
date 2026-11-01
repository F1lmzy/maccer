//! Local transport. Sessions and provider state belong to the service, not clients.
use crate::Service;
use anyhow::{Context, Result, bail};
use launcher_protocol::{RpcRequest, framing};
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::Shutdown,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub const MAX_CLIENTS: usize = 32;
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

struct SocketPath {
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl Drop for SocketPath {
    fn drop(&mut self) {
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path)
            && metadata.file_type().is_socket()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// A bound single-instance socket. Binding never removes an existing path.
/// The runtime directory must already exist, belong to this user and be private.
pub struct Server {
    listener: UnixListener,
    service: Arc<Service>,
    connections: Arc<Mutex<HashMap<u64, UnixStream>>>,
    next_connection: AtomicU64,
    _socket_path: SocketPath,
}
impl Server {
    pub fn bind(path: impl AsRef<Path>, service: Arc<Service>) -> Result<Self> {
        let path = path.as_ref();
        if !path.is_absolute() {
            bail!("socket path must be absolute");
        }
        let parent = path
            .parent()
            .context("socket path needs a parent directory")?;
        let directory = std::fs::symlink_metadata(parent).context("reading socket directory")?;
        // getuid is not a credential-changing syscall; effective UID owns the service.
        let uid = unsafe { libc::geteuid() };
        if !directory.is_dir() || directory.uid() != uid || directory.mode() & 0o077 != 0 {
            bail!("socket directory must be a real owner-only directory for the current user");
        }
        let listener = UnixListener::bind(path).context(
            "binding service socket; an existing socket may belong to a running service",
        )?;
        let metadata = std::fs::symlink_metadata(path)?;
        let socket_path = SocketPath {
            path: path.into(),
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            service,
            connections: Arc::new(Mutex::new(HashMap::new())),
            next_connection: AtomicU64::new(1),
            _socket_path: socket_path,
        })
    }

    /// Runs until requested to stop. Client disconnection never stops the service.
    pub fn serve_until(&self, stop: &AtomicBool) -> Result<()> {
        let result = self.accept_loop(stop);
        self.disconnect_clients();
        result
    }

    fn accept_loop(&self, stop: &AtomicBool) -> Result<()> {
        while !stop.load(Ordering::Acquire) {
            let (stream, _) = match self.listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            // macOS inherits O_NONBLOCK from the listener. Session framing uses blocking
            // I/O with explicit deadlines, so normalize every accepted descriptor.
            stream.set_nonblocking(false)?;
            if !same_user(&stream)? {
                let _ = stream.shutdown(Shutdown::Both);
                continue;
            }
            let id = self.next_connection.fetch_add(1, Ordering::Relaxed);
            {
                let mut connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
                if connections.len() >= MAX_CLIENTS {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                connections.insert(id, stream.try_clone()?);
            }
            let connections = self.connections.clone();
            let mut session = self.service.session();
            let spawn = thread::Builder::new()
                .name(format!("maccer-client-{id}"))
                .spawn(move || {
                    let _connection = ConnectionGuard { id, connections };
                    let mut stream = stream;
                    // Panics in one provider or session cannot terminate the listener.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        while let Ok(bytes) = read_request(&mut stream) {
                            // Malformed envelopes close this connection, not the listener.
                            let Ok(request) = serde_json::from_slice::<RpcRequest>(&bytes) else {
                                break;
                            };
                            if request.jsonrpc != "2.0"
                                || request.id.is_empty()
                                || request.id.len() > 128
                                || request.id.contains('\0')
                            {
                                break;
                            }
                            let (response, asset_bytes) = session.handle_transport(request);
                            let Ok(bytes) = serde_json::to_vec(&response) else {
                                break;
                            };
                            if write_response(&mut stream, &bytes).is_err() {
                                break;
                            }
                            if let Some(asset_bytes) = asset_bytes
                                && write_response(&mut stream, &asset_bytes).is_err()
                            {
                                break;
                            }
                        }
                    }));
                });
            if let Err(error) = spawn {
                self.connections
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                return Err(error.into());
            }
        }
        Ok(())
    }

    fn disconnect_clients(&self) {
        let connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        for stream in connections.values() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.disconnect_clients();
    }
}
struct ConnectionGuard {
    id: u64,
    connections: Arc<Mutex<HashMap<u64, UnixStream>>>,
}
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

// Each frame has one deadline, so a byte-at-a-time sender cannot keep extending it.
struct DeadlineStream<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
}
impl DeadlineStream<'_> {
    fn remaining(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "frame deadline exceeded"))
    }
}
impl Read for DeadlineStream<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        self.stream.read(bytes)
    }
}
impl Write for DeadlineStream<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}
fn read_request(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    // An idle frontend may stay connected indefinitely. Start the frame deadline
    // only when its first byte arrives; shutdown() still interrupts this read.
    stream.set_read_timeout(None)?;
    let mut first = [0];
    stream.read_exact(&mut first)?;
    let deadline = DeadlineStream {
        stream,
        deadline: Instant::now() + FRAME_TIMEOUT,
    };
    framing::read_frame(&mut io::Cursor::new(first).chain(deadline))
}
fn write_response(stream: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    framing::write_frame(
        &mut DeadlineStream {
            stream,
            deadline: Instant::now() + FRAME_TIMEOUT,
        },
        bytes,
    )
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn same_user(stream: &UnixStream) -> io::Result<bool> {
    let mut uid = 0;
    let mut gid = 0;
    // Descriptor is live; both output pointers are writable UID/GID storage.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid == unsafe { libc::geteuid() })
}
#[cfg(target_os = "linux")]
fn same_user(stream: &UnixStream) -> io::Result<bool> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // Kernel writes at most the supplied ucred size into initialized local storage.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid peer credentials",
        ));
    }
    Ok(credentials.uid == unsafe { libc::geteuid() })
}
#[cfg(not(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
fn same_user(_: &UnixStream) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "peer credential checks are unavailable",
    ))
}
