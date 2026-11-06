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
    routing::{any, get},
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
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as _;
use tokio_util::io::ReaderStream;
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct AppState {
    pub torrent_mgr: Arc<TorrentEngineManager>,
    pub downloads_mgr: Arc<DownloadsManager>,
    pub cache_mgr: Arc<CacheManager>,
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
pub struct StartDownloadQuery {
    pub hash: Option<String>,
    pub title: Option<String>,
    pub imdb_id: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub file_idx: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SettingsQuery {
    pub max_concurrent: Option<usize>,
    pub auto_pause_on_playback: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct MeteredQuery {
    pub value: Option<bool>,
}

/// Middleware for CORS, Private Network Access (PNA), and Loopback IP security
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
            "Content-Type, Range, Authorization, X-Requested-With"
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
        "Content-Type, Range, Authorization, X-Requested-With"
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

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/ping", get(handle_ping))
        .route("/status/check", get(handle_status_check))
        .route("/prewarm/:hash", get(handle_prewarm))
        .route("/start/:hash", get(handle_start))
        .route("/pause/:hash", get(handle_pause))
        .route("/stop/:hash", get(handle_stop).post(handle_stop))
        .route("/status/:hash", get(handle_status))
        .route("/proxy/:hash", get(handle_proxy))
        .route(
            "/cache/clear",
            get(handle_cache_clear)
                .post(handle_cache_clear)
                .delete(handle_cache_clear),
        )
        .route("/cache", get(handle_cache_clear).delete(handle_cache_clear))
        .route("/network/metered", get(handle_network_metered))
        .route("/hls/:hash/:file", get(handle_hls_route))
        .route("/subtitles/:hash", get(handle_list_subtitles))
        .route(
            "/subtitles/:hash/extract/:index",
            get(handle_extract_subtitle),
        )
        .route("/live/proxy", get(handle_live_proxy_route))
        .route("/download/start", get(handle_download_start))
        .route("/download/list", get(handle_download_list))
        .route("/download/events", get(handle_download_events))
        .route("/download/pause/:id", get(handle_download_pause))
        .route("/download/resume/:id", get(handle_download_resume))
        .route("/download/delete/:id", get(handle_download_delete))
        .route("/download/open/:id", get(handle_download_open))
        .route("/download/force-start/:id", get(handle_download_force_start))
        .route("/download/pause-all", get(handle_download_pause_all))
        .route("/download/resume-all", get(handle_download_resume_all))
        .route(
            "/download/clear-completed",
            get(handle_download_clear_completed),
        )
        .route("/download/settings", get(handle_download_settings))
        .route("/download/settings/set", any(handle_download_settings_set))
        .route(
            "/download/subtitle/:hash/:lang",
            get(handle_download_subtitle),
        )
        .layer(middleware::from_fn(cors_and_loopback_middleware))
        .with_state(state)
}

async fn handle_ping(State(state): State<AppState>) -> impl IntoResponse {
    let port = *state.torrent_mgr.primary_port.read();
    Json(json!({
        "status": "ok",
        "port": port,
        "version": "2.2",
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
) -> impl IntoResponse {
    let clean_hash = hash.trim().to_ascii_lowercase();
    if clean_hash.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Missing hash" })),
        );
    }
    state.torrent_mgr.cancel_stop_grace_timeout(&clean_hash);

    let cache_dir = state.torrent_mgr.config.streaming_cache_dir.clone();
    let hash_bg = clean_hash.clone();
    tokio::spawn(async move {
        let _ = prefetch_torrent_cache_file(&hash_bg, &cache_dir).await;
    });

    if !state
        .torrent_mgr
        .active_engines
        .read()
        .contains_key(&clean_hash)
    {
        let mgr = Arc::clone(&state.torrent_mgr);
        let h = clean_hash.clone();
        tokio::spawn(async move {
            mgr.start_or_get_engine(&h, None, None, None, None).await;
        });
    }

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

async fn handle_cache_clear(State(state): State<AppState>) -> impl IntoResponse {
    state.torrent_mgr.active_engines.write().clear();
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

/// Serves a local file with full HTTP 206 Partial Content / Range support
async fn serve_file_with_range(
    file_path: &Path,
     declared_size: Option<u64>,
    headers: &HeaderMap,
) -> Response {
    let mut file = match File::open(file_path).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::NOT_FOUND, "File not found").into_response(),
    };

    let disk_size = match file.metadata().await {
        Ok(m) => m.len(),
        Err(_) => return (StatusCode::NOT_FOUND, "Metadata unavailable").into_response(),
    };
    let file_size = declared_size.unwrap_or(disk_size).max(disk_size);
    if file_size == 0 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "File warming up (0 bytes)",
        )
            .into_response();
    }

    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("video.mp4");
    let content_type = get_video_content_type(file_name);

    let range_header = headers.get("range").and_then(|v| v.to_str().ok());

    if let Some(range_str) = range_header {
        let Some((start, end)) = parse_range_header(range_str, file_size) else {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("Content-Range", format!("bytes */{}", file_size))
                .body(Body::empty())
                .unwrap();
        };

        if file.seek(SeekFrom::Start(start)).await.is_err() {
            return (StatusCode::INTERNAL_SERVER_ERROR, "Seek error").into_response();
        }

        let chunk_size = (end - start) + 1;
        let limited = file.take(chunk_size);
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
        let stream = ReaderStream::with_capacity(file, 64 * 1024);
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Length", file_size.to_string())
            .header("Accept-Ranges", "bytes")
            .header("Content-Type", content_type)
            .body(Body::from_stream(stream))
            .unwrap()
    }
}

async fn handle_proxy(
    State(state): State<AppState>,
    AxumPath(hash): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let clean_hash = hash.trim().to_ascii_lowercase();
    state.torrent_mgr.cancel_stop_grace_timeout(&clean_hash);

    // 1. Check if completed download exists on disk
    if let Some((_, local_path)) = state.downloads_mgr.find_completed_video_path(&clean_hash) {
        return serve_file_with_range(&local_path, None, &headers).await;
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

    let start_byte = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(|r| parse_range_header(r, vf.length))
        .map(|(s, _)| s)
        .unwrap_or(0);

    state
        .torrent_mgr
        .on_proxy_range_request(&clean_hash, start_byte);

    serve_file_with_range(&vf.path, Some(vf.length), &headers).await
}

async fn handle_hls_route(
    State(state): State<AppState>,
    AxumPath((hash, file)): AxumPath<(String, String)>,
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

        let manifest = build_hls_vod_manifest(eng.duration);
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

        match spawn_hls_segment_transcoder(&proxy_url, seg_index, video_codec.as_deref()).await {
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
    let cached_subs = {
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
        eng.embedded_subtitles.clone()
    };

    if let Some(subs) = cached_subs {
        return Json(json!({ "subtitles": subs })).into_response();
    }

    let port = *state.torrent_mgr.primary_port.read();
    let proxy_url = format!("http://127.0.0.1:{}/proxy/{}", port, clean_hash);

    match probe_media_url(&proxy_url).await {
        Ok(probed) => {
            let subs = probed.embedded_subtitles.clone();
            {
                let mut guard = state.torrent_mgr.active_engines.write();
                if let Some(eng) = guard.get_mut(&clean_hash) {
                    if probed.duration > 0.0 {
                        eng.duration = probed.duration;
                    }
                    eng.video_codec = probed.video_codec;
                    eng.audio_codec = probed.audio_codec;
                    eng.embedded_subtitles = Some(probed.embedded_subtitles);
                }
            }
            Json(json!({ "subtitles": subs })).into_response()
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
    handle_live_proxy(query, headers, port).await
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
    let season = query.season.unwrap_or(0);
    let episode = query.episode.unwrap_or(0);

    let meta = state.downloads_mgr.start_download(
        hash,
        &title,
        &imdb_id,
        season,
        episode,
        query.file_idx,
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
            let _ = std::process::Command::new("explorer.exe")
                .arg(format!("/select,{}", target_to_open.display()))
                .spawn();
        } else {
            let _ = std::process::Command::new("explorer.exe")
                .arg(target_to_open.as_os_str())
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
) -> impl IntoResponse {
    let auto_pause = query.auto_pause_on_playback.as_deref().map(|v| {
        v.eq_ignore_ascii_case("true") || v == "1"
    });
    let updated = state
        .downloads_mgr
        .update_settings(query.max_concurrent, auto_pause);
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
/// binding primarily to `127.0.0.1` (Firewall-safe) with fallback to `[::]` and `0.0.0.0`.
/// Also launches the 72h cache cleanup loop and restores active downloads.
pub async fn start_multiport_server(state: AppState) -> anyhow::Result<Vec<u16>> {
    let ports = state.torrent_mgr.config.ports.clone();
    let mut bound_ports = Vec::new();

    for port in ports {
        let router = build_router(state.clone());
        let addr_v4_loopback: SocketAddr = ([127, 0, 0, 1], port).into();

        let listener = match tokio::net::TcpListener::bind(addr_v4_loopback).await {
            Ok(l) => {
                info!(
                    "[Server] Axum server listening on explicit loopback 127.0.0.1:{} (Firewall-safe)",
                    port
                );
                Some(l)
            }
            Err(e) => {
                warn!(
                    "[Server] Failed to bind to 127.0.0.1:{}: {}. Trying fallback...",
                    port, e
                );
                let addr_v4_any: SocketAddr = ([0, 0, 0, 0], port).into();
                tokio::net::TcpListener::bind(addr_v4_any).await.ok()
            }
        };

        if let Some(tcp_listener) = listener {
            bound_ports.push(port);
            if bound_ports.len() == 1 {
                *state.torrent_mgr.primary_port.write() = port;
                info!("[Server] Primary engine port is now: {}", port);
            }

            tokio::spawn(async move {
                if let Err(err) = axum::serve(
                    tcp_listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                {
                    error!("[Server] Error on port {}: {}", port, err);
                }
            });
        }
    }

    // Background startup tasks (restore downloads + hourly 72h cache cleanup)
    let state_bg = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        state_bg.downloads_mgr.restore_downloads();

        // Run cache cleanup on startup and then every hour
        loop {
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

            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });

    Ok(bound_ports)
}

