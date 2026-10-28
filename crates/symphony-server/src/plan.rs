//! `POST /api/v1/tasks/plan`: plans a set of tasks without creating or running anything.
//!
//! The request names each planned task by a client `key` and lists what it depends on: other
//! planned tasks (by key, a string) and existing tasks (by id, an integer). The answer is what
//! creating them in `order` would do right now:
//!
//! * `order`: the planned keys in dependency order (wave by wave, request order within a wave);
//! * `waves`: wave 0 depends on no planned task, wave `n + 1` on planned tasks of wave `n` or
//!   earlier, so each wave can run once the previous ones have succeeded;
//! * per task, its `wave` and `status`: `queued` (would be dispatched as soon as a slot is free),
//!   `waiting` (on `waiting_for`: the planned tasks it depends on and the existing ones that have
//!   not succeeded), or `failed` (`failed_dependency` already failed or was cancelled, or fails
//!   transitively; creating it would be refused).
//!
//! An unknown key or task id, a duplicate key, or a cycle is `invalid_parameter`.

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use serde::{Deserialize, Serialize};
use symphony_store::{MAX_TASK_DEPENDENCIES, TaskStatus};

use crate::app::AppState;
use crate::error::ApiError;
use crate::history::{enabled_store, invalid};

/// Most tasks one plan may hold.
pub const MAX_PLAN_TASKS: usize = 1000;
/// Longest key, in characters.
const MAX_KEY_CHARS: usize = 128;

/// A dependency: a planned task's key or an existing task's id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Dependency {
    Planned(String),
    Existing(i64),
}

/// Body of `POST /api/v1/tasks/plan`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanRequest {
    tasks: Vec<PlanTask>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanTask {
    key: String,
    #[serde(default)]
    depends_on: Vec<Dependency>,
}

/// What a planned task would be once created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PlannedStatus {
    Queued,
    Waiting,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct PlannedTask {
    key: String,
    wave: usize,
    status: PlannedStatus,
    waiting_for: Vec<Dependency>,
    failed_dependency: Option<Dependency>,
}

/// Body of the answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Plan {
    order: Vec<String>,
    waves: Vec<Vec<String>>,
    tasks: Vec<PlannedTask>,
}

/// The existing task ids `request` refers to.
fn existing_ids(request: &PlanRequest) -> Vec<i64> {
    let mut ids: Vec<i64> = request
        .tasks
        .iter()
        .flat_map(|task| &task.depends_on)
        .filter_map(|dependency| match dependency {
            Dependency::Existing(id) => Some(*id),
            Dependency::Planned(_) => None,
        })
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Plans `request` given the status of the existing tasks it names (see the module docs).
pub(crate) fn plan(request: &PlanRequest, existing: &[(i64, TaskStatus)]) -> Result<Plan, String> {
    let tasks = &request.tasks;
    if tasks.len() > MAX_PLAN_TASKS {
        return Err(format!("a plan holds at most {MAX_PLAN_TASKS} tasks"));
    }
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, task) in tasks.iter().enumerate() {
        if task.key.is_empty() || task.key.chars().count() > MAX_KEY_CHARS {
            return Err(format!("keys must be 1-{MAX_KEY_CHARS} characters"));
        }
        if index.insert(task.key.as_str(), i).is_some() {
            return Err(format!("duplicate key {:?}", task.key));
        }
        if task.depends_on.len() > MAX_TASK_DEPENDENCIES {
            return Err(format!(
                "{:?} depends on more than {MAX_TASK_DEPENDENCIES} tasks",
                task.key
            ));
        }
    }
    let status_of = |id: i64| existing.iter().find(|(e, _)| *e == id).map(|(_, s)| *s);
    // Planned dependencies by index, after checking every reference.
    let mut planned: Vec<Vec<usize>> = Vec::with_capacity(tasks.len());
    for task in tasks {
        let mut deps = Vec::new();
        for dependency in &task.depends_on {
            match dependency {
                Dependency::Planned(key) => match index.get(key.as_str()) {
                    Some(&dep) => deps.push(dep),
                    None => return Err(format!("{:?} depends on unknown key {key:?}", task.key)),
                },
                Dependency::Existing(id) if status_of(*id).is_none() => {
                    return Err(format!(
                        "{:?} depends on task {id}, which does not exist",
                        task.key
                    ));
                }
                Dependency::Existing(_) => {}
            }
        }
        deps.sort_unstable();
        deps.dedup();
        planned.push(deps);
    }

    // Kahn's algorithm, taking each wave in request order.
    let mut remaining: Vec<usize> = planned.iter().map(Vec::len).collect();
    let mut dependants: Vec<Vec<usize>> = vec![Vec::new(); tasks.len()];
    for (task, deps) in planned.iter().enumerate() {
        for &dep in deps {
            dependants[dep].push(task);
        }
    }
    let mut wave_of = vec![0usize; tasks.len()];
    let mut waves: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = (0..tasks.len()).filter(|&t| remaining[t] == 0).collect();
    let mut placed = 0;
    while !current.is_empty() {
        let mut next = Vec::new();
        for &task in &current {
            wave_of[task] = waves.len();
            for &dependant in &dependants[task] {
                remaining[dependant] -= 1;
                if remaining[dependant] == 0 {
                    next.push(dependant);
                }
            }
        }
        placed += current.len();
        next.sort_unstable();
        waves.push(std::mem::replace(&mut current, next));
    }
    if placed < tasks.len() {
        return Err(format!(
            "dependency cycle: {}",
            cycle(&planned, &remaining, tasks)
        ));
    }

    let mut planned_tasks: Vec<Option<PlannedTask>> = vec![None; tasks.len()];
    for &task in waves.iter().flatten() {
        let mut failed = None;
        let mut waiting_for = Vec::new();
        for dependency in &tasks[task].depends_on {
            match dependency {
                Dependency::Planned(key) => {
                    let dep = index[key.as_str()];
                    let dep_failed = planned_tasks[dep]
                        .as_ref()
                        .is_some_and(|d| d.status == PlannedStatus::Failed);
                    if dep_failed {
                        failed.get_or_insert_with(|| dependency.clone());
                    }
                    waiting_for.push(dependency.clone());
                }
                Dependency::Existing(id) => match status_of(*id) {
                    Some(TaskStatus::Succeeded) => {}
                    Some(TaskStatus::Failed | TaskStatus::Cancelled) => {
                        failed.get_or_insert_with(|| dependency.clone());
                    }
                    _ => waiting_for.push(dependency.clone()),
                },
            }
        }
        waiting_for.dedup();
        let status = match (&failed, waiting_for.is_empty()) {
            (Some(_), _) => PlannedStatus::Failed,
            (None, true) => PlannedStatus::Queued,
            (None, false) => PlannedStatus::Waiting,
        };
        planned_tasks[task] = Some(PlannedTask {
            key: tasks[task].key.clone(),
            wave: wave_of[task],
            status,
            waiting_for: if failed.is_some() {
                Vec::new()
            } else {
                waiting_for
            },
            failed_dependency: failed,
        });
    }
    let key = |task: &usize| tasks[*task].key.clone();
    Ok(Plan {
        order: waves.iter().flatten().map(key).collect(),
        waves: waves
            .iter()
            .map(|wave| wave.iter().map(key).collect())
            .collect(),
        tasks: planned_tasks.into_iter().flatten().collect(),
    })
}

/// One cycle among the tasks Kahn's algorithm could not place (`remaining > 0`), as keys.
fn cycle(planned: &[Vec<usize>], remaining: &[usize], tasks: &[PlanTask]) -> String {
    // Every unplaced task has an unplaced dependency, so walking those must revisit a task.
    let Some(start) = remaining.iter().position(|&r| r > 0) else {
        return String::new();
    };
    let mut path = vec![start];
    loop {
        let last = path[path.len() - 1];
        let Some(&dep) = planned[last].iter().find(|&&d| remaining[d] > 0) else {
            return String::new();
        };
        if let Some(at) = path.iter().position(|&t| t == dep) {
            let mut keys: Vec<&str> = path[at..].iter().map(|&t| tasks[t].key.as_str()).collect();
            keys.push(tasks[dep].key.as_str());
            return keys.join(" -> ");
        }
        path.push(dep);
    }
}

/// `POST /api/v1/tasks/plan`.
pub(crate) async fn create(
    State(app): State<AppState>,
    body: Result<Json<PlanRequest>, JsonRejection>,
) -> Result<Json<Plan>, ApiError> {
    let request = body.map(|Json(request)| request).map_err(|rejection| {
        ApiError::InvalidParameter(format!("invalid body: {}", rejection.body_text()))
    })?;
    if request.tasks.len() > MAX_PLAN_TASKS {
        return Err(invalid("a plan holds at most 1000 tasks"));
    }
    let ids = existing_ids(&request);
    let existing = if ids.is_empty() {
        Vec::new()
    } else {
        enabled_store(&app)?.task_states(ids).await?
    };
    plan(&request, &existing)
        .map(Json)
        .map_err(ApiError::InvalidParameter)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn request(value: Value) -> PlanRequest {
        serde_json::from_value(value).unwrap()
    }

    fn planned(value: Value, existing: &[(i64, TaskStatus)]) -> Value {
        serde_json::to_value(plan(&request(value), existing).unwrap()).unwrap()
    }

    #[test]
    fn waves_follow_dependencies_in_request_order() {
        let out = planned(
            json!({"tasks": [
                {"key": "deploy", "depends_on": ["build", "test"]},
                {"key": "test", "depends_on": ["build"]},
                {"key": "build"},
                {"key": "docs"},
            ]}),
            &[],
        );
        assert_eq!(out["order"], json!(["build", "docs", "test", "deploy"]));
        assert_eq!(
            out["waves"],
            json!([["build", "docs"], ["test"], ["deploy"]])
        );
        assert_eq!(
            out["tasks"][0],
            json!({"key": "deploy", "wave": 2, "status": "waiting",
                "waiting_for": ["build", "test"], "failed_dependency": null})
        );
        assert_eq!(out["tasks"][2]["status"], "queued");
        assert_eq!(out["tasks"][2]["waiting_for"], json!([]));
    }

    #[test]
    fn existing_tasks_set_the_waiting_set_and_failures_propagate() {
        let out = planned(
            json!({"tasks": [
                {"key": "a", "depends_on": [1, 2]},
                {"key": "b", "depends_on": [3]},
                {"key": "c", "depends_on": ["b"]},
                {"key": "d", "depends_on": [1]},
            ]}),
            &[
                (1, TaskStatus::Succeeded),
                (2, TaskStatus::Running),
                (3, TaskStatus::Cancelled),
            ],
        );
        // Existing dependencies add no wave; a succeeded one is not waited for.
        assert_eq!(out["waves"], json!([["a", "b", "d"], ["c"]]));
        assert_eq!(out["tasks"][0]["status"], "waiting");
        assert_eq!(out["tasks"][0]["waiting_for"], json!([2]));
        assert_eq!(out["tasks"][1]["status"], "failed");
        assert_eq!(out["tasks"][1]["failed_dependency"], json!(3));
        assert_eq!(out["tasks"][2]["status"], "failed");
        assert_eq!(out["tasks"][2]["failed_dependency"], json!("b"));
        assert_eq!(out["tasks"][3]["status"], "queued");
    }

    #[test]
    fn bad_graphs_are_refused() {
        let error = |value: Value| plan(&request(value), &[(1, TaskStatus::Queued)]).unwrap_err();
        assert_eq!(
            error(json!({"tasks": [
                {"key": "a", "depends_on": ["c"]},
                {"key": "b", "depends_on": ["a"]},
                {"key": "c", "depends_on": ["b"]},
                {"key": "d"},
            ]})),
            "dependency cycle: a -> c -> b -> a"
        );
        assert_eq!(
            error(json!({"tasks": [{"key": "a", "depends_on": ["a"]}]})),
            "dependency cycle: a -> a"
        );
        assert!(error(json!({"tasks": [{"key": "a"}, {"key": "a"}]})).contains("duplicate key"));
        assert!(
            error(json!({"tasks": [{"key": "a", "depends_on": ["x"]}]})).contains("unknown key")
        );
        assert!(
            error(json!({"tasks": [{"key": "a", "depends_on": [9]}]})).contains("does not exist")
        );
        assert!(error(json!({"tasks": [{"key": ""}]})).contains("1-128"));
        assert!(
            serde_json::from_value::<PlanRequest>(json!({"tasks": [{"key": "a", "prompt": "x"}]}))
                .is_err(),
            "unknown fields are refused"
        );
    }

    #[test]
    fn an_empty_plan_is_empty() {
        assert_eq!(
            planned(json!({"tasks": []}), &[]),
            json!({"order": [], "waves": [], "tasks": []})
        );
    }
}
