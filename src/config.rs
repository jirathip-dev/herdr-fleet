//! `hf-config/v1` / `hf-policy/v1` loading, validation, and typed model.
//!
//! Discovery contract (docs/contracts/spec-config.md): an explicit `--config`
//! path wins; otherwise the XDG default
//! (`$XDG_CONFIG_HOME/canter/config.toml`, falling back to
//! `$HOME/.config/canter/config.toml`) is used when present. There is
//! exactly one canonical config file and at most one explicit policy overlay
//! named by `config.policy.overlay` — no implicit profile/repository merge
//! stack exists. Overlay paths are resolved relative to the config file's
//! directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::formats::{is_repository_identity, is_slug};
use crate::schema::{Family, Refusal, Verdict, validate_bytes};
use crate::value::Val;

/// An error that prevents a config/policy document from being loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadError {
    /// Stable CLI error code (`config.invalid`, `config.not_found`,
    /// `policy.invalid`, `policy.not_found`).
    pub code: &'static str,
    /// Human message.
    pub message: String,
    /// Refusal class when the document itself was refused.
    pub refusal: Option<Refusal>,
    /// Path that was being loaded (display-only).
    pub path: String,
}

impl LoadError {
    fn invalid(path: &Path, verdict: &Verdict) -> LoadError {
        LoadError {
            code: "config.invalid",
            message: verdict.message().to_string(),
            refusal: verdict.refusal(),
            path: path.display().to_string(),
        }
    }
}

/// One configured repository entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repository {
    /// Config table key (slug).
    pub key: String,
    /// Derived `owner` component of the repository identity.
    pub owner: String,
    /// Derived `name` component of the repository identity.
    pub name: String,
    /// The configured origin URL.
    pub origin: String,
    /// Optional pinned branch.
    pub branch: Option<String>,
    /// Whether the entry is enabled (default: true).
    pub enabled: bool,
}

impl Repository {
    /// The `owner/name` repository identity.
    pub fn identity(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// One configured workflow pin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowPin {
    /// Config table key (slug).
    pub key: String,
    /// Workflow id.
    pub id: String,
    /// 64-hex hash of the canonical workflow document.
    pub hash: String,
}

/// One configured harness entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Harness {
    /// Harness table key.
    pub key: String,
    /// Adapter kind (e.g. `argv`).
    pub kind: String,
    /// Executable resolved via PATH (never an absolute path).
    pub executable: String,
    /// Explicit environment allowlist.
    pub env_allow: Vec<String>,
    /// Explicit provider binding token for the official prompt rows that
    /// carry the provider/model pair on argv (Pi, Jcode; issue #80). `None`
    /// when the config declares no binding — there is no default and no
    /// inference; the prompt refuses instead (`refusal.binding.missing`).
    pub provider: Option<String>,
    /// See [`Harness::provider`].
    pub model: Option<String>,
}

/// A bare binding token: non-empty, no whitespace, no path separators, no
/// NUL. The harness provider/model binding (issue #80) is carried as argv
/// data only — bare tokens are never paths and never shell text.
pub(crate) fn is_bare_token(text: &str) -> bool {
    !text.is_empty()
        && !text.contains('/')
        && !text.contains('\\')
        && !text.contains('\0')
        && !text.chars().any(char::is_whitespace)
}

/// The parsed policy overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Absolute path the overlay was loaded from.
    pub path: PathBuf,
    /// Repository allowlist (`owner/name`), if present.
    pub repositories: Option<Vec<String>>,
    /// `production_confirmation` rule (`tty` | `deny`), if present.
    pub production_confirmation: Option<String>,
}

/// A fully loaded and validated configuration document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Absolute path the config was loaded from.
    pub path: PathBuf,
    /// `daemon.enabled` when present (informational in this read-only slice).
    pub daemon_enabled: Option<bool>,
    /// `daemon.socket` when present.
    pub daemon_socket: Option<String>,
    /// Loaded policy overlay, when the config names one.
    pub policy: Option<Policy>,
    /// Configured repositories in key order.
    pub repositories: Vec<Repository>,
    /// Configured harnesses in key order.
    pub harnesses: Vec<Harness>,
    /// Configured workflow pins in key order.
    pub workflows: Vec<WorkflowPin>,
}

impl Config {
    /// Configured repositories that may be acted on: enabled repositories,
    /// narrowed by the policy overlay's repository allowlist when one is
    /// present (the overlay is constrain-only).
    pub fn effective_repositories(&self) -> Vec<&Repository> {
        let allowlist = self
            .policy
            .as_ref()
            .and_then(|policy| policy.repositories.as_ref());
        self.repositories
            .iter()
            .filter(|repository| {
                repository.enabled
                    && allowlist.is_none_or(|allowed| allowed.contains(&repository.identity()))
            })
            .collect()
    }
}

/// Locate the config file: explicit path wins; otherwise the XDG default
/// when present, then the pre-rename default (see [`legacy_config_path`]).
/// Returns `Ok(None)` when no default config exists.
pub fn discover_config(explicit: Option<&Path>) -> Result<Option<PathBuf>, LoadError> {
    if let Some(path) = explicit {
        if path.is_file() {
            return Ok(Some(path.to_path_buf()));
        }
        return Err(LoadError {
            code: "config.not_found",
            message: format!("config file not found: {}", path.display()),
            refusal: None,
            path: path.display().to_string(),
        });
    }
    Ok(default_config_path()
        .filter(|path| path.is_file())
        .or_else(|| legacy_config_path().filter(|path| path.is_file())))
}

/// The config directory name under the XDG config home (product rename,
/// issue #106).
pub const CONFIG_DIR_NAME: &str = "canter";

/// Pre-rename config directory name. A config at the pre-rename path is
/// still discovered when no new-path config exists; it is read in place
/// (never copied, moved, or rewritten). Normative rule:
/// docs/contracts/compatibility.md, "Product rename (issue #106)".
pub const LEGACY_CONFIG_DIR_NAME: &str = "herdr-fleet";

/// The XDG default config path (`$XDG_CONFIG_HOME` or `$HOME/.config`,
/// then `canter/config.toml`) regardless of whether it exists.
pub fn default_config_path() -> Option<PathBuf> {
    config_path_in(CONFIG_DIR_NAME)
}

/// The pre-rename XDG config path (`.../herdr-fleet/config.toml`),
/// regardless of whether it exists. Only used as a fallback when the
/// new path holds no config.
pub fn legacy_config_path() -> Option<PathBuf> {
    config_path_in(LEGACY_CONFIG_DIR_NAME)
}

fn config_path_in(dir_name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let base = xdg.or_else(|| home.map(|home| home.join(".config")));
    base.map(|base| base.join(dir_name).join("config.toml"))
}

/// A human hint naming where the default config would live.
pub fn default_config_hint() -> String {
    default_config_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| format!("<XDG config home>/{CONFIG_DIR_NAME}/config.toml"))
}

/// Load, validate, and type a config document from a path.
pub fn load_config(path: &Path) -> Result<Config, LoadError> {
    let bytes = std::fs::read(path).map_err(|err| LoadError {
        code: "config.invalid",
        message: format!("cannot read config file: {err}"),
        refusal: None,
        path: path.display().to_string(),
    })?;
    let verdict = validate_bytes(Family::Config, &bytes);
    if !verdict.is_accepted() {
        return Err(LoadError::invalid(path, &verdict));
    }
    let doc = Val::parse_toml(std::str::from_utf8(&bytes).expect("validated UTF-8"))
        .expect("validated TOML");
    extract_config(path, &doc)
}

/// Load and validate the policy overlay named by a config document.
fn load_policy(config_dir: &Path, overlay: &str) -> Result<Policy, LoadError> {
    let raw = Path::new(overlay);
    let resolved = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        config_dir.join(raw)
    };
    let bytes = std::fs::read(&resolved).map_err(|err| LoadError {
        code: "policy.not_found",
        message: format!("cannot read policy overlay {}: {err}", resolved.display()),
        refusal: None,
        path: resolved.display().to_string(),
    })?;
    let verdict = validate_bytes(Family::Policy, &bytes);
    if !verdict.is_accepted() {
        return Err(LoadError {
            code: "policy.invalid",
            message: verdict.message().to_string(),
            refusal: verdict.refusal(),
            path: resolved.display().to_string(),
        });
    }
    let doc = Val::parse_toml(std::str::from_utf8(&bytes).expect("validated UTF-8"))
        .expect("validated TOML");
    Ok(Policy {
        path: resolved,
        repositories: doc.get("repositories").and_then(|value| match value {
            Val::Arr(items) => Some(
                items
                    .iter()
                    .filter_map(Val::as_str)
                    .map(str::to_string)
                    .collect(),
            ),
            _ => None,
        }),
        production_confirmation: doc
            .get("production_confirmation")
            .and_then(Val::as_str)
            .map(str::to_string),
    })
}

/// Walk a validated config document into the typed model. All shape and
/// format checks already passed [`validate_bytes`]; this step adds the
/// documented closed-surface type rules for `branch`/`enabled` and derives
/// the `owner/name` identity from each repository origin.
fn extract_config(path: &Path, doc: &Val) -> Result<Config, LoadError> {
    let fail = |message: String| LoadError {
        code: "config.invalid",
        message,
        refusal: Some(Refusal::Malformed),
        path: path.display().to_string(),
    };

    let mut repositories = Vec::new();
    if let Some(entries) = doc.get("repository") {
        let Val::Obj(map) = entries else {
            return Err(fail("config.repository must be a table".to_string()));
        };
        for (key, entry) in map {
            let Some(origin) = entry.get("origin").and_then(Val::as_str) else {
                return Err(fail(format!(
                    "config.repository.{key}.origin must be a string"
                )));
            };
            let Some((owner, name)) = identity_from_origin(origin) else {
                return Err(fail(format!(
                    "config.repository.{key}.origin {origin:?} does not end in an owner/name identity"
                )));
            };
            let branch = entry
                .get("branch")
                .and_then(Val::as_str)
                .map(str::to_string);
            let enabled = match entry.get("enabled") {
                None => true,
                Some(Val::Bool(value)) => *value,
                Some(_) => {
                    return Err(fail(format!(
                        "config.repository.{key}.enabled must be a boolean"
                    )));
                }
            };
            repositories.push(Repository {
                key: key.clone(),
                owner: owner.to_string(),
                name: name.to_string(),
                origin: origin.to_string(),
                branch,
                enabled,
            });
        }
    }

    let mut harnesses = Vec::new();
    if let Some(entries) = doc.get("harness") {
        let Val::Obj(map) = entries else {
            return Err(fail("config.harness must be a table".to_string()));
        };
        for (key, entry) in map {
            let executable = entry
                .get("executable")
                .and_then(Val::as_str)
                .expect("validated")
                .to_string();
            // C1 (issue #8): a bare-name parse guard. Config harness
            // executables are always bare names resolved through the
            // allowlisted PATH (the verified absolute identity is spawned);
            // an absolute path in config is refused so no config can bypass
            // the resolution witness or name an unverifiable executable.
            if executable.is_empty() || executable.contains('/') || executable.contains('\\') {
                return Err(fail(format!(
                    "config.harness.{key}.executable must be a bare executable name (no path separators); it is resolved through the allowlisted PATH"
                )));
            }
            // The optional provider/model binding pair (issue #80). Each
            // declared token must be a bare token and the pair must be
            // declared together: there is no default, no inferred half, and
            // an unbound profile refuses the prompt at the adapter boundary.
            let provider = entry
                .get("provider")
                .and_then(Val::as_str)
                .map(str::to_string);
            let model = entry.get("model").and_then(Val::as_str).map(str::to_string);
            for (field, value) in [("provider", &provider), ("model", &model)] {
                if let Some(value) = value
                    && !is_bare_token(value)
                {
                    return Err(fail(format!(
                        "config.harness.{key}.{field} must be a non-empty bare token (no whitespace or path separators)"
                    )));
                }
            }
            if provider.is_some() != model.is_some() {
                return Err(fail(format!(
                    "config.harness.{key}.provider and .model must be declared together"
                )));
            }
            harnesses.push(Harness {
                key: key.clone(),
                kind: entry
                    .get("kind")
                    .and_then(Val::as_str)
                    .expect("validated")
                    .to_string(),
                executable,
                env_allow: match entry.get("env_allow") {
                    Some(Val::Arr(items)) => items
                        .iter()
                        .filter_map(Val::as_str)
                        .map(str::to_string)
                        .collect(),
                    _ => Vec::new(),
                },
                provider,
                model,
            });
        }
    }

    let mut workflows = Vec::new();
    if let Some(entries) = doc.get("workflow") {
        let Val::Obj(map) = entries else {
            return Err(fail("config.workflow must be a table".to_string()));
        };
        for (key, entry) in map {
            workflows.push(WorkflowPin {
                key: key.clone(),
                id: entry
                    .get("id")
                    .and_then(Val::as_str)
                    .expect("validated")
                    .to_string(),
                hash: entry
                    .get("hash")
                    .and_then(Val::as_str)
                    .expect("validated")
                    .to_string(),
            });
        }
    }

    let daemon = doc.get("daemon");
    let daemon_enabled = daemon
        .and_then(|d| d.get("enabled"))
        .and_then(|value| match value {
            Val::Bool(b) => Some(*b),
            _ => None,
        });
    let daemon_socket = daemon
        .and_then(|d| d.get("socket"))
        .and_then(Val::as_str)
        .map(str::to_string);

    let policy = match doc
        .get("policy")
        .and_then(|p| p.get("overlay"))
        .and_then(Val::as_str)
    {
        Some(overlay) => {
            let config_dir = path.parent().unwrap_or_else(|| Path::new("."));
            Some(load_policy(config_dir, overlay)?)
        }
        None => None,
    };

    Ok(Config {
        path: path.to_path_buf(),
        daemon_enabled,
        daemon_socket,
        policy,
        repositories,
        harnesses,
        workflows,
    })
}

/// Derive the `owner/name` identity from a repository origin. Rules:
/// - a trailing `.git` is ignored;
/// - with a scheme (`://`), the URL path must be exactly two segments that
///   form a valid identity (no nested subgroup paths — use the canonical
///   repository URL);
/// - without a scheme, the whole origin must be exactly `owner/name`.
fn identity_from_origin(origin: &str) -> Option<(&str, &str)> {
    let trimmed = origin.strip_suffix(".git").unwrap_or(origin);
    let segments: Vec<&str> = trimmed.split('/').collect();
    let (owner, name) = if trimmed.contains("://") {
        // segments: [scheme, "", authority, owner, name]
        if segments.len() != 5 {
            return None;
        }
        (segments[3], segments[4])
    } else {
        // bare owner/name only
        if segments.len() != 2 {
            return None;
        }
        (segments[0], segments[1])
    };
    if owner.is_empty() || name.is_empty() || !is_repository_identity(&format!("{owner}/{name}")) {
        return None;
    }
    Some((owner, name))
}

/// An annotated synthetic `hf-config/v1` template for `config init` (stdout
/// guidance; the CLI never writes files itself).
pub fn init_template() -> String {
    r#"# canter canonical configuration (hf-config/v1).
# One canonical XDG file, no implicit profile/repository merge stack.
# Save this as $XDG_CONFIG_HOME/canter/config.toml (or
# ~/.config/canter/config.toml) and edit the synthetic values below.
# A pre-rename config at $XDG_CONFIG_HOME/herdr-fleet/config.toml is still
# read when no new-path config exists (docs/contracts/compatibility.md).

schema = "hf-config/v1"

[daemon]
enabled = false

# Optional explicit policy overlay; no implicit discovery.
# [policy]
# overlay = "policy.toml"

# One configured repository per key. The origin URL must end in
# /owner/name; the owner/name pair is the repository identity.
[repository.widgets]
origin = "https://github.com/example-org/widgets"
branch = "staging"
enabled = true

# Optional harness adapter profiles (kind, PATH-resolved executable, and
# the explicit environment allowlist for subprocesses). A harness whose
# prompt row carries a provider/model pair on argv (pi, jcode) also needs
# this explicit binding pair; without it the prompt refuses (no default is
# inferred).
# [harness.cli]
# kind = "argv"
# executable = "hf-cli-example"
# env_allow = ["PATH", "HOME"]
# provider = "example-provider"
# model = "example-model"

# Optional workflow pin: id + 64-hex hash of the canonical workflow
# document. Without a pin, `plan` uses the built-in doctrine workflow.
# [workflow.bundle]
# id = "fleet-doctrine-1"
# hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#
    .to_string()
}

/// Validate a repository key lookup argument: it may name a configured key
/// (slug) or the repository's `owner/name` identity directly.
pub fn resolve_repository<'a>(
    config: &'a Config,
    argument: &str,
) -> Result<&'a Repository, String> {
    if is_slug(argument)
        && let Some(repository) = config.repositories.iter().find(|r| r.key == argument)
    {
        return Ok(repository);
    }
    config
        .repositories
        .iter()
        .find(|r| r.identity() == argument)
        .ok_or_else(|| {
            format!(
                "no configured repository matches {argument:?}; configure it first (see `canter config init`)"
            )
        })
}

/// Deterministic order helper: sort repository keys for display.
pub fn sorted_repositories(config: &Config) -> Vec<&Repository> {
    let mut repos: Vec<&Repository> = config.repositories.iter().collect();
    repos.sort_by(|a, b| a.key.cmp(&b.key));
    repos
}

/// Allowed subprocess environment for the built-in adapters. The trust model
/// (docs/contracts/trust-model.md T5) makes this allowlist the only
/// environment channel to child processes: no host variable beyond these
/// names is ever inherited.
pub const ADAPTER_ENV_ALLOW: [&str; 6] = [
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "XDG_CONFIG_HOME",
    "TMPDIR",
];

/// Build a `BTreeMap` of the adapter environment from the invoking process.
pub fn adapter_environment() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for name in ADAPTER_ENV_ALLOW {
        if let Some(value) = std::env::var_os(name)
            && let Ok(value) = value.into_string()
        {
            env.insert(name.to_string(), value);
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hf-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(name);
        std::fs::write(&path, content).expect("write");
        path
    }

    const VALID: &str = r#"schema = "hf-config/v1"
[daemon]
enabled = true
[policy]
overlay = "policy.overlay.toml"
[repository.widgets]
origin = "https://github.com/example-org/widgets"
branch = "staging"
enabled = true
[harness.cli]
kind = "argv"
executable = "hf-cli-example"
env_allow = ["PATH", "HOME"]
[workflow.bundle]
id = "fleet-doctrine-1"
hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#;

    const POLICY_VALID: &str = r#"schema = "hf-policy/v1"
repositories = ["example-org/widgets"]
production_confirmation = "tty"
"#;

    #[test]
    fn loads_and_types_valid_config_with_overlay() {
        let dir = std::env::temp_dir().join(format!("hf-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("policy.overlay.toml"), POLICY_VALID).expect("write overlay");
        let path = write_temp("config.toml", VALID);
        let config = load_config(&path).expect("load");
        assert_eq!(config.daemon_enabled, Some(true));
        assert_eq!(config.repositories.len(), 1);
        let repo = &config.repositories[0];
        assert_eq!(repo.key, "widgets");
        assert_eq!(repo.identity(), "example-org/widgets");
        assert_eq!(repo.origin, "https://github.com/example-org/widgets");
        assert_eq!(repo.branch.as_deref(), Some("staging"));
        assert!(repo.enabled);
        let policy = config.policy.as_ref().expect("overlay loaded");
        assert_eq!(
            policy.repositories.as_ref().map(|r| r.len()),
            Some(1),
            "allowlist present"
        );
        assert_eq!(policy.production_confirmation.as_deref(), Some("tty"));
        let effective = config.effective_repositories();
        assert_eq!(effective.len(), 1);
    }

    #[test]
    fn harness_provider_model_binding_is_decoded_and_validated() {
        let bound = load_config(&write_temp(
            "harness-binding.toml",
            r#"schema = "hf-config/v1"
[harness.pi]
kind = "pi"
executable = "pi"
env_allow = ["PATH"]
provider = "example-provider"
model = "example-model"
"#,
        ))
        .expect("binding decodes");
        let harness = &bound.harnesses[0];
        assert_eq!(harness.provider.as_deref(), Some("example-provider"));
        assert_eq!(harness.model.as_deref(), Some("example-model"));

        // A declared binding must be a bare token pair: no path separators,
        // no blank/whitespace values, and never a half pair.
        for (name, provider_line, model_line) in [
            (
                "path",
                "provider = \"example/provider\"",
                "model = \"example-model\"",
            ),
            ("blank", "provider = \"example-provider\"", "model = \"  \""),
            ("half", "provider = \"example-provider\"", ""),
        ] {
            let content = format!(
                "schema = \"hf-config/v1\"\n[harness.pi]\nkind = \"pi\"\nexecutable = \"pi\"\nenv_allow = [\"PATH\"]\n{provider_line}\n{model_line}\n"
            );
            let err = load_config(&write_temp(
                &format!("harness-binding-{name}.toml"),
                &content,
            ))
            .expect_err("invalid binding refused");
            assert_eq!(err.code, "config.invalid", "{name}");
            assert!(
                err.message.contains("provider") || err.message.contains("model"),
                "{name}: {}",
                err.message
            );
        }
    }

    #[test]
    fn overlay_narrows_effective_repositories() {
        let dir = std::env::temp_dir().join(format!("hf-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let config_text = r#"schema = "hf-config/v1"
[policy]
overlay = "policy.narrow.toml"
[repository.alpha]
origin = "https://github.com/example-org/alpha"
[repository.beta]
origin = "https://github.com/example-org/beta"
"#;
        std::fs::write(
            dir.join("policy.narrow.toml"),
            r#"schema = "hf-policy/v1"
repositories = ["example-org/alpha"]
"#,
        )
        .expect("write");
        let path = dir.join("narrow-config.toml");
        std::fs::write(&path, config_text).expect("write");
        let config = load_config(&path).expect("load");
        assert_eq!(config.repositories.len(), 2);
        let effective: Vec<&str> = config
            .effective_repositories()
            .iter()
            .map(|r| r.key.as_str())
            .collect();
        assert_eq!(effective, vec!["alpha"]);
    }

    #[test]
    fn origin_identity_derivation_rules() {
        assert_eq!(
            identity_from_origin("https://github.com/example-org/widgets"),
            Some(("example-org", "widgets"))
        );
        assert_eq!(
            identity_from_origin("https://github.com/example-org/widgets.git"),
            Some(("example-org", "widgets"))
        );
        assert_eq!(
            identity_from_origin("example-org/widgets"),
            Some(("example-org", "widgets"))
        );
        assert_eq!(identity_from_origin("https://github.com/widgets"), None);
        assert_eq!(
            identity_from_origin("git@github.com:example-org/widgets"),
            None
        );
        assert_eq!(identity_from_origin("not a url"), None);
    }

    #[test]
    fn explicit_missing_config_is_not_found() {
        let missing = std::env::temp_dir().join(format!(
            "hf-config-test-{}-definitely-missing.toml",
            std::process::id()
        ));
        let err = discover_config(Some(&missing)).expect_err("must fail");
        assert_eq!(err.code, "config.not_found");
    }

    #[test]
    fn resolve_repository_by_key_and_identity() {
        let dir = std::env::temp_dir().join(format!("hf-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        // Self-contained: no policy overlay reference, so this test never
        // depends on an overlay another test wrote into the shared
        // per-process temp dir (order-dependent coupling).
        let path = write_temp(
            "resolve.toml",
            r#"schema = "hf-config/v1"
[repository.widgets]
origin = "https://github.com/example-org/widgets"
branch = "staging"
enabled = true
"#,
        );
        let config = load_config(&path).expect("load");
        assert_eq!(
            resolve_repository(&config, "widgets")
                .expect("key")
                .identity(),
            "example-org/widgets"
        );
        assert_eq!(
            resolve_repository(&config, "example-org/widgets")
                .expect("identity")
                .key,
            "widgets"
        );
        assert!(resolve_repository(&config, "example-org/other").is_err());
        assert!(resolve_repository(&config, "other").is_err());
    }

    #[test]
    fn template_is_a_valid_config_document() {
        let template = init_template();
        let verdict = validate_bytes(Family::Config, template.as_bytes());
        assert!(
            verdict.is_accepted(),
            "init template must validate: {}",
            verdict.message()
        );
    }

    #[test]
    fn scp_like_origins_are_refused_at_extraction() {
        let dir = std::env::temp_dir().join(format!("hf-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = write_temp(
            "scp.toml",
            r#"schema = "hf-config/v1"
[repository.widgets]
origin = "git@github.com:example-org/widgets"
"#,
        );
        let err = load_config(&path).expect_err("scp origin must not yield an identity");
        assert_eq!(err.code, "config.invalid");
        assert!(err.message.contains("origin"));
    }
}
