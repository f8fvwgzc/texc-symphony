//! Port of `github_adapter_test.exs` (HTTP through wiremock instead of `request_fun` injection).

mod support;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use support::*;
use symphony_core::MapEnv;
use symphony_core::TrackerConfigError as C;
use symphony_trackers::github::{self, GitHubTracker};
use symphony_trackers::{
    HttpClient, Provider, ReqwestTransport, Tracker, TrackerError, bind_agent_tools,
};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ORIGIN: &str = "https://github.test";

fn tracker_json(overrides: Value) -> Value {
    let mut provider = json!({"repo": "octo/repo", "token": "test-token", "api_url": ORIGIN});
    if let (Value::Object(base), Value::Object(extra)) = (&mut provider, overrides) {
        for (k, v) in extra {
            base.insert(k, v);
        }
    }
    json!({
        "kind": "github",
        "provider": provider,
        "active_states": ["open"],
        "terminal_states": ["closed"],
    })
}

fn settings(overrides: Value) -> symphony_core::config::TrackerSettings {
    tracker_settings(tracker_json(overrides))
}

fn raw_issue(n: i64) -> Value {
    json!({
        "number": n,
        "id": 1000 + n,
        "node_id": format!("I_{n}"),
        "title": format!("Issue {n}"),
        "body": format!("Body {n}"),
        "state": "open",
        "html_url": format!("https://github.test/octo/repo/issues/{n}"),
        "assignee": {"login": "octocat"},
        "labels": [{"name": " Bug "}, {"name": "bug"}, {"name": "Platform"}],
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T00:00:00Z",
    })
}

fn tracker_for(server: &MockServer) -> GitHubTracker {
    GitHubTracker::new(http_for(&[(ORIGIN, server)]), Arc::new(MapEnv::new()))
}

fn offline() -> GitHubTracker {
    let deps = offline_deps(MapEnv::new());
    GitHubTracker::new(deps.http, deps.env)
}

#[tokio::test]
async fn adapter_validates_github_config_delegates_reads_and_advertises_github_api() {
    let tracker = offline();
    assert_eq!(tracker.validate_config(&settings(json!({}))), Ok(()));

    let mut t = tracker_json(json!({}));
    t.as_object_mut().unwrap().remove("active_states");
    assert_eq!(
        tracker.validate_config(&tracker_settings(t)),
        Err(C::MissingGithubActiveStates.into())
    );
    let mut t = tracker_json(json!({}));
    t.as_object_mut().unwrap().remove("terminal_states");
    assert_eq!(
        tracker.validate_config(&tracker_settings(t)),
        Err(C::MissingGithubTerminalStates.into())
    );
    let mut t = tracker_json(json!({}));
    t["active_states"] = json!([]);
    t["terminal_states"] = json!([]);
    assert_eq!(tracker.validate_config(&tracker_settings(t)), Ok(()));
    for (active, terminal) in [
        (json!(["Todo"]), json!(["closed"])),
        (json!(["closed"]), json!(["closed"])),
        (json!(["open"]), json!(["open"])),
    ] {
        let mut t = tracker_json(json!({}));
        t["active_states"] = active;
        t["terminal_states"] = terminal;
        assert_eq!(
            tracker.validate_config(&tracker_settings(t)),
            Err(C::InvalidGithubStates.into())
        );
    }

    let specs = tracker.agent_tool_specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0]["name"], "github_api");
    assert_eq!(
        specs[0]["description"],
        "Execute a GitHub REST API request using Symphony's configured auth.\n"
    );
    assert_eq!(
        specs[0]["inputSchema"]["required"],
        json!(["method", "path"])
    );
    assert_eq!(
        specs[0]["inputSchema"]["properties"]["method"]["enum"],
        json!(["GET", "POST", "PATCH", "PUT", "DELETE"])
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "octocat"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/repo/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([raw_issue(1)])))
        .mount(&server)
        .await;
    let tracker = tracker_for(&server);
    let issues = tracker
        .fetch_issues_by_states(&settings(json!({})), &strings(&["open"]))
        .await
        .unwrap();
    assert_eq!(ids(&issues), ["1"]);
    let (ok, _) = run_tool(
        &tracker,
        "github_api",
        json!({"method": "GET", "path": "/user"}),
        &settings(json!({})),
    )
    .await;
    assert!(ok);
}

#[tokio::test]
async fn client_validates_repository_settings_and_declares_token_environments() {
    let tracker = offline();
    let check = |overrides: Value| tracker.validate_config(&settings(overrides));
    assert_eq!(
        check(json!({"repo": 123})),
        Err(C::MissingGithubRepo.into())
    );
    assert_eq!(
        check(json!({"repo": "not-a-repo"})),
        Err(C::InvalidGithubRepo.into())
    );
    assert_eq!(
        check(json!({"token": 123})),
        Err(C::MissingGithubToken.into())
    );
    assert_eq!(
        check(json!({"api_url": "not a url"})),
        Err(C::InvalidGithubApiUrl.into())
    );
    assert_eq!(
        check(json!({"api_url": "http://api.github.com"})),
        Err(C::InvalidGithubApiUrl.into())
    );
    // Reads fail with the same reason before any request.
    assert_eq!(
        tracker
            .fetch_issues_by_ids(&settings(json!({"token": 123})), &strings(&["1"]))
            .await,
        Err(C::MissingGithubToken.into())
    );
    assert_eq!(
        tracker.secret_environment_names(&settings(json!({"token": "$SYMPHONY_GITHUB_TOKEN"}))),
        strings(&[
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "GITHUB_ENTERPRISE_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "SYMPHONY_GITHUB_TOKEN"
        ])
    );
}

#[test]
fn client_normalizes_github_issues_without_dropping_provider_details() {
    let issue = github::normalize_issue(&raw_issue(42), "octo/repo").expect("valid");
    assert_eq!(issue.id.as_deref(), Some("42"));
    assert_eq!(issue.identifier.as_deref(), Some("GH-42"));
    assert_eq!(
        Value::Object(issue.native_ref.clone().unwrap()),
        json!({"id": 1042, "node_id": "I_42", "number": 42, "repo": "octo/repo"})
    );
    assert_eq!(issue.title.as_deref(), Some("Issue 42"));
    assert_eq!(issue.description.as_deref(), Some("Body 42"));
    assert_eq!(issue.priority, None);
    assert_eq!(issue.state.as_deref(), Some("open"));
    assert_eq!(issue.branch_name, None);
    assert_eq!(
        issue.url.as_deref(),
        Some("https://github.test/octo/repo/issues/42")
    );
    assert_eq!(issue.assignee_id.as_deref(), Some("octocat"));
    assert_eq!(issue.labels, ["bug", "platform"]);
    assert!(issue.blocked_by.is_empty());
    assert!(issue.dispatchable);
    assert!(issue.created_at.is_some() && issue.updated_at.is_some());

    let mut pr = raw_issue(43);
    pr["pull_request"] = Value::Null;
    assert!(
        !github::normalize_issue(&pr, "octo/repo")
            .unwrap()
            .dispatchable
    );

    let mut blank = raw_issue(44);
    blank["title"] = json!(" ");
    assert!(github::normalize_issue(&blank, "octo/repo").is_none());
    let mut zero = raw_issue(45);
    zero["number"] = json!(0);
    assert!(github::normalize_issue(&zero, "octo/repo").is_none());
    let mut bare = raw_issue(46);
    bare["labels"] = json!(["Ops", {"name": 5}, 7]);
    assert_eq!(
        github::normalize_issue(&bare, "octo/repo").unwrap().labels,
        ["ops"]
    );
}

#[tokio::test]
async fn client_pages_state_reads_filters_requested_states_and_drops_malformed_records() {
    let server = MockServer::start().await;
    let page1: Vec<Value> = (1..=100)
        .map(|n| {
            let mut issue = raw_issue(n);
            match n {
                98 => issue["pull_request"] = json!({"url": "x"}),
                99 => issue["state"] = json!("closed"),
                100 => issue["title"] = json!(" "),
                _ => {}
            }
            issue
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/repos/octo/repo/issues"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(page1)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/repo/issues"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([raw_issue(101)])))
        .expect(1)
        .mount(&server)
        .await;

    let (logs, _guard) = capture_logs();
    let tracker = tracker_for(&server);
    let issues = tracker
        .fetch_issues_by_states(&settings(json!({})), &strings(&[" OPEN "]))
        .await
        .unwrap();
    assert_eq!(issues.len(), 99);
    assert_eq!(issues.first().unwrap().id.as_deref(), Some("1"));
    assert_eq!(issues.last().unwrap().id.as_deref(), Some("101"));
    assert!(!ids(&issues).contains(&"99".to_string()));
    let pr = issues
        .iter()
        .find(|i| i.id.as_deref() == Some("98"))
        .unwrap();
    assert!(!pr.dispatchable);
    assert!(
        logs.contents()
            .contains("Dropping malformed GitHub issue records count=1")
    );

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        query_of(&requests[0]),
        vec![
            ("direction".to_string(), "asc".to_string()),
            ("page".into(), "1".into()),
            ("per_page".into(), "100".into()),
            ("sort".into(), "created".into()),
            ("state".into(), "open".into()),
        ]
    );
    assert_eq!(
        header_of(&requests[0], "authorization").as_deref(),
        Some("Bearer test-token")
    );
    assert_eq!(
        header_of(&requests[0], "accept").as_deref(),
        Some("application/vnd.github+json")
    );
    assert_eq!(
        header_of(&requests[0], "x-github-api-version").as_deref(),
        Some("2022-11-28")
    );
    assert_eq!(
        header_of(&requests[0], "user-agent").as_deref(),
        Some("symphony")
    );

    // Unsupported or empty state lists return before settings validation and without a request.
    let offline = offline();
    assert_eq!(
        offline
            .fetch_issues_by_states(&settings(json!({"token": 123})), &strings(&["In Progress"]))
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
async fn state_query_maps_open_and_closed_to_all() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/repo/issues"))
        .and(query_param("state", "all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    let issues = tracker_for(&server)
        .fetch_issues_by_states(&settings(json!({})), &strings(&["open", "Closed"]))
        .await
        .unwrap();
    assert!(issues.is_empty());
}

#[tokio::test]
async fn client_refreshes_numeric_ids_in_order_omits_404s_and_rejects_malformed_refreshes() {
    let server = MockServer::start().await;
    for n in [1, 2] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/octo/repo/issues/{n}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(raw_issue(n)))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/repos/octo/repo/issues/404"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
        .expect(1)
        .mount(&server)
        .await;
    let mut blank = raw_issue(5);
    blank["title"] = json!("");
    Mock::given(method("GET"))
        .and(path("/repos/octo/repo/issues/5"))
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
            .fetch_issues_by_ids(&settings(json!({})), &strings(&["not-a-number"]))
            .await,
        Err(TrackerError::InvalidIssueId(Provider::Github))
    );
    assert_eq!(
        tracker
            .fetch_issues_by_ids(&settings(json!({})), &strings(&["+5"]))
            .await,
        Err(TrackerError::UnknownPayload(Provider::Github))
    );
    assert_eq!(
        offline()
            .fetch_issues_by_ids(&settings(json!({})), &[])
            .await,
        Ok(vec![])
    );
}

#[tokio::test]
async fn non_success_statuses_are_logged_and_mapped() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/repo/issues/7"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    let (logs, _guard) = capture_logs();
    let result = tracker_for(&server)
        .fetch_issues_by_ids(&settings(json!({})), &strings(&["7"]))
        .await;
    assert_eq!(
        result,
        Err(TrackerError::ApiStatus {
            provider: Provider::Github,
            status: 403
        })
    );
    assert!(logs.contents().contains(
        "GitHub API request failed status=403 method=GET path=/repos/octo/repo/issues/7"
    ));
}

#[tokio::test]
async fn github_api_preserves_rest_status_and_body_while_rejecting_unsafe_arguments() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octo/repo/issues/42/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 9})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/repo/issues/404"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
        .mount(&server)
        .await;
    let tracker = tracker_for(&server);
    let s = settings(json!({}));

    let (ok, output) = run_tool(
        &tracker,
        "github_api",
        json!({
            "method": "post",
            "path": " /repos/octo/repo/issues/42/comments ",
            "params": {"per_page": 10},
            "body": {"body": "hello"}
        }),
        &s,
    )
    .await;
    assert!(ok);
    assert_eq!(output, json!({"status": 201, "body": {"id": 9}}));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method.as_str(), "POST");
    assert_eq!(
        requests[0].url.path(),
        "/repos/octo/repo/issues/42/comments"
    );
    assert_eq!(
        query_of(&requests[0]),
        vec![("per_page".into(), "10".into())]
    );
    assert_eq!(json_body(&requests[0]), json!({"body": "hello"}));
    assert_eq!(
        header_of(&requests[0], "content-type").as_deref(),
        Some("application/json")
    );

    let (ok, output) = run_tool(
        &tracker,
        "github_api",
        json!({"method": "GET", "path": "/repos/octo/repo/issues/404"}),
        &s,
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"status": 404, "body": {"message": "Not Found"}})
    );

    let before = server.received_requests().await.unwrap().len();
    for (args, message) in [
        (
            json!({"method": "GET", "path": "https://api.github.com/user"}),
            "`github_api.path` must be a relative GitHub REST path.",
        ),
        (
            json!({"method": "GET", "path": "/user", "params": false}),
            "`github_api.params` must be a JSON object when provided.",
        ),
        (
            json!({"method": "GET", "path": "/user", "params": {"ids": [1, 2]}}),
            "`github_api.params` must be a JSON object when provided.",
        ),
        (
            json!({"path": "/user"}),
            "`github_api.method` must be GET, POST, PATCH, PUT, or DELETE.",
        ),
    ] {
        let (ok, output) = run_tool(&tracker, "github_api", args, &s).await;
        assert!(!ok);
        assert_eq!(output, json!({"error": {"message": message}}));
    }
    assert_eq!(server.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn github_api_reports_unsupported_tools_malformed_calls_and_client_failures() {
    let tracker = offline();
    let s = settings(json!({}));
    let (ok, output) = run_tool(&tracker, "not_github_api", json!({}), &s).await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {
            "message": "Unsupported dynamic tool: \"not_github_api\".",
            "supportedTools": ["github_api"]
        }})
    );
    let (ok, output) = run_tool(&tracker, "github_api", json!("not-an-object"), &s).await;
    assert!(!ok);
    assert_eq!(
        output["error"]["message"],
        "`github_api` expects an object with `method` and `path`."
    );
    let (ok, _) = run_tool(
        &tracker,
        "github_api",
        json!({"method": "GET", "path": 123}),
        &s,
    )
    .await;
    assert!(!ok);

    let (ok, output) = run_tool(
        &tracker,
        "github_api",
        json!({"method": "GET", "path": "/user"}),
        &settings(json!({"token": 123})),
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {"message": "Symphony is missing GitHub auth. Set `tracker.provider.token` in `WORKFLOW.md` or export `GITHUB_TOKEN`."}})
    );
    let (ok, output) = run_tool(
        &tracker,
        "github_api",
        json!({"method": "GET", "path": "/user"}),
        &settings(json!({"repo": 123})),
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {"message": "GitHub API tool execution failed.", "reason": ":missing_github_repo"}})
    );

    // Transport timeout (POST: not retried) and a non-JSON body.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(800)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/text"))
        .respond_with(ResponseTemplate::new(200).set_body_string("plain text"))
        .mount(&server)
        .await;
    let transport =
        ReqwestTransport::with_timeouts(Duration::from_secs(5), Duration::from_millis(150))
            .unwrap()
            .with_origin_override(ORIGIN, &server.uri())
            .unwrap();
    let slow = GitHubTracker::new(
        HttpClient::new(Arc::new(transport)).with_retry_policy(fast_retry()),
        Arc::new(MapEnv::new()),
    );
    let (ok, output) = run_tool(
        &slow,
        "github_api",
        json!({"method": "POST", "path": "/slow"}),
        &s,
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {
            "message": "GitHub API request failed before receiving a successful response.",
            "reason": ":timeout"
        }})
    );
    let (ok, output) = run_tool(
        &slow,
        "github_api",
        json!({"method": "GET", "path": "/text"}),
        &s,
    )
    .await;
    assert!(ok);
    assert_eq!(output, json!({"status": 200, "body": "plain text"}));
}

#[tokio::test]
async fn tracker_binds_github_tools_and_token_env_names_from_provider_config() {
    let env = MapEnv::new().with("SYMPHONY_GITHUB_TOKEN_1", "secret-value");
    let workflow = settings_with_env(
        tracker_json(json!({"token": "$SYMPHONY_GITHUB_TOKEN_1"})),
        &env,
    );
    let deps = offline_deps(env);
    let binding = bind_agent_tools(&workflow, &deps).unwrap();
    assert_eq!(binding.kind(), "github");
    assert_eq!(
        binding.secret_environment_names(),
        strings(&[
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "GITHUB_ENTERPRISE_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "SYMPHONY_GITHUB_TOKEN_1"
        ])
    );
    assert_eq!(binding.tool_names(), ["github_api"]);
    assert_eq!(
        symphony_trackers::validate_config(&workflow.tracker, &deps),
        Ok(())
    );
}
