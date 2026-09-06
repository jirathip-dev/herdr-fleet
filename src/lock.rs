//! Single-writer daemon lock + per-user socket containment (issue #5, AC1).
//!
//! One daemon per host/user is enforced with an advisory exclusive file
//! lock (`flock`) on a lock file inside the STATE directory (beside the
//! SQLite database): one writer per state dir no matter which socket path a
//! daemon was started with.
//! The OS releases the lock automatically when the owning process dies —
//! including SIGKILL and power loss — which is the stale-process recovery
//! mechanism; the lock file's recorded owner is diagnostic only and never
//! authoritative. The lock file is opened with `O_NOFOLLOW` so a symlink
//! planted at the path is refused, never followed (path containment).
//!
//! Socket preparation refuses every non-socket occupant of the socket path:
//! a live daemon is reported as busy, a dead daemon's socket (connect
//! refused) is removed and rebound, and a symlink or regular file is an
//! unsafe-path error.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

use fs2::FileExt;

#[cfg(unix)]
use libc::O_NOFOLLOW;

/// A failure acquiring the daemon lock or preparing the socket path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockError {
    /// Stable error code (`lock.busy`, `lock.io`, `lock.unsafe_path`).
    pub code: &'static str,
    /// Human message.
    pub message: String,
}

fn error(code: &'static str, message: impl Into<String>) -> LockError {
    LockError {
        code,
        message: message.into(),
    }
}

/// The recorded owner of a lock file (diagnostic; the flock is the truth).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockOwner {
    /// Owning process id recorded when the lock was taken.
    pub pid: u32,
    /// RFC3339 UTC start time recorded when the lock was taken.
    pub started_at: String,
}

/// The held single-writer lock. Dropping releases the advisory lock.
#[derive(Debug)]
pub struct DaemonLock {
    file: File,
}

impl DaemonLock {
    /// Acquire the exclusive lock at `path`, recording `pid`/`started_at`.
    /// Fails with `lock.busy` when another process holds the lock.
    pub fn acquire(path: &Path, started_at: &str) -> Result<DaemonLock, LockError> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        // Path containment: never follow a symlink at the lock path.
        options.custom_flags(O_NOFOLLOW);
        options.mode(0o600);
        let mut file = options.open(path).map_err(|err| {
            error(
                "lock.io",
                format!("open lock file {}: {err}", path.display()),
            )
        })?;
        match file.try_lock_exclusive() {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                let owner = read_owner(&file).unwrap_or(None);
                let detail = owner
                    .map(|owner| format!("pid {} (started {})", owner.pid, owner.started_at))
                    .unwrap_or_else(|| "owner could not be read".to_string());
                return Err(error(
                    "lock.busy",
                    format!("another daemon holds the lock ({detail})"),
                ));
            }
            Err(err) => {
                return Err(error("lock.io", format!("lock {}: {err}", path.display())));
            }
        }
        // Record the owner (diagnostic only). Content is rewritten on every
        // acquisition, so a stale pid from a crashed daemon never survives.
        file.set_len(0)
            .map_err(|err| error("lock.io", format!("truncate lock: {err}")))?;
        let pid = std::process::id();
        file.seek(SeekFrom::Start(0))
            .map_err(|err| error("lock.io", format!("seek lock: {err}")))?;
        write!(&mut file, "pid {pid}\nstarted_at {started_at}\n")
            .map_err(|err| error("lock.io", format!("write lock owner: {err}")))?;
        file.sync_all()
            .map_err(|err| error("lock.io", format!("fsync lock owner: {err}")))?;
        Ok(DaemonLock { file })
    }

    /// Read the recorded owner of an *already acquired* lock.
    pub fn recorded_owner(&self) -> Result<Option<LockOwner>, LockError> {
        read_owner(&self.file)
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        // Best-effort unlock; the OS also releases on process death.
        let _ = FileExt::unlock(&self.file);
    }
}

/// Parse the recorded owner from lock file bytes.
fn parse_owner(text: &str) -> Option<LockOwner> {
    let mut pid = None;
    let mut started_at = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("pid ") {
            pid = value.parse::<u32>().ok();
        } else if let Some(value) = line.strip_prefix("started_at ") {
            started_at = Some(value.to_string());
        }
    }
    Some(LockOwner {
        pid: pid?,
        started_at: started_at?,
    })
}

fn read_owner(file: &File) -> Result<Option<LockOwner>, LockError> {
    let mut content = String::new();
    let mut reader = file
        .try_clone()
        .map_err(|err| error("lock.io", format!("clone lock file for reading: {err}")))?;
    // The clone shares the file offset with the writer (which ended at the
    // end of the owner text); rewind before reading.
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|err| error("lock.io", format!("seek lock owner: {err}")))?;
    reader
        .read_to_string(&mut content)
        .map_err(|err| error("lock.io", format!("read lock owner: {err}")))?;
    Ok(parse_owner(&content))
}

/// Occupant classification of a socket path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocketPresence {
    /// No occupant; safe to bind.
    Absent,
    /// A live daemon answers on this socket.
    Active,
    /// A dead daemon's socket (connect refused); safe to remove + rebind.
    Stale,
    /// A symlink or non-socket occupant; never removed or followed.
    Unsafe(String),
}

/// Classify what occupies `socket_path` (never modifies anything).
pub fn socket_presence(socket_path: &Path) -> SocketPresence {
    use std::fs::symlink_metadata;
    use std::os::unix::fs::FileTypeExt;
    let meta = match symlink_metadata(socket_path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return SocketPresence::Absent,
        Err(err) => {
            return SocketPresence::Unsafe(format!("stat failed: {err}"));
        }
    };
    if meta.file_type().is_symlink() {
        return SocketPresence::Unsafe("socket path is a symlink (refused, never followed)".into());
    }
    if !meta.file_type().is_socket() {
        return SocketPresence::Unsafe(format!(
            "socket path is occupied by a non-socket ({})",
            file_type_name(&meta.file_type())
        ));
    }
    match UnixStream::connect(socket_path) {
        Ok(_) => SocketPresence::Active,
        Err(err) if matches!(err.kind(), std::io::ErrorKind::ConnectionRefused) => {
            SocketPresence::Stale
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => SocketPresence::Stale,
        Err(err) => SocketPresence::Unsafe(format!("socket probe failed: {err}")),
    }
}

fn file_type_name(file_type: &std::fs::FileType) -> &'static str {
    if file_type.is_dir() {
        "directory"
    } else if file_type.is_file() {
        "regular file"
    } else {
        "special file"
    }
}

/// Remove a stale socket (only call after [`SocketPresence::Stale`]).
pub fn remove_stale_socket(socket_path: &Path) -> Result<(), LockError> {
    std::fs::remove_file(socket_path)
        .map_err(|err| error("lock.io", format!("remove stale socket: {err}")))
}

/// Whether a daemon currently answers on `socket_path` (read-only probe).
pub fn daemon_answers(socket_path: &Path) -> Result<bool, String> {
    match socket_presence(socket_path) {
        SocketPresence::Active => Ok(true),
        SocketPresence::Absent | SocketPresence::Stale => Ok(false),
        SocketPresence::Unsafe(reason) => Err(reason),
    }
}

/// Bind the daemon listener: refuse non-socket occupants, reclaim a stale
/// socket, bind, and pin per-user permissions (0600 socket, 0700 parent).
pub fn bind_listener(socket_path: &Path) -> Result<std::os::unix::net::UnixListener, LockError> {
    use std::os::unix::fs::PermissionsExt;
    match socket_presence(socket_path) {
        SocketPresence::Absent => {}
        SocketPresence::Stale => remove_stale_socket(socket_path)?,
        SocketPresence::Active => {
            return Err(error(
                "lock.busy",
                format!("a daemon already serves {}", socket_path.display()),
            ));
        }
        SocketPresence::Unsafe(reason) => {
            return Err(error("lock.unsafe_path", reason));
        }
    }
    if let Some(parent) = socket_path.parent() {
        // A pre-existing SHARED directory (world/group-writable, e.g. /tmp)
        // must never be retightened or trusted as a socket home: refuse.
        // Directories the daemon prepares are per-user 0700 already and pass
        // this check (defense in depth for callers that bypass prepare()).
        use std::os::unix::fs::PermissionsExt;
        let shared = std::fs::metadata(parent)
            .map(|meta| meta.permissions().mode() & 0o022 != 0)
            .unwrap_or(true);
        if shared {
            return Err(error(
                "lock.unsafe_dir",
                format!(
                    "socket parent {} is a shared/world-writable directory; use a private path",
                    parent.display()
                ),
            ));
        }
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    let listener = std::os::unix::net::UnixListener::bind(socket_path)
        .map_err(|err| error("lock.io", format!("bind {}: {err}", socket_path.display())))?;
    let _ = std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600));
    Ok(listener)
}

/// Human summary for diagnostics (used by `daemon status`).
pub fn describe_socket(socket_path: &Path) -> String {
    match socket_presence(socket_path) {
        SocketPresence::Active => "running".to_string(),
        SocketPresence::Absent => "absent".to_string(),
        SocketPresence::Stale => "stale (previous daemon did not exit cleanly)".to_string(),
        SocketPresence::Unsafe(reason) => format!("unsafe: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "hf-lock-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).expect("temp dir");
        base
    }

    #[test]
    fn owner_parse_round_trip() {
        let owner = parse_owner("pid 4242\nstarted_at 2026-09-06T00:00:00Z\n");
        assert_eq!(
            owner,
            Some(LockOwner {
                pid: 4242,
                started_at: "2026-09-06T00:00:00Z".to_string(),
            })
        );
        assert_eq!(parse_owner("pid nope\n"), None);
        assert_eq!(parse_owner(""), None);
    }

    #[test]
    fn second_lock_is_busy_and_first_drop_releases() {
        let dir = temp_dir();
        let path = dir.join("daemon.lock");
        let first = DaemonLock::acquire(&path, "2026-09-06T00:00:00Z").expect("first lock");
        let owner = first.recorded_owner().expect("owner").expect("recorded");
        assert_eq!(owner.pid, std::process::id());
        let err = DaemonLock::acquire(&path, "2026-09-06T00:00:01Z").expect_err("must be busy");
        assert_eq!(err.code, "lock.busy");
        assert!(err.message.contains("pid"));
        drop(first);
        let _ = DaemonLock::acquire(&path, "2026-09-06T00:00:02Z").expect("reacquire after drop");
    }

    #[test]
    fn lock_symlink_is_refused() {
        let dir = temp_dir();
        let target = dir.join("target.lock");
        std::fs::write(&target, "x").expect("write target");
        let link = dir.join("link.lock");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let err = DaemonLock::acquire(&link, "2026-09-06T00:00:00Z").expect_err("must refuse");
        assert_eq!(err.code, "lock.io", "O_NOFOLLOW refuses the symlink");
        // The target was never locked or modified.
        assert_eq!(
            std::fs::read_to_string(&target).expect("target intact"),
            "x"
        );
    }

    #[test]
    fn socket_presence_classifies_occupants() {
        let dir = temp_dir();
        let socket_path = dir.join("daemon.sock");
        assert_eq!(socket_presence(&socket_path), SocketPresence::Absent);

        // A regular file occupant is unsafe (never removed).
        std::fs::write(&socket_path, "not a socket").expect("write file");
        assert!(matches!(
            socket_presence(&socket_path),
            SocketPresence::Unsafe(_)
        ));

        // A symlink occupant is unsafe.
        let _ = std::fs::remove_file(&socket_path);
        let elsewhere = dir.join("elsewhere");
        std::fs::write(&elsewhere, "x").expect("write");
        std::os::unix::fs::symlink(&elsewhere, &socket_path).expect("symlink");
        assert!(matches!(
            socket_presence(&socket_path),
            SocketPresence::Unsafe(reason) if reason.contains("symlink")
        ));

        // A bound listener is active; its stale form is recoverable.
        let _ = std::fs::remove_file(&socket_path);
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind");
        assert_eq!(socket_presence(&socket_path), SocketPresence::Active);
        drop(listener);
        // After the listener dropped, connect refuses -> stale (kernel
        // teardown of the listen socket can take a few milliseconds).
        let mut stale = false;
        for _ in 0..50 {
            if socket_presence(&socket_path) == SocketPresence::Stale {
                stale = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(stale, "listener drop must make the socket stale");
        remove_stale_socket(&socket_path).expect("remove stale");
        assert_eq!(socket_presence(&socket_path), SocketPresence::Absent);
    }

    #[test]
    fn daemon_answers_probe_does_not_block() {
        let dir = temp_dir();
        let socket_path = dir.join("probe.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind");
        // Set a read timeout so a connecting peer never blocks the probe.
        let _ = listener.set_nonblocking(true);
        assert_eq!(
            daemon_answers(&socket_path),
            Ok(true),
            "a bound socket answers"
        );
        drop(listener);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(daemon_answers(&socket_path), Ok(false));
    }
}
