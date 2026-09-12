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

/// Directory name the daemon-owned tree lives under, relative to
/// `$XDG_STATE_HOME` and `$XDG_RUNTIME_DIR` (product rename, issue #106).
pub const DIR_NAME: &str = "canter";

/// Pre-rename directory name. A user whose state/runtime tree already
/// exists under this name keeps using it: the directory is adopted **in
/// place** (never copied, moved, migrated, or deleted), and the new name is
/// used only when no pre-rename tree exists. Normative rule:
/// docs/contracts/compatibility.md, "Product rename (issue #106)".
pub const LEGACY_DIR_NAME: &str = "herdr-fleet";

/// Choose this user's directory name: [`DIR_NAME`], except when a
/// pre-rename tree exists and the new one does not (adopt in place).
fn adopted_dir_name(state_home: &std::path::Path) -> &'static str {
    if state_home.join(DIR_NAME).exists() || !state_home.join(LEGACY_DIR_NAME).exists() {
        DIR_NAME
    } else {
        LEGACY_DIR_NAME
    }
}

/// All per-user paths for one daemon instance. Values are derived per call
/// (never cached) so tests and the daemon observe the same environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonPaths {
    /// State root for this user ([`DIR_NAME`] under the state home; a
    /// pre-rename ([`LEGACY_DIR_NAME`]) tree is adopted in place when it
    /// exists and the new one does not).
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
    /// Lane checkpoint brief artifact directory (issue #74; daemon-owned,
    /// derived briefs only — the durable capture lives in the database).
    pub checkpoints_dir: PathBuf,
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
        let state_home = state_home_for(xdg_state_home, home)?;
        let dir_name = adopted_dir_name(&state_home);
        let state_root = state_home.join(dir_name);
        let socket_override = socket_override.filter(|path| !path.is_empty());
        // An explicit socket override removes the XDG_RUNTIME_DIR
        // requirement: the socket moves into the override's directory
        // (the lock stays with the state dir). Without one the XDG
        // runtime root applies for the socket, under the SAME directory
        // name the state tree uses (adoption keeps socket and state
        // together).
        let runtime_root: PathBuf = match &socket_override {
            Some(path) => PathBuf::from(path)
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/")),
            None => runtime_home_for(xdg_runtime_dir)?.join(dir_name),
        };
        let socket_path = match &socket_override {
            Some(path) => PathBuf::from(path),
            None => runtime_root.join("daemon.sock"),
        };
        // The single-writer lock is bound to the STATE directory (beside
        // the database/journal): one writer per state dir is enforced no
        // matter which socket path a daemon was started with.
        Ok(DaemonPaths {
            state_dir: state_root.clone(),
            runtime_dir: runtime_root,
            lock_path: state_root.join("daemon.lock"),
            socket_path,
            db_path: state_root.join("state.db"),
            audit_mirror_path: state_root.join("journal").join("audit.jsonl"),
            events_mirror_path: state_root.join("journal").join("events.jsonl"),
            backups_dir: state_root.join("backups"),
            checkpoints_dir: state_root.join("checkpoints"),
            log_path: state_root.join("daemon.log"),
        })
    }

    /// Create the daemon-owned directories with per-user permissions
    /// (0700). Refuses to follow an existing symlink for any directory.
    pub fn prepare(&self) -> Result<(), PathError> {
        for dir in [&self.state_dir, &self.backups_dir, &self.checkpoints_dir] {
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
    use std::os::unix::fs::PermissionsExt;
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
            // A pre-existing SHARED directory (world/group-writable, e.g. a
            // shared socket home) must never be retightened or trusted:
            // refuse instead (issue #5 r2, non-blocking (b)).
            let mode = meta.permissions().mode();
            if mode & 0o022 != 0 {
                return Err(error(
                    "dirs.not_private",
                    format!("refusing shared/world-writable directory {}", dir.display()),
                ));
            }
        }
        Err(_) => {
            std::fs::create_dir_all(dir)
                .map_err(|err| error("dirs.create", format!("create {}: {err}", dir.display())))?;
        }
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
    use std::path::Path;

    use super::*;

    #[cfg(unix)]
    fn mode_of(path: &std::path::Path) -> Option<u32> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|meta| meta.mode())
    }

    /// One temp base per test name keeps parallel tests isolated; every
    /// path is runtime-derived so no absolute literal reaches the tree.
    fn fixture_base(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hf-dirs-{}-{}", std::process::id(), name))
    }

    fn os(value: &std::path::Path) -> Option<std::ffi::OsString> {
        Some(value.as_os_str().to_owned())
    }

    fn derive_with(
        xdg_state: Option<&Path>,
        xdg_runtime: Option<&Path>,
        home: Option<&Path>,
        override_socket: Option<&str>,
    ) -> Result<DaemonPaths, PathError> {
        DaemonPaths::derive_for(
            xdg_state.map(|path| path.as_os_str().to_owned()),
            xdg_runtime.map(|path| path.as_os_str().to_owned()),
            home.map(|path| path.as_os_str().to_owned()),
            override_socket,
        )
    }

    #[test]
    fn paths_derive_from_xdg_environment_values() {
        let base = fixture_base("xdg");
        let state = base.join("st");
        let run = base.join("run");
        let home = base.join("home");
        let paths = derive_with(Some(&state), Some(&run), Some(&home), None).expect("derive");
        assert_eq!(paths.state_dir, state.join("canter"));
        assert_eq!(paths.runtime_dir, run.join("canter"));
        assert_eq!(paths.socket_path, run.join("canter/daemon.sock"));
        // The single-writer lock belongs to the STATE dir (AC1: one writer
        // per state dir regardless of socket path).
        assert_eq!(paths.lock_path, state.join("canter/daemon.lock"));
        assert_eq!(paths.db_path, state.join("canter/state.db"));
        assert_eq!(
            paths.audit_mirror_path,
            state.join("canter/journal/audit.jsonl")
        );
    }

    #[test]
    fn fresh_user_derives_the_new_directory() {
        // Neither tree exists: the canonical name applies everywhere.
        let base = fixture_base("fresh-106");
        let _ = std::fs::remove_dir_all(&base);
        let state = base.join("st");
        let run = base.join("run");
        let home = base.join("home");
        let paths = derive_with(Some(&state), Some(&run), Some(&home), None).expect("derive");
        assert_eq!(paths.state_dir, state.join(DIR_NAME));
        assert_eq!(paths.runtime_dir, run.join(DIR_NAME));
        assert_eq!(paths.socket_path, run.join(DIR_NAME).join("daemon.sock"));
    }

    #[test]
    fn pre_rename_tree_is_adopted_in_place() {
        // A pre-rename state/runtime tree keeps being used, byte-for-byte:
        // derivation must select the legacy directory and must NOT create,
        // copy, or touch the new one (state continuity, issue #106).
        let base = fixture_base("legacy-106");
        let _ = std::fs::remove_dir_all(&base);
        let state = base.join("st");
        let run = base.join("run");
        let home = base.join("home");
        std::fs::create_dir_all(state.join(LEGACY_DIR_NAME)).expect("legacy state dir");
        let paths = derive_with(Some(&state), Some(&run), Some(&home), None).expect("derive");
        assert_eq!(paths.state_dir, state.join(LEGACY_DIR_NAME));
        assert_eq!(paths.db_path, state.join(LEGACY_DIR_NAME).join("state.db"));
        assert_eq!(
            paths.lock_path,
            state.join(LEGACY_DIR_NAME).join("daemon.lock")
        );
        assert_eq!(paths.runtime_dir, run.join(LEGACY_DIR_NAME));
        assert_eq!(
            paths.socket_path,
            run.join(LEGACY_DIR_NAME).join("daemon.sock")
        );
        assert!(
            !state.join(DIR_NAME).exists(),
            "derivation must not create the new directory"
        );
    }

    #[test]
    fn new_tree_wins_when_both_exist() {
        // Once a new-name tree exists it is authoritative; the legacy tree
        // is left untouched (never merged, never deleted).
        let base = fixture_base("both-106");
        let _ = std::fs::remove_dir_all(&base);
        let state = base.join("st");
        let run = base.join("run");
        let home = base.join("home");
        std::fs::create_dir_all(state.join(DIR_NAME)).expect("new state dir");
        std::fs::create_dir_all(state.join(LEGACY_DIR_NAME)).expect("legacy state dir");
        let paths = derive_with(Some(&state), Some(&run), Some(&home), None).expect("derive");
        assert_eq!(paths.state_dir, state.join(DIR_NAME));
        assert_eq!(paths.socket_path, run.join(DIR_NAME).join("daemon.sock"));
        assert!(
            state.join(LEGACY_DIR_NAME).is_dir(),
            "the legacy tree is not removed"
        );
    }

    #[test]
    fn state_home_falls_back_to_home_local_state() {
        let base = fixture_base("fallback");
        let home = base.join("home-u");
        assert_eq!(
            state_home_for(None, os(&home)).expect("fallback"),
            home.join(".local/state")
        );
        assert_eq!(
            config_home_for(None, os(&home)).expect("fallback"),
            home.join(".config")
        );
        let err = state_home_for(None, None).expect_err("must refuse");
        assert_eq!(err.code, "dirs.state");
    }

    #[test]
    fn socket_override_moves_socket_but_lock_stays_with_state() {
        let base = fixture_base("override");
        let state = base.join("st");
        let run = base.join("run");
        let home = base.join("home");
        let paths = derive_with(
            Some(&state),
            Some(&run),
            Some(&home),
            Some(
                base.join("custom")
                    .join("daemon.sock")
                    .to_str()
                    .expect("utf8"),
            ),
        )
        .expect("derive");
        assert_eq!(paths.socket_path, base.join("custom").join("daemon.sock"));
        assert_eq!(
            paths.lock_path,
            state.join("canter/daemon.lock"),
            "the writer lock stays with the state dir, not the socket"
        );
    }

    #[test]
    fn missing_runtime_dir_is_a_typed_error_unless_socket_override_present() {
        let base = fixture_base("runtime");
        let state = base.join("st");
        let home = base.join("home");
        let err = derive_with(Some(&state), None, Some(&home), None).expect_err("must refuse");
        assert_eq!(err.code, "dirs.runtime");
        // An explicit socket override removes the runtime-dir requirement.
        let paths = derive_with(
            Some(&state),
            None,
            Some(&home),
            Some(
                base.join("custom")
                    .join("daemon.sock")
                    .to_str()
                    .expect("utf8"),
            ),
        )
        .expect("derive");
        assert_eq!(paths.socket_path, base.join("custom").join("daemon.sock"));
        assert_eq!(paths.lock_path, state.join("canter/daemon.lock"));
    }

    #[cfg(unix)]
    #[test]
    fn prepared_directories_are_private_and_symlinks_refused() {
        let base = fixture_base("prepare");
        let state = base.join("state");
        let run = base.join("run");
        let home = base.join("home");
        let paths = derive_with(Some(&state), Some(&run), Some(&home), None).expect("derive");
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
