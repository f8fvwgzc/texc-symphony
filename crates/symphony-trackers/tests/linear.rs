//! Port of the Linear parts of `dynamic_tool_test.exs`, `extensions_test.exs`,
//! `workspace_and_config_test.exs` and `core_test.exs`.

mod support;

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{Value, json};
use support::*;
use symphony_core::config::TrackerSettings;
use symphony_core::{BlockerRef, MapEnv, TrackerConfigError as C};
use symphony_trackers::linear::{self, LinearTracker};
use symphony_trackers::{
    HttpClient, Provider, ToolBinding, Tracker, TrackerError, bind_agent_tools,
};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ORIGIN: &str = "https://linear.test";

fn linear_settings(extra: Value) -> TrackerSettings {
    let mut tracker = json!({
        "kind": "linear",
        "endpoint": "https://linear.test/graphql",
        "api_key": "token",
        "project_slug": "project",
    });
    if let (Value::Object(base), Value::Object(extra)) = (&mut tracker, extra) {
        for (k, v) in extra {
            base.insert(k, v);
        }
    }
    tracker_settings(tracker)
}

fn tracker_for(server: &MockServer) -> LinearTracker {
    LinearTracker::new(http_for(&[(ORIGIN, server)]))
}

fn offline() -> LinearTracker {
    LinearTracker::new(offline_deps(MapEnv::new()).http)
}

fn node(id: &str, identifier: &str, state: &str) -> Value {
    json!({
        "id": id,
        "identifier": identifier,
        "title": format!("Title {identifier}"),
        "state": {"name": state},
        "assignee": {"id": "user-1"},
        "labels": {"nodes": []},
        "inverseRelations": {"nodes": []},
    })
}

fn page(nodes: Vec<Value>, has_next: bool, cursor: Option<&str>) -> Value {
    json!({"data": {"issues": {"nodes": nodes, "pageInfo": {"hasNextPage": has_next, "endCursor": cursor}}}})
}

fn terminal() -> HashSet<String> {
    ["closed", "cancelled", "canceled", "duplicate", "done"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn filter(id: &str) -> HashSet<String> {
    HashSet::from([id.to_string()])
}

// ---- dynamic_tool_test.exs ----

#[test]
fn tool_specs_advertises_the_linear_graphql_input_contract() {
    let specs = offline().agent_tool_specs();
    assert_eq!(specs.len(), 1);
    let spec = &specs[0];
    assert_eq!(spec["name"], "linear_graphql");
    assert!(spec["description"].as_str().unwrap().contains("Linear"));
    assert_eq!(spec["inputSchema"]["type"], "object");
    assert_eq!(spec["inputSchema"]["required"], json!(["query"]));
    let props = spec["inputSchema"]["properties"].as_object().unwrap();
    assert_eq!(props.keys().collect::<Vec<_>>(), ["query", "variables"]);
}

#[tokio::test]
async fn unsupported_tools_return_a_failure_payload_with_the_supported_tool_list() {
    let tracker = offline();
    let (ok, output) = run_tool(
        &tracker,
        "not_a_real_tool",
        json!({}),
        &linear_settings(json!({})),
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {
            "message": "Unsupported dynamic tool: \"not_a_real_tool\".",
            "supportedTools": ["linear_graphql"]
        }})
    );
}

#[tokio::test]
async fn bound_tools_keep_the_adapter_and_auth_snapshot_from_session_startup() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": {"viewer": {"id": "usr_bound"}}})),
        )
        .mount(&server)
        .await;
    let deps = deps_for(MapEnv::new(), &[(ORIGIN, &server)]);
    let session = settings_with_env(
        json!({
            "kind": "linear",
            "endpoint": "https://linear.test/graphql",
            "api_key": "session-token",
            "project_slug": "session-project",
        }),
        &MapEnv::new(),
    );
    let binding = bind_agent_tools(&session, &deps).unwrap();
    assert_eq!(binding.kind(), "linear");

    // The workflow reloads as memory: a new binding has no tools, the old one keeps Linear.
    let reloaded = settings_with_env(json!({"kind": "memory"}), &MapEnv::new());
    assert!(
        bind_agent_tools(&reloaded, &deps)
            .unwrap()
            .tool_specs()
            .is_empty()
    );

    let result = binding
        .execute(
            Some("linear_graphql"),
            &json!({"query": "query Viewer { viewer { id } }"}),
            None,
        )
        .await;
    assert!(result.success);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        header_of(&requests[0], "authorization").as_deref(),
        Some("session-token")
    );
    assert_eq!(
        json_body(&requests[0]),
        json!({"query": "query Viewer { viewer { id } }", "variables": {}})
    );
    assert_eq!(
        binding.settings().project_slug.as_deref(),
        Some("session-project")
    );
}

async fn graphql_server(response: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn linear_graphql_returns_successful_graphql_responses_as_tool_text() {
    let body = json!({"data": {"viewer": {"id": "usr_123"}}});
    let server = graphql_server(body.clone()).await;
    let result = tracker_for(&server)
        .execute_agent_tool(
            Some("linear_graphql"),
            &json!({"query": "query Viewer { viewer { id } }", "variables": {"includeTeams": false}}),
            &ctx(&linear_settings(json!({}))),
        )
        .await;
    assert!(result.success);
    assert_eq!(serde_json::from_str::<Value>(&result.output).unwrap(), body);
    assert_eq!(result.output, serde_json::to_string_pretty(&body).unwrap());
    assert_eq!(result.content_items[0].text, result.output);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        json_body(&requests[0]),
        json!({"query": "query Viewer { viewer { id } }", "variables": {"includeTeams": false}})
    );
}

#[tokio::test]
async fn linear_graphql_accepts_raw_strings_ignores_operation_name_and_passes_multi_operation_documents()
 {
    let server = graphql_server(json!({
        "errors": [{"message": "Must provide operation name if query contains multiple operations."}]
    }))
    .await;
    let tracker = tracker_for(&server);
    let s = linear_settings(json!({}));

    let (ok, _) = run_tool(
        &tracker,
        "linear_graphql",
        json!("  query Viewer { viewer { id } }  "),
        &s,
    )
    .await;
    assert!(!ok, "an errors body is a failure");
    let (_, _) = run_tool(
        &tracker,
        "linear_graphql",
        json!({"query": "query Viewer { viewer { id } }", "operationName": "Viewer"}),
        &s,
    )
    .await;
    let document = "query Viewer { viewer { id } }\nquery Teams { teams { nodes { id } } }\n";
    let (ok, _) = run_tool(&tracker, "linear_graphql", json!({"query": document}), &s).await;
    assert!(!ok);

    let bodies: Vec<Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(json_body)
        .collect();
    assert_eq!(
        bodies,
        vec![
            json!({"query": "query Viewer { viewer { id } }", "variables": {}}),
            json!({"query": "query Viewer { viewer { id } }", "variables": {}}),
            json!({"query": document.trim(), "variables": {}}),
        ]
    );
}

#[tokio::test]
async fn linear_graphql_validates_arguments_before_calling_linear() {
    let tracker = offline();
    let s = linear_settings(json!({}));
    let missing =
        json!({"error": {"message": "`linear_graphql` requires a non-empty `query` string."}});
    for args in [
        json!("   "),
        json!({"variables": {"x": 1}}),
        json!({"query": "   "}),
    ] {
        let (ok, output) = run_tool(&tracker, "linear_graphql", args, &s).await;
        assert!(!ok);
        assert_eq!(output, missing);
    }
    let (ok, output) = run_tool(&tracker, "linear_graphql", json!(["bad"]), &s).await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {"message": "`linear_graphql` expects either a GraphQL query string or an object with `query` and optional `variables`."}})
    );
    let (ok, output) = run_tool(
        &tracker,
        "linear_graphql",
        json!({"query": "query { viewer { id } }", "variables": ["bad"]}),
        &s,
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {"message": "`linear_graphql.variables` must be a JSON object when provided."}})
    );
}

#[tokio::test]
async fn linear_graphql_marks_graphql_error_responses_as_failures_while_preserving_the_body() {
    let body = json!({"data": null, "errors": [{"message": "Unknown field `nope`"}]});
    let server = graphql_server(body.clone()).await;
    let (ok, output) = run_tool(
        &tracker_for(&server),
        "linear_graphql",
        json!({"query": "{ nope }"}),
        &linear_settings(json!({})),
    )
    .await;
    assert!(!ok);
    assert_eq!(output, body);
}

#[tokio::test]
async fn linear_graphql_formats_transport_and_auth_failures() {
    // Missing token: the auth message (Elixir surfaced a transport message here; see migration notes).
    let mut no_key = linear_settings(json!({}));
    no_key.api_key = None;
    let (ok, output) = run_tool(
        &offline(),
        "linear_graphql",
        json!({"query": "{ viewer { id } }"}),
        &no_key,
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {"message": "Symphony is missing Linear auth. Set `tracker.provider.api_key` in `WORKFLOW.md` or export `LINEAR_API_KEY`."}})
    );

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
        .mount(&server)
        .await;
    let (ok, output) = run_tool(
        &tracker_for(&server),
        "linear_graphql",
        json!({"query": "{ viewer { id } }"}),
        &linear_settings(json!({})),
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {"message": "Linear GraphQL request failed with HTTP 503.", "status": 503}})
    );
    // POST is never retried.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    let slow = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(800)))
        .mount(&slow)
        .await;
    let transport = symphony_trackers::ReqwestTransport::with_timeouts(
        std::time::Duration::from_secs(5),
        std::time::Duration::from_millis(100),
    )
    .unwrap()
    .with_origin_override(ORIGIN, &slow.uri())
    .unwrap();
    let tracker = LinearTracker::new(HttpClient::new(Arc::new(transport)));
    let (ok, output) = run_tool(
        &tracker,
        "linear_graphql",
        json!({"query": "{ viewer { id } }"}),
        &linear_settings(json!({})),
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {
            "message": "Linear GraphQL request failed before receiving a successful response.",
            "reason": ":timeout"
        }})
    );
}

#[tokio::test]
async fn linear_graphql_formats_unexpected_failures_and_non_json_payloads() {
    // An endpoint that is not a URL is an unexpected (non-transport-response) failure path.
    let tracker = offline();
    let mut bad = linear_settings(json!({}));
    bad.endpoint = Some("not a url".into());
    let (ok, output) = run_tool(&tracker, "linear_graphql", json!({"query": "{ x }"}), &bad).await;
    assert!(!ok);
    assert_eq!(
        output["error"]["message"],
        "Linear GraphQL request failed before receiving a successful response."
    );
    assert_eq!(output["error"]["reason"], "{:invalid_url, \"not a url\"}");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let result = tracker_for(&server)
        .execute_agent_tool(
            Some("linear_graphql"),
            &json!({"query": "{ x }"}),
            &ctx(&linear_settings(json!({}))),
        )
        .await;
    assert!(result.success);
    assert_eq!(result.output, "\"ok\"");
}

// ---- extensions_test.exs ----

#[tokio::test]
async fn linear_adapter_delegates_reads_and_advertises_its_native_agent_tool() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"variables": {"stateNames": ["Todo"]}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![node("issue-1", "MT-1", "Todo")],
            false,
            None,
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"variables": {"ids": ["issue-1"]}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": {"issues": {"nodes": [node("issue-1", "MT-1", "Todo")]}}}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let tracker = tracker_for(&server);
    let s = linear_settings(json!({}));
    assert_eq!(
        ids(&tracker
            .fetch_issues_by_states(&s, &strings(&["Todo"]))
            .await
            .unwrap()),
        ["issue-1"]
    );
    assert_eq!(
        ids(&tracker
            .fetch_issues_by_ids(&s, &strings(&["issue-1"]))
            .await
            .unwrap()),
        ["issue-1"]
    );
    assert_eq!(tracker.agent_tool_specs()[0]["name"], "linear_graphql");
}

// ---- workspace_and_config_test.exs ----

#[test]
fn linear_client_normalizes_blockers_from_inverse_relations() {
    let raw = json!({
        "id": "issue-1",
        "identifier": "MT-1",
        "title": "Blocked todo",
        "description": "Needs dependency",
        "priority": 2,
        "state": {"name": "Todo"},
        "branchName": "mt-1",
        "url": "https://example.org/issues/MT-1",
        "assignee": {"id": "user-1"},
        "labels": {"nodes": [{"name": "Backend"}, {"name": " backend "}, {"name": " "}]},
        "inverseRelations": {"nodes": [
            {"type": "blocks", "issue": {"id": "issue-2", "identifier": "MT-2", "state": {"name": "In Progress"}}},
            {"type": "relatesTo", "issue": {"id": "issue-3", "identifier": "MT-3", "state": {"name": "Done"}}}
        ]},
        "createdAt": "2026-01-01T00:00:00Z",
        "updatedAt": "2026-01-02T00:00:00Z"
    });
    let issue = linear::normalize_issue(&raw, Some(&filter("user-1")), &terminal()).unwrap();
    assert_eq!(
        issue.blocked_by,
        vec![BlockerRef {
            id: Some("issue-2".into()),
            identifier: Some("MT-2".into()),
            state: Some("In Progress".into()),
        }]
    );
    assert_eq!(issue.labels, ["backend"]);
    assert_eq!(issue.native_ref, None);
    assert_eq!(issue.priority, Some(2));
    assert_eq!(issue.state.as_deref(), Some("Todo"));
    assert_eq!(issue.branch_name.as_deref(), Some("mt-1"));
    assert_eq!(issue.assignee_id.as_deref(), Some("user-1"));
    assert!(issue.created_at.is_some());
    assert!(!issue.dispatchable, "Todo with a non-terminal blocker");

    // Terminal blockers and non-Todo states do not block.
    let mut done_blocker = raw.clone();
    done_blocker["inverseRelations"]["nodes"][0]["issue"]["state"]["name"] = json!(" Done ");
    assert!(
        linear::normalize_issue(&done_blocker, Some(&filter("user-1")), &terminal())
            .unwrap()
            .dispatchable
    );
    let mut in_progress = raw.clone();
    in_progress["state"]["name"] = json!("In Progress");
    assert!(
        linear::normalize_issue(&in_progress, None, &terminal())
            .unwrap()
            .dispatchable
    );
    // A blocker without a state counts as blocking.
    let mut stateless = raw.clone();
    stateless["inverseRelations"]["nodes"][0]["issue"]["state"] = Value::Null;
    assert!(
        !linear::normalize_issue(&stateless, None, &terminal())
            .unwrap()
            .dispatchable
    );
    // Non-integer priorities become nil.
    let mut float_priority = raw;
    float_priority["priority"] = json!(2.5);
    assert_eq!(
        linear::normalize_issue(&float_priority, None, &terminal())
            .unwrap()
            .priority,
        None
    );
}

#[tokio::test]
async fn linear_client_rejects_malformed_issues_instead_of_returning_invalid_scheduler_records() {
    let malformed = json!({
        "id": "issue-empty-title",
        "identifier": "MT-EMPTY",
        "title": " ",
        "state": {"name": "Todo"}
    });
    assert!(linear::normalize_issue(&malformed, None, &terminal()).is_none());

    let server = graphql_server(json!({"data": {"issues": {"nodes": [malformed]}}})).await;
    assert_eq!(
        tracker_for(&server)
            .fetch_issues_by_ids(
                &linear_settings(json!({})),
                &strings(&["issue-empty-title"])
            )
            .await,
        Err(TrackerError::UnknownPayload(Provider::Linear))
    );
}

#[test]
fn linear_client_marks_explicitly_unassigned_issues_as_not_routed_to_worker() {
    let raw = json!({
        "id": "issue-99",
        "identifier": "MT-99",
        "title": "Someone else's task",
        "state": {"name": "Todo"},
        "assignee": {"id": "user-2"}
    });
    assert!(
        !linear::normalize_issue(&raw, Some(&filter("user-1")), &terminal())
            .unwrap()
            .dispatchable
    );
    let unassigned = json!({"id": "i", "identifier": "MT-1", "title": "t", "state": {"name": "Todo"}, "assignee": null});
    assert!(
        !linear::normalize_issue(&unassigned, Some(&filter("user-1")), &terminal())
            .unwrap()
            .dispatchable
    );
    assert!(
        linear::normalize_issue(&unassigned, None, &terminal())
            .unwrap()
            .dispatchable
    );
}

#[tokio::test]
async fn linear_client_paginates_issue_state_fetches_by_id_beyond_one_page() {
    let requested: Vec<String> = (1..=55).map(|n| format!("issue-{n}")).collect();
    let server = MockServer::start().await;
    // Return each batch reversed so ordering must come from the request.
    for batch in requested.chunks(50) {
        let nodes: Vec<Value> = batch
            .iter()
            .rev()
            .map(|id| node(id, &format!("MT-{}", &id[6..]), "In Progress"))
            .collect();
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"variables": {"ids": batch}})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": {"issues": {"nodes": nodes}}})),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    let mut settings = linear_settings(json!({}));
    settings.project_slug = Some("test-project".into());
    let issues = tracker_for(&server)
        .fetch_issues_by_ids(&settings, &requested)
        .await
        .unwrap();
    assert_eq!(ids(&issues), requested);

    let bodies: Vec<Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(json_body)
        .collect();
    assert_eq!(bodies.len(), 2);
    assert_eq!(
        bodies[0]["variables"],
        json!({"ids": requested[..50], "projectSlug": "test-project", "first": 50, "relationFirst": 50})
    );
    assert_eq!(
        bodies[1]["variables"],
        json!({"ids": requested[50..], "projectSlug": "test-project", "first": 5, "relationFirst": 50})
    );
    assert_eq!(bodies[0]["query"], bodies[1]["query"]);
    assert_eq!(bodies[0]["query"], linear::ISSUES_BY_ID_QUERY);
}

#[tokio::test]
async fn linear_client_pages_state_reads_with_cursors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"variables": {"after": null}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![
                node("a", "MT-1", "Todo"),
                json!({"id": "bad"}),
                node("b", "MT-2", "Todo"),
            ],
            true,
            Some("cursor-1"),
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"variables": {"after": "cursor-1"}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![node("c", "MT-3", "Todo")],
            false,
            None,
        )))
        .expect(1)
        .mount(&server)
        .await;
    let (logs, _guard) = capture_logs();
    let issues = tracker_for(&server)
        .fetch_issues_by_states(
            &linear_settings(json!({})),
            &strings(&["Todo", "Todo", " In Progress "]),
        )
        .await
        .unwrap();
    assert_eq!(ids(&issues), ["a", "b", "c"]);
    assert!(
        logs.contents()
            .contains("Dropping malformed Linear issue records count=1")
    );
    let first = json_body(&server.received_requests().await.unwrap()[0]);
    assert_eq!(first["query"], linear::POLL_QUERY);
    assert_eq!(
        first["variables"],
        json!({"projectSlug": "project", "stateNames": ["Todo", " In Progress "], "first": 50, "relationFirst": 50, "after": null})
    );
}

#[tokio::test]
async fn linear_pagination_errors_missing_cursor_repeated_cursor_and_missing_page_info() {
    let missing = graphql_server(page(vec![], true, None)).await;
    assert_eq!(
        tracker_for(&missing)
            .fetch_issues_by_states(&linear_settings(json!({})), &strings(&["Todo"]))
            .await,
        Err(TrackerError::MissingPageCursor(Provider::Linear))
    );

    // Improvement: a server that repeats its cursor fails instead of looping forever.
    let repeating = graphql_server(page(vec![node("a", "MT-1", "Todo")], true, Some("same"))).await;
    assert_eq!(
        tracker_for(&repeating)
            .fetch_issues_by_states(&linear_settings(json!({})), &strings(&["Todo"]))
            .await,
        Err(TrackerError::PaginationRepeatedCursor(Provider::Linear))
    );
    assert_eq!(repeating.received_requests().await.unwrap().len(), 2);

    // Improvement: a page without pageInfo ends pagination and keeps earlier pages.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"variables": {"after": null}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![node("a", "MT-1", "Todo")],
            true,
            Some("c1"),
        )))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"variables": {"after": "c1"}})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": {"issues": {"nodes": [node("b", "MT-2", "Todo")]}}})),
        )
        .mount(&server)
        .await;
    assert_eq!(
        ids(&tracker_for(&server)
            .fetch_issues_by_states(&linear_settings(json!({})), &strings(&["Todo"]))
            .await
            .unwrap()),
        ["a", "b"]
    );

    let errors = graphql_server(json!({"errors": [{"message": "boom"}]})).await;
    assert_eq!(
        tracker_for(&errors)
            .fetch_issues_by_states(&linear_settings(json!({})), &strings(&["Todo"]))
            .await,
        Err(TrackerError::LinearGraphqlErrors(
            json!([{"message": "boom"}])
        ))
    );
    let unknown = graphql_server(json!({"data": {}})).await;
    assert_eq!(
        tracker_for(&unknown)
            .fetch_issues_by_ids(&linear_settings(json!({})), &strings(&["x"]))
            .await,
        Err(TrackerError::UnknownPayload(Provider::Linear))
    );
}

#[tokio::test]
async fn linear_client_logs_response_bodies_for_non_200_graphql_responses() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_raw(
            r#"{"errors":[{"message":"Variable \"$ids\" got invalid value"}]}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    let (logs, _guard) = capture_logs();
    let result = tracker_for(&server)
        .fetch_issues_by_ids(&linear_settings(json!({})), &strings(&["x"]))
        .await;
    assert_eq!(
        result,
        Err(TrackerError::ApiStatus {
            provider: Provider::Linear,
            status: 400
        })
    );
    let logged = logs.contents();
    assert!(logged.contains("Linear GraphQL request failed status=400"));
    assert!(logged.contains("got invalid value"));
}

#[tokio::test]
async fn linear_graphql_honors_a_bound_tracker_settings_snapshot() {
    let server = graphql_server(json!({"data": {}})).await;
    // A bare snapshot (no workflow defaults), as bound at session start.
    let settings = TrackerSettings {
        kind: Some("linear".into()),
        api_key: Some("bound-token".into()),
        endpoint: Some("https://linear.test/graphql".into()),
        ..TrackerSettings::default()
    };
    let body = tracker_for(&server)
        .graphql(&settings, "query Q { viewer { id } }", json!({}))
        .await
        .unwrap();
    assert_eq!(body, json!({"data": {}}));
    let request = &server.received_requests().await.unwrap()[0];
    let body = json_body(request);
    assert!(body.get("operationName").is_none());
    assert_eq!(
        header_of(request, "authorization").as_deref(),
        Some("bound-token")
    );
    assert_eq!(
        header_of(request, "content-type").as_deref(),
        Some("application/json")
    );
}

#[tokio::test]
async fn assignee_routing_uses_ids_and_resolves_me_through_the_viewer_query() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"query": linear::VIEWER_QUERY})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": {"viewer": {"id": " user-1 "}}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut other = node("b", "MT-2", "Todo");
    other["assignee"] = json!({"id": "user-2"});
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"query": linear::POLL_QUERY})))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![node("a", "MT-1", "Todo"), other],
            false,
            None,
        )))
        .mount(&server)
        .await;
    let me = linear_settings(json!({"assignee": "me"}));
    let issues = tracker_for(&server)
        .fetch_issues_by_states(&me, &strings(&["Todo"]))
        .await
        .unwrap();
    let dispatchable: Vec<bool> = issues.iter().map(|i| i.dispatchable).collect();
    assert_eq!(dispatchable, [true, false]);

    let no_viewer = graphql_server(json!({"data": {"viewer": null}})).await;
    assert_eq!(
        tracker_for(&no_viewer)
            .fetch_issues_by_states(&me, &strings(&["Todo"]))
            .await,
        Err(TrackerError::MissingLinearViewerIdentity)
    );
}

// ---- core_test.exs ----

#[tokio::test]
async fn empty_reads_are_no_ops_and_missing_config_is_reported_before_requests() {
    let tracker = offline();
    let s = linear_settings(json!({}));
    assert_eq!(tracker.fetch_issues_by_ids(&s, &[]).await, Ok(vec![]));
    assert_eq!(tracker.fetch_issues_by_states(&s, &[]).await, Ok(vec![]));
    let mut no_key = s.clone();
    no_key.api_key = None;
    assert_eq!(
        tracker
            .fetch_issues_by_states(&no_key, &strings(&["Todo"]))
            .await,
        Err(C::MissingLinearApiToken.into())
    );
    let mut no_slug = s;
    no_slug.project_slug = None;
    assert_eq!(
        tracker
            .fetch_issues_by_ids(&no_slug, &strings(&["x"]))
            .await,
        Err(C::MissingLinearProjectSlug.into())
    );
}

#[test]
fn validation_and_secret_names_follow_core() {
    let env = MapEnv::new().with("LINEAR_API_KEY", "env-key");
    let s = settings_with_env(json!({"kind": "linear", "project_slug": "p"}), &env).tracker;
    let tracker = offline();
    assert_eq!(tracker.validate_config(&s), Ok(()));
    assert_eq!(tracker.secret_environment_names(&s), ["LINEAR_API_KEY"]);
    let blank = settings_with_env(
        json!({"kind": "linear", "api_key": "   ", "project_slug": "p"}),
        &env,
    )
    .tracker;
    assert_eq!(
        tracker.validate_config(&blank),
        Err(C::MissingLinearApiToken.into())
    );
    let binding = ToolBinding::bind(Arc::new(offline()), s);
    assert_eq!(binding.secret_environment_names(), ["LINEAR_API_KEY"]);
}
