//! Native Simple Voice Chat UDP proxy integration.
//!
//! This implements SVC's UDP proxy protocol. The Minecraft plugin-message
//! handshake supplies the backend voice port and UUID; UDP packets carry
//! 0xFF + UUID + encrypted payload, which selects the route.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use aes_gcm::aead::Aead;
use aes_gcm::{Aes128Gcm, KeyInit, Nonce};
use bytes::Bytes;
use infrarust_api::event::bus::EventBus;
use infrarust_api::events::connection::ServerConnectedEvent;
use infrarust_api::events::lifecycle::DisconnectEvent;
use infrarust_api::events::messaging::PluginMessageEvent;
use infrarust_api::messaging::{ChannelId, Endpoint};
use infrarust_api::plugin::{Plugin, PluginContext, PluginMetadata};
use infrarust_api::prelude::{EventBusExt, EventPriority, ListenerHandle, PluginError};
use serde::Deserialize;
use tokio::net::UdpSocket;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const SVC_MAGIC: u8 = 0xff;
const MIN_COMPATIBILITY_VERSION: i32 = 10;
const MAX_DATAGRAM_SIZE: usize = 4096;
const MAX_ROUTES: usize = 16_384;
const INGRESS_QUEUE_CAPACITY: usize = 256;
const INGRESS_WORKERS: usize = 4;
const PROXY_LISTEN_DEFAULT: &str = "0.0.0.0:25565";
const REQUEST_SECRET: (&str, &str) = ("voicechat:request_secret", "vc:request_secret");
const SECRET: (&str, &str) = ("voicechat:secret", "vc:secret");

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SvcConfig {
    /// Public UDP bind address. TCP and UDP may use the same numeric port.
    listen: String,
    /// Optional hostname sent to clients. Empty uses the Minecraft hostname.
    public_voice_host: String,
    /// Override the backend port sniffed from SVC's secret response.
    backend_port_override: Option<u16>,
    /// Highest client compatibility version understood by this proxy parser.
    max_compatibility_version: i32,
}

impl Default for SvcConfig {
    fn default() -> Self {
        Self {
            listen: PROXY_LISTEN_DEFAULT.to_owned(),
            public_voice_host: String::new(),
            backend_port_override: None,
            // Current official SVC proxy compatibility version (2.6.23).
            max_compatibility_version: 20,
        }
    }
}

#[derive(Default)]
pub struct SvcProxyPlugin {
    runtime: Mutex<Option<PluginRuntime>>,
}

struct PluginRuntime {
    state: Arc<SvcState>,
    event_bus: Arc<dyn EventBus>,
    listeners: Vec<ListenerHandle>,
}

impl Plugin for SvcProxyPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new(
            "svc-proxy",
            "Simple Voice Chat UDP Proxy",
            env!("CARGO_PKG_VERSION"),
        )
        .author("Infrarust")
        .description("Proxies Simple Voice Chat UDP traffic to each player's current backend")
    }

    fn on_enable<'a>(
        &'a self,
        ctx: &'a dyn PluginContext,
    ) -> infrarust_api::event::BoxFuture<'a, Result<(), PluginError>> {
        Box::pin(async move {
            let config = load_config(&ctx.data_dir().join("config.toml"))?;
            let listener = UdpSocket::bind(&config.listen).await.map_err(|error| {
                PluginError::from(format!(
                    "cannot bind SVC UDP listener {}: {error}",
                    config.listen
                ))
            })?;
            let state = Arc::new(SvcState::new(config));
            state.start(listener, ctx.proxy_shutdown());

            for (modern, legacy) in [REQUEST_SECRET, SECRET] {
                let channel = ChannelId::pair(modern, legacy)
                    .map_err(|error| PluginError::from(error.to_string()))?;
                ctx.channel_registrar().register(channel);
            }

            let event_bus = ctx.event_bus();
            let mut listeners = Vec::with_capacity(3);
            let plugin_messages = Arc::clone(&state);
            listeners.push(event_bus.subscribe(
                EventPriority::NORMAL,
                move |event: &mut PluginMessageEvent| match plugin_messages.on_plugin_message(event)
                {
                    Some(Some(replacement)) => event.replace(Bytes::from(replacement)),
                    Some(None) => event.handled(),
                    None => {}
                },
            ));

            let switch_state = Arc::clone(&state);
            listeners.push(event_bus.subscribe(
                EventPriority::NORMAL,
                move |event: &mut ServerConnectedEvent| {
                    if event.previous_server.is_some() {
                        switch_state.remove_player(event.player.profile().uuid);
                    }
                },
            ));

            let disconnect_state = Arc::clone(&state);
            listeners.push(event_bus.subscribe(
                EventPriority::NORMAL,
                move |event: &mut DisconnectEvent| {
                    disconnect_state.remove_player(event.player.profile().uuid);
                },
            ));

            *self.runtime.lock().unwrap_or_else(PoisonError::into_inner) = Some(PluginRuntime {
                state,
                event_bus,
                listeners,
            });
            Ok(())
        })
    }

    fn on_disable(&self) -> infrarust_api::event::BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            if let Some(runtime) = self
                .runtime
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                let PluginRuntime {
                    state,
                    event_bus,
                    listeners,
                } = runtime;
                for listener in listeners {
                    event_bus.unsubscribe(listener);
                }
                state.stop();
            }
            Ok(())
        })
    }
}

fn load_config(path: &Path) -> Result<SvcConfig, PluginError> {
    let config = match std::fs::read_to_string(path) {
        Ok(source) => toml::from_str::<SvcConfig>(&source).map_err(|error| {
            PluginError::from(format!("invalid SVC config {}: {error}", path.display()))
        })?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => SvcConfig::default(),
        Err(error) => {
            return Err(PluginError::from(format!(
                "cannot read SVC config {}: {error}",
                path.display()
            )));
        }
    };
    if config.listen.parse::<SocketAddr>().is_err() {
        return Err(PluginError::from(format!(
            "SVC listen must be a numeric socket address: {}",
            config.listen
        )));
    }
    if config
        .listen
        .parse::<SocketAddr>()
        .is_ok_and(|address| address.port() == 0)
    {
        return Err(PluginError::from(
            "SVC listen port must be between 1 and 65535",
        ));
    }
    if config.public_voice_host.len() > 32_767 || config.public_voice_host.contains('\0') {
        return Err(PluginError::from(
            "SVC public_voice_host is too long or contains NUL",
        ));
    }
    if !(MIN_COMPATIBILITY_VERSION..=255).contains(&config.max_compatibility_version) {
        return Err(PluginError::from(
            "SVC max_compatibility_version must be between 10 and 255",
        ));
    }
    if config.backend_port_override == Some(0) {
        return Err(PluginError::from(
            "SVC backend_port_override must be between 1 and 65535",
        ));
    }
    Ok(config)
}

struct SvcState {
    config: SvcConfig,
    listener: RwLock<Option<Arc<UdpSocket>>>,
    routes: RwLock<HashMap<Uuid, Arc<PlayerRoute>>>,
    proxy_uuid_by_backend_uuid: RwLock<HashMap<Uuid, Uuid>>,
    compat_versions: RwLock<HashMap<Uuid, i32>>,
    shutdown: CancellationToken,
    listener_task: Mutex<Option<JoinHandle<()>>>,
}

impl SvcState {
    fn new(config: SvcConfig) -> Self {
        Self {
            config,
            listener: RwLock::new(None),
            routes: RwLock::new(HashMap::new()),
            proxy_uuid_by_backend_uuid: RwLock::new(HashMap::new()),
            compat_versions: RwLock::new(HashMap::new()),
            shutdown: CancellationToken::new(),
            listener_task: Mutex::new(None),
        }
    }

    fn start(self: &Arc<Self>, listener: UdpSocket, proxy_shutdown: CancellationToken) {
        let listener = Arc::new(listener);
        *self
            .listener
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&listener));
        let state = Arc::clone(self);
        let shutdown = self.shutdown.clone();
        let handle = tokio::spawn(async move {
            let (tx, rx) =
                tokio::sync::mpsc::channel::<(Vec<u8>, SocketAddr)>(INGRESS_QUEUE_CAPACITY);
            let rx = Arc::new(AsyncMutex::new(rx));
            let mut workers = tokio::task::JoinSet::new();
            for _ in 0..INGRESS_WORKERS {
                let rx = Arc::clone(&rx);
                let worker_state = Arc::clone(&state);
                let worker_shutdown = shutdown.clone();
                workers.spawn(async move {
                    loop {
                        let (packet, client_address) = tokio::select! {
                            _ = worker_shutdown.cancelled() => break,
                            packet = async { rx.lock().await.recv().await } => match packet {
                                Some(packet) => packet,
                                None => break,
                            },
                        };
                        if packet.len() < 17 || packet[0] != SVC_MAGIC { continue; }
                        let Some(backend_uuid) = uuid_from_bytes(&packet[1..17]) else { continue; };
                        let proxy_uuid = worker_state.proxy_uuid_by_backend_uuid
                            .read().unwrap_or_else(PoisonError::into_inner)
                            .get(&backend_uuid).copied().unwrap_or(backend_uuid);
                        let route = worker_state.routes.read().unwrap_or_else(PoisonError::into_inner)
                            .get(&proxy_uuid).cloned();
                        let Some(route) = route else { continue; };
                        if let Err(error) = route.forward(&packet, client_address).await {
                            tracing::debug!(player = %proxy_uuid, %error, "dropping invalid or unrouteable SVC datagram");
                        }
                    }
                });
            }
            let mut buffer = [0_u8; MAX_DATAGRAM_SIZE];
            if let Ok(address) = listener.local_addr() {
                tracing::info!(bind = %address, "Simple Voice Chat UDP proxy listening");
            }
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = proxy_shutdown.cancelled() => break,
                    received = listener.recv_from(&mut buffer) => {
                        let Ok((length, client_address)) = received else { continue; };
                        match tx.try_send((buffer[..length].to_vec(), client_address)) {
                            Ok(()) => {},
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => tracing::debug!("dropping SVC datagram because ingress queue is full"),
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    }
                }
            }
            drop(tx);
            workers.abort_all();
            while workers.join_next().await.is_some() {}
            state.clear_routes();
            tracing::info!("Simple Voice Chat UDP proxy stopped");
        });
        *self
            .listener_task
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(handle);
    }

    fn stop(&self) {
        self.shutdown.cancel();
        if let Some(task) = self
            .listener_task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
        self.clear_routes();
        self.listener
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    fn clear_routes(&self) {
        let mut routes = self.routes.write().unwrap_or_else(PoisonError::into_inner);
        let mut backend_map = self
            .proxy_uuid_by_backend_uuid
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        for route in routes.values() {
            route.shutdown.cancel();
        }
        routes.clear();
        backend_map.clear();
        self.compat_versions
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    fn remove_player(&self, proxy_uuid: Uuid) {
        self.remove_route(proxy_uuid);
        self.compat_versions
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&proxy_uuid);
    }

    fn remove_route(&self, proxy_uuid: Uuid) {
        let mut routes = self.routes.write().unwrap_or_else(PoisonError::into_inner);
        let mut backend_map = self
            .proxy_uuid_by_backend_uuid
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(route) = routes.remove(&proxy_uuid) {
            route.shutdown.cancel();
        }
        backend_map.retain(|_, mapped| *mapped != proxy_uuid);
    }

    fn on_plugin_message(&self, event: &PluginMessageEvent) -> Option<Option<Vec<u8>>> {
        let proxy_uuid = event.player.profile().uuid;
        let channel = event.raw_channel.as_str();

        if event.from_client() && matches_channel(channel, REQUEST_SECRET) {
            if event.data.len() != 4 {
                return None;
            }
            let version = i32::from_be_bytes(event.data[..4].try_into().ok()?);
            self.compat_versions
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(proxy_uuid, version);
            return None;
        }

        if !matches_channel(channel, SECRET) {
            return None;
        }
        let Endpoint::Backend(server_id) = &event.source else {
            return Some(None);
        };
        if event.player.current_server().as_ref() != Some(server_id) {
            return Some(None);
        }

        let version = self
            .compat_versions
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&proxy_uuid)
            .copied();
        let Some(version) = version else {
            return Some(None);
        };
        if !compatibility_version_supported(version, self.config.max_compatibility_version) {
            tracing::info!(player = %event.player.profile().username, version, "SVC client compatibility version is unsupported by the proxy");
            return Some(None);
        }

        let Some(peer) = event.player.backend_remote_addr() else {
            return Some(None);
        };
        let Some(packet) = SecretMessage::parse(&event.data) else {
            tracing::warn!(player = %event.player.profile().username, "dropping malformed SVC secret response");
            return Some(None);
        };
        let backend_port = self
            .config
            .backend_port_override
            .unwrap_or(packet.backend_port);
        let patched = packet.patch(
            &event.data,
            self.config.public_voice_host.as_str(),
            self.listen_port(),
        );

        let listener = self
            .listener
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()?;
        let target = SocketAddr::new(peer.ip(), backend_port);
        let route = Arc::new(PlayerRoute::new(
            target,
            packet.backend_uuid,
            packet.secret,
            listener,
        ));
        let mut routes = self.routes.write().unwrap_or_else(PoisonError::into_inner);
        let mut backend_map = self
            .proxy_uuid_by_backend_uuid
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if routes.len() >= MAX_ROUTES && !routes.contains_key(&proxy_uuid) {
            tracing::warn!("SVC proxy route limit reached; withholding secret response");
            return Some(None);
        }
        if backend_map
            .get(&packet.backend_uuid)
            .is_some_and(|mapped| *mapped != proxy_uuid)
        {
            tracing::warn!(backend_uuid = %packet.backend_uuid, "SVC backend UUID is already routed to another player");
            return Some(None);
        }
        backend_map.retain(|_, mapped| *mapped != proxy_uuid);
        backend_map.insert(packet.backend_uuid, proxy_uuid);
        if let Some(previous) = routes.insert(proxy_uuid, route) {
            previous.shutdown.cancel();
        }
        Some(Some(patched))
    }

    fn listen_port(&self) -> u16 {
        self.listener
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(|socket| socket.local_addr().ok())
            .map_or(25565, |address| address.port())
    }
}

struct PlayerRoute {
    target: SocketAddr,
    backend_uuid: Uuid,
    secret: [u8; 16],
    listener: Arc<UdpSocket>,
    client_address: Arc<Mutex<Option<SocketAddr>>>,
    backend_socket: AsyncMutex<Option<Arc<UdpSocket>>>,
    shutdown: CancellationToken,
    receive_task: Mutex<Option<JoinHandle<()>>>,
}

impl PlayerRoute {
    fn new(
        target: SocketAddr,
        backend_uuid: Uuid,
        secret: [u8; 16],
        listener: Arc<UdpSocket>,
    ) -> Self {
        Self {
            target,
            backend_uuid,
            secret,
            listener,
            client_address: Arc::new(Mutex::new(None)),
            backend_socket: AsyncMutex::new(None),
            shutdown: CancellationToken::new(),
            receive_task: Mutex::new(None),
        }
    }

    async fn forward(&self, packet: &[u8], client_address: SocketAddr) -> io::Result<()> {
        if self.shutdown.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "SVC route has been removed",
            ));
        }
        if !self.authenticated_packet(packet) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SVC datagram authentication failed",
            ));
        }
        *self
            .client_address
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(client_address);
        let backend = self.backend_socket().await?;
        if self.shutdown.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "SVC route has been removed",
            ));
        }
        backend.send_to(packet, self.target).await.map(|_| ())
    }

    fn authenticated_packet(&self, packet: &[u8]) -> bool {
        if packet.len() < 18
            || packet[0] != SVC_MAGIC
            || uuid_from_bytes(&packet[1..17]) != Some(self.backend_uuid)
        {
            return false;
        }
        let Some((payload_length, length_bytes)) = read_varint(packet, 17) else {
            return false;
        };
        let Ok(payload_length) = usize::try_from(payload_length) else {
            return false;
        };
        let Some(payload_start) = 17_usize.checked_add(length_bytes) else {
            return false;
        };
        let Some(payload_end) = payload_start.checked_add(payload_length) else {
            return false;
        };
        if payload_length < 12 + 16 || payload_end != packet.len() {
            return false;
        }
        let encrypted_payload = &packet[payload_start..payload_end];
        let Ok(nonce) = Nonce::try_from(&encrypted_payload[..12]) else {
            return false;
        };
        let cipher = match Aes128Gcm::new_from_slice(&self.secret) {
            Ok(cipher) => cipher,
            Err(_) => return false,
        };
        cipher.decrypt(&nonce, &encrypted_payload[12..]).is_ok()
    }

    async fn backend_socket(&self) -> io::Result<Arc<UdpSocket>> {
        let mut guard = self.backend_socket.lock().await;
        if let Some(socket) = guard.as_ref() {
            return Ok(Arc::clone(socket));
        }
        let bind = if self.target.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = Arc::new(UdpSocket::bind(bind).await?);
        let receive_socket = Arc::clone(&socket);
        let listener = Arc::clone(&self.listener);
        let client_address = Arc::clone(&self.client_address);
        let shutdown = self.shutdown.clone();
        let target = self.target;
        let task = tokio::spawn(async move {
            let mut buffer = [0_u8; MAX_DATAGRAM_SIZE];
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    received = receive_socket.recv_from(&mut buffer) => {
                        let (length, backend_address) = match received {
                            Ok(received) => received,
                            Err(error) => {
                                tracing::debug!(%error, "SVC backend UDP receive failed; retrying");
                                tokio::select! {
                                    _ = shutdown.cancelled() => break,
                                    _ = tokio::time::sleep(Duration::from_millis(50)) => continue,
                                }
                            }
                        };
                        if backend_address != target { continue; }
                        let client = *client_address.lock().unwrap_or_else(PoisonError::into_inner);
                        let Some(client) = client else { continue; };
                        if let Err(error) = listener.send_to(&buffer[..length], client).await {
                            tracing::debug!(%error, "failed to forward SVC backend datagram to client");
                        }
                    }
                }
            }
        });
        *self
            .receive_task
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(task);
        *guard = Some(Arc::clone(&socket));
        Ok(socket)
    }
}

impl Drop for PlayerRoute {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(task) = self
            .receive_task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }
}

struct SecretMessage {
    secret: [u8; 16],
    backend_port: u16,
    backend_uuid: Uuid,
    host_length_offset: usize,
    host_length_bytes: usize,
    host_length: usize,
}

impl SecretMessage {
    fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 56 {
            return None;
        }
        let port = u32::from_be_bytes(data[16..20].try_into().ok()?);
        let backend_port = u16::try_from(port).ok()?;
        if backend_port == 0 {
            return None;
        }
        let backend_uuid = uuid_from_bytes(&data[20..36])?;
        let host_length_offset = 54;
        let (host_length, host_length_bytes) = read_varint(data, host_length_offset)?;
        let host_length = usize::try_from(host_length).ok()?;
        if host_length > 32_767 {
            return None;
        }
        let host_start = host_length_offset.checked_add(host_length_bytes)?;
        let host_end = host_start.checked_add(host_length)?;
        if host_end.checked_add(1)? > data.len() {
            return None;
        }
        std::str::from_utf8(data.get(host_start..host_end)?).ok()?;
        Some(Self {
            secret: data[..16].try_into().ok()?,
            backend_port,
            backend_uuid,
            host_length_offset,
            host_length_bytes,
            host_length,
        })
    }

    fn patch(&self, data: &[u8], public_host: &str, public_port: u16) -> Vec<u8> {
        let host_start = self.host_length_offset + self.host_length_bytes;
        let host_end = host_start + self.host_length;
        let mut patched = Vec::with_capacity(data.len() + public_host.len());
        patched.extend_from_slice(&data[..16]);
        patched.extend_from_slice(&u32::from(public_port).to_be_bytes());
        patched.extend_from_slice(&data[20..self.host_length_offset]);
        write_varint(public_host.len() as i32, &mut patched);
        patched.extend_from_slice(public_host.as_bytes());
        patched.extend_from_slice(&data[host_end..]);
        patched
    }
}

fn matches_channel(raw: &str, pair: (&str, &str)) -> bool {
    raw == pair.0 || raw == pair.1
}

fn uuid_from_bytes(bytes: &[u8]) -> Option<Uuid> {
    Some(Uuid::from_bytes(bytes.get(..16)?.try_into().ok()?))
}

fn read_varint(bytes: &[u8], offset: usize) -> Option<(i32, usize)> {
    let mut result = 0_u32;
    for index in 0..5 {
        let byte = *bytes.get(offset + index)?;
        if index == 4 && byte & 0xf0 != 0 {
            return None;
        }
        result |= u32::from(byte & 0x7f).checked_shl((index * 7) as u32)?;
        if byte & 0x80 == 0 {
            return Some((result as i32, index + 1));
        }
    }
    None
}

fn write_varint(mut value: i32, output: &mut Vec<u8>) {
    loop {
        if value & !0x7f == 0 {
            output.push(value as u8);
            return;
        }
        output.push((value & 0x7f | 0x80) as u8);
        value = ((value as u32) >> 7) as i32;
    }
}

fn compatibility_version_supported(version: i32, maximum: i32) -> bool {
    (MIN_COMPATIBILITY_VERSION..=maximum).contains(&version) && version != 19
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    fn secret_packet(port: u16, backend_uuid: Uuid, host: &str) -> Vec<u8> {
        let mut packet = vec![0x5a; 16];
        packet.extend_from_slice(&u32::from(port).to_be_bytes());
        packet.extend_from_slice(backend_uuid.as_bytes());
        packet.push(2); // codec
        packet.extend_from_slice(&1200_u32.to_be_bytes()); // MTU
        packet.extend_from_slice(&48.0_f64.to_be_bytes()); // voice distance
        packet.extend_from_slice(&1000_u32.to_be_bytes()); // keepalive
        packet.push(1); // groups enabled
        write_varint(host.len() as i32, &mut packet);
        packet.extend_from_slice(host.as_bytes());
        packet.push(1); // recording allowed
        packet
    }

    fn authenticated_voice_packet(secret: &[u8; 16], backend_uuid: Uuid) -> Vec<u8> {
        let nonce = [7_u8; 12];
        let cipher = Aes128Gcm::new_from_slice(secret).expect("valid AES key");
        let encrypted = cipher
            .encrypt(&Nonce::from(nonce), b"voice payload".as_ref())
            .expect("encrypt voice payload");
        let mut payload = nonce.to_vec();
        payload.extend_from_slice(&encrypted);
        let mut packet = vec![SVC_MAGIC];
        packet.extend_from_slice(backend_uuid.as_bytes());
        write_varint(payload.len() as i32, &mut packet);
        packet.extend_from_slice(&payload);
        packet
    }

    #[test]
    fn secret_message_parses_backend_port_uuid_and_host() {
        let backend_uuid = Uuid::new_v4();
        let packet = secret_packet(24_454, backend_uuid, "backend.example");

        let parsed = SecretMessage::parse(&packet).expect("valid secret message");

        assert_eq!(parsed.backend_port, 24_454);
        assert_eq!(parsed.backend_uuid, backend_uuid);
        let host_start = parsed.host_length_offset + parsed.host_length_bytes;
        assert_eq!(
            std::str::from_utf8(&packet[host_start..host_start + parsed.host_length])
                .expect("valid host"),
            "backend.example"
        );
    }

    #[test]
    fn secret_message_rewrites_only_advertised_host_and_voice_port() {
        let backend_uuid = Uuid::new_v4();
        let original = secret_packet(24_454, backend_uuid, "backend.example");
        let parsed = SecretMessage::parse(&original).expect("valid secret message");

        let patched = parsed.patch(&original, "voice.example", 25_565);
        let patched_secret = SecretMessage::parse(&patched).expect("patched message remains valid");
        let host_start = patched_secret.host_length_offset + patched_secret.host_length_bytes;

        assert_eq!(patched_secret.backend_port, 25_565);
        assert_eq!(patched_secret.backend_uuid, backend_uuid);
        assert_eq!(
            &patched[host_start..host_start + patched_secret.host_length],
            b"voice.example"
        );
        assert_eq!(patched.last(), original.last());
        assert_eq!(&patched[..16], &original[..16]);
    }

    #[test]
    fn empty_public_host_preserves_minecraft_hostname_fallback() {
        let original = secret_packet(24_454, Uuid::new_v4(), "backend.example");
        let patched = SecretMessage::parse(&original)
            .expect("valid secret")
            .patch(&original, "", 25_565);
        let parsed = SecretMessage::parse(&patched).expect("valid patched secret");
        assert_eq!(parsed.host_length, 0);
        assert_eq!(parsed.backend_port, 25_565);
    }

    #[test]
    fn secret_message_rejects_invalid_ports_and_truncated_fields() {
        let backend_uuid = Uuid::new_v4();
        let mut zero_port = secret_packet(24_454, backend_uuid, "backend");
        zero_port[16..20].copy_from_slice(&0_u32.to_be_bytes());
        let mut oversized_port = secret_packet(24_454, backend_uuid, "backend");
        oversized_port[16..20].copy_from_slice(&65_536_u32.to_be_bytes());
        let truncated = secret_packet(24_454, backend_uuid, "backend")[..55].to_vec();

        assert!(SecretMessage::parse(&zero_port).is_none());
        assert!(SecretMessage::parse(&oversized_port).is_none());
        assert!(SecretMessage::parse(&truncated).is_none());
    }

    #[test]
    fn compatibility_gate_matches_upstream_supported_range_and_exclusion() {
        assert!(!compatibility_version_supported(9, 20));
        assert!(compatibility_version_supported(10, 20));
        assert!(!compatibility_version_supported(19, 20));
        assert!(compatibility_version_supported(20, 20));
        assert!(!compatibility_version_supported(21, 20));
    }

    #[test]
    fn varint_reader_rejects_overflow_in_fifth_byte() {
        assert!(read_varint(&[0xff, 0xff, 0xff, 0xff, 0x10], 0).is_none());
        assert_eq!(
            read_varint(&[0xff, 0xff, 0xff, 0xff, 0x07], 0),
            Some((i32::MAX, 5))
        );
    }

    #[tokio::test]
    async fn udp_route_accepts_client_ip_distinct_from_backend_and_cleans_up() {
        let public_listener = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("public listener bind");
        let public_address = public_listener
            .local_addr()
            .expect("public listener address");
        let backend = UdpSocket::bind("127.0.0.1:0").await.expect("backend bind");
        let backend_address = backend.local_addr().expect("backend address");
        let client = UdpSocket::bind("127.0.0.2:0")
            .await
            .expect("client bind on a different loopback IP than backend");
        let proxy_uuid = Uuid::new_v4();
        let backend_uuid = Uuid::new_v4();
        let state = Arc::new(SvcState::new(SvcConfig::default()));
        state.start(public_listener, CancellationToken::new());
        let secret = [9_u8; 16];
        let route = Arc::new(PlayerRoute::new(
            backend_address,
            backend_uuid,
            secret,
            state
                .listener
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
                .expect("listener is installed"),
        ));
        state
            .routes
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(proxy_uuid, Arc::clone(&route));
        state
            .proxy_uuid_by_backend_uuid
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(backend_uuid, proxy_uuid);
        state
            .compat_versions
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(proxy_uuid, 20);
        drop(route);

        let packet = authenticated_voice_packet(&secret, backend_uuid);
        client
            .send_to(&packet, public_address)
            .await
            .expect("send client voice packet");

        let mut received = [0_u8; MAX_DATAGRAM_SIZE];
        let (length, bridge_address) =
            timeout(Duration::from_secs(2), backend.recv_from(&mut received))
                .await
                .expect("backend receives client packet")
                .expect("backend UDP receive");
        assert_eq!(&received[..length], packet.as_slice());

        backend
            .send_to(b"encrypted backend reply", bridge_address)
            .await
            .expect("send backend voice packet");
        let (length, source) = timeout(Duration::from_secs(2), client.recv_from(&mut received))
            .await
            .expect("client receives backend packet")
            .expect("client UDP receive");
        assert_eq!(&received[..length], b"encrypted backend reply");
        assert_eq!(source, public_address);

        state.remove_player(proxy_uuid);
        assert!(
            !state
                .routes
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key(&proxy_uuid)
        );
        assert!(
            !state
                .proxy_uuid_by_backend_uuid
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key(&backend_uuid)
        );
        assert!(
            !state
                .compat_versions
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key(&proxy_uuid)
        );
        state.stop();
    }

    #[tokio::test]
    async fn unauthenticated_sender_cannot_claim_client_route() {
        let public_listener = UdpSocket::bind("127.0.0.1:0").await.expect("public bind");
        let public_address = public_listener.local_addr().expect("public address");
        let backend = UdpSocket::bind("127.0.0.1:0").await.expect("backend bind");
        let backend_address = backend.local_addr().expect("backend address");
        let client = UdpSocket::bind("127.0.0.1:0").await.expect("client bind");
        let attacker = UdpSocket::bind("127.0.0.1:0").await.expect("attacker bind");
        let proxy_uuid = Uuid::new_v4();
        let backend_uuid = Uuid::new_v4();
        let secret = [3_u8; 16];
        let state = Arc::new(SvcState::new(SvcConfig::default()));
        state.start(public_listener, CancellationToken::new());
        let listener = state
            .listener
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .expect("listener is installed");
        state
            .routes
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                proxy_uuid,
                Arc::new(PlayerRoute::new(
                    backend_address,
                    backend_uuid,
                    secret,
                    listener,
                )),
            );
        state
            .proxy_uuid_by_backend_uuid
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(backend_uuid, proxy_uuid);

        let forged = authenticated_voice_packet(&[4_u8; 16], backend_uuid);
        attacker
            .send_to(&forged, public_address)
            .await
            .expect("send forged packet");
        let mut buffer = [0_u8; MAX_DATAGRAM_SIZE];
        assert!(
            timeout(Duration::from_millis(150), backend.recv_from(&mut buffer))
                .await
                .is_err()
        );

        let valid = authenticated_voice_packet(&secret, backend_uuid);
        client
            .send_to(&valid, public_address)
            .await
            .expect("send valid packet");
        let (length, bridge_address) =
            timeout(Duration::from_secs(2), backend.recv_from(&mut buffer))
                .await
                .expect("backend receives valid packet")
                .expect("backend receive");
        assert_eq!(&buffer[..length], valid.as_slice());
        backend
            .send_to(b"reply", bridge_address)
            .await
            .expect("send reply");
        let (length, _) = timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
            .await
            .expect("client receives reply")
            .expect("client receive");
        assert_eq!(&buffer[..length], b"reply");
        assert!(
            timeout(Duration::from_millis(150), attacker.recv_from(&mut buffer))
                .await
                .is_err()
        );
        state.stop();
    }
}
