//! Port of `jira_adapter_test.exs`.

mod support;

use std::sync::Arc;

use base64::Engine;
use serde_json::{Value, json};
use support::*;
use symphony_core::config::{JiraSettings, TrackerSettings, resolve_jira};
use symphony_core::{BlockerRef, MapEnv, TrackerConfigError as C};
use symphony_trackers::jira::{self, JiraTracker};
use symphony_trackers::{Provider, Tracker, TrackerError, bind_agent_tools};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ORIGIN: &str = "https://jira.example.test";

fn tracker_json(overrides: Value) -> Value {
    let mut provider = json!({
        "base_url": ORIGIN,
        "email": "agent@example.test",
        "api_token": "test-token",
        "project_key": "SYM",
    });
    if let (Value::Object(base), Value::Object(extra)) = (&mut provider, overrides) {
        for (k, v) in extra {
            base.insert(k, v);
        }
    }
    json!({
        "kind": "jira",
        "provider": provider,
        "active_states": ["To Do"],
        "terminal_states": ["Done"],
    })
}

fn settings(overrides: Value) -> TrackerSettings {
    tracker_settings(tracker_json(overrides))
}

fn resolved(tracker: &TrackerSettings) -> JiraSettings {
    resolve_jira(tracker, &MapEnv::new()).expect("valid jira settings")
}

fn raw_issue(id: &str, key: &str, state: &str, project: &str) -> Value {
    json!({
        "id": id,
        "key": key,
        "fields": {
            "summary": format!("Issue {key}"),
            "description": {"type": "doc", "version": 1, "content": [{"type": "paragraph", "content": [
                {"type": "text", "text": "First line"},
                {"type": "hardBreak"},
                {"type": "text", "text": "Second line"}
            ]}]},
            "status": {"name": state},
            "labels": [" Bug ", "bug", "Platform"],
            "assignee": {"accountId": "account-1"},
            "created": "2026-01-01T00:00:00.000+0000",
            "updated": "2026-01-02T00:00:00.000+0000",
            "project": {"key": project}
        }
    })
}

fn issue(id: &str, key: &str) -> Value {
    raw_issue(id, key, "To Do", "SYM")
}

fn linked_issue(id: &str, key: &str, state: &str) -> Value {
    json!({"id": id, "key": key, "fields": {"status": {"name": state}}})
}

fn tracker_for(server: &MockServer) -> JiraTracker {
    JiraTracker::new(http_for(&[(ORIGIN, server)]), Arc::new(MapEnv::new()))
}

fn offline() -> JiraTracker {
    let deps = offline_deps(MapEnv::new());
    JiraTracker::new(deps.http, deps.env)
}

#[tokio::test]
async fn adapter_validates_jira_config_delegates_reads_and_advertises_jira_rest() {
    let tracker = offline();
    assert_eq!(tracker.validate_config(&settings(json!({}))), Ok(()));
    let mut t = tracker_json(json!({}));
    t["active_states"] = json!([]);
    t["terminal_states"] = json!([]);
    assert_eq!(tracker.validate_config(&tracker_settings(t)), Ok(()));
    let mut t = tracker_json(json!({}));
    t.as_object_mut().unwrap().remove("active_states");
    assert_eq!(
        tracker.validate_config(&tracker_settings(t)),
        Err(C::MissingJiraActiveStates.into())
    );
    let mut t = tracker_json(json!({}));
    t.as_object_mut().unwrap().remove("terminal_states");
    assert_eq!(
        tracker.validate_config(&tracker_settings(t)),
        Err(C::MissingJiraTerminalStates.into())
    );
    let mut t = tracker_json(json!({}));
    t["active_states"] = json!([" "]);
    assert_eq!(
        tracker.validate_config(&tracker_settings(t)),
        Err(C::InvalidJiraStates.into())
    );
    let specs = tracker.agent_tool_specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0]["name"], "jira_rest");
    assert_eq!(
        specs[0]["description"],
        "Execute a Jira Cloud REST v3 request using Symphony's configured auth.\n"
    );
    assert_eq!(
        specs[0]["inputSchema"]["properties"]["path"]["description"],
        "Jira REST v3 path beginning with /rest/api/3/."
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/rest/api/3/myself"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"accountId": "user"})))
        .mount(&server)
        .await;
    let (ok, _) = run_tool(
        &tracker_for(&server),
        "jira_rest",
        json!({"method": "GET", "path": "/rest/api/3/myself"}),
        &settings(json!({})),
    )
    .await;
    assert!(ok);
    let request = &server.received_requests().await.unwrap()[0];
    let expected =
        base64::engine::general_purpose::STANDARD.encode("agent@example.test:test-token");
    assert_eq!(
        header_of(request, "authorization"),
        Some(format!("Basic {expected}"))
    );
    assert_eq!(
        header_of(request, "accept").as_deref(),
        Some("application/json")
    );
}

#[test]
fn client_validates_provider_settings_and_declares_token_environments() {
    let tracker = offline();
    let check = |overrides: Value| tracker.validate_config(&settings(overrides));
    assert_eq!(check(json!({})), Ok(()));
    assert_eq!(
        check(json!({"base_url": "http://jira.test"})),
        Err(C::InvalidJiraBaseUrl.into())
    );
    assert_eq!(
        check(json!({"base_url": "https://jira.test?x=1"})),
        Err(C::InvalidJiraBaseUrl.into())
    );
    assert_eq!(
        check(json!({"email": 123})),
        Err(C::MissingJiraEmail.into())
    );
    assert_eq!(
        check(json!({"api_token": 123})),
        Err(C::MissingJiraApiToken.into())
    );
    assert_eq!(
        check(json!({"project_key": 123})),
        Err(C::MissingJiraProjectKey.into())
    );
    assert_eq!(
        tracker.secret_environment_names(&settings(json!({"api_token": "$SYMPHONY_JIRA_TOKEN"}))),
        strings(&["JIRA_API_TOKEN", "SYMPHONY_JIRA_TOKEN"])
    );
}

#[test]
fn client_normalizes_jira_issue_fields_and_projects_adf_description_text() {
    let s = resolved(&settings(json!({})));
    let normalized = jira::normalize_issue(&issue("10001", "SYM-1"), &s).unwrap();
    assert_eq!(normalized.id.as_deref(), Some("10001"));
    assert_eq!(normalized.identifier.as_deref(), Some("SYM-1"));
    assert_eq!(normalized.native_ref, None);
    assert_eq!(normalized.title.as_deref(), Some("Issue SYM-1"));
    assert_eq!(
        normalized.description.as_deref(),
        Some("First line\nSecond line")
    );
    assert_eq!(normalized.priority, None);
    assert_eq!(normalized.state.as_deref(), Some("To Do"));
    assert_eq!(normalized.branch_name, None);
    assert_eq!(
        normalized.url.as_deref(),
        Some("https://jira.example.test/browse/SYM-1")
    );
    assert_eq!(normalized.assignee_id.as_deref(), Some("account-1"));
    assert_eq!(normalized.labels, ["bug", "platform"]);
    assert!(normalized.blocked_by.is_empty());
    assert!(normalized.dispatchable);
    assert_eq!(
        normalized.created_at.map(|d| d.to_rfc3339()),
        Some("2026-01-01T00:00:00+00:00".to_string())
    );
    assert!(normalized.updated_at.is_some());

    let mut empty = issue("10002", "SYM-2");
    empty["fields"]["summary"] = json!("");
    assert!(jira::normalize_issue(&empty, &s).is_none());

    let mut rich = issue("10003", "SYM-3");
    rich["fields"]["description"] = json!({"type": "doc", "content": [{"type": "paragraph", "content": [
        {"type": "mention", "attrs": {"text": "@Alex"}},
        {"type": "emoji", "attrs": {"shortName": ":wave:"}},
        {"type": "status", "attrs": {"text": "Ready"}},
        {"type": "inlineCard", "attrs": {"url": "https://example.test/card"}}
    ]}]});
    assert_eq!(
        jira::normalize_issue(&rich, &s)
            .unwrap()
            .description
            .as_deref(),
        Some("@Alex:wave:Readyhttps://example.test/card")
    );
    let mut panel = issue("10004", "SYM-4");
    panel["fields"]["description"] = json!({"type": "doc", "content": [{"type": "panel",
        "attrs": {"panelType": "info"},
        "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Panel text"}]}]}]});
    assert_eq!(
        jira::normalize_issue(&panel, &s)
            .unwrap()
            .description
            .as_deref(),
        Some("Panel text")
    );

    let lower = resolved(&settings(json!({"project_key": "sym"})));
    assert!(jira::normalize_issue(&issue("10001", "SYM-1"), &lower).is_some());
    let other = raw_issue("10005", "OTHER-5", "To Do", "OTHER");
    assert!(jira::normalize_issue(&other, &s).is_none());
    // Keys are percent-encoded in the browse URL.
    let odd = issue("10006", "SYM 6");
    assert_eq!(
        jira::normalize_issue(&odd, &s).unwrap().url.as_deref(),
        Some("https://jira.example.test/browse/SYM%206")
    );
}

#[test]
fn client_retains_inward_blocks_links_and_gates_only_ready_jira_issues() {
    let s = resolved(&settings(json!({})));
    let active = linked_issue("20001", "SYM-10", "In Progress");
    let terminal = linked_issue("20002", "OTHER-20", "Ready for Release");
    let mut base = issue("10001", "SYM-1");
    base["fields"]["issuelinks"] = json!([
        {"type": {"name": " Blocks "}, "inwardIssue": active},
        {"type": {"name": "blocks"}, "inwardIssue": terminal},
        {"type": {"name": "Relates"}, "inwardIssue": linked_issue("20003", "SYM-30", "In Progress")},
        {"type": {"name": "Blocks"}, "outwardIssue": linked_issue("20004", "SYM-40", "In Progress")}
    ]);
    let normalized = jira::normalize_issue(&base, &s).unwrap();
    let blocker = |id: &str, key: &str, state: &str| BlockerRef {
        id: Some(id.into()),
        identifier: Some(key.into()),
        state: Some(state.into()),
    };
    assert_eq!(
        normalized.blocked_by,
        vec![
            blocker("20001", "SYM-10", "In Progress"),
            blocker("20002", "OTHER-20", "Ready for Release")
        ]
    );
    assert!(!normalized.dispatchable);

    let mut open = base.clone();
    open["fields"]["status"] = json!({"name": "Open", "statusCategory": {"key": "new"}});
    assert!(!jira::normalize_issue(&open, &s).unwrap().dispatchable);

    let mut terminal_only = base.clone();
    terminal_only["fields"]["issuelinks"] =
        json!([{"type": {"name": "Blocks"}, "inwardIssue": terminal}]);
    let mut t = tracker_json(json!({}));
    t["terminal_states"] = json!(["Ready for Release"]);
    let release = resolved(&tracker_settings(t));
    assert!(
        jira::normalize_issue(&terminal_only, &release)
            .unwrap()
            .dispatchable
    );

    let mut unknown = base.clone();
    unknown["fields"]["status"]["name"] = json!("Todo");
    unknown["fields"]["issuelinks"] =
        json!([{"type": {"name": "Blocks"}, "inwardIssue": {"id": "20005"}}]);
    let normalized = jira::normalize_issue(&unknown, &s).unwrap();
    assert_eq!(
        normalized.blocked_by,
        vec![BlockerRef {
            id: Some("20005".into()),
            identifier: None,
            state: None
        }]
    );
    assert!(!normalized.dispatchable);

    let mut in_progress = base;
    in_progress["fields"]["status"] =
        json!({"name": "In Progress", "statusCategory": {"key": "indeterminate"}});
    assert!(
        jira::normalize_issue(&in_progress, &s)
            .unwrap()
            .dispatchable
    );
}

#[tokio::test]
async fn client_pages_enhanced_search_filters_states_and_drops_malformed_candidates() {
    let server = MockServer::start().await;
    let mut malformed = issue("10002", "SYM-2");
    malformed["fields"]["summary"] = json!("");
    Mock::given(method("POST"))
        .and(path("/rest/api/3/search/jql"))
        .and(body_partial_json(json!({"nextPageToken": "next-page"})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"issues": [issue("10004", "SYM-4")], "isLast": true})),
        )
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/rest/api/3/search/jql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issues": [issue("10001", "SYM-1"), malformed, raw_issue("10003", "SYM-3", "Done", "SYM")],
            "isLast": false,
            "nextPageToken": "next-page"
        })))
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;
    let (logs, _guard) = capture_logs();
    let issues = tracker_for(&server)
        .fetch_issues_by_states(&settings(json!({})), &strings(&["To Do"]))
        .await
        .unwrap();
    assert_eq!(ids(&issues), ["10001", "10004"]);
    assert!(
        logs.contents()
            .contains("Dropping malformed Jira issue records count=1")
    );
    let requests = server.received_requests().await.unwrap();
    let first = json_body(&requests[0]);
    assert_eq!(first["jql"], "project = \"SYM\" AND status IN (\"To Do\")");
    assert_eq!(first["maxResults"], 100);
    assert_eq!(first["fields"], json!(jira::ISSUE_FIELDS));
    assert!(first.get("nextPageToken").is_none());
    assert!(requests[0].url.query().is_none());
    assert_eq!(json_body(&requests[1])["nextPageToken"], "next-page");

    assert_eq!(
        offline()
            .fetch_issues_by_states(&settings(json!({})), &[])
            .await,
        Ok(vec![])
    );
}

#[tokio::test]
async fn client_errors_when_enhanced_search_pagination_omits_its_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"issues": [], "isLast": false})),
        )
        .mount(&server)
        .await;
    assert_eq!(
        tracker_for(&server)
            .fetch_issues_by_states(&settings(json!({})), &strings(&["To Do"]))
            .await,
        Err(TrackerError::MissingPageCursor(Provider::Jira))
    );
}

#[tokio::test]
async fn search_rejects_repeated_tokens_and_unknown_payloads() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"issues": [], "isLast": false, "nextPageToken": "again"})),
        )
        .mount(&server)
        .await;
    assert_eq!(
        tracker_for(&server)
            .fetch_issues_by_states(&settings(json!({})), &strings(&["To Do"]))
            .await,
        Err(TrackerError::PaginationRepeatedCursor(Provider::Jira))
    );
    let unknown = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"issues": []})))
        .mount(&unknown)
        .await;
    assert_eq!(
        tracker_for(&unknown)
            .fetch_issues_by_states(&settings(json!({})), &strings(&["To Do"]))
            .await,
        Err(TrackerError::UnknownPayload(Provider::Jira))
    );
}

#[tokio::test]
async fn client_refreshes_ids_in_batches_preserves_order_and_omits_missing_scope() {
    let requested: Vec<String> = (1..=101).map(|n| n.to_string()).collect();
    let server = MockServer::start().await;
    for batch in requested.chunks(100) {
        let issues: Vec<Value> = batch
            .iter()
            .rev()
            .map(|id| issue(id, &format!("SYM-{id}")))
            .collect();
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/bulkfetch"))
            .and(body_partial_json(json!({"issueIdsOrKeys": batch})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"issues": issues})))
            .expect(1)
            .mount(&server)
            .await;
    }
    let issues = tracker_for(&server)
        .fetch_issues_by_ids(&settings(json!({})), &requested)
        .await
        .unwrap();
    assert_eq!(ids(&issues), requested);
    for request in server.received_requests().await.unwrap() {
        let body = json_body(&request);
        assert!(
            body["fields"]
                .as_array()
                .unwrap()
                .contains(&json!("issuelinks"))
        );
    }

    let scoped = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issues": [issue("1", "SYM-1"), raw_issue("2", "OTHER-2", "To Do", "OTHER"), issue("999", "SYM-999")],
            "issueErrors": []
        })))
        .mount(&scoped)
        .await;
    let issues = tracker_for(&scoped)
        .fetch_issues_by_ids(&settings(json!({})), &strings(&["1", "2", "404"]))
        .await
        .unwrap();
    assert_eq!(ids(&issues), ["1"]);

    let broken = MockServer::start().await;
    let mut malformed = issue("1", "SYM-1");
    malformed["fields"]["summary"] = json!("");
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"issues": [malformed]})))
        .mount(&broken)
        .await;
    assert_eq!(
        tracker_for(&broken)
            .fetch_issues_by_ids(&settings(json!({})), &strings(&["1"]))
            .await,
        Err(TrackerError::UnknownPayload(Provider::Jira))
    );
    assert_eq!(
        offline()
            .fetch_issues_by_ids(&settings(json!({})), &[])
            .await,
        Ok(vec![])
    );
}

#[tokio::test]
async fn client_refreshes_jira_blockers_before_dispatch() {
    let server = MockServer::start().await;
    let mut blocked = issue("10001", "SYM-1");
    blocked["fields"]["issuelinks"] = json!([
        {"type": {"name": "Blocks"}, "inwardIssue": linked_issue("20001", "SYM-10", "In Progress")}
    ]);
    Mock::given(method("POST"))
        .and(path("/rest/api/3/issue/bulkfetch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"issues": [blocked]})))
        .mount(&server)
        .await;
    let issues = tracker_for(&server)
        .fetch_issues_by_ids(&settings(json!({})), &strings(&["10001"]))
        .await
        .unwrap();
    assert_eq!(
        issues[0].blocked_by,
        vec![BlockerRef {
            id: Some("20001".into()),
            identifier: Some("SYM-10".into()),
            state: Some("In Progress".into())
        }]
    );
    assert!(!issues[0].dispatchable);
}

#[tokio::test]
async fn jira_status_errors_are_logged_and_never_retried_for_post() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let (logs, _guard) = capture_logs();
    assert_eq!(
        tracker_for(&server)
            .fetch_issues_by_states(&settings(json!({})), &strings(&["To Do"]))
            .await,
        Err(TrackerError::ApiStatus {
            provider: Provider::Jira,
            status: 503
        })
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert!(
        logs.contents()
            .contains("Jira API request failed status=503 method=POST path=/rest/api/3/search/jql")
    );
}

#[tokio::test]
async fn jira_rest_preserves_rest_status_and_body_while_rejecting_unsafe_arguments() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rest/api/3/issue/10001/comment"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "comment-1"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest/api/3/issue/404"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"errorMessages": ["nope"]})))
        .mount(&server)
        .await;
    let tracker = tracker_for(&server);
    let s = settings(json!({}));
    let (ok, output) = run_tool(
        &tracker,
        "jira_rest",
        json!({
            "method": "post",
            "path": " /rest/api/3/issue/10001/comment ",
            "query": {"expand": "renderedBody"},
            "body": {"body": {"type": "doc"}}
        }),
        &s,
    )
    .await;
    assert!(ok);
    assert_eq!(output, json!({"status": 201, "body": {"id": "comment-1"}}));
    let request = &server.received_requests().await.unwrap()[0];
    assert_eq!(request.method.as_str(), "POST");
    assert_eq!(
        query_of(request),
        vec![("expand".into(), "renderedBody".into())]
    );
    assert_eq!(json_body(request), json!({"body": {"type": "doc"}}));

    let (ok, output) = run_tool(
        &tracker,
        "jira_rest",
        json!({"method": "GET", "path": "/rest/api/3/issue/404"}),
        &s,
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"status": 404, "body": {"errorMessages": ["nope"]}})
    );

    let before = server.received_requests().await.unwrap().len();
    for (args, message) in [
        (
            json!({"method": "GET", "path": "https://jira.test/rest/api/3/myself"}),
            "jira_rest.path must begin with /rest/api/3/.",
        ),
        (
            json!({"method": "GET", "path": "/rest/api/2/myself"}),
            "jira_rest.path must begin with /rest/api/3/.",
        ),
        (
            json!({"method": "GET", "path": "/rest/api/3/myself", "query": false}),
            "jira_rest.query must be a JSON object when provided.",
        ),
        (
            json!({"path": "/rest/api/3/myself"}),
            "jira_rest.method must be GET, POST, PUT, or DELETE.",
        ),
        (
            json!({"method": "PATCH", "path": "/rest/api/3/myself"}),
            "jira_rest.method must be GET, POST, PUT, or DELETE.",
        ),
    ] {
        let (ok, output) = run_tool(&tracker, "jira_rest", args, &s).await;
        assert!(!ok);
        assert_eq!(output, json!({"error": {"message": message}}));
    }
    assert_eq!(server.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn jira_rest_reports_unsupported_tools_malformed_calls_and_client_failures() {
    let tracker = offline();
    let s = settings(json!({}));
    let (ok, output) = run_tool(&tracker, "not_jira_rest", json!({}), &s).await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {"message": "Unsupported dynamic tool: \"not_jira_rest\".", "supportedTools": ["jira_rest"]}})
    );
    let result = tracker.execute_agent_tool(None, &json!({}), &ctx(&s)).await;
    assert!(result.output.contains("Unsupported dynamic tool: nil."));
    let (ok, output) = run_tool(&tracker, "jira_rest", json!("not-an-object"), &s).await;
    assert!(!ok);
    assert_eq!(
        output["error"]["message"],
        "jira_rest expects an object with method and path."
    );
    let (ok, _) = run_tool(
        &tracker,
        "jira_rest",
        json!({"method": "GET", "path": 123}),
        &s,
    )
    .await;
    assert!(!ok);
    let (ok, output) = run_tool(
        &tracker,
        "jira_rest",
        json!({"method": "GET", "path": "/rest/api/3/myself"}),
        &settings(json!({"api_token": 123})),
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {"message": "Symphony is missing Jira auth. Set tracker.provider.api_token or export JIRA_API_TOKEN."}})
    );
    let (ok, output) = run_tool(
        &tracker,
        "jira_rest",
        json!({"method": "GET", "path": "/rest/api/3/myself"}),
        &settings(json!({"base_url": "http://jira.test"})),
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {"message": "Jira REST tool execution failed.", "reason": ":invalid_jira_base_url"}})
    );
}

#[tokio::test]
async fn tracker_binds_jira_tools_and_token_env_names_from_provider_config() {
    let env = MapEnv::new().with("SYMPHONY_JIRA_TOKEN_1", "test-token");
    let workflow = settings_with_env(
        tracker_json(json!({"api_token": "$SYMPHONY_JIRA_TOKEN_1"})),
        &env,
    );
    let deps = offline_deps(env);
    let binding = bind_agent_tools(&workflow, &deps).unwrap();
    assert_eq!(binding.kind(), "jira");
    assert_eq!(
        binding.secret_environment_names(),
        strings(&["JIRA_API_TOKEN", "SYMPHONY_JIRA_TOKEN_1"])
    );
    assert_eq!(binding.tool_names(), ["jira_rest"]);
    assert_eq!(
        symphony_trackers::validate_config(&workflow.tracker, &deps),
        Ok(())
    );
}
