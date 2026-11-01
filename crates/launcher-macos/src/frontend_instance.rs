//! Shared per-service native frontend presence lock. Keep the inode on disk after unlocking:
//! unlinking it would allow two callers to lock different inodes at one path.
use anyhow::{Context, Result, bail};
use std::{
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
};

pub struct FrontendInstance {
    _file: File,
}

impl FrontendInstance {
    /// Claim one native frontend process for this service socket, regardless of UI kind. `None` means an existing
    /// process still holds the lock, even if its presentation lease has expired.
    /// Dropping the claim releases the lock without stopping either process.
    pub fn claim(socket: &Path) -> Result<Option<Self>> {
        if !socket.is_absolute() {
            bail!("frontend instance socket must be absolute");
        }
        let parent = socket.parent().context("frontend socket needs a parent")?;
        let directory = std::fs::symlink_metadata(parent)?;
        // SAFETY: geteuid only reads the current effective user identifier.
        let uid = unsafe { libc::geteuid() };
        if !directory.is_dir() || directory.uid() != uid || directory.mode() & 0o077 != 0 {
            bail!("frontend instance directory must be owner-only");
        }
        let mut name = socket.as_os_str().to_os_string();
        name.push(".frontend.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(Path::new(&name))
            .context("opening frontend instance lock")?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            bail!("frontend instance lock must be an owner-only regular file");
        }
        // SAFETY: file owns a live descriptor. LOCK_NB never waits on another UI.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error.into());
        }
        Ok(Some(Self { _file: file }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn presence_is_exclusive_and_survives_stale_lock_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("rpc");
        let first = FrontendInstance::claim(&socket).unwrap().unwrap();
        assert!(FrontendInstance::claim(&socket).unwrap().is_none());
        drop(first);
        assert!(FrontendInstance::claim(&socket).unwrap().is_some());
    }

    #[test]
    fn rejects_symlink_and_insecure_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("rpc");
        let target = dir.path().join("target");
        std::fs::write(&target, "owned").unwrap();
        symlink(&target, dir.path().join("rpc.frontend.lock")).unwrap();
        assert!(FrontendInstance::claim(&socket).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "owned");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(FrontendInstance::claim(&dir.path().join("another")).is_err());
    }
}
