//! k-board standalone sync server.
//!
//! A WebSocket fan-out over per-scope rooms, plus static hosting for the web
//! client. This is the *standalone* adapter: it supplies its own implementations
//! of what the engine refuses to decide — identity, authority, storage — so the
//! same engine that runs embedded in a host platform also runs on its own.
//!
//! Everything here is deliberately replaceable. Embedded in K-Comms, this whole
//! binary is what the host already owns: Phoenix Channels instead of these
//! sockets, conversation membership instead of the actor allocator below,
//! Postgres instead of the in-memory rooms.
//!
//! Not production: rooms are in memory and every connection is admitted. See
//! `authorize` for exactly where a real deployment plugs in.

mod room;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tower_http::services::{ServeDir, ServeFile};

use kboard_core::document::{Document, ScopeId};
use kboard_core::op::StampedOp;

use crate::room::Room;

#[derive(Clone)]
struct AppState {
    /// A `std::sync::Mutex` is correct here only because no lock is ever held
    /// across an `.await`. Every critical section below is synchronous.
    rooms: Arc<Mutex<HashMap<String, Room>>>,
    next_connection: Arc<AtomicU64>,
    wasm_path: PathBuf,
}

impl AppState {
    fn with_room<T>(&self, scope: &str, action: impl FnOnce(&mut Room) -> T) -> T {
        let mut rooms = self
            .rooms
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let room = rooms
            .entry(scope.to_owned())
            .or_insert_with(|| Room::new(ScopeId::new(scope)));
        action(room)
    }
}

// -- wire protocol ---------------------------------------------------------

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage<'a> {
    /// Sent once on join: the actor id this connection must stamp with, and the
    /// materialised document. Never the raw history — it may be truncated.
    Init {
        actor: u64,
        doc: &'a Document,
    },
    Ops {
        ops: &'a [StampedOp],
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Ops { ops: Vec<StampedOp> },
}

// -- authority -------------------------------------------------------------

/// The standalone deployment's authority decision.
///
/// This is the seam. A real deployment authenticates here; K-Comms replaces
/// this entire binary with its own membership check and never reaches this
/// function. The engine itself has no opinion either way — which is what lets
/// both arrangements exist.
fn authorize(_scope: &str) -> bool {
    true
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8080);

    let web_root = PathBuf::from(std::env::var("KBOARD_WEB").unwrap_or_else(|_| "web".to_owned()));
    let wasm_path = PathBuf::from(
        std::env::var("KBOARD_WASM")
            .unwrap_or_else(|_| "target/wasm32-unknown-unknown/release/kboard.wasm".to_owned()),
    );

    let state = AppState {
        rooms: Arc::new(Mutex::new(HashMap::new())),
        next_connection: Arc::new(AtomicU64::new(0)),
        wasm_path,
    };

    let static_files =
        ServeDir::new(&web_root).not_found_service(ServeFile::new(web_root.join("index.html")));

    let app = Router::new()
        .route("/ws/{scope}", get(websocket))
        .route("/kboard.wasm", get(serve_wasm))
        .route("/health", get(|| async { "ok" }))
        .route("/api/rooms/{scope}/stats", get(room_stats))
        .fallback_service(static_files)
        .with_state(state);

    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = match tokio::net::TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("k-board: cannot bind {address}: {error}");
            std::process::exit(1);
        }
    };

    println!("k-board server listening on http://{address}");
    println!("  open http://{address}/ in two tabs to see convergence");

    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
    {
        eprintln!("k-board: server error: {error}");
    }
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    println!("\nk-board: shutting down");
}

async fn serve_wasm(State(state): State<AppState>) -> Response {
    match tokio::fs::read(&state.wasm_path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/wasm")], bytes).into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            format!(
                "engine not built at {}\n\nRun:\n  cargo build -p kboard-ffi --target wasm32-unknown-unknown --release\n",
                state.wasm_path.display()
            ),
        )
            .into_response(),
    }
}

async fn room_stats(Path(scope): Path<String>, State(state): State<AppState>) -> Response {
    let stats = state.with_room(&scope, |room| room.stats());
    axum::Json(stats).into_response()
}

async fn websocket(
    upgrade: WebSocketUpgrade,
    Path(scope): Path<String>,
    State(state): State<AppState>,
) -> Response {
    if !authorize(&scope) {
        return (StatusCode::FORBIDDEN, "not permitted").into_response();
    }
    upgrade.on_upgrade(move |socket| session(socket, scope, state))
}

async fn session(socket: WebSocket, scope: String, state: AppState) {
    // The server allocates actor ids. Two replicas that shared one would break
    // the tiebreak that makes concurrent edits resolve identically everywhere.
    let connection = state.next_connection.fetch_add(1, Ordering::Relaxed) + 1;

    let (mut outbound, mut inbound) = socket.split();

    let (mut updates, init) = {
        let joined = state.with_room(&scope, |room| (room.subscribe(), room.join_state()));
        let payload = serde_json::to_string(&ServerMessage::Init {
            actor: connection,
            doc: joined.1.document(),
        });
        match payload {
            Ok(json) => (joined.0, json),
            Err(_) => return,
        }
    };

    if outbound.send(Message::Text(init.into())).await.is_err() {
        return;
    }

    let mut relay = tokio::spawn(async move {
        while let Ok((origin, payload)) = updates.recv().await {
            // Do not echo a connection's own operations back to it; it applied
            // them locally the moment the user drew.
            if origin == connection {
                continue;
            }
            if outbound.send(Message::Text(payload.into())).await.is_err() {
                break;
            }
        }
    });

    let receiving_state = state.clone();
    let receiving_scope = scope.clone();
    let mut receive = tokio::spawn(async move {
        while let Some(Ok(message)) = inbound.next().await {
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(ClientMessage::Ops { ops }) = serde_json::from_str::<ClientMessage>(&text)
            else {
                continue; // A malformed frame drops, it does not close the session.
            };
            if ops.is_empty() {
                continue;
            }

            let Ok(relayed) = serde_json::to_string(&ServerMessage::Ops { ops: &ops }) else {
                continue;
            };

            receiving_state.with_room(&receiving_scope, |room| {
                room.accept(ops);
                room.broadcast(connection, relayed);
            });
        }
    });

    // Whichever half ends first, tear down the other.
    tokio::select! {
        _ = &mut relay => receive.abort(),
        _ = &mut receive => relay.abort(),
    }
}
