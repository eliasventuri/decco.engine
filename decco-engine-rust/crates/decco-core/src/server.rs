use crate::cache::CacheManager;
use crate::config::{find_exact_video_file, get_download_id, get_video_content_type};
use crate::downloads::DownloadsManager;
use crate::live_proxy::{handle_live_proxy, LiveProxyQuery};
use crate::torrent::{prefetch_torrent_cache_file, TorrentEngineManager};
use crate::transcode::{
    build_hls_vod_manifest, probe_media_url, spawn_hls_segment_transcoder,
    spawn_subtitle_extractor, srt_to_webvtt,
};
use axum::{
    body::Body,
    extract::{ConnectInfo, Path as AxumPath, Query, Request, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use std::convert::Infallible;
use std::io::SeekFrom;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt};
use tokio::sync::watch;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as _;
use tokio_util::io::ReaderStream;
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct AppState {
    pub torrent_mgr: Arc<TorrentEngineManager>,
    pub downloads_mgr: Arc<DownloadsManager>,
    pub cache_mgr: Arc<CacheManager>,
    #[cfg(feature = "lan-server")]
    pub lan_mgr: Arc<crate::lan::LanSecurityManager>,
    pub shutdown: watch::Sender<bool>,
    pub auth_token: Option<String>,
    pub http_client: reqwest::Client,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StartStreamQuery {
    pub file_idx: Option<usize>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HlsQuery {
    pub audio_track: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StartDownloadQuery {
    pub hash: Option<String>,
    pub title: Option<String>,
    pub imdb_id: Option<String>,
    pub poster: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub file_idx: Option<isize>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SettingsQuery {
    pub max_concurrent: Option<usize>,
    pub auto_pause_on_playback: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SettingsBody {
    pub max_concurrent: Option<usize>,
    pub auto_pause_on_playback: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ReorderBody {
    pub order: Vec<String>,
}

async fn handle_download_reorder(
    State(state): State<AppState>,
    Json(body): Json<ReorderBody>,
) -> impl IntoResponse {
    let updated = state.downloads_mgr.reorder_downloads(&body.order);
    Json(json!({ "status": "reordered", "updated": updated }))
}

#[derive(Debug, Deserialize, Default)]
pub struct MeteredQuery {
    pub value: Option<bool>,
}

fn is_trusted_loopback_origin(origin: &str) -> bool {
    let lower = origin.trim().to_ascii_lowercase();
    lower == "https://decco.tv"
        || lower == "https://www.decco.tv"
        || lower.ends_with(".decco.tv")
        || lower.ends_with(".vercel.app")
        || lower.starts_with("http://localhost:")
        || lower == "http://localhost"
        || lower.starts_with("http://127.0.0.1:")
        || lower == "http://127.0.0.1"
        || lower.starts_with("chrome-extension://")
}

/// Middleware for CORS, Private Network Access (PNA), Origin verification, and Loopback IP security
async fn cors_and_loopback_middleware(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let ip = addr.ip();
    if !ip.is_loopback() && !ip.is_unspecified() {
        warn!("[Security] Blocked non-local request from {}", ip);
        return (
            StatusCode::FORBIDDEN,
            "Access denied: connections allowed only from localhost",
        )
            .into_response();
    }

    if let Some(origin_val) = req.headers().get("origin").and_then(|v| v.to_str().ok()) {
        if !is_trusted_loopback_origin(origin_val) {
            warn!("[Security] Blocked untrusted browser Origin on loopback: {}", origin_val);
            return (
                StatusCode::FORBIDDEN,
                "Access denied: untrusted browser origin",
            )
                .into_response();
        }
    }

    let is_pna = req
        .headers()
        .contains_key("access-control-request-private-network");

    if req.method() == Method::OPTIONS {
        let mut resp = StatusCode::OK.into_response();
        let headers = resp.headers_mut();
        headers.insert("Access-Control-Allow-Origin", "*".parse().unwrap());
        headers.insert(
            "Access-Control-Allow-Methods",
            "GET,POST,PUT,DELETE,OPTIONS".parse().unwrap(),
        );
        headers.insert(
            "Access-Control-Allow-Headers",
            "Content-Type, Range, Authorization, X-Requested-With, Accept, Cache-Control"
                .parse()
                .unwrap(),
        );
        if is_pna {
            headers.insert(
                "Access-Control-Allow-Private-Network",
                "true".parse().unwrap(),
            );
        }
        return resp;
    }

    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    headers.insert("Access-Control-Allow-Origin", "*".parse().unwrap());
    headers.insert(
        "Access-Control-Allow-Methods",
        "GET,POST,PUT,DELETE,OPTIONS".parse().unwrap(),
    );
    headers.insert(
        "Access-Control-Allow-Headers",
        "Content-Type, Range, Authorization, X-Requested-With, Accept, Cache-Control"
            .parse()
            .unwrap(),
    );
    if is_pna {
        headers.insert(
            "Access-Control-Allow-Private-Network",
            "true".parse().unwrap(),
        );
    }
    resp
}

async fn auth_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if let Some(expected) = state.auth_token {
        let valid = req
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(|value| value == format!("Bearer {expected}"))
            .unwrap_or(false);
        if !valid && req.method() != Method::OPTIONS {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    }
    next.run(req).await
}

pub fn build_router(state: AppState) -> Router {
    let router = Router::new()
        .route("/ping", get(handle_ping))
        .route("/status/check", get(handle_status_check))
        .route("/prewarm/:hash", get(handle_prewarm))
        .route("/start/:hash", get(handle_start))
        .route("/pause/:hash", get(handle_pause).post(handle_pause))
        .route("/stop/:hash", get(handle_stop).post(handle_stop))
        .route("/status/:hash", get(handle_status))
        .route("/status/events/:hash", get(handle_status_events))
        .route("/proxy/:hash", get(handle_proxy))
        .route(
            "/cache/clear",
            get(handle_cache_clear)
                .post(handle_cache_clear)
                .delete(handle_cache_clear),
        )
        .route("/cache", get(handle_cache_clear).delete(handle_cache_clear))
        .route(
            "/network/metered",
            get(handle_network_metered).post(handle_network_metered),
        )
        .route("/hls/:hash/:file", get(handle_hls_route))
        .route("/subtitles/:hash", get(handle_list_subtitles))
        .route(
            "/subtitles/:hash/extract/:index",
            get(handle_extract_subtitle),
        )
        .route("/live/proxy", get(handle_live_proxy_route))
        .route(
            "/download/start",
            get(handle_download_start).post(handle_download_start),
        )
        .route("/download/list", get(handle_download_list))
        .route("/download/events", get(handle_download_events))
        .route(
            "/download/pause/:id",
            get(handle_download_pause).post(handle_download_pause),
        )
        .route(
            "/download/resume/:id",
            get(handle_download_resume).post(handle_download_resume),
        )
        .route(
            "/download/delete/:id",
            get(handle_download_delete)
                .post(handle_download_delete)
                .delete(handle_download_delete),
        )
        .route("/download/open/:id", get(handle_download_open))
        .route("/download/force-start/:id", get(handle_download_force_start))
        .route(
            "/download/pause-all",
            get(handle_download_pause_all).post(handle_download_pause_all),
        )
        .route(
            "/download/resume-all",
            get(handle_download_resume_all).post(handle_download_resume_all),
        )
        .route(
            "/download/clear-completed",
            get(handle_download_clear_completed).post(handle_download_clear_completed),
        )
        .route(
            "/download/reorder",
            axum::routing::post(handle_download_reorder),
        )
        .route("/download/settings", get(handle_download_settings))
        .route(
            "/download/settings/set",
            get(handle_download_settings_set).post(handle_download_settings_set),
        )
        .route(
            "/download/subtitle/:hash/:lang",
            get(handle_download_subtitle),
        );

    #[cfg(feature = "lan-server")]
    let router = router
        .route("/lan/status", get(handle_lan_status))
        .route("/lan/bind-owner", axum::routing::post(handle_lan_bind_owner));

    router
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(middleware::from_fn(cors_and_loopback_middleware))
        .with_state(state)
}

#[cfg(feature = "lan-server")]
async fn handle_lan_status(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.lan_mgr.status_summary())
}

#[cfg(feature = "lan-server")]
async fn handle_lan_bind_owner(
    State(state): State<AppState>,
    Json(body): Json<crate::lan::BindOwnerRequest>,
) -> Response {
    match state
        .lan_mgr
        .bind_owner_from_session(&state.http_client, &body.session_token)
        .await
    {
        Ok((user_id, email)) => Json(json!({
            "status": "bound",
            "userId": user_id,
            "email": email
        }))
        .into_response(),
        Err(msg) => (StatusCode::UNAUTHORIZED, Json(json!({ "error": msg }))).into_response(),
    }
}

#[cfg(feature = "lan-server")]
fn is_safe_lan_path_identifiers(path: &str) -> bool {
    if path.contains("..") || path.contains('\\') || path.contains('\0') {
        return false;
    }
    true
}

#[cfg(feature = "lan-server")]
async fn lan_security_guard_middleware(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if !state.lan_mgr.is_enabled() {
        return (StatusCode::FORBIDDEN, "LAN sharing disabled").into_response();
    }

    if !state.lan_mgr.is_allowed_lan_peer(addr.ip()) {
        warn!("[LAN Security] Blocked peer outside local subnet: {}", addr);
        return (StatusCode::FORBIDDEN, "Peer IP outside local subnet").into_response();
    }

    // Block all web browser requests (CSRF / Private Network Access / DNS rebinding protection)
    if req.headers().contains_key("origin")
        || req.headers().contains_key("sec-fetch-mode")
        || req.headers().contains_key("sec-fetch-site")
    {
        warn!(
            "[LAN Security] Blocked browser request on LAN port from {}",
            addr.ip()
        );
        return (
            StatusCode::FORBIDDEN,
            "Browser origins forbidden on native LAN port",
        )
            .into_response();
    }

    // Host header validation to prevent DNS rebinding
    if let Some(host_hdr) = req.headers().get("host").and_then(|v| v.to_str().ok()) {
        let host_only = if let Some(stripped) = host_hdr.strip_prefix('[') {
            stripped.split(']').next().unwrap_or("")
        } else {
            host_hdr.split(':').next().unwrap_or("")
        };
        let valid_host = host_only.parse::<std::net::IpAddr>().is_ok()
            || host_only.eq_ignore_ascii_case("decco-lan.local")
            || (*state.lan_mgr.allow_loopback_peers_for_test.read()
                && host_only.eq_ignore_ascii_case("localhost"));
        if !valid_host {
            warn!("[LAN Security] Blocked invalid Host header: {}", host_hdr);
            return (StatusCode::FORBIDDEN, "Invalid Host header").into_response();
        }
    }

    if !is_safe_lan_path_identifiers(req.uri().path()) {
        return (StatusCode::BAD_REQUEST, "Malformed path").into_response();
    }

    next.run(req).await
}

#[cfg(feature = "lan-server")]
async fn lan_bearer_auth_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let token_opt = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")));

    match token_opt {
        Some(token) if state.lan_mgr.validate_bearer_token(token) => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Unauthorized LAN device" })),
        )
            .into_response(),
    }
}

#[cfg(feature = "lan-server")]
async fn handle_lan_hello(State(state): State<AppState>) -> impl IntoResponse {
    let summary = state.lan_mgr.status_summary();
    Json(json!({
        "status": "ok",
        "protocolVersion": 1,
        "nodeId": summary.node_id,
        "deviceName": summary.device_name,
        "httpsPort": summary.https_port,
        "certSha256": summary.cert_sha256,
        "hasOwner": summary.has_owner,
        "pairedDevicesCount": summary.paired_devices_count
    }))
}

#[cfg(feature = "lan-server")]
async fn handle_lan_pair(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<crate::lan::PairRequest>,
) -> Response {
    if !state.lan_mgr.check_pair_rate_limit(addr.ip()) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "Too many pairing attempts, try again later" })),
        )
            .into_response();
    }

    match state.lan_mgr.pair_tv_device(&state.http_client, body).await {
        Ok(resp) => Json(resp).into_response(),
        Err((code, msg)) => (code, Json(json!({ "error": msg }))).into_response(),
    }
}

/// Builds the strictly-scoped LAN TLS router (excludes desktop-only `/download/open`, `/live/proxy`, etc.)
#[cfg(feature = "lan-server")]
pub fn build_lan_router(state: AppState) -> Router {
    let protected_routes = Router::new()
        .route("/ping", get(handle_ping))
        .route("/status/check", get(handle_status_check))
        .route("/prewarm/:hash", get(handle_prewarm))
        .route("/start/:hash", get(handle_start))
        .route("/pause/:hash", get(handle_pause).post(handle_pause))
        .route("/stop/:hash", get(handle_stop).post(handle_stop))
        .route("/status/:hash", get(handle_status))
        .route("/status/events/:hash", get(handle_status_events))
        .route("/proxy/:hash", get(handle_proxy))
        .route("/hls/:hash/:file", get(handle_hls_route))
        .route("/subtitles/:hash", get(handle_list_subtitles))
        .route(
            "/subtitles/:hash/extract/:index",
            get(handle_extract_subtitle),
        )
        .route(
            "/download/start",
            get(handle_download_start).post(handle_download_start),
        )
        .route("/download/list", get(handle_download_list))
        .route("/download/events", get(handle_download_events))
        .route(
            "/download/pause/:id",
            get(handle_download_pause).post(handle_download_pause),
        )
        .route(
            "/download/resume/:id",
            get(handle_download_resume).post(handle_download_resume),
        )
        .route(
            "/download/delete/:id",
            get(handle_download_delete)
                .post(handle_download_delete)
                .delete(handle_download_delete),
        )
        .route("/download/force-start/:id", get(handle_download_force_start))
        .route("/download/settings", get(handle_download_settings))
        .route(
            "/download/settings/set",
            get(handle_download_settings_set).post(handle_download_settings_set),
        )
        .route(
            "/download/subtitle/:hash/:lang",
            get(handle_download_subtitle),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            lan_bearer_auth_middleware,
        ));

    let public_handshake_routes = Router::new()
        .route("/lan/hello", get(handle_lan_hello))
        .route("/lan/pair", axum::routing::post(handle_lan_pair));

    public_handshake_routes
        .merge(protected_routes)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            lan_security_guard_middleware,
        ))
        .with_state(state)
}

async fn handle_ping(State(state): State<AppState>) -> impl IntoResponse {
    let port = *state.torrent_mgr.primary_port.read();
    Json(json!({
        "status": "ok",
        "port": port,
        "version": "2.2",
        "engineVersion": env!("CARGO_PKG_VERSION"),
        "capabilities": {
            "streamSse": true,
            "downloadSse": true,
            "audioTracks": true
        },
        "pong": true
    }))
}

async fn handle_status_check() -> impl IntoResponse {
    let platform = if cfg!(target_os = "android") {
        "android"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        std::env::consts::OS
    };
    Json(json!({
        "status": "ok",
        "platform": platform,
        "version": env!("CARGO_PKG_VERSION")
    }))
}

async fn handle_prewarm(
    State(state): State<AppState>,
    AxumPath(hash): AxumPath<String>,
    Query(_query): Query<StartStreamQuery>,
) -> impl IntoResponse {
    let clean_hash = hash.trim().to_ascii_lowercase();
    if clean_hash.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Missing hash" })),
        );
    }
    state.torrent_mgr.cancel_stop_grace_timeout(&clean_hash);

    let state_bg = state.clone();
    let hash_bg = clean_hash.clone();
    tokio::spawn(async move {
        // Fast-fetch .torrent metadata via HTTP caches in parallel (~150ms, 0 disk/checksum overhead).
        // Do NOT spin up a full librqbit torrent engine on hover (especially for season packs where
        // season/episode may not be passed), so real /start calls never block behind hover checksums.
        let _ = prefetch_torrent_cache_file(
            &hash_bg,
            &state_bg.torrent_mgr.config.streaming_cache_dir,
        )
        .await;
    });

    (
        StatusCode::OK,
        Json(json!({ "status": "prewarming", "hash": clean_hash })),
    )
}

async fn handle_start(
    State(state): State<AppState>,
    AxumPath(hash): AxumPath<String>,
    Query(query): Query<StartStreamQuery>,
) -> impl IntoResponse {
    let clean_hash = hash.trim().to_ascii_lowercase();
    let dl_id = get_download_id(&clean_hash, query.season, query.episode, query.file_idx);

    state
        .downloads_mgr
        .pause_competing_streams_and_downloads(&clean_hash, Some(&dl_id));

    // Check if already completed in downloads database
    if let Some((entry, local_path)) = state.downloads_mgr.find_completed_video_path(&dl_id) {
        info!(
            "[HTTP Start] {} (ID: {}) is fully downloaded. Creating mock engine.",
            clean_hash, dl_id
        );
        state.torrent_mgr.register_mock_engine_from_local_file(
            &clean_hash,
            local_path,
            query.file_idx,
            entry.duration.unwrap_or(0.0),
        );
        return Json(json!({
            "status": "started",
            "hash": clean_hash,
            "fileIdx": query.file_idx,
            "season": query.season,
            "episode": query.episode
        }));
    }

    info!(
        "[HTTP Start] hash: {}, fileIdx: {:?}, S{:?}E{:?}",
        clean_hash, query.file_idx, query.season, query.episode
    );
    state
        .torrent_mgr
        .start_or_get_engine(&clean_hash, query.file_idx, query.season, query.episode, None)
        .await;

    Json(json!({
        "status": "started",
        "hash": clean_hash,
        "fileIdx": query.file_idx,
        "season": query.season,
        "episode": query.episode
    }))
}

async fn handle_pause(
    State(state): State<AppState>,
    AxumPath(hash): AxumPath<String>,
) -> impl IntoResponse {
    state.torrent_mgr.pause_engine(&hash);
    Json(json!({ "status": "paused", "hash": hash }))
}

async fn handle_stop(
    State(state): State<AppState>,
    AxumPath(hash): AxumPath<String>,
) -> impl IntoResponse {
    info!("[HTTP Stop] Stopping stream for hash: {}", hash);
    state
        .downloads_mgr
        .stop_stream_and_resume_auto_paused(&hash);
    Json(json!({ "status": "stopped", "hash": hash }))
}

async fn handle_status(
    State(state): State<AppState>,
    AxumPath(hash): AxumPath<String>,
) -> impl IntoResponse {
    Json(state.torrent_mgr.get_status(&hash))
}

async fn handle_status_events(
    State(state): State<AppState>,
    AxumPath(hash): AxumPath<String>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let clean_hash = hash.trim().to_ascii_lowercase();
    let initial_payload = serde_json::to_string(&state.torrent_mgr.get_status(&clean_hash))
        .unwrap_or_else(|_| "{}".to_string());
    let rx = state.torrent_mgr.status_tx.subscribe();

    let initial_stream = tokio_stream::once(Ok(Event::default().data(initial_payload.clone())));
    let last_sent = std::sync::Arc::new(parking_lot::Mutex::new(initial_payload));
    let broadcast_stream = BroadcastStream::new(rx).filter_map(move |msg| match msg {
        Ok((evt_hash, data)) if evt_hash == clean_hash => {
            let mut last = last_sent.lock();
            if *last == data {
                None
            } else {
                *last = data.clone();
                Some(Ok(Event::default().data(data)))
            }
        }
        _ => None,
    });

    let combined = initial_stream.chain(broadcast_stream);
    Sse::new(combined).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    )
}

async fn handle_cache_clear(State(state): State<AppState>) -> impl IntoResponse {
    let protected = state.downloads_mgr.protected_download_hashes();
    state.torrent_mgr.destroy_engines_except(&protected);
    match state.cache_mgr.clear_all_streaming_cache() {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "cleared": true })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "cleared": false, "error": e.to_string() })),
        ),
    }
}

async fn handle_network_metered(
    State(state): State<AppState>,
    Query(query): Query<MeteredQuery>,
) -> impl IntoResponse {
    let metered = query.value.unwrap_or(false);
    state.torrent_mgr.set_metered_mode(metered);
    Json(json!({ "status": "ok", "metered": metered }))
}

/// Parses HTTP `Range: bytes=start-end` header
fn parse_range_header(range_header: &str, file_size: u64) -> Option<(u64, u64)> {
    let raw = range_header.strip_prefix("bytes=")?.trim();
    if raw.contains(',') || file_size == 0 {
        return None;
    }
    let (start_str, end_str) = raw.split_once('-')?;
    let start_str = start_str.trim();
    let end_str = end_str.trim();

    if start_str.is_empty() {
        let suffix_len: u64 = end_str.parse().ok()?;
        if suffix_len == 0 {
            return None;
        }
        let start = file_size.saturating_sub(suffix_len);
        let end = file_size - 1;
        return Some((start, end));
    }

    let start: u64 = start_str.parse().ok()?;
    if start >= file_size {
        return None;
    }

    let end: u64 = if end_str.is_empty() {
        file_size - 1
    } else {
        end_str.parse::<u64>().ok()?.min(file_size - 1)
    };

    if end < start {
        return None;
    }
    Some((start, end))
}

/// Serves any `AsyncRead + AsyncSeek` stream with full HTTP 206 Partial Content / Range support
async fn serve_reader_with_range<R>(
    mut reader: R,
    file_size: u64,
    content_type: &'static str,
    headers: &HeaderMap,
) -> Response
where
    R: AsyncRead + AsyncSeek + Unpin + Send + 'static,
{
    if file_size == 0 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "File warming up (0 bytes)",
        )
            .into_response();
    }

    let range_header = headers.get("range").and_then(|v| v.to_str().ok());

    if let Some(range_str) = range_header {
        let Some((start, end)) = parse_range_header(range_str, file_size) else {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("Content-Range", format!("bytes */{}", file_size))
                .body(Body::empty())
                .unwrap();
        };

        if reader.seek(SeekFrom::Start(start)).await.is_err() {
            return (StatusCode::INTERNAL_SERVER_ERROR, "Seek error").into_response();
        }

        let chunk_size = (end - start) + 1;
        let limited = reader.take(chunk_size);
        let stream = ReaderStream::with_capacity(limited, 64 * 1024);

        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header("Content-Range", format!("bytes {}-{}/{}", start, end, file_size))
            .header("Accept-Ranges", "bytes")
            .header("Content-Length", chunk_size.to_string())
            .header("Content-Type", content_type)
            .body(Body::from_stream(stream))
            .unwrap()
    } else {
        let stream = ReaderStream::with_capacity(reader, 64 * 1024);
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Length", file_size.to_string())
            .header("Accept-Ranges", "bytes")
            .header("Content-Type", content_type)
            .body(Body::from_stream(stream))
            .unwrap()
    }
}

/// Serves a local file with full HTTP 206 Partial Content / Range support
async fn serve_file_with_range(
    file_path: &Path,
    declared_size: Option<u64>,
    headers: &HeaderMap,
) -> Response {
    let file = match File::open(file_path).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::NOT_FOUND, "File not found").into_response(),
    };

    let disk_size = match file.metadata().await {
        Ok(m) => m.len(),
        Err(_) => return (StatusCode::NOT_FOUND, "Metadata unavailable").into_response(),
    };
    let file_size = declared_size.unwrap_or(disk_size).max(disk_size);

    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("video.mp4");
    let content_type = get_video_content_type(file_name);

    serve_reader_with_range(file, file_size, content_type, headers).await
}

async fn handle_proxy(
    State(state): State<AppState>,
    AxumPath(hash): AxumPath<String>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let clean_hash = hash.trim().to_ascii_lowercase();
    state.torrent_mgr.cancel_stop_grace_timeout(&clean_hash);

    // 1. Check active engine first so a completed episode in a season pack doesn't shadow
    //    streaming a different episode from the same torrent hash.
    let active_non_mock = state
        .torrent_mgr
        .active_engines
        .read()
        .get(&clean_hash)
        .map(|e| !e.is_mock)
        .unwrap_or(false);

    if !active_non_mock {
        if let Some((_, local_path)) = state.downloads_mgr.find_completed_video_path(&clean_hash) {
            return serve_file_with_range(&local_path, None, &headers).await;
        }
    }

    // 2. Auto-start if not active yet (Android & direct player compatibility)
    if !state
        .torrent_mgr
        .active_engines
        .read()
        .contains_key(&clean_hash)
    {
        state
            .torrent_mgr
            .start_or_get_engine(&clean_hash, None, None, None, None)
            .await;
    }

    // Wait briefly if metadata is still resolving
    for _ in 0..40 {
        let ready = state
            .torrent_mgr
            .active_engines
            .read()
            .get(&clean_hash)
            .and_then(|e| e.video_file.clone());
        if ready.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let video_file = {
        let guard = state.torrent_mgr.active_engines.read();
        guard.get(&clean_hash).and_then(|e| e.video_file.clone())
    };

    let Some(vf) = video_file else {
        return (StatusCode::NOT_FOUND, "Video file not ready").into_response();
    };

    // Fast-path HTTP HEAD probes (browsers/VLC/ExoPlayer) without opening a librqbit FileStream
    if method == Method::HEAD {
        let content_type = get_video_content_type(&vf.name);
        return Response::builder()
            .status(StatusCode::OK)
            .header("Content-Length", vf.length.to_string())
            .header("Accept-Ranges", "bytes")
            .header("Content-Type", content_type)
            .body(Body::empty())
            .unwrap();
    }

    let start_byte = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(|r| parse_range_header(r, vf.length))
        .map(|(s, _)| s)
        .unwrap_or(0);

    state
        .torrent_mgr
        .on_proxy_range_request(&clean_hash, start_byte);

    #[cfg(feature = "rqbit-backend")]
    if let Some(torrent_stream) = state.torrent_mgr.open_stream_for_hash(&clean_hash) {
        let content_type = get_video_content_type(&vf.name);
        return serve_reader_with_range(torrent_stream, vf.length, content_type, &headers).await;
    }

    // Only fall back to direct disk read for mock (already-completed local) engines.
    // For active P2P torrents, reading a preallocated sparse file directly would serve raw zeroes.
    let is_mock_engine = state
        .torrent_mgr
        .active_engines
        .read()
        .get(&clean_hash)
        .map(|e| e.is_mock)
        .unwrap_or(false);
    if !is_mock_engine {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Torrent stream still initializing",
        )
            .into_response();
    }

    serve_file_with_range(&vf.path, Some(vf.length), &headers).await
}

async fn handle_hls_route(
    State(state): State<AppState>,
    AxumPath((hash, file)): AxumPath<(String, String)>,
    Query(query): Query<HlsQuery>,
) -> Response {
    let clean_hash = hash.trim().to_ascii_lowercase();
    state.torrent_mgr.cancel_stop_grace_timeout(&clean_hash);

    if file == "index.m3u8" {
        let existing_opt = state
            .torrent_mgr
            .active_engines
            .read()
            .get(&clean_hash)
            .cloned();
        let eng = if let Some(existing) = existing_opt {
            existing
        } else {
            state
                .torrent_mgr
                .start_or_get_engine(&clean_hash, None, None, None, None)
                .await
        };

        if !eng.metadata_ready {
            return (StatusCode::SERVICE_UNAVAILABLE, "Wait for metadata").into_response();
        }

        let manifest = build_hls_vod_manifest(eng.duration, query.audio_track);
        return (
            StatusCode::OK,
            [("Content-Type", "application/vnd.apple.mpegurl")],
            manifest,
        )
            .into_response();
    }

    if let Some(seg_str) = file
        .strip_prefix("segment-")
        .and_then(|s| s.strip_suffix(".ts"))
    {
        let Ok(seg_index) = seg_str.parse::<u64>() else {
            return StatusCode::BAD_REQUEST.into_response();
        };

        let video_codec = {
            let guard = state.torrent_mgr.active_engines.read();
            let Some(eng) = guard.get(&clean_hash) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            if eng.video_file.is_none() {
                return StatusCode::NOT_FOUND.into_response();
            }
            eng.video_codec.clone()
        };

        let port = *state.torrent_mgr.primary_port.read();
        let proxy_url = format!("http://127.0.0.1:{}/proxy/{}", port, clean_hash);

        match spawn_hls_segment_transcoder(
            &proxy_url,
            seg_index,
            video_codec.as_deref(),
            query.audio_track,
        )
        .await
        {
            Ok(mut child) => {
                let Some(stdout) = child.stdout.take() else {
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                };
                // Keep `child` alive inside the stream so `kill_on_drop(true)` terminates FFmpeg when client disconnects
                let stream = tokio_util::io::ReaderStream::new(stdout);
                let guarded_stream = futures_util::stream::unfold(
                    (stream, child),
                    |(mut s, c)| async move {
                        s.next().await.map(|item| (item, (s, c)))
                    },
                );

                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "video/mp2t")
                    .body(Body::from_stream(guarded_stream))
                    .unwrap()
            }
            Err(e) => {
                error!("[HLS] Failed to transcode segment {}: {}", seg_index, e);
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn handle_list_subtitles(
    State(state): State<AppState>,
    AxumPath(hash): AxumPath<String>,
) -> Response {
    let clean_hash = hash.trim().to_ascii_lowercase();
    let (cached_subs, cached_audio, is_mock) = {
        let guard = state.torrent_mgr.active_engines.read();
        let Some(eng) = guard.get(&clean_hash) else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Engine or video file not found" })),
            )
                .into_response();
        };
        if eng.video_file.is_none() {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Engine or video file not found" })),
            )
                .into_response();
        }
        (
            eng.embedded_subtitles.clone(),
            eng.audio_tracks.clone(),
            eng.is_mock,
        )
    };

    if let Some(subs) = cached_subs {
        return Json(json!({
            "subtitles": subs,
            "audioTracks": cached_audio.unwrap_or_default(),
            "pending": false
        }))
        .into_response();
    }

    // For active P2P torrents, do NOT spawn competing inline ffprobe processes inside the HTTP handler:
    // the background stats loop in torrent.rs probes once bytes flow and broadcasts results via SSE.
    if !is_mock {
        return Json(json!({
            "subtitles": [],
            "audioTracks": cached_audio.unwrap_or_default(),
            "pending": true
        }))
        .into_response();
    }

    let port = *state.torrent_mgr.primary_port.read();
    let proxy_url = format!("http://127.0.0.1:{}/proxy/{}", port, clean_hash);

    match probe_media_url(&proxy_url).await {
        Ok(probed) => {
            let subs = probed.embedded_subtitles.clone();
            let audio_tracks = probed.audio_tracks.clone();
            {
                let mut guard = state.torrent_mgr.active_engines.write();
                if let Some(eng) = guard.get_mut(&clean_hash) {
                    if probed.duration > 0.0 {
                        eng.duration = probed.duration;
                    }
                    eng.video_codec = probed.video_codec;
                    eng.audio_codec = probed.audio_codec;
                    eng.audio_tracks = Some(probed.audio_tracks);
                    eng.embedded_subtitles = Some(probed.embedded_subtitles);
                }
            }
            state.torrent_mgr.broadcast_stream_status(&clean_hash);
            Json(json!({
                "subtitles": subs,
                "audioTracks": audio_tracks
            }))
            .into_response()
        }
        Err(e) => {
            error!("[Subtitles] FFprobe error: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to probe video" })),
            )
                .into_response()
        }
    }
}

async fn handle_extract_subtitle(
    State(state): State<AppState>,
    AxumPath((hash, index)): AxumPath<(String, usize)>,
) -> Response {
    let clean_hash = hash.trim().to_ascii_lowercase();
    let has_video = state
        .torrent_mgr
        .active_engines
        .read()
        .get(&clean_hash)
        .and_then(|e| e.video_file.as_ref())
        .is_some();

    if !has_video {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Engine or video file not found" })),
        )
            .into_response();
    }

    let port = *state.torrent_mgr.primary_port.read();
    let proxy_url = format!("http://127.0.0.1:{}/proxy/{}", port, clean_hash);

    match spawn_subtitle_extractor(&proxy_url, index).await {
        Ok(mut child) => {
            let Some(stdout) = child.stdout.take() else {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            };
            let stream = tokio_util::io::ReaderStream::new(stdout);
            let guarded_stream =
                futures_util::stream::unfold((stream, child), |(mut s, c)| async move {
                    s.next().await.map(|item| (item, (s, c)))
                });

            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "text/vtt")
                .body(Body::from_stream(guarded_stream))
                .unwrap()
        }
        Err(e) => {
            error!("[Subtitles] Extraction error: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn handle_live_proxy_route(
    State(state): State<AppState>,
    query: Query<LiveProxyQuery>,
    headers: HeaderMap,
) -> Response {
    let port = *state.torrent_mgr.primary_port.read();
    handle_live_proxy(query, headers, port, &state.http_client).await
}

async fn handle_download_start(
    State(state): State<AppState>,
    Query(query): Query<StartDownloadQuery>,
) -> Response {
    let Some(ref hash) = query.hash else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "hash is required" })),
        )
            .into_response();
    };

    let title = query.title.unwrap_or_else(|| "Video".to_string());
    let imdb_id = query.imdb_id.unwrap_or_default();
    let poster = query.poster.unwrap_or_default();
    let season = query.season.unwrap_or(0);
    let episode = query.episode.unwrap_or(0);

    let meta = state.downloads_mgr.start_download_with_poster(
        hash,
        &title,
        &imdb_id,
        &poster,
        season,
        episode,
        query.file_idx.and_then(|idx| usize::try_from(idx).ok()),
        false,
    );

    Json(json!({
        "status": "started",
        "hash": meta.hash,
        "id": meta.id,
        "title": meta.title
    }))
    .into_response()
}

async fn handle_download_list(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.downloads_mgr.get_downloads_payload())
}

async fn handle_download_events(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let initial_payload = serde_json::to_string(&state.downloads_mgr.get_downloads_payload())
        .unwrap_or_else(|_| "{}".to_string());
    let rx = state.downloads_mgr.sse_tx.subscribe();

    let initial_stream =
        tokio_stream::once(Ok(Event::default().data(initial_payload)));
    let broadcast_stream = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(data) => Some(Ok(Event::default().data(data))),
        Err(_) => None,
    });

    let combined = initial_stream.chain(broadcast_stream);
    Sse::new(combined).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    )
}

async fn handle_download_pause(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.downloads_mgr.pause_download(&id) {
        Some(meta) => Json(json!({
            "status": "paused",
            "hash": meta.hash,
            "id": meta.id
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Download not found" })),
        )
            .into_response(),
    }
}

async fn handle_download_resume(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.downloads_mgr.resume_download(&id) {
        Some(meta) => Json(json!({
            "status": "resumed",
            "hash": meta.hash,
            "id": meta.id
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Download not found in meta" })),
        )
            .into_response(),
    }
}

async fn handle_download_force_start(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.downloads_mgr.force_start_download(&id) {
        Some(meta) => Json(json!({
            "status": "started",
            "hash": meta.hash,
            "id": meta.id
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Download not found" })),
        )
            .into_response(),
    }
}

async fn handle_download_delete(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> impl IntoResponse {
    let (hash, dl_id) = state.downloads_mgr.delete_download(&id);
    Json(json!({
        "status": "deleted",
        "hash": hash,
        "id": dl_id
    }))
}

async fn handle_download_open(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let entry_opt = {
        let db = state.downloads_mgr.db.read();
        db.downloads.get(&id).cloned().or_else(|| {
            db.downloads
                .values()
                .find(|v| v.hash.eq_ignore_ascii_case(&id))
                .cloned()
        })
    };

    let Some(entry) = entry_opt else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Download not found" })),
        )
            .into_response();
    };

    let dl_dir = Path::new(&entry.download_dir);
    let target_to_open = find_exact_video_file(dl_dir, entry.file_name.as_deref())
        .unwrap_or_else(|| dl_dir.to_path_buf());

    #[cfg(target_os = "windows")]
    {
        if target_to_open.is_file() {
            // Pass `/select,` as a separate arg so Rust's Windows arg quoting doesn't wrap `/select,` inside quotes.
            let _ = std::process::Command::new("explorer.exe")
                .arg("/select,")
                .arg(target_to_open.as_os_str())
                .spawn();
        } else if dl_dir.exists() {
            let _ = std::process::Command::new("explorer.exe")
                .arg(dl_dir.as_os_str())
                .spawn();
        }
    }

    #[cfg(target_os = "macos")]
    {
        if target_to_open.is_file() {
            let _ = std::process::Command::new("open")
                .arg("-R")
                .arg(target_to_open.as_os_str())
                .spawn();
        } else if dl_dir.exists() {
            let _ = std::process::Command::new("open")
                .arg(dl_dir.as_os_str())
                .spawn();
        }
    }

    #[cfg(target_os = "linux")]
    {
        if dl_dir.exists() {
            let _ = std::process::Command::new("xdg-open")
                .arg(dl_dir.as_os_str())
                .spawn();
        }
    }

    Json(json!({
        "status": "opened",
        "hash": entry.hash,
        "id": entry.id
    }))
    .into_response()
}

async fn handle_download_pause_all(State(state): State<AppState>) -> impl IntoResponse {
    let count = state.downloads_mgr.pause_all_downloads();
    Json(json!({ "status": "paused_all", "count": count }))
}

async fn handle_download_resume_all(State(state): State<AppState>) -> impl IntoResponse {
    let count = state.downloads_mgr.resume_all_downloads();
    Json(json!({ "status": "resumed_all", "count": count }))
}

async fn handle_download_clear_completed(State(state): State<AppState>) -> impl IntoResponse {
    let count = state.downloads_mgr.clear_completed_downloads();
    Json(json!({ "status": "cleared_completed", "count": count }))
}

async fn handle_download_settings(State(state): State<AppState>) -> impl IntoResponse {
    let settings = state.downloads_mgr.db.read().settings.clone();
    Json(json!({
        "settings": settings,
        "diskSpace": state.downloads_mgr.get_disk_space(),
        "downloadDir": state.downloads_mgr.config.completed_downloads_dir.to_string_lossy()
    }))
}

async fn handle_download_settings_set(
    State(state): State<AppState>,
    Query(query): Query<SettingsQuery>,
    body: Option<Json<SettingsBody>>,
) -> impl IntoResponse {
    let body_val = body.map(|Json(b)| b);
    let max_concurrent = query
        .max_concurrent
        .or_else(|| body_val.as_ref().and_then(|b| b.max_concurrent));
    let auto_pause = query
        .auto_pause_on_playback
        .as_deref()
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .or_else(|| {
            body_val
                .as_ref()
                .and_then(|b| b.auto_pause_on_playback.as_ref())
                .and_then(|v| match v {
                    serde_json::Value::Bool(b) => Some(*b),
                    serde_json::Value::Number(n) => Some(n.as_i64() == Some(1)),
                    serde_json::Value::String(s) => {
                        Some(s.eq_ignore_ascii_case("true") || s == "1")
                    }
                    _ => None,
                })
        });
    let updated = state
        .downloads_mgr
        .update_settings(max_concurrent, auto_pause);
    Json(json!({
        "status": "ok",
        "settings": updated
    }))
}

async fn handle_download_subtitle(
    State(state): State<AppState>,
    AxumPath((hash, lang)): AxumPath<(String, String)>,
) -> Response {
    let sub_path_opt = {
        let db = state.downloads_mgr.db.read();
        db.downloads
            .values()
            .find(|d| d.hash.eq_ignore_ascii_case(&hash))
            .and_then(|d| {
                d.subtitles
                    .iter()
                    .find(|s| s.lang.eq_ignore_ascii_case(&lang))
                    .map(|s| s.path.clone())
            })
    };

    let Some(sub_path) = sub_path_opt else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Subtitle not found" })),
        )
            .into_response();
    };

    match tokio::fs::read_to_string(&sub_path).await {
        Ok(raw_srt) => {
            let vtt = if raw_srt.trim_start().starts_with("WEBVTT") {
                raw_srt
            } else {
                srt_to_webvtt(&raw_srt)
            };
            (StatusCode::OK, [("Content-Type", "text/vtt")], vtt).into_response()
        }
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Subtitle file not found" })),
        )
            .into_response(),
    }
}

/// Starts the multi-port HTTP server (`[18889, 18888, 28888, 38888, 48888, 8888]`)
/// binding exclusively to `127.0.0.1` so the local control API is never exposed
/// to the LAN when the preferred port is unavailable.
/// Also launches the 72h cache cleanup loop and restores active downloads.
pub async fn start_multiport_server(state: AppState) -> anyhow::Result<Vec<u16>> {
    let ports = state.torrent_mgr.config.ports.clone();
    let mut bound_ports = Vec::new();

    for port in ports {
        let router = build_router(state.clone());
        let addr_v4_loopback: SocketAddr = ([127, 0, 0, 1], port).into();

        // Retry briefly on AddrInUse so tray restarts / installer upgrades cleanly acquire ports
        // as soon as the exiting instance releases its sockets (prevents `active on ports: []`).
        let max_attempts = if bound_ports.is_empty() { 15 } else { 5 };
        let mut listener = None;
        for attempt in 1..=max_attempts {
            match tokio::net::TcpListener::bind(addr_v4_loopback).await {
                Ok(l) => {
                    info!(
                        "[Server] Axum server listening on explicit loopback 127.0.0.1:{} (Firewall-safe)",
                        port
                    );
                    listener = Some(l);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && attempt < max_attempts => {
                    if attempt == 1 {
                        warn!(
                            "[Server] Port 127.0.0.1:{} busy (likely restarting), retrying...",
                            port
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(e) => {
                    warn!(
                        "[Server] Failed to bind to 127.0.0.1:{}: {}",
                        port, e
                    );
                    break;
                }
            }
        }

        if let Some(tcp_listener) = listener {
            bound_ports.push(port);
            if bound_ports.len() == 1 {
                *state.torrent_mgr.primary_port.write() = port;
                info!("[Server] Primary engine port is now: {}", port);
            }

            let mut shutdown = state.shutdown.subscribe();
            tokio::spawn(async move {
                let server = axum::serve(
                    tcp_listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(async move {
                    let _ = shutdown.changed().await;
                });
                if let Err(err) = server.await {
                    error!("[Server] Error on port {}: {}", port, err);
                }
            });
        }
    }

    #[cfg(feature = "lan-server")]
    if state.torrent_mgr.config.lan_tls_port > 0 {
        spawn_lan_tls_server(state.clone()).await;
    }

    // Background startup tasks (restore downloads + hourly 72h cache cleanup)
    let state_bg = state.clone();
    tokio::spawn(async move {
        let mut shutdown = state_bg.shutdown.subscribe();
        tokio::time::sleep(Duration::from_secs(3)).await;
        if *shutdown.borrow() {
            return;
        }
        state_bg.downloads_mgr.restore_downloads();

        // Run cache cleanup on startup and then every hour
        loop {
            if *shutdown.borrow() {
                break;
            }
            let expired = state_bg.cache_mgr.collect_expired_hashes();
            for hash in expired {
                state_bg.torrent_mgr.destroy_engine(&hash);
            }
            let active_hashes: HashSet<String> = state_bg
                .torrent_mgr
                .active_engines
                .read()
                .keys()
                .cloned()
                .collect();
            state_bg.cache_mgr.clean_orphaned_files(&active_hashes);

            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
                _ = shutdown.changed() => break,
            }
        }
    });

    Ok(bound_ports)
}

#[cfg(feature = "lan-server")]
pub async fn spawn_lan_tls_server(state: AppState) -> Option<u16> {
    let preferred_port = state.torrent_mgr.config.lan_tls_port;
    let candidate_ports = [preferred_port, preferred_port.saturating_add(2)];

    let tls_acceptor = match state.lan_mgr.build_tls_acceptor() {
        Ok(acc) => acc,
        Err(e) => {
            error!("[LAN TLS] Failed to initialize TLS 1.3 acceptor: {}", e);
            return None;
        }
    };

    let mut bound_listener = None;
    let mut actual_port = 0u16;
    for port in candidate_ports {
        if port == 0 {
            continue;
        }
        let bind_addr = SocketAddr::from(([0, 0, 0, 0], port));
        match tokio::net::TcpListener::bind(bind_addr).await {
            Ok(l) => {
                actual_port = l.local_addr().map(|a| a.port()).unwrap_or(port);
                info!(
                    "[LAN TLS] Secure TLS 1.3 server listening on 0.0.0.0:{}",
                    actual_port
                );
                bound_listener = Some(l);
                break;
            }
            Err(e) => {
                warn!("[LAN TLS] Failed to bind 0.0.0.0:{}: {}", port, e);
            }
        }
    }

    let listener = bound_listener?;
    *state.lan_mgr.bound_https_port.write() = actual_port;

    // Launch UDP discovery responder
    if state.torrent_mgr.config.lan_discovery_port > 0 {
        let lan_mgr_udp = Arc::clone(&state.lan_mgr);
        let shutdown_udp = state.shutdown.subscribe();
        tokio::spawn(async move {
            crate::lan::spawn_lan_udp_discovery(lan_mgr_udp, shutdown_udp).await;
        });
    }

    let lan_router = build_lan_router(state.clone());
    let lan_mgr = Arc::clone(&state.lan_mgr);
    let mut shutdown = state.shutdown.subscribe();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                accept_res = listener.accept() => {
                    let Ok((tcp_stream, remote_addr)) = accept_res else { continue };
                    if !lan_mgr.is_enabled() || !lan_mgr.is_allowed_lan_peer(remote_addr.ip()) {
                        drop(tcp_stream);
                        continue;
                    }
                    let _ = tcp_stream.set_nodelay(true);
                    let acceptor = tls_acceptor.clone();
                    let router_for_conn = lan_router
                        .clone()
                        .layer(axum::Extension(ConnectInfo(remote_addr)));

                    tokio::spawn(async move {
                        let Ok(Ok(tls_stream)) = tokio::time::timeout(
                            Duration::from_secs(5),
                            acceptor.accept(tcp_stream),
                        )
                        .await
                        else {
                            return;
                        };

                        let io = hyper_util::rt::TokioIo::new(tls_stream);
                        let hyper_service = hyper_util::service::TowerToHyperService::new(router_for_conn);
                        let _ = hyper_util::server::conn::auto::Builder::new(
                            hyper_util::rt::TokioExecutor::new(),
                        )
                        .serve_connection(io, hyper_service)
                        .await;
                    });
                }
            }
        }
    });

    Some(actual_port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EngineConfig;
    use crate::DeccoEngine;

    #[test]
    fn parse_range_header_handles_standard_and_edge_cases() {
        assert_eq!(parse_range_header("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range_header("bytes=100-", 1000), Some((100, 999)));
        assert_eq!(parse_range_header("bytes=-200", 1000), Some((800, 999)));
        assert_eq!(parse_range_header("bytes=-2000", 1000), Some((0, 999)));
        assert_eq!(parse_range_header("bytes=0-5000", 1000), Some((0, 999)));
        assert_eq!(parse_range_header("bytes=1000-", 1000), None);
        assert_eq!(parse_range_header("bytes=500-100", 1000), None);
        assert_eq!(parse_range_header("bytes=0-1,2-3", 1000), None);
        assert_eq!(parse_range_header("bytes=0-0", 0), None);
    }

    async fn spawn_test_server(auth_token: Option<String>) -> (u16, DeccoEngine, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "decco_test_{}_{}",
            std::process::id(),
            crate::config::now_ms()
        ));
        let cfg = EngineConfig::new(dir.clone(), false);
        let engine = match auth_token {
            Some(t) => DeccoEngine::new_with_auth(cfg, t),
            None => DeccoEngine::new(cfg),
        };
        let router = build_router(engine.state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        *engine.state.torrent_mgr.primary_port.write() = port;

        let mut shutdown = engine.state.shutdown.subscribe();
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = shutdown.changed().await;
            })
            .await;
        });

        (port, engine, dir)
    }

    #[tokio::test]
    async fn http_router_endpoints_and_cors_pna() {
        let (port, engine, dir) = spawn_test_server(None).await;
        let base = format!("http://127.0.0.1:{}", port);
        let client = reqwest::Client::new();

        // OPTIONS preflight with PNA
        let opt = client
            .request(reqwest::Method::OPTIONS, format!("{}/ping", base))
            .header("Access-Control-Request-Private-Network", "true")
            .send()
            .await
            .unwrap();
        assert_eq!(opt.status(), 200);
        assert_eq!(
            opt.headers()
                .get("access-control-allow-private-network")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );

        // GET /ping & /status/check
        let ping: serde_json::Value = client
            .get(format!("{}/ping", base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(ping["pong"], true);
        assert_eq!(ping["port"], port);

        let sc: serde_json::Value = client
            .get(format!("{}/status/check", base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(sc["status"], "ok");

        // POST /download/settings/set with JSON body
        let set_resp: serde_json::Value = client
            .post(format!("{}/download/settings/set", base))
            .json(&json!({ "maxConcurrent": 4, "autoPauseOnPlayback": false }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(set_resp["settings"]["maxConcurrent"], 4);
        assert_eq!(set_resp["settings"]["autoPauseOnPlayback"], false);

        // GET /live/proxy validation
        let bad_proto = client
            .get(format!("{}/live/proxy?url=file:///etc/passwd", base))
            .send()
            .await
            .unwrap();
        assert_eq!(bad_proto.status(), 400);

        engine.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn http_router_enforces_bearer_auth_when_configured() {
        let (port, engine, dir) = spawn_test_server(Some("secret-token".to_string())).await;
        let base = format!("http://127.0.0.1:{}", port);
        let client = reqwest::Client::new();

        let unauth = client
            .get(format!("{}/ping", base))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), 401);

        let auth = client
            .get(format!("{}/ping", base))
            .bearer_auth("secret-token")
            .send()
            .await
            .unwrap();
        assert_eq!(auth.status(), 200);

        engine.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(feature = "lan-server")]
    #[tokio::test]
    async fn lan_router_enforces_tofu_bearer_browser_and_host_security() {
        let dir = std::env::temp_dir().join(format!(
            "decco_lan_test_{}_{}",
            std::process::id(),
            crate::config::now_ms()
        ));
        let mut cfg = EngineConfig::new(dir.clone(), false);
        cfg.lan_tls_port = 0;
        cfg.lan_discovery_port = 0;
        let engine = DeccoEngine::new(cfg);
        engine.state.lan_mgr.set_enabled(true);
        *engine.state.lan_mgr.allow_loopback_peers_for_test.write() = true;

        let lan_router = build_lan_router(engine.state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        *engine.state.lan_mgr.bound_https_port.write() = port;

        let mut shutdown = engine.state.shutdown.subscribe();
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                lan_router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = shutdown.changed().await;
            })
            .await;
        });

        let base = format!("http://127.0.0.1:{}", port);
        let client = reqwest::Client::new();

        // 1. Unauthenticated /download/list MUST return 401
        let unauth_dl = client
            .get(format!("{}/download/list", base))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth_dl.status(), 401);

        // 2. Browser Origin header MUST be rejected (403) even on public /lan/hello
        let browser_req = client
            .get(format!("{}/lan/hello", base))
            .header("Origin", "https://evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(browser_req.status(), 403);

        // 3. DNS Rebinding Host header MUST be rejected (403)
        let rebind_req = client
            .get(format!("{}/lan/hello", base))
            .header("Host", "evil.example:18890")
            .send()
            .await
            .unwrap();
        assert_eq!(rebind_req.status(), 403);

        // 4. Desktop-only /download/open and /live/proxy MUST NOT be exposed (404/401)
        // First pair a legitimate first TV device (TOFU)
        let pair_resp = client
            .post(format!("{}/lan/pair", base))
            .json(&json!({
                "deviceId": "tv_living_room_001",
                "deviceName": "Sony Bravia TV"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(pair_resp.status(), 200);
        let pair_json: serde_json::Value = pair_resp.json().await.unwrap();
        let tv_token = pair_json["token"].as_str().unwrap().to_string();
        assert!(tv_token.starts_with("dlan_"));

        // 5. Second unknown unauthenticated device MUST be blocked (403) once first TV paired
        let rogue_pair = client
            .post(format!("{}/lan/pair", base))
            .json(&json!({
                "deviceId": "rogue_neighbor_999",
                "deviceName": "Rogue Device"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(rogue_pair.status(), 403);

        // 6. Paired TV can access /download/list with its Bearer token
        let authed_dl = client
            .get(format!("{}/download/list", base))
            .bearer_auth(&tv_token)
            .send()
            .await
            .unwrap();
        assert_eq!(authed_dl.status(), 200);

        // 7. Sensitive desktop routes (/download/open/abc, /live/proxy) are NOT mounted (404)
        let open_attempt = client
            .get(format!("{}/download/open/abc", base))
            .bearer_auth(&tv_token)
            .send()
            .await
            .unwrap();
        assert_eq!(open_attempt.status(), 404);

        let live_attempt = client
            .get(format!("{}/live/proxy?url=http://example.com", base))
            .bearer_auth(&tv_token)
            .send()
            .await
            .unwrap();
        assert_eq!(live_attempt.status(), 404);

        // 8. Disabling LAN sharing immediately blocks even already-paired TV (403)
        engine.state.lan_mgr.set_enabled(false);
        let disabled_req = client
            .get(format!("{}/download/list", base))
            .bearer_auth(&tv_token)
            .send()
            .await
            .unwrap();
        assert_eq!(disabled_req.status(), 403);

        engine.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }
}

