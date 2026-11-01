#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Docker Swarm provider against a scripted Docker manager API.
//!
//! The mock speaks the Docker HTTP API over TCP, so these tests exercise the
//! real Bollard requests: the version sent on update, the replica-only body,
//! conflict retries, the stop wait, and the readiness chain.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use infrarust_config::DockerSwarmManagerConfig;
use infrarust_server_manager::{
    ProviderStatus, ServerManagerError, ServerProvider, SwarmServerProvider,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const SERVICE_ID: &str = "e2ztjcpm0qgbqaiscyz2ngx1k";
const FIXTURE: &str = include_str!("fixtures/swarm_service.json");

#[derive(Clone)]
struct Reply {
    status: u16,
    body: String,
}

fn ok(body: Value) -> Reply {
    Reply {
        status: 200,
        body: body.to_string(),
    }
}

fn fail(status: u16, message: &str) -> Reply {
    Reply {
        status,
        body: json!({ "message": message }).to_string(),
    }
}

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
    body: String,
}

/// Route: method + path fragment, answered from a queue. The last entry repeats.
struct Route {
    method: &'static str,
    path: String,
    replies: VecDeque<Reply>,
}

fn route(method: &'static str, path: impl Into<String>, replies: Vec<Reply>) -> Route {
    Route {
        method,
        path: path.into(),
        replies: replies.into(),
    }
}

#[derive(Default)]
struct Script {
    routes: Vec<Route>,
    requests: Vec<Recorded>,
}

impl Script {
    fn answer(&mut self, method: &str, path: &str, body: &str) -> Reply {
        self.requests.push(Recorded {
            method: method.to_string(),
            path: path.to_string(),
            body: body.to_string(),
        });
        let Some(route) = self
            .routes
            .iter_mut()
            .find(|r| r.method == method && path.contains(&r.path))
        else {
            return fail(404, "no scripted route");
        };
        if route.replies.len() > 1 {
            route.replies.pop_front().unwrap()
        } else {
            route.replies.front().cloned().unwrap()
        }
    }
}

struct MockDocker {
    addr: SocketAddr,
    script: Arc<Mutex<Script>>,
}

impl MockDocker {
    async fn start(routes: Vec<Route>) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let script = Arc::new(Mutex::new(Script {
            routes,
            requests: Vec::new(),
        }));
        let shared = Arc::clone(&script);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(serve_connection(stream, Arc::clone(&shared)));
            }
        });
        Self { addr, script }
    }

    fn endpoint(&self) -> String {
        format!("tcp://{}", self.addr)
    }

    fn requests(&self) -> Vec<Recorded> {
        self.script.lock().unwrap().requests.clone()
    }

    fn requests_to(&self, method: &str, fragment: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == method && r.path.contains(fragment))
            .collect()
    }
}

async fn serve_connection(mut stream: tokio::net::TcpStream, script: Arc<Mutex<Script>>) {
    let mut buf = Vec::new();
    loop {
        let header_end = loop {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            let mut chunk = [0u8; 4096];
            let n = match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let content_length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while buf.len() < header_end + content_length {
            let mut chunk = [0u8; 4096];
            let n = match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
        }
        let body =
            String::from_utf8_lossy(&buf[header_end..header_end + content_length]).to_string();
        buf.drain(..header_end + content_length);

        let request_line = head.lines().next().unwrap_or_default();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();
        let reply = script.lock().unwrap().answer(&method, &path, &body);

        let response = format!(
            "HTTP/1.1 {} STATUS\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            reply.status,
            reply.body.len(),
            reply.body
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// A Minecraft status responder that answers every connection with valid JSON.
async fn spawn_status_server() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut scratch = vec![0u8; 512];
                let _ = stream.read(&mut scratch).await;
                let json = br#"{"version":{"name":"1.21","protocol":767}}"#;
                let mut body = vec![0x00, json.len() as u8];
                body.extend_from_slice(json);
                let mut frame = vec![body.len() as u8];
                frame.extend_from_slice(&body);
                let _ = stream.write_all(&frame).await;
            });
        }
    });
    port
}

/// A port with nothing listening on it.
async fn closed_port() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// Fixture service with its version, replica count, and VIP adjusted.
fn service(version: u64, replicas: i64, vip: &str) -> Value {
    let mut raw: Value = serde_json::from_str(FIXTURE).unwrap();
    raw["Version"]["Index"] = json!(version);
    raw["Spec"]["Mode"]["Replicated"]["Replicas"] = json!(replicas);
    raw["Endpoint"]["VirtualIPs"][0]["Addr"] = json!(vip);
    raw
}

fn with_label(mut raw: Value, key: &str, value: Option<&str>) -> Value {
    let labels = raw["Spec"]["Labels"].as_object_mut().unwrap();
    match value {
        Some(v) => labels.insert(key.to_string(), json!(v)),
        None => labels.remove(key),
    };
    raw
}

fn task(state: &str, desired: &str) -> Value {
    json!({
        "ID": format!("task-{state}-{desired}"),
        "ServiceID": SERVICE_ID,
        "NodeID": "node1",
        "DesiredState": desired,
        "Status": { "State": state }
    })
}

fn node(state: &str) -> Value {
    json!({ "ID": "node1", "Status": { "State": state } })
}

fn config(
    endpoint: String,
    game_port: u16,
    shutdown_timeout: Duration,
) -> DockerSwarmManagerConfig {
    DockerSwarmManagerConfig {
        endpoint,
        service_id: SERVICE_ID.to_string(),
        game_port,
        network_id: None,
        min_replicas: 0,
        max_replicas: 1,
        shutdown_after: None,
        min_uptime: None,
        cooldown: None,
        start_timeout: Duration::from_secs(30),
        shutdown_timeout,
        readiness_timeout: Duration::from_secs(1),
        poll_interval: Duration::from_secs(5),
    }
}

fn inspect_route(replies: Vec<Reply>) -> Route {
    route("GET", format!("/services/{SERVICE_ID}"), replies)
}

fn update_route(replies: Vec<Reply>) -> Route {
    route("POST", format!("/services/{SERVICE_ID}/update"), replies)
}

fn tasks_route(replies: Vec<Reply>) -> Route {
    route("GET", "/tasks", replies)
}

fn node_route(state: &str) -> Route {
    route("GET", "/nodes/node1", vec![ok(node(state))])
}

const CONFLICT: &str = "rpc error: code = Unknown desc = update out of sequence";

// ---------- start ----------

#[tokio::test]
async fn start_sends_one_update_with_the_read_version_and_only_replicas_changed() {
    let mock = MockDocker::start(vec![
        inspect_route(vec![ok(service(42, 0, "127.0.0.1/24"))]),
        update_route(vec![ok(json!({}))]),
    ])
    .await;
    let provider =
        SwarmServerProvider::new(&config(mock.endpoint(), 25565, Duration::from_secs(5)));

    provider.start().await.unwrap();

    let updates = mock.requests_to("POST", "/update");
    assert_eq!(updates.len(), 1);
    assert!(
        updates[0].path.contains("version=42"),
        "{}",
        updates[0].path
    );

    let sent: Value = serde_json::from_str(&updates[0].body).unwrap();
    let inspected: Value = serde_json::from_str(FIXTURE).unwrap();
    let mut expected = inspected["Spec"].clone();
    expected["Mode"]["Replicated"]["Replicas"] = json!(1);
    assert_eq!(
        sent, expected,
        "update body must equal the inspected spec with Replicas=1"
    );
}

#[tokio::test]
async fn start_is_idempotent_when_the_service_is_already_at_one_replica() {
    let mock = MockDocker::start(vec![
        inspect_route(vec![ok(service(42, 1, "127.0.0.1/24"))]),
        update_route(vec![ok(json!({}))]),
    ])
    .await;
    let provider =
        SwarmServerProvider::new(&config(mock.endpoint(), 25565, Duration::from_secs(5)));

    provider.start().await.unwrap();
    provider.start().await.unwrap();

    assert!(mock.requests_to("POST", "/update").is_empty());
}

#[tokio::test]
async fn start_re_reads_and_retries_an_out_of_sequence_conflict() {
    let mock = MockDocker::start(vec![
        inspect_route(vec![
            ok(service(42, 0, "127.0.0.1/24")),
            ok(service(43, 0, "127.0.0.1/24")),
        ]),
        update_route(vec![fail(500, CONFLICT), ok(json!({}))]),
    ])
    .await;
    let provider =
        SwarmServerProvider::new(&config(mock.endpoint(), 25565, Duration::from_secs(5)));

    provider.start().await.unwrap();

    let updates = mock.requests_to("POST", "/update");
    assert_eq!(updates.len(), 2);
    assert!(updates[0].path.contains("version=42"));
    assert!(
        updates[1].path.contains("version=43"),
        "retry must carry the re-read version"
    );
}

#[tokio::test]
async fn start_gives_up_after_a_bounded_number_of_conflicts() {
    let mock = MockDocker::start(vec![
        inspect_route(vec![ok(service(42, 0, "127.0.0.1/24"))]),
        update_route(vec![fail(500, CONFLICT)]),
    ])
    .await;
    let provider =
        SwarmServerProvider::new(&config(mock.endpoint(), 25565, Duration::from_secs(5)));

    let error = provider.start().await.unwrap_err();

    assert!(error.to_string().contains("gave up"), "{error}");
    assert_eq!(mock.requests_to("POST", "/update").len(), 3);
}

#[tokio::test]
async fn start_refuses_an_opted_out_service_without_writing() {
    let mock = MockDocker::start(vec![
        inspect_route(vec![ok(with_label(
            service(42, 0, "127.0.0.1/24"),
            "infrarust.swarm.scale",
            None,
        ))]),
        update_route(vec![ok(json!({}))]),
    ])
    .await;
    let provider =
        SwarmServerProvider::new(&config(mock.endpoint(), 25565, Duration::from_secs(5)));

    let error = provider.start().await.unwrap_err();

    assert!(error.to_string().contains("opted out"), "{error}");
    assert!(mock.requests_to("POST", "/update").is_empty());
}

#[tokio::test]
async fn start_refuses_a_service_scaled_past_one_replica_without_writing() {
    let mock = MockDocker::start(vec![
        inspect_route(vec![ok(service(42, 3, "127.0.0.1/24"))]),
        update_route(vec![ok(json!({}))]),
    ])
    .await;
    let provider =
        SwarmServerProvider::new(&config(mock.endpoint(), 25565, Duration::from_secs(5)));

    let error = provider.start().await.unwrap_err();

    assert!(error.to_string().contains("singleton"), "{error}");
    assert!(mock.requests_to("POST", "/update").is_empty());
}

// ---------- stop ----------

#[tokio::test]
async fn stop_sets_zero_then_waits_for_old_tasks_to_be_terminal() {
    let mock = MockDocker::start(vec![
        inspect_route(vec![ok(service(42, 1, "127.0.0.1/24"))]),
        update_route(vec![ok(json!({}))]),
        tasks_route(vec![
            ok(json!([task("running", "shutdown")])),
            ok(json!([task("shutdown", "shutdown")])),
        ]),
    ])
    .await;
    let provider =
        SwarmServerProvider::new(&config(mock.endpoint(), 25565, Duration::from_secs(5)));

    provider.stop().await.unwrap();

    let updates = mock.requests_to("POST", "/update");
    assert_eq!(updates.len(), 1);
    let sent: Value = serde_json::from_str(&updates[0].body).unwrap();
    assert_eq!(sent["Mode"]["Replicated"]["Replicas"], json!(0));
    assert!(
        mock.requests_to("GET", "/tasks").len() >= 2,
        "stop must poll tasks after the update, not return on the API update alone"
    );
}

#[tokio::test]
async fn stop_fails_when_tasks_do_not_terminate_within_the_bound() {
    let mock = MockDocker::start(vec![
        inspect_route(vec![ok(service(42, 1, "127.0.0.1/24"))]),
        update_route(vec![ok(json!({}))]),
        tasks_route(vec![ok(json!([task("running", "shutdown")]))]),
    ])
    .await;
    let provider =
        SwarmServerProvider::new(&config(mock.endpoint(), 25565, Duration::from_secs(1)));

    let started = Instant::now();
    let error = provider.stop().await.unwrap_err();

    assert!(error.to_string().contains("still active"), "{error}");
    assert!(started.elapsed() >= Duration::from_secs(1));
}

// ---------- check_status ----------

#[tokio::test]
async fn zero_replicas_with_no_live_tasks_is_stopped() {
    let mock = MockDocker::start(vec![
        inspect_route(vec![ok(service(42, 0, "127.0.0.1/24"))]),
        tasks_route(vec![ok(json!([task("shutdown", "shutdown")]))]),
    ])
    .await;
    let provider =
        SwarmServerProvider::new(&config(mock.endpoint(), 25565, Duration::from_secs(5)));

    assert_eq!(
        provider.check_status().await.unwrap(),
        ProviderStatus::Stopped
    );
}

#[tokio::test]
async fn zero_replicas_with_a_live_task_is_stopping_not_stopped() {
    let mock = MockDocker::start(vec![
        inspect_route(vec![ok(service(42, 0, "127.0.0.1/24"))]),
        tasks_route(vec![ok(json!([task("running", "shutdown")]))]),
    ])
    .await;
    let provider =
        SwarmServerProvider::new(&config(mock.endpoint(), 25565, Duration::from_secs(5)));

    assert_eq!(
        provider.check_status().await.unwrap(),
        ProviderStatus::Stopping
    );
}

#[tokio::test]
async fn running_needs_a_running_task_on_a_ready_node_and_a_vip_status_ping() {
    let status_port = spawn_status_server().await;
    let mock = MockDocker::start(vec![
        inspect_route(vec![ok(service(42, 1, "127.0.0.1/24"))]),
        tasks_route(vec![ok(json!([task("running", "running")]))]),
        node_route("ready"),
    ])
    .await;
    let provider = SwarmServerProvider::new(&config(
        mock.endpoint(),
        status_port,
        Duration::from_secs(5),
    ));

    assert_eq!(
        provider.check_status().await.unwrap(),
        ProviderStatus::Running
    );
}

#[tokio::test]
async fn a_running_task_that_does_not_answer_the_vip_is_starting() {
    let port = closed_port().await;
    let mock = MockDocker::start(vec![
        inspect_route(vec![ok(service(42, 1, "127.0.0.1/24"))]),
        tasks_route(vec![ok(json!([task("running", "running")]))]),
        node_route("ready"),
    ])
    .await;
    let provider = SwarmServerProvider::new(&config(mock.endpoint(), port, Duration::from_secs(5)));

    assert_eq!(
        provider.check_status().await.unwrap(),
        ProviderStatus::Starting
    );
}

#[tokio::test]
async fn a_task_still_starting_is_starting_even_if_the_vip_answers() {
    let status_port = spawn_status_server().await;
    let mock = MockDocker::start(vec![
        inspect_route(vec![ok(service(42, 1, "127.0.0.1/24"))]),
        tasks_route(vec![ok(json!([task("starting", "running")]))]),
        node_route("ready"),
    ])
    .await;
    let provider = SwarmServerProvider::new(&config(
        mock.endpoint(),
        status_port,
        Duration::from_secs(5),
    ));

    assert_eq!(
        provider.check_status().await.unwrap(),
        ProviderStatus::Starting
    );
}

#[tokio::test]
async fn a_task_on_a_node_that_is_not_ready_is_starting() {
    let status_port = spawn_status_server().await;
    let mock = MockDocker::start(vec![
        inspect_route(vec![ok(service(42, 1, "127.0.0.1/24"))]),
        tasks_route(vec![ok(json!([task("running", "running")]))]),
        node_route("down"),
    ])
    .await;
    let provider = SwarmServerProvider::new(&config(
        mock.endpoint(),
        status_port,
        Duration::from_secs(5),
    ));

    assert_eq!(
        provider.check_status().await.unwrap(),
        ProviderStatus::Starting
    );
}

#[tokio::test]
async fn an_api_error_is_an_error_never_stopped() {
    let mock = MockDocker::start(vec![inspect_route(vec![fail(500, "manager unavailable")])]).await;
    let provider =
        SwarmServerProvider::new(&config(mock.endpoint(), 25565, Duration::from_secs(5)));

    let result = provider.check_status().await;

    assert!(
        matches!(result, Err(ServerManagerError::Provider { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn an_opted_out_service_is_an_error_never_stopped() {
    let mock = MockDocker::start(vec![inspect_route(vec![ok(with_label(
        service(42, 0, "127.0.0.1/24"),
        "infrarust.swarm.scale",
        Some("false"),
    ))])])
    .await;
    let provider =
        SwarmServerProvider::new(&config(mock.endpoint(), 25565, Duration::from_secs(5)));

    assert!(provider.check_status().await.is_err());
}

#[tokio::test]
async fn an_unreachable_manager_is_an_error_never_stopped() {
    let port = closed_port().await;
    let provider = SwarmServerProvider::new(&config(
        format!("tcp://127.0.0.1:{port}"),
        25565,
        Duration::from_secs(5),
    ));

    assert!(provider.check_status().await.is_err());
}
