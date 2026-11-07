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

/// String value of `key = "..."` in the TOML section `[header]`.
fn toml_value<'a>(text: &'a str, header: &str, key: &str) -> Option<&'a str> {
    let header = format!("[{header}]");
    text.lines()
        .map(str::trim)
        .skip_while(|l| *l != header)
        .skip(1)
        .take_while(|l| !l.starts_with('['))
        .find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == key).then(|| v.trim().strip_prefix('"')?.strip_suffix('"'))?
        })
}

#[test]
fn mise_pins_process_compose_and_defines_proxy_tasks() {
    let mise = read("mise.toml");
    let version =
        toml_value(&mise, "tools", "process-compose").expect("process-compose pin is a string");
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
        let run = toml_value(&mise, &format!("tasks.\"{task}\""), "run")
            .unwrap_or_else(|| panic!("task {task} needs a run string"));
        assert_eq!(run, format!("scripts/process-compose.sh {verb}"), "{task}");
    }
}

#[test]
fn mise_tasks_ls_lists_proxy_tasks_when_installed() {
    if !tool_available("mise", "REQUIRE_MISE") {
        return;
    }
    let out = Command::new("mise")
        .current_dir(root())
        .args(["tasks", "ls"])
        .env("MISE_TRUSTED_CONFIG_PATHS", root())
        .env_remove("MISE_ENV")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let listed = String::from_utf8_lossy(&out.stdout);
    for task in TASKS {
        assert!(listed.contains(task), "mise tasks ls misses {task}");
    }
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn top_indent(lines: &[&str]) -> usize {
    lines.iter().map(|l| indent(l)).min().unwrap_or(0)
}

/// Lines nested under the one `key:` line at the top indent of `lines`.
fn yaml_block<'a>(lines: &[&'a str], key: &str) -> Vec<&'a str> {
    let top = top_indent(lines);
    let head = format!("{key}:");
    let starts: Vec<usize> = (0..lines.len())
        .filter(|&i| indent(lines[i]) == top && lines[i].trim() == head)
        .collect();
    assert_eq!(starts.len(), 1, "need exactly one {head} (duplicate key?)");
    lines[starts[0] + 1..]
        .iter()
        .take_while(|l| indent(l) > top)
        .copied()
        .collect()
}

/// `value` without its double quotes. Panics on a quote that is not closed.
fn unquote(value: &str) -> &str {
    let value = value.trim();
    if !value.starts_with('"') && !value.ends_with('"') {
        return value;
    }
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or_else(|| panic!("unclosed quote: {value}"))
}

/// Value of `key: value` at the top indent of `lines`, without quotes.
fn yaml_scalar<'a>(lines: &[&'a str], key: &str) -> Option<&'a str> {
    let top = top_indent(lines);
    let head = format!("{key}:");
    let values: Vec<&str> = lines
        .iter()
        .filter(|l| indent(l) == top)
        .filter_map(|l| l.trim().strip_prefix(head.as_str()))
        .collect();
    assert!(
        values.len() <= 1,
        "need at most one {head} (duplicate key?)"
    );
    values.first().map(|v| unquote(v))
}

/// Check the copilot-proxy-rs process contract in process-compose.yaml text.
fn check_process_compose(text: &str) {
    assert!(
        !text.contains('\t'),
        "tab characters are not valid YAML indentation"
    );
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .collect();
    let proxy = yaml_block(&yaml_block(&lines, "processes"), "copilot-proxy-rs");
    let probe = yaml_block(&yaml_block(&proxy, "readiness_probe"), "http_get");
    assert_eq!(yaml_scalar(&probe, "path"), Some("/health"));
    assert_eq!(yaml_scalar(&probe, "host"), Some("127.0.0.1"));
    let port = yaml_scalar(&probe, "port").expect("probe port is set");
    assert_eq!(port, "${COPILOT_PROXY_RS_PORT:-8080}");
    let env_lines = yaml_block(&proxy, "environment");
    let env_top = top_indent(&env_lines);
    let env: Vec<&str> = env_lines
        .iter()
        .filter(|l| indent(l) == env_top)
        .filter_map(|l| l.trim().strip_prefix("- "))
        .map(unquote)
        .collect();
    assert!(
        env.contains(&format!("COPILOT_PROXY_RS_PORT={port}").as_str()),
        "environment port must equal probe port: {env:?}"
    );
    assert!(
        env.contains(&"COPILOT_PROXY_RS_HOST=127.0.0.1"),
        "bind host must match the 127.0.0.1 probe: {env:?}"
    );
    assert_eq!(
        yaml_scalar(&yaml_block(&proxy, "availability"), "restart"),
        Some("on_failure")
    );
}

#[test]
fn process_compose_config_probes_health_on_configured_port() {
    check_process_compose(&read("process-compose.yaml"));
}

#[test]
#[should_panic(expected = "tab characters")]
fn process_compose_check_rejects_tab_indentation() {
    let text = read("process-compose.yaml").replace("    command:", "\tcommand:");
    check_process_compose(&text);
}

#[test]
#[should_panic(expected = "duplicate key")]
fn process_compose_check_rejects_duplicate_process_key() {
    let text = read("process-compose.yaml");
    let dup = text.replace(
        "processes:\n",
        "processes:\n  copilot-proxy-rs:\n    command: x\n",
    );
    assert_ne!(dup, text);
    check_process_compose(&dup);
}

#[test]
#[should_panic(expected = "bind host must match")]
fn process_compose_check_ignores_nested_env_entries() {
    let text = read("process-compose.yaml");
    let nested = text.replace(
        "      - \"COPILOT_PROXY_RS_HOST=127.0.0.1\"\n",
        "      - nested:\n          - \"COPILOT_PROXY_RS_HOST=127.0.0.1\"\n",
    );
    assert_ne!(nested, text);
    check_process_compose(&nested);
}

#[test]
#[should_panic(expected = "unclosed quote")]
fn process_compose_check_rejects_unclosed_env_quote() {
    let text = read("process-compose.yaml");
    let bad = text.replace(
        "\"COPILOT_PROXY_RS_HOST=127.0.0.1\"",
        "\"COPILOT_PROXY_RS_HOST=127.0.0.1",
    );
    assert_ne!(bad, text);
    check_process_compose(&bad);
}

#[test]
#[should_panic(expected = "unclosed quote")]
fn process_compose_check_rejects_unclosed_scalar_quote() {
    let text = read("process-compose.yaml");
    let bad = text.replace(
        "port: ${COPILOT_PROXY_RS_PORT:-8080}",
        "port: \"${COPILOT_PROXY_RS_PORT:-8080}",
    );
    assert_ne!(bad, text);
    check_process_compose(&bad);
}

#[test]
#[should_panic(expected = "duplicate key")]
fn process_compose_check_rejects_duplicate_probe_scalar() {
    let text = read("process-compose.yaml");
    let bad = text.replace(
        "        path: /health\n",
        "        path: /health\n        path: /other\n",
    );
    assert_ne!(bad, text);
    check_process_compose(&bad);
}

/// True if `bin` is on PATH. If not, skip, or panic when `require_var=1`.
fn tool_available(bin: &str, require_var: &str) -> bool {
    if on_path(bin) {
        return true;
    }
    assert!(
        std::env::var(require_var).as_deref() != Ok("1"),
        "{require_var}=1 but {bin} is not on PATH"
    );
    eprintln!("skip: {bin} not on PATH (set {require_var}=1 to require it)");
    false
}

#[test]
fn process_compose_validates_config_when_installed() {
    if !tool_available("process-compose", "REQUIRE_PROCESS_COMPOSE") {
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
