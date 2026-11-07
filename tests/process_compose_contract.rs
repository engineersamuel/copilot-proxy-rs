use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TASKS: [&str; 5] = ["up", "down", "restart", "status", "logs"];

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &str) -> String {
    fs::read_to_string(repo().join(path)).unwrap_or_else(|err| panic!("read {path}: {err}"))
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pc-contract-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("bin")).unwrap();
    dir
}

fn write_stub(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn mise_pins_process_compose_exactly() {
    let mise = read("mise.toml");
    let tools = mise
        .split("[tools]")
        .nth(1)
        .expect("mise.toml has a [tools] table");
    let tools = tools.split("\n[").next().unwrap();
    assert!(
        tools
            .lines()
            .any(|line| line.trim() == r#"process-compose = "1.124.0""#),
        "process-compose must be pinned to exactly 1.124.0 in [tools]"
    );
}

#[test]
fn mise_defines_the_five_proxy_tasks_through_one_script() {
    let mise = read("mise.toml");
    for task in TASKS {
        let header = format!(r#"[tasks."proxy:{task}"]"#);
        let body = mise
            .split(&header)
            .nth(1)
            .unwrap_or_else(|| panic!("missing task proxy:{task}"));
        let body = body.split("\n[").next().unwrap();
        assert!(
            body.contains(&format!("scripts/proxy.sh {task}")),
            "proxy:{task} must run scripts/proxy.sh {task}"
        );
    }
}

#[test]
fn process_compose_probes_health_on_the_proxy_port() {
    let config = read("process-compose.yaml");
    let probe = config
        .split("readiness_probe:")
        .nth(1)
        .expect("proxy has a readiness_probe");
    assert!(probe.contains("http_get:"));
    assert!(probe.contains("host: 127.0.0.1"));
    assert!(probe.contains("path: /health"));
    assert!(probe.contains("port: ${COPILOT_PROXY_RS_PORT"));
    assert!(config.contains("RUST_LOG=info"));
    assert!(config.contains("COPILOT_PROXY_RS_PORT=${COPILOT_PROXY_RS_PORT:-8080}"));
}

#[test]
fn scripts_parse_with_bash() {
    for script in ["scripts/proxy.sh", "scripts/bootstrap.sh"] {
        let output = Command::new("bash")
            .arg("-n")
            .arg(repo().join(script))
            .output()
            .unwrap();
        assert_success(&output);
    }
}

#[test]
fn runtime_dir_is_gitignored() {
    assert!(
        read(".gitignore")
            .lines()
            .any(|line| line.trim() == "/.run/")
    );
}

/// Runs scripts/proxy.sh with a recording process-compose stub on PATH and
/// returns one line per stub call: "<args> | <PC_* env>".
fn proxy_calls(command: &str, extra_env: &[(&str, &str)]) -> Vec<String> {
    let dir = scratch_dir(&format!("proxy-{command}"));
    let log = dir.join("calls.log");
    write_stub(
        &dir.join("bin/process-compose"),
        &format!(
            r#"printf '%s | %s\n' "$*" "$(env | grep '^PC_' | sort | tr '\n' ' ')" >> '{}'"#,
            log.display()
        ),
    );
    let path = format!("{}:/usr/bin:/bin", dir.join("bin").display());
    let mut cmd = Command::new("bash");
    cmd.arg(repo().join("scripts/proxy.sh"))
        .arg(command)
        .env("PATH", path)
        .env("PC_SOCKET_PATH", "/tmp/factory-supervisor.sock")
        .env("PC_PORT_NUM", "9999")
        .env("PC_ADDRESS", "0.0.0.0");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    assert_success(&cmd.output().unwrap());
    let calls = fs::read_to_string(&log).unwrap_or_default();
    let _ = fs::remove_dir_all(&dir);
    calls.lines().map(str::to_owned).collect()
}

#[test]
fn proxy_control_uses_only_the_unix_socket_and_ignores_inherited_pc_env() {
    let sock = "/tmp/pc-contract-explicit.sock";
    for command in ["status", "logs", "down"] {
        let calls = proxy_calls(command, &[("COPILOT_PROXY_RS_PC_SOCKET", sock)]);
        assert!(!calls.is_empty(), "{command} must call process-compose");
        for call in calls {
            let (args, env) = call.split_once(" | ").unwrap();
            assert!(
                args.starts_with(&format!("-U -u {sock} ")),
                "{command}: control must go through the unix socket: {args}"
            );
            assert!(!args.contains("--port") && !args.contains("-p "), "{args}");
            assert!(
                env.trim().is_empty(),
                "{command}: inherited PC_* env leaked: {env}"
            );
        }
    }
}

#[test]
fn proxy_socket_defaults_to_a_per_port_path_under_run_dir() {
    let calls = proxy_calls("status", &[("COPILOT_PROXY_RS_PORT", "18081")]);
    let expected = format!("-U -u {}/.run/pc-18081.sock ", repo().display());
    assert!(
        calls.iter().all(|call| call.starts_with(&expected)),
        "{calls:?}"
    );
}

#[test]
fn proxy_down_succeeds_when_nothing_runs() {
    let dir = scratch_dir("down-idle");
    write_stub(&dir.join("bin/process-compose"), "exit 1");
    let output = Command::new("bash")
        .arg(repo().join("scripts/proxy.sh"))
        .arg("down")
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", dir.join("bin").display()),
        )
        .env("COPILOT_PROXY_RS_PC_SOCKET", dir.join("pc.sock"))
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert_success(&output);
}

fn run_bootstrap(dir: &Path) -> Output {
    Command::new("bash")
        .arg(repo().join("scripts/bootstrap.sh"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", dir.join("bin").display()),
        )
        .env("HOME", dir)
        .output()
        .unwrap()
}

#[test]
fn bootstrap_skips_install_when_mise_exists_and_runs_mise_install() {
    let dir = scratch_dir("bootstrap-present");
    let log = dir.join("calls.log");
    write_stub(
        &dir.join("bin/mise"),
        &format!(r#"echo "mise $*" >> '{}'"#, log.display()),
    );
    write_stub(
        &dir.join("bin/curl"),
        &format!(r#"echo "curl $*" >> '{}'"#, log.display()),
    );
    let output = run_bootstrap(&dir);
    let calls = fs::read_to_string(&log).unwrap_or_default();
    let _ = fs::remove_dir_all(&dir);
    assert_success(&output);
    assert_eq!(calls, "mise install\n");
}

#[test]
fn bootstrap_installs_mise_when_missing_then_runs_mise_install() {
    let dir = scratch_dir("bootstrap-missing");
    let log = dir.join("calls.log");
    // Stub installer: what `curl https://mise.run` pipes to sh. It drops a
    // recording mise into ~/.local/bin, like the real installer.
    let installer = format!(
        r#"mkdir -p "$HOME/.local/bin"
printf '#!/bin/sh\necho "mise $*" >> {log}\n' > "$HOME/.local/bin/mise"
chmod +x "$HOME/.local/bin/mise""#,
        log = log.display()
    );
    fs::write(dir.join("installer.sh"), installer).unwrap();
    write_stub(
        &dir.join("bin/curl"),
        &format!(
            r#"echo "curl $*" >> '{}'
cat '{}'"#,
            log.display(),
            dir.join("installer.sh").display()
        ),
    );
    let output = run_bootstrap(&dir);
    let calls = fs::read_to_string(&log).unwrap_or_default();
    let _ = fs::remove_dir_all(&dir);
    assert_success(&output);
    let mut lines = calls.lines();
    assert!(
        lines.next().unwrap_or_default().starts_with("curl ") && calls.contains("https://mise.run"),
        "{calls}"
    );
    assert_eq!(lines.next(), Some("mise install"), "{calls}");
}
