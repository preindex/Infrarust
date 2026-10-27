#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use infrarust_config::{DockerSwarmManagerConfig, ServerManagerConfig};

fn parse(toml_text: &str) -> Result<ServerManagerConfig, toml::de::Error> {
    toml::from_str(toml_text)
}

fn swarm(toml_text: &str) -> DockerSwarmManagerConfig {
    match parse(toml_text).expect("config should parse") {
        ServerManagerConfig::DockerSwarm(config) => config,
        other => panic!("expected docker_swarm, got {other:?}"),
    }
}

#[test]
fn a_minimal_singleton_gets_stateful_safe_defaults() {
    let config = swarm("type = \"docker_swarm\"\nservice_id = \"e2ztjcpm0qgbqaiscyz2ngx1k\"\n");

    assert_eq!(config.service_id, "e2ztjcpm0qgbqaiscyz2ngx1k");
    assert_eq!(config.endpoint, "unix:///var/run/docker.sock");
    assert_eq!(config.game_port, 25565);
    assert_eq!(config.min_replicas, 0);
    assert_eq!(config.max_replicas, 1);
    assert_eq!(config.shutdown_after, None);
    assert_eq!(config.min_uptime, None);
    assert_eq!(config.cooldown, None);
    assert_eq!(config.readiness_timeout, Duration::from_secs(3));
}

#[test]
fn idle_timeout_min_uptime_and_cooldown_are_separate_fields() {
    let config = swarm(
        r#"
type = "docker_swarm"
service_id = "svc"
min_replicas = 1
max_replicas = 1
shutdown_after = "15m"
min_uptime = "5m"
cooldown = "90s"
start_timeout = "2m"
shutdown_timeout = "45s"
readiness_timeout = "2s"
network_id = "n3t"
"#,
    );

    assert_eq!(config.min_replicas, 1);
    assert_eq!(config.shutdown_after, Some(Duration::from_secs(15 * 60)));
    assert_eq!(config.min_uptime, Some(Duration::from_secs(5 * 60)));
    assert_eq!(config.cooldown, Some(Duration::from_secs(90)));
    assert_eq!(config.start_timeout, Duration::from_secs(120));
    assert_eq!(config.shutdown_timeout, Duration::from_secs(45));
    assert_eq!(config.readiness_timeout, Duration::from_secs(2));
    assert_eq!(config.network_id.as_deref(), Some("n3t"));
}

#[test]
fn min_replicas_above_one_is_rejected_with_a_clear_reason() {
    let error =
        parse("type = \"docker_swarm\"\nservice_id = \"svc\"\nmin_replicas = 2\n").unwrap_err();

    assert!(
        error.to_string().contains("min_replicas must be 0 or 1"),
        "{error}"
    );
}

#[test]
fn max_replicas_other_than_one_is_rejected_with_a_clear_reason() {
    for value in [0, 2, 3] {
        let text =
            format!("type = \"docker_swarm\"\nservice_id = \"svc\"\nmax_replicas = {value}\n");
        let error = parse(&text).unwrap_err();
        assert!(
            error.to_string().contains("max_replicas must be 1"),
            "{value}: {error}"
        );
    }
}

#[test]
fn unknown_swarm_keys_are_rejected() {
    let error = parse("type = \"docker_swarm\"\nservice_id = \"svc\"\nreplicas = 4\n").unwrap_err();

    assert!(error.to_string().contains("unknown field"), "{error}");
}

#[test]
fn service_id_is_required() {
    assert!(parse("type = \"docker_swarm\"\n").is_err());
}
