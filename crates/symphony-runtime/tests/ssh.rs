//! Ports of `ssh_test.exs` (B.13.4) against a fake `ssh` executable.

mod support;

use std::time::Duration;

use support::{executable_copy, fixture, read};
use symphony_codex::RemoteLauncher;
use symphony_runtime::process::EnvPolicy;
use symphony_runtime::ssh::{SshConfig, SshError, SshLauncher, SshRunError};

struct FakeSsh {
    dir: tempfile::TempDir,
    config: SshConfig,
}

impl FakeSsh {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let exe = executable_copy(&fixture("ssh/fake_ssh.sh"), dir.path(), "ssh");
        Self {
            config: SshConfig::default().with_executable(exe),
            dir,
        }
    }

    fn trace(&self) -> String {
        read(&self.dir.path().join("ssh.trace"))
    }
}

async fn run(config: &SshConfig, host: &str, command: &str) -> (String, i32) {
    let out = config
        .run(
            host,
            command,
            Duration::from_secs(10),
            &EnvPolicy::inherit(),
        )
        .await
        .unwrap();
    (out.output, out.status)
}

#[tokio::test]
async fn keeps_bracketed_ipv6_host_port_targets_intact() {
    let ssh = FakeSsh::new();
    assert_eq!(
        run(&ssh.config, "root@[::1]:2200", "printf ok").await,
        (String::new(), 0)
    );
    assert!(ssh.trace().contains("-T -p 2200 root@[::1] bash -lc"));
}

#[tokio::test]
async fn leaves_unbracketed_ipv6_style_targets_unchanged() {
    let ssh = FakeSsh::new();
    run(&ssh.config, "::1:2200", "printf ok").await;
    let trace = ssh.trace();
    assert!(trace.contains("-T ::1:2200 bash -lc"));
    assert!(!trace.contains("-p 2200"));
}

#[tokio::test]
async fn passes_host_port_targets_through_ssh_p_with_the_config_file() {
    let ssh = FakeSsh::new();
    let config = ssh
        .config
        .clone()
        .with_config_file(Some("/tmp/symphony-test-ssh-config".into()));
    run(&config, "localhost:2222", "echo ready").await;
    let trace = ssh.trace();
    assert!(trace.contains("-F /tmp/symphony-test-ssh-config"));
    assert!(trace.contains("-T -p 2222 localhost bash -lc"));
    assert!(trace.contains("echo ready"));
}

#[tokio::test]
async fn keeps_the_user_prefix_when_parsing_user_host_port() {
    let ssh = FakeSsh::new();
    run(&ssh.config, "root@127.0.0.1:2200", "true").await;
    assert!(ssh.trace().contains("-T -p 2200 root@127.0.0.1 bash -lc"));
}

#[tokio::test]
async fn reports_spawn_failures_for_a_missing_executable() {
    let config = SshConfig::default().with_executable("/nonexistent/dir/ssh");
    let err = config
        .run(
            "host",
            "true",
            Duration::from_secs(1),
            &EnvPolicy::inherit(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SshRunError::Ssh(SshError::Spawn(_))));
}

#[tokio::test]
async fn remote_commands_are_bounded_and_their_ssh_process_is_killed() {
    let ssh = FakeSsh::new();
    let started = std::time::Instant::now();
    let err = ssh
        .config
        .run(
            "host",
            "sleep-forever",
            Duration::from_millis(200),
            &EnvPolicy::inherit(),
        )
        .await
        .unwrap_err();
    assert_eq!(err, SshRunError::Timeout);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn hook_env_policy_strips_secrets_from_the_local_ssh_process() {
    let ssh = FakeSsh::new();
    let policy = EnvPolicy::inherit().with_var("FAKE_SECRET", "s3cr3t");
    run_with(&ssh, &policy).await;
    assert!(ssh.trace().contains("ENV_SECRET:s3cr3t"));
    let ssh = FakeSsh::new();
    let policy = EnvPolicy::strip(["FAKE_SECRET"]).with_var("FAKE_SECRET", "s3cr3t");
    run_with(&ssh, &policy).await;
    assert!(ssh.trace().contains("ENV_SECRET:\n"));
}

async fn run_with(ssh: &FakeSsh, policy: &EnvPolicy) {
    ssh.config
        .run("host", "true", Duration::from_secs(5), policy)
        .await
        .unwrap();
}

#[tokio::test]
async fn launcher_builds_the_ssh_command_for_codex() {
    let ssh = FakeSsh::new();
    let launcher = SshLauncher::new(ssh.config.clone());
    let command = launcher
        .command("localhost:2222", "cd '/w' && exec codex app-server")
        .unwrap();
    let std = command.as_std();
    let args: Vec<String> = std
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "-T",
            "-p",
            "2222",
            "localhost",
            "bash -lc 'cd '\"'\"'/w'\"'\"' && exec codex app-server'"
        ]
    );
    let no_line_mode = SshLauncher::new(ssh.config.clone())
        .command("localhost", "x")
        .unwrap();
    let args: Vec<String> = no_line_mode
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(!args.iter().any(|a| a == "-F"));
    assert_eq!(&args[..2], ["-T", "localhost"]);
}
