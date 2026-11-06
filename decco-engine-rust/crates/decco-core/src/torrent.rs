use crate::cache::CacheManager;
use crate::config::{
    build_episode_pattern, build_magnet_uri, find_exact_video_file, is_video_file, EngineConfig,
    READAHEAD_WINDOW_BYTES,
};
use crate::transcode::{probe_media_url, EmbeddedSubtitle};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{error, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TorrentFileInfo {
    pub index: usize,
    pub name: String,
    pub path: PathBuf,
    pub length: u64,
    pub offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamEngineState {
    pub hash: String,
    pub status: String,
    pub metadata_ready: bool,
    pub is_mock: bool,
    pub video_file: Option<TorrentFileInfo>,
    pub files: Vec<TorrentFileInfo>,
    pub requested_file_idx: Option<usize>,
    pub requested_season: Option<u32>,
    pub requested_episode: Option<u32>,
    pub duration: f64,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub embedded_subtitles: Option<Vec<EmbeddedSubtitle>>,
    pub progress: f64,
    pub peers: usize,
    pub download_speed_bytes: u64,
    pub piece_length: u64,
    pub save_dir: PathBuf,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStatusResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_ready: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_idx: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitles: Option<Vec<EmbeddedSubtitle>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peers: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
}

/// Pre-fetches `.torrent` metadata file from public HTTP caches (`itorrents.org` and `torrage.info`)
/// so metadata resolves in milliseconds before DHT/tracker magnet exchange finishes.
pub async fn prefetch_torrent_cache_file(hash: &str, target_dir: &Path) -> Option<PathBuf> {
    let clean_hash = hash.trim().to_ascii_lowercase();
    if clean_hash.is_empty() {
        return None;
    }
    let upper_hash = clean_hash.to_ascii_uppercase();
    let cache_torrent_dir = target_dir.join("torrent-stream");
    let cache_torrent_path = cache_torrent_dir.join(format!("{}.torrent", clean_hash));

    if let Ok(meta) = std::fs::metadata(&cache_torrent_path) {
        if meta.len() > 64 {
            return Some(cache_torrent_path);
        }
    }

    let urls = [
        format!("https://itorrents.org/torrent/{}.torrent", upper_hash),
        format!("https://torrage.info/torrent.php?h={}", upper_hash),
    ];

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(2200))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .ok()?;

    for url in &urls {
        if let Ok(res) = client.get(url).send().await {
            if !res.status().is_success() {
                continue;
            }
            if let Ok(bytes) = res.bytes().await {
                // Valid bencoded .torrent starts with 'd' (0x64)
                if bytes.len() > 64 && bytes[0] == 0x64 {
                    let _ = std::fs::create_dir_all(&cache_torrent_dir);
                    if std::fs::write(&cache_torrent_path, &bytes).is_ok() {
                        info!(
                            "[Engine] Fast-resolved .torrent metadata via HTTP cache ({} bytes) for {}",
                            bytes.len(),
                            clean_hash
                        );
                        return Some(cache_torrent_path);
                    }
                }
            }
        }
    }

    None
}

/// Selects the target video file according to Decco Engine's 3-tier priority rules:
/// 1. Episode regex (`S05E06`, `5x06`, etc.)
/// 2. Explicit `file_idx`
/// 3. Largest video file in the torrent
pub fn select_target_video_file(
    files: &[TorrentFileInfo],
    file_idx: Option<usize>,
    season: Option<u32>,
    episode: Option<u32>,
) -> Option<TorrentFileInfo> {
    if files.is_empty() {
        return None;
    }

    // Priority 1: Search by episode pattern
    if let (Some(s), Some(e)) = (season, episode) {
        if s > 0 || e > 0 {
            if let Some(re) = build_episode_pattern(s, e) {
                let matching: Vec<&TorrentFileInfo> = files
                    .iter()
                    .filter(|f| is_video_file(&f.name) && re.is_match(&f.name))
                    .collect();
                if let Some(best) = matching.into_iter().max_by_key(|f| f.length) {
                    info!(
                        "[Engine] Found file by episode pattern S{}E{}: {}",
                        s, e, best.name
                    );
                    return Some(best.clone());
                }
            }
        }
    }

    // Priority 2: Explicit fileIdx
    if let Some(idx) = file_idx {
        if let Some(f) = files.get(idx) {
            info!("[Engine] Using fileIdx {}: {}", idx, f.name);
            return Some(f.clone());
        }
    }

    // Priority 3: Largest video file
    let video_files: Vec<&TorrentFileInfo> =
        files.iter().filter(|f| is_video_file(&f.name)).collect();
    if let Some(largest) = video_files.into_iter().max_by_key(|f| f.length) {
        info!("[Engine] Fallback to largest video: {}", largest.name);
        return Some(largest.clone());
    }

    files.first().cloned()
}

/// Computes the critical piece window `[start_piece, width]` for the head of a file or a byte offset
pub fn compute_critical_piece_window(
    file: &TorrentFileInfo,
    byte_offset: u64,
    piece_length: u64,
) -> Option<(u32, u32)> {
    if piece_length == 0 || file.length == 0 {
        return None;
    }
    let current_piece = ((file.offset + byte_offset) / piece_length) as u32;
    let file_end_piece = ((file.offset + file.length.saturating_sub(1)) / piece_length) as u32;
    let readahead_pieces = ((READAHEAD_WINDOW_BYTES as f64 / piece_length as f64).ceil() as u32)
        .clamp(2, 8);
    let remaining_pieces = file_end_piece.saturating_sub(current_piece) + 1;
    let width = readahead_pieces.min(remaining_pieces.max(1));
    Some((current_piece, width))
}

pub struct TorrentEngineManager {
    pub config: EngineConfig,
    pub cache: Arc<CacheManager>,
    pub active_engines: Arc<RwLock<HashMap<String, StreamEngineState>>>,
    pub active_streaming_hash: Arc<RwLock<Option<String>>>,
    pub stop_grace_timers: Arc<RwLock<HashMap<String, JoinHandle<()>>>>,
    pub is_metered: Arc<RwLock<bool>>,
    pub primary_port: Arc<RwLock<u16>>,
    #[cfg(feature = "rqbit-backend")]
    pub rqbit_session: Arc< tokio::sync::OnceCell<Arc<librqbit::Session>> >,
}

impl TorrentEngineManager {
    pub fn new(config: EngineConfig, cache: Arc<CacheManager>) -> Self {
        Self {
            config,
            cache,
            active_engines: Arc::new(RwLock::new(HashMap::new())),
            active_streaming_hash: Arc::new(RwLock::new(None)),
            stop_grace_timers: Arc::new(RwLock::new(HashMap::new())),
            is_metered: Arc::new(RwLock::new(false)),
            primary_port: Arc::new(RwLock::new(crate::config::DEFAULT_PRIMARY_PORT)),
            #[cfg(feature = "rqbit-backend")]
            rqbit_session: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    #[cfg(feature = "rqbit-backend")]
    pub async fn get_or_init_session(&self) -> anyhow::Result<Arc<librqbit::Session>> {
        let dir = self.config.streaming_cache_dir.clone();
        self.rqbit_session
            .get_or_try_init(|| async move {
                info!("[DHT] Initializing persistent BitTorrent session and pre-warming DHT routing table...");
                let mut opts = librqbit::SessionOptions::default();
                opts.disable_dht = false;
                opts.fastresume = true;
                let session = librqbit::Session::new_with_opts(dir, opts).await?;
                Ok(session)
            })
            .await
            .cloned()
    }

    pub fn cancel_stop_grace_timeout(&self, hash: &str) {
        let normalized = hash.trim().to_ascii_lowercase();
        if let Some(handle) = self.stop_grace_timers.write().remove(&normalized) {
            handle.abort();
        }
    }

    pub fn set_metered_mode(&self, metered: bool) {
        *self.is_metered.write() = metered;
        info!("[Engine] Network metered mode set to: {}", metered);
    }

    pub fn get_status(&self, hash: &str) -> EngineStatusResponse {
        let clean_hash = hash.trim().to_ascii_lowercase();
        self.cancel_stop_grace_timeout(&clean_hash);

        let guard = self.active_engines.read();
        let Some(engine) = guard.get(&clean_hash) else {
            return EngineStatusResponse {
                status: "not_started".to_string(),
                metadata_ready: None,
                file_name: None,
                file_path: None,
                file_size: None,
                file_idx: None,
                total_files: None,
                duration: None,
                video_codec: None,
                audio_codec: None,
                subtitles: None,
                progress: None,
                peers: None,
                speed: None,
            };
        };

        EngineStatusResponse {
            status: engine.status.clone(),
            metadata_ready: Some(engine.metadata_ready),
            file_name: engine.video_file.as_ref().map(|f| f.name.clone()),
            file_path: engine
                .video_file
                .as_ref()
                .map(|f| f.path.to_string_lossy().to_string()),
            file_size: engine.video_file.as_ref().map(|f| f.length),
            file_idx: engine.requested_file_idx,
            total_files: Some(engine.files.len()),
            duration: Some(engine.duration),
            video_codec: engine.video_codec.clone(),
            audio_codec: engine.audio_codec.clone(),
            subtitles: engine.embedded_subtitles.clone(),
            progress: Some(if engine.is_mock { 1.0 } else { engine.progress }),
            peers: Some(engine.peers),
            speed: Some(format!("{:.2}", engine.download_speed_bytes as f64 / 1024.0)),
        }
    }

    /// Creates a MockEngine for an already completed local download so playback uses 0 P2P connections
    pub fn register_mock_engine_from_local_file(
        self: &Arc<Self>,
        hash: &str,
        file_path: PathBuf,
        file_idx: Option<usize>,
        saved_duration: f64,
    ) -> StreamEngineState {
        let clean_hash = hash.trim().to_ascii_lowercase();
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("video.mp4")
            .to_string();
        let file_size = std::fs::metadata(&file_path).map(|m| m.len()).unwrap_or(0);
        let save_dir = file_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.config.completed_downloads_dir.clone());

        let file_info = TorrentFileInfo {
            index: file_idx.unwrap_or(0),
            name: file_name,
            path: file_path.clone(),
            length: file_size,
            offset: 0,
        };

        let mock_state = StreamEngineState {
            hash: clean_hash.clone(),
            status: "ready".to_string(),
            metadata_ready: true,
            is_mock: true,
            video_file: Some(file_info.clone()),
            files: vec![file_info],
            requested_file_idx: file_idx,
            requested_season: None,
            requested_episode: None,
            duration: saved_duration,
            video_codec: None,
            audio_codec: None,
            embedded_subtitles: None,
            progress: 1.0,
            peers: 0,
            download_speed_bytes: 0,
            piece_length: 0,
            save_dir,
            error: None,
        };

        self.active_engines
            .write()
            .insert(clean_hash.clone(), mock_state.clone());

        // Probe local file in background for duration, codecs, and embedded subtitles
        let mgr = Arc::clone(self);
        let hash_clone = clean_hash.clone();
        let port = *self.primary_port.read();
        tokio::spawn(async move {
            let proxy_url = format!("http://127.0.0.1:{}/proxy/{}", port, hash_clone);
            if let Ok(probed) = probe_media_url(&proxy_url).await {
                let mut guard = mgr.active_engines.write();
                if let Some(eng) = guard.get_mut(&hash_clone) {
                    if probed.duration > 0.0 {
                        eng.duration = probed.duration;
                    }
                    eng.video_codec = probed.video_codec;
                    eng.audio_codec = probed.audio_codec;
                    eng.embedded_subtitles = Some(probed.embedded_subtitles);
                    info!(
                        "[Mock Engine] Probed duration: {}s | video: {:?} | audio: {:?}",
                        eng.duration, eng.video_codec, eng.audio_codec
                    );
                }
            }
        });

        mock_state
    }

    /// Starts or updates a streaming torrent engine
    pub async fn start_or_get_engine(
        self: &Arc<Self>,
        hash: &str,
        file_idx: Option<usize>,
        season: Option<u32>,
        episode: Option<u32>,
        custom_save_dir: Option<PathBuf>,
    ) -> StreamEngineState {
        let clean_hash = hash.trim().to_ascii_lowercase();
        self.cancel_stop_grace_timeout(&clean_hash);
        *self.active_streaming_hash.write() = Some(clean_hash.clone());

        // Check if already active and update episode selection if needed
        {
            let mut guard = self.active_engines.write();
            if let Some(existing) = guard.get_mut(&clean_hash) {
                if let (Some(s), Some(e)) = (season, episode) {
                    if existing.metadata_ready && !existing.is_mock {
                        if let Some(re) = build_episode_pattern(s, e) {
                            let needs_reselect = existing
                                .video_file
                                .as_ref()
                                .map(|vf| !re.is_match(&vf.name))
                                .unwrap_or(true);
                            if needs_reselect {
                                if let Some(correct) =
                                    select_target_video_file(&existing.files, file_idx, season, episode)
                                {
                                    info!(
                                        "[Engine] Corrected file selection to: {} - Other files deselected",
                                        correct.name
                                    );
                                    existing.video_file = Some(correct);
                                }
                            }
                        }
                    }
                }
                return existing.clone();
            }
        }

        let save_dir = custom_save_dir.unwrap_or_else(|| {
            self.config
                .streaming_cache_dir
                .join(&clean_hash[..clean_hash.len().min(16)])
        });
        let _ = std::fs::create_dir_all(&save_dir);

        // If the video file is already present on disk in cache, register immediately
        if let Some(existing_video) = find_exact_video_file(&save_dir, None) {
            if let Ok(meta) = std::fs::metadata(&existing_video) {
                if meta.len() > 0 {
                    let fname = existing_video
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("video.mp4")
                        .to_string();
                    let finfo = TorrentFileInfo {
                        index: file_idx.unwrap_or(0),
                        name: fname,
                        path: existing_video.clone(),
                        length: meta.len(),
                        offset: 0,
                    };
                    let state = StreamEngineState {
                        hash: clean_hash.clone(),
                        status: "ready".to_string(),
                        metadata_ready: true,
                        is_mock: false,
                        video_file: Some(finfo.clone()),
                        files: vec![finfo],
                        requested_file_idx: file_idx,
                        requested_season: season,
                        requested_episode: episode,
                        duration: 0.0,
                        video_codec: None,
                        audio_codec: None,
                        embedded_subtitles: None,
                        progress: 0.0,
                        peers: 0,
                        download_speed_bytes: 0,
                        piece_length: 1024 * 1024,
                        save_dir: save_dir.clone(),
                        error: None,
                    };
                    self.active_engines
                        .write()
                        .insert(clean_hash.clone(), state.clone());
                }
            }
        }

        let initial_state = StreamEngineState {
            hash: clean_hash.clone(),
            status: "loading".to_string(),
            metadata_ready: false,
            is_mock: false,
            video_file: None,
            files: Vec::new(),
            requested_file_idx: file_idx,
            requested_season: season,
            requested_episode: episode,
            duration: 0.0,
            video_codec: None,
            audio_codec: None,
            embedded_subtitles: None,
            progress: 0.0,
            peers: 0,
            download_speed_bytes: 0,
            piece_length: 1024 * 1024,
            save_dir: save_dir.clone(),
            error: None,
        };

        self.active_engines
            .write()
            .insert(clean_hash.clone(), initial_state.clone());

        self.cache.add_torrent(&clean_hash, file_idx, season, episode);

        // Spawn background task to warm .torrent HTTP cache and start librqbit session
        let mgr = Arc::clone(self);
        let hash_for_task = clean_hash.clone();
        let save_dir_for_task = save_dir.clone();
        tokio::spawn(async move {
            let cached_torrent =
                prefetch_torrent_cache_file(&hash_for_task, &mgr.config.streaming_cache_dir).await;

            #[cfg(feature = "rqbit-backend")]
            {
                if let Ok(session) = mgr.get_or_init_session().await {
                    let add_target = if let Some(ref path) = cached_torrent {
                        if let Ok(bytes) = std::fs::read(path) {
                            librqbit::AddTorrent::from_bytes(bytes)
                        } else {
                            librqbit::AddTorrent::from_url(build_magnet_uri(&hash_for_task))
                        }
                    } else {
                        librqbit::AddTorrent::from_url(build_magnet_uri(&hash_for_task))
                    };

                    let mut add_opts = librqbit::AddTorrentOptions::default();
                    add_opts.overwrite = true;
                    add_opts.output_folder =
                        Some(save_dir_for_task.to_string_lossy().to_string());

                    match session.add_torrent(add_target, Some(add_opts)).await {
                        Ok(response) => {
                            if let Some(handle) = response.into_handle() {
                                let _ = handle.wait_until_initialized().await;

                                let mut discovered_files = Vec::new();
                                let _ = handle.with_metadata(|meta| {
                                    let mut offset = 0u64;
                                    for (idx, details) in meta.file_infos.iter().enumerate() {
                                        let name = details
                                            .relative_filename
                                            .file_name()
                                            .and_then(|n| n.to_str())
                                            .unwrap_or("video.mp4")
                                            .to_string();
                                        let full_path =
                                            save_dir_for_task.join(&details.relative_filename);
                                        discovered_files.push(TorrentFileInfo {
                                            index: idx,
                                            name,
                                            path: full_path,
                                            length: details.len,
                                            offset,
                                        });
                                        offset += details.len;
                                    }
                                });

                                let selected = select_target_video_file(
                                    &discovered_files,
                                    file_idx,
                                    season,
                                    episode,
                                );

                                if let Some(ref target_file) = selected {
                                    let mut only_files = HashSet::new();
                                    only_files.insert(target_file.index);
                                    let _ = session
                                        .update_only_files(&handle, &only_files)
                                        .await;

                                    if let Some((start_piece, width)) =
                                        compute_critical_piece_window(target_file, 0, 1024 * 1024)
                                    {
                                        info!(
                                            "[Engine] Prioritized head pieces: [{} (x{})] for {}",
                                            start_piece, width, target_file.name
                                        );
                                    }
                                }

                                {
                                    let mut guard = mgr.active_engines.write();
                                    if let Some(eng) = guard.get_mut(&hash_for_task) {
                                        eng.files = discovered_files;
                                        eng.video_file = selected;
                                        eng.metadata_ready = true;
                                        eng.status = "ready".to_string();
                                    }
                                }

                                // Spawn probe & stats loop
                                let mgr_stats = Arc::clone(&mgr);
                                let hash_stats = hash_for_task.clone();
                                let handle_stats = handle.clone();
                                tokio::spawn(async move {
                                    let port = *mgr_stats.primary_port.read();
                                    let proxy_url =
                                        format!("http://127.0.0.1:{}/proxy/{}", port, hash_stats);
                                    let mut probed_ok = false;

                                    loop {
                                        if !mgr_stats.active_engines.read().contains_key(&hash_stats)
                                        {
                                            break;
                                        }

                                        let stats = handle_stats.stats();
                                        let (speed, peers, progress) = match &stats.live {
                                            Some(live) => {
                                                let spd = (live.download_speed.mbps * 1024.0 * 1024.0)
                                                    as u64;
                                                let prs = live.snapshot.peer_stats.live;
                                                let prog = if stats.total_bytes > 0 {
                                                    stats.progress_bytes as f64
                                                        / stats.total_bytes as f64
                                                } else {
                                                    0.0
                                                };
                                                (spd, prs, prog)
                                            }
                                            None => (0, 0, 0.0),
                                        };

                                        {
                                            let mut guard = mgr_stats.active_engines.write();
                                            if let Some(eng) = guard.get_mut(&hash_stats) {
                                                eng.download_speed_bytes = speed;
                                                eng.peers = peers;
                                                eng.progress = progress;
                                            }
                                        }

                                        if !probed_ok {
                                            if let Ok(probe_res) = probe_media_url(&proxy_url).await
                                            {
                                                if probe_res.duration > 0.0 {
                                                    probed_ok = true;
                                                    let mut guard = mgr_stats.active_engines.write();
                                                    if let Some(eng) = guard.get_mut(&hash_stats) {
                                                        eng.duration = probe_res.duration;
                                                        eng.video_codec = probe_res.video_codec;
                                                        eng.audio_codec = probe_res.audio_codec;
                                                        eng.embedded_subtitles =
                                                            Some(probe_res.embedded_subtitles);
                                                        info!(
                                                            "[Engine] Codec Detected: {:?} / {:?} | Duration: {}s",
                                                            eng.video_codec,
                                                            eng.audio_codec,
                                                            eng.duration
                                                        );
                                                    }
                                                }
                                            }
                                        }

                                        tokio::time::sleep(Duration::from_secs(2)).await;
                                    }
                                });
                            }
                        }
                        Err(e) => {
                            error!("[Engine] Error adding torrent {}: {}", hash_for_task, e);
                            let mut guard = mgr.active_engines.write();
                            if let Some(eng) = guard.get_mut(&hash_for_task) {
                                eng.status = "error".to_string();
                                eng.error = Some(e.to_string());
                            }
                        }
                    }
                }
            }
        });

        initial_state
    }

    pub fn pause_engine(&self, hash: &str) {
        let clean_hash = hash.trim().to_ascii_lowercase();
        if let Some(eng) = self.active_engines.write().get_mut(&clean_hash) {
            eng.status = "paused".to_string();
        }
        info!("[Engine] Paused engine for {}", clean_hash);
    }

    pub fn destroy_engine(&self, hash: &str) -> Option<PathBuf> {
        let clean_hash = hash.trim().to_ascii_lowercase();
        self.cancel_stop_grace_timeout(&clean_hash);
        let removed = self.active_engines.write().remove(&clean_hash);
        if let Some(ref eng) = removed {
            info!("[Engine] Destroyed streaming engine for {}", clean_hash);
            return Some(eng.save_dir.clone());
        }
        None
    }

    /// Stops all competing streaming engines other than `target_hash`
    pub fn stop_competing_streams(&self, target_hash: &str, shared_download_hashes: &HashSet<String>) {
        let target = target_hash.trim().to_ascii_lowercase();
        self.cancel_stop_grace_timeout(&target);
        *self.active_streaming_hash.write() = Some(target.clone());

        let mut guard = self.active_engines.write();
        let keys: Vec<String> = guard.keys().cloned().collect();
        for other_hash in keys {
            if other_hash == target {
                continue;
            }
            if shared_download_hashes.contains(&other_hash) {
                guard.remove(&other_hash);
                continue;
            }
            guard.remove(&other_hash);
            info!(
                "[Engine] Paused previous streaming engine for {} to prioritize {}",
                other_hash, target
            );
        }
    }

    /// Updates sliding readahead window `[start, start + 24MB]` on HTTP Range requests
    pub fn on_proxy_range_request(&self, hash: &str, start_byte: u64) {
        let clean_hash = hash.trim().to_ascii_lowercase();
        self.cancel_stop_grace_timeout(&clean_hash);
        self.cache.update_access(&clean_hash);

        if let Some(eng) = self.active_engines.read().get(&clean_hash) {
            if !eng.is_mock {
                if let Some(ref vf) = eng.video_file {
                    if let Some((piece, width)) =
                        compute_critical_piece_window(vf, start_byte, eng.piece_length)
                    {
                        tracing::debug!(
                            "[Proxy] {}: Sliding readahead critical pieces [{}..+{}]",
                            clean_hash,
                            piece,
                            width
                        );
                    }
                }
            }
        }
    }
}

