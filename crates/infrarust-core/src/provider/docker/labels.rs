//! Docker label parsing and address resolution.
//!
//! Converts Docker labels (`infrarust.*`) to `ServerConfig` and resolves
//! container network addresses. Standalone containers accept the original
//! key set; swarm service labels accept the wider set documented on
//! [`service_labels_to_server_config`].

use std::collections::HashMap;

use bollard::models::ContainerInspectResponse;
use serde::Serialize;
use serde::de::DeserializeOwned;
use toml::{Table, Value};

use infrarust_config::{
    BalanceStrategy, DomainRewrite, ForwardingMode, ProxyMode, ServerAddress, ServerConfig,
};

/// MOTD states accepted as `infrarust.motd.<state>.*` on swarm services.
const MOTD_STATES: [&str; 6] = [
    "online",
    "sleeping",
    "starting",
    "crashed",
    "stopping",
    "unreachable",
];

/// Which label vocabulary applies to a labelled object.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LabelScope {
    /// Standalone containers: the original `infrarust.*` keys only.
    Container,
    /// Swarm services: the container keys plus service-level settings.
    Service,
}

/// Converts Docker container labels to a `ServerConfig`.
///
/// Only the original container key set is read, so standalone container
/// discovery behaves exactly as before.
pub fn labels_to_server_config(
    container_name: &str,
    labels: &HashMap<String, String>,
    address: &str,
) -> ServerConfig {
    build_server_config(container_name, labels, address, LabelScope::Container)
}

/// Converts swarm service labels (`Spec.Labels`) to a `ServerConfig`.
///
/// Accepts the container keys plus these service-level keys:
///
/// - `infrarust.forwarding_mode`: `none`, `bungeecord`, `bungeeguard`, `velocity`
/// - `infrarust.domain_rewrite`: `none`, `from_backend`, or an explicit domain
/// - `infrarust.motd.<state>.text`, `.version_name`, `.max_players` for
///   `online`, `sleeping`, `starting`, `crashed`, `stopping`, `unreachable`
///   (`infrarust.motd.text` remains an alias for `online`)
/// - `infrarust.timeouts.connect`, `.read`, `.write` (durations, e.g. `"5s"`)
/// - `infrarust.max_players`, `infrarust.balance`, `infrarust.limbo_handlers`
///
/// Invalid values are logged and ignored one key at a time. Secrets are never
/// read from labels: server-manager credentials and favicon payloads are not
/// mapped.
pub fn service_labels_to_server_config(
    service_name: &str,
    labels: &HashMap<String, String>,
    address: &str,
) -> ServerConfig {
    build_server_config(service_name, labels, address, LabelScope::Service)
}

fn build_server_config(
    default_id: &str,
    labels: &HashMap<String, String>,
    address: &str,
    scope: LabelScope,
) -> ServerConfig {
    let domains = labels
        .get("infrarust.domains")
        .map(|d| d.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_else(|| vec![format!("{default_id}.docker.local")]);

    let proxy_mode = labels
        .get("infrarust.proxy_mode")
        .and_then(|m| match m.as_str() {
            "passthrough" => Some(ProxyMode::Passthrough),
            "client_only" => Some(ProxyMode::ClientOnly),
            "offline" => Some(ProxyMode::Offline),
            "server_only" => Some(ProxyMode::ServerOnly),
            "zero_copy" => Some(ProxyMode::ZeroCopy),
            _ => None,
        })
        .unwrap_or(ProxyMode::Passthrough);

    let name = labels.get("infrarust.name").cloned();
    let network = labels.get("infrarust.network").cloned();

    let send_proxy_protocol = labels
        .get("infrarust.send_proxy_protocol")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);

    let server_address: ServerAddress = address.parse().unwrap_or_else(|_| ServerAddress {
        host: address.to_string(),
        port: infrarust_config::DEFAULT_MC_PORT,
    });

    // Deserializing through a toml::Table (instead of a struct literal) keeps
    // serde defaults for all unset fields and escapes label values correctly.
    let built = (|| -> Result<ServerConfig, Box<dyn std::error::Error>> {
        let mut table = Table::new();
        table.insert("id".into(), Value::String(default_id.to_string()));
        table.insert(
            "domains".into(),
            Value::Array(domains.iter().cloned().map(Value::String).collect()),
        );
        table.insert(
            "addresses".into(),
            Value::Array(vec![Value::String(server_address.to_string())]),
        );
        table.insert("proxy_mode".into(), Value::try_from(proxy_mode)?);
        table.insert(
            "send_proxy_protocol".into(),
            Value::Boolean(send_proxy_protocol),
        );
        if let Some(name) = &name {
            table.insert("name".into(), Value::String(name.clone()));
        }
        if let Some(network) = &network {
            table.insert("network".into(), Value::String(network.clone()));
        }
        if let Some(text) = labels.get("infrarust.motd.text") {
            let mut online = Table::new();
            online.insert("text".into(), Value::String(text.clone()));
            let mut motd = Table::new();
            motd.insert("online".into(), Value::Table(online));
            table.insert("motd".into(), Value::Table(motd));
        }
        if scope == LabelScope::Service {
            insert_service_settings(&mut table, labels);
        }

        Ok(table.try_into()?)
    })();

    built.unwrap_or_else(|e| {
        tracing::warn!(
            container = %default_id,
            error = %e,
            "generated docker server config failed to parse, using minimal fallback"
        );
        // Manual fallback — less complete but functional
        ServerConfig {
            id: Some(default_id.to_string()),
            name,
            network,
            domains,
            addresses: vec![server_address.into()],
            balance: Default::default(),
            slow_start: None,
            slow_start_aggression: 1.0,
            active_health: None,
            proxy_mode,
            forwarding_mode: None,
            send_proxy_protocol,
            domain_rewrite: Default::default(),
            motd: Default::default(),
            server_manager: None,
            timeouts: None,
            max_players: 0,
            ip_filter: None,
            disconnect_message: None,
            limbo_handlers: Vec::new(),
            bungeecord_channel: false,
        }
    })
}

/// Adds the swarm-only keys to `table`. Each key is validated on its own so a
/// bad value only drops that setting.
fn insert_service_settings(table: &mut Table, labels: &HashMap<String, String>) {
    if let Some(value) = labels
        .get("infrarust.forwarding_mode")
        .and_then(|raw| enum_value::<ForwardingMode>("infrarust.forwarding_mode", raw))
    {
        table.insert("forwarding_mode".into(), value);
    }

    if let Some(value) = labels
        .get("infrarust.domain_rewrite")
        .and_then(|raw| domain_rewrite_value(raw))
    {
        table.insert("domain_rewrite".into(), value);
    }

    if let Some(value) = labels
        .get("infrarust.balance")
        .and_then(|raw| enum_value::<BalanceStrategy>("infrarust.balance", raw))
    {
        table.insert("balance".into(), value);
    }

    if let Some(players) = labels
        .get("infrarust.max_players")
        .and_then(|raw| parse_u32("infrarust.max_players", raw))
    {
        table.insert("max_players".into(), Value::Integer(i64::from(players)));
    }

    if let Some(raw) = labels.get("infrarust.limbo_handlers") {
        let handlers = raw
            .split(',')
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(|h| Value::String(h.to_string()))
            .collect();
        table.insert("limbo_handlers".into(), Value::Array(handlers));
    }

    let mut timeouts = Table::new();
    for field in ["connect", "read", "write"] {
        let key = format!("infrarust.timeouts.{field}");
        let Some(raw) = labels.get(&key) else {
            continue;
        };
        let raw = raw.trim();
        if humantime::parse_duration(raw).is_ok() {
            timeouts.insert(field.into(), Value::String(raw.to_string()));
        } else {
            tracing::warn!(label = %key, "ignoring invalid duration label");
        }
    }
    if !timeouts.is_empty() {
        table.insert("timeouts".into(), Value::Table(timeouts));
    }

    // `motd` already exists when the legacy `infrarust.motd.text` key was set.
    let motd = table
        .entry("motd")
        .or_insert_with(|| Value::Table(Table::new()));
    if let Value::Table(motd) = motd {
        for state in MOTD_STATES {
            let prefix = format!("infrarust.motd.{state}");
            let text = labels.get(&format!("{prefix}.text")).or_else(|| {
                (state == "online")
                    .then(|| labels.get("infrarust.motd.text"))
                    .flatten()
            });
            let Some(text) = text else {
                if labels.contains_key(&format!("{prefix}.version_name"))
                    || labels.contains_key(&format!("{prefix}.max_players"))
                {
                    tracing::warn!(
                        label = %prefix,
                        "motd state fields ignored without a .text label"
                    );
                }
                continue;
            };

            let mut entry = Table::new();
            entry.insert("text".into(), Value::String(text.clone()));
            if let Some(version) = labels.get(&format!("{prefix}.version_name")) {
                entry.insert("version_name".into(), Value::String(version.clone()));
            }
            if let Some(players) = labels
                .get(&format!("{prefix}.max_players"))
                .and_then(|raw| parse_u32(&format!("{prefix}.max_players"), raw))
            {
                entry.insert("max_players".into(), Value::Integer(i64::from(players)));
            }
            motd.insert(state.to_string(), Value::Table(entry));
        }
    }
}

/// Parses an enum-valued label through its serde representation, so the
/// accepted spellings (and aliases) match the config file.
fn enum_value<T>(key: &str, raw: &str) -> Option<Value>
where
    T: DeserializeOwned + Serialize,
{
    match Value::String(raw.trim().to_string()).try_into::<T>() {
        Ok(parsed) => Value::try_from(parsed).ok(),
        Err(e) => {
            tracing::warn!(label = %key, error = %e, "ignoring invalid label value");
            None
        }
    }
}

fn domain_rewrite_value(raw: &str) -> Option<Value> {
    let rewrite = match raw.trim() {
        "" => return None,
        "none" => DomainRewrite::None,
        "from_backend" => DomainRewrite::FromBackend,
        domain => DomainRewrite::Explicit(domain.to_string()),
    };
    Value::try_from(rewrite).ok()
}

fn parse_u32(key: &str, raw: &str) -> Option<u32> {
    match raw.trim().parse::<u32>() {
        Ok(n) => Some(n),
        Err(_) => {
            tracing::warn!(label = %key, "ignoring label: expected a non-negative integer");
            None
        }
    }
}

/// Resolves the best address for a Docker container.
///
/// Priority:
/// 1. Network IP from the preferred network (or first available)
/// 2. Port bindings → `host_ip:host_port`
/// 3. Container name as hostname
pub fn resolve_container_address(
    info: &ContainerInspectResponse,
    preferred_network: Option<&str>,
    port: u16,
) -> String {
    // 1. Network IP
    if let Some(networks) = info
        .network_settings
        .as_ref()
        .and_then(|ns| ns.networks.as_ref())
    {
        // Try preferred network first
        if let Some(net_name) = preferred_network
            && let Some(net) = networks.get(net_name)
            && let Some(ip) = &net.ip_address
            && !ip.is_empty()
        {
            return format!("{ip}:{port}");
        }
        // Try any network
        for net in networks.values() {
            if let Some(ip) = &net.ip_address
                && !ip.is_empty()
            {
                return format!("{ip}:{port}");
            }
        }
    }

    // 2. Port bindings
    if let Some(bindings) = info
        .host_config
        .as_ref()
        .and_then(|hc| hc.port_bindings.as_ref())
    {
        let key = format!("{port}/tcp");
        if let Some(Some(binding_list)) = bindings.get(&key)
            && let Some(binding) = binding_list.first()
        {
            let host_port = binding.host_port.as_deref().unwrap_or("25565");
            let host_ip = binding.host_ip.as_deref().unwrap_or("0.0.0.0");
            let actual_ip = if host_ip == "0.0.0.0" {
                "127.0.0.1"
            } else {
                host_ip
            };
            return format!("{actual_ip}:{host_port}");
        }
    }

    // 3. Container name
    let name = info
        .name
        .as_deref()
        .unwrap_or("unknown")
        .trim_start_matches('/');
    format!("{name}:{port}")
}
