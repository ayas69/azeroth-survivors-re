//! `GET /coop/{room}` — the co-op relay: every tab in a room hears what the others say. The host
//! keeps no game state of its own; the first player in a room hosts the run (its tab spawns and
//! moves the horde) and the rest follow it. When the host leaves, the next player in line is told
//! it hosts now.
//!
//! Frames are JSON text. A player sends `{"to": id?, "msg": ...}` (no `to` = everyone else); the
//! others receive `{"from": id, "msg": ...}`. The relay itself speaks `{"sys": ...}`:
//! `welcome` (your id, the host, who is already in), `joined`, and `left` (with the host after).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// Well inside Cloudflare's 100 s idle cut-off, so a quiet room (everyone choosing a card) stays up.
const PING_EVERY: Duration = Duration::from_secs(25);
/// The most players one room takes.
const ROOM_MAX: usize = 8;

type Tx = mpsc::UnboundedSender<Message>;

#[derive(Default)]
struct Room {
    /// In joining order: the first one hosts.
    members: Vec<(u32, Tx)>,
}

impl Room {
    fn host(&self) -> Option<u32> {
        self.members.first().map(|m| m.0)
    }
}

#[derive(Clone, Default)]
struct CoopState {
    rooms: Arc<Mutex<HashMap<String, Room>>>,
    next_id: Arc<AtomicU32>,
}

pub fn router() -> Router {
    Router::new()
        .route("/coop/{room}", get(upgrade))
        .with_state(CoopState::default())
}

/// Room names are the battleground's key: short, plain.
fn room_key(raw: &str) -> Option<String> {
    let key: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(48)
        .collect();
    (!key.is_empty()).then_some(key)
}

async fn upgrade(
    State(state): State<CoopState>,
    Path(room): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let room = room_key(&room).unwrap_or_else(|| "default".into());
    ws.on_upgrade(move |socket| session(state, room, socket))
}

fn text(v: Value) -> Message {
    Message::text(v.to_string())
}

async fn session(state: CoopState, room: String, ws: WebSocket) {
    let id = state.next_id.fetch_add(1, Ordering::Relaxed) + 1;
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    {
        let mut rooms = state.rooms.lock().unwrap();
        let r = rooms.entry(room.clone()).or_default();
        if r.members.len() >= ROOM_MAX {
            let _ = tx.send(text(json!({"sys": "full"})));
        } else {
            let peers: Vec<u32> = r.members.iter().map(|m| m.0).collect();
            for (_, other) in &r.members {
                let _ = other.send(text(json!({"sys": "joined", "id": id})));
            }
            r.members.push((id, tx.clone()));
            let _ = tx.send(text(
                json!({"sys": "welcome", "id": id, "host": r.host(), "peers": peers}),
            ));
        }
    }
    tracing::info!(room, id, "coop: joined");

    let (mut ws_tx, mut ws_rx) = ws.split();
    // One writer task drains the queue every relay pushes into, and pings the quiet line.
    let writer = tokio::spawn(async move {
        loop {
            match tokio::time::timeout(PING_EVERY, rx.recv()).await {
                Ok(Some(m)) => {
                    if ws_tx.send(m).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    if ws_tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    while let Some(Ok(m)) = ws_rx.next().await {
        let raw = match m {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(mut v) = serde_json::from_str::<Value>(raw.as_str()) else {
            continue;
        };
        let to = v.get("to").and_then(Value::as_u64).map(|t| t as u32);
        let out = text(json!({"from": id, "msg": v.get_mut("msg").map(Value::take)}));
        let rooms = state.rooms.lock().unwrap();
        let Some(r) = rooms.get(&room) else { break };
        for (other, otx) in &r.members {
            if *other != id && to.is_none_or(|t| t == *other) {
                let _ = otx.send(out.clone());
            }
        }
    }

    {
        let mut rooms = state.rooms.lock().unwrap();
        if let Some(r) = rooms.get_mut(&room) {
            r.members.retain(|m| m.0 != id);
            let host = r.host();
            for (_, other) in &r.members {
                let _ = other.send(text(json!({"sys": "left", "id": id, "host": host})));
            }
            if r.members.is_empty() {
                rooms.remove(&room);
            }
        }
    }
    writer.abort();
    tracing::info!(room, id, "coop: left");
}
