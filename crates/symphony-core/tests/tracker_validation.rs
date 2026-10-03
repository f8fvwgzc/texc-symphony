//! Per-tracker preflight validation, `$VAR` rules and secret env names (adapter `validate_config/1`,
//! `settings/1` and `secret_environment_names/1`).

use serde_json::{Value, json};
use symphony_core::config::{
    self, Settings, resolve_asana, resolve_github, resolve_gitlab, resolve_jira, resolve_linear,
    secret_environment_names,
};
use symphony_core::error::TrackerConfigError as E;
use symphony_core::{ConfigError, MapEnv};

fn settings(tracker: Value, env: &MapEnv) -> Settings {
    let Value::Object(map) = json!({ "tracker": tracker }) else {
        unreachable!()
    };
    config::parse(&map, env).expect("parses")
}

fn validate(tracker: Value, env: &MapEnv) -> Result<(), ConfigError> {
    config::validate_settings(&settings(tracker, env), env)
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn memory_needs_no_configuration() {
    assert_eq!(validate(json!({"kind": "memory"}), &MapEnv::new()), Ok(()));
    let s = settings(json!({"kind": "memory"}), &MapEnv::new());
    assert!(secret_environment_names(&s.tracker).is_empty());
}

#[test]
fn kinds_are_case_sensitive() {
    assert_eq!(
        validate(json!({"kind": "Linear"}), &MapEnv::new()),
        Err(ConfigError::UnsupportedTrackerKind("Linear".into()))
    );
}

#[test]
fn linear_resolution_and_checks() {
    let env = MapEnv::new().with("LINEAR_API_KEY", "env-key");
    let s = settings(json!({"kind": "linear", "project_slug": "proj"}), &env);
    let linear = resolve_linear(&s.tracker).unwrap();
    assert_eq!(linear.endpoint, "https://api.linear.app/graphql");
    assert_eq!(linear.api_key, "env-key");
    assert_eq!(linear.project_slug, "proj");
    assert_eq!(linear.assignee, None);
    assert!(!format!("{linear:?}").contains("env-key"));

    assert_eq!(
        validate(
            json!({"kind": "linear", "endpoint": "", "project_slug": "p"}),
            &env
        ),
        Err(E::InvalidLinearEndpoint.into())
    );
    assert_eq!(
        validate(
            json!({"kind": "linear", "project_slug": "p"}),
            &MapEnv::new()
        ),
        Err(E::MissingLinearApiToken.into())
    );
    assert_eq!(
        validate(
            json!({"kind": "linear", "project_slug": "p", "assignee": ""}),
            &env
        ),
        Ok(()),
        "an empty assignee normalizes to nil"
    );
    assert_eq!(
        validate(
            json!({"kind": "linear", "project_slug": "p", "assignee": " "}),
            &env
        ),
        Err(E::InvalidLinearAssignee.into())
    );
}

#[test]
fn github_states_must_be_open_and_closed() {
    let env = MapEnv::new()
        .with("GITHUB_TOKEN", "t")
        .with("GITHUB_REPO", "o/r");
    assert_eq!(
        validate(json!({"kind": "github"}), &env),
        Err(E::MissingGithubActiveStates.into())
    );
    assert_eq!(
        validate(json!({"kind": "github", "active_states": [" Open "]}), &env),
        Err(E::MissingGithubTerminalStates.into())
    );
    assert_eq!(
        validate(
            json!({"kind": "github", "active_states": ["todo"], "terminal_states": ["closed"]}),
            &env
        ),
        Err(E::InvalidGithubStates.into())
    );
    assert_eq!(
        validate(
            json!({"kind": "github", "active_states": ["open"], "terminal_states": ["done"]}),
            &env
        ),
        Err(E::InvalidGithubStates.into())
    );
    assert_eq!(
        validate(
            json!({"kind": "github", "active_states": ["Open"], "terminal_states": ["CLOSED"]}),
            &env
        ),
        Ok(())
    );
}

#[test]
fn github_settings_resolution() {
    let base = |provider: Value| json!({"kind": "github", "active_states": ["open"], "terminal_states": ["closed"], "provider": provider});
    let empty = MapEnv::new();

    assert_eq!(
        validate(base(json!({"api_url": "http://x"})), &empty),
        Err(E::InvalidGithubApiUrl.into())
    );
    assert_eq!(
        validate(base(json!({"api_url": 5})), &empty),
        Err(E::InvalidGithubApiUrl.into())
    );
    assert_eq!(
        validate(base(json!({})), &empty),
        Err(E::MissingGithubRepo.into())
    );
    assert_eq!(
        validate(base(json!({"repo": "nope"})), &empty),
        Err(E::InvalidGithubRepo.into())
    );
    assert_eq!(
        validate(base(json!({"repo": "o/r"})), &empty),
        Err(E::MissingGithubToken.into())
    );

    let env = MapEnv::new()
        .with("GITHUB_REPO", "env/repo")
        .with("GITHUB_TOKEN", "env-token")
        .with("MY_TOKEN", "  custom-token  ")
        .with("EMPTY_TOKEN", "");
    let s = settings(
        base(json!({"api_url": "https://ghe.example.com/api/v3/"})),
        &env,
    );
    let gh = resolve_github(&s.tracker, &env).unwrap();
    assert_eq!(gh.api_url, "https://ghe.example.com/api/v3");
    assert_eq!(gh.repo, "env/repo");
    assert_eq!(gh.token, "env-token");
    assert!(!format!("{gh:?}").contains("env-token"));

    let s = settings(base(json!({"repo": " o/r ", "token": "$MY_TOKEN"})), &env);
    let gh = resolve_github(&s.tracker, &env).unwrap();
    assert_eq!(gh.api_url, "https://api.github.com");
    assert_eq!(gh.repo, "o/r");
    assert_eq!(gh.token, "custom-token");
    assert_eq!(
        secret_environment_names(&s.tracker),
        strings(&[
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "GITHUB_ENTERPRISE_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "MY_TOKEN"
        ])
    );
    assert_eq!(
        s.secret_environment_names(),
        secret_environment_names(&s.tracker)
    );

    // An empty env value does not fall back to GITHUB_TOKEN; an invalid reference resolves to nothing.
    let s = settings(base(json!({"repo": "o/r", "token": "$EMPTY_TOKEN"})), &env);
    assert_eq!(resolve_github(&s.tracker, &env), Err(E::MissingGithubToken));
    let s = settings(base(json!({"repo": "o/r", "token": "$bad-name"})), &env);
    assert_eq!(resolve_github(&s.tracker, &env), Err(E::MissingGithubToken));
    // An unset reference falls back to GITHUB_TOKEN.
    let s = settings(base(json!({"repo": "o/r", "token": "$UNSET_TOKEN"})), &env);
    assert_eq!(resolve_github(&s.tracker, &env).unwrap().token, "env-token");
}

#[test]
fn gitlab_validation() {
    let base = |provider: Value| json!({"kind": "gitlab", "active_states": ["opened"], "terminal_states": ["closed"], "provider": provider});
    let empty = MapEnv::new();
    assert_eq!(
        validate(
            json!({"kind": "gitlab", "active_states": ["open"], "terminal_states": ["closed"]}),
            &empty
        ),
        Err(E::InvalidGitlabStates.into())
    );
    assert_eq!(
        validate(
            json!({"kind": "gitlab", "active_states": ["opened"]}),
            &empty
        ),
        Err(E::MissingGitlabTerminalStates.into())
    );
    assert_eq!(
        validate(base(json!({"api_url": "ftp://x"})), &empty),
        Err(E::InvalidGitlabApiUrl.into())
    );
    assert_eq!(
        validate(base(json!({})), &empty),
        Err(E::MissingGitlabProjectPath.into())
    );
    assert_eq!(
        validate(base(json!({"project_path": "group/my project"})), &empty),
        Err(E::InvalidGitlabProjectPath.into())
    );
    assert_eq!(
        validate(base(json!({"project_path": "g/p"})), &empty),
        Err(E::MissingGitlabApiKey.into())
    );

    let env = MapEnv::new()
        .with("GITLAB_PROJECT_PATH", "group/sub/proj")
        .with("GITLAB_PAT", "pat");
    let s = settings(base(json!({"api_key": "$GL_KEY"})), &env);
    let gl = resolve_gitlab(&s.tracker, &env).unwrap();
    assert_eq!(gl.api_url, "https://gitlab.com/api/v4");
    assert_eq!(gl.project_path, "group/sub/proj");
    assert_eq!(gl.api_key, "pat");
    assert_eq!(
        secret_environment_names(&s.tracker),
        strings(&[
            "GITLAB_PAT",
            "GITLAB_ACCESS_TOKEN",
            "GITLAB_TOKEN",
            "OAUTH_TOKEN",
            "GL_KEY"
        ])
    );
}

#[test]
fn jira_validation() {
    let base = |provider: Value| json!({"kind": "jira", "active_states": ["To Do"], "terminal_states": [" Done ", "Won't Do"], "provider": provider});
    let empty = MapEnv::new();
    assert_eq!(
        validate(json!({"kind": "jira"}), &empty),
        Err(E::MissingJiraActiveStates.into())
    );
    assert_eq!(
        validate(
            json!({"kind": "jira", "active_states": ["To Do", " "], "terminal_states": ["Done"]}),
            &empty
        ),
        Err(E::InvalidJiraStates.into())
    );
    assert_eq!(
        validate(json!({"kind": "jira", "active_states": ["To Do"]}), &empty),
        Err(E::MissingJiraTerminalStates.into())
    );
    assert_eq!(
        validate(base(json!({})), &empty),
        Err(E::InvalidJiraBaseUrl.into())
    );
    assert_eq!(
        validate(
            base(json!({"base_url": "https://x.atlassian.net?a=1"})),
            &empty
        ),
        Err(E::InvalidJiraBaseUrl.into())
    );
    assert_eq!(
        validate(base(json!({"base_url": "https://x.atlassian.net"})), &empty),
        Err(E::MissingJiraEmail.into())
    );
    assert_eq!(
        validate(
            base(json!({"base_url": "https://x.atlassian.net", "email": "a@b"})),
            &empty
        ),
        Err(E::MissingJiraApiToken.into())
    );
    assert_eq!(
        validate(
            base(json!({"base_url": "https://x.atlassian.net", "email": "a@b", "api_token": "t"})),
            &empty
        ),
        Err(E::MissingJiraProjectKey.into())
    );

    let env = MapEnv::new()
        .with("JIRA_BASE_URL", "https://acme.atlassian.net/")
        .with("JIRA_EMAIL", "bot@acme.test")
        .with("JIRA_API_TOKEN", "jira-token");
    let s = settings(base(json!({"project_key": "SYM"})), &env);
    let jira = resolve_jira(&s.tracker, &env).unwrap();
    assert_eq!(jira.base_url, "https://acme.atlassian.net");
    assert_eq!(jira.email, "bot@acme.test");
    assert_eq!(jira.api_token, "jira-token");
    assert_eq!(jira.project_key, "SYM");
    assert_eq!(jira.terminal_states, strings(&["done", "won't do"]));
    assert!(!format!("{jira:?}").contains("jira-token"));
    assert_eq!(
        secret_environment_names(&s.tracker),
        strings(&["JIRA_API_TOKEN"])
    );
}

#[test]
fn asana_validation() {
    let base = |provider: Value| json!({"kind": "asana", "active_states": ["Doing"], "terminal_states": ["Done"], "provider": provider});
    let empty = MapEnv::new();
    assert_eq!(
        validate(json!({"kind": "asana", "active_states": ["Doing"]}), &empty),
        Err(E::MissingAsanaTerminalStates.into())
    );
    assert_eq!(
        validate(
            json!({"kind": "asana", "active_states": [""], "terminal_states": ["Done"]}),
            &empty
        ),
        Err(E::InvalidAsanaStates.into())
    );
    assert_eq!(
        validate(base(json!({"endpoint": "http://x"})), &empty),
        Err(E::InvalidAsanaEndpoint.into())
    );
    assert_eq!(
        validate(base(json!({})), &empty),
        Err(E::MissingAsanaApiKey.into())
    );
    assert_eq!(
        validate(base(json!({"api_key": "k"})), &empty),
        Err(E::MissingAsanaProjectGid.into())
    );

    let env = MapEnv::new().with("ASANA_PAT", "pat").with("PROJ", "12345");
    let s = settings(
        base(json!({"project_gid": "$PROJ", "api_key": "$ASANA_ALT"})),
        &env,
    );
    let asana = resolve_asana(&s.tracker, &env).unwrap();
    assert_eq!(asana.endpoint, "https://app.asana.com/api/1.0");
    assert_eq!(asana.api_key, "pat");
    assert_eq!(asana.project_gid, "12345");
    assert_eq!(
        secret_environment_names(&s.tracker),
        strings(&["ASANA_PAT", "ASANA_ALT"])
    );
}
