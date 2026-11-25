use crate::cache::CacheManager;
use crate::config::{
    build_episode_pattern, build_magnet_uri, is_sample_video, is_video_file, EngineConfig,
    READAHEAD_WINDOW_BYTES, TRACKERS,
};
use crate::transcode::{probe_media_url, EmbeddedAudioTrack, EmbeddedSubtitle};
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
    pub head_preloaded: bool,
    pub is_mock: bool,
    pub video_file: Option<TorrentFileInfo>,
    pub files: Vec<TorrentFileInfo>,
    #[serde(default)]
    pub file_progress: Vec<u64>,
    #[serde(default)]
    pub selected_file_indices: HashSet<usize>,
    #[serde(default)]
    pub streaming_file_idx: Option<usize>,
    pub requested_file_idx: Option<usize>,
    pub requested_season: Option<u32>,
    pub requested_episode: Option<u32>,
    pub duration: f64,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_tracks: Option<Vec<EmbeddedAudioTrack>>,
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
    pub has_first_piece: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_playback_pieces: Option<bool>,
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
    pub audio_tracks: Option<Vec<EmbeddedAudioTrack>>,
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
        .timeout(Duration::from_millis(1200))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .ok()?;

    // Race all HTTP .torrent caches in parallel so we never block sequentially
    let mut set = tokio::task::JoinSet::new();
    for url in urls {
        let c = client.clone();
        set.spawn(async move {
            let res = c.get(&url).send().await.ok()?;
            if !res.status().is_success() {
                return None;
            }
            let bytes = res.bytes().await.ok()?;
            // Valid bencoded .torrent starts with 'd' (0x64)
            if bytes.len() > 64 && bytes[0] == 0x64 {
                Some(bytes)
            } else {
                None
            }
        });
    }

    while let Some(res) = set.join_next().await {
        if let Ok(Some(bytes)) = res {
            let _ = std::fs::create_dir_all(&cache_torrent_dir);
            if std::fs::write(&cache_torrent_path, &bytes).is_ok() {
                info!(
                    "[Engine] Fast-resolved .torrent metadata via HTTP cache ({} bytes) for {}",
                    bytes.len(),
                    clean_hash
                );
                set.abort_all();
                return Some(cache_torrent_path);
            }
        }
    }

    None
}

fn normalize_title_tokens(title: &str) -> Vec<String> {
    const STOP: &[&str] = &["the", "and", "for", "with", "from", "of", "in", "on", "to", "a", "an"];
    title
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|tok| tok.len() >= 2 && !STOP.contains(tok))
        .map(|s| s.to_string())
        .collect()
}

fn title_match_score(haystack: &str, tokens: &[String]) -> usize {
    if tokens.is_empty() {
        return 0;
    }
    let lower = haystack.to_ascii_lowercase();
    tokens.iter().filter(|t| lower.contains(t.as_str())).count()
}

fn file_match_text(file: &TorrentFileInfo) -> String {
    let path_str = file.path.to_string_lossy();
    if path_str.is_empty() {
        file.name.clone()
    } else {
        path_str.to_string()
    }
}

/// Checks if a filename/path explicitly declares a DIFFERENT season or episode than `(season, episode)`.
/// Also recognizes multi-episode combined files like `S01E01-E02` or `S01E01E02` as non-conflicting when
/// `episode` falls inside that span.
fn has_conflicting_episode(text: &str, season: u32, episode: u32) -> bool {
    static ANY_EP_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = ANY_EP_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(?:^|[^a-z0-9])(?:s0*(\d{1,2})[.\s_-]*e0*(\d{1,4})(?:[.\s_-]*-?[.\s_-]*e?0*(\d{1,4}))?|0*(\d{1,2})x0*(\d{1,4}))(?:\D|$)",
        )
        .unwrap()
    });
    let mut found_any = false;
    for cap in re.captures_iter(text) {
        let s_match = cap
            .get(1)
            .or_else(|| cap.get(4))
            .and_then(|m| m.as_str().parse::<u32>().ok());
        let e_start = cap
            .get(2)
            .or_else(|| cap.get(5))
            .and_then(|m| m.as_str().parse::<u32>().ok());
        let e_end = cap
            .get(3)
            .and_then(|m| m.as_str().parse::<u32>().ok());
        if let (Some(sm), Some(es)) = (s_match, e_start) {
            found_any = true;
            let matches_ep = match e_end {
                Some(ee) if ee >= es && ee.saturating_sub(es) <= 4 => episode >= es && episode <= ee,
                _ => episode == es,
            };
            if sm == season && matches_ep {
                return false;
            }
        }
    }
    if found_any {
        return true;
    }

    // Check explicit season-folder conflicts (e.g. "Season 1/03.mkv" when requesting Season 2)
    static SEASON_DIR_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let s_re = SEASON_DIR_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(?:^|[/\\._\-\s\[])(?:season|temporada|book)[.\s_-]*0*(\d{1,2})(?:\D|$)|(?:^|[/\\])s0*(\d{1,2})(?:[/\\])",
        )
        .unwrap()
    });
    let mut found_season = false;
    for cap in s_re.captures_iter(text) {
        let sm = cap
            .get(1)
            .or_else(|| cap.get(2))
            .and_then(|m| m.as_str().parse::<u32>().ok());
        if let Some(s_val) = sm {
            found_season = true;
            if s_val == season {
                return false;
            }
        }
    }
    found_season
}

/// Checks if a filename explicitly covers `(season, episode)` via a multi-episode span (e.g. `S01E01-E02`, `S01E01E02`, `S01E01-02`).
fn matches_multi_episode_span(text: &str, season: u32, episode: u32) -> bool {
    static SPAN_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = SPAN_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(?:^|[^a-z0-9])s0*(\d{1,2})[.\s_-]*e0*(\d{1,4})(?:[.\s_-]*-[.\s_-]*e?|e)0*(\d{1,4})(?:\D|$)",
        )
        .unwrap()
    });
    for cap in re.captures_iter(text) {
        let s = cap.get(1).and_then(|m| m.as_str().parse::<u32>().ok());
        let e1 = cap.get(2).and_then(|m| m.as_str().parse::<u32>().ok());
        let e2 = cap.get(3).and_then(|m| m.as_str().parse::<u32>().ok());
        if let (Some(sv), Some(start), Some(end)) = (s, e1, e2) {
            if sv == season && end >= start && end.saturating_sub(start) <= 4 && episode >= start && episode <= end {
                return true;
            }
        }
    }
    false
}

/// Selects the target video file prioritizing title + season + episode over `file_idx`:
/// 1. Exact Season + Episode regex on full relative path (scored by title tokens, then size)
/// 2. Episode-only pattern (`E03`, `Ep 03`, `Episode 03`, `- 03`) without conflicting season/episode
/// 3. Title token match (for movies or non-standard episode naming)
/// 4. Validated `file_idx` (only if it is a video file and does not conflict with requested season/episode)
/// 5. Largest non-sample video file fallback
pub fn select_target_video_file(
    files: &[TorrentFileInfo],
    file_idx: Option<usize>,
    season: Option<u32>,
    episode: Option<u32>,
) -> Option<TorrentFileInfo> {
    select_target_video_file_with_title(files, None, file_idx, season, episode)
}

pub fn select_target_video_file_with_title(
    files: &[TorrentFileInfo],
    title: Option<&str>,
    file_idx: Option<usize>,
    season: Option<u32>,
    episode: Option<u32>,
) -> Option<TorrentFileInfo> {
    if files.is_empty() {
        return None;
    }

    let title_tokens = title.map(normalize_title_tokens).unwrap_or_default();
    let video_files: Vec<&TorrentFileInfo> = files
        .iter()
        .filter(|f| is_video_file(&f.name))
        .collect();
    let non_sample_videos: Vec<&TorrentFileInfo> = video_files
        .iter()
        .copied()
        .filter(|f| !is_sample_video(&file_match_text(f)))
        .collect();
    let candidate_videos = if !non_sample_videos.is_empty() {
        &non_sample_videos
    } else {
        &video_files
    };

    // Priority 1: Exact Season + Episode pattern on full path (folder + filename)
    if let (Some(s), Some(e)) = (season, episode) {
        if s > 0 || e > 0 {
            if let Some(re) = build_episode_pattern(s, e) {
                let matching: Vec<&TorrentFileInfo> = candidate_videos
                    .iter()
                    .copied()
                    .filter(|f| re.is_match(&file_match_text(f)))
                    .collect();
                if let Some(best) = matching.into_iter().max_by_key(|f| {
                    let txt = file_match_text(f);
                    (title_match_score(&txt, &title_tokens), f.length)
                }) {
                    info!(
                        "[Engine] Found file by episode pattern S{:02}E{:02}: {} (idx={})",
                        s, e, best.name, best.index
                    );
                    return Some(best.clone());
                }
            }

            // Priority 1b: Combined multi-episode files (e.g. "Show.S01E01-E02.mkv", "Show.S01E01E02.mkv")
            let span_matching: Vec<&TorrentFileInfo> = candidate_videos
                .iter()
                .copied()
                .filter(|f| matches_multi_episode_span(&file_match_text(f), s, e))
                .collect();
            if let Some(best) = span_matching.into_iter().max_by_key(|f| {
                let txt = file_match_text(f);
                (title_match_score(&txt, &title_tokens), f.length)
            }) {
                info!(
                    "[Engine] Found file by multi-episode span S{:02}E{:02}: {} (idx={})",
                    s, e, best.name, best.index
                );
                return Some(best.clone());
            }

            // Priority 2: Episode-only pattern (e.g. "Season 2/03 - Title.mkv", "Show - 012 [1080p].mkv", "Ep03.mkv")
            let ep_only_pat = format!(
                r"(?i)(?:^0*{e}(?:[\s._\-]|$)|(?:^|[\s._\-\[\(])(?:ep(?:isode|isodio)?\.?[.\s_-]*|e|-\s*)0*{e}(?:\D|$))",
                e = e
            );
            if let Ok(ep_re) = regex::Regex::new(&ep_only_pat) {
                let season_hint_pat = format!(
                    r"(?i)(?:(?:season|temporada|book)[.\s_-]*0*{s}|(?:^|[/\\._\-\s\[])s0*{s})(?:\D|$)",
                    s = s
                );
                let season_re = regex::Regex::new(&season_hint_pat).ok();
                let matching: Vec<&TorrentFileInfo> = candidate_videos
                    .iter()
                    .copied()
                    .filter(|f| {
                        let txt = file_match_text(f);
                        !has_conflicting_episode(&txt, s, e) && ep_re.is_match(&f.name)
                    })
                    .collect();
                if let Some(best) = matching.into_iter().max_by_key(|f| {
                    let txt = file_match_text(f);
                    let season_bonus = season_re
                        .as_ref()
                        .map(|r| if r.is_match(&txt) { 1usize } else { 0usize })
                        .unwrap_or(0);
                    (season_bonus, title_match_score(&txt, &title_tokens), f.length)
                }) {
                    info!(
                        "[Engine] Found file by episode-only pattern S{:02}E{:02}: {} (idx={})",
                        s, e, best.name, best.index
                    );
                    return Some(best.clone());
                }
            }
        }
    }

    // Priority 3: Title token match (for movies or packs without standard SxxExx numbering)
    if !title_tokens.is_empty() && !candidate_videos.is_empty() {
        let best_by_title = candidate_videos
            .iter()
            .copied()
            .filter(|f| {
                if let (Some(s), Some(e)) = (season, episode) {
                    if s > 0 || e > 0 {
                        return !has_conflicting_episode(&file_match_text(f), s, e);
                    }
                }
                true
            })
            .filter_map(|f| {
                let score = title_match_score(&file_match_text(f), &title_tokens);
                if score > 0 { Some((score, f)) } else { None }
            })
            .max_by_key(|(score, f)| (*score, f.length));
        if let Some((_, best)) = best_by_title {
            // Only use pure title match for movies (when no season/episode was requested)
            if season.unwrap_or(0) == 0 && episode.unwrap_or(0) == 0 {
                info!("[Engine] Found file by title match: {} (idx={})", best.name, best.index);
                return Some(best.clone());
            }
        }
    }

    // Priority 4: Validated explicit fileIdx (must be a video file and not a conflicting episode)
    if let Some(idx) = file_idx {
        if let Some(f) = files.get(idx) {
            let conflicts = match (season, episode) {
                (Some(s), Some(e)) if s > 0 || e > 0 => {
                    has_conflicting_episode(&file_match_text(f), s, e)
                }
                _ => false,
            };
            if is_video_file(&f.name) && !conflicts {
                info!("[Engine] Using validated fileIdx {}: {}", idx, f.name);
                return Some(f.clone());
            }
        }
    }

    // Priority 5: Largest non-sample video file
    if let Some(largest) = candidate_videos.iter().copied().max_by_key(|f| f.length) {
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
    pub status_tx: tokio::sync::broadcast::Sender<(String, String)>,
    #[cfg(feature = "rqbit-backend")]
    pub rqbit_session: Arc<tokio::sync::OnceCell<Arc<librqbit::Session>>>,
    #[cfg(feature = "rqbit-backend")]
    pub rqbit_handles: Arc<RwLock<HashMap<String, Arc<librqbit::ManagedTorrent>>>>,
}

impl TorrentEngineManager {
    pub fn new(config: EngineConfig, cache: Arc<CacheManager>) -> Self {
        let (status_tx, _) = tokio::sync::broadcast::channel(128);
        Self {
            config,
            cache,
            active_engines: Arc::new(RwLock::new(HashMap::new())),
            active_streaming_hash: Arc::new(RwLock::new(None)),
            stop_grace_timers: Arc::new(RwLock::new(HashMap::new())),
            is_metered: Arc::new(RwLock::new(false)),
            primary_port: Arc::new(RwLock::new(crate::config::DEFAULT_PRIMARY_PORT)),
            status_tx,
            #[cfg(feature = "rqbit-backend")]
            rqbit_session: Arc::new(tokio::sync::OnceCell::new()),
            #[cfg(feature = "rqbit-backend")]
            rqbit_handles: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Broadcasts the current `EngineStatusResponse` for `hash` to any connected SSE listeners (`/status/events/:hash`).
    pub fn broadcast_stream_status(&self, hash: &str) {
        if self.status_tx.receiver_count() == 0 {
            return;
        }
        let clean_hash = hash.trim().to_ascii_lowercase();
        if let Ok(payload) = serde_json::to_string(&self.get_status(&clean_hash)) {
            let _ = self.status_tx.send((clean_hash, payload));
        }
    }

    #[cfg(feature = "rqbit-backend")]
    pub async fn get_or_init_session(&self) -> anyhow::Result<Arc<librqbit::Session>> {
        let dir = self.config.streaming_cache_dir.clone();
        let is_tv_mode = self.config.is_tv_mode;
        self.rqbit_session
            .get_or_try_init(|| async move {
                info!("[DHT] Initializing BitTorrent session (persistent DHT, deferred RAM writes, global trackers, UPnP)...");
                let mut opts = librqbit::SessionOptions::default();
                opts.disable_dht = false;
                opts.disable_dht_persistence = false;
                opts.dht_config = Some(librqbit::dht::PersistentDhtConfig {
                    config_filename: Some(dir.join("dht.json")),
                    dump_interval: Some(Duration::from_secs(60)),
                });
                opts.fastresume = false;
                opts.persistence = None;
                opts.listen_port_range = Some(4240..4260);
                opts.enable_upnp_port_forwarding = true;
                opts.defer_writes_up_to = None;
                opts.concurrent_init_limit = Some(if is_tv_mode { 2 } else { 4 });
                opts.trackers = TRACKERS
                    .iter()
                    .filter_map(|t| url::Url::parse(t).ok())
                    .collect();
                opts.peer_opts = Some(librqbit::PeerConnectionOptions {
                    connect_timeout: Some(Duration::from_secs(4)),
                    read_write_timeout: Some(Duration::from_secs(10)),
                    ..Default::default()
                });
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
                has_first_piece: None,
                has_playback_pieces: None,
                file_name: None,
                file_path: None,
                file_size: None,
                file_idx: None,
                total_files: None,
                duration: None,
                video_codec: None,
                audio_codec: None,
                audio_tracks: None,
                subtitles: None,
                progress: None,
                peers: None,
                speed: None,
            };
        };

        let piece_ready =
            engine.is_mock || engine.head_preloaded || engine.progress > 0.0;

        EngineStatusResponse {
            status: engine.status.clone(),
            metadata_ready: Some(engine.metadata_ready),
            has_first_piece: Some(piece_ready),
            has_playback_pieces: Some(piece_ready),
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
            audio_tracks: engine.audio_tracks.clone(),
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
            head_preloaded: true,
            is_mock: true,
            video_file: Some(file_info.clone()),
            files: vec![file_info],
            file_progress: vec![file_size],
            selected_file_indices: HashSet::from([file_idx.unwrap_or(0)]),
            streaming_file_idx: Some(file_idx.unwrap_or(0)),
            requested_file_idx: file_idx,
            requested_season: None,
            requested_episode: None,
            duration: saved_duration,
            video_codec: None,
            audio_codec: None,
            audio_tracks: None,
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
        self.broadcast_stream_status(&clean_hash);

        // Probe local file in background for duration, codecs, and embedded subtitles
        let mgr = Arc::clone(self);
        let hash_clone = clean_hash.clone();
        let port = *self.primary_port.read();
        tokio::spawn(async move {
            let proxy_url = format!("http://127.0.0.1:{}/proxy/{}", port, hash_clone);
            if let Ok(probed) = probe_media_url(&proxy_url).await {
                {
                    let mut guard = mgr.active_engines.write();
                    if let Some(eng) = guard.get_mut(&hash_clone) {
                        if probed.duration > 0.0 {
                            eng.duration = probed.duration;
                        }
                        eng.video_codec = probed.video_codec;
                        eng.audio_codec = probed.audio_codec;
                        eng.audio_tracks = Some(probed.audio_tracks);
                        eng.embedded_subtitles = Some(probed.embedded_subtitles);
                        info!(
                            "[Mock Engine] Probed duration: {}s | video: {:?} | audio: {:?}",
                            eng.duration, eng.video_codec, eng.audio_codec
                        );
                    }
                }
                mgr.broadcast_stream_status(&hash_clone);
            }
        });

        mock_state
    }

    /// Starts or updates a streaming torrent engine (or background download when `custom_save_dir` is `Some`).
    pub async fn start_or_get_engine(
        self: &Arc<Self>,
        hash: &str,
        file_idx: Option<usize>,
        season: Option<u32>,
        episode: Option<u32>,
        custom_save_dir: Option<PathBuf>,
    ) -> StreamEngineState {
        let clean_hash = hash.trim().to_ascii_lowercase();
        let is_streaming_mode = custom_save_dir.is_none();
        self.cancel_stop_grace_timeout(&clean_hash);
        if is_streaming_mode {
            *self.active_streaming_hash.write() = Some(clean_hash.clone());
        }

        // Check if already active and update episode selection if needed
        {
            let mut guard = self.active_engines.write();
            if let Some(existing) = guard.get_mut(&clean_hash) {
                let was_paused = existing.status == "paused";
                if was_paused && !existing.is_mock {
                    existing.status = if existing.metadata_ready {
                        "ready".to_string()
                    } else {
                        "loading".to_string()
                    };
                    #[cfg(feature = "rqbit-backend")]
                    if let Some(handle) = self.rqbit_handles.read().get(&clean_hash).cloned() {
                        let mgr = Arc::clone(self);
                        tokio::spawn(async move {
                            if let Ok(session) = mgr.get_or_init_session().await {
                                let _ = session.unpause(&handle).await;
                            }
                        });
                    }
                }

                // Always keep the latest requested episode/file up to date (even while metadata is resolving)
                if is_streaming_mode {
                    if file_idx.is_some() {
                        existing.requested_file_idx = file_idx;
                    }
                    if season.is_some() {
                        existing.requested_season = season;
                    }
                    if episode.is_some() {
                        existing.requested_episode = episode;
                    }
                }

                if let (Some(s), Some(e)) = (season, episode) {
                    if existing.metadata_ready && !existing.is_mock {
                        let current_matches = existing
                            .video_file
                            .as_ref()
                            .map(|vf| {
                                let txt = file_match_text(vf);
                                build_episode_pattern(s, e)
                                    .map(|re| re.is_match(&txt))
                                    .unwrap_or(false)
                                    || matches_multi_episode_span(&txt, s, e)
                            })
                            .unwrap_or(false);

                        if !current_matches {
                            if let Some(correct) =
                                select_target_video_file(&existing.files, file_idx, season, episode)
                            {
                                let target_idx = correct.index;
                                let target_len = correct.length;
                                let target_name = correct.name.clone();
                                if is_streaming_mode {
                                    info!(
                                        "[Engine] Switched stream file in pack to: {} (idx={})",
                                        target_name, target_idx
                                    );
                                    if let Some(prev_stream_idx) = existing.streaming_file_idx {
                                        if prev_stream_idx != target_idx {
                                            existing.selected_file_indices.remove(&prev_stream_idx);
                                        }
                                    }
                                    existing.streaming_file_idx = Some(target_idx);
                                    existing.video_file = Some(correct);
                                    existing.head_preloaded = false;
                                    existing.duration = 0.0;
                                    existing.video_codec = None;
                                    existing.audio_codec = None;
                                    existing.audio_tracks = None;
                                    existing.embedded_subtitles = None;
                                } else {
                                    info!(
                                        "[Engine] Adding season-pack episode file to active selection: {} (idx={})",
                                        target_name, target_idx
                                    );
                                    if existing.video_file.is_none() {
                                        existing.video_file = Some(correct);
                                    }
                                }
                                existing.selected_file_indices.insert(target_idx);
                                let union_files = existing.selected_file_indices.clone();
                                #[cfg(feature = "rqbit-backend")]
                                if let Some(handle) =
                                    self.rqbit_handles.read().get(&clean_hash).cloned()
                                {
                                    let mgr = Arc::clone(self);
                                    let hash_sw = clean_hash.clone();
                                    tokio::spawn(async move {
                                        if let Ok(session) = mgr.get_or_init_session().await {
                                            let _ = session
                                                .update_only_files(&handle, &union_files)
                                                .await;
                                        }
                                        if is_streaming_mode {
                                            use tokio::io::{AsyncReadExt, AsyncSeekExt};
                                            let preload_fut = async {
                                                let Ok(mut stream) = handle.stream(target_idx) else {
                                                    return;
                                                };
                                                let mut buf = vec![0u8; 64 * 1024];
                                                let mut read_head = 0usize;
                                                while read_head < 256 * 1024 {
                                                    match stream.read(&mut buf).await {
                                                        Ok(0) | Err(_) => break,
                                                        Ok(n) => {
                                                            read_head += n;
                                                            if read_head >= 64 * 1024 {
                                                                if let Some(eng) = mgr.active_engines.write().get_mut(&hash_sw) {
                                                                    eng.head_preloaded = true;
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                if read_head > 0 {
                                                    if let Some(eng) = mgr.active_engines.write().get_mut(&hash_sw) {
                                                        eng.head_preloaded = true;
                                                    }
                                                }
                                                if target_len > 2 * 1024 * 1024 {
                                                    let tail_offset = target_len.saturating_sub(512 * 1024);
                                                    if stream.seek(std::io::SeekFrom::Start(tail_offset)).await.is_ok() {
                                                        let _ = stream.read(&mut buf).await;
                                                    }
                                                }
                                                info!(
                                                    "[Engine] Preloaded switched episode head ({} KB) + tail for {}",
                                                    read_head / 1024,
                                                    target_name
                                                );
                                            };
                                            let _ = tokio::time::timeout(
                                                Duration::from_secs(15),
                                                preload_fut,
                                            )
                                            .await;
                                        }
                                    });
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

        // Never mark a non-mock torrent as `ready` from a preallocated/sparse file on disk
        // before librqbit has initialized its handle — otherwise `/proxy` could stream raw zeroes.
        let initial_state = StreamEngineState {
            hash: clean_hash.clone(),
            status: "loading".to_string(),
            metadata_ready: false,
            head_preloaded: false,
            is_mock: false,
            video_file: None,
            files: Vec::new(),
            file_progress: Vec::new(),
            selected_file_indices: HashSet::new(),
            streaming_file_idx: None,
            requested_file_idx: file_idx,
            requested_season: season,
            requested_episode: episode,
            duration: 0.0,
            video_codec: None,
            audio_codec: None,
            audio_tracks: None,
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

        if is_streaming_mode {
            self.cache.add_torrent(&clean_hash, file_idx, season, episode);
        }

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
                    let default_trackers: Vec<String> =
                        TRACKERS.iter().map(|s| s.to_string()).collect();

                    // Only use `list_only` when we already have local `.torrent` bytes (0ms in-memory parse).
                    // For magnets, add directly ONCE with all trackers so we never drop connected peers!
                    let (final_target, only_files_vec) = if let Some(bytes) = cached_torrent
                        .as_ref()
                        .and_then(|path| std::fs::read(path).ok())
                    {
                        let mut list_opts = librqbit::AddTorrentOptions::default();
                        list_opts.list_only = true;
                        list_opts.output_folder =
                            Some(save_dir_for_task.to_string_lossy().to_string());

                        match session
                            .add_torrent(librqbit::AddTorrent::from_bytes(bytes.clone()), Some(list_opts))
                            .await
                        {
                            Ok(librqbit::AddTorrentResponse::ListOnly(list_res)) => {
                                let mut preview_files = Vec::new();
                                let mut offset = 0u64;
                                if let Ok(iter) = list_res.info.iter_file_details() {
                                    for (idx, fd) in iter.enumerate() {
                                        if let Ok(rel) = fd.filename.to_pathbuf() {
                                            let name = rel
                                                .file_name()
                                                .and_then(|n| n.to_str())
                                                .unwrap_or("video.mp4")
                                                .to_string();
                                            preview_files.push(TorrentFileInfo {
                                                index: idx,
                                                name,
                                                path: save_dir_for_task.join(&rel),
                                                length: fd.len,
                                                offset,
                                            });
                                        }
                                        offset += fd.len;
                                    }
                                }
                                let only = select_target_video_file(
                                    &preview_files,
                                    file_idx,
                                    season,
                                    episode,
                                )
                                .map(|f| vec![f.index]);
                                (
                                    librqbit::AddTorrent::from_bytes(list_res.torrent_bytes),
                                    only,
                                )
                            }
                            _ => (
                                librqbit::AddTorrent::from_bytes(bytes),
                                file_idx.map(|i| vec![i]),
                            ),
                        }
                    } else {
                        // For magnets where season/episode is known (season packs), start with an EMPTY
                        // `only_files` selection so librqbit resolves metadata with 0 disk checksums and
                        // 0 unwanted episode allocations, then immediately selects ONLY the exact episode.
                        let initial_only = if season.unwrap_or(0) > 0 && episode.unwrap_or(0) > 0 {
                            Some(Vec::new())
                        } else {
                            file_idx.map(|i| vec![i])
                        };
                        (
                            librqbit::AddTorrent::from_url(build_magnet_uri(&hash_for_task)),
                            initial_only,
                        )
                    };

                    let mut add_opts = librqbit::AddTorrentOptions::default();
                    add_opts.overwrite = true;
                    add_opts.only_files = only_files_vec.clone();
                    add_opts.trackers = Some(default_trackers);
                    add_opts.force_tracker_interval = Some(Duration::from_secs(60));
                    add_opts.output_folder =
                        Some(save_dir_for_task.to_string_lossy().to_string());

                    match session.add_torrent(final_target, Some(add_opts)).await {
                        Ok(response) => {
                            if let Some(handle) = response.into_handle() {
                                {
                                    mgr.rqbit_handles
                                        .write()
                                        .insert(hash_for_task.clone(), handle.clone());
                                }
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

                                // Read latest requested episode/file_idx in case user switched episodes while metadata resolved
                                let (eff_file_idx, eff_season, eff_episode) = {
                                    let guard = mgr.active_engines.read();
                                    if let Some(eng) = guard.get(&hash_for_task) {
                                        (
                                            eng.requested_file_idx.or(file_idx),
                                            eng.requested_season.or(season),
                                            eng.requested_episode.or(episode),
                                        )
                                    } else {
                                        (file_idx, season, episode)
                                    }
                                };

                                let selected = select_target_video_file(
                                    &discovered_files,
                                    eff_file_idx,
                                    eff_season,
                                    eff_episode,
                                );

                                if let Some(ref target_file) = selected {
                                    let union_files = {
                                        let mut guard = mgr.active_engines.write();
                                        if let Some(eng) = guard.get_mut(&hash_for_task) {
                                            eng.selected_file_indices.insert(target_file.index);
                                            if is_streaming_mode {
                                                eng.streaming_file_idx = Some(target_file.index);
                                            }
                                            eng.selected_file_indices.clone()
                                        } else {
                                            HashSet::from([target_file.index])
                                        }
                                    };
                                    let already_selected = union_files.len() == 1
                                        && only_files_vec.as_deref() == Some(&[target_file.index]);
                                    if !already_selected {
                                        let _ = session
                                            .update_only_files(&handle, &union_files)
                                            .await;
                                    }

                                    if let Some((start_piece, width)) =
                                        compute_critical_piece_window(target_file, 0, 1024 * 1024)
                                    {
                                        info!(
                                            "[Engine] Prioritized head pieces: [{} (x{})] for {}",
                                            start_piece, width, target_file.name
                                        );
                                    }

                                    // TorrServer / Stremio trick: immediately open a short-lived FileStream
                                    // to prioritize Piece 0 (container header) AND the tail piece (MP4 moov atom
                                    // / MKV Cues seek index) before <video> or ffprobe even requests /proxy.
                                    if is_streaming_mode {
                                        let handle_preload = handle.clone();
                                        let mgr_preload = Arc::clone(&mgr);
                                        let hash_preload = hash_for_task.clone();
                                        let file_index = target_file.index;
                                        let file_len = target_file.length;
                                        let file_name = target_file.name.clone();
                                        tokio::spawn(async move {
                                            use tokio::io::{AsyncReadExt, AsyncSeekExt};
                                            let preload_fut = async {
                                                let Ok(mut stream) = handle_preload.stream(file_index) else {
                                                    return;
                                                };
                                                let mut buf = vec![0u8; 64 * 1024];
                                                // 1. Pull head (up to 256 KB) to prioritize Piece 0 immediately
                                                let mut read_head = 0usize;
                                                while read_head < 256 * 1024 {
                                                    match stream.read(&mut buf).await {
                                                        Ok(0) | Err(_) => break,
                                                        Ok(n) => {
                                                            read_head += n;
                                                            if read_head >= 64 * 1024 {
                                                                if let Some(eng) = mgr_preload.active_engines.write().get_mut(&hash_preload) {
                                                                    eng.head_preloaded = true;
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                if read_head > 0 {
                                                    if let Some(eng) = mgr_preload.active_engines.write().get_mut(&hash_preload) {
                                                        eng.head_preloaded = true;
                                                    }
                                                }
                                                // 2. Pull tail (last 512 KB -> 64 KB) for MP4 moov / MKV Cues
                                                if file_len > 2 * 1024 * 1024 {
                                                    let tail_offset = file_len.saturating_sub(512 * 1024);
                                                    if stream.seek(std::io::SeekFrom::Start(tail_offset)).await.is_ok() {
                                                        let _ = stream.read(&mut buf).await;
                                                    }
                                                }
                                                info!(
                                                    "[Engine] Preloaded head ({} KB) + tail index for {}",
                                                    read_head / 1024,
                                                    file_name
                                                );
                                            };
                                            let _ = tokio::time::timeout(
                                                Duration::from_secs(15),
                                                preload_fut,
                                            )
                                            .await;
                                        });
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
                                mgr.broadcast_stream_status(&hash_for_task);

                                // Spawn non-blocking stats loop (500ms tick) + detached ffprobe once bytes flow (non-Android)
                                let mgr_stats = Arc::clone(&mgr);
                                let hash_stats = hash_for_task.clone();
                                let handle_stats = handle.clone();
                                tokio::spawn(async move {
                                    #[cfg(not(target_os = "android"))]
                                    let proxy_url = {
                                        let port = *mgr_stats.primary_port.read();
                                        format!("http://127.0.0.1:{}/proxy/{}", port, hash_stats)
                                    };
                                    #[cfg(not(target_os = "android"))]
                                    let mut probe_spawned = false;

                                    loop {
                                        if !mgr_stats.active_engines.read().contains_key(&hash_stats)
                                        {
                                            break;
                                        }

                                        let stats = handle_stats.stats();
                                        let file_prog = stats.file_progress.clone();
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
                                                if !file_prog.is_empty() {
                                                    eng.file_progress = file_prog;
                                                }
                                                // Compute per-episode progress for the active streamed video_file
                                                // so multi-episode packs report accurate episode progress.
                                                eng.progress = if let Some(ref vf) = eng.video_file {
                                                    if vf.length > 0 {
                                                        if let Some(&fb) = eng.file_progress.get(vf.index) {
                                                            (fb.min(vf.length) as f64 / vf.length as f64).min(1.0)
                                                        } else {
                                                            progress
                                                        }
                                                    } else {
                                                        progress
                                                    }
                                                } else {
                                                    progress
                                                };
                                            }
                                        }
                                        mgr_stats.broadcast_stream_status(&hash_stats);

                                        #[cfg(not(target_os = "android"))]
                                        if !probe_spawned && stats.progress_bytes > 0 {
                                            probe_spawned = true;
                                            let mgr_probe = Arc::clone(&mgr_stats);
                                            let hash_probe = hash_stats.clone();
                                            let url_probe = proxy_url.clone();
                                            tokio::spawn(async move {
                                                for _ in 0..2 {
                                                    if !mgr_probe.active_engines.read().contains_key(&hash_probe) {
                                                        break;
                                                    }
                                                    if let Ok(probe_res) = probe_media_url(&url_probe).await {
                                                        if probe_res.duration > 0.0 {
                                                            {
                                                                let mut guard = mgr_probe.active_engines.write();
                                                                if let Some(eng) = guard.get_mut(&hash_probe) {
                                                                    eng.duration = probe_res.duration;
                                                                    eng.video_codec = probe_res.video_codec;
                                                                    eng.audio_codec = probe_res.audio_codec;
                                                                    eng.audio_tracks = Some(probe_res.audio_tracks);
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
                                                            mgr_probe.broadcast_stream_status(&hash_probe);
                                                            break;
                                                        }
                                                    }
                                                    tokio::time::sleep(Duration::from_secs(3)).await;
                                                }
                                            });
                                        }

                                        tokio::time::sleep(Duration::from_millis(500)).await;
                                    }
                                });
                            }
                        }
                        Err(e) => {
                            error!("[Engine] Error adding torrent {}: {}", hash_for_task, e);
                            {
                                let mut guard = mgr.active_engines.write();
                                if let Some(eng) = guard.get_mut(&hash_for_task) {
                                    eng.status = "error".to_string();
                                    eng.error = Some(e.to_string());
                                }
                            }
                            mgr.broadcast_stream_status(&hash_for_task);
                        }
                    }
                }
            }
        });

        initial_state
    }

    #[cfg(feature = "rqbit-backend")]
    pub fn open_stream_for_file_idx(
        &self,
        hash: &str,
        file_idx: usize,
    ) -> Option<impl tokio::io::AsyncRead + tokio::io::AsyncSeek + Unpin + Send + 'static> {
        let clean_hash = hash.trim().to_ascii_lowercase();
        let handle = self.rqbit_handles.read().get(&clean_hash)?.clone();
        handle.stream(file_idx).ok()
    }

    #[cfg(feature = "rqbit-backend")]
    pub fn open_stream_for_hash(
        &self,
        hash: &str,
    ) -> Option<impl tokio::io::AsyncRead + tokio::io::AsyncSeek + Unpin + Send + 'static> {
        let clean_hash = hash.trim().to_ascii_lowercase();
        let file_idx = {
            let guard = self.active_engines.read();
            let eng = guard.get(&clean_hash)?;
            if eng.is_mock {
                return None;
            }
            eng.video_file.as_ref()?.index
        };
        let handle = self.rqbit_handles.read().get(&clean_hash)?.clone();
        handle.stream(file_idx).ok()
    }

    pub fn pause_engine(self: &Arc<Self>, hash: &str) {
        let clean_hash = hash.trim().to_ascii_lowercase();
        if let Some(eng) = self.active_engines.write().get_mut(&clean_hash) {
            eng.status = "paused".to_string();
        }
        #[cfg(feature = "rqbit-backend")]
        if let Some(handle) = self.rqbit_handles.read().get(&clean_hash).cloned() {
            let mgr = Arc::clone(self);
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move {
                    if let Ok(session) = mgr.get_or_init_session().await {
                        let _ = session.pause(&handle).await;
                    }
                });
            }
        }
        info!("[Engine] Paused engine for {}", clean_hash);
    }

    /// Async variant of `destroy_engine` that awaits `session.delete` so all OS file handles
    /// are closed before callers move or delete files on Windows.
    pub async fn destroy_engine_async(self: &Arc<Self>, hash: &str) -> Option<PathBuf> {
        let clean_hash = hash.trim().to_ascii_lowercase();
        self.cancel_stop_grace_timeout(&clean_hash);
        #[cfg(feature = "rqbit-backend")]
        {
            let handle_opt = self.rqbit_handles.write().remove(&clean_hash);
            if let Some(handle) = handle_opt {
                if let Ok(session) = self.get_or_init_session().await {
                    let _ = session
                        .delete(librqbit::api::TorrentIdOrHash::Id(handle.id()), false)
                        .await;
                }
            }
        }
        let removed = self.active_engines.write().remove(&clean_hash);
        if let Some(ref eng) = removed {
            info!("[Engine] Destroyed streaming engine (async) for {}", clean_hash);
            return Some(eng.save_dir.clone());
        }
        None
    }

    /// Synchronizes `librqbit` `only_files` to the exact set of active file indices for a shared torrent
    /// (e.g. when pausing, resuming, or completing one episode of a multi-episode season pack).
    pub async fn sync_only_files(self: &Arc<Self>, hash: &str, desired_indices: HashSet<usize>) {
        let clean_hash = hash.trim().to_ascii_lowercase();
        {
            let mut guard = self.active_engines.write();
            if let Some(eng) = guard.get_mut(&clean_hash) {
                if eng.selected_file_indices == desired_indices {
                    return;
                }
                eng.selected_file_indices = desired_indices.clone();
            }
        }
        #[cfg(feature = "rqbit-backend")]
        {
            let handle = self.rqbit_handles.read().get(&clean_hash).cloned();
            if let Some(handle) = handle {
                if let Ok(session) = self.get_or_init_session().await {
                    if let Err(e) = session.update_only_files(&handle, &desired_indices).await {
                        error!(
                            "[Engine] Failed to update_only_files for {} ({:?}): {}",
                            clean_hash, desired_indices, e
                        );
                    } else {
                        info!(
                            "[Engine] Updated active files for {} -> {:?}",
                            clean_hash, desired_indices
                        );
                    }
                }
            }
        }
    }

    pub fn destroy_engine(self: &Arc<Self>, hash: &str) -> Option<PathBuf> {
        let clean_hash = hash.trim().to_ascii_lowercase();
        self.cancel_stop_grace_timeout(&clean_hash);
        #[cfg(feature = "rqbit-backend")]
        if let Some(handle) = self.rqbit_handles.write().remove(&clean_hash) {
            let mgr = Arc::clone(self);
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move {
                    if let Ok(session) = mgr.get_or_init_session().await {
                        let _ = session
                            .delete(librqbit::api::TorrentIdOrHash::Id(handle.id()), false)
                            .await;
                    }
                });
            }
        }
        let removed = self.active_engines.write().remove(&clean_hash);
        if let Some(ref eng) = removed {
            info!("[Engine] Destroyed streaming engine for {}", clean_hash);
            return Some(eng.save_dir.clone());
        }
        None
    }

    pub fn destroy_all_engines(self: &Arc<Self>) {
        let keys: Vec<String> = self.active_engines.read().keys().cloned().collect();
        for hash in keys {
            self.destroy_engine(&hash);
        }
    }

    pub fn destroy_engines_except(self: &Arc<Self>, protected_hashes: &HashSet<String>) {
        let keys: Vec<String> = self.active_engines.read().keys().cloned().collect();
        for hash in keys {
            if !protected_hashes.contains(&hash) {
                self.destroy_engine(&hash);
            }
        }
    }

    /// Stops all competing streaming engines other than `target_hash`
    pub fn stop_competing_streams(
        self: &Arc<Self>,
        target_hash: &str,
        shared_download_hashes: &HashSet<String>,
    ) {
        let target = target_hash.trim().to_ascii_lowercase();
        self.cancel_stop_grace_timeout(&target);
        *self.active_streaming_hash.write() = Some(target.clone());

        let keys: Vec<String> = self.active_engines.read().keys().cloned().collect();
        for other_hash in keys {
            if other_hash == target {
                continue;
            }
            if shared_download_hashes.contains(&other_hash) {
                self.active_engines.write().remove(&other_hash);
                continue;
            }
            self.destroy_engine(&other_hash);
            info!(
                "[Engine] Stopped previous streaming engine for {} to prioritize {}",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_file(index: usize, name: &str, length: u64, offset: u64) -> TorrentFileInfo {
        TorrentFileInfo {
            index,
            name: name.to_string(),
            path: PathBuf::from(name),
            length,
            offset,
        }
    }

    #[test]
    fn select_target_video_file_prioritizes_episode_then_idx_then_largest() {
        let files = vec![
            mk_file(0, "Show.S01E01.1080p.mkv", 1_000_000, 0),
            mk_file(1, "Show.S01E02.1080p.mkv", 2_000_000, 1_000_000),
            mk_file(2, "readme.txt", 500, 3_000_000),
        ];

        // Priority 1: Episode regex overrides file_idx
        let ep1 = select_target_video_file(&files, Some(1), Some(1), Some(1)).unwrap();
        assert_eq!(ep1.index, 0);

        // Priority 2: Explicit file_idx when no season/episode
        let idx0 = select_target_video_file(&files, Some(0), None, None).unwrap();
        assert_eq!(idx0.index, 0);

        // Priority 3: Largest video file fallback
        let largest = select_target_video_file(&files, None, None, None).unwrap();
        assert_eq!(largest.index, 1);
    }

    #[test]
    fn select_target_video_file_handles_underscores_folders_titles_and_samples() {
        let mk_path_file = |index: usize, rel_path: &str, length: u64| -> TorrentFileInfo {
            let pb = PathBuf::from(rel_path);
            let name = pb
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(rel_path)
                .to_string();
            TorrentFileInfo {
                index,
                name,
                path: pb,
                length,
                offset: 0,
            }
        };

        // 1. Underscore separator + sample exclusion + wrong file_idx
        let pack_underscores = vec![
            mk_path_file(0, "MyShow_S01E01_1080p.mkv", 1_200_000_000),
            mk_path_file(1, "MyShow_S01E02_sample.mkv", 45_000_000),
            mk_path_file(2, "MyShow_S01E02_1080p.mkv", 1_350_000_000),
            mk_path_file(3, "MyShow_S01E05_1080p.mkv", 1_500_000_000),
        ];
        // Even if file_idx points to S01E05 (index 3), S01E02 must resolve to index 2 (not sample index 1)
        let ep2 = select_target_video_file_with_title(
            &pack_underscores,
            Some("MyShow"),
            Some(3),
            Some(1),
            Some(2),
        )
        .unwrap();
        assert_eq!(ep2.index, 2);

        // 2. Folder-based season pack (Season 2/03 - Episode Name.mkv)
        let pack_folders = vec![
            mk_path_file(0, "Season 1/03 - Old Episode.mkv", 1_000_000_000),
            mk_path_file(1, "Season 2/02 - Prev Episode.mkv", 1_100_000_000),
            mk_path_file(2, "Season 2/03 - Target Episode.mkv", 1_050_000_000),
        ];
        let s2e3 = select_target_video_file_with_title(
            &pack_folders,
            Some("Target"),
            Some(0), // wrong file_idx pointing to Season 1
            Some(2),
            Some(3),
        )
        .unwrap();
        assert_eq!(s2e3.index, 2);

        // 3. Title-based disambiguation for movies (ignoring wrong file_idx pointing to bonus)
        let movie_pack = vec![
            mk_path_file(0, "Extras/Behind.The.Scenes.mkv", 500_000_000),
            mk_path_file(1, "Inception.2010.1080p.BluRay.mkv", 4_200_000_000),
            mk_path_file(2, "Inception.2010.Sample.mkv", 60_000_000),
        ];
        let movie = select_target_video_file_with_title(
            &movie_pack,
            Some("Inception"),
            Some(0),
            None,
            None,
        )
        .unwrap();
        assert_eq!(movie.index, 1);

        // 4. Multi-episode span (S01E01-E02) + 3-digit episode padding (S01E005) + Extras exclusion
        let large_pack = vec![
            mk_path_file(0, "Extras/S01E02 - Deleted Scene.mkv", 200_000_000),
            mk_path_file(1, "Show.S01E01-E02.1080p.mkv", 2_400_000_000),
            mk_path_file(2, "Show.S01E005.1080p.mkv", 1_200_000_000),
            mk_path_file(3, "Season 1/003 - Ep3.mkv", 1_100_000_000),
            mk_path_file(4, "Season 2/003 - Ep3.mkv", 1_150_000_000),
        ];
        // S01E02 should match the combined S01E01-E02 file (idx 1), NOT Extras (idx 0)
        let combined = select_target_video_file_with_title(
            &large_pack,
            Some("Show"),
            Some(0),
            Some(1),
            Some(2),
        )
        .unwrap();
        assert_eq!(combined.index, 1);

        // S01E05 should match S01E005 (idx 2)
        let ep5 = select_target_video_file_with_title(
            &large_pack,
            Some("Show"),
            None,
            Some(1),
            Some(5),
        )
        .unwrap();
        assert_eq!(ep5.index, 2);

        // S02E03 should match Season 2/003 (idx 4) and reject Season 1/003 (idx 3)
        let s2e3_pad = select_target_video_file_with_title(
            &large_pack,
            Some("Show"),
            Some(3),
            Some(2),
            Some(3),
        )
        .unwrap();
        assert_eq!(s2e3_pad.index, 4);
    }

    #[test]
    fn compute_critical_piece_window_bounds() {
        let f = mk_file(0, "movie.mkv", 50_000_000, 10_000_000);
        let (start, width) = compute_critical_piece_window(&f, 0, 4 * 1024 * 1024).unwrap();
        assert_eq!(start, 2);
        assert!((2..=8).contains(&width));
        assert!(compute_critical_piece_window(&f, 0, 0).is_none());
    }
}

