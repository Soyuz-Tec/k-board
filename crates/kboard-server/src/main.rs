//! k-board standalone sync server.
#![forbid(unsafe_code)]
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
//! Resource limits are enforced (see [`limits`]) so a peer cannot exhaust the
//! process, and connections are authenticated when a secret is configured (see
//! [`auth`]).
//!
//! Without a secret the server admits everyone — and refuses to bind anything
//! but loopback, so that configuration is a development convenience rather than
//! an unauthenticated writable store on a network.

mod auth;
mod directory;
mod limits;
mod protocol;
mod room;
mod room_cell;
mod security;
mod storage_writer;
mod telemetry;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tower_http::services::{ServeDir, ServeFile};

use kboard_core::clock::ActorId;
use kboard_core::op::StampedOp;
use kboard_store::SqliteStore;

use crate::auth::{Authority, Grant};
use crate::directory::{OpenError, ScopeDirectory};
use crate::limits::RateLimiter;
use crate::protocol::{
    actor_for, operations_hash, valid_opaque_id, validate_stamps, RefusalCode, StampError,
    V1ClientMessage, V1ServerMessage, V2ClientMessage, V2ServerMessage, Version, VERSION_2,
    VERSION_2_SUBPROTOCOL,
};
use crate::room::{Fanout, Refused};
use crate::room_cell::{CellError, CommitOutcome, CommitRequest};
use crate::security::{
    scope_correlation, AdmissionControl, AdmissionError, EmbeddingPolicy, OriginPolicy,
};
use crate::storage_writer::StorageWriter;

#[derive(Clone)]
struct AppState {
    directory: ScopeDirectory,
    storage: StorageWriter,
    next_connection: Arc<AtomicU64>,
    admission: AdmissionControl,
    wasm_path: PathBuf,
    authority: Arc<Authority>,
    origin: OriginPolicy,
    embedding: EmbeddingPolicy,
    accepting: Arc<AtomicBool>,
    persistent_database: bool,
}

#[derive(Serialize)]
struct BackupEvidence {
    backup: kboard_store::BackupReport,
    isolated_restore: kboard_store::RestoreVerification,
}

fn argument_value<'a>(arguments: &'a [String], name: &str) -> Option<&'a str> {
    arguments
        .iter()
        .position(|argument| argument == name)
        .and_then(|index| arguments.get(index + 1))
        .map(String::as_str)
}

fn validate_persistent_path(path: &FsPath) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("database path is empty".to_owned());
    }
    if path.is_dir() {
        return Err("database path names a directory".to_owned());
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if parent.is_some_and(|parent| !parent.is_dir()) {
        return Err("database parent directory does not exist".to_owned());
    }
    Ok(())
}

fn run_database_command(arguments: &[String]) -> Result<bool, String> {
    if let Some(path) = argument_value(arguments, "--verify-database") {
        let verification = SqliteStore::verify_database(path).map_err(|error| error.to_string())?;
        println!(
            "{}",
            serde_json::to_string_pretty(&verification).map_err(|error| error.to_string())?
        );
        return Ok(true);
    }
    if let Some(path) = argument_value(arguments, "--restore-check") {
        let verification = SqliteStore::verify_restore(path).map_err(|error| error.to_string())?;
        println!(
            "{}",
            serde_json::to_string_pretty(&verification).map_err(|error| error.to_string())?
        );
        return Ok(true);
    }
    if let Some(destination) = argument_value(arguments, "--backup") {
        let source = std::env::var("KBOARD_DB")
            .map_err(|_| "KBOARD_DB is required for --backup".to_owned())?;
        validate_persistent_path(FsPath::new(&source))?;
        let destination = FsPath::new(destination);
        validate_persistent_path(destination)?;
        if destination.exists() {
            return Err("backup destination already exists".to_owned());
        }
        let store = SqliteStore::open(&source).map_err(|error| error.to_string())?;
        let backup = store
            .online_backup(destination)
            .map_err(|error| error.to_string())?;
        let isolated_restore =
            SqliteStore::verify_restore(destination).map_err(|error| error.to_string())?;
        let evidence = BackupEvidence {
            backup,
            isolated_restore,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&evidence).map_err(|error| error.to_string())?
        );
        return Ok(true);
    }
    Ok(false)
}

fn non_empty_operations(ops: Vec<StampedOp>) -> Option<Vec<StampedOp>> {
    (!ops.is_empty()).then_some(ops)
}

// -- authority -------------------------------------------------------------

/// Browsers cannot set headers on a WebSocket handshake, so the token travels
/// as a subprotocol rather than a query parameter. A URL ends up in server
/// logs, browser history, and referrers; a subprotocol does not.
const TOKEN_PROTOCOL_PREFIX: &str = "kboard.token.";
const VERSION_1_SUBPROTOCOL: &str = "kboard.v1";

/// Extracts the bearer token a client offered, if any.
fn offered_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)?
        .to_str()
        .ok()?
        .split(',')
        .map(str::trim)
        .find_map(|protocol| protocol.strip_prefix(TOKEN_PROTOCOL_PREFIX))
}

fn offered_v2(headers: &HeaderMap) -> bool {
    headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|protocol| protocol == VERSION_2_SUBPROTOCOL)
        })
}

fn offered_v1(headers: &HeaderMap) -> bool {
    headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|protocol| protocol == VERSION_1_SUBPROTOCOL)
        })
}

#[tokio::main]
async fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let port: u16 = match std::env::var("PORT") {
        Ok(value) => match value.parse() {
            Ok(port) if port > 0 => port,
            _ => {
                eprintln!("k-board: PORT must be an integer from 1 through 65535");
                std::process::exit(1);
            }
        },
        Err(_) => 8080,
    };

    let web_root = PathBuf::from(std::env::var("KBOARD_WEB").unwrap_or_else(|_| "web".to_owned()));
    let wasm_path = PathBuf::from(
        std::env::var("KBOARD_WASM")
            .unwrap_or_else(|_| "target/wasm32-unknown-unknown/release/kboard.wasm".to_owned()),
    );

    let authority = Authority::from_env();

    // `--token <scope>` mints a grant and exits, so issuing one needs no
    // separate tool and no second copy of the token format.
    if let Some(scope) = arguments
        .iter()
        .position(|argument| argument == "--token")
        .and_then(|index| arguments.get(index + 1))
    {
        let ttl = arguments
            .iter()
            .position(|argument| argument == "--ttl")
            .and_then(|index| arguments.get(index + 1))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(auth::DEFAULT_TTL_SECONDS);
        match authority.mint(scope, ttl) {
            Some(token) => {
                println!("{token}");
                return;
            }
            None => {
                eprintln!("k-board: set KBOARD_SECRET before minting a token");
                std::process::exit(1);
            }
        }
    }

    match run_database_command(&arguments) {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("k-board: database command failed: {error}");
            std::process::exit(1);
        }
    }
    if !web_root.is_dir() {
        eprintln!("k-board: KBOARD_WEB is not a readable directory");
        std::process::exit(1);
    }
    if !wasm_path.is_file() {
        eprintln!("k-board: KBOARD_WASM is not a readable file");
        std::process::exit(1);
    }

    let bind: IpAddr = match std::env::var("KBOARD_BIND") {
        Ok(value) => match value.parse() {
            Ok(address) => address,
            Err(_) => {
                eprintln!("k-board: KBOARD_BIND is not a valid IP address");
                std::process::exit(1);
            }
        },
        Err(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
    };

    // Checked before a database is opened or a port is bound: an open server on
    // loopback is a development convenience, while an open server on any other
    // interface is an unauthenticated writable store on a network. That is not
    // a configuration to warn about.
    if !authority.permits_bind(bind) {
        eprintln!(
            "k-board: refusing to bind {bind} without authentication. \
             Set KBOARD_SECRET, or bind loopback."
        );
        std::process::exit(1);
    }
    let enforcing = authority.is_enforcing();
    let origin = match OriginPolicy::from_env(bind, port) {
        Ok(policy) => policy,
        Err(error) => {
            eprintln!("k-board: invalid origin policy: {error}");
            std::process::exit(1);
        }
    };
    let embedding = match EmbeddingPolicy::from_env() {
        Ok(policy) => policy,
        Err(error) => {
            eprintln!("k-board: invalid embedding origin policy: {error}");
            std::process::exit(1);
        }
    };

    // Durability is always on; whether it survives the process depends on
    // whether a path was given. One code path either way, so the in-memory
    // case cannot drift from the durable one.
    let database_path = std::env::var("KBOARD_DB").ok();
    if !bind.is_loopback() && database_path.is_none() {
        eprintln!("k-board: public binds require KBOARD_DB persistent storage");
        std::process::exit(1);
    }
    if let Some(path) = database_path.as_deref() {
        if let Err(error) = validate_persistent_path(FsPath::new(path)) {
            eprintln!("k-board: invalid KBOARD_DB: {error}");
            std::process::exit(1);
        }
    }
    let store = match database_path {
        Some(path) => SqliteStore::open(&path).map(|store| (store, Some(path))),
        None => SqliteStore::in_memory().map(|store| (store, None)),
    };
    let (store, database) = match store {
        Ok(opened) => opened,
        Err(error) => {
            eprintln!("k-board: cannot open store: {error}");
            std::process::exit(1);
        }
    };

    let storage = StorageWriter::start(store);
    let directory = ScopeDirectory::new(storage.clone());
    let state = AppState {
        directory: directory.clone(),
        storage,
        next_connection: Arc::new(AtomicU64::new(0)),
        admission: AdmissionControl::new(),
        wasm_path,
        authority: Arc::new(authority),
        origin,
        embedding,
        accepting: Arc::new(AtomicBool::new(true)),
        persistent_database: database.is_some(),
    };

    directory.spawn_sweeper();

    let static_files =
        ServeDir::new(&web_root).not_found_service(ServeFile::new(web_root.join("index.html")));
    let embedded_shell = ServeFile::new(web_root.join("index.html"));

    let app = Router::new()
        .route("/ws/{scope}", get(websocket))
        .route("/kboard.wasm", get(serve_wasm))
        .route("/health", get(liveness))
        .route("/health/live", get(liveness))
        .route("/health/ready", get(readiness))
        .route("/health/diagnostics", get(diagnostics))
        .route("/api/storage/health", get(storage_health))
        .route("/api/directory/health", get(directory_health))
        .route("/api/rooms/{scope}/stats", get(room_stats))
        .route_service("/embed", embedded_shell)
        .fallback_service(static_files)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security_headers,
        ))
        .with_state(state.clone());

    let address = SocketAddr::new(bind, port);
    let listener = match tokio::net::TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("k-board: cannot bind {address}: {error}");
            std::process::exit(1);
        }
    };

    println!("k-board server listening on http://{address}");
    println!("  open http://{address}/ in two tabs to see convergence");
    match &database {
        Some(path) => println!("  storing boards in {path}"),
        None => println!("  WARNING: in-memory store — boards are lost on restart (set KBOARD_DB)"),
    }
    if enforcing {
        println!("  authentication required; mint a grant with --token <scope>");
    } else {
        println!("  WARNING: no authentication — loopback only (set KBOARD_SECRET)");
    }

    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown(
            directory,
            state.storage.clone(),
            state.accepting.clone(),
        ))
        .await
    {
        eprintln!("k-board: server error: {error}");
    }
}

async fn shutdown(directory: ScopeDirectory, storage: StorageWriter, accepting: Arc<AtomicBool>) {
    let _ = tokio::signal::ctrl_c().await;
    accepting.store(false, Ordering::Release);
    println!("\nk-board: draining; new WebSockets are refused");
    let timeout = std::env::var("KBOARD_DRAIN_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| (1..=300).contains(seconds))
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(20));
    let report = directory.drain_all_bounded(timeout).await;
    let flush = tokio::time::timeout(timeout, storage.flush()).await;
    println!(
        "k-board: drain={} storage_flush={}",
        serde_json::to_string(&report).unwrap_or_else(|_| "unavailable".to_owned()),
        if matches!(flush, Ok(Ok(_))) {
            "complete"
        } else {
            "incomplete"
        }
    );
}

/// Conservative defaults for a page that loads wasm and opens a WebSocket.
///
/// The standalone shell is never frameable. Only the explicit embedded shell
/// uses the configured exact frame-ancestor policy.
async fn security_headers(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let embedded_shell = path == "/embed";
    let public_embed_asset = matches!(
        path,
        "/embed-sdk.js" | "/embed-contract.mjs" | "/embed-element.css"
    );
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if !embedded_shell {
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    }
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if public_embed_asset {
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
        headers.insert(
            header::HeaderName::from_static("cross-origin-resource-policy"),
            HeaderValue::from_static("cross-origin"),
        );
    }
    let frame_ancestors = if embedded_shell {
        state.embedding.frame_ancestors()
    } else {
        "'none'".to_owned()
    };
    let policy = format!(
        "default-src 'self'; \
         script-src 'self' 'wasm-unsafe-eval'; \
         style-src 'self'; \
         img-src 'self' data:; \
         connect-src 'self' ws: wss:; \
         base-uri 'none'; \
         object-src 'none'; \
         frame-ancestors {frame_ancestors}"
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_str(&policy).expect("validated origins produce a valid CSP"),
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
    match state.directory.existing(&scope).await {
        Some(cell) => match cell.stats().await {
            Ok(stats) => axum::Json(stats).into_response(),
            Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "room unavailable").into_response(),
        },
        None => (StatusCode::NOT_FOUND, "no such room").into_response(),
    }
}

async fn liveness() -> Response {
    axum::Json(serde_json::json!({ "status": "live" })).into_response()
}

#[derive(Serialize)]
struct ReadinessResponse {
    status: &'static str,
    reasons: Vec<&'static str>,
}

fn readiness_reasons(
    accepting: bool,
    directory: &directory::DirectoryStats,
    storage: Option<&storage_writer::StorageHealth>,
) -> Vec<&'static str> {
    let mut reasons = Vec::new();
    if !accepting {
        reasons.push("draining");
    }
    let Some(storage) = storage else {
        reasons.push("storage_unavailable");
        return reasons;
    };
    if !storage.readable || !storage.writable {
        reasons.push("storage_not_read_write");
    }
    if storage.metrics.queue_depth >= (limits::STORAGE_MAILBOX_CAPACITY as u64 * 3 / 4) {
        reasons.push("storage_saturated");
    }
    if directory.restoring >= limits::MAX_RESTORING_ROOMS
        || (directory.restoring > 0 && directory.restore_permits_available == 0)
    {
        reasons.push("restore_saturated");
    }
    reasons
}

async fn readiness(State(state): State<AppState>) -> Response {
    let directory = state.directory.stats().await;
    let storage = state.storage.health().await.ok();
    let reasons = readiness_reasons(
        state.accepting.load(Ordering::Acquire),
        &directory,
        storage.as_ref(),
    );
    let ready = reasons.is_empty();
    let body = axum::Json(ReadinessResponse {
        status: if ready { "ready" } else { "not_ready" },
        reasons,
    });
    if ready {
        body.into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, body).into_response()
    }
}

#[derive(Serialize)]
struct DiagnosticsResponse {
    status: &'static str,
    accepting: bool,
    persistent_database: bool,
    reasons: Vec<&'static str>,
    directory: directory::DirectoryStats,
    storage: Option<StorageHealthResponse>,
    latency_buckets_micros: [u64; 12],
}

async fn diagnostics(State(state): State<AppState>) -> Response {
    let accepting = state.accepting.load(Ordering::Acquire);
    let directory = state.directory.stats().await;
    let storage = state.storage.health().await.ok();
    let reasons = readiness_reasons(accepting, &directory, storage.as_ref());
    let degraded = !state.persistent_database
        || directory.failed > 0
        || storage
            .as_ref()
            .is_some_and(|health| health.checkpoint.busy > 0);
    let status = if !accepting {
        "draining"
    } else if !reasons.is_empty() {
        "not_ready"
    } else if degraded {
        "degraded"
    } else {
        "ready"
    };
    axum::Json(DiagnosticsResponse {
        status,
        accepting,
        persistent_database: state.persistent_database,
        reasons,
        directory,
        storage: storage.map(StorageHealthResponse::from),
        latency_buckets_micros: telemetry::LATENCY_BUCKETS_MICROS,
    })
    .into_response()
}

#[derive(Serialize)]
struct StorageHealthResponse {
    schema_version: u32,
    wal_busy: u32,
    wal_log_frames: u32,
    wal_checkpointed_frames: u32,
    writer_concurrency: u32,
    max_operations_per_batch: usize,
    readable: bool,
    writable: bool,
    metrics: storage_writer::StorageMetricsSnapshot,
}

impl From<storage_writer::StorageHealth> for StorageHealthResponse {
    fn from(health: storage_writer::StorageHealth) -> Self {
        Self {
            schema_version: health.schema_version,
            wal_busy: health.checkpoint.busy,
            wal_log_frames: health.checkpoint.log_frames,
            wal_checkpointed_frames: health.checkpoint.checkpointed_frames,
            writer_concurrency: 1,
            max_operations_per_batch: limits::MAX_OPS_PER_FRAME,
            readable: health.readable,
            writable: health.writable,
            metrics: health.metrics,
        }
    }
}

async fn storage_health(State(state): State<AppState>) -> Response {
    match state.storage.health().await {
        Ok(health) => axum::Json(StorageHealthResponse::from(health)).into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "storage unavailable").into_response(),
    }
}

#[derive(Serialize)]
struct DirectoryHealthResponse {
    directory: directory::DirectoryStats,
    admission: security::AdmissionStats,
    room_mailbox_capacity: usize,
    storage_mailbox_capacity: usize,
    max_concurrent_restores: usize,
}

async fn directory_health(State(state): State<AppState>) -> Response {
    axum::Json(DirectoryHealthResponse {
        directory: state.directory.stats().await,
        admission: state.admission.stats(),
        room_mailbox_capacity: limits::CELL_MAILBOX_CAPACITY,
        storage_mailbox_capacity: limits::STORAGE_MAILBOX_CAPACITY,
        max_concurrent_restores: limits::MAX_CONCURRENT_RESTORES,
    })
    .into_response()
}

async fn websocket(
    upgrade: WebSocketUpgrade,
    Path(scope): Path<String>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Response {
    if !state.accepting.load(Ordering::Acquire) {
        return (StatusCode::SERVICE_UNAVAILABLE, "draining").into_response();
    }
    if !limits::scope_is_acceptable(&scope) {
        return (StatusCode::BAD_REQUEST, "invalid scope").into_response();
    }

    if state.origin.check(&headers).is_err() {
        return (StatusCode::FORBIDDEN, "not permitted").into_response();
    }

    let token = offered_token(&headers);
    let grant = match state.authority.verify(token, &scope) {
        Ok(grant) => grant,
        Err(reason) => {
            // The public response is deliberately generic. Correlation is a
            // fixed digest prefix, never the tenant/scope string or bearer.
            eprintln!(
                "k-board: refused scope={} reason={reason:?}",
                scope_correlation(&scope)
            );
            return (StatusCode::UNAUTHORIZED, "not permitted").into_response();
        }
    };

    let version = if offered_v2(&headers) {
        Version::V2
    } else {
        Version::V1
    };

    // Enforced at the protocol layer as well as in the read loop, so an
    // oversized frame is rejected before it is ever fully buffered.
    let upgrade = upgrade.max_message_size(limits::MAX_FRAME_BYTES);
    let upgrade = match version {
        Version::V2 => upgrade.protocols([VERSION_2_SUBPROTOCOL]),
        Version::V1 if offered_v1(&headers) => upgrade.protocols([VERSION_1_SUBPROTOCOL]),
        Version::V1 => upgrade,
    };
    upgrade.on_upgrade(move |socket| session(socket, scope, state, version, grant))
}

enum ClientEvent {
    Ops {
        batch: Option<String>,
        ops: Vec<StampedOp>,
    },
    Presence {
        x: f64,
        y: f64,
    },
}

fn parse_client_event(version: Version, text: &str) -> Result<ClientEvent, ()> {
    match version {
        Version::V1 => match serde_json::from_str::<V1ClientMessage>(text).map_err(|_| ())? {
            V1ClientMessage::Ops { ops } => Ok(ClientEvent::Ops { batch: None, ops }),
            V1ClientMessage::Presence { x, y } => Ok(ClientEvent::Presence { x, y }),
        },
        Version::V2 => match serde_json::from_str::<V2ClientMessage>(text).map_err(|_| ())? {
            V2ClientMessage::Ops { batch, ops } => Ok(ClientEvent::Ops {
                batch: Some(batch),
                ops,
            }),
            V2ClientMessage::Presence { x, y } => Ok(ClientEvent::Presence { x, y }),
            V2ClientMessage::Hello { .. } => Err(()),
        },
    }
}

fn refusal(batch: Option<&str>, code: RefusalCode, retryable: bool) -> Option<String> {
    serde_json::to_string(&V2ServerMessage::Refused {
        batch,
        code,
        retryable,
    })
    .ok()
}

fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn room_refusal(refused: Refused) -> (RefusalCode, bool) {
    match refused {
        Refused::ActorMismatch => (RefusalCode::ActorMismatch, false),
        Refused::BatchConflict => (RefusalCode::BatchConflict, false),
        Refused::BatchTooLarge => (RefusalCode::BatchTooLarge, false),
        Refused::InvalidOperation => (RefusalCode::InvalidOperation, false),
        Refused::RoomFull => (RefusalCode::RoomFull, false),
    }
}

fn cell_error(error: CellError) -> (RefusalCode, bool) {
    match error {
        CellError::Overloaded | CellError::Deadline => (RefusalCode::Overloaded, true),
        CellError::Unavailable | CellError::Draining => (RefusalCode::NotDurable, true),
        CellError::ResourceLimit => (RefusalCode::RoomFull, false),
        CellError::Refused(refused) => room_refusal(refused),
    }
}

async fn session(
    socket: WebSocket,
    scope: String,
    state: AppState,
    version: Version,
    grant: Grant,
) {
    let connection = state.next_connection.fetch_add(1, Ordering::Relaxed) + 1;
    let (mut outbound, mut inbound) = socket.split();
    let Ok(_connection_claim) = state.admission.claim_connection(&scope) else {
        if version == Version::V2 {
            if let Some(payload) = refusal(None, RefusalCode::Overloaded, true) {
                let _ = outbound.send(Message::Text(payload.into())).await;
            }
        }
        let _ = outbound.send(Message::Close(None)).await;
        return;
    };

    let (replica, _replica_claim) = match version {
        Version::V1 => (None, None),
        Version::V2 => {
            let hello = tokio::time::timeout(Duration::from_secs(5), inbound.next()).await;
            let replica = match hello {
                Ok(Some(Ok(Message::Text(text)))) => {
                    match serde_json::from_str::<V2ClientMessage>(&text) {
                        Ok(V2ClientMessage::Hello { version, replica })
                            if version == VERSION_2 && valid_opaque_id(&replica) =>
                        {
                            Some(replica)
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            let Some(replica) = replica else {
                if let Some(payload) = refusal(None, RefusalCode::Protocol, false) {
                    let _ = outbound.send(Message::Text(payload.into())).await;
                }
                let _ = outbound.send(Message::Close(None)).await;
                return;
            };
            let claim = match state.admission.claim_replica(&scope, &replica, connection) {
                Ok(claim) => claim,
                Err(AdmissionError::ReplicaInUse) => {
                    if let Some(payload) = refusal(None, RefusalCode::ReplicaInUse, true) {
                        let _ = outbound.send(Message::Text(payload.into())).await;
                    }
                    let _ = outbound.send(Message::Close(None)).await;
                    return;
                }
                Err(_) => {
                    if let Some(payload) = refusal(None, RefusalCode::Overloaded, true) {
                        let _ = outbound.send(Message::Text(payload.into())).await;
                    }
                    let _ = outbound.send(Message::Close(None)).await;
                    return;
                }
            };
            (Some(replica), Some(claim))
        }
    };
    let actor = replica
        .as_deref()
        .map_or(ActorId(connection), |replica| actor_for(&scope, replica));

    let cell = match state.directory.open(&scope).await {
        Ok(cell) => cell,
        Err(error) => {
            if version == Version::V2 {
                let (code, retryable) = match error {
                    OpenError::Capacity => (RefusalCode::RoomFull, false),
                    OpenError::RestoreCapacity | OpenError::FailedCapacity => {
                        (RefusalCode::Overloaded, true)
                    }
                    OpenError::Failed | OpenError::Draining => (RefusalCode::NotDurable, true),
                };
                if let Some(payload) = refusal(None, code, retryable) {
                    let _ = outbound.send(Message::Text(payload.into())).await;
                }
            }
            let _ = outbound.send(Message::Close(None)).await;
            return;
        }
    };
    let joined = match cell.join(version, actor, replica.clone()).await {
        Ok(joined) => joined,
        Err(error) => {
            if version == Version::V2 {
                let (code, retryable) = cell_error(error);
                if let Some(payload) = refusal(None, code, retryable) {
                    let _ = outbound.send(Message::Text(payload.into())).await;
                }
            }
            let _ = outbound.send(Message::Close(None)).await;
            return;
        }
    };
    let mut updates = joined.updates;
    if outbound
        .send(Message::Text(joined.init.into()))
        .await
        .is_err()
    {
        return;
    }

    let (direct, mut direct_messages) = mpsc::channel::<String>(32);
    let mut relay = tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(payload) = direct_messages.recv() => {
                    if outbound.send(Message::Text(payload.into())).await.is_err() {
                        break;
                    }
                }
                update = updates.recv() => match update {
                    Ok(event) => {
                        if event.origin == connection {
                            continue;
                        }
                        let payload = match version {
                            Version::V1 => event.v1,
                            Version::V2 => event.v2,
                        };
                        if outbound.send(Message::Text(payload.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(missed)) => {
                        eprintln!(
                            "k-board: connection {connection} lagged {missed} messages, closing to force resync"
                        );
                        break;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    });

    let receiving_cell = cell.clone();
    let receiving_admission = state.admission.clone();
    let receiving_scope = scope.clone();
    let receiving_replica = replica.clone();
    let mut receive = tokio::spawn(async move {
        let mut limiter = RateLimiter::new();
        while let Some(Ok(message)) = inbound.next().await {
            let Message::Text(text) = message else {
                continue;
            };
            if text.len() > limits::MAX_FRAME_BYTES {
                break;
            }
            if !limiter.allow()
                || !receiving_admission.allow_frame(&receiving_scope, receiving_replica.as_deref())
            {
                if version == Version::V2 {
                    if let Some(payload) = refusal(None, RefusalCode::Overloaded, true) {
                        let _ = direct.try_send(payload);
                    }
                }
                if limiter.should_disconnect() {
                    eprintln!("k-board: connection {connection} exceeded its rate budget, closing");
                    break;
                }
                continue;
            }

            let event = match parse_client_event(version, &text) {
                Ok(event) => event,
                Err(()) => {
                    if version == Version::V2 {
                        if let Some(payload) = refusal(None, RefusalCode::Protocol, false) {
                            let _ = direct.try_send(payload);
                        }
                    }
                    continue;
                }
            };

            let (batch, ops) = match event {
                ClientEvent::Presence { x, y } => {
                    if !x.is_finite() || !y.is_finite() {
                        continue;
                    }
                    let Ok(v1) = serde_json::to_string(&V1ServerMessage::Presence {
                        actor: actor.0,
                        x,
                        y,
                    }) else {
                        continue;
                    };
                    let Ok(v2) = serde_json::to_string(&V2ServerMessage::Presence {
                        version: VERSION_2,
                        actor: actor.0,
                        x,
                        y,
                    }) else {
                        continue;
                    };
                    if receiving_cell
                        .presence(Fanout {
                            origin: connection,
                            v1,
                            v2,
                        })
                        .is_err()
                        && version == Version::V2
                    {
                        if let Some(payload) = refusal(None, RefusalCode::Overloaded, true) {
                            let _ = direct.try_send(payload);
                        }
                    }
                    continue;
                }
                ClientEvent::Ops { batch, ops } => (batch, ops),
            };

            let Some(ops) = non_empty_operations(ops) else {
                if version == Version::V2 {
                    if let Some(payload) =
                        refusal(batch.as_deref(), RefusalCode::InvalidBatch, false)
                    {
                        let _ = direct.try_send(payload);
                    }
                }
                continue;
            };

            let recorded_at = current_time_millis();
            if version == Version::V2 {
                let batch_valid = batch.as_deref().is_some_and(valid_opaque_id);
                if !batch_valid {
                    if let Some(payload) =
                        refusal(batch.as_deref(), RefusalCode::InvalidBatch, false)
                    {
                        let _ = direct.try_send(payload);
                    }
                    continue;
                }
                if let Err(error) = validate_stamps(&ops, actor, recorded_at) {
                    let code = match error {
                        StampError::ActorMismatch => RefusalCode::ActorMismatch,
                        StampError::FutureSkew => RefusalCode::ClockSkew,
                    };
                    if let Some(payload) = refusal(batch.as_deref(), code, false) {
                        let _ = direct.try_send(payload);
                    }
                    continue;
                }
            }

            let Ok(v1) = serde_json::to_string(&V1ServerMessage::Ops { ops: &ops }) else {
                continue;
            };
            let payload_hash = match operations_hash(&ops) {
                Ok(hash) => hash,
                Err(_) => {
                    if version == Version::V2 {
                        if let Some(payload) =
                            refusal(batch.as_deref(), RefusalCode::InvalidOperation, false)
                        {
                            let _ = direct.try_send(payload);
                        }
                    }
                    continue;
                }
            };
            let outcome = receiving_cell
                .commit(
                    CommitRequest {
                        origin: connection,
                        version,
                        actor,
                        replica: receiving_replica.clone(),
                        batch: batch.clone(),
                        operations: ops,
                        payload_hash,
                        recorded_at_millis: recorded_at,
                        v1_payload: v1,
                    },
                    direct.clone(),
                )
                .await;

            match outcome {
                Ok(CommitOutcome::Committed(_) | CommitOutcome::Duplicate(_)) => {}
                Err(error) => {
                    let (code, retryable) = cell_error(error);
                    if version == Version::V2 {
                        if let Some(payload) = refusal(batch.as_deref(), code, retryable) {
                            let _ = direct.try_send(payload);
                        }
                    }
                    if version == Version::V1
                        && matches!(code, RefusalCode::BatchTooLarge | RefusalCode::NotDurable)
                    {
                        break;
                    }
                }
            }
        }
    });

    let expires = grant.remaining();
    let expiry = async move {
        match expires {
            Some(remaining) => tokio::time::sleep(remaining).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(expiry);
    enum SessionEnd {
        Relay,
        Receive,
        Expiry,
    }
    let ended = tokio::select! {
        _ = &mut relay => SessionEnd::Relay,
        _ = &mut receive => SessionEnd::Receive,
        _ = &mut expiry => SessionEnd::Expiry,
    };
    match ended {
        // The selected JoinHandle has already yielded its output. Polling it a
        // second time panics and used to skip the departure command entirely.
        SessionEnd::Relay => {
            receive.abort();
            let _ = receive.await;
        }
        SessionEnd::Receive => {
            relay.abort();
            let _ = relay.await;
        }
        SessionEnd::Expiry => {
            relay.abort();
            receive.abort();
            let _ = relay.await;
            let _ = receive.await;
        }
    }

    let v1 = serde_json::to_string(&V1ServerMessage::Left { actor: actor.0 });
    let v2 = serde_json::to_string(&V2ServerMessage::Left {
        version: VERSION_2,
        actor: actor.0,
    });
    if let (Ok(v1), Ok(v2)) = (v1, v2) {
        cell.leave(Fanout {
            origin: connection,
            v1,
            v2,
        });
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    #[test]
    fn an_empty_operation_batch_is_an_ignored_no_op() {
        let message = serde_json::from_str::<V1ClientMessage>(r#"{"type":"ops","ops":[]}"#)
            .expect("empty batches are syntactically valid");
        let V1ClientMessage::Ops { ops } = message else {
            panic!("expected an operation batch");
        };

        assert!(non_empty_operations(ops).is_none());
    }

    #[test]
    fn malformed_client_message_corpus_is_refused_without_panicking() {
        let malformed = [
            "",
            "{",
            "null",
            "[]",
            r#"{"type":"unknown"}"#,
            r#"{"type":"ops"}"#,
            r#"{"type":"ops","ops":{}}"#,
            r#"{"type":"presence","x":"left","y":1}"#,
            r#"{"type":"ops","ops":[{"stamp":null,"op":null}]}"#,
        ];
        for input in malformed {
            assert!(
                serde_json::from_str::<V1ClientMessage>(input).is_err(),
                "unexpectedly accepted {input:?}"
            );
        }

        // Truncating a valid seed at every byte supplies a cheap deterministic
        // mutation corpus in ordinary CI. Dedicated coverage-guided fuzzing is
        // still appropriate for the broader security gate.
        let seed = r#"{"type":"presence","x":1,"y":2}"#;
        for boundary in 0..seed.len() {
            let _ = serde_json::from_str::<V1ClientMessage>(&seed[..boundary]);
        }
    }
}

#[cfg(test)]
mod operational_tests {
    use super::*;

    #[test]
    fn persistent_path_validation_rejects_directories_and_missing_parents() {
        assert!(validate_persistent_path(&std::env::temp_dir()).is_err());
        let missing = std::env::temp_dir()
            .join("kboard-parent-that-must-not-exist")
            .join("boards.sqlite3");
        assert!(validate_persistent_path(&missing).is_err());
        assert!(validate_persistent_path(FsPath::new("boards.sqlite3")).is_ok());
    }

    #[test]
    fn readiness_fails_closed_for_drain_restore_and_storage_saturation() {
        let mut directory = directory::DirectoryStats::default();
        let mut storage = storage_writer::StorageHealth {
            schema_version: 1,
            checkpoint: kboard_store::CheckpointHealth {
                busy: 0,
                log_frames: 0,
                checkpointed_frames: 0,
            },
            metrics: storage_writer::StorageMetricsSnapshot::default(),
            readable: true,
            writable: true,
        };
        assert!(readiness_reasons(true, &directory, Some(&storage)).is_empty());
        assert_eq!(
            readiness_reasons(false, &directory, Some(&storage)),
            ["draining"]
        );

        directory.restoring = 1;
        directory.restore_permits_available = 0;
        assert!(readiness_reasons(true, &directory, Some(&storage)).contains(&"restore_saturated"));

        directory = directory::DirectoryStats::default();
        storage.metrics.queue_depth = limits::STORAGE_MAILBOX_CAPACITY as u64;
        assert!(readiness_reasons(true, &directory, Some(&storage)).contains(&"storage_saturated"));
        assert_eq!(
            readiness_reasons(true, &directory, None),
            ["storage_unavailable"]
        );
    }
}
