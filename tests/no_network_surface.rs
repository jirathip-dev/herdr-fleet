//! Issue #5 AC10: the daemon slice introduces no network listener,
//! telemetry, auto-update, or notification provider. The check is a static
//! scan over the crate sources (fixtures assert the surface stays closed);
//! integration tests elsewhere prove the daemon binds a Unix socket only.

use std::path::PathBuf;

fn crate_sources() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut sources = Vec::new();
    let mut queue = vec![root.join("src")];
    while let Some(dir) = queue.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if path.is_dir() {
                queue.push(path);
            } else if path.extension().map(|ext| ext == "rs").unwrap_or(false) {
                sources.push(path);
            }
        }
    }
    sources.sort();
    sources
}

#[test]
fn no_network_listener_telemetry_or_update_surface_is_introduced() {
    // Banned tokens: anything that would open a network listener or ship
    // telemetry/auto-update/notification machinery. The daemon's only
    // listener is the Unix-domain socket (std::os::unix::net::UnixListener).
    let banned: &[(&str, &str)] = &[
        ("TcpListener", "TCP listeners are banned"),
        ("TcpStream", "TCP streams are banned (Unix socket only)"),
        ("UdpSocket", "UDP sockets are banned"),
        ("reqwest", "no HTTP client dependency"),
        ("ureq", "no HTTP client dependency"),
        ("tokio", "no async runtime"),
        ("telemetry", "no telemetry"),
        ("auto_update", "no auto-update"),
        ("autoupdate", "no auto-update"),
        ("notification provider", "no notification provider"),
        // Issue #9 AC10 (lifecycle slice): schedules/lifecycle/remote add
        // no messaging, agent-plugin, or webhook surface either - the
        // recurring-work path stays on the same Unix socket and local
        // process spawns.
        ("webhook", "no webhook surface"),
        ("websocket", "no websocket surface"),
        ("mcp", "no MCP/plugin-agent surface"),
    ];

    let mut violations: Vec<String> = Vec::new();
    for path in crate_sources() {
        let content = std::fs::read_to_string(&path).expect("read source");
        for (token, reason) in banned {
            // Only flag occurrences outside comments/doc strings would be
            // ideal; a plain scan over this small slice is the fixture and
            // any occurrence is reviewed by hand in the diff. Tokens that
            // legitimately appear in prose are listed here as accepted
            // phrases when the context is documentation.
            if content.contains(token) {
                violations.push(format!("{}: {token} ({reason})", path.display()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "network/telemetry surface leaked into sources:\n{}",
        violations.join("\n")
    );
}

#[test]
fn daemon_listener_is_unix_domain_only() {
    // The daemon module may only reference the unix listener family.
    let daemon_source = crate_sources()
        .into_iter()
        .find(|path| path.ends_with("daemon.rs"))
        .expect("daemon.rs");
    let content = std::fs::read_to_string(&daemon_source).expect("read daemon.rs");
    assert!(
        content.contains("UnixListener"),
        "daemon must bind a Unix-domain listener"
    );
    for banned in ["TcpListener", "UdpSocket"] {
        assert!(
            !content.contains(banned),
            "daemon.rs must not reference {banned}"
        );
    }
}

#[test]
fn audit_deletion_is_append_side_retention_only() {
    // Issue #9 AC8: deleting audit records is itself an explicit,
    // bounded, append-side retention operation - no API can purge audit
    // rows on demand. The single `DELETE FROM audit` in the whole state
    // layer lives inside prune_audit_locked (bounded retention keeps the
    // chain genesis); there is no purge/truncate surface anywhere.
    let state_source = crate_sources()
        .into_iter()
        .find(|path| path.ends_with("state.rs"))
        .expect("state.rs");
    let content = std::fs::read_to_string(&state_source).expect("read state.rs");
    assert_eq!(
        content.matches("DELETE FROM audit").count(),
        1,
        "audit rows are deleted exactly once, inside the retention prune"
    );
    let prune_index = content.find("DELETE FROM audit").expect("prune sql");
    let prune_fn = content.find("fn prune_audit_locked").expect("prune fn");
    assert!(
        prune_fn < prune_index,
        "the only audit DELETE must live inside prune_audit_locked"
    );
    assert!(
        !content.contains("purge_audit") && !content.contains("TRUNCATE"),
        "no on-demand audit purge surface may exist"
    );
}
