use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, ConnectInfo, Path, Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use rand::{distributions::Alphanumeric, rngs::OsRng, Rng};
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc, Duration as ChronoDuration};
use tokio::sync::{broadcast, Mutex};
use tokio::time::sleep;
use tower::ServiceBuilder;
use tower_http::{cors::CorsLayer, services::{ServeDir, ServeFile}, trace::TraceLayer};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    rooms: Arc<Mutex<HashMap<String, Room>>>,
    public_base: String,
}

#[derive(Clone)]
struct Room {
    tx: broadcast::Sender<ChatEvent>,
    members: usize,
    history: Vec<ChatEvent>,
    last_activity: DateTime<Utc>,
}

#[derive(Serialize)]
struct NewRoomResponse {
    room_id: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct WsQuery {
    name: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatEvent {
    System { message: String, at: DateTime<Utc> },
    Chat { from: String, message: String, at: DateTime<Utc> },
}

#[derive(Debug, Deserialize)]
struct ClientMessage {
    message: String,
}

#[tokio::main]
async fn main() {
    let state = AppState {
        rooms: Arc::new(Mutex::new(HashMap::new())),
        public_base: std::env::var("PUBLIC_BASE")
            .unwrap_or_else(|_| "https://adhocchat.cleanyong.familybankbank.com".to_string()),
    };

    tokio::spawn(cleanup_task(state.clone()));

    let static_dir = ServeDir::new("static").not_found_service(ServeFile::new("static/index.html"));

    let app = Router::new()
        .route("/api/rooms", post(create_room))
        .route("/ws/:room_id", get(ws_handler))
        .route("/r/:room_id", get(index_html))
        .route("/r/:room_id/*rest", get(index_html))
        .nest_service("/", static_dir)
        .with_state(state)
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(CorsLayer::very_permissive()),
        );

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3002")
        .await
        .expect("bind 3000");
    println!("🚀 AdHoc Chat running on http://127.0.0.1:3002");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .unwrap();
}

async fn index_html() -> impl IntoResponse {
    match tokio::fs::read("static/index.html").await {
        Ok(body) => (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn create_room(State(state): State<AppState>) -> impl IntoResponse {
    let mut rooms = state.rooms.lock().await;
    let room_id = Uuid::new_v4().simple().to_string();
    let (tx, _rx) = broadcast::channel(100);
    rooms.insert(room_id.clone(), Room { tx, members: 0, history: Vec::new(), last_activity: Utc::now() });
    let url = format!("{}/r/{}", state.public_base.trim_end_matches('/'), room_id);
    Json(NewRoomResponse { room_id, url })
}

async fn ws_handler(
    Path(room_id): Path<String>,
    Query(query): Query<WsQuery>,
    State(state): State<AppState>,
    ConnectInfo(_addr): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let name = query.name.unwrap_or_else(random_name);

    let (tx, history) = {
        let mut rooms = state.rooms.lock().await;
        if let Some(room) = rooms.get_mut(&room_id) {
            if room.members >= 2 {
                return StatusCode::FORBIDDEN.into_response();
            }
            room.members += 1;
            (room.tx.clone(), room.history.clone())
        } else {
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    ws.on_upgrade(move |socket| handle_ws(socket, state, room_id, name, tx, history))
}

async fn handle_ws(socket: WebSocket, state: AppState, room_id: String, name: String, tx: broadcast::Sender<ChatEvent>, history: Vec<ChatEvent>) {
    let mut rx = tx.subscribe();
    let join_event = ChatEvent::System {
        message: format!("{} 加入房间", name),
        at: Utc::now(),
    };
    let _ = tx.send(join_event.clone());
    persist_event(&state, &room_id, join_event).await;

    let (mut sender, mut receiver) = socket.split();

    // send history to the newly joined client
    for ev in history {
        if let Ok(text) = serde_json::to_string(&ev) {
            if sender.send(Message::Text(text)).await.is_err() {
                return;
            }
        }
    }

    // forward from broadcast to this socket
    let forward = tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            if let Ok(text) = serde_json::to_string(&event) {
                if sender.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
        }
    });

    // listen for messages from this client
    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(txt) => {
                if let Ok(payload) = serde_json::from_str::<ClientMessage>(&txt) {
                    if payload.message.trim().is_empty() {
                        continue;
                    }
                    // Enforce per-message size limit (3000 chars)
                    let trimmed = payload.message.chars().take(3000).collect::<String>();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let evt = ChatEvent::Chat {
                        from: name.clone(),
                        message: trimmed,
                        at: Utc::now(),
                    };
                    persist_event(&state, &room_id, evt.clone()).await;
                    let _ = tx.send(evt);
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    let leave_evt = ChatEvent::System {
        message: format!("{} 离开房间", name),
        at: Utc::now(),
    };
    persist_event(&state, &room_id, leave_evt.clone()).await;
    let _ = tx.send(leave_evt);

    forward.abort();

    let mut rooms = state.rooms.lock().await;
    if let Some(room) = rooms.get_mut(&room_id) {
        if room.members > 0 {
            room.members -= 1;
        }
        if room.members == 0 {
            rooms.remove(&room_id);
        }
    }
}

async fn persist_event(state: &AppState, room_id: &str, event: ChatEvent) {
    let mut rooms = state.rooms.lock().await;
    if let Some(room) = rooms.get_mut(room_id) {
        room.history.push(event);
        room.last_activity = Utc::now();
        let cutoff = Utc::now() - ChronoDuration::hours(1);
        room.history.retain(|evt| match evt {
            ChatEvent::System { at, .. } | ChatEvent::Chat { at, .. } => *at >= cutoff,
        });
        if room.history.len() > 200 {
            let drop = room.history.len() - 200;
            room.history.drain(0..drop);
        }
        // total characters cap (30k): drop oldest events until under cap
        const MAX_CHARS: usize = 30_000;
        while total_chars(&room.history) > MAX_CHARS && !room.history.is_empty() {
            room.history.remove(0);
        }
    }
}

fn total_chars(events: &[ChatEvent]) -> usize {
    events
        .iter()
        .map(|e| match e {
            ChatEvent::System { message, .. } => message.chars().count(),
            ChatEvent::Chat { message, .. } => message.chars().count(),
        })
        .sum()
}

fn random_name() -> String {
    let word: String = OsRng
        .sample_iter(&Alphanumeric)
        .take(6)
        .map(char::from)
        .collect();
    format!("user-{}", word.to_lowercase())
}

async fn cleanup_task(state: AppState) {
    loop {
        sleep(Duration::from_secs(60)).await;
        let cutoff = Utc::now() - ChronoDuration::hours(1);
        let mut rooms = state.rooms.lock().await;
        rooms.retain(|_, room| {
            // drop old history
            room.history.retain(|evt| match evt {
                ChatEvent::System { at, .. } | ChatEvent::Chat { at, .. } => *at >= cutoff,
            });
            // room lives only if recent activity within 1h
            room.last_activity >= cutoff && (room.members > 0 || !room.history.is_empty())
        });
    }
}
