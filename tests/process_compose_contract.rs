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
    assert!(config.contains("run = 'bash scripts/proxy-up.sh'"));
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn run_script(
    script: &str,
    dir: &Path,
    port: u16,
    config: &Path,
    timeout: &str,
) -> std::process::Output {
    let child = std::process::Command::new("bash")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(script))
        .env("COPILOT_PROXY_RS_PC_SOCKET", dir.join("pc.sock"))
        .env("COPILOT_PROXY_RS_PC_CONFIG", config)
        .env("COPILOT_PROXY_RS_PORT", port.to_string())
        .env("COPILOT_PROXY_RS_SKIP_BUILD", "1")
        .env("COPILOT_PROXY_RS_UP_TIMEOUT", timeout)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    within_deadline(child)
}

// Fails the test instead of hanging when the script never returns.
fn within_deadline(mut child: std::process::Child) -> std::process::Output {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            child.kill().ok();
            panic!("script did not finish within 60s");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    child.wait_with_output().unwrap()
}

fn pc_client(socket: &Path, args: &[&str]) -> std::process::Output {
    let child = std::process::Command::new("process-compose")
        .args(["-U", "-u"])
        .arg(socket)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    within_deadline(child)
}

// Runs the real script against process-compose on a private port and socket.
#[test]
fn proxy_up_is_idempotent_recovers_stale_socket_and_bounds_failure() {
    if std::process::Command::new("process-compose")
        .arg("version")
        .output()
        .is_err()
    {
        eprintln!("process-compose not installed; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("cprs-pc-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let bin = env!("CARGO_BIN_EXE_copilot-proxy-rs");
    let good = dir.join("good.yaml");
    fs::write(
        &good,
        config_file("process-compose.yaml").replace("./target/release/copilot-proxy-rs", bin),
    )
    .unwrap();
    let port = free_port();
    let socket = dir.join("pc.sock");

    // A stale socket file with no process behind it must not block startup.
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    assert!(socket.exists());
    let up = run_script("scripts/proxy-up.sh", &dir, port, &good, "30");
    assert!(
        up.status.success(),
        "{}",
        String::from_utf8_lossy(&up.stderr)
    );
    let list = pc_client(&socket, &["process", "list", "--output", "wide"]);
    assert!(String::from_utf8_lossy(&list.stdout).contains("Running"));
    let again = run_script("scripts/proxy-up.sh", &dir, port, &good, "30");
    assert!(again.status.success());
    assert!(String::from_utf8_lossy(&again.stdout).contains("already running"));

    assert!(pc_client(&socket, &["down"]).status.success());

    // A socket that accepts but never answers may belong to a live supervisor:
    // proxy:up must give up promptly and leave the socket alone.
    let hung = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let blocked = run_script("scripts/proxy-up.sh", &dir, port, &good, "30");
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("unresponsive"));
    assert!(socket.exists());
    drop(hung);
    fs::remove_file(&socket).unwrap();

    // A proxy that never becomes ready fails within the timeout and leaves nothing running.
    let bad = dir.join("bad.yaml");
    fs::write(&bad, "version: \"0.5\"\nprocesses:\n  proxy:\n    command: sleep 300\n    readiness_probe:\n      exec:\n        command: \"false\"\n      period_seconds: 1\n").unwrap();
    let failed = run_script("scripts/proxy-up.sh", &dir, port, &bad, "3");
    assert!(!failed.status.success());
    assert!(!socket.exists());
    fs::remove_dir_all(&dir).ok();
}
