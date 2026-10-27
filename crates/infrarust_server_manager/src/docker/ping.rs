//! Minecraft status ping for readiness checks.
//!
//! A self-contained pre-login status exchange: Handshake, StatusRequest, then
//! StatusResponse. `infrarust-core` already depends on this crate, so its
//! `status_exchange` connector cannot be called from here without a cycle.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const HANDSHAKE_PACKET_ID: u8 = 0x00;
const STATUS_REQUEST_PACKET_ID: u8 = 0x00;
const STATUS_RESPONSE_PACKET_ID: i32 = 0x00;
/// Any protocol version works for a status request; servers answer regardless.
const PING_PROTOCOL_VERSION: i32 = 0;
const STATUS_NEXT_STATE: i32 = 1;
/// Upper bound for a status response frame, so a hostile peer cannot exhaust memory.
const MAX_FRAME_LEN: usize = 1 << 20;

/// Runs a status ping against `addr` and succeeds only when the peer answers
/// with a JSON status object within `timeout`.
pub(crate) async fn status_ping(addr: SocketAddr, timeout: Duration) -> Result<(), String> {
    tokio::time::timeout(timeout, exchange(addr))
        .await
        .map_err(|_| "status ping timed out".to_string())?
}

async fn exchange(addr: SocketAddr) -> Result<(), String> {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connect failed: {e}"))?;

    let mut handshake = Vec::new();
    handshake.push(HANDSHAKE_PACKET_ID);
    write_varint(&mut handshake, PING_PROTOCOL_VERSION);
    write_string(&mut handshake, &addr.ip().to_string());
    handshake.extend_from_slice(&addr.port().to_be_bytes());
    write_varint(&mut handshake, STATUS_NEXT_STATE);

    let mut request = Vec::new();
    write_frame(&mut request, &handshake);
    write_frame(&mut request, &[STATUS_REQUEST_PACKET_ID]);
    stream
        .write_all(&request)
        .await
        .map_err(|e| format!("write failed: {e}"))?;

    let payload = read_frame(&mut stream).await?;
    let mut cursor = payload.as_slice();
    let packet_id = read_varint(&mut cursor)?;
    if packet_id != STATUS_RESPONSE_PACKET_ID {
        return Err(format!(
            "unexpected packet id {packet_id:#x} in status reply"
        ));
    }
    let json = read_string(&mut cursor)?;
    let status: serde_json::Value =
        serde_json::from_str(&json).map_err(|e| format!("status JSON is invalid: {e}"))?;
    if status.is_object() {
        Ok(())
    } else {
        Err("status reply is not a JSON object".to_string())
    }
}

fn write_varint(out: &mut Vec<u8>, value: i32) {
    let mut remaining = value as u32;
    loop {
        if remaining & !0x7f == 0 {
            out.push(remaining as u8);
            return;
        }
        out.push((remaining & 0x7f) as u8 | 0x80);
        remaining >>= 7;
    }
}

fn write_string(out: &mut Vec<u8>, value: &str) {
    write_varint(out, i32::try_from(value.len()).unwrap_or(i32::MAX));
    out.extend_from_slice(value.as_bytes());
}

fn write_frame(out: &mut Vec<u8>, body: &[u8]) {
    write_varint(out, i32::try_from(body.len()).unwrap_or(i32::MAX));
    out.extend_from_slice(body);
}

fn read_varint(cursor: &mut &[u8]) -> Result<i32, String> {
    let mut value: u32 = 0;
    for shift in (0..35).step_by(7) {
        let (&byte, rest) = cursor.split_first().ok_or("truncated VarInt")?;
        *cursor = rest;
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value as i32);
        }
    }
    Err("VarInt is too long".to_string())
}

fn read_string(cursor: &mut &[u8]) -> Result<String, String> {
    let len = usize::try_from(read_varint(cursor)?).map_err(|_| "negative string length")?;
    if len > cursor.len() {
        return Err("truncated string".to_string());
    }
    let (bytes, rest) = cursor.split_at(len);
    *cursor = rest;
    String::from_utf8(bytes.to_vec()).map_err(|_| "string is not UTF-8".to_string())
}

async fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut len: u32 = 0;
    for shift in (0..35).step_by(7) {
        let byte = stream
            .read_u8()
            .await
            .map_err(|e| format!("read failed: {e}"))?;
        len |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            break;
        }
    }
    let len = usize::try_from(len).map_err(|_| "frame length overflow")?;
    if len == 0 || len > MAX_FRAME_LEN {
        return Err(format!("frame length {len} is out of bounds"));
    }
    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    Ok(payload)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::net::Ipv4Addr;

    use tokio::net::TcpListener;

    use super::*;

    /// Serves one connection: reads the client's bytes, then writes `reply`.
    async fn serve_once(reply: Vec<u8>) -> SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut scratch = vec![0u8; 512];
            let _ = stream.read(&mut scratch).await;
            let _ = stream.write_all(&reply).await;
        });
        addr
    }

    fn status_reply(json: &str) -> Vec<u8> {
        let mut body = vec![0x00];
        write_string(&mut body, json);
        let mut frame = Vec::new();
        write_frame(&mut frame, &body);
        frame
    }

    #[tokio::test]
    async fn a_json_status_reply_passes_the_ping() {
        let addr = serve_once(status_reply(r#"{"version":{"name":"1.21"}}"#)).await;
        status_ping(addr, Duration::from_secs(2)).await.unwrap();
    }

    #[tokio::test]
    async fn a_non_json_reply_fails_the_ping() {
        let addr = serve_once(status_reply("not json")).await;
        assert!(status_ping(addr, Duration::from_secs(2)).await.is_err());
    }

    #[tokio::test]
    async fn a_wrong_packet_id_fails_the_ping() {
        let mut body = vec![0x05];
        write_string(&mut body, "{}");
        let mut frame = Vec::new();
        write_frame(&mut frame, &body);
        let addr = serve_once(frame).await;
        assert!(status_ping(addr, Duration::from_secs(2)).await.is_err());
    }

    #[tokio::test]
    async fn a_silent_peer_times_out() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let error = status_ping(addr, Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(error.contains("timed out"));
    }

    #[tokio::test]
    async fn a_refused_connection_fails_the_ping() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        assert!(status_ping(addr, Duration::from_secs(2)).await.is_err());
    }

    #[test]
    fn varint_round_trips_across_widths() {
        for value in [0, 1, 127, 128, 25565, i32::MAX, -1] {
            let mut bytes = Vec::new();
            write_varint(&mut bytes, value);
            let mut cursor = bytes.as_slice();
            assert_eq!(read_varint(&mut cursor).unwrap(), value);
            assert!(cursor.is_empty());
        }
    }
}
