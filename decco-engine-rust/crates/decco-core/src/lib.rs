pub mod cache;
pub mod config;
pub mod downloads;
pub mod live_proxy;
pub mod server;
pub mod torrent;
pub mod transcode;

use crate::cache::CacheManager;
use crate::config::EngineConfig;
use crate::downloads::DownloadsManager;
use crate::server::{start_multiport_server, AppState};
use crate::torrent::TorrentEngineManager;
use regex::Regex;
use std::sync::Arc;
use tracing::info;

#[derive(Clone)]
pub struct DeccoEngine {
    pub state: AppState,
}

impl DeccoEngine {
    pub fn new(config: EngineConfig) -> Self {
        let cache_mgr = Arc::new(CacheManager::new(config.clone()));
        let torrent_mgr = Arc::new(TorrentEngineManager::new(
            config.clone(),
            Arc::clone(&cache_mgr),
        ));
        let downloads_mgr = Arc::new(DownloadsManager::new(
            config,
            Arc::clone(&torrent_mgr),
        ));

        Self {
            state: AppState {
                torrent_mgr,
                downloads_mgr,
                cache_mgr,
            },
        }
    }

    /// Starts the multi-port HTTP server and background workers
    pub async fn start(&self) -> anyhow::Result<Vec<u16>> {
        // Pre-warm hardware encoder detection in background
        tokio::spawn(async {
            let _ = crate::transcode::detect_hardware_encoder().await;
        });

        start_multiport_server(self.state.clone()).await
    }

    /// Parses and handles custom `decco://<hash>?fileIdx=...&season=...&episode=...` protocol links
    pub async fn handle_protocol_link(&self, url: &str) {
        let hash_re = Regex::new(r"decco://([a-fA-F0-9]+)").unwrap();
        let file_idx_re = Regex::new(r"fileIdx=(\d+)").unwrap();
        let season_re = Regex::new(r"season=(\d+)").unwrap();
        let episode_re = Regex::new(r"episode=(\d+)").unwrap();
        let path_season_re = Regex::new(r"(?i)-s(\d+)").unwrap();
        let path_episode_re = Regex::new(r"(?i)-e(\d+)").unwrap();

        if let Some(caps) = hash_re.captures(url) {
            let hash = caps[1].to_ascii_lowercase();
            let file_idx = file_idx_re
                .captures(url)
                .and_then(|c| c[1].parse::<usize>().ok());
            let season = season_re
                .captures(url)
                .or_else(|| path_season_re.captures(url))
                .and_then(|c| c[1].parse::<u32>().ok());
            let episode = episode_re
                .captures(url)
                .or_else(|| path_episode_re.captures(url))
                .and_then(|c| c[1].parse::<u32>().ok());

            info!(
                "[Protocol] Handling link - hash: {}, fileIdx: {:?}, S{:?}E{:?}",
                hash, file_idx, season, episode
            );
            self.state
                .torrent_mgr
                .start_or_get_engine(&hash, file_idx, season, episode, None)
                .await;
        }
    }

    pub fn clear_cache(&self) -> anyhow::Result<()> {
        self.state.torrent_mgr.active_engines.write().clear();
        self.state.cache_mgr.clear_all_streaming_cache()
    }
}

