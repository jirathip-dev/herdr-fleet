//! Per-user directory/path derivation for the daemon foundation (issue #5).
//!
//! All paths derive from environment variables (`XDG_CONFIG_HOME`,
//! `XDG_STATE_HOME`, `XDG_RUNTIME_DIR`, `HOME`) or from an explicit
//! `daemon.socket` config override. No machine-specific default path is
//! compiled in: the daemon refuses to run when the per-user runtime/state
//! directories cannot be derived, and every test supplies its own XDG
//! environment (clean-host fixtures). Nothing here ever reads a host-level
//! service configuration.

use std::env;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// An error deriving or preparing a per-user path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathError {
    /// Stable error code (`dirs.state`, `dirs.runtime`, `dirs.socket`).
    pub code: &'static str,
    /// Human message.
    pub message: String,
}

fn error(code: &'static str, message: impl Into<String>) -> PathError {
    PathError {
        code,
        message: message.into(),
    }
}

/// `$XDG_CONFIG_HOME` or `$HOME/.config` (config discovery home).
pub fn config_home() -> Result<PathBuf, PathError> {
    config_home_for(env::var_os("XDG_CONFIG_HOME"), env::var_os("HOME"))
}

fn config_home_for(
    xdg_config_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf, PathError> {
    if let Some(value) = xdg_config_home {
        if value.is_empty() {
            return Err(error("dirs.config", "XDG_CONFIG_HOME is set but empty"));
        }
        return Ok(PathBuf::from(value));
    }
    home.map(PathBuf::from)
        .map(|h| h.join(".config"))
        .ok_or_else(|| {
            error(
                "dirs.config",
                "cannot derive the XDG config home: neither XDG_CONFIG_HOME nor HOME is set",
            )
        })
}

/// `$XDG_STATE_HOME` or `$HOME/.local/state` (daemon state home).
pub fn state_home() -> Result<PathBuf, PathError> {
    state_home_for(env::var_os("XDG_STATE_HOME"), env::var_os("HOME"))
}

fn state_home_for(
    xdg_state_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf, PathError> {
    if let Some(value) = xdg_state_home {
        if value.is_empty() {
            return Err(error("dirs.state", "XDG_STATE_HOME is set but empty"));
        }
        return Ok(PathBuf::from(value));
    }
    home.map(PathBuf::from)
        .map(|h| h.join(".local").join("state"))
        .ok_or_else(|| {
            error(
                "dirs.state",
                "cannot derive the XDG state home: neither XDG_STATE_HOME nor HOME is set",
            )
        })
}

/// `$XDG_RUNTIME_DIR` (daemon socket/lock home; per-user).
pub fn runtime_home() -> Result<PathBuf, PathError> {
    runtime_home_for(env::var_os("XDG_RUNTIME_DIR"))
}

fn runtime_home_for(xdg_runtime_dir: Option<std::ffi::OsString>) -> Result<PathBuf, PathError> {
    match xdg_runtime_dir {
        Some(value) if !value.is_empty() => Ok(PathBuf::from(value)),
        _ => Err(error(
            "dirs.runtime",
            "cannot derive the per-user runtime dir: XDG_RUNTIME_DIR is not set; \
             set XDG_RUNTIME_DIR (launchd/systemd user sessions provide one) or pass \
             an explicit `daemon.socket` in the config",
        )),
    }
}

/// All per-user paths for one daemon instance. Values are derived per call
/// (never cached) so tests and the daemon observe the same environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonPaths {
    /// State root for this user (`herdr-fleet` under the state home).
    pub state_dir: PathBuf,
    /// Runtime root for this user (socket parent; 0700 by convention).
    pub runtime_dir: PathBuf,
    /// The per-user Unix socket path.
    pub socket_path: PathBuf,
    /// The single-writer lock file next to the socket.
    pub lock_path: PathBuf,
    /// SQLite state database (daemon-owned; clients never read it).
    pub db_path: PathBuf,
    /// Audit JSONL mirror file (bounded, hash-chained to the table).
    pub audit_mirror_path: PathBuf,
    /// Event JSONL mirror file (bounded, seq-ordered).
    pub events_mirror_path: PathBuf,
    /// Backup artifact directory (daemon-owned backups only).
    pub backups_dir: PathBuf,
    /// Allowlisted structured daemon log (JSONL).
    pub log_path: PathBuf,
}

impl DaemonPaths {
    /// Derive the standard paths from the live environment (a daemon socket
    /// override wins; otherwise the XDG runtime default).
    pub fn derive(socket_override: Option<&str>) -> Result<DaemonPaths, PathError> {
        DaemonPaths::derive_for(
            env::var_os("XDG_STATE_HOME"),
            env::var_os("XDG_RUNTIME_DIR"),
            env::var_os("HOME"),
            socket_override,
        )
    }

    /// Pure derivation used by [`DaemonPaths::derive`] and by unit tests
    /// (no environment mutation needed).
    fn derive_for(
        xdg_state_home: Option<std::ffi::OsString>,
        xdg_runtime_dir: Option<std::ffi::OsString>,
        home: Option<std::ffi::OsString>,
        socket_override: Option<&str>,
    ) -> Result<DaemonPaths, PathError> {
        let state_root = state_home_for(xdg_state_home, home)?.join("herdr-fleet");
        let socket_override = socket_override.filter(|path| !path.is_empty());
        // An explicit socket override removes the XDG_RUNTIME_DIR
        // requirement: the pair (socket + lock) moves into the override's
        // directory. Without one the XDG runtime root applies.
        let runtime_root: PathBuf = match &socket_override {
            Some(path) => PathBuf::from(path)
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/")),
            None => runtime_home_for(xdg_runtime_dir)?.join("herdr-fleet"),
        };
        let socket_path = match &socket_override {
            Some(path) => PathBuf::from(path),
            None => runtime_root.join("daemon.sock"),
        };
        // The lock file always sits next to the socket so a socket override
        // cannot split the pair across permission domains.
        let lock_dir = socket_path
            .parent()
            .map(PathBuf::from)
            .ok_or_else(|| error("dirs.socket", "socket path has no parent directory"))?;
        Ok(DaemonPaths {
            state_dir: state_root.clone(),
            runtime_dir: runtime_root,
            lock_path: lock_dir.join("daemon.lock"),
            socket_path,
            db_path: state_root.join("state.db"),
            audit_mirror_path: state_root.join("journal").join("audit.jsonl"),
            events_mirror_path: state_root.join("journal").join("events.jsonl"),
            backups_dir: state_root.join("backups"),
            log_path: state_root.join("daemon.log"),
        })
    }

    /// Create the daemon-owned directories with per-user permissions
    /// (0700). Refuses to follow an existing symlink for any directory.
    pub fn prepare(&self) -> Result<(), PathError> {
        for dir in [&self.state_dir, &self.backups_dir] {
            create_private_dir(dir)?;
        }
        if let Some(journal_dir) = self.audit_mirror_path.parent() {
            create_private_dir(journal_dir)?;
        }
        if let Some(socket_dir) = self.socket_path.parent() {
            create_private_dir(socket_dir)?;
        }
        Ok(())
    }
}

/// Create one directory (and parents) with 0700 permissions; a symlink at
/// the leaf is refused (path containment), never followed.
fn create_private_dir(dir: &std::path::Path) -> Result<(), PathError> {
    use std::fs::symlink_metadata;
    match symlink_metadata(dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(error(
                    "dirs.symlink",
                    format!("refusing symlink at {}", dir.display()),
                ));
            }
            if !meta.is_dir() {
                return Err(error(
                    "dirs.not_dir",
                    format!("{} exists and is not a directory", dir.display()),
                ));
            }
        }
        Err(_) => std::fs::create_dir_all(dir)
            .map_err(|err| error("dirs.create", format!("create {}: {err}", dir.display())))?,
    }
    // Ensure the permission is per-user even when the directory pre-existed
    // from an earlier run under a stricter umask.
    let result = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    if let Err(err) = result {
        return Err(error(
            "dirs.permissions",
            format!("chmod 0700 {}: {err}", dir.display()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode_of(path: &std::path::Path) -> Option<u32> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|meta| meta.mode())
    }

    fn os(value: &str) -> Option<std::ffi::OsString> {
        Some(std::ffi::OsString::from(value))
    }

    fn derive_with(
        xdg_state: Option<&str>,
        xdg_runtime: Option<&str>,
        override_socket: Option<&str>,
    ) -> Result<DaemonPaths, PathError> {
        DaemonPaths::derive_for(
            xdg_state.map(std::ffi::OsString::from),
            xdg_runtime.map(std::ffi::OsString::from),
            Some(std::ffi::OsString::from("/tmp/home")),
            override_socket,
        )
    }

    #[test]
    fn paths_derive_from_xdg_environment_values() {
        let paths = derive_with(Some("/tmp/st"), Some("/tmp/run"), None).expect("derive");
        assert_eq!(paths.state_dir, PathBuf::from("/tmp/st/herdr-fleet"));
        assert_eq!(paths.runtime_dir, PathBuf::from("/tmp/run/herdr-fleet"));
        assert_eq!(
            paths.socket_path,
            PathBuf::from("/tmp/run/herdr-fleet/daemon.sock")
        );
        assert_eq!(
            paths.lock_path,
            PathBuf::from("/tmp/run/herdr-fleet/daemon.lock")
        );
        assert_eq!(paths.db_path, PathBuf::from("/tmp/st/herdr-fleet/state.db"));
        assert_eq!(
            paths.audit_mirror_path,
            PathBuf::from("/tmp/st/herdr-fleet/journal/audit.jsonl")
        );
    }

    #[test]
    fn state_home_falls_back_to_home_local_state() {
        assert_eq!(
            state_home_for(None, os("/home/u")).expect("fallback"),
            PathBuf::from("/home/u/.local/state")
        );
        assert_eq!(
            config_home_for(None, os("/home/u")).expect("fallback"),
            PathBuf::from("/home/u/.config")
        );
        let err = state_home_for(None, None).expect_err("must refuse");
        assert_eq!(err.code, "dirs.state");
    }

    #[test]
    fn socket_override_moves_socket_and_lock_together() {
        let paths = derive_with(
            Some("/tmp/st"),
            Some("/tmp/run"),
            Some("/tmp/custom/daemon.sock"),
        )
        .expect("derive");
        assert_eq!(paths.socket_path, PathBuf::from("/tmp/custom/daemon.sock"));
        assert_eq!(
            paths.lock_path,
            PathBuf::from("/tmp/custom/daemon.lock"),
            "lock stays next to the socket"
        );
    }

    #[test]
    fn missing_runtime_dir_is_a_typed_error_unless_socket_override_present() {
        let err = derive_with(Some("/tmp/st"), None, None).expect_err("must refuse");
        assert_eq!(err.code, "dirs.runtime");
        // An explicit override removes the runtime-dir requirement.
        let paths =
            derive_with(Some("/tmp/st"), None, Some("/tmp/custom/daemon.sock")).expect("derive");
        assert_eq!(paths.socket_path, PathBuf::from("/tmp/custom/daemon.sock"));
    }

    #[cfg(unix)]
    #[test]
    fn prepared_directories_are_private_and_symlinks_refused() {
        let base = std::env::temp_dir().join(format!("hf-dirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let paths = DaemonPaths::derive_for(
            Some(std::ffi::OsString::from(base.join("state"))),
            Some(std::ffi::OsString::from(base.join("runtime"))),
            Some(std::ffi::OsString::from(base.join("home"))),
            None,
        )
        .expect("derive");
        paths.prepare().expect("prepare");
        for dir in [paths.state_dir.as_path(), paths.runtime_dir.as_path()] {
            assert!(dir.is_dir());
            assert_eq!(mode_of(dir), Some(0o40700), "0700 directory");
        }
        // A symlink where the state dir belongs is refused.
        let state_parent = paths.state_dir.parent().expect("parent");
        let _ = std::fs::remove_dir_all(state_parent);
        std::fs::create_dir_all(state_parent).expect("state parent");
        let elsewhere = state_parent.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("elsewhere");
        std::os::unix::fs::symlink(&elsewhere, &paths.state_dir).expect("symlink");
        let err = paths.prepare().expect_err("symlink must be refused");
        assert_eq!(err.code, "dirs.symlink");
        let _ = std::fs::remove_dir_all(state_parent);
    }
}
