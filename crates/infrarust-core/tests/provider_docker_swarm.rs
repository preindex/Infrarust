#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(feature = "docker")]

//! Swarm service label mapping, snapshot building, and poll diffing.
//! All fixtures are typed bollard models: no daemon is needed.

use std::collections::HashMap;
use std::time::Duration;

use bollard::models::{
    EndpointSpec, EndpointSpecModeEnum, Service, ServiceEndpoint, ServiceEndpointVirtualIps,
    ServiceSpec, ServiceSpecMode, ServiceSpecModeReplicated, ServiceSpecModeReplicatedJob,
    ServiceSpecUpdateConfig, ServiceSpecUpdateConfigOrderEnum, Task, TaskState, TaskStatus,
};
use infrarust_config::{BalanceStrategy, DomainRewrite, ForwardingMode, ProxyMode, ServerConfig};
use infrarust_core::provider::ProviderEvent;
use infrarust_core::provider::docker::labels::{
    labels_to_server_config, service_labels_to_server_config,
};
use infrarust_core::provider::docker::swarm::{
    SkipReason, build_snapshot, count_running_tasks, diff_routes,
};

const OVERLAY_ID: &str = "overlay-net-id";

fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Labels for a service that opts into discovery, plus extras.
fn enabled(extra: &[(&str, &str)]) -> HashMap<String, String> {
    let mut all = labels(&[("infrarust.enable", "true")]);
    all.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    all
}

fn replicated(replicas: i64) -> ServiceSpecMode {
    ServiceSpecMode {
        replicated: Some(ServiceSpecModeReplicated {
            replicas: Some(replicas),
        }),
        ..Default::default()
    }
}

fn vip(network_id: &str, addr: &str) -> ServiceEndpointVirtualIps {
    ServiceEndpointVirtualIps {
        network_id: Some(network_id.to_string()),
        addr: Some(addr.to_string()),
    }
}

fn service(
    id: &str,
    name: &str,
    labels: HashMap<String, String>,
    mode: ServiceSpecMode,
    vips: Vec<ServiceEndpointVirtualIps>,
) -> Service {
    Service {
        id: Some(id.to_string()),
        spec: Some(ServiceSpec {
            name: Some(name.to_string()),
            labels: Some(labels),
            mode: Some(mode),
            ..Default::default()
        }),
        endpoint: Some(ServiceEndpoint {
            virtual_ips: Some(vips),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A routable service: one replica, VIP on the configured overlay.
fn routable(id: &str, name: &str, vip_addr: &str) -> Service {
    service(
        id,
        name,
        enabled(&[]),
        replicated(1),
        vec![vip(OVERLAY_ID, vip_addr)],
    )
}

fn running_task(service_id: &str) -> Task {
    Task {
        service_id: Some(service_id.to_string()),
        status: Some(TaskStatus {
            state: Some(TaskState::RUNNING),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn skip_reason(services: &[Service], running: &HashMap<String, usize>) -> SkipReason {
    let snapshot = build_snapshot(services, running, OVERLAY_ID);
    assert!(snapshot.routes.is_empty(), "service should not be routed");
    assert_eq!(snapshot.skipped.len(), 1, "exactly one skip expected");
    snapshot.skipped[0].reason.clone()
}

// ---------------------------------------------------------------------------
// Label mapping
// ---------------------------------------------------------------------------

#[test]
fn service_labels_map_service_level_settings() {
    let map = labels(&[
        ("infrarust.enable", "true"),
        ("infrarust.domains", "a.example, b.example"),
        ("infrarust.name", "survival"),
        ("infrarust.network", "main"),
        ("infrarust.proxy_mode", "client_only"),
        ("infrarust.send_proxy_protocol", "true"),
        ("infrarust.forwarding_mode", "velocity"),
        ("infrarust.domain_rewrite", "from_backend"),
        ("infrarust.balance", "least_conn"),
        ("infrarust.max_players", "200"),
        ("infrarust.limbo_handlers", "auth, queue"),
        ("infrarust.timeouts.connect", "3s"),
        ("infrarust.timeouts.read", "20s"),
        ("infrarust.motd.text", "Hello"),
        ("infrarust.motd.sleeping.text", "Sleeping"),
        ("infrarust.motd.sleeping.version_name", "Zzz"),
        ("infrarust.motd.sleeping.max_players", "0"),
        ("infrarust.motd.crashed.text", "Down"),
    ]);
    let config = service_labels_to_server_config("svc", &map, "10.0.1.5:25565");

    assert_eq!(config.domains, vec!["a.example", "b.example"]);
    assert_eq!(config.effective_id(), "survival");
    assert_eq!(config.network.as_deref(), Some("main"));
    assert_eq!(config.proxy_mode, ProxyMode::ClientOnly);
    assert!(config.send_proxy_protocol);
    assert_eq!(config.forwarding_mode, Some(ForwardingMode::Velocity));
    assert_eq!(config.domain_rewrite, DomainRewrite::FromBackend);
    assert_eq!(config.balance, BalanceStrategy::LeastConn);
    assert_eq!(config.max_players, 200);
    assert_eq!(config.limbo_handlers, vec!["auth", "queue"]);

    let timeouts = config.timeouts.expect("timeouts set");
    assert_eq!(timeouts.connect, Duration::from_secs(3));
    assert_eq!(timeouts.read, Duration::from_secs(20));

    assert_eq!(config.motd.online.as_ref().unwrap().text, "Hello");
    let sleeping = config.motd.sleeping.as_ref().unwrap();
    assert_eq!(sleeping.text, "Sleeping");
    assert_eq!(sleeping.version_name.as_deref(), Some("Zzz"));
    assert_eq!(sleeping.max_players, Some(0));
    assert_eq!(config.motd.crashed.as_ref().unwrap().text, "Down");
    assert!(config.motd.starting.is_none());
}

#[test]
fn service_labels_explicit_domain_rewrite() {
    let map = enabled(&[("infrarust.domain_rewrite", "mc.internal.example")]);
    let config = service_labels_to_server_config("svc", &map, "10.0.1.5:25565");
    assert_eq!(
        config.domain_rewrite,
        DomainRewrite::Explicit("mc.internal.example".to_string())
    );
}

#[test]
fn invalid_service_label_values_drop_only_that_key() {
    let map = enabled(&[
        ("infrarust.domains", "a.example"),
        ("infrarust.forwarding_mode", "bogus"),
        ("infrarust.max_players", "lots"),
        ("infrarust.timeouts.connect", "soon"),
        ("infrarust.balance", "round_robin"),
    ]);
    let config = service_labels_to_server_config("svc", &map, "10.0.1.5:25565");

    assert_eq!(config.domains, vec!["a.example"]);
    assert_eq!(config.forwarding_mode, None);
    assert_eq!(config.max_players, 0);
    assert!(config.timeouts.is_none());
    assert_eq!(config.balance, BalanceStrategy::RoundRobin);
}

#[test]
fn container_labels_ignore_service_only_keys() {
    // Standalone container discovery must not change: the swarm-only keys
    // are not read from container labels.
    let map = enabled(&[
        ("infrarust.forwarding_mode", "velocity"),
        ("infrarust.max_players", "200"),
        ("infrarust.limbo_handlers", "auth"),
        ("infrarust.balance", "least_conn"),
        ("infrarust.motd.sleeping.text", "Sleeping"),
        ("infrarust.motd.text", "Legacy"),
    ]);
    let config = labels_to_server_config("mc-test", &map, "172.17.0.2:25565");

    assert_eq!(config.forwarding_mode, None);
    assert_eq!(config.max_players, 0);
    assert!(config.limbo_handlers.is_empty());
    assert_eq!(config.balance, BalanceStrategy::FirstAvailable);
    assert!(config.motd.sleeping.is_none());
    assert_eq!(config.motd.online.as_ref().unwrap().text, "Legacy");
}

#[test]
fn service_labels_never_configure_server_manager() {
    let map = enabled(&[
        ("infrarust.server_manager.provider", "pterodactyl"),
        ("infrarust.server_manager.api_key", "super-secret"),
    ]);
    let config: ServerConfig = service_labels_to_server_config("svc", &map, "10.0.1.5:25565");
    assert!(config.server_manager.is_none());
}

// ---------------------------------------------------------------------------
// Snapshot: routing target
// ---------------------------------------------------------------------------

#[test]
fn routable_service_targets_its_overlay_vip() {
    let services = vec![routable("svc-1", "mc-survival", "10.0.1.5/24")];
    let snapshot = build_snapshot(&services, &HashMap::new(), OVERLAY_ID);

    assert!(snapshot.skipped.is_empty());
    assert_eq!(snapshot.routes.len(), 1);
    let route = &snapshot.routes[0];
    assert_eq!(route.id.to_string(), "docker@swarm:svc-1");
    assert_eq!(route.config.effective_id(), "mc-survival");
    assert_eq!(route.config.domains, vec!["mc-survival.docker.local"]);
    assert_eq!(route.config.addresses[0].address.host, "10.0.1.5");
    assert_eq!(route.config.addresses[0].address.port, 25565);
}

#[test]
fn port_label_selects_the_backend_port() {
    let services = vec![service(
        "svc-1",
        "mc-survival",
        enabled(&[("infrarust.port", "25566")]),
        replicated(1),
        vec![vip(OVERLAY_ID, "10.0.1.5/24")],
    )];
    let snapshot = build_snapshot(&services, &HashMap::new(), OVERLAY_ID);
    assert_eq!(snapshot.routes[0].config.addresses[0].address.port, 25566);
}

#[test]
fn zero_replica_service_is_registered() {
    let services = vec![service(
        "svc-0",
        "mc-idle",
        enabled(&[]),
        replicated(0),
        vec![vip(OVERLAY_ID, "10.0.1.9/24")],
    )];
    let snapshot = build_snapshot(&services, &HashMap::new(), OVERLAY_ID);
    assert_eq!(snapshot.routes.len(), 1);
    assert_eq!(snapshot.routes[0].id.to_string(), "docker@swarm:svc-0");
}

#[test]
fn vip_on_another_network_is_not_routed() {
    let services = vec![service(
        "svc-1",
        "mc-survival",
        enabled(&[]),
        replicated(1),
        vec![vip("ingress-net-id", "10.0.0.5/24")],
    )];
    assert_eq!(
        skip_reason(&services, &HashMap::new()),
        SkipReason::NotOnOverlay
    );
}

#[test]
fn unlabelled_services_are_ignored_silently() {
    let services = vec![service(
        "svc-1",
        "other",
        labels(&[("team", "x")]),
        replicated(1),
        vec![vip(OVERLAY_ID, "10.0.1.5/24")],
    )];
    let snapshot = build_snapshot(&services, &HashMap::new(), OVERLAY_ID);
    assert!(snapshot.routes.is_empty());
    assert!(snapshot.skipped.is_empty());
}

// ---------------------------------------------------------------------------
// Snapshot: unsupported services are skipped, never routed
// ---------------------------------------------------------------------------

#[test]
fn global_service_is_skipped() {
    let mode = ServiceSpecMode {
        global: Some(HashMap::new()),
        ..Default::default()
    };
    let services = vec![service(
        "svc-g",
        "mc-global",
        enabled(&[]),
        mode,
        vec![vip(OVERLAY_ID, "10.0.1.5/24")],
    )];
    assert_eq!(skip_reason(&services, &HashMap::new()), SkipReason::Global);
}

#[test]
fn job_service_is_skipped() {
    let mode = ServiceSpecMode {
        replicated_job: Some(ServiceSpecModeReplicatedJob::default()),
        ..Default::default()
    };
    let services = vec![service(
        "svc-j",
        "mc-job",
        enabled(&[]),
        mode,
        vec![vip(OVERLAY_ID, "10.0.1.5/24")],
    )];
    assert_eq!(skip_reason(&services, &HashMap::new()), SkipReason::Job);
}

#[test]
fn dnsrr_service_is_skipped() {
    let mut service = routable("svc-d", "mc-dnsrr", "10.0.1.5/24");
    service.spec.as_mut().unwrap().endpoint_spec = Some(EndpointSpec {
        mode: Some(EndpointSpecModeEnum::DNSRR),
        ..Default::default()
    });
    assert_eq!(skip_reason(&[service], &HashMap::new()), SkipReason::DnsRr);
}

#[test]
fn start_first_update_is_skipped() {
    let mut service = routable("svc-s", "mc-start-first", "10.0.1.5/24");
    service.spec.as_mut().unwrap().update_config = Some(ServiceSpecUpdateConfig {
        order: Some(ServiceSpecUpdateConfigOrderEnum::START_FIRST),
        ..Default::default()
    });
    assert_eq!(
        skip_reason(&[service], &HashMap::new()),
        SkipReason::StartFirst
    );
}

#[test]
fn stop_first_update_is_routed() {
    let mut service = routable("svc-s", "mc-stop-first", "10.0.1.5/24");
    service.spec.as_mut().unwrap().update_config = Some(ServiceSpecUpdateConfig {
        order: Some(ServiceSpecUpdateConfigOrderEnum::STOP_FIRST),
        ..Default::default()
    });
    let snapshot = build_snapshot(&[service], &HashMap::new(), OVERLAY_ID);
    assert_eq!(snapshot.routes.len(), 1);
}

#[test]
fn multiple_replicas_are_skipped() {
    let services = vec![service(
        "svc-m",
        "mc-scaled",
        enabled(&[]),
        replicated(2),
        vec![vip(OVERLAY_ID, "10.0.1.5/24")],
    )];
    assert_eq!(
        skip_reason(&services, &HashMap::new()),
        SkipReason::MultipleReplicas { replicas: 2 }
    );
}

#[test]
fn multiple_running_tasks_are_skipped() {
    let services = vec![routable("svc-1", "mc-survival", "10.0.1.5/24")];
    let running = HashMap::from([("svc-1".to_string(), 2)]);
    assert_eq!(
        skip_reason(&services, &running),
        SkipReason::MultipleRunningTasks { running: 2 }
    );
}

#[test]
fn service_without_a_mode_is_skipped() {
    let mut service = routable("svc-n", "mc-nomode", "10.0.1.5/24");
    service.spec.as_mut().unwrap().mode = None;
    assert_eq!(
        skip_reason(&[service], &HashMap::new()),
        SkipReason::UnknownMode
    );
}

#[test]
fn invalid_server_name_is_skipped_with_reason() {
    let services = vec![routable("svc-1", "MC_Survival", "10.0.1.5/24")];
    match skip_reason(&services, &HashMap::new()) {
        SkipReason::InvalidConfig(_) => {}
        other => panic!("expected InvalidConfig, got {other:?}"),
    }
}

#[test]
fn one_unsupported_service_does_not_hide_the_others() {
    let services = vec![
        routable("svc-1", "mc-survival", "10.0.1.5/24"),
        service(
            "svc-m",
            "mc-scaled",
            enabled(&[]),
            replicated(3),
            vec![vip(OVERLAY_ID, "10.0.1.6/24")],
        ),
    ];
    let snapshot = build_snapshot(&services, &HashMap::new(), OVERLAY_ID);
    assert_eq!(snapshot.routes.len(), 1);
    assert_eq!(snapshot.routes[0].id.to_string(), "docker@swarm:svc-1");
    assert_eq!(snapshot.skipped.len(), 1);
    assert_eq!(snapshot.skipped[0].service_id, "svc-m");
}

// ---------------------------------------------------------------------------
// Task counting
// ---------------------------------------------------------------------------

#[test]
fn only_running_tasks_with_a_service_are_counted() {
    let pending = Task {
        service_id: Some("svc-a".to_string()),
        status: Some(TaskStatus {
            state: Some(TaskState::PENDING),
            ..Default::default()
        }),
        ..Default::default()
    };
    let orphan = Task {
        status: Some(TaskStatus {
            state: Some(TaskState::RUNNING),
            ..Default::default()
        }),
        ..Default::default()
    };
    let tasks = vec![
        running_task("svc-a"),
        running_task("svc-a"),
        pending,
        running_task("svc-b"),
        orphan,
    ];

    let counts = count_running_tasks(&tasks);
    assert_eq!(counts.get("svc-a"), Some(&2));
    assert_eq!(counts.get("svc-b"), Some(&1));
    assert_eq!(counts.len(), 2);
}

// ---------------------------------------------------------------------------
// Poll diffing
// ---------------------------------------------------------------------------

#[test]
fn first_poll_adds_every_route() {
    let services = vec![
        routable("svc-1", "mc-a", "10.0.1.5/24"),
        routable("svc-2", "mc-b", "10.0.1.6/24"),
    ];
    let snapshot = build_snapshot(&services, &HashMap::new(), OVERLAY_ID);
    let mut known = HashMap::new();

    let events = diff_routes(&mut known, snapshot.routes.clone());
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| matches!(e, ProviderEvent::Added(_))));
    assert_eq!(known.len(), 2);
}

#[test]
fn steady_poll_emits_nothing() {
    let services = vec![routable("svc-1", "mc-a", "10.0.1.5/24")];
    let snapshot = build_snapshot(&services, &HashMap::new(), OVERLAY_ID);
    let mut known = HashMap::new();
    diff_routes(&mut known, snapshot.routes.clone());

    let again = diff_routes(&mut known, snapshot.routes.clone());
    assert!(again.is_empty());
}

#[test]
fn changed_vip_emits_update() {
    let mut known = HashMap::new();
    let first = build_snapshot(
        &[routable("svc-1", "mc-a", "10.0.1.5/24")],
        &HashMap::new(),
        OVERLAY_ID,
    );
    diff_routes(&mut known, first.routes.clone());

    let moved = build_snapshot(
        &[routable("svc-1", "mc-a", "10.0.1.99/24")],
        &HashMap::new(),
        OVERLAY_ID,
    );
    let events = diff_routes(&mut known, moved.routes.clone());
    assert_eq!(events.len(), 1);
    match &events[0] {
        ProviderEvent::Updated(pc) => {
            assert_eq!(pc.config.addresses[0].address.host, "10.0.1.99");
        }
        other => panic!("expected Updated, got {other:?}"),
    }
}

#[test]
fn service_leaving_the_snapshot_emits_removed() {
    let mut known = HashMap::new();
    let both = build_snapshot(
        &[
            routable("svc-1", "mc-a", "10.0.1.5/24"),
            routable("svc-2", "mc-b", "10.0.1.6/24"),
        ],
        &HashMap::new(),
        OVERLAY_ID,
    );
    diff_routes(&mut known, both.routes.clone());

    let one = build_snapshot(
        &[routable("svc-1", "mc-a", "10.0.1.5/24")],
        &HashMap::new(),
        OVERLAY_ID,
    );
    let events = diff_routes(&mut known, one.routes.clone());
    assert_eq!(events.len(), 1);
    match &events[0] {
        ProviderEvent::Removed(id) => assert_eq!(id.to_string(), "docker@swarm:svc-2"),
        other => panic!("expected Removed, got {other:?}"),
    }
    assert_eq!(known.len(), 1);
}

#[test]
fn service_becoming_unsupported_emits_removed() {
    let mut known = HashMap::new();
    let routed = build_snapshot(
        &[routable("svc-1", "mc-a", "10.0.1.5/24")],
        &HashMap::new(),
        OVERLAY_ID,
    );
    diff_routes(&mut known, routed.routes.clone());

    // Scaled to two replicas: no longer routable, so the route is withdrawn.
    let scaled = build_snapshot(
        &[service(
            "svc-1",
            "mc-a",
            enabled(&[]),
            replicated(2),
            vec![vip(OVERLAY_ID, "10.0.1.5/24")],
        )],
        &HashMap::new(),
        OVERLAY_ID,
    );
    let events = diff_routes(&mut known, scaled.routes.clone());
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], ProviderEvent::Removed(_)));
    assert!(known.is_empty());
}
