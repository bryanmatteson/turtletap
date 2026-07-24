//! Endpoint conventions and leader supervision for resident products.
//!
//! Starting a resident is a race: several clients may find no leader at the
//! same moment, and a newer binary may need to displace an older one that is
//! still serving. This module owns that choreography — locking, probing,
//! replacement, and readiness — while the caller keeps the product-specific
//! part, namely how its own resident process is spawned.

use std::{
    collections::hash_map::DefaultHasher,
    env, fs,
    hash::{Hash, Hasher},
    io,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::Child,
    thread,
    time::{Duration, Instant},
};

use super::{
    ClientCapabilities, ClientRequest, ControlResult, LeaderLock,
    blocking::{Client, Timeouts},
};

/// Longest socket path the platform reliably accepts for a Unix domain socket.
pub const MAX_SOCKET_PATH_BYTES: usize = 100;

/// Where a product's resident socket lives when the user has not chosen a path.
///
/// The parent directory is keyed by a hash of `HOME`, so separate users on a
/// shared machine never contend for one socket.
#[must_use]
pub fn default_socket_path(slug: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    env::var_os("HOME").hash(&mut hasher);
    env::temp_dir()
        .join(format!("{slug}-{:016x}", hasher.finish()))
        .join("resident.sock")
}

/// Where durable session state lives for a given socket.
#[must_use]
pub fn default_state_dir(socket: &Path) -> PathBuf {
    socket.with_extension("state")
}

/// Rejects socket paths the platform cannot bind.
pub fn validate_socket_path(path: &Path) -> io::Result<()> {
    let length = path.as_os_str().as_bytes().len();
    if length > MAX_SOCKET_PATH_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "socket path is {length} bytes; use a path no longer than {MAX_SOCKET_PATH_BYTES} bytes"
            ),
        ));
    }
    Ok(())
}

/// Creates the socket's parent directory with owner-only permissions.
///
/// A symlinked parent is refused rather than followed: the socket's directory
/// governs who may connect, so an attacker-controlled link would hand over
/// every session hosted beneath it.
pub fn prepare_socket_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket path has no parent"))?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing symlinked socket parent: {}", parent.display()),
            ));
        }
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("socket parent is not a directory: {}", parent.display()),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
}

/// Whether a leader is listening and answering on `socket`.
pub fn probe(socket: &Path, binary_version: &str, client_name: &str) -> io::Result<bool> {
    let mut client = match connect(socket, binary_version, client_name) {
        Ok(client) => client,
        Err(error) if is_absent(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    match client.request(ClientRequest::Ping) {
        Ok(ControlResult::Pong) => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "resident returned the wrong ping response",
        )),
        Err(error) => {
            let error = io::Error::other(error.to_string());
            if is_absent(&error) {
                Ok(false)
            } else {
                Err(error)
            }
        }
    }
}

pub use super::replacement_is_newer;

/// Outcome of [`ensure_leader`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnsureOutcome {
    /// A compatible leader was already serving.
    AlreadyRunning,
    /// This call won the election and started the leader.
    Spawned,
}

/// Inputs to [`ensure_leader`].
#[derive(Clone, Copy, Debug)]
pub struct EnsureConfig<'a> {
    /// Endpoint the leader binds.
    pub socket: &'a Path,
    /// Version of the binary requesting leadership.
    pub binary_version: &'a str,
    /// Name this client reports during handshakes.
    pub client_name: &'a str,
    /// Total budget for starting and reaching readiness.
    pub start_timeout: Duration,
    /// Delay between readiness polls.
    pub poll_interval: Duration,
}

impl<'a> EnsureConfig<'a> {
    /// Configuration with a five-second budget polled every twenty milliseconds.
    #[must_use]
    pub fn new(socket: &'a Path, binary_version: &'a str, client_name: &'a str) -> Self {
        Self {
            socket,
            binary_version,
            client_name,
            start_timeout: Duration::from_secs(5),
            poll_interval: Duration::from_millis(20),
        }
    }
}

/// Guarantees a leader is serving `config.socket`, starting one if needed.
///
/// Reuses a running leader, replaces one older than `config.binary_version`,
/// and otherwise takes the lock and spawns via `spawn`. The lock is released
/// only once the new leader has bound its socket, so a second caller either
/// waits or finds the leader already answering — never both spawn.
pub fn ensure_leader(
    config: EnsureConfig<'_>,
    mut spawn: impl FnMut(&Path) -> io::Result<Child>,
) -> io::Result<EnsureOutcome> {
    let path = config.socket;
    if use_or_replace_running_leader(&config)? {
        return Ok(EnsureOutcome::AlreadyRunning);
    }
    prepare_socket_parent(path)?;

    let deadline = Instant::now() + config.start_timeout;
    let mut lock = LeaderLock::for_socket(path);
    loop {
        if lock.try_acquire().map_err(io::Error::other)? {
            break;
        }
        if probe(path, config.binary_version, config.client_name)? {
            return Ok(EnsureOutcome::AlreadyRunning);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for another client to start the resident",
            ));
        }
        thread::sleep(config.poll_interval);
    }
    if probe(path, config.binary_version, config.client_name)? {
        lock.release_for_handoff().map_err(io::Error::other)?;
        return Ok(EnsureOutcome::AlreadyRunning);
    }
    lock.cleanup_stale_socket().map_err(io::Error::other)?;

    let mut child = spawn(path)?;
    loop {
        if socket_connectable(path) {
            lock.release_for_handoff().map_err(io::Error::other)?;
            break;
        }
        if let Some(status) = child.try_wait()? {
            let mut detail = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                use std::io::Read as _;
                let _ = stderr.read_to_string(&mut detail);
            }
            return Err(io::Error::other(format!(
                "resident exited during startup ({status}): {}",
                detail.trim()
            )));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "resident did not bind its socket within the start timeout",
            ));
        }
        thread::sleep(config.poll_interval);
    }
    while Instant::now() < deadline {
        if probe(path, config.binary_version, config.client_name)? {
            return Ok(EnsureOutcome::Spawned);
        }
        thread::sleep(config.poll_interval);
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "resident did not become ready within the start timeout",
    ))
}

/// Returns whether a usable leader is already serving, asking an older one to
/// stand down first when this binary is newer.
fn use_or_replace_running_leader(config: &EnsureConfig<'_>) -> io::Result<bool> {
    let path = config.socket;
    let mut client = match connect(path, config.binary_version, config.client_name) {
        Ok(client) => client,
        Err(error) if is_absent(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    let current = client.leader().binary_version.clone();
    if !replacement_is_newer(&current, config.binary_version) {
        return Ok(true);
    }
    client
        .request(ClientRequest::ReplaceLeader {
            binary_version: config.binary_version.to_owned(),
        })
        .map_err(|error| io::Error::other(error.to_string()))?;
    drop(client);

    let deadline = Instant::now() + config.start_timeout;
    while socket_connectable(path) && Instant::now() < deadline {
        thread::sleep(config.poll_interval);
    }
    if socket_connectable(path) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "older resident accepted replacement but did not release its socket",
        ));
    }
    Ok(false)
}

fn connect(socket: &Path, binary_version: &str, client_name: &str) -> io::Result<Client> {
    validate_socket_path(socket)?;
    Client::connect(
        socket,
        binary_version,
        client_name,
        ClientCapabilities {
            incremental_events: true,
            resumable: true,
            driver_leases: true,
        },
        Timeouts::default(),
    )
    .map_err(|error| match error {
        super::ClientError::Io(error) => error,
        other => io::Error::other(other.to_string()),
    })
}

fn socket_connectable(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// Whether the failure means "no leader here" rather than a real fault.
fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_requires_a_strictly_newer_version() {
        assert!(replacement_is_newer("0.1.0", "0.2.0"));
        assert!(!replacement_is_newer("0.2.0", "0.2.0"));
        assert!(!replacement_is_newer("0.2.0", "0.1.0"));
    }

    #[test]
    fn unparseable_versions_fall_back_to_string_ordering() {
        assert!(replacement_is_newer("alpha", "beta"));
        assert!(!replacement_is_newer("beta", "alpha"));
    }

    #[test]
    fn socket_paths_longer_than_the_platform_limit_are_refused() {
        let long = PathBuf::from("/".to_owned() + &"a".repeat(MAX_SOCKET_PATH_BYTES));
        assert!(validate_socket_path(&long).is_err());
        assert!(validate_socket_path(Path::new("/tmp/short.sock")).is_ok());
    }

    #[test]
    fn state_directory_is_derived_from_the_socket() {
        assert_eq!(
            default_state_dir(Path::new("/tmp/turtletap-abc/resident.sock")),
            PathBuf::from("/tmp/turtletap-abc/resident.state")
        );
    }
}
