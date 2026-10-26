//! `server:` auth/host settings and the `budget:` section.

use serde_json::{Map, Value, json};
use symphony_core::config::{self, BudgetSettings, Settings};
use symphony_core::{ConfigError, MapEnv};

fn parse(value: Value, env: &MapEnv) -> Result<Settings, ConfigError> {
    let Value::Object(map) = value else {
        panic!("expected an object")
    };
    config::parse(&map, env)
}

fn invalid(value: Value) -> String {
    match parse(value, &MapEnv::new()) {
        Err(ConfigError::InvalidWorkflowConfig(message)) => message,
        other => panic!("expected invalid config, got {other:?}"),
    }
}

#[test]
fn api_token_resolves_env_references_and_the_default_variable() {
    let env = MapEnv::new().with("MY_TOKEN", "from-ref");
    let settings = parse(json!({"server": {"api_token": "$MY_TOKEN"}}), &env).unwrap();
    assert_eq!(settings.server.api_token.as_deref(), Some("from-ref"));
    assert_eq!(
        settings.server.secret_environment_names,
        vec!["SYMPHONY_API_TOKEN".to_owned(), "MY_TOKEN".to_owned()]
    );

    let env = MapEnv::new().with("SYMPHONY_API_TOKEN", "fallback");
    let settings = parse(json!({}), &env).unwrap();
    assert_eq!(settings.server.api_token.as_deref(), Some("fallback"));

    let settings = parse(json!({"server": {"api_token": "literal"}}), &MapEnv::new()).unwrap();
    assert_eq!(settings.server.api_token.as_deref(), Some("literal"));

    let settings = parse(json!({}), &MapEnv::new()).unwrap();
    assert_eq!(settings.server.api_token, None);
}

#[test]
fn the_api_token_is_stripped_from_agent_environments_and_redacted_in_debug() {
    let env = MapEnv::new().with("MY_TOKEN", "s3cret-value");
    let settings = parse(json!({"server": {"api_token": "$MY_TOKEN"}}), &env).unwrap();
    let names = settings.secret_environment_names();
    assert!(names.iter().any(|n| n == "SYMPHONY_API_TOKEN"), "{names:?}");
    assert!(names.iter().any(|n| n == "MY_TOKEN"), "{names:?}");
    let debug = format!("{:?}", settings.server);
    assert!(!debug.contains("s3cret-value"), "{debug}");
    assert!(debug.contains("<redacted>"), "{debug}");
}

#[test]
fn allowed_hosts_and_origins_are_normalized_and_validated() {
    let settings = parse(
        json!({"server": {
            "allowed_hosts": [" Symphony.Example ", "", "symphony.example"],
            "allowed_origins": ["https://Dash.Example/", "*"],
        }}),
        &MapEnv::new(),
    )
    .unwrap();
    assert_eq!(settings.server.allowed_hosts, vec!["symphony.example"]);
    assert_eq!(
        settings.server.allowed_origins,
        vec!["https://dash.example", "*"]
    );

    let message = invalid(json!({"server": {"allowed_origins": ["dash.example"]}}));
    assert!(
        message.contains("server.allowed_origins is invalid"),
        "{message}"
    );
    let message = invalid(json!({"server": {"allowed_hosts": "x"}}));
    assert!(
        message.contains("server.allowed_hosts is invalid"),
        "{message}"
    );
}

#[test]
fn budget_defaults_to_unlimited_and_free() {
    let settings = parse(json!({}), &MapEnv::new()).unwrap();
    assert_eq!(settings.budget, BudgetSettings::default());
    assert_eq!(settings.budget.cost_micro_usd(1_000_000, 1_000_000), 0);
}

#[test]
fn budget_limits_and_prices_are_cast_and_validated() {
    let settings = parse(
        json!({"budget": {
            "max_tokens_per_issue": 2_000_000,
            "max_tokens_per_day": "20000000",
            "max_cost_usd_per_issue": 5,
            "max_cost_usd_per_day": "12.5",
            "input_usd_per_million_tokens": 1.25,
            "output_usd_per_million_tokens": 10,
        }}),
        &MapEnv::new(),
    )
    .unwrap();
    let budget = &settings.budget;
    assert_eq!(budget.max_tokens_per_issue, Some(2_000_000));
    assert_eq!(budget.max_tokens_per_day, Some(20_000_000));
    assert_eq!(budget.max_cost_usd_per_issue, Some(5.0));
    assert_eq!(budget.max_cost_usd_per_day, Some(12.5));
    // 1M input at $1.25 + 100k output at $10/M = $2.25.
    assert_eq!(budget.cost_micro_usd(1_000_000, 100_000), 2_250_000);

    let message = invalid(json!({"budget": {
        "max_tokens_per_issue": 0,
        "max_cost_usd_per_day": -1,
        "input_usd_per_million_tokens": -0.5,
        "output_usd_per_million_tokens": "free",
    }}));
    for fragment in [
        "budget.max_tokens_per_issue must be greater than 0",
        "budget.max_cost_usd_per_day must be greater than 0",
        "budget.input_usd_per_million_tokens must be greater than or equal to 0",
        "budget.output_usd_per_million_tokens is invalid",
    ] {
        assert!(message.contains(fragment), "{fragment}: {message}");
    }
    let mut map = Map::new();
    map.insert("budget".into(), json!("nope"));
    let message = invalid(Value::Object(map));
    assert!(message.contains("budget is invalid"), "{message}");
}
