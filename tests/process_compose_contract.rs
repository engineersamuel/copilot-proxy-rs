#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

fn read(path: &str) -> String {
    fs::read_to_string(Path::new(ROOT).join(path)).unwrap()
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn mise_pins_process_compose_and_exposes_all_tasks() {
    let mise = read("mise.toml");
    assert!(mise.contains("[tools]\nprocess-compose = \"1.124.0\""));
    for task in ["up", "down", "restart", "status", "logs"] {
        assert!(mise.contains(&format!("[tasks.\"proxy:{task}\"]")));
        assert!(mise.contains(&format!("scripts/proxy-compose.sh {task}")));
    }
}

#[test]
fn proxy_has_loopback_readiness_and_preserves_logging_configuration() {
    let compose = read("process-compose.yaml");
    for required in [
        "proxy:",
        "cargo run --release --locked --bin copilot-proxy-rs",
        "readiness_probe:",
        "http_get:",
        "host: 127.0.0.1",
        "path: /health",
        "port: ${COPILOT_PROXY_RS_PORT:-8080}",
        "COPILOT_PROXY_RS_PORT=${COPILOT_PROXY_RS_PORT:-8080}",
        "RUST_LOG=${RUST_LOG:-info}",
    ] {
        assert!(compose.contains(required), "missing {required}");
    }
    assert!(!compose.contains("COPILOT_PROXY_RS_LOG_FAILED_REQUEST_BODIES"));
}

#[test]
fn scripts_are_executable_valid_bash_and_bootstrap_installs_mise() {
    for script in ["scripts/bootstrap.sh", "scripts/proxy-compose.sh"] {
        let path = Path::new(ROOT).join(script);
        assert_ne!(fs::metadata(&path).unwrap().permissions().mode() & 0o111, 0);
        success(Command::new("bash").arg("-n").arg(path).output().unwrap());
        assert!(read(script).contains("set -euo pipefail"));
    }
    let bootstrap = read("scripts/bootstrap.sh");
    for required in [
        "command -v mise",
        "$HOME/.local/bin/mise",
        "https://mise.run",
        "mise install",
    ] {
        assert!(bootstrap.contains(required), "missing {required}");
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = PathBuf::from(format!(
            "/tmp/cpr-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn executable(&self, name: &str, body: &str) {
        let path = self.0.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn script(&self, command: &str) -> Command {
        let mut cmd = Command::new("bash");
        cmd.arg(Path::new(ROOT).join("scripts/proxy-compose.sh"))
            .arg(command)
            .env("PATH", format!("{}:/usr/bin:/bin", self.0.display()))
            .env(
                "PC_SOCKET_PATH",
                "/tmp/factory-supervisor-must-not-be-used.sock",
            )
            .env_remove("COPILOT_PROXY_RS_PC_SOCKET")
            .env_remove("COPILOT_PROXY_RS_PORT")
            .env("TMPDIR", &self.0)
            .env("CALLS", self.0.join("calls"));
        cmd
    }

    fn ready_process_compose(&self) {
        fs::write(self.0.join("calls"), "").unwrap();
        self.executable(
            "process-compose",
            r#"#!/bin/bash
set -eu
[[ ${PC_SOCKET_PATH:-} != /tmp/factory-supervisor-must-not-be-used.sock ]]
printf '%s\n' "$*" >> "$CALLS"
printf '[{"name":"proxy","status":"Running","is_ready":"Ready"}]\n'
"#,
        );
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn controls_use_stable_isolated_socket_and_correct_commands() {
    let scratch = Scratch::new();
    scratch.ready_process_compose();
    success(
        scratch
            .script("up")
            .env("COPILOT_PROXY_RS_PORT", "18080")
            .output()
            .unwrap(),
    );
    for command in ["status", "logs", "tui", "down"] {
        success(scratch.script(command).output().unwrap());
    }
    let calls = fs::read_to_string(scratch.0.join("calls")).unwrap();
    let mut sockets = Vec::new();
    for call in calls.lines() {
        let args: Vec<_> = call.split_whitespace().collect();
        assert!(args.contains(&"-U"));
        assert!(!args.contains(&"-p"));
        assert!(!args.contains(&"--port"));
        let index = args.iter().position(|arg| *arg == "--unix-socket").unwrap();
        assert!(args[index + 1].len() < 100);
        sockets.push(args[index + 1]);
    }
    assert!(sockets.iter().all(|socket| *socket == sockets[0]));
    for required in [
        "-f process-compose.yaml",
        "-D",
        "--tui=false",
        "process list",
        "process logs proxy",
        "attach",
    ] {
        assert!(calls.contains(required), "missing {required}: {calls}");
    }
    success(
        scratch
            .script("tui")
            .env(
                "COPILOT_PROXY_RS_PC_SOCKET",
                scratch.0.join("override.sock"),
            )
            .output()
            .unwrap(),
    );
    let calls = fs::read_to_string(scratch.0.join("calls")).unwrap();
    assert!(calls.lines().last().unwrap().contains("override.sock"));
}

fn restart_with_reload_response(status: u16, body: &str) -> (Output, String) {
    let scratch = Scratch::new();
    scratch.ready_process_compose();
    let socket = scratch.0.join("reload.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let body = body.to_owned();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "reload request was not received");
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept reload request: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(&stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
        }
        write!(
            stream,
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        request_line
    });
    let output = scratch
        .script("restart")
        .env("COPILOT_PROXY_RS_PC_SOCKET", &socket)
        .output()
        .unwrap();
    assert_eq!(
        server.join().unwrap(),
        "POST /project/configuration HTTP/1.1\r\n"
    );
    let calls = fs::read_to_string(scratch.0.join("calls")).unwrap();
    for call in calls.lines() {
        assert!(call.starts_with(&format!("-U --unix-socket {} ", socket.display())));
    }
    (output, calls)
}

#[test]
fn restart_updated_proxy_reloads_without_an_extra_restart_and_waits_ready() {
    for body in [r#"{"proxy":"updated"}"#, "{ \"proxy\": \t \"updated\" }"] {
        let (output, calls) = restart_with_reload_response(200, body);
        assert_eq!(
            success(output),
            "Reloaded process-compose.yaml; proxy restarted with the new configuration\nproxy is Ready\n"
        );
        assert!(!calls.contains("process restart proxy"), "{calls}");
        assert!(calls.ends_with("process list -o json\n"), "{calls}");
    }
}

#[test]
fn restart_unchanged_proxy_reloads_then_restarts_once_and_waits_ready() {
    let (output, calls) = restart_with_reload_response(200, "{}");
    let stdout = success(output);
    assert!(stdout.starts_with("Reloaded process-compose.yaml (proxy configuration unchanged)\n"));
    assert!(stdout.ends_with("proxy is Ready\n"));
    assert_eq!(calls.matches("process restart proxy").count(), 1);
    assert!(
        calls
            .lines()
            .next()
            .unwrap()
            .ends_with("process restart proxy")
    );
    assert!(calls.ends_with("process list -o json\n"), "{calls}");
}

#[test]
fn restart_rejects_non_200_reload_without_restart_or_readiness() {
    for status in [400, 207] {
        let (output, calls) =
            restart_with_reload_response(status, &format!(r#"{{"error":"{}"}}"#, "x".repeat(4096)));
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("process-compose could not reload process-compose.yaml"));
        assert!(stderr.contains(&status.to_string()));
        assert!(
            stderr.len() < 512,
            "reload failure must not dump a large body"
        );
        assert!(calls.is_empty(), "{calls}");
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn restart_unreachable_reload_fails_without_restart_or_readiness() {
    let scratch = Scratch::new();
    scratch.ready_process_compose();
    let output = scratch
        .script("restart")
        .env("COPILOT_PROXY_RS_PC_SOCKET", scratch.0.join("missing.sock"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("process-compose could not reload process-compose.yaml")
    );
    assert!(
        fs::read_to_string(scratch.0.join("calls"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn restart_reload_is_time_bounded() {
    let script = read("scripts/proxy-compose.sh");
    let restart = script.split("    restart)\n").nth(1).unwrap();
    let restart = restart.split(";;").next().unwrap();
    assert!(restart.contains("--connect-timeout 1"));
    assert!(restart.contains("--max-time 60"));
}

#[test]
fn tui_preserves_attach_failure_and_filters_only_config_notice() {
    let scratch = Scratch::new();
    scratch.executable(
        "process-compose",
        r#"#!/bin/bash
set -eu
[[ $* == *" attach" ]]
printf '%s\n' 'process compose config home: test' 'cannot attach to proxy' >&2
exit 42
"#,
    );
    let output = scratch.script("tui").output().unwrap();
    assert_eq!(output.status.code(), Some(42));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "cannot attach to proxy\n"
    );
}

#[test]
fn unresponsive_socket_client_is_bounded() {
    for command in ["status", "logs"] {
        let scratch = Scratch::new();
        scratch.executable("process-compose", "#!/bin/bash\nexec sleep 30\n");
        let start = Instant::now();
        let output = scratch.script(command).output().unwrap();
        assert!(!output.status.success());
        assert!(start.elapsed() < Duration::from_secs(15));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .to_lowercase()
                .contains("timed out")
        );
    }
}

#[test]
fn down_cleans_refused_sockets_but_preserves_unresponsive_ones() {
    let scratch = Scratch::new();
    let socket = scratch.0.join("stale.sock");
    drop(UnixListener::bind(&socket).unwrap());
    success(
        scratch
            .script("down")
            .env("COPILOT_PROXY_RS_PC_SOCKET", &socket)
            .output()
            .unwrap(),
    );
    assert!(!socket.exists());

    let _listener = UnixListener::bind(&socket).unwrap();
    let output = scratch
        .script("down")
        .env("COPILOT_PROXY_RS_PC_SOCKET", &socket)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(socket.exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("leaving it intact"));
}

struct LiveProxy {
    socket: PathBuf,
}

impl LiveProxy {
    fn task(&self, task: &str) -> Command {
        let mut command = Command::new("mise");
        command
            .current_dir(ROOT)
            .args(["run", &format!("proxy:{task}")])
            .env("COPILOT_PROXY_RS_PC_SOCKET", &self.socket)
            .env(
                "PC_SOCKET_PATH",
                "/tmp/factory-supervisor-must-not-be-used.sock",
            );
        command
    }
}

impl Drop for LiveProxy {
    fn drop(&mut self) {
        let _ = self.task("down").output();
    }
}

#[test]
fn required_live_mise_lifecycle_uses_an_isolated_port() {
    if std::env::var("REQUIRE_MISE").as_deref() != Ok("1")
        && std::env::var("REQUIRE_PROCESS_COMPOSE").as_deref() != Ok("1")
    {
        eprintln!("live lifecycle skipped; set REQUIRE_MISE=1 to require it");
        return;
    }
    let scratch = Scratch::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    assert_ne!(port, 8080);
    let proxy = LiveProxy {
        socket: scratch.0.join("live.sock"),
    };
    drop(listener);
    success(
        proxy
            .task("up")
            .env("COPILOT_PROXY_RS_PORT", port.to_string())
            .output()
            .unwrap(),
    );
    let url = format!("http://127.0.0.1:{port}/health");
    success(
        Command::new("curl")
            .args(["-fsS", "--max-time", "5", &url])
            .output()
            .unwrap(),
    );
    let status = success(proxy.task("status").output().unwrap());
    assert!(status.contains("Running"));
    assert!(status.contains("Ready"));
    success(proxy.task("restart").output().unwrap());
    success(proxy.task("logs").output().unwrap());
    success(proxy.task("down").output().unwrap());
    assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
}
