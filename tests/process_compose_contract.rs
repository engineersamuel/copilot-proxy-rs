use std::fs;
use std::path::Path;

const TASKS: [&str; 5] = [
    "proxy:up",
    "proxy:down",
    "proxy:restart",
    "proxy:status",
    "proxy:logs",
];

fn read(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    fs::read_to_string(&full).unwrap_or_else(|err| panic!("read {}: {err}", full.display()))
}

#[test]
fn mise_pins_process_compose_and_defines_proxy_tasks() {
    let mise = read("mise.toml");
    assert!(mise.contains("[tools]"), "mise.toml needs a [tools] table");
    assert!(
        mise.contains("process-compose = \"1.124.0\""),
        "process-compose must be pinned to an exact version"
    );
    for task in TASKS {
        assert!(
            mise.contains(&format!("[tasks.\"{task}\"]")),
            "mise.toml is missing task {task}"
        );
    }
}

#[test]
fn process_compose_config_probes_health_on_configured_port() {
    let config = read("process-compose.yaml");
    assert!(
        config.contains("copilot-proxy-rs:"),
        "missing proxy process"
    );
    assert!(
        config.contains("readiness_probe:"),
        "missing readiness probe"
    );
    assert!(config.contains("http_get:"), "probe must use http_get");
    assert!(config.contains("path: /health"), "probe must hit /health");
    assert!(
        config.contains("port: ${COPILOT_PROXY_RS_PORT:-8080}"),
        "probe port must follow COPILOT_PROXY_RS_PORT with default 8080"
    );
    assert!(
        config.contains("restart: on_failure"),
        "missing restart policy"
    );
}

#[test]
fn process_compose_control_uses_only_a_unix_socket() {
    let script = read("scripts/process-compose.sh");
    assert!(
        script.contains("COPILOT_PROXY_RS_PC_SOCKET"),
        "socket env var"
    );
    assert!(
        script.contains("unset PC_SOCKET_PATH"),
        "must drop inherited PC_SOCKET_PATH"
    );
    assert!(
        script.contains("-U -u"),
        "client and server must use the unix socket"
    );
    for forbidden in [" -p ", "--port", "PC_PORT_NUM"] {
        assert!(
            !script.contains(forbidden),
            "process-compose must not use a TCP port ({forbidden})"
        );
    }
}

#[test]
fn bootstrap_installs_mise_only_when_missing() {
    let script = read("scripts/bootstrap.sh");
    assert!(script.contains("set -euo pipefail"));
    assert!(script.contains("command -v mise"));
    assert!(script.contains("https://mise.run"));
    assert!(script.contains("mise install"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/bootstrap.sh");
        let mode = fs::metadata(path).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "bootstrap.sh must be executable");
    }
}

#[test]
fn readme_documents_bootstrap_and_tasks() {
    let readme = read("README.md");
    assert!(readme.contains("scripts/bootstrap.sh"));
    assert!(readme.contains("COPILOT_PROXY_RS_PC_SOCKET"));
    for task in TASKS {
        assert!(
            readme.contains(&format!("mise run {task}")),
            "README misses {task}"
        );
    }
}
