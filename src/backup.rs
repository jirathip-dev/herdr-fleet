//! Daemon-owned backup/restore artifacts (issue #5 backup.create/restore).
//!
//! A backup is a consistent SQLite snapshot (`VACUUM INTO`) plus a
//! checksummed manifest. Backups cover daemon-owned state only — never
//! external repositories (spec-state.md boundary table). The manifest is an
//! implementation-local canonical JSON document (self-describing
//! `"schema":"hf-backup/v1"`); a registry fixture family for it arrives with
//! the lifecycle slice that owns deterministic export/restore.
//!
//! Restore is orchestrated by the daemon (journaled intent, epoch rotation,
//! grant invalidation, ambiguity marking); this module only moves verified
//! bytes: copy to a temp file, fsync, atomic rename over the live database.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::canonical::{canonical_text, sha256_hex};
use crate::time;
use crate::value::{Val, integer, object, string};

/// A backup/restore artifact failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupError {
    /// Stable error code (`backup.io`, `backup.verify`, `backup.invalid`,
    /// `backup.not_found`, `backup.exists`).
    pub code: &'static str,
    /// Human message.
    pub message: String,
}

fn backup_error(code: &'static str, message: impl Into<String>) -> BackupError {
    BackupError {
        code,
        message: message.into(),
    }
}

/// Parsed backup manifest (canonical JSON, checksummed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupManifest {
    /// State epoch captured by this backup.
    pub epoch: i64,
    /// Journal (audit) seq captured by this backup.
    pub journal_seq: i64,
    /// Event seq captured by this backup.
    pub event_seq: i64,
    /// sha256 of the snapshot database file.
    pub db_sha256: String,
    /// Snapshot file size in bytes.
    pub db_bytes: i64,
    /// RFC3339 UTC creation time.
    pub created_at: String,
    /// Base file name of the snapshot database (`*.db` next to manifest).
    pub snapshot_name: String,
}

impl BackupManifest {
    /// Canonical JSON bytes of the manifest (stable serialization).
    pub fn to_val(&self) -> Val {
        object(vec![
            ("schema", string("hf-backup/v1")),
            ("epoch", integer(self.epoch)),
            ("journal_seq", integer(self.journal_seq)),
            ("event_seq", integer(self.event_seq)),
            ("db_sha256", string(&self.db_sha256)),
            ("db_bytes", integer(self.db_bytes)),
            ("created_at", string(&self.created_at)),
            ("snapshot", string(&self.snapshot_name)),
        ])
    }

    fn parse(doc: &Val, snapshot_name: &str) -> Result<BackupManifest, BackupError> {
        let field = |key: &str| -> Result<String, BackupError> {
            doc.get(key)
                .and_then(Val::as_str)
                .map(str::to_string)
                .ok_or_else(|| backup_error("backup.invalid", format!("manifest missing {key:?}")))
        };
        let int_field = |key: &str| -> Result<i64, BackupError> {
            match doc.get(key) {
                Some(Val::Int(value)) if *value >= 0 => Ok(*value),
                _ => Err(backup_error(
                    "backup.invalid",
                    format!("manifest {key:?} must be a non-negative integer"),
                )),
            }
        };
        if field("schema")? != "hf-backup/v1" {
            return Err(backup_error(
                "backup.invalid",
                "manifest schema is not hf-backup/v1",
            ));
        }
        Ok(BackupManifest {
            epoch: int_field("epoch")?,
            journal_seq: int_field("journal_seq")?,
            event_seq: int_field("event_seq")?,
            db_sha256: field("db_sha256")?,
            db_bytes: int_field("db_bytes")?,
            created_at: field("created_at")?,
            snapshot_name: snapshot_name.to_string(),
        })
    }
}

/// Name components for one backup pair
/// (`backup-<ts>-e<epoch>-<nanos>.db/.json`). The nanosecond suffix keeps
/// rapid backup.create calls on distinct artifact pairs.
fn backup_stem(epoch: i64) -> String {
    let unix = crate::time::rfc3339_now().replace([':', '-'], "");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    format!("backup-{unix}-e{epoch}-{nanos:09}")
}

/// The manifest path for a snapshot file path (`.db` -> `.json`).
pub fn manifest_path_for(snapshot_path: &Path) -> PathBuf {
    snapshot_path.with_extension("json")
}

/// Produce a consistent snapshot of `db_path` into `dest_path` using SQLite
/// `VACUUM INTO`, then checksum it. The destination must not exist.
pub fn create_snapshot(db_path: &Path, dest_path: &Path) -> Result<(String, i64), BackupError> {
    if dest_path.exists() {
        return Err(backup_error(
            "backup.exists",
            format!(
                "snapshot destination {} already exists",
                dest_path.display()
            ),
        ));
    }
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            backup_error("backup.io", format!("create {}: {err}", parent.display()))
        })?;
    }
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|err| backup_error("backup.io", format!("open state for snapshot: {err}")))?;
    // VACUUM INTO cannot be parameterized; the destination path comes from
    // the daemon's own backups directory (single quotes are escaped).
    let escaped = dest_path.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("VACUUM INTO '{escaped}';"))
        .map_err(|err| backup_error("backup.io", format!("VACUUM INTO snapshot: {err}")))?;
    let bytes = fs::metadata(dest_path)
        .map_err(|err| backup_error("backup.io", format!("stat snapshot: {err}")))?
        .len();
    let digest = file_sha256(dest_path)?;
    Ok((digest, bytes as i64))
}

/// Write a manifest next to its snapshot (atomic tmp + rename + fsync).
pub fn write_manifest(snapshot_path: &Path, manifest: &BackupManifest) -> Result<(), BackupError> {
    let path = manifest_path_for(snapshot_path);
    let tmp = path.with_extension("json.tmp");
    let mut file = fs::File::create(&tmp)
        .map_err(|err| backup_error("backup.io", format!("create {}: {err}", tmp.display())))?;
    file.write_all(canonical_text(&manifest.to_val()).as_bytes())
        .map_err(|err| backup_error("backup.io", format!("write manifest: {err}")))?;
    file.sync_all()
        .map_err(|err| backup_error("backup.io", format!("fsync manifest: {err}")))?;
    fs::rename(&tmp, &path)
        .map_err(|err| backup_error("backup.io", format!("rename manifest: {err}")))?;
    Ok(())
}

/// Read + parse a manifest from its file.
pub fn read_manifest(manifest_path: &Path) -> Result<BackupManifest, BackupError> {
    let text = fs::read_to_string(manifest_path).map_err(|err| {
        backup_error(
            "backup.not_found",
            format!("read manifest {}: {err}", manifest_path.display()),
        )
    })?;
    let doc = Val::parse_json(&text)
        .map_err(|err| backup_error("backup.invalid", format!("manifest parse: {err}")))?;
    let snapshot_name = manifest_path
        .file_stem()
        .map(|stem| format!("{}.db", stem.to_string_lossy()))
        .ok_or_else(|| backup_error("backup.invalid", "manifest path has no file name"))?;
    BackupManifest::parse(&doc, &snapshot_name)
}

/// Verify a snapshot against its manifest (external read-back: checksum +
/// size). Used after backup.create and before restore.
pub fn verify_backup(snapshot_path: &Path, manifest: &BackupManifest) -> Result<(), BackupError> {
    let meta = fs::metadata(snapshot_path)
        .map_err(|err| backup_error("backup.not_found", format!("stat snapshot: {err}")))?;
    if meta.len() as i64 != manifest.db_bytes {
        return Err(backup_error(
            "backup.verify",
            format!(
                "snapshot size {} does not match manifest {}",
                meta.len(),
                manifest.db_bytes
            ),
        ));
    }
    let digest = file_sha256(snapshot_path)?;
    if digest != manifest.db_sha256 {
        return Err(backup_error(
            "backup.verify",
            format!(
                "snapshot sha256 {digest} does not match manifest {}",
                manifest.db_sha256
            ),
        ));
    }
    Ok(())
}

/// Every snapshot+manifest pair currently present in `backups_dir`.
pub fn list_backups(backups_dir: &Path) -> Result<Vec<BackupManifest>, BackupError> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(backups_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => {
            return Err(backup_error(
                "backup.io",
                format!("list {}: {err}", backups_dir.display()),
            ));
        }
    };
    let mut manifests: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| backup_error("backup.io", format!("read dir: {err}")))?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") && path.is_file() {
            manifests.push(path);
        }
    }
    manifests.sort();
    for manifest_path in manifests {
        if let Ok(manifest) = read_manifest(&manifest_path) {
            let snapshot = manifest_path.with_file_name(&manifest.snapshot_name);
            if snapshot.exists() && verify_backup(&snapshot, &manifest).is_ok() {
                out.push(manifest);
            }
        } // unreadable/incomplete pairs are not offered.
    }
    Ok(out)
}

/// Move a verified backup snapshot into place over the live state database:
/// copy to a temp file, fsync, then atomic rename. Crash windows: before
/// the rename the live DB is untouched; after the rename the restored file
/// is in place and the daemon's reconcile completes the epoch rotation.
pub fn restore_snapshot(snapshot_path: &Path, state_db_path: &Path) -> Result<(), BackupError> {
    let tmp = state_db_path.with_extension("db.restore-tmp");
    fs::copy(snapshot_path, &tmp)
        .map_err(|err| backup_error("backup.io", format!("copy snapshot to temp: {err}")))?;
    let file = fs::File::open(&tmp)
        .map_err(|err| backup_error("backup.io", format!("open restored temp: {err}")))?;
    file.sync_all()
        .map_err(|err| backup_error("backup.io", format!("fsync restored temp: {err}")))?;
    fs::rename(&tmp, state_db_path).map_err(|err| {
        backup_error(
            "backup.io",
            format!("rename restored state into place: {err}"),
        )
    })?;
    if let Some(parent) = state_db_path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// sha256 of a file's bytes.
pub fn file_sha256(path: &Path) -> Result<String, BackupError> {
    let bytes = fs::read(path)
        .map_err(|err| backup_error("backup.io", format!("read {}: {err}", path.display())))?;
    Ok(sha256_hex(&bytes))
}

/// Build a manifest value for a fresh backup (created after snapshot).
pub fn manifest_for(
    epoch: i64,
    journal_seq: i64,
    event_seq: i64,
    db_sha256: String,
    db_bytes: i64,
    snapshot_path: &Path,
) -> BackupManifest {
    BackupManifest {
        epoch,
        journal_seq,
        event_seq,
        db_sha256,
        db_bytes,
        created_at: time::rfc3339_now(),
        snapshot_name: snapshot_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// Path helpers for a new backup pair in `backups_dir`.
pub fn new_backup_paths(backups_dir: &Path, epoch: i64) -> (PathBuf, PathBuf) {
    let stem = backup_stem(epoch);
    (
        backups_dir.join(format!("{stem}.db")),
        backups_dir.join(format!("{stem}.json")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Retention, State};

    fn temp_dir(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("hf-backup-{}", std::process::id()));
        fs::create_dir_all(&base).expect("temp dir");
        let dir = base.join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("case dir");
        dir
    }

    #[test]
    fn snapshot_manifest_round_trip_and_verify() {
        let dir = temp_dir("roundtrip");
        let db_path = dir.join("state.db");
        let state = State::open(&db_path, Retention::default()).expect("open");
        let key = "ik_backup-test-0001";
        state
            .journal_intent(
                "mutate.backup.create",
                "example-org/widgets",
                key,
                &"ab".repeat(8),
                "backup.create",
                None,
                None,
                &canonical_text(&object(vec![
                    ("schema", string("hf-rpc-request/v1")),
                    ("id", string(&"ab".repeat(8))),
                    ("method", string("backup.create")),
                    ("params", object(vec![("idempotency_key", string(key))])),
                ])),
            )
            .expect("journal");
        drop(state);

        let backups = dir.join("backups");
        let (snapshot, _manifest_path) = new_backup_paths(&backups, 1);
        let (digest, bytes) = create_snapshot(&db_path, &snapshot).expect("snapshot");
        assert!(bytes > 0);
        let (epoch, journal_seq, event_seq, _) = State::open(&db_path, Retention::default())
            .expect("reopen")
            .summary()
            .expect("summary");
        let manifest = manifest_for(
            epoch,
            journal_seq,
            event_seq,
            digest.clone(),
            bytes,
            &snapshot,
        );
        write_manifest(&snapshot, &manifest).expect("write manifest");
        let read = read_manifest(&manifest_path_for(&snapshot)).expect("read manifest");
        assert_eq!(read.epoch, manifest.epoch);
        assert_eq!(read.db_sha256, digest);
        verify_backup(&snapshot, &read).expect("verify");

        // A tampered snapshot is refused by verification (checksum bite).
        fs::write(&snapshot, b"tampered").expect("tamper");
        let err = verify_backup(&snapshot, &read).expect_err("must fail verification");
        assert_eq!(err.code, "backup.verify");
    }

    #[test]
    fn restore_replaces_state_and_reopens() {
        let dir = temp_dir("restore");
        let db_path = dir.join("state.db");
        {
            let state = State::open(&db_path, Retention::default()).expect("open");
            let key = "ik_restore-test-0001";
            state
                .journal_intent(
                    "mutate.grant.revoke",
                    "example-org/widgets",
                    key,
                    &"cd".repeat(8),
                    "grants.revoke",
                    None,
                    None,
                    "{}",
                )
                .expect("journal");
        }
        let backups = dir.join("backups");
        let (snapshot, _) = new_backup_paths(&backups, 1);
        create_snapshot(&db_path, &snapshot).expect("snapshot");
        let original = fs::read(&db_path).expect("read original db");
        assert_ne!(fs::read(&snapshot).expect("read snapshot"), original);

        // Restore the snapshot over the live DB (bytes differ, but both are
        // valid databases; reopening proves integrity).
        restore_snapshot(&snapshot, &db_path).expect("restore");
        let state = State::open(&db_path, Retention::default()).expect("reopen after restore");
        assert_eq!(state.verify_chain(), Ok(()));
        assert!(state.current_epoch().expect("epoch") >= 1);
    }

    #[test]
    fn list_backups_offers_only_verified_pairs() {
        let dir = temp_dir("list");
        let db_path = dir.join("state.db");
        let state = State::open(&db_path, Retention::default()).expect("open");
        drop(state);
        let backups = dir.join("backups");
        let (snapshot_a, _) = new_backup_paths(&backups, 1);
        let (digest_a, bytes_a) = create_snapshot(&db_path, &snapshot_a).expect("snapshot a");
        let manifest_a = manifest_for(1, 0, 0, digest_a.clone(), bytes_a, &snapshot_a);
        write_manifest(&snapshot_a, &manifest_a).expect("manifest a");

        // An unverified/incomplete second pair (manifest without snapshot)
        // must not be offered.
        let (snapshot_b, _) = new_backup_paths(&backups, 2);
        let (digest_b, bytes_b) = create_snapshot(&db_path, &snapshot_b).expect("snapshot b");
        fs::remove_file(&snapshot_b).expect("remove snapshot b");
        let manifest_b = manifest_for(1, 0, 0, digest_b, bytes_b, &snapshot_b);
        write_manifest(&snapshot_b, &manifest_b).expect("manifest b");

        let listed = list_backups(&backups).expect("list");
        assert_eq!(listed.len(), 1, "only the verified pair is offered");
        assert_eq!(listed[0].db_sha256, digest_a);
    }
}
