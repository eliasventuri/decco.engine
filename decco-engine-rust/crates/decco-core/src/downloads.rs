use crate::config::{
    find_exact_video_file, get_download_id, is_video_file, now_ms, EngineConfig,
    AUTO_RESUME_DELAY_MS, STALLED_TIMEOUT_MS, STOP_GRACE_PERIOD_MS,
};
use crate::torrent::TorrentEngineManager;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use sysinfo::Disks;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleEntry {
    pub lang: String,
    pub label: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadSettings {
    pub max_concurrent: usize,
    pub auto_pause_on_playback: bool,
}

impl Default for DownloadSettings {
    fn default() -> Self {
        Self {
            max_concurrent: 2,
            auto_pause_on_playback: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadItemMeta {
    pub id: String,
    pub hash: String,
    pub title: String,
    pub imdb_id: String,
    #[serde(default)]
    pub poster: String,
    pub season: u32,
    pub episode: u32,
    pub file_idx: Option<usize>,
    pub file_name: Option<String>,
    pub file_size: u64,
    pub download_dir: String,
    #[serde(default)]
    pub subtitles: Vec<SubtitleEntry>,
    pub status: String,
    #[serde(default)]
    pub progress: f64,
    #[serde(default)]
    pub speed: u64,
    #[serde(default)]
    pub peers: usize,
    pub eta: Option<u64>,
    #[serde(default)]
    pub is_stalled: bool,
    #[serde(default)]
    pub downloaded_bytes: u64,
    #[serde(default)]
    pub auto_paused_for_stream: bool,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub queue_position: Option<u64>,
    pub duration: Option<f64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadsDatabase {
    pub downloads: HashMap<String, DownloadItemMeta>,
    #[serde(default)]
    pub settings: DownloadSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskSpaceInfo {
    pub free: u64,
    pub total: u64,
    pub used: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadsPayload {
    pub downloads: Vec<DownloadItemMeta>,
    pub settings: DownloadSettings,
    pub disk_space: Option<DiskSpaceInfo>,
    pub download_dir: String,
}

pub struct ActiveDownloadTask {
    pub meta: DownloadItemMeta,
    pub poll_handle: Option<JoinHandle<()>>,
    pub stalled_handle: Option<JoinHandle<()>>,
}

pub struct DownloadsManager {
    pub config: EngineConfig,
    pub torrent_mgr: Arc<TorrentEngineManager>,
    pub db: Arc<RwLock<DownloadsDatabase>>,
    pub active_downloads: Arc<RwLock<HashMap<String, ActiveDownloadTask>>>,
    pub sse_tx: broadcast::Sender<String>,
    last_broadcast_payload: Arc<RwLock<String>>,
    auto_resume_handle: Arc<RwLock<Option<JoinHandle<()>>>>,
    disk_space_cache: Arc<RwLock<Option<(std::time::Instant, DiskSpaceInfo)>>>,
    meta_save_lock: Arc<parking_lot::Mutex<()>>,
}

impl DownloadsManager {
    pub fn new(config: EngineConfig, torrent_mgr: Arc<TorrentEngineManager>) -> Self {
        let db = Self::load_downloads_meta(&config.downloads_meta_path);
        let (sse_tx, _) = broadcast::channel(64);
        Self {
            config,
            torrent_mgr,
            db: Arc::new(RwLock::new(db)),
            active_downloads: Arc::new(RwLock::new(HashMap::new())),
            sse_tx,
            last_broadcast_payload: Arc::new(RwLock::new(String::new())),
            auto_resume_handle: Arc::new(RwLock::new(None)),
            disk_space_cache: Arc::new(RwLock::new(None)),
            meta_save_lock: Arc::new(parking_lot::Mutex::new(())),
        }
    }

    fn load_downloads_meta(path: &Path) -> DownloadsDatabase {
        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(raw) => match serde_json::from_str::<DownloadsDatabase>(&raw) {
                    Ok(parsed) => return parsed,
                    Err(e) => {
                        warn!("[Downloads] Error loading meta: {}", e);
                        let backup = path.with_extension(format!("json.corrupted.{}", now_ms()));
                        if std::fs::rename(path, &backup).is_ok() {
                            info!(
                                "[Downloads] Corrupted meta backed up to: {}",
                                backup.display()
                            );
                        }
                    }
                },
                Err(e) => warn!("[Downloads] Could not read meta file: {}", e),
            }
        }
        DownloadsDatabase::default()
    }

    /// Atomically persists `downloads-meta.json` via a temporary file rename
    pub fn save_downloads_meta(&self) {
        let snapshot = self.db.read().clone();
        let path = self.config.downloads_meta_path.clone();
        let lock = Arc::clone(&self.meta_save_lock);
        Self::write_downloads_meta_snapshot(&snapshot, &path, &lock);
    }

    fn write_downloads_meta_snapshot(
        snapshot: &DownloadsDatabase,
        path: &Path,
        lock: &parking_lot::Mutex<()>,
    ) {
        let _guard = lock.lock();
        let temp_path = path.with_extension(format!("json.tmp.{}", now_ms()));
        if let Ok(json_str) = serde_json::to_string_pretty(snapshot) {
            if std::fs::write(&temp_path, &json_str).is_ok() {
                if std::fs::rename(&temp_path, path).is_err() {
                    let _ = std::fs::write(path, &json_str);
                    let _ = std::fs::remove_file(&temp_path);
                }
            }
        }
    }

    /// Offloads metadata persistence to a blocking thread so HTTP handlers return immediately.
    pub fn save_downloads_meta_async(&self) {
        let snapshot = self.db.read().clone();
        let path = self.config.downloads_meta_path.clone();
        let lock = Arc::clone(&self.meta_save_lock);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(move || {
                Self::write_downloads_meta_snapshot(&snapshot, &path, &lock);
            });
        } else {
            Self::write_downloads_meta_snapshot(&snapshot, &path, &lock);
        }
    }

    pub fn persist_download_meta(&self, download_id: &str, meta: &DownloadItemMeta) {
        {
            let mut guard = self.db.write();
            guard.downloads.insert(download_id.to_string(), meta.clone());
        }
        self.save_downloads_meta();
    }

    pub fn persist_download_meta_async(&self, download_id: &str, meta: &DownloadItemMeta) {
        {
            let mut guard = self.db.write();
            guard.downloads.insert(download_id.to_string(), meta.clone());
        }
        self.save_downloads_meta_async();
    }

    pub fn get_disk_space(&self) -> Option<DiskSpaceInfo> {
        {
            let cache_guard = self.disk_space_cache.read();
            if let Some((captured_at, ref cached)) = *cache_guard {
                if captured_at.elapsed() < Duration::from_secs(15) {
                    return Some(cached.clone());
                }
            }
        }

        let disks = Disks::new_with_refreshed_list();
        let target_dir = &self.config.completed_downloads_dir;
        let mut best_match: Option<&sysinfo::Disk> = None;
        let mut best_len = 0usize;

        for disk in disks.list() {
            let mount = disk.mount_point();
            if target_dir.starts_with(mount) {
                let len = mount.as_os_str().len();
                if len >= best_len {
                    best_len = len;
                    best_match = Some(disk);
                }
            }
        }

        let disk = best_match.or_else(|| disks.list().first())?;
        let total = disk.total_space();
        let free = disk.available_space();
        let info = DiskSpaceInfo {
            free,
            total,
            used: total.saturating_sub(free),
        };
        *self.disk_space_cache.write() = Some((std::time::Instant::now(), info.clone()));
        Some(info)
    }

    pub fn get_downloads_payload(&self) -> DownloadsPayload {
        let db_guard = self.db.read();
        let active_guard = self.active_downloads.read();
        let mut result = Vec::with_capacity(db_guard.downloads.len());

        let active_engines_guard = self.torrent_mgr.active_engines.read();
        for (download_id, entry) in &db_guard.downloads {
            if let Some(active) = active_guard.get(download_id) {
                let mut item = active.meta.clone();
                if let Some(eng) = active_engines_guard.get(&item.hash) {
                    if let Some(ref vf) = eng.video_file {
                        if item.file_name.is_none() {
                            item.file_name = Some(vf.name.clone());
                        }
                        if item.file_size == 0 {
                            item.file_size = vf.length;
                        }
                        if item.status == "loading" {
                            item.status = "downloading".to_string();
                        }
                    }
                    if item.status == "downloading" {
                        item.progress = eng.progress.max(item.progress);
                        item.speed = eng.download_speed_bytes;
                        item.peers = eng.peers;
                        if item.speed > 0 || item.peers > 0 || item.progress > 0.0 {
                            item.is_stalled = false;
                        }
                    }
                }
                if item.downloaded_bytes == 0 && item.file_size > 0 {
                    item.downloaded_bytes = (item.progress * item.file_size as f64).round() as u64;
                }
                result.push(item);
            } else {
                let mut item = entry.clone();
                let is_completed = item.status == "completed";
                item.progress = if is_completed { 1.0 } else { item.progress };
                item.speed = 0;
                item.peers = 0;
                item.eta = None;
                item.is_stalled = false;
                item.downloaded_bytes = if is_completed { item.file_size } else { 0 };
                result.push(item);
            }
        }

        DownloadsPayload {
            downloads: result,
            settings: db_guard.settings.clone(),
            disk_space: self.get_disk_space(),
            download_dir: self
                .config
                .completed_downloads_dir
                .to_string_lossy()
                .to_string(),
        }
    }

    pub fn broadcast_download_events(&self) {
        if self.sse_tx.receiver_count() == 0 {
            return;
        }
        if let Ok(payload_str) = serde_json::to_string(&self.get_downloads_payload()) {
            {
                let mut last = self.last_broadcast_payload.write();
                if *last == payload_str {
                    return;
                }
                *last = payload_str.clone();
            }
            let _ = self.sse_tx.send(payload_str);
        }
    }

    /// Downloads external subtitles from `https://decco.tv/api/subtitles/external`
    /// and saves them as `<baseName>.<lang>.srt` next to the video file for VLC compatibility.
    pub async fn download_subtitles(
        imdb_id: &str,
        season: u32,
        episode: u32,
        video_file_path: &Path,
    ) -> Vec<SubtitleEntry> {
        if imdb_id.trim().is_empty() {
            return Vec::new();
        }

        let url = format!(
            "https://decco.tv/api/subtitles/external?imdbId={}&season={}&episode={}",
            imdb_id.trim(),
            season,
            episode
        );
        info!("[Downloads] Fetching subtitles from: {}", url);

        let Ok(client) = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        else {
            return Vec::new();
        };

        let Ok(resp) = client.get(&url).send().await else {
            return Vec::new();
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        let Ok(json_val) = resp.json::<serde_json::Value>().await else {
            return Vec::new();
        };

        let subs_arr = json_val
            .get("subtitles")
            .and_then(|s| s.as_array())
            .or_else(|| json_val.as_array());

        let Some(subtitles) = subs_arr else {
            return Vec::new();
        };

        let base_name = video_file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("video");
        let video_dir = video_file_path
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let _ = std::fs::create_dir_all(video_dir);

        let mut saved_subs = Vec::new();
        for sub in subtitles {
            let sub_url = sub
                .get("url")
                .or_else(|| sub.get("src"))
                .and_then(|v| v.as_str());
            let Some(sub_url) = sub_url else { continue };

            let lang = sub
                .get("lang")
                .or_else(|| sub.get("language"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let label = sub
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or(lang);

            if let Ok(sub_res) = client.get(sub_url).send().await {
                if sub_res.status().is_success() {
                    if let Ok(content) = sub_res.text().await {
                        let srt_path = video_dir.join(format!("{}.{}.srt", base_name, lang));
                        if std::fs::write(&srt_path, content).is_ok() {
                            info!("[Downloads] Saved subtitle: {}", srt_path.display());
                            saved_subs.push(SubtitleEntry {
                                lang: lang.to_string(),
                                label: label.to_string(),
                                path: srt_path.to_string_lossy().to_string(),
                            });
                        }
                    }
                }
            }
        }

        saved_subs
    }

    /// Removes unwanted adjacent video files written by boundary pieces in multipack/season-pack torrents
    pub fn cleanup_unwanted_files(&self, download_dir: &Path, wanted_file_name: &str) {
        let mut wanted_files = HashSet::new();
        wanted_files.insert(wanted_file_name.to_string());

        let download_dir_str = download_dir.to_string_lossy().to_string();
        {
            let guard = self.db.read();
            for entry in guard.downloads.values() {
                if entry.download_dir == download_dir_str {
                    if let Some(ref fname) = entry.file_name {
                        wanted_files.insert(fname.clone());
                    }
                }
            }
        }

        fn scan_and_clean(dir: &Path, wanted: &HashSet<String>) {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        scan_and_clean(&path, wanted);
                    } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if is_video_file(name) && !wanted.contains(name) {
                            if std::fs::remove_file(&path).is_ok() {
                                info!("[Downloads] Cleaned up unwanted multipack file: {}", name);
                            }
                        }
                    }
                }
            }
        }

        scan_and_clean(download_dir, &wanted_files);
    }

    /// Starts queued downloads up to `settings.max_concurrent`
    pub fn pump_download_queue(self: &Arc<Self>) {
        let (max_concurrent, queued_items) = {
            let db_guard = self.db.read();
            let active_guard = self.active_downloads.read();
            let max_c = db_guard.settings.max_concurrent;

            let currently_downloading = active_guard
                .values()
                .filter(|dl| dl.meta.status == "downloading" || dl.meta.status == "loading")
                .count();

            if max_c > 0 && currently_downloading >= max_c {
                return;
            }

            let available_slots = if max_c == 0 {
                999
            } else {
                max_c.saturating_sub(currently_downloading)
            };

            let mut queued: Vec<DownloadItemMeta> = db_guard
                .downloads
                .values()
                .filter(|d| d.status == "queued" && !active_guard.contains_key(&d.id))
                .cloned()
                .collect();

            queued.sort_by_key(|d| d.queue_position.unwrap_or(d.started_at));
            queued.truncate(available_slots);
            (max_c, queued)
        };

        let _ = max_concurrent;
        for item in queued_items {
            info!("[Queue] Starting queued item: {} ({})", item.title, item.id);
            self.start_download(
                &item.hash,
                &item.title,
                &item.imdb_id,
                item.season,
                item.episode,
                item.file_idx,
                true,
            );
        }
    }

    /// Starts or queues a download, migrating cached streaming files if the user was already watching the stream
    pub fn start_download(
        self: &Arc<Self>,
        hash: &str,
        title: &str,
        imdb_id: &str,
        season: u32,
        episode: u32,
        file_idx: Option<usize>,
        bypass_queue: bool,
    ) -> DownloadItemMeta {
        self.start_download_with_poster(
            hash,
            title,
            imdb_id,
            "",
            season,
            episode,
            file_idx,
            bypass_queue,
        )
    }

    pub fn start_download_with_poster(
        self: &Arc<Self>,
        hash: &str,
        title: &str,
        imdb_id: &str,
        poster: &str,
        season: u32,
        episode: u32,
        file_idx: Option<usize>,
        bypass_queue: bool,
    ) -> DownloadItemMeta {
        let clean_hash = hash.trim().to_ascii_lowercase();
        let download_id = get_download_id(
            &clean_hash,
            if season > 0 { Some(season) } else { None },
            if episode > 0 { Some(episode) } else { None },
            file_idx,
        );

        if let Some(existing_active) = self.active_downloads.read().get(&download_id) {
            info!("[Downloads] Already downloading: {}", download_id);
            return existing_active.meta.clone();
        }

        let download_dir_path = self
            .config
            .completed_downloads_dir
            .join(&clean_hash[..clean_hash.len().min(16)]);
        let download_dir_str = download_dir_path.to_string_lossy().to_string();

        // Concurrency queue check
        if !bypass_queue {
            let (max_concurrent, currently_downloading, existing_entry) = {
                let db_guard = self.db.read();
                let active_guard = self.active_downloads.read();
                let count = active_guard
                    .iter()
                    .filter(|(k, dl)| {
                        *k != &download_id
                            && (dl.meta.status == "downloading" || dl.meta.status == "loading")
                    })
                    .count();
                (
                    db_guard.settings.max_concurrent,
                    count,
                    db_guard.downloads.get(&download_id).cloned(),
                )
            };

            if max_concurrent > 0 && currently_downloading >= max_concurrent {
                info!(
                    "[Downloads] Concurrency limit reached ({}/{}). Queueing {}",
                    currently_downloading, max_concurrent, title
                );
                let now = now_ms();
                let resolved_poster = if !poster.trim().is_empty() {
                    poster.trim().to_string()
                } else {
                    existing_entry
                        .as_ref()
                        .map(|e| e.poster.clone())
                        .unwrap_or_default()
                };
                let meta = DownloadItemMeta {
                    id: download_id.clone(),
                    hash: clean_hash,
                    title: if !title.is_empty() {
                        title.to_string()
                    } else {
                        existing_entry
                            .as_ref()
                            .map(|e| e.title.clone())
                            .unwrap_or_else(|| "Unknown".to_string())
                    },
                    imdb_id: imdb_id.to_string(),
                    poster: resolved_poster,
                    season,
                    episode,
                    file_idx,
                    file_name: existing_entry.as_ref().and_then(|e| e.file_name.clone()),
                    file_size: existing_entry.as_ref().map(|e| e.file_size).unwrap_or(0),
                    download_dir: download_dir_str,
                    subtitles: existing_entry
                        .as_ref()
                        .map(|e| e.subtitles.clone())
                        .unwrap_or_default(),
                    status: "queued".to_string(),
                    progress: existing_entry.as_ref().map(|e| e.progress).unwrap_or(0.0),
                    speed: 0,
                    peers: 0,
                    eta: None,
                    is_stalled: false,
                    downloaded_bytes: 0,
                    auto_paused_for_stream: false,
                    started_at: existing_entry.as_ref().map(|e| e.started_at).unwrap_or(now),
                    completed_at: None,
                    queue_position: Some(now),
                    duration: existing_entry.as_ref().and_then(|e| e.duration),
                    last_error: None,
                };
                self.persist_download_meta_async(&download_id, &meta);
                self.broadcast_download_events();
                return meta;
            }
        }

        // Hot upgrade: capture streaming engine path quickly (in-memory only); offload fs moves to background task
        let was_streaming = {
            let guard = self.torrent_mgr.active_engines.read();
            guard
                .get(&clean_hash)
                .map(|e| !e.is_mock)
                .unwrap_or(false)
        };
        let migration_src_path = if was_streaming {
            info!(
                "[Downloads] Found active streaming engine for {}. Upgrading to permanent download...",
                clean_hash
            );
            self.torrent_mgr.destroy_engine(&clean_hash)
        } else {
            None
        };

        let existing_meta = self.db.read().downloads.get(&download_id).cloned();
        if let Some(ref existing) = existing_meta {
            if existing.status == "completed" {
                if find_exact_video_file(
                    Path::new(&existing.download_dir),
                    existing.file_name.as_deref(),
                )
                .is_some()
                {
                    info!("[Downloads] Already completed on disk: {}", download_id);
                    return existing.clone();
                }
            }
        }
        let is_already_completed = false;

        let resolved_poster = if !poster.trim().is_empty() {
            poster.trim().to_string()
        } else {
            existing_meta
                .as_ref()
                .map(|m| m.poster.clone())
                .unwrap_or_default()
        };

        let now = now_ms();
        let meta = DownloadItemMeta {
            id: download_id.clone(),
            hash: clean_hash.clone(),
            title: if !title.is_empty() {
                title.to_string()
            } else {
                "Unknown".to_string()
            },
            imdb_id: imdb_id.to_string(),
            poster: resolved_poster,
            season,
            episode,
            file_idx,
            file_name: existing_meta.as_ref().and_then(|m| m.file_name.clone()),
            file_size: existing_meta.as_ref().map(|m| m.file_size).unwrap_or(0),
            download_dir: download_dir_str,
            subtitles: existing_meta
                .as_ref()
                .map(|m| m.subtitles.clone())
                .unwrap_or_default(),
            status: if is_already_completed {
                "completed".to_string()
            } else {
                "loading".to_string()
            },
            progress: if is_already_completed { 1.0 } else { 0.0 },
            speed: 0,
            peers: 0,
            eta: None,
            is_stalled: false,
            downloaded_bytes: 0,
            auto_paused_for_stream: false,
            started_at: existing_meta.as_ref().map(|m| m.started_at).unwrap_or(now),
            completed_at: existing_meta.as_ref().and_then(|m| m.completed_at),
            queue_position: None,
            duration: existing_meta.as_ref().and_then(|m| m.duration),
            last_error: None,
        };

        // Update state and notify listeners immediately before any filesystem or torrent I/O
        self.persist_download_meta_async(&download_id, &meta);
        self.active_downloads.write().insert(
            download_id.clone(),
            ActiveDownloadTask {
                meta: meta.clone(),
                poll_handle: None,
                stalled_handle: None,
            },
        );
        self.broadcast_download_events();

        // Stalled detection timer (45s)
        let dl_mgr_stalled = Arc::clone(self);
        let dl_id_stalled = download_id.clone();
        let stalled_handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(STALLED_TIMEOUT_MS)).await;
            let mut broadcast = false;
            {
                let mut guard = dl_mgr_stalled.active_downloads.write();
                if let Some(active) = guard.get_mut(&dl_id_stalled) {
                    if (active.meta.status == "downloading" || active.meta.status == "loading")
                        && active.meta.speed == 0
                        && active.meta.peers == 0
                        && active.meta.progress == 0.0
                    {
                        active.meta.is_stalled = true;
                        broadcast = true;
                    }
                }
            }
            if broadcast {
                dl_mgr_stalled.broadcast_download_events();
            }
        });

        // Background download execution & 1s progress polling loop
        let dl_mgr = Arc::clone(self);
        let dl_id_poll = download_id.clone();
        let hash_poll = clean_hash.clone();
        let imdb_poll = imdb_id.to_string();
        let dl_dir_poll = download_dir_path.clone();

        let poll_handle = tokio::spawn(async move {
            let dl_dir_setup = dl_dir_poll.clone();
            let hash_setup = hash_poll.clone();
            let cache_mgr = dl_mgr.torrent_mgr.cache.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let _ = std::fs::create_dir_all(&dl_dir_setup);
                if let Some(src_path) = migration_src_path {
                    if src_path.exists() && src_path != dl_dir_setup {
                        if let Ok(entries) = std::fs::read_dir(&src_path) {
                            for entry in entries.flatten() {
                                let src_item = entry.path();
                                if let Some(name) = src_item.file_name() {
                                    let dest_item = dl_dir_setup.join(name);
                                    let _ = std::fs::rename(&src_item, &dest_item);
                                }
                            }
                        }
                        let _ = std::fs::remove_dir_all(&src_path);
                        cache_mgr.remove_torrent(&hash_setup);
                        info!("[Downloads] Moved cached stream files to permanent download dir.");
                    }
                }
            })
            .await;

            let _ = dl_mgr
                .torrent_mgr
                .start_or_get_engine(
                    &hash_poll,
                    file_idx,
                    if season > 0 { Some(season) } else { None },
                    if episode > 0 { Some(episode) } else { None },
                    Some(dl_dir_poll.clone()),
                )
                .await;

            let mut subs_fetched = false;

            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;

                let stream_state = dl_mgr
                    .torrent_mgr
                    .active_engines
                    .read()
                    .get(&hash_poll)
                    .cloned();

                let Some(eng) = stream_state else {
                    break;
                };

                if eng.metadata_ready && !subs_fetched {
                    subs_fetched = true;
                    if let Some(ref vf) = eng.video_file {
                        let dl_mgr_subs = Arc::clone(&dl_mgr);
                        let dl_id_subs = dl_id_poll.clone();
                        let imdb_subs = imdb_poll.clone();
                        let vf_path = vf.path.clone();
                        tokio::spawn(async move {
                            let subs = Self::download_subtitles(&imdb_subs, season, episode, &vf_path).await;
                            if !subs.is_empty() {
                                let mut guard = dl_mgr_subs.active_downloads.write();
                                if let Some(active) = guard.get_mut(&dl_id_subs) {
                                    active.meta.subtitles = subs;
                                    let meta_clone = active.meta.clone();
                                    drop(guard);
                                    dl_mgr_subs.persist_download_meta(&dl_id_subs, &meta_clone);
                                    dl_mgr_subs.broadcast_download_events();
                                }
                            }
                        });
                    }
                }

                let mut completed_now = false;
                let mut completed_file_name: Option<String> = None;
                let mut updated_meta: Option<DownloadItemMeta> = None;

                {
                    let mut guard = dl_mgr.active_downloads.write();
                    let Some(active) = guard.get_mut(&dl_id_poll) else {
                        break;
                    };

                    if let Some(ref vf) = eng.video_file {
                        active.meta.file_name = Some(vf.name.clone());
                        active.meta.file_size = vf.length;
                        if active.meta.status == "loading" {
                            active.meta.status = "downloading".to_string();
                        }
                    }

                    active.meta.progress = eng.progress;
                    active.meta.speed = eng.download_speed_bytes;
                    active.meta.peers = eng.peers;
                    active.meta.downloaded_bytes =
                        (eng.progress * active.meta.file_size as f64).round() as u64;

                    if active.meta.speed > 0 || active.meta.peers > 0 || active.meta.progress > 0.0
                    {
                        active.meta.is_stalled = false;
                    }

                    if active.meta.speed > 0
                        && active.meta.file_size > 0
                        && active.meta.progress < 1.0
                    {
                        let remaining = active
                            .meta
                            .file_size
                            .saturating_sub(active.meta.downloaded_bytes);
                        active.meta.eta = Some(remaining / active.meta.speed);
                    } else {
                        active.meta.eta = None;
                    }

                    if active.meta.progress >= 1.0 && active.meta.status == "downloading" {
                        active.meta.status = "completed".to_string();
                        active.meta.completed_at = Some(now_ms());
                        active.meta.progress = 1.0;
                        active.meta.eta = Some(0);
                        completed_now = true;
                        completed_file_name = active.meta.file_name.clone();
                        updated_meta = Some(active.meta.clone());
                    }
                }

                if completed_now {
                    if let Some(meta_to_save) = updated_meta {
                        info!("[Downloads] COMPLETED: {}", meta_to_save.title);
                        dl_mgr.persist_download_meta(&dl_id_poll, &meta_to_save);
                    }
                    if let Some(fname) = completed_file_name {
                        dl_mgr.cleanup_unwanted_files(&dl_dir_poll, &fname);
                    }
                    dl_mgr.broadcast_download_events();
                    dl_mgr.pump_download_queue();
                    break;
                } else {
                    dl_mgr.broadcast_download_events();
                }
            }
        });

        if let Some(active) = self.active_downloads.write().get_mut(&download_id) {
            active.poll_handle = Some(poll_handle);
            active.stalled_handle = Some(stalled_handle);
        }

        meta
    }

    /// Resolves a download ID by exact ID or by matching torrent hash
    fn resolve_download_id(&self, id_or_hash: &str) -> Option<String> {
        let clean = id_or_hash.trim();
        let db_guard = self.db.read();
        if db_guard.downloads.contains_key(clean) {
            return Some(clean.to_string());
        }
        let lower = clean.to_ascii_lowercase();
        for (k, v) in &db_guard.downloads {
            if v.hash.to_ascii_lowercase() == lower {
                return Some(k.clone());
            }
        }
        None
    }

    pub fn pause_download(&self, id_or_hash: &str) -> Option<DownloadItemMeta> {
        let download_id = self.resolve_download_id(id_or_hash)?;
        let mut active_opt = self.active_downloads.write().remove(&download_id);

        if let Some(ref mut active) = active_opt {
            if let Some(h) = active.poll_handle.take() {
                h.abort();
            }
            if let Some(h) = active.stalled_handle.take() {
                h.abort();
            }
            self.torrent_mgr.destroy_engine(&active.meta.hash);
            active.meta.status = "paused".to_string();
            active.meta.auto_paused_for_stream = false;
            {
                let mut db_guard = self.db.write();
                db_guard
                    .downloads
                    .insert(download_id.clone(), active.meta.clone());
            }
            self.broadcast_download_events();
            self.save_downloads_meta_async();
            return Some(active.meta.clone());
        }

        let mut db_guard = self.db.write();
        if let Some(saved) = db_guard.downloads.get_mut(&download_id) {
            saved.status = "paused".to_string();
            saved.auto_paused_for_stream = false;
            let cloned = saved.clone();
            drop(db_guard);
            self.broadcast_download_events();
            self.save_downloads_meta_async();
            return Some(cloned);
        }
        None
    }

    pub fn resume_download(self: &Arc<Self>, id_or_hash: &str) -> Option<DownloadItemMeta> {
        let download_id = self.resolve_download_id(id_or_hash)?;
        let saved = self.db.read().downloads.get(&download_id).cloned()?;
        let meta = self.start_download(
            &saved.hash,
            &saved.title,
            &saved.imdb_id,
            saved.season,
            saved.episode,
            saved.file_idx,
            false,
        );
        self.broadcast_download_events();
        Some(meta)
    }

    pub fn force_start_download(self: &Arc<Self>, id_or_hash: &str) -> Option<DownloadItemMeta> {
        let download_id = self.resolve_download_id(id_or_hash)?;
        let saved = self.db.read().downloads.get(&download_id).cloned()?;
        let meta = self.start_download(
            &saved.hash,
            &saved.title,
            &saved.imdb_id,
            saved.season,
            saved.episode,
            saved.file_idx,
            true,
        );
        self.broadcast_download_events();
        Some(meta)
    }

    pub fn delete_download(&self, id_or_hash: &str) -> (String, String) {
        let download_id = self
            .resolve_download_id(id_or_hash)
            .unwrap_or_else(|| id_or_hash.to_string());

        let entry = self.db.write().downloads.remove(&download_id);
        if let Some(mut active) = self.active_downloads.write().remove(&download_id) {
            if let Some(h) = active.poll_handle.take() {
                h.abort();
            }
            if let Some(h) = active.stalled_handle.take() {
                h.abort();
            }
            self.torrent_mgr.destroy_engine(&active.meta.hash);
        }

        let hash_ret = entry
            .as_ref()
            .map(|e| e.hash.clone())
            .unwrap_or_else(|| id_or_hash.to_string());

        // Broadcast SSE removal and return HTTP response immediately before disk cleanup
        self.broadcast_download_events();
        self.save_downloads_meta_async();

        if let Some(item) = entry {
            let other_shares_hash = self
                .db
                .read()
                .downloads
                .values()
                .any(|d| d.hash == item.hash && d.id != item.id);

            if !other_shares_hash {
                self.torrent_mgr.destroy_engine(&item.hash);
            }

            let dl_dir = PathBuf::from(&item.download_dir);
            let file_name = item.file_name.clone();
            let subtitles = item.subtitles.clone();

            let cleanup_task = move || {
                if other_shares_hash {
                    if let Some(fname) = file_name {
                        let _ = std::fs::remove_file(dl_dir.join(fname));
                    }
                    for sub in subtitles {
                        let _ = std::fs::remove_file(&sub.path);
                    }
                } else {
                    for attempt in 1..=5 {
                        if !dl_dir.exists() || std::fs::remove_dir_all(&dl_dir).is_ok() {
                            info!(
                                "[Downloads] Deleted folder on attempt {}: {}",
                                attempt,
                                dl_dir.display()
                            );
                            break;
                        }
                        std::thread::sleep(Duration::from_secs(attempt));
                    }
                }
            };

            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn_blocking(cleanup_task);
            } else {
                cleanup_task();
            }
        }

        (hash_ret, download_id)
    }

    pub fn pause_all_downloads(&self) -> usize {
        let mut count = 0usize;
        let active_keys: Vec<String> = self.active_downloads.read().keys().cloned().collect();
        for dl_id in active_keys {
            if let Some(mut active) = self.active_downloads.write().remove(&dl_id) {
                if let Some(h) = active.poll_handle.take() {
                    h.abort();
                }
                if let Some(h) = active.stalled_handle.take() {
                    h.abort();
                }
                self.torrent_mgr.destroy_engine(&active.meta.hash);
                active.meta.status = "paused".to_string();
                active.meta.auto_paused_for_stream = false;
                self.db.write().downloads.insert(dl_id, active.meta);
                count += 1;
            }
        }

        {
            let mut db_guard = self.db.write();
            for item in db_guard.downloads.values_mut() {
                if item.status == "queued" {
                    item.status = "paused".to_string();
                    count += 1;
                }
            }
        }

        self.save_downloads_meta();
        self.broadcast_download_events();
        count
    }

    pub fn resume_all_downloads(self: &Arc<Self>) -> usize {
        let mut count = 0usize;
        let now = now_ms();
        {
            let mut db_guard = self.db.write();
            for item in db_guard.downloads.values_mut() {
                if item.status == "paused" {
                    item.status = "queued".to_string();
                    item.queue_position = Some(now + count as u64);
                    count += 1;
                }
            }
        }
        self.save_downloads_meta();
        self.pump_download_queue();
        self.broadcast_download_events();
        count
    }

    pub fn clear_completed_downloads(&self) -> usize {
        let mut count = 0usize;
        {
            let mut db_guard = self.db.write();
            db_guard.downloads.retain(|_, item| {
                if item.status == "completed" {
                    count += 1;
                    false
                } else {
                    true
                }
            });
        }
        self.save_downloads_meta();
        self.broadcast_download_events();
        count
    }

    pub fn update_settings(
        self: &Arc<Self>,
        max_concurrent: Option<usize>,
        auto_pause_on_playback: Option<bool>,
    ) -> DownloadSettings {
        let updated = {
            let mut db_guard = self.db.write();
            if let Some(mc) = max_concurrent {
                db_guard.settings.max_concurrent = mc;
            }
            if let Some(ap) = auto_pause_on_playback {
                db_guard.settings.auto_pause_on_playback = ap;
            }
            db_guard.settings.clone()
        };
        self.save_downloads_meta();
        self.pump_download_queue();
        self.broadcast_download_events();
        updated
    }

    /// Returns all torrent hashes belonging to active or completed downloads (so cache cleanup/stream switching preserves them)
    pub fn protected_download_hashes(&self) -> HashSet<String> {
        let mut hashes: HashSet<String> = self
            .active_downloads
            .read()
            .values()
            .map(|dl| dl.meta.hash.clone())
            .collect();
        let db_guard = self.db.read();
        for item in db_guard.downloads.values() {
            if item.status == "completed" {
                hashes.insert(item.hash.clone());
            }
        }
        hashes
    }

    /// Pauses competing streams and background downloads when user starts watching `target_hash`
    pub fn pause_competing_streams_and_downloads(
        &self,
        target_hash: &str,
        target_download_id: Option<&str>,
    ) {
        let clean_target = target_hash.trim().to_ascii_lowercase();
        if let Some(handle) = self.auto_resume_handle.write().take() {
            handle.abort();
        }

        let shared_hashes = self.protected_download_hashes();

        self.torrent_mgr
            .stop_competing_streams(&clean_target, &shared_hashes);

        let auto_pause_enabled = self.db.read().settings.auto_pause_on_playback;
        if !auto_pause_enabled {
            return;
        }

        let mut paused_any = false;
        let keys: Vec<String> = self.active_downloads.read().keys().cloned().collect();
        for dl_id in keys {
            if Some(dl_id.as_str()) == target_download_id {
                continue;
            }
            let should_pause = self
                .active_downloads
                .read()
                .get(&dl_id)
                .map(|dl| {
                    dl.meta.hash != clean_target
                        && (dl.meta.status == "downloading" || dl.meta.status == "loading")
                })
                .unwrap_or(false);

            if should_pause {
                if let Some(mut active) = self.active_downloads.write().remove(&dl_id) {
                    info!(
                        "[Downloads] Auto-pausing background download \"{}\" ({}) to maximize bandwidth for active playback ({})",
                        active.meta.title, dl_id, clean_target
                    );
                    if let Some(h) = active.poll_handle.take() {
                        h.abort();
                    }
                    if let Some(h) = active.stalled_handle.take() {
                        h.abort();
                    }
                    self.torrent_mgr.destroy_engine(&active.meta.hash);
                    active.meta.status = "paused".to_string();
                    active.meta.auto_paused_for_stream = true;
                    self.persist_download_meta(&dl_id, &active.meta);
                    paused_any = true;
                }
            }
        }

        if paused_any {
            self.broadcast_download_events();
        }
    }

    /// Debounces stream stop by 6s (to survive React StrictMode remounts) and auto-resumes paused downloads
    pub fn stop_stream_and_resume_auto_paused(self: &Arc<Self>, stopped_hash: &str) {
        let normalized = stopped_hash.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return;
        }
        self.torrent_mgr.cancel_stop_grace_timeout(&normalized);

        let dl_mgr = Arc::clone(self);
        let hash_clone = normalized.clone();
        let grace_handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(STOP_GRACE_PERIOD_MS)).await;
            dl_mgr
                .torrent_mgr
                .stop_grace_timers
                .write()
                .remove(&hash_clone);

            let is_shared = dl_mgr
                .active_downloads
                .read()
                .values()
                .any(|dl| dl.meta.hash == hash_clone);

            if !is_shared {
                dl_mgr.torrent_mgr.destroy_engine(&hash_clone);
                info!(
                    "[Engine] Stopped streaming engine for {} (playback exited after grace period)",
                    hash_clone
                );
            }

            {
                let mut active_stream = dl_mgr.torrent_mgr.active_streaming_hash.write();
                if active_stream.as_deref() == Some(hash_clone.as_str()) {
                    *active_stream = None;
                }
            }

            let dl_mgr_resume = Arc::clone(&dl_mgr);
            let resume_task = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(AUTO_RESUME_DELAY_MS)).await;
                if dl_mgr_resume
                    .torrent_mgr
                    .active_streaming_hash
                    .read()
                    .is_some()
                {
                    return;
                }

                let to_resume: Vec<DownloadItemMeta> = {
                    let mut db_guard = dl_mgr_resume.db.write();
                    let mut items = Vec::new();
                    for saved in db_guard.downloads.values_mut() {
                        if saved.status == "paused" && saved.auto_paused_for_stream {
                            saved.auto_paused_for_stream = false;
                            items.push(saved.clone());
                        }
                    }
                    items
                };

                if !to_resume.is_empty() {
                    dl_mgr_resume.save_downloads_meta();
                    for item in to_resume {
                        info!(
                            "[Downloads] Auto-resuming download \"{}\" ({}) now that playback stopped",
                            item.title, item.id
                        );
                        dl_mgr_resume.start_download(
                            &item.hash,
                            &item.title,
                            &item.imdb_id,
                            item.season,
                            item.episode,
                            item.file_idx,
                            false,
                        );
                    }
                    dl_mgr_resume.broadcast_download_events();
                }
            });

            *dl_mgr.auto_resume_handle.write() = Some(resume_task);
        });

        self.torrent_mgr
            .stop_grace_timers
            .write()
            .insert(normalized, grace_handle);
    }

    /// Restores in-progress downloads on engine startup
    pub fn restore_downloads(self: &Arc<Self>) {
        let items_to_restore: Vec<DownloadItemMeta> = {
            let mut db_guard = self.db.write();
            let mut list = Vec::new();
            for saved in db_guard.downloads.values_mut() {
                if saved.status == "downloading"
                    || saved.status == "loading"
                    || (saved.status == "paused" && saved.auto_paused_for_stream)
                {
                    saved.auto_paused_for_stream = false;
                    list.push(saved.clone());
                }
            }
            list
        };

        info!(
            "[Downloads] Restoring {} active downloads from saved database...",
            items_to_restore.len()
        );

        for item in items_to_restore {
            self.start_download(
                &item.hash,
                &item.title,
                &item.imdb_id,
                item.season,
                item.episode,
                item.file_idx,
                false,
            );
        }

        self.seed_completed_downloads();
    }

    /// Re-announces completed downloads to the P2P swarm so they continue seeding after restart
    pub fn seed_completed_downloads(self: &Arc<Self>) {
        let completed_items: Vec<DownloadItemMeta> = {
            let db_guard = self.db.read();
            db_guard
                .downloads
                .values()
                .filter(|d| d.status == "completed")
                .cloned()
                .collect()
        };

        if completed_items.is_empty() {
            return;
        }

        info!(
            "[Seeding] Re-announcing {} completed downloads to P2P network...",
            completed_items.len()
        );

        let mgr = Arc::clone(&self.torrent_mgr);
        tokio::spawn(async move {
            for item in completed_items {
                let dl_dir = PathBuf::from(&item.download_dir);
                if find_exact_video_file(&dl_dir, item.file_name.as_deref()).is_none() {
                    continue;
                }
                let season = if item.season > 0 { Some(item.season) } else { None };
                let episode = if item.episode > 0 { Some(item.episode) } else { None };
                mgr.start_or_get_engine(&item.hash, item.file_idx, season, episode, Some(dl_dir))
                    .await;
            }
        });
    }

    pub fn find_completed_video_path(&self, id_or_hash: &str) -> Option<(DownloadItemMeta, PathBuf)> {
        let dl_id = self.resolve_download_id(id_or_hash)?;
        let entry = self.db.read().downloads.get(&dl_id).cloned()?;
        if entry.status != "completed" {
            return None;
        }
        let path = find_exact_video_file(
            Path::new(&entry.download_dir),
            entry.file_name.as_deref(),
        )?;
        Some((entry, path))
    }
}

