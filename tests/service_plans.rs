//! Issue #5 AC9: service doctor/install/status/uninstall PLANS are pure
//! fixtures — no launchd/systemd activation happens on the host. The
//! rendered unit and the step commands are asserted here; the same plans
//! are what a clean-host fixture runner executes (see .report-5.md for the
//! documented fixture-host commands).
//!
//! No test in this file loads, starts, or touches a real service.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_herdr-fleet")
}

/// A fixture home/config layout: nothing outside this dir is read or
/// written by the commands under test.
struct ServiceFixture {
    dir: PathBuf,
}

impl ServiceFixture {
    fn new(name: &str) -> ServiceFixture {
        let dir = std::env::temp_dir().join(format!("hf-svc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).expect("fixture config dir");
        std::fs::create_dir_all(dir.join("state")).expect("fixture state dir");
        std::fs::create_dir_all(dir.join("runtime")).expect("fixture runtime dir");
        ServiceFixture { dir }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(binary());
        command
            .env("HOME", &self.dir)
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("XDG_RUNTIME_DIR", self.dir.join("runtime"))
            .env_remove("HERDR_FLEET_CRASH_POINT");
        command
    }

    /// Run a command, returning (exit_code, stdout, stderr).
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let mut child = self
            .command()
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn herdr-fleet");
        let mut stdout = String::new();
        let mut stderr = String::new();
        use std::io::Read;
        child
            .stdout
            .take()
            .expect("stdout")
            .read_to_string(&mut stdout)
            .expect("read stdout");
        child
            .stderr
            .take()
            .expect("stderr")
            .read_to_string(&mut stderr)
            .expect("read stderr");
        let status = child.wait().expect("wait");
        (status.code().unwrap_or(-1), stdout, stderr)
    }
}

impl Drop for ServiceFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Write a minimal config under the fixture XDG dirs.
fn write_config(fixture: &ServiceFixture, socket: &Path) {
    let config_dir = fixture.dir.join("config").join("herdr-fleet");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let mut file = std::fs::File::create(config_dir.join("config.toml")).expect("config file");
    write!(
        file,
        "schema = \"hf-config/v1\"\n[daemon]\nsocket = \"{}\"\n",
        socket.display()
    )
    .expect("write config");
}

#[test]
fn service_doctor_reports_platform_and_read_only_rows() {
    let fixture = ServiceFixture::new("doctor");
    let (exit, stdout, _stderr) = fixture.run(&["service", "doctor", "--json"]);
    assert_eq!(exit, 0, "doctor exits 0 on a clean fixture: {stdout}");
    assert!(stdout.contains("\"kind\":\"ok\""), "{stdout}");
    // Platform is one of the supported unit families; no activation happened.
    let platform = if cfg!(target_os = "macos") {
        "launchd"
    } else if cfg!(target_os = "linux") {
        "systemd"
    } else {
        "unsupported"
    };
    assert!(
        stdout.contains(&format!("\"platform\":\"{platform}\"")),
        "{stdout}"
    );
    assert!(stdout.contains("doctor"), "{stdout}");
}

#[test]
fn service_install_plan_renders_a_fixture_unit_with_steps() {
    let fixture = ServiceFixture::new("install-plan");
    let socket = fixture.dir.join("runtime").join("daemon.sock");
    write_config(&fixture, &socket);

    let (exit, stdout, _stderr) = fixture.run(&["service", "install-plan", "--json"]);
    assert_eq!(exit, 0, "install-plan exits 0: {stdout}");

    let platform = if cfg!(target_os = "macos") {
        "launchd"
    } else if cfg!(target_os = "linux") {
        "systemd"
    } else {
        "unsupported"
    };
    assert!(
        stdout.contains(&format!("\"platform\":\"{platform}\"")),
        "{stdout}"
    );
    // The unit text and the concrete install steps are part of the plan.
    let unit = if cfg!(target_os = "macos") {
        "<key>Label</key>"
    } else {
        "ExecStart="
    };
    assert!(stdout.contains(unit), "unit content missing: {stdout}");
    assert!(stdout.contains("\"steps\""), "{stdout}");
    if cfg!(target_os = "macos") {
        assert!(stdout.contains("<key>ProgramArguments</key>"), "{stdout}");
    } else {
        assert!(stdout.contains("herdr-fleet daemon run"), "{stdout}");
    }
    // The plan *documents* the fixture-host activation command (install
    // plans are executed on clean supported hosts, never on this one); the
    // unit and every step target the fixture socket path.
    let activation = if cfg!(target_os = "macos") {
        "launchctl bootstrap"
    } else {
        "systemctl --user enable"
    };
    assert!(stdout.contains(activation), "{stdout}");
    assert!(
        stdout.contains("daemon.sock"),
        "plan targets the fixture socket: {stdout}"
    );
}

#[test]
fn service_status_and_uninstall_plans_are_consistent() {
    let fixture = ServiceFixture::new("status-plan");
    let socket = fixture.dir.join("runtime").join("daemon.sock");
    write_config(&fixture, &socket);

    let (exit_status, stdout_status, _) = fixture.run(&["service", "status-plan", "--json"]);
    assert_eq!(exit_status, 0, "{stdout_status}");
    assert!(stdout_status.contains("\"steps\""), "{stdout_status}");

    let (exit_uninstall, stdout_uninstall, _) =
        fixture.run(&["service", "uninstall-plan", "--json"]);
    assert_eq!(exit_uninstall, 0, "{stdout_uninstall}");
    assert!(stdout_uninstall.contains("\"steps\""), "{stdout_uninstall}");

    // Both plans reference the same unit target so a fixture runner can
    // install and uninstall symmetrically.
    let target_marker = if cfg!(target_os = "macos") {
        "Library/LaunchAgents"
    } else {
        "systemd/user"
    };
    assert!(stdout_status.contains(target_marker), "{stdout_status}");
    assert!(
        stdout_uninstall.contains(target_marker),
        "{stdout_uninstall}"
    );
}
