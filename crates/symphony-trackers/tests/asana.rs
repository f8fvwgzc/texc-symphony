//! Port of `asana_adapter_test.exs`.

mod support;

use std::sync::Arc;

use serde_json::{Value, json};
use support::*;
use symphony_core::config::{AsanaSettings, TrackerSettings, resolve_asana};
use symphony_core::{MapEnv, TrackerConfigError as C};
use symphony_trackers::asana::{self, AsanaTracker};
use symphony_trackers::{Provider, Tracker, TrackerError, bind_agent_tools};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ORIGIN: &str = "https://asana.test";

fn tracker_json(overrides: Value) -> Value {
    let mut provider = json!({
        "project_gid": "project-1",
        "api_key": "test-token",
        "endpoint": "https://asana.test/api/1.0",
    });
    if let (Value::Object(base), Value::Object(extra)) = (&mut provider, overrides) {
        for (k, v) in extra {
            base.insert(k, v);
        }
    }
    json!({
        "kind": "asana",
        "provider": provider,
        "active_states": ["Todo"],
        "terminal_states": ["Done"],
    })
}

fn settings(overrides: Value) -> TrackerSettings {
    tracker_settings(tracker_json(overrides))
}

fn resolved(tracker: &TrackerSettings) -> AsanaSettings {
    resolve_asana(tracker, &MapEnv::new()).expect("valid asana settings")
}

fn task_in_section(gid: &str, section: &str, section_gid: &str) -> Value {
    json!({
        "gid": gid,
        "name": format!("Task {gid}"),
        "notes": format!(" Notes {gid} "),
        "completed": false,
        "resource_subtype": "default_task",
        "assignee": {"gid": "assignee-1"},
        "tags": [{"name": " Bug "}, {"name": "bug"}, {"name": "Platform"}],
        "memberships": [{"project": {"gid": "project-1"}, "section": {"gid": section_gid, "name": section}}],
        "permalink_url": format!("https://app.asana.test/0/project-1/{gid}"),
        "created_at": "2026-01-01T00:00:00Z",
        "modified_at": "2026-01-02T00:00:00Z"
    })
}

fn raw_task(gid: &str) -> Value {
    task_in_section(gid, "Todo", "section-todo")
}

fn tracker_for(server: &MockServer) -> AsanaTracker {
    AsanaTracker::new(http_for(&[(ORIGIN, server)]), Arc::new(MapEnv::new()))
}

fn offline() -> AsanaTracker {
    let deps = offline_deps(MapEnv::new());
    AsanaTracker::new(deps.http, deps.env)
}

#[tokio::test]
async fn adapter_validates_asana_config_delegates_reads_and_advertises_asana_api() {
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
        Err(C::MissingAsanaActiveStates.into())
    );
    let mut t = tracker_json(json!({}));
    t.as_object_mut().unwrap().remove("terminal_states");
    assert_eq!(
        tracker.validate_config(&tracker_settings(t)),
        Err(C::MissingAsanaTerminalStates.into())
    );
    let mut t = tracker_json(json!({}));
    t["active_states"] = json!([""]);
    assert_eq!(
        tracker.validate_config(&tracker_settings(t)),
        Err(C::InvalidAsanaStates.into())
    );
    let specs = tracker.agent_tool_specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0]["name"], "asana_api");
    assert_eq!(
        specs[0]["inputSchema"]["properties"]["path"]["description"],
        "Asana REST path such as /tasks/{task_gid}/stories."
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/1.0/users/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"gid": "me"}})))
        .mount(&server)
        .await;
    let (ok, _) = run_tool(
        &tracker_for(&server),
        "asana_api",
        json!({"method": "GET", "path": "/users/me"}),
        &settings(json!({})),
    )
    .await;
    assert!(ok);
    let request = &server.received_requests().await.unwrap()[0];
    assert_eq!(
        header_of(request, "authorization").as_deref(),
        Some("Bearer test-token")
    );
    assert_eq!(
        header_of(request, "accept").as_deref(),
        Some("application/json")
    );
}

#[tokio::test]
async fn client_validates_project_settings_and_declares_token_environments() {
    let tracker = offline();
    let check = |overrides: Value| tracker.validate_config(&settings(overrides));
    assert_eq!(
        check(json!({"project_gid": 123})),
        Err(C::MissingAsanaProjectGid.into())
    );
    assert_eq!(
        check(json!({"api_key": 123})),
        Err(C::MissingAsanaApiKey.into())
    );
    assert_eq!(
        check(json!({"endpoint": "not a url"})),
        Err(C::InvalidAsanaEndpoint.into())
    );
    assert_eq!(
        check(json!({"endpoint": "http://app.asana.com/api/1.0"})),
        Err(C::InvalidAsanaEndpoint.into())
    );
    assert_eq!(
        check(json!({"endpoint": ""})),
        Err(C::InvalidAsanaEndpoint.into())
    );
    assert_eq!(
        tracker.secret_environment_names(&settings(json!({"api_key": "$SYMPHONY_ASANA_PAT"}))),
        strings(&["ASANA_PAT", "SYMPHONY_ASANA_PAT"])
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/1.0/projects/project-1/tasks"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": [], "next_page": null})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let trimmed = settings(json!({"project_gid": " project-1 ", "api_key": " token "}));
    assert_eq!(resolved(&trimmed).api_key, "token");
    assert_eq!(resolved(&trimmed).project_gid, "project-1");
    assert_eq!(
        tracker_for(&server)
            .fetch_issues_by_states(&trimmed, &strings(&["Todo"]))
            .await,
        Ok(vec![])
    );
    let request = &server.received_requests().await.unwrap()[0];
    assert_eq!(
        header_of(request, "authorization").as_deref(),
        Some("Bearer token")
    );
    assert!(request.body.is_empty());
}

#[test]
fn client_normalizes_asana_tasks_without_dropping_provider_details() {
    let s = resolved(&settings(json!({})));
    let issue = asana::normalize_issue(&raw_task("42"), &s).unwrap();
    assert_eq!(issue.id.as_deref(), Some("42"));
    assert_eq!(issue.identifier.as_deref(), Some("ASANA-42"));
    assert_eq!(
        Value::Object(issue.native_ref.clone().unwrap()),
        json!({"task_gid": "42", "project_gid": "project-1", "section_gid": "section-todo"})
    );
    assert_eq!(issue.title.as_deref(), Some("Task 42"));
    assert_eq!(issue.description.as_deref(), Some("Notes 42"));
    assert_eq!(issue.state.as_deref(), Some("Todo"));
    assert_eq!(
        issue.url.as_deref(),
        Some("https://app.asana.test/0/project-1/42")
    );
    assert_eq!(issue.assignee_id.as_deref(), Some("assignee-1"));
    assert_eq!(issue.labels, ["bug", "platform"]);
    assert!(issue.blocked_by.is_empty());
    assert!(issue.dispatchable);
    assert!(issue.created_at.is_some() && issue.updated_at.is_some());

    let with = |key: &str, value: Value| {
        let mut task = raw_task("43");
        task[key] = value;
        asana::normalize_issue(&task, &s)
    };
    assert!(!with("completed", json!(true)).unwrap().dispatchable);
    assert!(
        with("resource_subtype", json!("milestone"))
            .unwrap()
            .dispatchable
    );
    assert!(
        !with("resource_subtype", json!("section"))
            .unwrap()
            .dispatchable
    );
    assert!(with("memberships", json!([])).is_none());
    assert_eq!(with("permalink_url", json!(5)).unwrap().url, None);
    assert!(!with("completed", Value::Null).unwrap().dispatchable);
}

#[tokio::test]
async fn client_pages_state_reads_filters_requested_sections_and_drops_malformed_records() {
    let server = MockServer::start().await;
    let mut malformed = raw_task("3");
    malformed["name"] = json!("");
    Mock::given(method("GET"))
        .and(path("/api/1.0/projects/project-1/tasks"))
        .and(query_param("offset", "next-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [task_in_section("4", "Done", "section-done")],
            "next_page": null
        })))
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/1.0/projects/project-1/tasks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [raw_task("1"), raw_task("2"), malformed],
            "next_page": {"offset": "next-token"}
        })))
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;
    let (logs, _guard) = capture_logs();
    let issues = tracker_for(&server)
        .fetch_issues_by_states(&settings(json!({})), &strings(&[" todo "]))
        .await
        .unwrap();
    assert_eq!(ids(&issues), ["1", "2"]);
    assert!(
        logs.contents()
            .contains("Dropping malformed Asana task records count=1")
    );
    let requests = server.received_requests().await.unwrap();
    let first = query_of(&requests[0]);
    assert!(first.contains(&("limit".into(), "100".into())));
    assert!(
        first
            .iter()
            .any(|(k, v)| k == "opt_fields" && v.contains("memberships.section.name"))
    );
    assert!(!first.iter().any(|(k, _)| k == "offset"));
    assert!(query_of(&requests[1]).contains(&("offset".into(), "next-token".into())));
    assert_eq!(
        offline()
            .fetch_issues_by_states(&settings(json!({})), &[])
            .await,
        Ok(vec![])
    );
}

#[tokio::test]
async fn task_pages_report_missing_offsets_unknown_payloads_and_repeated_offsets() {
    for (body, expected) in [
        (
            json!({"data": [], "next_page": {"offset": ""}}),
            TrackerError::MissingPageCursor(Provider::Asana),
        ),
        (
            json!({"data": []}),
            TrackerError::UnknownPayload(Provider::Asana),
        ),
        (
            json!({"data": [], "next_page": {"offset": "same"}}),
            TrackerError::PaginationRepeatedCursor(Provider::Asana),
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        assert_eq!(
            tracker_for(&server)
                .fetch_issues_by_states(&settings(json!({})), &strings(&["Todo"]))
                .await,
            Err(expected)
        );
    }
    let missing = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&missing)
        .await;
    assert_eq!(
        tracker_for(&missing)
            .fetch_issues_by_states(&settings(json!({})), &strings(&["Todo"]))
            .await,
        Err(TrackerError::ApiStatus {
            provider: Provider::Asana,
            status: 404
        })
    );
}

#[tokio::test]
async fn client_refreshes_ids_in_order_omits_404s_and_out_of_project_tasks_and_rejects_malformed_refreshes()
 {
    let server = MockServer::start().await;
    for gid in ["1", "2"] {
        Mock::given(method("GET"))
            .and(path(format!("/api/1.0/tasks/{gid}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": raw_task(gid)})))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/api/1.0/tasks/404"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"errors": []})))
        .expect(1)
        .mount(&server)
        .await;
    let mut out = raw_task("out");
    out["memberships"][0]["project"]["gid"] = json!("other-project");
    Mock::given(method("GET"))
        .and(path("/api/1.0/tasks/out"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": out})))
        .expect(1)
        .mount(&server)
        .await;
    let mut bad = raw_task("bad");
    bad["name"] = json!("");
    Mock::given(method("GET"))
        .and(path("/api/1.0/tasks/bad"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": bad})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/1.0/tasks/forbidden"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let tracker = tracker_for(&server);
    let issues = tracker
        .fetch_issues_by_ids(
            &settings(json!({})),
            &strings(&["2", "1", "404", "out", "2"]),
        )
        .await
        .unwrap();
    assert_eq!(ids(&issues), ["2", "1"]);
    for request in server.received_requests().await.unwrap() {
        assert!(query_of(&request).iter().any(|(k, _)| k == "opt_fields"));
        assert!(request.body.is_empty());
    }
    assert_eq!(
        tracker
            .fetch_issues_by_ids(&settings(json!({})), &strings(&["bad"]))
            .await,
        Err(TrackerError::UnknownPayload(Provider::Asana))
    );
    assert_eq!(
        tracker
            .fetch_issues_by_ids(&settings(json!({})), &strings(&["forbidden"]))
            .await,
        Err(TrackerError::ApiStatus {
            provider: Provider::Asana,
            status: 403
        })
    );
    assert_eq!(
        offline()
            .fetch_issues_by_ids(&settings(json!({})), &[])
            .await,
        Ok(vec![])
    );
}

#[tokio::test]
async fn asana_api_preserves_rest_status_and_body_while_rejecting_unsafe_arguments() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/1.0/tasks/42/stories"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"gid": "story-1"}})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/1.0/tasks/404"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"errors": []})))
        .mount(&server)
        .await;
    let tracker = tracker_for(&server);
    let s = settings(json!({}));
    let result = tracker
        .execute_agent_tool(
            Some("asana_api"),
            &json!({
                "method": "post",
                "path": " /tasks/42/stories ",
                "query": {"opt_fields": "gid"},
                "body": {"data": {"text": "hello"}}
            }),
            &ctx(&s),
        )
        .await;
    assert!(result.success);
    assert_eq!(
        serde_json::from_str::<Value>(&result.output).unwrap(),
        json!({"status": 201, "body": {"data": {"gid": "story-1"}}})
    );
    assert_eq!(result.content_items[0].kind, "inputText");
    assert_eq!(result.content_items[0].text, result.output);
    let request = &server.received_requests().await.unwrap()[0];
    assert_eq!(query_of(request), vec![("opt_fields".into(), "gid".into())]);
    assert_eq!(json_body(request), json!({"data": {"text": "hello"}}));

    let (ok, output) = run_tool(
        &tracker,
        "asana_api",
        json!({"method": "GET", "path": "/tasks/404"}),
        &s,
    )
    .await;
    assert!(!ok);
    assert_eq!(output, json!({"status": 404, "body": {"errors": []}}));

    let before = server.received_requests().await.unwrap().len();
    for (args, message) in [
        (
            json!({"method": "GET", "path": "https://app.asana.com/api/1.0/users/me"}),
            "asana_api.path must be a relative Asana REST path.",
        ),
        (
            json!({"method": "PATCH", "path": "/tasks/42"}),
            "asana_api.method must be GET, POST, PUT, or DELETE.",
        ),
        (
            json!({"method": "GET", "path": "/tasks/42", "query": false}),
            "asana_api.query must be a JSON object when provided.",
        ),
        (
            json!({"path": "/tasks/42"}),
            "asana_api.method must be GET, POST, PUT, or DELETE.",
        ),
    ] {
        let (ok, output) = run_tool(&tracker, "asana_api", args, &s).await;
        assert!(!ok);
        assert_eq!(output, json!({"error": {"message": message}}));
    }
    assert_eq!(server.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn asana_api_reports_unsupported_tools_malformed_calls_and_client_failures() {
    let tracker = offline();
    let s = settings(json!({}));
    let (ok, output) = run_tool(&tracker, "not_asana_api", json!({}), &s).await;
    assert!(!ok);
    assert_eq!(output["error"]["supportedTools"], json!(["asana_api"]));
    let (ok, _) = run_tool(&tracker, "asana_api", json!("not-an-object"), &s).await;
    assert!(!ok);
    let (ok, _) = run_tool(
        &tracker,
        "asana_api",
        json!({"method": "GET", "path": 123}),
        &s,
    )
    .await;
    assert!(!ok);
    let (ok, output) = run_tool(
        &tracker,
        "asana_api",
        json!({"method": "GET", "path": "/users/me"}),
        &settings(json!({"api_key": 123})),
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {"message": "Symphony is missing Asana auth. Set tracker.provider.api_key or export ASANA_PAT."}})
    );
    let (ok, output) = run_tool(
        &tracker,
        "asana_api",
        json!({"method": "GET", "path": "/users/me"}),
        &settings(json!({"project_gid": 123})),
    )
    .await;
    assert!(!ok);
    assert_eq!(
        output,
        json!({"error": {"message": "Asana REST tool execution failed.", "reason": ":missing_asana_project_gid"}})
    );
}

#[tokio::test]
async fn tracker_binds_asana_tools_and_token_env_names_from_provider_config() {
    let env = MapEnv::new().with("SYMPHONY_ASANA_PAT_1", "test-token");
    let workflow = settings_with_env(
        tracker_json(json!({"api_key": "$SYMPHONY_ASANA_PAT_1"})),
        &env,
    );
    let deps = offline_deps(env);
    let binding = bind_agent_tools(&workflow, &deps).unwrap();
    assert_eq!(binding.kind(), "asana");
    assert_eq!(
        binding.secret_environment_names(),
        strings(&["ASANA_PAT", "SYMPHONY_ASANA_PAT_1"])
    );
    assert_eq!(binding.tool_names(), ["asana_api"]);
    assert_eq!(
        symphony_trackers::validate_config(&workflow.tracker, &deps),
        Ok(())
    );
}
