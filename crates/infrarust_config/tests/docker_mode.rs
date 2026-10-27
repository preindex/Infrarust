#![allow(clippy::unwrap_used, clippy::expect_used)]

use infrarust_config::{DockerMode, ProxyConfig, validate_proxy_config};

fn proxy_from_toml(toml: &str, servers_dir: &std::path::Path) -> ProxyConfig {
    let mut config: ProxyConfig = toml::from_str(toml).expect("failed to parse TOML");
    config.servers_dir = servers_dir.to_path_buf();
    config
}

#[test]
fn docker_mode_defaults_to_container() {
    let config: ProxyConfig = toml::from_str("[docker]").expect("failed to parse TOML");
    let docker = config.docker.expect("[docker] section present");
    assert_eq!(docker.mode, DockerMode::Container);
}

#[test]
fn docker_mode_container_needs_no_network() {
    let dir = tempfile::tempdir().unwrap();
    let config = proxy_from_toml(
        r#"
        [docker]
        mode = "container"
    "#,
        dir.path(),
    );
    assert!(validate_proxy_config(&config).is_ok());
}

#[test]
fn docker_mode_swarm_parses() {
    let config: ProxyConfig = toml::from_str(
        r#"
        [docker]
        mode = "swarm"
        network = "mc-overlay"
    "#,
    )
    .expect("failed to parse TOML");
    let docker = config.docker.expect("[docker] section present");
    assert_eq!(docker.mode, DockerMode::Swarm);
    assert_eq!(docker.network.as_deref(), Some("mc-overlay"));
}

#[test]
fn docker_mode_swarm_requires_a_network() {
    let dir = tempfile::tempdir().unwrap();
    let missing = proxy_from_toml(
        r#"
        [docker]
        mode = "swarm"
    "#,
        dir.path(),
    );
    assert!(validate_proxy_config(&missing).is_err());

    let blank = proxy_from_toml(
        r#"
        [docker]
        mode = "swarm"
        network = "   "
    "#,
        dir.path(),
    );
    assert!(validate_proxy_config(&blank).is_err());
}

#[test]
fn docker_mode_swarm_does_not_require_a_file_provider_directory() {
    let missing = std::path::Path::new("/definitely/not/a/servers/directory");
    let config = proxy_from_toml(
        r#"
        [docker]
        mode = "swarm"
        network = "mc-overlay"
    "#,
        missing,
    );
    assert!(validate_proxy_config(&config).is_ok());
}

#[test]
fn docker_mode_rejects_unknown_values() {
    let result: Result<ProxyConfig, _> = toml::from_str(
        r#"
        [docker]
        mode = "portainer"
    "#,
    );
    assert!(result.is_err());
}
