use crate::config::{now_ms, EngineConfig, CACHE_MAX_AGE_MS};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedTorrentEntry {
    pub added_at: u64,
    pub last_accessed: u64,
    pub file_idx: Option<usize>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheMeta {
    pub torrents: HashMap<String, CachedTorrentEntry>,
}

pub struct CacheManager {
    config: EngineConfig,
    meta: Arc<RwLock<CacheMeta>>,
}

impl CacheManager {
    pub fn new(config: EngineConfig) -> Self {
        let meta = Self::load_from_disk(&config.cache_meta_path);
        info!(
            "[Cache] {} cached streaming torrents available on disk (idle until played).",
            meta.torrents.len()
        );
        Self {
            config,
            meta: Arc::new(RwLock::new(meta)),
        }
    }

    fn load_from_disk(path: &PathBuf) -> CacheMeta {
        if path.exists() {
            if let Ok(raw) = std::fs::read_to_string(path) {
                if let Ok(parsed) = serde_json::from_str::<CacheMeta>(&raw) {
                    return parsed;
                }
            }
        }
        CacheMeta::default()
    }

    pub fn save_to_disk(&self) {
        let snapshot = self.meta.read().clone();
        let path = self.config.cache_meta_path.clone();
        let tmp_path = path.with_extension("json.tmp");
        if let Ok(serialized) = serde_json::to_string_pretty(&snapshot) {
            if std::fs::write(&tmp_path, serialized).is_ok() {
                let _ = std::fs::rename(&tmp_path, &path);
            }
        }
    }

    pub fn add_torrent(
        &self,
        hash: &str,
        file_idx: Option<usize>,
        season: Option<u32>,
        episode: Option<u32>,
    ) {
        let clean_hash = hash.trim().to_ascii_lowercase();
        let now = now_ms();
        {
            let mut guard = self.meta.write();
            guard.torrents.insert(
                clean_hash.clone(),
                CachedTorrentEntry {
                    added_at: now,
                    last_accessed: now,
                    file_idx,
                    season,
                    episode,
                },
            );
        }
        self.save_to_disk();
        info!("[Cache] Added torrent {} to cache meta", clean_hash);
    }

    pub fn update_access(&self, hash: &str) {
        let clean_hash = hash.trim().to_ascii_lowercase();
        let now = now_ms();
        let mut should_persist = false;
        {
            let mut guard = self.meta.write();
            if let Some(entry) = guard.torrents.get_mut(&clean_hash) {
                // Only write to disk if more than 60s have passed since last update
                if now.saturating_sub(entry.last_accessed) > 60_000 {
                    should_persist = true;
                }
                entry.last_accessed = now;
            }
        }
        if should_persist {
            self.save_to_disk();
        }
    }

    pub fn remove_torrent(&self, hash: &str) {
        let clean_hash = hash.trim().to_ascii_lowercase();
        {
            let mut guard = self.meta.write();
            guard.torrents.remove(&clean_hash);
        }
        self.save_to_disk();
        info!("[Cache] Removed torrent {} from cache meta", clean_hash);
    }

    /// Returns hashes that have expired past the 72h TTL and removes them from metadata
    pub fn collect_expired_hashes(&self) -> Vec<String> {
        let now = now_ms();
        let mut expired = Vec::new();
        {
            let mut guard = self.meta.write();
            guard.torrents.retain(|hash, entry| {
                let age = now.saturating_sub(entry.last_accessed);
                if age > CACHE_MAX_AGE_MS {
                    info!(
                        "[Cache] Cleaned old torrent: {} (age: {}h)",
                        hash,
                        age / 3_600_000
                    );
                    expired.push(hash.clone());
                    false
                } else {
                    true
                }
            });
        }
        if !expired.is_empty() {
            self.save_to_disk();
        }
        expired
    }

    /// Cleans orphaned directories in the streaming cache folder
    pub fn clean_orphaned_files(&self, active_hashes: &HashSet<String>) {
        let meta_hashes: HashSet<String> = {
            let guard = self.meta.read();
            guard.torrents.keys().cloned().collect()
        };

        if let Ok(entries) = std::fs::read_dir(&self.config.streaming_cache_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let dir_name = match path.file_name().and_then(|n| n.to_str()) {
                        Some(n) => n.to_string(),
                        None => continue,
                    };
                    if dir_name == "torrent-stream" {
                        continue;
                    }
                    let prefix: String = dir_name.chars().take(20).collect();
                    let is_active = active_hashes.iter().any(|h| h.contains(&prefix));
                    let in_meta = meta_hashes.iter().any(|h| h.contains(&prefix));

                    if !is_active && !in_meta {
                        if let Err(e) = std::fs::remove_dir_all(&path) {
                            warn!("[Cache] Error deleting orphaned dir {}: {}", dir_name, e);
                        } else {
                            info!("[Cache] Deleted orphaned directory: {}", dir_name);
                        }
                    }
                }
            }
        }
    }

    /// Clears all streaming cache files without touching completed-downloads
    pub fn clear_all_streaming_cache(&self) -> anyhow::Result<()> {
        info!("[Cache] Clearing streaming cache only (downloads are NOT affected)...");
        {
            let mut guard = self.meta.write();
            guard.torrents.clear();
        }
        self.save_to_disk();

        let protected = ["cache-meta.json", "decco-engine.log", "update-info.json"];
        if let Ok(entries) = std::fs::read_dir(&self.config.streaming_cache_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if !protected.contains(&name) {
                        if path.is_dir() {
                            let _ = std::fs::remove_dir_all(&path);
                        } else {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
            }
        }
        info!("[Cache] Streaming cache cleared successfully");
        Ok(())
    }
}

