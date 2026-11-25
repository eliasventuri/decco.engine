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
use tokio::sync::watch;
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
        let (shutdown, _) = watch::channel(false);
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(12))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            state: AppState {
                torrent_mgr,
                downloads_mgr,
                cache_mgr,
                shutdown,
                auth_token: None,
                http_client,
            },
        }
    }

    pub fn new_with_auth(config: EngineConfig, auth_token: String) -> Self {
        let mut engine = Self::new(config);
        engine.state.auth_token = Some(auth_token);
        engine
    }

    /// Starts the multi-port HTTP server and background workers
    pub async fn start(&self) -> anyhow::Result<Vec<u16>> {
        // Pre-warm hardware encoder detection in background (Desktop only; Android uses native ExoPlayer)
        #[cfg(not(target_os = "android"))]
        tokio::spawn(async {
            let _ = crate::transcode::detect_hardware_encoder().await;
        });

        // Pre-warm BitTorrent session & DHT routing table in background so first play starts instantly
        #[cfg(feature = "rqbit-backend")]
        {
            let mgr = Arc::clone(&self.state.torrent_mgr);
            tokio::spawn(async move {
                let _ = mgr.get_or_init_session().await;
            });
        }

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
        let protected = self.state.downloads_mgr.protected_download_hashes();
        self.state.torrent_mgr.destroy_engines_except(&protected);
        self.state.cache_mgr.clear_all_streaming_cache()
    }

    /// Stops active download/stream work while keeping the engine object reusable.
    pub fn shutdown(&self) {
        self.state.torrent_mgr.destroy_all_engines();
        self.state.downloads_mgr.pause_all_downloads();
        let _ = self.state.shutdown.send(true);
    }
}

