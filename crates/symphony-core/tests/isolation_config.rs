//! The `isolation:` section: casting, defaults, errors and the egress allow-list.

use serde_json::{Value, json};
use symphony_core::config::{
    self, GitHubAppSettings, HostPort, IsolationSettings, NetworkMode, Settings, TaskUids,
};
use symphony_core::{ConfigError, MapEnv};

fn parse(value: Value, env: &MapEnv) -> Result<Settings, ConfigError> {
    let Value::Object(map) = value else {
        panic!("expected an object")
    };
    config::parse(&map, env)
}

fn isolation(value: Value) -> IsolationSettings {
    parse(json!({ "isolation": value }), &MapEnv::new())
        .unwrap()
        .isolation
        .expect("isolation section")
}

fn invalid(value: Value) -> String {
    match parse(json!({ "isolation": value }), &MapEnv::new()) {
        Err(ConfigError::InvalidWorkflowConfig(message)) => message,
        other => panic!("expected invalid config, got {other:?}"),
    }
}

#[test]
fn isolation_is_absent_unless_configured_and_denies_the_network_by_default() {
    let settings = parse(json!({}), &MapEnv::new()).unwrap();
    assert_eq!(settings.isolation, None);

    let empty = isolation(json!({}));
    assert_eq!(empty.task_uids, None);
    assert_eq!(empty.github_app, None);
    assert_eq!(empty.network.mode, NetworkMode::Deny);
    assert!(empty.network.denies());
    assert!(empty.network.allow.is_empty());
}

#[test]
fn every_part_is_cast_with_defaults() {
    let settings = isolation(json!({
        "task_uids": {"first": 200000, "count": "1000"},
        "github_app": {"app_id": 123456, "private_key_file": "/run/secrets/app.pem"},
        "network": {"mode": "allow", "allow": ["PyPI.org:443", "[::1]:8080"]},
    }));
    assert_eq!(
        settings.task_uids,
        Some(TaskUids {
            first: 200_000,
            count: 1000
        })
    );
    assert_eq!(
        settings.github_app,
        Some(GitHubAppSettings {
            app_id: "123456".into(),
            private_key_file: "/run/secrets/app.pem".into(),
            api_url: "https://api.github.com".into(),
            git_host: "github.com".into(),
        })
    );
    assert_eq!(settings.network.mode, NetworkMode::Allow);
    assert_eq!(
        settings.network.allow,
        vec![HostPort::new("pypi.org", 443), HostPort::new("::1", 8080)]
    );

    let uids = json!({"first": 200000, "count": 10});
    let custom = isolation(json!({"task_uids": uids, "github_app": {
        "app_id": "Iv1.abc", "private_key_file": "/k.pem",
        "api_url": "https://ghe.example/api/v3/", "git_host": "GHE.example",
    }}));
    let app = custom.github_app.unwrap();
    assert_eq!(app.app_id, "Iv1.abc");
    assert_eq!(app.api_url, "https://ghe.example/api/v3");
    assert_eq!(app.git_host, "ghe.example");

    // Plain http only to a loopback host (local mocks).
    let local = isolation(json!({"task_uids": uids, "github_app": {
        "app_id": 1, "private_key_file": "/k.pem", "api_url": "http://127.0.0.1:8080",
    }}));
    assert_eq!(local.github_app.unwrap().api_url, "http://127.0.0.1:8080");
}

#[test]
fn a_github_app_needs_task_uids() {
    // Without its own uid, a task's agent could read the App key Symphony reads.
    let message = invalid(json!({"github_app": {"app_id": 1, "private_key_file": "/k.pem"}}));
    assert!(
        message.contains("isolation.github_app needs isolation.task_uids"),
        "{message}"
    );
}

#[test]
fn bad_values_are_reported_by_path() {
    let message = invalid(json!({
        "task_uids": {"first": 0, "count": -3},
        "github_app": {"app_id": 0, "private_key_file": "relative.pem",
            "api_url": "http://api.github.com", "git_host": "https://github.com"},
        "network": {"mode": "maybe", "allow": ["pypi.org"]},
    }));
    for fragment in [
        "isolation.task_uids.first must be greater than 0",
        "isolation.task_uids.count must be greater than 0",
        "isolation.github_app.app_id is invalid",
        "isolation.github_app.private_key_file must be an absolute path",
        "isolation.github_app.api_url is invalid",
        "isolation.github_app.git_host is invalid",
        "isolation.network.mode is invalid",
        "isolation.network.allow is invalid",
    ] {
        assert!(message.contains(fragment), "{fragment}: {message}");
    }

    let message = invalid(json!({"task_uids": {}, "github_app": {}}));
    for fragment in [
        "isolation.task_uids.first can't be blank",
        "isolation.task_uids.count can't be blank",
        "isolation.github_app.app_id can't be blank",
        "isolation.github_app.private_key_file can't be blank",
    ] {
        assert!(message.contains(fragment), "{fragment}: {message}");
    }

    let message = invalid(json!({"task_uids": {"first": 4_294_967_000_u64, "count": 1000}}));
    assert!(
        message.contains("isolation.task_uids.count is too large for first"),
        "{message}"
    );
    let message = invalid(json!({"task_uids": {"first": 5_000_000_000_u64, "count": 1}}));
    assert!(
        message.contains("isolation.task_uids.first is invalid"),
        "{message}"
    );
    for (value, fragment) in [
        (json!("on"), "isolation is invalid"),
        (json!({"task_uids": 5}), "isolation.task_uids is invalid"),
        (json!({"network": []}), "isolation.network is invalid"),
    ] {
        let message = invalid(value);
        assert!(message.contains(fragment), "{fragment}: {message}");
    }
}

#[test]
fn a_github_app_task_may_also_reach_the_api_of_its_git_host() {
    let settings = parse(
        json!({"isolation": {"task_uids": {"first": 200000, "count": 10}, "github_app": {
            "app_id": 7, "private_key_file": "/run/secrets/app.pem",
            "api_url": "https://ghe.example.com/api/v3", "git_host": "ghe.example.com",
        }}}),
        &MapEnv::new(),
    )
    .unwrap();
    let isolation = settings.isolation.as_ref().unwrap();
    assert_eq!(
        isolation.egress_allow_list(Some("https://ghe.example.com/acme/widgets"), None),
        vec![HostPort::new("ghe.example.com", 443)],
        "on GHE the API shares the git host"
    );
    let settings = parse(
        json!({"isolation": {"task_uids": {"first": 200000, "count": 10},
            "github_app": {"app_id": 7, "private_key_file": "/run/secrets/app.pem"}}}),
        &MapEnv::new(),
    )
    .unwrap();
    let isolation = settings.isolation.as_ref().unwrap();
    assert_eq!(
        isolation.egress_allow_list(Some("https://github.com/acme/widgets"), None),
        vec![
            HostPort::new("github.com", 443),
            HostPort::new("api.github.com", 443)
        ]
    );
    // A repo on a host the operator did not allow is not added (and is refused at preflight).
    assert!(
        isolation
            .egress_allow_list(Some("https://gitlab.com/acme/widgets"), None)
            .is_empty()
    );
}

#[test]
fn only_operator_allowed_git_hosts_are_reachable() {
    let settings = parse(
        json!({"isolation": {"task_uids": {"first": 200000, "count": 10},
            "network": {"git_hosts": ["GitHub.com", "git.internal"]}}}),
        &MapEnv::new(),
    )
    .unwrap();
    let isolation = settings.isolation.as_ref().unwrap();
    // A listed host is reachable and allowed.
    assert_eq!(
        isolation.egress_allow_list(Some("https://github.com/a/b"), None),
        vec![HostPort::new("github.com", 443)]
    );
    assert!(
        isolation
            .check_repo_host(Some("https://github.com/a/b"))
            .is_ok()
    );
    assert!(
        isolation
            .check_repo_host(Some("ssh://git@git.internal/a/b"))
            .is_ok()
    );
    // The submitter cannot pick another host: refused, and not on the allow-list.
    assert!(
        isolation
            .egress_allow_list(Some("https://evil.example/a/b"), None)
            .is_empty()
    );
    assert_eq!(
        isolation.check_repo_host(Some("https://evil.example/a/b")),
        Err("evil.example".to_owned())
    );
    assert_eq!(
        isolation.check_repo_host(Some("https://10.0.0.5/a/b")),
        Err("10.0.0.5".to_owned())
    );
    // allow mode does not restrict egress, so it does not restrict the repo host.
    let open = parse(
        json!({"isolation": {"task_uids": {"first": 200000, "count": 10},
            "network": {"mode": "allow"}}}),
        &MapEnv::new(),
    )
    .unwrap();
    assert!(
        open.isolation
            .unwrap()
            .check_repo_host(Some("https://anywhere.example/a/b"))
            .is_ok()
    );
}

#[test]
fn the_allow_list_is_the_git_host_the_gateway_and_the_extras() {
    let env = MapEnv::new()
        .with("LLM_GATEWAY_URL", "http://Gateway.internal:8080/")
        .with("LLM_MODEL", "claude-sonnet");
    let settings = parse(
        json!({"isolation": {"network": {
            "git_hosts": ["github.com"], "allow": ["pypi.org:443", "github.com:443"]}}}),
        &env,
    )
    .unwrap();
    let isolation = settings.isolation.as_ref().unwrap();
    assert_eq!(
        isolation.egress_allow_list(
            Some("git@github.com:acme/widgets.git"),
            settings.llm.as_ref()
        ),
        vec![
            HostPort::new("github.com", 443),
            HostPort::new("gateway.internal", 8080),
            HostPort::new("pypi.org", 443),
        ]
    );
    assert_eq!(
        isolation.egress_allow_list(None, None),
        vec![
            HostPort::new("pypi.org", 443),
            HostPort::new("github.com", 443)
        ]
    );
}
