use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard};

const TASKS: [&str; 5] = [
    "proxy:up",
    "proxy:down",
    "proxy:restart",
    "proxy:status",
    "proxy:logs",
];

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &str) -> String {
    let full = root().join(path);
    fs::read_to_string(&full).unwrap_or_else(|err| panic!("read {}: {err}", full.display()))
}

#[test]
fn mise_pins_process_compose_and_defines_proxy_tasks() {
    let mise: toml::Table = toml::from_str(&read("mise.toml")).expect("mise.toml parses");
    let version = mise["tools"]["process-compose"]
        .as_str()
        .expect("process-compose pin is a string");
    let parts: Vec<&str> = version.split('.').collect();
    assert!(
        parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
        "process-compose must be pinned to an exact semver, got {version}"
    );
    for task in TASKS {
        let verb = task.trim_start_matches("proxy:");
        let run = mise["tasks"][task]["run"]
            .as_str()
            .unwrap_or_else(|| panic!("task {task} needs a run string"));
        assert_eq!(run, format!("scripts/process-compose.sh {verb}"), "{task}");
    }
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Lines nested under the first `key:` line found at the top level of `lines`.
fn block<'a>(lines: &[&'a str], key: &str) -> Vec<&'a str> {
    let header = format!("{key}:");
    let base = lines.iter().map(|l| indent(l)).min().unwrap_or(0);
    let start = lines
        .iter()
        .position(|l| indent(l) == base && l.trim() == header)
        .unwrap_or_else(|| panic!("missing block {key}"));
    lines[start + 1..]
        .iter()
        .take_while(|l| l.trim().is_empty() || indent(l) > base)
        .filter(|l| !l.trim().is_empty())
        .copied()
        .collect()
}

/// Value of a top-level `key: value` line in `lines`, without quotes.
fn scalar<'a>(lines: &[&'a str], key: &str) -> &'a str {
    let base = lines.iter().map(|l| indent(l)).min().unwrap_or(0);
    let prefix = format!("{key}:");
    lines
        .iter()
        .filter(|l| indent(l) == base)
        .find_map(|l| l.trim().strip_prefix(&prefix))
        .map(|v| v.trim().trim_matches('"'))
        .unwrap_or_else(|| panic!("missing {key}"))
}

/// Entries of a top-level `- item` list in `lines`, without quotes.
fn items<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    lines
        .iter()
        .filter_map(|l| l.trim().strip_prefix("- "))
        .map(|v| v.trim().trim_matches('"'))
        .collect()
}

#[test]
fn process_compose_config_probes_health_on_configured_port() {
    let text = read("process-compose.yaml");
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect();
    let proxy = block(&block(&lines, "processes"), "copilot-proxy-rs");
    let probe = block(&block(&proxy, "readiness_probe"), "http_get");
    assert_eq!(scalar(&probe, "path"), "/health");
    assert_eq!(scalar(&probe, "host"), "127.0.0.1");
    let env = items(&block(&proxy, "environment"));
    let port = scalar(&probe, "port");
    assert_eq!(port, "${COPILOT_PROXY_RS_PORT:-8080}");
    assert!(
        env.contains(&format!("COPILOT_PROXY_RS_PORT={port}").as_str()),
        "environment port must equal probe port: {env:?}"
    );
    assert!(
        env.contains(&"COPILOT_PROXY_RS_HOST=127.0.0.1"),
        "bind host must match the 127.0.0.1 probe: {env:?}"
    );
    assert_eq!(
        scalar(&block(&proxy, "availability"), "restart"),
        "on_failure"
    );
}

#[test]
fn process_compose_validates_config_when_installed() {
    if !on_path("process-compose") {
        eprintln!("skip: process-compose not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new("process-compose")
        .current_dir(root())
        .args(["-f", "process-compose.yaml", "--dry-run", "-U", "-u"])
        .arg(dir.path().join("pc.sock"))
        .env_remove("PC_SOCKET_PATH")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
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

fn on_path(bin: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {bin}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

const SYSTEM_PATH: &str = "/usr/bin:/bin";

/// A temp dir with stub commands that append "<name> <args>" to `calls.log`.
struct Stubs {
    dir: tempfile::TempDir,
}

impl Stubs {
    fn new() -> Self {
        let stubs = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir(stubs.bin()).unwrap();
        stubs
    }

    fn bin(&self) -> PathBuf {
        self.dir.path().join("bin")
    }

    fn log(&self) -> PathBuf {
        self.dir.path().join("calls.log")
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.log()).unwrap_or_default()
    }

    fn add(&self, name: &str, body: &str) {
        let path = self.bin().join(name);
        let script = format!(
            "#!/bin/sh\necho \"{name} $*\" >> \"{}\"\n{body}\n",
            self.log().display()
        );
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn path(&self) -> String {
        format!("{}:{SYSTEM_PATH}", self.bin().display())
    }
}

/// Tests that bind or pick a TCP port run one at a time, so a port from
/// `free_port()` is not taken by a parallel test before the script checks it.
static PORT_LOCK: Mutex<()> = Mutex::new(());

fn port_lock() -> MutexGuard<'static, ()> {
    PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Call only while holding `port_lock()`.
fn free_port() -> u16 {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    assert_ne!(port, 8080, "tests must never use port 8080");
    port
}

/// Stubs for process-compose.sh. `pc_list` and `pc_down` set the exit codes of
/// `process-compose process list` and `process-compose down`; `curl` sets the
/// /health result. `seq` and `sleep 1` are stubbed so the health loop runs once.
fn pc_stubs(pc_list: i32, pc_down: i32, curl: i32) -> Stubs {
    let stubs = Stubs::new();
    stubs.add(
        "process-compose",
        &format!(
            "case \"$*\" in\n  *\"process list\"*) exit {pc_list} ;;\n  down*) exit {pc_down} ;;\nesac"
        ),
    );
    stubs.add("cargo", "exit 0");
    stubs.add("curl", &format!("exit {curl}"));
    stubs.add("seq", "echo 1");
    stubs.add("sleep", "[ \"$1\" = 1 ] && exit 0\nexec /bin/sleep \"$@\"");
    stubs
}

fn run_pc(stubs: &Stubs, port: u16, socket: &Path, arg: &str) -> Output {
    Command::new("/bin/bash")
        .arg(root().join("scripts/process-compose.sh"))
        .arg(arg)
        .env("PATH", stubs.path())
        .env("COPILOT_PROXY_RS_PORT", port.to_string())
        .env("COPILOT_PROXY_RS_PC_SOCKET", socket)
        .output()
        .unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn up_refuses_when_port_is_in_use() {
    let _port = port_lock();
    let stubs = pc_stubs(1, 0, 0);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let socket = stubs.dir.path().join("pc.sock");
    let out = run_pc(&stubs, port, &socket, "up");
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("already in use"), "{}", stderr(&out));
    assert!(!stubs.calls().contains("cargo"), "{}", stubs.calls());
}

#[test]
fn up_refuses_when_live_supervisor_owns_socket() {
    let _port = port_lock();
    let stubs = pc_stubs(0, 0, 0);
    let socket = stubs.dir.path().join("pc.sock");
    let _sock = UnixListener::bind(&socket).unwrap();
    let out = run_pc(&stubs, free_port(), &socket, "up");
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("already runs"), "{}", stderr(&out));
    assert!(socket.exists());
    assert!(!stubs.calls().contains("cargo"), "{}", stubs.calls());
}

#[test]
fn up_removes_stale_socket_and_starts() {
    let _port = port_lock();
    let stubs = pc_stubs(1, 0, 0);
    let socket = stubs.dir.path().join("pc.sock");
    let sock = UnixListener::bind(&socket).unwrap();
    drop(sock);
    let out = run_pc(&stubs, free_port(), &socket, "up");
    assert!(out.status.success(), "{}", stderr(&out));
    let calls = stubs.calls();
    assert!(calls.contains("cargo build --release --locked"), "{calls}");
    assert!(calls.contains("process-compose up -D"), "{calls}");
    assert!(!socket.exists(), "stale socket must be removed");
}

#[test]
fn up_stops_supervisor_when_health_never_succeeds() {
    let _port = port_lock();
    let stubs = pc_stubs(1, 0, 7);
    let socket = stubs.dir.path().join("pc.sock");
    let _sock = UnixListener::bind(&socket).unwrap();
    let out = run_pc(&stubs, free_port(), &socket, "up");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("not OK after 120s"),
        "{}",
        stderr(&out)
    );
    let calls = stubs.calls();
    let up = calls.find("process-compose up -D").expect("pc up called");
    let down = calls.find("process-compose down").expect("pc down called");
    assert!(down > up, "{calls}");
    assert!(!socket.exists(), "socket must be removed after failed up");
}

#[test]
fn restart_keeps_socket_when_down_fails() {
    let _port = port_lock();
    let stubs = pc_stubs(0, 1, 0);
    let socket = stubs.dir.path().join("pc.sock");
    let _sock = UnixListener::bind(&socket).unwrap();
    let out = run_pc(&stubs, free_port(), &socket, "restart");
    assert!(!out.status.success());
    assert!(socket.exists(), "socket of a live supervisor must stay");
    assert!(!stubs.calls().contains("cargo"), "{}", stubs.calls());
}

#[test]
fn down_without_socket_succeeds() {
    let _port = port_lock();
    let stubs = pc_stubs(1, 0, 0);
    let socket = stubs.dir.path().join("pc.sock");
    let out = run_pc(&stubs, free_port(), &socket, "down");
    assert_eq!(out.status.code(), Some(0));
    assert!(
        !stubs.calls().contains("process-compose"),
        "{}",
        stubs.calls()
    );
}

#[test]
fn unknown_subcommand_exits_2() {
    let _port = port_lock();
    let stubs = pc_stubs(1, 0, 0);
    let socket = stubs.dir.path().join("pc.sock");
    let out = run_pc(&stubs, free_port(), &socket, "bogus");
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("usage:"), "{}", stderr(&out));
}

#[test]
fn scripts_pass_syntax_and_shellcheck() {
    for script in ["scripts/bootstrap.sh", "scripts/process-compose.sh"] {
        let path = root().join(script);
        let out = Command::new("/bin/bash")
            .arg("-n")
            .arg(&path)
            .output()
            .unwrap();
        assert!(out.status.success(), "{script}: {}", stderr(&out));
        if on_path("shellcheck") {
            let out = Command::new("shellcheck").arg(&path).output().unwrap();
            assert!(
                out.status.success(),
                "{script}: {}",
                String::from_utf8_lossy(&out.stdout)
            );
        } else {
            eprintln!("skip: shellcheck not on PATH");
        }
    }
}

fn run_bootstrap(stubs: &Stubs, home: &Path) -> Output {
    Command::new("/bin/bash")
        .arg(root().join("scripts/bootstrap.sh"))
        .env("PATH", stubs.path())
        .env("HOME", home)
        .output()
        .unwrap()
}

#[test]
fn bootstrap_skips_install_when_mise_exists() {
    let stubs = Stubs::new();
    stubs.add("cargo", "exit 0");
    stubs.add("curl", "exit 0");
    stubs.add("mise", "exit 0");
    let home = tempfile::tempdir().unwrap();
    let out = run_bootstrap(&stubs, home.path());
    assert!(out.status.success(), "{}", stderr(&out));
    let calls = stubs.calls();
    assert!(!calls.contains("curl"), "{calls}");
    assert!(calls.contains("mise install"), "{calls}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("PATH"));
}

#[test]
fn bootstrap_installs_mise_once_when_missing() {
    let stubs = Stubs::new();
    stubs.add("cargo", "exit 0");
    // The installer script that `curl | sh` runs: put a mise stub in ~/.local/bin.
    let mise_stub = format!(
        "#!/bin/sh\necho \"mise $*\" >> \"{}\"\n",
        stubs.log().display()
    );
    stubs.add(
        "curl",
        &format!(
            "cat <<'EOS'\nmkdir -p \"$HOME/.local/bin\"\ncat > \"$HOME/.local/bin/mise\" <<'EOM'\n{mise_stub}EOM\nchmod +x \"$HOME/.local/bin/mise\"\nEOS"
        ),
    );
    let home = tempfile::tempdir().unwrap();
    let out = run_bootstrap(&stubs, home.path());
    assert!(out.status.success(), "{}", stderr(&out));
    let calls = stubs.calls();
    assert_eq!(calls.matches("curl ").count(), 1, "{calls}");
    assert!(calls.contains("mise install"), "{calls}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(".local/bin"),
        "bootstrap must print a PATH hint"
    );
}

#[test]
fn bootstrap_fails_without_cargo() {
    let stubs = Stubs::new();
    stubs.add("mise", "exit 0");
    stubs.add("curl", "exit 0");
    let home = tempfile::tempdir().unwrap();
    let out = run_bootstrap(&stubs, home.path());
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("rustup"), "{}", stderr(&out));
    assert!(!stubs.calls().contains("mise"), "{}", stubs.calls());
}

#[test]
fn bootstrap_is_executable() {
    let mode = fs::metadata(root().join("scripts/bootstrap.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert!(mode & 0o111 != 0, "bootstrap.sh must be executable");
}
