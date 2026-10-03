//! Port of `gitlab_adapter_test.exs` and `gitlab_redirect_test.exs`.

mod support;

use std::sync::Arc;

use serde_json::{Value, json};
use support::*;
use symphony_core::MapEnv;
use symphony_core::TrackerConfigError as C;
use symphony_trackers::gitlab::{self, GitLabTracker};
use symphony_trackers::{Provider, Tracker, TrackerError, bind_agent_tools};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ORIGIN: &str = "https://gitlab.test";

fn tracker_json(overrides: Value) -> Value {
    let mut provider =
        json!({"project_path": "group/project", "api_key": "test-token", "api_url": ORIGIN});
    if let (Value::Object(base), Value::Object(extra)) = (&mut provider, overrides) {
        for (k, v) in extra {
            base.insert(k, v);
        }
    }
    json!({
        "kind": "gitlab",
        "provider": provider,
        "active_states": ["opened"],
        "terminal_states": ["closed"],
    })
}

fn settings(overrides: Value) -> symphony_core::config::TrackerSettings {
    tracker_settings(tracker_json(overrides))
}

fn raw_issue(iid: i64) -> Value {
    json!({
        "iid": iid,
        "id": 1000 + iid,
        "project_id": 77,
        "title": format!("Issue {iid}"),
        "description": format!(" Body {iid} "),
        "state": "opened",
        "web_url": format!("https://gitlab.test/group/project/-/issues/{iid}"),
        "assignees": [{"id": 99, "username": "octocat"}],
        "assignee": {"username": "octocat"},
        "labels": [" Bug ", "bug", "Platform"],
        "references": {"full": format!("group/project#{iid}")},
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T00:00:00Z",
    })
}

fn tracker_for(server: &MockServer) -> GitLabTracker {
    GitLabTracker::new(http_for(&[(ORIGIN, server)]), Arc::new(MapEnv::new()))
}

fn offline() -> GitLabTracker {
    let deps = offline_deps(MapEnv::new());
    GitLabTracker::new(deps.http, deps.env)
}

#[tokio::test]
async fn adapter_validates_gitlab_config_delegates_reads_and_advertises_gitlab_api() {
    let tracker = offline();
    assert_eq!(tracker.validate_config(&settings(json!({}))), Ok(()));
    let mut t = tracker_json(json!({}));
    t.as_object_mut().unwrap().remove("active_states");
    assert_eq!(
        tracker.validate_config(&tracker_settings(t)),
        Err(C::MissingGitlabActiveStates.into())
    );
    let mut t = tracker_json(json!({}));
    t.as_object_mut().unwrap().remove("terminal_states");
    assert_eq!(
        tracker.validate_config(&tracker_settings(t)),
        Err(C::MissingGitlabTerminalStates.into())
    );
    let mut t = tracker_json(json!({}));
    t["active_states"] = json!([]);
    t["terminal_states"] = json!([]);
    assert_eq!(tracker.validate_config(&tracker_settings(t)), Ok(()));
    for (active, terminal) in [
        (json!(["Todo"]), json!(["closed"])),
        (json!(["open"]), json!(["closed"])),
        (json!(["opened"]), json!(["opened"])),
    ] {
        let mut t = tracker_json(json!({}));
        t["active_states"] = active;
        t["terminal_states"] = terminal;
        assert_eq!(
            tracker.validate_config(&tracker_settings(t)),
            Err(C::InvalidGitlabStates.into())
        );
    }
    let specs = tracker.agent_tool_specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0]["name"], "gitlab_api");
    assert_eq!(
        specs[0]["inputSchema"]["properties"]["method"]["enum"],
        json!(["GET", "POST", "PUT", "DELETE"])
    );
    assert!(specs[0]["inputSchema"]["properties"]["query"].is_object());

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .mount(&server)
        .await;
    let (ok, _) = run_tool(
        &tracker_for(&server),
        "gitlab_api",
        json!({"method": "GET", "path": "/user"}),
        &settings(json!({})),
    )
    .await;
    assert!(ok);
}

#[tokio::test]
async fn client_validates_project_settings_and_declares_token_environments() {
    let tracker = offline();
    let check = |overrides: Value| tracker.validate_config(&settings(overrides));
    assert_eq!(
        check(json!({"project_path": 123})),
        Err(C::MissingGitlabProjectPath.into())
    );
    assert_eq!(
        check(json!({"project_path": "group / project"})),
        Err(C::InvalidGitlabProjectPath.into())
    );
    assert_eq!(
        check(json!({"api_key": 123})),
        Err(C::MissingGitlabApiKey.into())
    );
    assert_eq!(
        check(json!({"api_url": "not a url"})),
        Err(C::InvalidGitlabApiUrl.into())
    );
    assert_eq!(
        check(json!({"api_url": "http://gitlab.com/api/v4"})),
        Err(C::InvalidGitlabApiUrl.into())
    );
    assert_eq!(
        tracker.secret_environment_names(&settings(json!({"api_key": "$SYMPHONY_GITLAB_TOKEN"}))),
        strings(&[
            "GITLAB_PAT",
            "GITLAB_ACCESS_TOKEN",
            "GITLAB_TOKEN",
            "OAUTH_TOKEN",
            "SYMPHONY_GITLAB_TOKEN"
        ])
    );

    // Trimmed settings: the path is encoded as a whole and the token is trimmed.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/projects/group%2Fproject/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    let issues = tracker_for(&server)
        .fetch_issues_by_states(
            &settings(json!({"project_path": " group/project ", "api_key": " token "})),
            &strings(&["opened"]),
        )
        .await
        .unwrap();
    assert!(issues.is_empty());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        header_of(&requests[0], "authorization").as_deref(),
        Some("Bearer token")
    );
    assert_eq!(
        header_of(&requests[0], "accept").as_deref(),
        Some("application/json")
    );
    assert!(header_of(&requests[0], "private-token").is_none());
}

#[test]
fn client_normalizes_gitlab_issues_without_dropping_provider_details() {
    let issue = gitlab::normalize_issue(&raw_issue(42), "group/project").expect("valid");
    assert_eq!(issue.id.as_deref(), Some("42"));
    assert_eq!(issue.identifier.as_deref(), Some("GL-42"));
    assert_eq!(
        Value::Object(issue.native_ref.clone().unwrap()),
        json!({
            "id": 1042,
            "iid": 42,
            "project_id": 77,
            "project_path": "group/project",
            "references": {"full": "group/project#42"}
        })
    );
    assert_eq!(issue.title.as_deref(), Some("Issue 42"));
    assert_eq!(issue.description.as_deref(), Some("Body 42"));
    assert_eq!(issue.state.as_deref(), Some("opened"));
    assert_eq!(
        issue.url.as_deref(),
        Some("https://gitlab.test/group/project/-/issues/42")
    );
    assert_eq!(issue.assignee_id.as_deref(), Some("99"));
    assert_eq!(issue.labels, ["bug", "platform"]);
    assert!(issue.blocked_by.is_empty());
    assert!(issue.dispatchable);
    assert!(issue.created_at.is_some() && issue.updated_at.is_some());

    let mut closed = raw_issue(43);
    closed["state"] = json!("closed");
    closed["confidential"] = json!(true);
    closed["issue_type"] = json!("incident");
    assert!(
        gitlab::normalize_issue(&closed, "group/project")
            .unwrap()
            .dispatchable
    );

    let mut fallback = raw_issue(44);
    fallback["assignees"] = json!([]);
    fallback["assignee"] = json!({"username": "fallback-user"});
    assert_eq!(
        gitlab::normalize_issue(&fallback, "group/project")
            .unwrap()
            .assignee_id
            .as_deref(),
        Some("fallback-user")
    );

    // Improvement: an unassigned issue no longer reports its own id as the assignee.
    let mut unassigned = raw_issue(45);
    unassigned["assignees"] = json!([]);
    unassigned["assignee"] = Value::Null;
    assert_eq!(
        gitlab::normalize_issue(&unassigned, "group/project")
            .unwrap()
            .assignee_id,
        None
    );

    let mut blank = raw_issue(46);
    blank["title"] = json!(" ");
    assert!(gitlab::normalize_issue(&blank, "group/project").is_none());
    let mut no_description = raw_issue(47);
    no_description["description"] = json!("   ");
    assert_eq!(
        gitlab::normalize_issue(&no_description, "group/project")
            .unwrap()
            .description,
        None
    );
}

#[tokio::test]
async fn client_pages_state_reads_maps_gitlab_states_and_drops_malformed_records() {
    let server = MockServer::start().await;
    let page1: Vec<Value> = (1..=100)
        .map(|n| {
            let mut issue = raw_issue(n);
            match n {
                99 => issue["state"] = json!("closed"),
                100 => issue["title"] = json!(""),
                _ => {}
            }
            issue
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/projects/group%2Fproject/issues"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(page1)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/projects/group%2Fproject/issues"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([raw_issue(101)])))
        .expect(1)
        .mount(&server)
        .await;
    let (logs, _guard) = capture_logs();
    let issues = tracker_for(&server)
        .fetch_issues_by_states(&settings(json!({})), &strings(&["opened"]))
        .await
        .unwrap();
    assert_eq!(issues.len(), 99);
    assert_eq!(issues[0].id.as_deref(), Some("1"));
    assert_eq!(issues.last().unwrap().id.as_deref(), Some("101"));
    assert!(!ids(&issues).contains(&"99".to_string()));
    assert!(
        logs.contents()
            .contains("Dropping malformed GitLab issue records count=1")
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        query_of(&requests[0]),
        vec![
            ("order_by".to_string(), "created_at".to_string()),
            ("page".into(), "1".into()),
            ("per_page".into(), "100".into()),
            ("sort".into(), "asc".into()),
            ("state".into(), "opened".into()),
        ]
    );

    let all = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("state", "all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&all)
        .await;
    tracker_for(&all)
        .fetch_issues_by_states(&settings(json!({})), &strings(&["opened", "closed"]))
        .await
        .unwrap();

    let offline = offline();
    assert_eq!(
        offline
            .fetch_issues_by_states(&settings(json!({})), &strings(&["Todo"]))
            .await,
        Ok(vec![])
    );
    assert_eq!(
        offline
            .fetch_issues_by_states(&settings(json!({})), &[])
            .await,
        Ok(vec![])
    );
}

#[tokio::test]
async fn client_refreshes_iids_in_order_omits_404s_and_rejects_malformed_refreshes() {
    let server = MockServer::start().await;
    for n in [1, 2] {
        Mock::given(method("GET"))
            .and(path(format!("/projects/group%2Fproject/issues/{n}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(raw_issue(n)))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/projects/group%2Fproject/issues/404"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let mut blank = raw_issue(5);
    blank["title"] = json!("");
    Mock::given(method("GET"))
        .and(path("/projects/group%2Fproject/issues/5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(blank))
        .mount(&server)
        .await;
    let tracker = tracker_for(&server);
    let issues = tracker
        .fetch_issues_by_ids(&settings(json!({})), &strings(&["2", "1", "404", "2"]))
        .await
        .unwrap();
    assert_eq!(ids(&issues), ["2", "1"]);
    assert_eq!(
        tracker
            .fetch_issues_by_ids(&settings(json!({})), &strings(&["nope"]))
            .await,
        Err(TrackerError::InvalidIssueId(Provider::Gitlab))
    );
    assert_eq!(
        tracker
            .fetch_issues_by_ids(&settings(json!({})), &strings(&["5"]))
            .await,
        Err(TrackerError::UnknownPayload(Provider::Gitlab))
    );
    assert_eq!(
        offline()
            .fetch_issues_by_ids(&settings(json!({})), &[])
            .await,
        Ok(vec![])
    );
}

#[tokio::test]
async fn gitlab_api_preserves_rest_status_and_body_while_rejecting_unsafe_arguments() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/projects/group%2Fproject/issues/42/notes"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 7})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/missing"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "404 Not found"})))
        .mount(&server)
        .await;
    let tracker = tracker_for(&server);
    let s = settings(json!({}));
    let (ok, output) = run_tool(
        &tracker,
        "gitlab_api",
        json!({
            "method": "post",
            "path": " /projects/group%2Fproject/issues/42/notes ",
            "query": {"per_page": 10},
            "body": {"body": "hello"}
        }),
        &s,
    )
    .await;
    assert!(ok);
    assert_eq!(output, json!({"status": 201, "body": {"id": 7}}));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].url.path(),
        "/projects/group%2Fproject/issues/42/notes"
    );
    assert_eq!(
        query_of(&requests[0]),
        vec![("per_page".into(), "10".into())]
    );
    assert_eq!(json_body(&requests[0]), json!({"body": "hello"}));

    let (ok, output) = run_tool(
        &tracker,
        "gitlab_api",
        json!({"method": "GET", "path": "/missing"}),
        &s,
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"status": 404, "body": {"message": "404 Not found"}})
    );

    let before = server.received_requests().await.unwrap().len();
    for (args, message) in [
        (
            json!({"method": "GET", "path": "https://gitlab.com/api/v4/user"}),
            "gitlab_api.path must be a relative GitLab REST path.",
        ),
        (
            json!({"method": "PATCH", "path": "/user"}),
            "gitlab_api.method must be GET, POST, PUT, or DELETE.",
        ),
        (
            json!({"method": "GET", "path": "/user", "query": false}),
            "gitlab_api.query must be a JSON object when provided.",
        ),
        (
            json!({"path": "/user"}),
            "gitlab_api.method must be GET, POST, PUT, or DELETE.",
        ),
    ] {
        let (ok, output) = run_tool(&tracker, "gitlab_api", args, &s).await;
        assert!(!ok);
        assert_eq!(output, json!({"error": {"message": message}}));
    }
    assert_eq!(server.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn gitlab_api_reports_unsupported_tools_malformed_calls_and_client_failures() {
    let tracker = offline();
    let s = settings(json!({}));
    let (ok, output) = run_tool(&tracker, "not_gitlab_api", json!({}), &s).await;
    assert!(!ok);
    assert_eq!(output["error"]["supportedTools"], json!(["gitlab_api"]));
    let (ok, output) = run_tool(&tracker, "gitlab_api", json!("not-an-object"), &s).await;
    assert!(!ok);
    assert_eq!(
        output["error"]["message"],
        "gitlab_api expects an object with method and path."
    );
    let (ok, output) = run_tool(
        &tracker,
        "gitlab_api",
        json!({"method": "GET", "path": "/user"}),
        &settings(json!({"api_key": 123})),
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {"message": "Symphony is missing GitLab auth. Set tracker.provider.api_key or export GITLAB_PAT."}})
    );
    let (ok, output) = run_tool(
        &tracker,
        "gitlab_api",
        json!({"method": "GET", "path": "/user"}),
        &settings(json!({"project_path": 123})),
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {"message": "GitLab REST tool execution failed.", "reason": ":missing_gitlab_project_path"}})
    );
}

#[tokio::test]
async fn tracker_binds_gitlab_tools_and_token_env_names_from_provider_config() {
    let env = MapEnv::new().with("SYMPHONY_GITLAB_TOKEN_1", "secret-value");
    let workflow = settings_with_env(
        tracker_json(json!({"api_key": "$SYMPHONY_GITLAB_TOKEN_1"})),
        &env,
    );
    let deps = offline_deps(env);
    let binding = bind_agent_tools(&workflow, &deps).unwrap();
    assert_eq!(binding.kind(), "gitlab");
    assert_eq!(
        binding.secret_environment_names(),
        strings(&[
            "GITLAB_PAT",
            "GITLAB_ACCESS_TOKEN",
            "GITLAB_TOKEN",
            "OAUTH_TOKEN",
            "SYMPHONY_GITLAB_TOKEN_1"
        ])
    );
    assert_eq!(binding.tool_names(), ["gitlab_api"]);
    assert_eq!(
        symphony_trackers::validate_config(&workflow.tracker, &deps),
        Ok(())
    );
}

// ---- gitlab_redirect_test.exs ----

const TOKEN: &str = "redirect-test-token";
const BAIT_PATH: &str = "/projects/attacker%2Fbait/releases/v1/downloads/release.sha256";

async fn redirect_request(location: Option<&str>) -> (bool, MockServer, MockServer) {
    let gitlab = MockServer::start().await;
    let sink = MockServer::start().await;
    let mut first = ResponseTemplate::new(200);
    if let Some(location) = location {
        first = ResponseTemplate::new(302).insert_header("location", location);
    }
    Mock::given(path(BAIT_PATH))
        .respond_with(first.set_body_raw("{}", "application/json"))
        .mount(&gitlab)
        .await;
    for server in [&gitlab, &sink] {
        Mock::given(path("/sink"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
            .mount(server)
            .await;
    }
    let tracker = GitLabTracker::new(
        http_for(&[(ORIGIN, &gitlab), ("https://sink.test", &sink)]),
        Arc::new(MapEnv::new()),
    );
    let settings = tracker_settings(json!({
        "kind": "gitlab",
        "provider": {"api_url": ORIGIN, "api_key": TOKEN, "project_path": "test/repo"},
    }));
    let (ok, _) = run_tool(
        &tracker,
        "gitlab_api",
        json!({"method": "GET", "path": BAIT_PATH}),
        &settings,
    )
    .await;
    (ok, gitlab, sink)
}

#[tokio::test]
async fn direct_requests_authenticate_with_bearer_instead_of_private_token() {
    let (ok, gitlab, _sink) = redirect_request(None).await;
    assert!(ok);
    let requests = gitlab.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), BAIT_PATH);
    assert_eq!(
        header_of(&requests[0], "authorization"),
        Some(format!("Bearer {TOKEN}"))
    );
    assert!(header_of(&requests[0], "private-token").is_none());
}

#[tokio::test]
async fn same_origin_redirects_retain_authentication() {
    let (ok, gitlab, sink) = redirect_request(Some("/sink")).await;
    assert!(ok);
    let requests = gitlab.received_requests().await.unwrap();
    let sink_request = requests.iter().find(|r| r.url.path() == "/sink").unwrap();
    assert_eq!(
        header_of(sink_request, "authorization"),
        Some(format!("Bearer {TOKEN}"))
    );
    assert!(header_of(sink_request, "private-token").is_none());
    assert!(sink.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn cross_origin_redirects_receive_neither_credential_header() {
    let (ok, gitlab, sink) = redirect_request(Some("https://sink.test/sink")).await;
    assert!(ok);
    assert!(
        gitlab
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path() != "/sink")
    );
    let requests = sink.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/sink");
    assert!(header_of(&requests[0], "authorization").is_none());
    assert!(header_of(&requests[0], "private-token").is_none());
}
