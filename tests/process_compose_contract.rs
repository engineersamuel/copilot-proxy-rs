use std::{fs, path::Path, process::Command};

fn repo_file(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap()
}

#[test]
fn mise_pins_process_compose_and_exposes_lifecycle_tasks() {
    let mise = repo_file("mise.toml");
    let tools = mise
        .split("[tools]")
        .nth(1)
        .unwrap()
        .split('[')
        .next()
        .unwrap();
    assert!(tools.contains("process-compose = \"1.124.0\""));
    assert!(!tools.contains("rust"));
    for task in ["up", "down", "restart", "status", "logs"] {
        assert!(mise.contains(&format!("[tasks.\"proxy:{task}\"]")));
    }
    assert!(mise.contains("-f process-compose.yaml"));
    assert!(mise.contains("cargo build --release --locked"));
}

#[test]
fn process_compose_tasks_use_only_unix_socket_commands() {
    let mise = repo_file("mise.toml");
    assert!(mise.contains("PC_SOCKET_PATH = \"{{config_root}}/.process-compose/pc.sock\""));
    assert!(!mise.contains("PC_NO_SERVER"));
    let wait = repo_file("scripts/proxy-wait-ready.sh");
    let tasks = &mise[mise.find("[tasks.").unwrap()..];
    let commands = format!("{tasks}\n{wait}");
    let invocations: Vec<_> = commands
        .lines()
        .filter(|line| !line.starts_with("description ="))
        .filter_map(|line| line.split_once("process-compose ").map(|(_, args)| args))
        .collect();
    assert!(invocations.len() >= 6);
    for invocation in invocations {
        let args: Vec<_> = invocation
            .split_whitespace()
            .map(|arg| arg.trim_matches('"'))
            .collect();
        assert!(args.contains(&"-U"), "missing -U: {invocation}");
        assert!(!args.contains(&"-p"), "TCP port flag: {invocation}");
        assert!(
            !invocation.contains("--port"),
            "TCP port flag: {invocation}"
        );
        assert!(!invocation.contains("--no-server"));
    }
}

#[test]
fn process_compose_probes_the_configured_proxy_port() {
    let config = repo_file("process-compose.yaml");
    assert!(config.contains("version: \"0.5\""));
    assert!(config.contains("copilot-proxy-rs:"));
    assert!(config.contains("target/release/copilot-proxy-rs"));
    assert!(config.contains("COPILOT_PROXY_RS_PORT=${COPILOT_PROXY_RS_PORT:-8080}"));
    let probe = config.split("readiness_probe:").nth(1).unwrap();
    assert!(probe.contains("http_get:"));
    assert!(probe.contains("host: 127.0.0.1"));
    assert!(probe.contains("port: \"${COPILOT_PROXY_RS_PORT:-8080}\""));
    assert!(probe.contains("path: /health"));
    assert!(config.contains("restart: on_failure"));
    assert!(config.contains("signal: 15"));
    assert!(repo_file(".gitignore").contains("/.process-compose/"));
}

#[test]
fn bootstrap_reuses_mise_on_path_and_installs_from_repo_root() {
    let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let mise = temp.path().join("mise");
    fs::write(
        &mise,
        "#!/bin/sh\nprintf '%s:%s\\n' \"$PWD\" \"$*\" >> \"$BOOTSTRAP_CALLS\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&mise, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let calls = temp.path().join("calls");
    let output = Command::new("bash")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/bootstrap.sh"))
        .current_dir(temp.path())
        .env("PATH", format!("{}:/usr/bin:/bin", temp.path().display()))
        .env("BOOTSTRAP_CALLS", &calls)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let root = env!("CARGO_MANIFEST_DIR");
    assert_eq!(
        fs::read_to_string(calls).unwrap(),
        format!("{root}:trust\n{root}:install\n")
    );
}
