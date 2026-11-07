use std::{fs, path::Path};

fn config_file(name: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(name))
        .unwrap_or_else(|error| panic!("cannot read {name}: {error}"))
}

#[test]
fn supervised_proxy_has_readiness_logging_and_restart_policy() {
    let config = config_file("process-compose.yaml");
    for required in [
        "command: ./target/release/copilot-proxy-rs",
        "COPILOT_PROXY_RS_PORT=${COPILOT_PROXY_RS_PORT:-8080}",
        "RUST_LOG=${RUST_LOG:-info}",
        "readiness_probe:",
        "http_get:",
        "host: 127.0.0.1",
        "path: /health",
        "port: ${COPILOT_PROXY_RS_PORT:-8080}",
        "restart: on_failure",
        "log_location: .process-compose/",
    ] {
        assert!(config.contains(required), "missing {required}");
    }
}

#[test]
fn mise_tasks_use_an_isolated_unix_socket() {
    let config = config_file("mise.toml");
    assert!(config.contains("[tools]\nprocess-compose = \"1.124.0\""));
    assert!(config.contains("COPILOT_PROXY_RS_PC_SOCKET"));
    assert!(config.contains("/.process-compose/pc.sock"));
    assert!(!config.contains("$PC_SOCKET_PATH"));
    assert!(!config.contains("env.PC_SOCKET_PATH"));

    for (task, operation) in [
        ("up", "up -D"),
        ("down", "down"),
        ("restart", "process restart proxy"),
        ("status", "process list --output wide"),
        ("logs", "process logs proxy"),
    ] {
        let header = format!("[tasks.\"proxy:{task}\"]");
        let block = config
            .split_once(&header)
            .unwrap_or_else(|| panic!("missing {header}"))
            .1
            .split("[tasks.")
            .next()
            .unwrap();
        let clear_env = "for name in $(compgen -v PC_); do unset \"$name\"; done";
        assert!(block.contains(clear_env), "{task} inherits PC_* variables");
        assert!(block.contains(operation), "{task} missing {operation}");
        for line in block
            .lines()
            .filter(|line| line.starts_with("process-compose "))
        {
            assert!(
                line.contains("-U -u \"$COPILOT_PROXY_RS_PC_SOCKET\""),
                "{task} must explicitly use the project unix socket"
            );
        }
    }
    assert!(config.contains("cargo build --release --locked"));
    assert!(config.contains("--disable-dotenv"));
    assert!(config.contains("project is-ready --wait"));
}
