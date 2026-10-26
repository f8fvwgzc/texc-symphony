//! `symphony init`, the container image's PID 1: its environment holds every secret the
//! container was started with, so no other process of its user may read it, and it must still
//! behave as an init (exit status, signals, orphans).
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const SYMPHONY: &str = env!("CARGO_BIN_EXE_symphony");
const SECRET: &str = "SYMPHONY_INIT_TEST_SECRET";

/// `symphony init -- sh -c <script>` (optionally behind `wrapper`), with a marker secret in the
/// environment and stdin detached (a terminal would be handed to the script).
fn init_sh(wrapper: &[&str], script: &str) -> Output {
    let (program, prefix) = match wrapper.split_first() {
        Some((program, rest)) => (*program, rest.to_vec()),
        None => (SYMPHONY, Vec::new()),
    };
    let mut command = Command::new(program);
    command.args(prefix);
    if !wrapper.is_empty() {
        command.arg(SYMPHONY);
    }
    command
        .args(["init", "--", "sh", "-c", script])
        .env(SECRET, "init-test-secret-0123456789")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[cfg(target_os = "linux")]
fn describe(output: &Output) -> String {
    format!(
        "{:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn the_command_exit_status_is_passed_through() {
    assert_eq!(init_sh(&[], "exit 7").status.code(), Some(7));
    // Killed by a signal: 128 + signal, as a shell reports it.
    assert_eq!(init_sh(&[], "kill -KILL $$").status.code(), Some(128 + 9));
    let missing = Command::new(SYMPHONY)
        .args(["init", "--", "/nonexistent/command"])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("cannot start /nonexistent/command"));
}

#[test]
fn sigterm_to_init_reaches_the_command() {
    let mut init = Command::new(SYMPHONY)
        .args([
            "init",
            "--",
            "sh",
            "-c",
            "trap 'exit 42' TERM; echo ready; while :; do sleep 0.05; done",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    BufReader::new(init.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready.trim(), "ready");
    rustix::process::kill_process(
        rustix::process::Pid::from_child(&init),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = init.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = init.kill();
            panic!("SIGTERM was not forwarded to the command");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(42));
}

/// The check every agent child could run: read the environment of its init. A sibling of the
/// same user stays readable, so the denial is real and not an artefact of the test host.
#[cfg(target_os = "linux")]
const READ_INIT_ENVIRONMENT: &str = r#"
    init=$1
    [ "$(cat /proc/$init/comm)" = symphony ] || { echo "pid $init is $(cat /proc/$init/comm)"; exit 1; }
    sleep 5 & control=$!
    tr '\0' '\n' < /proc/$control/environ | grep -q "^SYMPHONY_INIT_TEST_SECRET=" \
        || { echo "control environment unreadable"; exit 1; }
    kill $control; wait $control
    if cat /proc/$init/environ > /dev/null 2>&1; then echo "init environment readable"; exit 1; fi
"#;

#[cfg(target_os = "linux")]
#[test]
fn a_child_of_the_same_user_cannot_read_the_init_environment() {
    let output = init_sh(&[], &format!("set -- $PPID\n{READ_INIT_ENVIRONMENT}"));
    assert!(output.status.success(), "{}", describe(&output));
}

/// The image layout: `symphony init` is PID 1 of a PID namespace and an agent child runs next to
/// Symphony in it, outside any sandbox.
#[cfg(target_os = "linux")]
#[test]
fn as_pid_1_init_hides_its_environment_and_reaps_orphans() {
    if !bubblewrap_as_pid_1_works() {
        eprintln!("skipped: bubblewrap cannot start a command as PID 1 here");
        return;
    }
    let script = format!(
        r#"set -- 1
        {READ_INIT_ENVIRONMENT}
        # An orphan is re-parented to PID 1, which must reap it once it ends.
        sh -c 'sleep 0.2 &'
        sleep 1
        for stat in /proc/[0-9]*/stat; do
            # After the command name: state, then the parent pid.
            case "$(sed 's/^.*) //' "$stat" 2>/dev/null)" in "Z 1 "*) echo "zombie: $stat"; exit 1;; esac
        done"#
    );
    let output = init_sh(
        &[
            "bwrap",
            "--unshare-user",
            "--unshare-pid",
            "--as-pid-1",
            "--die-with-parent",
            "--dev-bind",
            "/",
            "/",
            "--proc",
            "/proc",
            "--",
        ],
        &script,
    );
    assert!(output.status.success(), "{}", describe(&output));
}

#[cfg(target_os = "linux")]
fn bubblewrap_as_pid_1_works() -> bool {
    let works = Command::new("bwrap")
        .args([
            "--unshare-user",
            "--unshare-pid",
            "--as-pid-1",
            "--dev-bind",
            "/",
            "/",
            "--proc",
            "/proc",
            "--",
            "sh",
            "-c",
            "test $$ = 1",
        ])
        .stdin(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !works && std::env::var_os("SYMPHONY_REQUIRE_SANDBOX").is_some_and(|v| !v.is_empty()) {
        panic!("SYMPHONY_REQUIRE_SANDBOX is set but bubblewrap cannot start a PID 1 here");
    }
    works
}
