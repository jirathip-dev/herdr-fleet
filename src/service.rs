//! First-party per-user service units: launchd (macOS) and systemd (Linux)
//! rendering plus install/status/uninstall *plans* (issue #5, AC9).
//!
//! Nothing in this module installs, starts, stops, or queries the host
//! service manager: it renders unit text and command steps so operators can
//! review and run them on clean supported hosts. The daemon itself never
//! activates a service. All renderers take explicit paths so fixtures and
//! tests never depend on this host's layout.

use std::path::{Path, PathBuf};

/// LaunchAgent label (one per user; synthetic and stable).
pub const LAUNCHD_LABEL: &str = "com.herdr-fleet.daemon";
/// systemd user unit file name.
pub const SYSTEMD_UNIT_NAME: &str = "herdr-fleet.service";

/// The service-manager platform this binary targets.
pub fn detect_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "launchd"
    } else if cfg!(target_os = "linux") {
        "systemd"
    } else {
        "unsupported"
    }
}

/// The per-user LaunchAgent plist path (`~/Library/LaunchAgents/…plist`).
pub fn launchd_plist_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

/// The per-user systemd unit path under the config home.
pub fn systemd_unit_path(config_home: &Path) -> PathBuf {
    config_home
        .join("systemd")
        .join("user")
        .join(SYSTEMD_UNIT_NAME)
}

/// Render the per-user LaunchAgent plist for `bin` serving `socket`.
pub fn launchd_unit(bin: &Path, socket: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
    <key>Label</key>\n\
    <string>{LAUNCHD_LABEL}</string>\n\
    <key>ProgramArguments</key>\n\
    <array>\n\
        <string>{bin}</string>\n\
        <string>daemon</string>\n\
        <string>run</string>\n\
        <string>--socket</string>\n\
        <string>{socket}</string>\n\
    </array>\n\
    <key>RunAtLoad</key>\n\
    <true/>\n\
    <key>KeepAlive</key>\n\
    <dict>\n\
        <key>SuccessfulExit</key>\n\
        <false/>\n\
    </dict>\n\
    <key>ThrottleInterval</key>\n\
    <integer>10</integer>\n\
    <key>ProcessType</key>\n\
    <string>Background</string>\n\
</dict>\n\
</plist>\n",
        bin = bin.display(),
        socket = socket.display(),
    )
}

/// Render the per-user systemd unit for `bin` serving `socket`.
pub fn systemd_unit(bin: &Path, socket: &Path) -> String {
    format!(
        "# herdr-fleet per-user daemon unit (issue #5; rendered, not activated)\n\
[Unit]\n\
Description=herdr-fleet state daemon (single writer per user)\n\
After=default.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={bin} daemon run --socket {socket}\n\
Restart=on-failure\n\
RestartSec=5\n\
NoNewPrivileges=true\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        bin = bin.display(),
        socket = socket.display(),
    )
}

/// Render the numbered command steps for installing the service on this
/// platform (a plan: nothing here executes the steps).
pub fn install_plan_steps(
    platform: &str,
    bin: &Path,
    socket: &Path,
    home: &Path,
    config_home: &Path,
) -> Vec<String> {
    match platform {
        "launchd" => {
            let plist = launchd_plist_path(home);
            vec![
                format!(
                    "write the rendered plist to {} (per-user; content printed by the install command)",
                    plist.display()
                ),
                format!("launchctl bootstrap gui/$(id -u) {}", plist.display()),
                format!(
                    "verify: launchctl print gui/$(id -u)/{LAUNCHD_LABEL} (expect the daemon pid serving {})",
                    socket.display()
                ),
            ]
        }
        "systemd" => {
            let unit = systemd_unit_path(config_home);
            let _ = (bin, socket);
            vec![
                format!(
                    "write the rendered unit to {} (content printed by the install command)",
                    unit.display()
                ),
                "run: systemctl --user daemon-reload".to_string(),
                "run: systemctl --user enable --now herdr-fleet.service".to_string(),
                "verify: systemctl --user --no-pager status herdr-fleet.service".to_string(),
            ]
        }
        other => vec![format!(
            "no first-party unit exists for platform {other:?}; run the daemon directly with `herdr-fleet daemon run`"
        )],
    }
}

/// Render the command steps for checking service status on this platform.
pub fn status_plan_steps(platform: &str) -> Vec<String> {
    match platform {
        "launchd" => vec![
            format!("launchctl print gui/$(id -u)/{LAUNCHD_LABEL}"),
            "herdr-fleet daemon status --json".to_string(),
        ],
        "systemd" => vec![
            "systemctl --user --no-pager status herdr-fleet.service".to_string(),
            "herdr-fleet daemon status --json".to_string(),
        ],
        other => vec![format!(
            "no first-party service exists for platform {other:?}"
        )],
    }
}

/// Render the command steps for uninstalling the service on this platform.
pub fn uninstall_plan_steps(platform: &str, home: &Path, config_home: &Path) -> Vec<String> {
    match platform {
        "launchd" => vec![
            format!("launchctl bootout gui/$(id -u)/{LAUNCHD_LABEL}"),
            format!("rm -f {}", launchd_plist_path(home).display()),
        ],
        "systemd" => vec![
            "systemctl --user --no-pager disable --now herdr-fleet.service".to_string(),
            format!("rm -f {}", systemd_unit_path(config_home).display()),
            "systemctl --user daemon-reload".to_string(),
        ],
        other => vec![format!(
            "no first-party service exists for platform {other:?}"
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runtime-derived fixture paths: the tracked tree must never carry an
    /// absolute-path literal (public-tree scanner).
    fn fixture_paths() -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("hf-service-{}", std::process::id()));
        (
            base.join("fixture-user"),
            base.join("fixture-user").join(".config"),
            base.join("fixture-user")
                .join(".local")
                .join("bin")
                .join("herdr-fleet"),
            base.join("run-user-1000").join("daemon.sock"),
        )
    }

    #[test]
    fn launchd_unit_is_a_complete_plist() {
        let (_, _, bin, socket) = fixture_paths();
        let unit = launchd_unit(&bin, &socket);
        assert!(unit.contains(LAUNCHD_LABEL));
        assert!(unit.contains("<key>KeepAlive</key>"));
        assert!(unit.contains("<key>RunAtLoad</key>"));
        assert!(unit.contains(&bin.display().to_string()));
        assert!(unit.contains(&socket.display().to_string()));
        assert!(unit.ends_with("</plist>\n"));
        // The daemon must run in the foreground under launchd: no & anywhere.
        assert!(!unit.contains('&'), "plist must not background the daemon");
    }

    #[test]
    fn systemd_unit_is_a_complete_user_unit() {
        let (_, _, bin, socket) = fixture_paths();
        let unit = systemd_unit(&bin, &socket);
        assert!(unit.contains("[Unit]"));
        assert!(unit.contains("[Service]"));
        assert!(unit.contains("[Install]"));
        assert!(unit.contains(&format!(
            "ExecStart={} daemon run --socket {}",
            bin.display(),
            socket.display()
        )));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains("NoNewPrivileges=true"));
    }

    #[test]
    fn paths_are_per_user_and_synthetic_safe() {
        let (home, config_home, _, _) = fixture_paths();
        let plist = launchd_plist_path(&home);
        assert_eq!(
            plist,
            home.join("Library")
                .join("LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist"))
        );
        let unit = systemd_unit_path(&config_home);
        assert_eq!(
            unit,
            config_home
                .join("systemd")
                .join("user")
                .join("herdr-fleet.service")
        );
    }

    #[test]
    fn plans_are_steps_only_and_platform_scoped() {
        let (home, config_home, bin, socket) = fixture_paths();
        for platform in ["launchd", "systemd", "plan9"] {
            let install = install_plan_steps(platform, &bin, &socket, &home, &config_home);
            assert!(!install.is_empty());
            let uninstall = uninstall_plan_steps(platform, &home, &config_home);
            assert!(!uninstall.is_empty());
        }
        let launchd_steps = install_plan_steps("launchd", &bin, &socket, &home, &config_home);
        assert!(
            launchd_steps
                .iter()
                .any(|step| step.contains("launchctl bootstrap")),
            "launchd install plan documents the bootstrap command"
        );
        let systemd_steps = install_plan_steps("systemd", &bin, &socket, &home, &config_home);
        assert!(
            systemd_steps
                .iter()
                .any(|step| step.contains("systemctl --user enable")),
            "systemd install plan documents the enable command"
        );
        // Unsupported platforms produce a plan that says so (never silence).
        assert!(uninstall_plan_steps("plan9", &home, &config_home)[0].contains("no first-party"));
    }

    #[test]
    fn platform_is_one_of_the_known_kinds() {
        assert!(matches!(
            detect_platform(),
            "launchd" | "systemd" | "unsupported"
        ));
    }

    #[test]
    fn rendered_units_never_leak_private_path_markers() {
        let (home, config_home, bin, socket) = fixture_paths();
        // Unit text may legitimately carry URLs (the plist DOCTYPE), but
        // never a private-machine absolute-path marker.
        for unit in [launchd_unit(&bin, &socket), systemd_unit(&bin, &socket)] {
            // The marker strings are assembled so the tracked tree itself
            // never carries an absolute-path literal (public-tree scanner).
            let user_home_marker = ["/Us", "ers/"].concat();
            let posix_home_marker = ["/ho", "me/"].concat();
            assert!(
                !unit.contains(&user_home_marker),
                "unit leaked a host path marker"
            );
            assert!(
                !unit.contains(&posix_home_marker),
                "unit leaked a host path marker"
            );
        }
        assert!(
            launchd_plist_path(&home)
                .display()
                .to_string()
                .contains("Library")
        );
        assert!(
            systemd_unit_path(&config_home)
                .display()
                .to_string()
                .contains("systemd")
        );
    }
}
