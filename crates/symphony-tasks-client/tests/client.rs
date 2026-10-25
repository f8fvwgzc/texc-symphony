//! The client against the real server router on a TCP port (the wire contract end to end).

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::json;
use symphony_server::testing::StaticControlPlane;
use symphony_server::{Auth, ServerConfig, StateView, serve};
use symphony_store::{NewRun, Store, TaskStatus as StoreStatus};
use symphony_tasks_client::{
    ClientError, ListTasks, NewTask, TaskEvent, TaskLimits, TaskStatus, TasksClient,
};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "client-test-token-0123456789";

async fn server(store: Store) -> (String, CancellationToken) {
    let shutdown = CancellationToken::new();
    let bound = serve(
        ServerConfig {
            auth: Auth::token(TOKEN),
            ..ServerConfig::new("127.0.0.1", 0)
        },
        Arc::new(StaticControlPlane::new(Ok(StateView::default()))),
        store,
        shutdown.clone(),
    )
    .await
    .unwrap();
    (format!("http://127.0.0.1:{}", bound.port()), shutdown)
}

#[tokio::test]
async fn create_watch_and_wait_for_a_task() {
    let store = Store::open_in_memory().unwrap();
    let (base, shutdown) = server(store.clone()).await;
    let client = TasksClient::new(&base).unwrap().with_token(TOKEN);

    let task = client
        .create(&NewTask {
            title: Some("Haiku".into()),
            prompt: "Write a haiku about Rust".into(),
            limits: Some(TaskLimits {
                max_tokens: Some(10_000),
                timeout_ms: None,
            }),
            metadata: Some(json!({"ticket": 3})),
            ..NewTask::default()
        })
        .await
        .unwrap();
    assert_eq!(task.status, TaskStatus::Queued);
    assert_eq!(task.identifier, format!("task-{}", task.id));
    assert_eq!(client.get(task.id).await.unwrap(), task);
    let page = client
        .list(&ListTasks {
            status: Some(TaskStatus::Queued),
            ..ListTasks::default()
        })
        .await
        .unwrap();
    assert_eq!(page.tasks.len(), 1);

    // Play the orchestrator: run the task and finish it.
    let id = task.id;
    let worker = {
        let store = store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            store.start_task(id).await.unwrap();
            let run = store
                .start_run(NewRun::new(format!("task:{id}"), format!("task-{id}")))
                .await
                .unwrap();
            store
                .append_event(run, "session_started", None, None)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(700)).await;
            store
                .finish_task(
                    id,
                    StoreStatus::Succeeded,
                    Some("five seven five".into()),
                    None,
                )
                .await
                .unwrap();
            store
                .finish_run(run, symphony_store::RunStatus::Succeeded, None)
                .await
                .unwrap();
        })
    };
    let mut events = client.watch(id).await.unwrap();
    let mut saw_run_event = false;
    let mut last = None;
    while let Some(event) = events.next().await {
        match event.unwrap() {
            TaskEvent::RunEvent(event) => {
                assert_eq!(event.kind, "session_started");
                saw_run_event = true;
            }
            TaskEvent::Task(task) => last = Some(task),
        }
    }
    worker.await.unwrap();
    assert!(saw_run_event);
    let last = last.unwrap();
    assert_eq!(last.status, TaskStatus::Succeeded);
    assert_eq!(last.result.as_deref(), Some("five seven five"));
    // `wait` on a final task returns at once.
    assert_eq!(client.wait(id).await.unwrap().status, TaskStatus::Succeeded);
    shutdown.cancel();
}

#[tokio::test]
async fn errors_carry_the_api_code() {
    let (base, shutdown) = server(Store::open_in_memory().unwrap()).await;
    let anonymous = TasksClient::new(&base).unwrap();
    match anonymous.get(1).await {
        Err(ClientError::Api { status, code, .. }) => {
            assert_eq!((status, code.as_str()), (401, "unauthorized"));
        }
        other => panic!("expected 401, got {other:?}"),
    }
    let client = TasksClient::new(&base).unwrap().with_token(TOKEN);
    match client.get(42).await {
        Err(ClientError::Api { status, code, .. }) => {
            assert_eq!((status, code.as_str()), (404, "task_not_found"));
        }
        other => panic!("expected 404, got {other:?}"),
    }
    match client
        .create(&NewTask {
            prompt: "x".into(),
            repo: Some("file:///etc".into()),
            ..NewTask::default()
        })
        .await
    {
        Err(ClientError::Api { status, code, .. }) => {
            assert_eq!((status, code.as_str()), (400, "invalid_parameter"));
        }
        other => panic!("expected 400, got {other:?}"),
    }
    let task = client
        .create(&NewTask {
            prompt: "x".into(),
            ..NewTask::default()
        })
        .await
        .unwrap();
    assert_eq!(
        client.cancel(task.id).await.unwrap().status,
        TaskStatus::Cancelled
    );
    match client.unblock(task.id, Some("go")).await {
        Err(ClientError::Api { status, code, .. }) => {
            assert_eq!((status, code.as_str()), (409, "task_conflict"));
        }
        other => panic!("expected 409, got {other:?}"),
    }
    assert!(matches!(
        TasksClient::new("ftp://x"),
        Err(ClientError::InvalidUrl(_))
    ));
    shutdown.cancel();
}
