//! `symphony init -- <command> [args...]`: the container image's PID 1.
//!
//! PID 1 keeps the environment the container started with: the API token, tracker tokens and LLM
//! keys. Under tini that environment stayed readable (`/proc/1/environ`) to every process of the
//! same user outside a PID namespace, such as Claude Code's `Bash` tool, Codex commands and
//! hooks. This init marks itself non-dumpable before anything else ([`protect_secrets`]), and it
//! refuses to run when it cannot. It then:
//!
//! * starts the command in its own process group, giving it the terminal when there is one (so a
//!   Ctrl-C reaches it once);
//! * forwards termination, hang-up, user and window-size signals to it (PID 1 drops signals it
//!   has no handler for);
//! * reaps every child that ends, including orphans re-parented to it;
//! * exits with the command's status (`128 + signal` when a signal ended it).
//!
//! [`protect_secrets`]: crate::hardening::protect_secrets

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

#[cfg(unix)]
use rustix::process::{Pid, Signal};

/// Runs `command` (program and arguments) under the init and returns its exit status.
pub fn run(command: &[OsString]) -> ExitCode {
    supervise(command)
}

fn fail(message: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr().lock(), "symphony init: {message}");
    ExitCode::FAILURE
}

#[cfg(not(unix))]
fn supervise(_command: &[OsString]) -> ExitCode {
    fail("needs a Unix system")
}

/// Signals passed on to the command.
#[cfg(unix)]
const FORWARDED: [Signal; 7] = [
    Signal::TERM,
    Signal::INT,
    Signal::HUP,
    Signal::QUIT,
    Signal::USR1,
    Signal::USR2,
    Signal::WINCH,
];

#[cfg(unix)]
fn supervise(command: &[OsString]) -> ExitCode {
    use std::os::unix::process::CommandExt;

    if let Err(err) = crate::hardening::protect_secrets() {
        return fail(&format!(
            "cannot make the environment unreadable to other processes: {err}"
        ));
    }
    let Some((program, args)) = command.split_first() else {
        return fail("no command given");
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => return fail(&format!("cannot start: {err}")),
    };
    runtime.block_on(async {
        // Handlers first: a signal that arrives before the command exists is delivered to it
        // once it runs.
        let mut streams = Vec::with_capacity(FORWARDED.len());
        for signal in FORWARDED {
            let kind = tokio::signal::unix::SignalKind::from_raw(signal.as_raw());
            match tokio::signal::unix::signal(kind) {
                Ok(stream) => streams.push((signal, stream)),
                Err(err) => return fail(&format!("cannot handle {signal:?}: {err}")),
            }
        }
        let child = match std::process::Command::new(program)
            .args(args)
            .process_group(0)
            .spawn()
        {
            Ok(child) => child,
            Err(err) => {
                return fail(&format!(
                    "cannot start {}: {err}",
                    program.to_string_lossy()
                ));
            }
        };
        let pid = Pid::from_child(&child);
        hand_over_terminal(pid);
        for (signal, mut stream) in streams {
            tokio::spawn(async move {
                while stream.recv().await.is_some() {
                    let _ = rustix::process::kill_process(pid, signal);
                }
            });
        }
        tokio::task::spawn_blocking(move || reap_until(pid))
            .await
            .unwrap_or(ExitCode::FAILURE)
    })
}

/// Makes the command's process group the terminal's foreground group, when stdin is a terminal
/// whose foreground group is ours (otherwise `tcsetpgrp` would stop us with `SIGTTOU`).
#[cfg(unix)]
fn hand_over_terminal(pid: Pid) {
    let stdin = std::io::stdin();
    if rustix::termios::isatty(&stdin)
        && rustix::termios::tcgetpgrp(&stdin).is_ok_and(|group| group == rustix::process::getpgrp())
    {
        let _ = rustix::termios::tcsetpgrp(&stdin, pid);
    }
}

/// Reaps children until `child` ends; returns its exit status.
#[cfg(unix)]
fn reap_until(child: Pid) -> ExitCode {
    use rustix::io::Errno;
    use rustix::process::{WaitOptions, wait};

    loop {
        match wait(WaitOptions::empty()) {
            Ok(Some((pid, status))) if pid == child => {
                let code = match (status.exit_status(), status.terminating_signal()) {
                    (Some(code), _) => u8::try_from(code).unwrap_or(u8::MAX),
                    (None, Some(signal)) => u8::try_from(128 + signal).unwrap_or(u8::MAX),
                    (None, None) => u8::MAX,
                };
                return ExitCode::from(code);
            }
            // An orphan, or a signal interrupted the wait.
            Ok(_) | Err(Errno::INTR) => {}
            Err(err) => return fail(&format!("lost the command: {err}")),
        }
    }
}
