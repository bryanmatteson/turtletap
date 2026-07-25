use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use fs2::FileExt;

/// Leader-election failures.
#[derive(Debug)]
pub enum LockError {
    /// Filesystem operation failed.
    Io(io::Error),
    /// Operation requires the lock to be held.
    NotHeld,
}

impl std::fmt::Display for LockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::NotHeld => formatter.write_str("resident leader lock is not held"),
        }
    }
}

impl std::error::Error for LockError {}

impl From<io::Error> for LockError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// An OS-backed single-winner resident lock.
pub struct LeaderLock {
    lock_path: PathBuf,
    socket_path: PathBuf,
    file: Option<File>,
    cleanup_socket: bool,
}

impl LeaderLock {
    /// Creates a lock adjacent to a resident socket.
    #[must_use]
    pub fn for_socket(socket_path: impl Into<PathBuf>) -> Self {
        let socket_path = socket_path.into();
        let mut lock_path = socket_path.clone();
        lock_path.set_extension("lock");
        Self::new(lock_path, socket_path)
    }

    #[cfg(all(unix, feature = "tokio"))]
    pub(crate) fn for_startup(socket_path: impl Into<PathBuf>) -> Self {
        let socket_path = socket_path.into();
        let mut lock_path = socket_path.clone();
        lock_path.set_extension("start.lock");
        Self::new(lock_path, socket_path)
    }

    fn new(lock_path: PathBuf, socket_path: PathBuf) -> Self {
        Self {
            lock_path,
            socket_path,
            file: None,
            cleanup_socket: false,
        }
    }

    /// Attempts to acquire exclusive ownership without blocking.
    pub fn try_acquire(&mut self) -> Result<bool, LockError> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.lock_path)?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                self.file = Some(file);
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Blocks until exclusive ownership is available.
    pub fn acquire(&mut self) -> Result<(), LockError> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.lock_path)?;
        file.lock_exclusive()?;
        self.file = Some(file);
        Ok(())
    }

    /// Records the leader PID for diagnostics and enables socket cleanup on drop.
    pub fn assume_leadership(&mut self, pid: u32) -> Result<(), LockError> {
        let file = self.file.as_mut().ok_or(LockError::NotHeld)?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        write!(file, "{pid}")?;
        file.sync_all()?;
        self.cleanup_socket = true;
        Ok(())
    }

    /// Reads the diagnostic PID without acquiring the lock.
    #[must_use]
    pub fn read_pid(&self) -> Option<u32> {
        let mut value = String::new();
        File::open(&self.lock_path)
            .and_then(|mut file| file.read_to_string(&mut value))
            .ok()?;
        value.trim().parse().ok()
    }

    /// Removes a stale socket while ownership is held.
    pub fn cleanup_stale_socket(&self) -> Result<(), LockError> {
        if self.file.is_none() {
            return Err(LockError::NotHeld);
        }
        remove_socket(&self.socket_path)?;
        Ok(())
    }

    /// Releases a spawner's lock without deleting the child leader's socket.
    pub fn release_for_handoff(&mut self) -> Result<(), LockError> {
        let Some(file) = self.file.take() else {
            return Err(LockError::NotHeld);
        };
        FileExt::unlock(&file)?;
        self.cleanup_socket = false;
        Ok(())
    }

    /// Returns the lock-file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.lock_path
    }
}

impl Drop for LeaderLock {
    fn drop(&mut self) {
        if self.cleanup_socket {
            let _ = remove_socket(&self.socket_path);
        }
    }
}

fn remove_socket(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        #[cfg(unix)]
        Ok(metadata) if std::os::unix::fs::FileTypeExt::is_socket(&metadata.file_type()) => {
            fs::remove_file(path)
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("refusing to replace non-socket path: {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
