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
//! ## Posture
//!
//! Resource limits are enforced (see [`limits`]) so an unauthenticated peer
//! cannot exhaust the process. **Authentication is still absent** —
//! [`authorize`] admits everyone. Until that changes this is safe to run on a
//! trusted network and nowhere else.

mod limits;
mod room;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tower_http::services::{ServeDir, ServeFile};

use kboard_core::document::{Document, ScopeId};
use kboard_core::op::StampedOp;

use crate::limits::RateLimiter;
use crate::room::{Refused, Room};

#[derive(Clone)]
struct AppState {
    /// A `std::sync::Mutex` is correct here only because no lock is ever held
    /// across an `.await`. Every critical section below is synchronous.
    rooms: Arc<Mutex<HashMap<String, Room>>>,
    next_connection: Arc<AtomicU64>,
    wasm_path: PathBuf,
}

impl AppState {
    fn lock_rooms(&self) -> std::sync::MutexGuard<'_, HashMap<String, Room>> {
        self.rooms
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Access a room, creating it if capacity allows.
    ///
    /// Returns `None` when the room cap is reached. Rooms are minted from URL
    /// paths, so without a cap any visitor can allocate unbounded state.
    fn with_room<T>(&self, scope: &str, action: impl FnOnce(&mut Room) -> T) -> Option<T> {
        let mut rooms = self.lock_rooms();
        if !rooms.contains_key(scope) && rooms.len() >= limits::MAX_ROOMS {
            return None;
        }
        let room = rooms
            .entry(scope.to_owned())
            .or_insert_with(|| Room::new(ScopeId::new(scope)));
        Some(action(room))
    }

    /// Access a room only if it already exists — never creates one.
    fn with_existing<T>(&self, scope: &str, action: impl FnOnce(&mut Room) -> T) -> Option<T> {
        self.lock_rooms().get_mut(scope).map(action)
    }
}

// -- wire protocol ---------------------------------------------------------

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage<'a> {
    /// Sent once on join: the actor id this connection must stamp with, and the
    /// materialised document.
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
///
/// It currently admits everyone. That is the single reason this server is not
/// production-ready, and it is deliberately one function so it stays obvious.
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

    spawn_room_sweeper(state.clone());

    let static_files =
        ServeDir::new(&web_root).not_found_service(ServeFile::new(web_root.join("index.html")));

    let app = Router::new()
        .route("/ws/{scope}", get(websocket))
        .route("/kboard.wasm", get(serve_wasm))
        .route("/health", get(|| async { "ok" }))
        .route("/api/rooms/{scope}/stats", get(room_stats))
        .fallback_service(static_files)
        .layer(middleware::from_fn(security_headers))
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
    println!("  WARNING: no authentication — trusted networks only");

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

/// Reclaim rooms nobody is connected to that have been quiet.
///
/// Without this, a room created by a single visit is retained for the process
/// lifetime. Combined with the room cap that turns a slow leak into an eventual
/// hard refusal, which is worse than reclaiming.
fn spawn_room_sweeper(state: AppState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(limits::SWEEP_INTERVAL);
        ticker.tick().await; // the first tick fires immediately
        loop {
            ticker.tick().await;
            let now = Instant::now();
            let mut rooms = state.lock_rooms();
            let before = rooms.len();
            rooms.retain(|_, room| !room.is_reclaimable(now));
            let reclaimed = before - rooms.len();
            if reclaimed > 0 {
                println!(
                    "k-board: reclaimed {reclaimed} idle room(s), {} remain",
                    rooms.len()
                );
            }
        }
    });
}

/// Conservative defaults for a page that loads wasm and opens a WebSocket.
///
/// `frame-ancestors 'none'` blocks embedding this *demo* server in an iframe.
/// A host embedding the canvas serves the client itself and sets its own policy;
/// nothing here should make that decision for them.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; \
             script-src 'self' 'wasm-unsafe-eval'; \
             style-src 'self'; \
             img-src 'self' data:; \
             connect-src 'self' ws: wss:; \
             base-uri 'none'; \
             object-src 'none'; \
             frame-ancestors 'none'",
        ),
    );
    response
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
    if !limits::scope_is_acceptable(&scope) {
        return (StatusCode::BAD_REQUEST, "invalid scope").into_response();
    }
    // Reads must not mint rooms; otherwise polling stats is itself an
    // allocation vector.
    match state.with_existing(&scope, |room| room.stats()) {
        Some(stats) => axum::Json(stats).into_response(),
        None => (StatusCode::NOT_FOUND, "no such room").into_response(),
    }
}

async fn websocket(
    upgrade: WebSocketUpgrade,
    Path(scope): Path<String>,
    State(state): State<AppState>,
) -> Response {
    if !limits::scope_is_acceptable(&scope) {
        return (StatusCode::BAD_REQUEST, "invalid scope").into_response();
    }
    if !authorize(&scope) {
        return (StatusCode::FORBIDDEN, "not permitted").into_response();
    }
    // Enforced at the protocol layer as well as in the read loop, so an
    // oversized frame is rejected before it is ever fully buffered.
    upgrade
        .max_message_size(limits::MAX_FRAME_BYTES)
        .on_upgrade(move |socket| session(socket, scope, state))
}

async fn session(socket: WebSocket, scope: String, state: AppState) {
    // The server allocates actor ids. Two replicas that shared one would break
    // the tiebreak that makes concurrent edits resolve identically everywhere.
    let connection = state.next_connection.fetch_add(1, Ordering::Relaxed) + 1;

    let (mut outbound, mut inbound) = socket.split();

    let joined = state.with_room(&scope, |room| {
        let updates = room.subscribe();
        let payload = serde_json::to_string(&ServerMessage::Init {
            actor: connection,
            doc: room.document(),
        });
        (updates, payload)
    });

    // At the room cap: refuse this connection rather than evict somebody
    // else's board.
    let Some((mut updates, payload)) = joined else {
        let _ = outbound.send(Message::Close(None)).await;
        return;
    };
    let Ok(init) = payload else { return };

    if outbound.send(Message::Text(init.into())).await.is_err() {
        return;
    }

    let mut relay = tokio::spawn(async move {
        loop {
            match updates.recv().await {
                Ok((origin, payload)) => {
                    // Do not echo a connection's own operations back to it; it
                    // applied them locally the moment the user drew.
                    if origin == connection {
                        continue;
                    }
                    if outbound.send(Message::Text(payload.into())).await.is_err() {
                        break;
                    }
                }
                // The consumer fell behind the channel. Closing is deliberate:
                // the client reconnects and receives a fresh full document, so
                // it re-converges. Continuing would silently skip operations,
                // which is the one outcome a CRDT cannot repair.
                Err(RecvError::Lagged(missed)) => {
                    eprintln!(
                        "k-board: connection {connection} lagged {missed} messages, closing to force resync"
                    );
                    break;
                }
                Err(RecvError::Closed) => break,
            }
        }
    });

    let receiving_state = state.clone();
    let receiving_scope = scope.clone();
    let mut receive = tokio::spawn(async move {
        let mut limiter = RateLimiter::new();

        while let Some(Ok(message)) = inbound.next().await {
            let Message::Text(text) = message else {
                continue;
            };

            if text.len() > limits::MAX_FRAME_BYTES {
                break; // Not a legitimate client.
            }

            if !limiter.allow() {
                if limiter.should_disconnect() {
                    eprintln!("k-board: connection {connection} exceeded its rate budget, closing");
                    break;
                }
                continue; // Transient burst: drop this frame, keep the session.
            }

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

            // Relay only what the room accepted. Broadcasting a refused batch
            // would leave peers holding operations this room does not have —
            // divergence introduced by the server itself.
            let outcome =
                receiving_state.with_existing(&receiving_scope, |room| match room.accept(&ops) {
                    Ok(_) => {
                        room.broadcast(connection, relayed);
                        Ok(())
                    }
                    Err(refused) => Err(refused),
                });

            match outcome {
                Some(Ok(())) => {}
                Some(Err(Refused::BatchTooLarge)) => break,
                Some(Err(Refused::RoomFull)) => continue,
                None => break, // The room was reclaimed underneath us.
            }
        }
    });

    // Whichever half ends first, tear down the other.
    tokio::select! {
        _ = &mut relay => receive.abort(),
        _ = &mut receive => relay.abort(),
    }
}
