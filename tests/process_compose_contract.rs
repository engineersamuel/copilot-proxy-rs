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
    assert!(mise.contains("config_root ~ '/.process-compose/pc.sock'"));
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
    assert!(probe.contains("host: \"${COPILOT_PROXY_RS_HOST:-127.0.0.1}\""));
    assert!(probe.contains("port: \"${COPILOT_PROXY_RS_PORT:-8080}\""));
    assert!(probe.contains("path: /health"));
    assert!(config.contains("restart: on_failure"));
    assert!(config.contains("signal: 15"));
    assert!(repo_file(".gitignore").contains("/.process-compose/"));
}

#[test]
fn process_compose_defaults_rust_log_to_info_with_an_override() {
    let config = repo_file("process-compose.yaml");
    let process = config.split("  copilot-proxy-rs:\n").nth(1).unwrap();
    let environment = process
        .split("    environment:\n")
        .nth(1)
        .unwrap()
        .split("\n    log_location:")
        .next()
        .unwrap();
    assert!(environment.contains("      - \"RUST_LOG=${RUST_LOG:-info}\""));
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

#[test]
fn preflight_rejects_a_port_that_is_already_listening() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/proxy-preflight.sh");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let busy = Command::new("bash")
        .arg(&script)
        .env("COPILOT_PROXY_RS_PORT", &port)
        .env("PC_SOCKET_PATH", "/nonexistent/pc.sock")
        .output()
        .unwrap();
    assert!(!busy.status.success());
    assert!(String::from_utf8_lossy(&busy.stderr).contains("already in use"));

    drop(listener);
    let free = Command::new("bash")
        .arg(&script)
        .env("COPILOT_PROXY_RS_PORT", &port)
        .env("PC_SOCKET_PATH", "/nonexistent/pc.sock")
        .status()
        .unwrap();
    assert!(free.success());
}

#[test]
fn proxy_up_runs_preflight_before_building() {
    let mise = repo_file("mise.toml");
    let preflight = mise.find("./scripts/proxy-preflight.sh").unwrap();
    assert!(preflight < mise.find("cargo build --release").unwrap());
}

#[test]
fn failed_readiness_stops_supervisor_and_restarts_are_bounded() {
    let mise = repo_file("mise.toml");
    assert!(mise.contains("proxy-wait-ready.sh || { process-compose down -U ; exit 1; }"));
    assert!(repo_file("process-compose.yaml").contains("max_restarts:"));
}

#[test]
fn wait_ready_parses_json_status() {
    let dir = std::env::temp_dir().join(format!("pc-fake-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let fake = dir.join("process-compose");
    fs::write(
        &fake,
        "#!/bin/sh\ncase \"$*\" in *json*) echo '[{\"name\":\"copilot-proxy-rs\",\"status\":\"Running\",\"is_ready\":\"Ready\",\"pid\":42}]';; *) echo table;; esac\n",
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
    let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap());
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/proxy-wait-ready.sh");
    let ok = Command::new(&script).env("PATH", &path).output().unwrap();
    assert!(ok.status.success());
    let ok = Command::new(&script)
        .arg("7")
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(ok.status.success());
    fs::remove_dir_all(&dir).ok();
}

fn fake_pc(dir: &Path, body: &str) -> String {
    fs::create_dir_all(dir).unwrap();
    let fake = dir.join("process-compose");
    fs::write(&fake, format!("#!/bin/sh\n{body}\n")).unwrap();
    Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
    format!("{}:{}", dir.display(), std::env::var("PATH").unwrap())
}

fn script(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join(name)
}

#[test]
fn preflight_rejects_wildcard_hosts_and_long_socket_paths() {
    let wild = Command::new("bash")
        .arg(script("proxy-preflight.sh"))
        .env("COPILOT_PROXY_RS_HOST", "0.0.0.0")
        .output()
        .unwrap();
    assert!(!wild.status.success());
    let long = Command::new("bash")
        .arg(script("proxy-preflight.sh"))
        .env(
            "PC_SOCKET_PATH",
            format!("/tmp/{}/pc.sock", "a".repeat(120)),
        )
        .output()
        .unwrap();
    assert!(!long.status.success());
    assert!(String::from_utf8_lossy(&long.stderr).contains("PC_SOCKET_PATH"));
}

#[test]
fn preflight_rejects_live_supervisor_and_clears_stale_socket() {
    let dir = std::env::temp_dir().join(format!("pc-pre-{}", std::process::id()));
    let sock = dir.join("s.sock");
    fs::create_dir_all(&dir).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port().to_string()
    };
    let run = |path: &str| {
        Command::new("bash")
            .arg(script("proxy-preflight.sh"))
            .env("PATH", path)
            .env("PC_SOCKET_PATH", &sock)
            .env("COPILOT_PROXY_RS_PORT", &port)
            .output()
            .unwrap()
    };
    let live = run(&fake_pc(&dir.join("live"), "exit 0"));
    assert!(!live.status.success());
    assert!(String::from_utf8_lossy(&live.stderr).contains("already running"));
    let stale = run(&fake_pc(&dir.join("dead"), "exit 1"));
    assert!(stale.status.success());
    assert!(!sock.exists());
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn wait_ready_fails_fast_when_supervisor_is_missing_or_process_errored() {
    let dir = std::env::temp_dir().join(format!("pc-wait-{}", std::process::id()));
    let down = fake_pc(&dir.join("down"), "exit 1");
    let out = Command::new(script("proxy-wait-ready.sh"))
        .env("PATH", &down)
        .env("PROXY_WAIT_GRACE", "1")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("failed to start"));
    let errored = fake_pc(
        &dir.join("err"),
        "echo '[{\"name\":\"copilot-proxy-rs\",\"status\":\"Error\",\"is_ready\":\"-\",\"pid\":0}]'",
    );
    let out = Command::new(script("proxy-wait-ready.sh"))
        .env("PATH", &errored)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("exited before"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn status_and_logs_guard_a_missing_supervisor() {
    let mise = repo_file("mise.toml");
    assert_eq!(
        mise.matches("./scripts/proxy-require-supervisor.sh")
            .count(),
        3
    );
    let dir = std::env::temp_dir().join(format!("pc-req-{}", std::process::id()));
    let out = Command::new(script("proxy-require-supervisor.sh"))
        .env("PATH", fake_pc(&dir, "exit 1"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("proxy:up"));
    fs::remove_dir_all(&dir).ok();
}
