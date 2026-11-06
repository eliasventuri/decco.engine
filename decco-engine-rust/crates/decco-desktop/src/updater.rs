use decco_core::config::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

const GITHUB_RELEASES_LATEST_API: &str =
    "https://api.github.com/repos/eliasventuri/decco.engine/releases/latest";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub last_updated: Option<u64>,
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct UpdaterState {
    pub info_path: PathBuf,
    pub current_version: String,
    pub update_ready: Arc<RwLock<bool>>,
    pub downloaded_installer: Arc<RwLock<Option<PathBuf>>>,
}

impl UpdaterState {
    pub fn new(info_path: PathBuf) -> Self {
        let current_version = env!("CARGO_PKG_VERSION").to_string();
        let state = Self {
            info_path,
            current_version,
            update_ready: Arc::new(RwLock::new(false)),
            downloaded_installer: Arc::new(RwLock::new(None)),
        };
        state.check_version_change();
        state
    }

    pub fn load_update_info(&self) -> UpdateInfo {
        if self.info_path.exists() {
            if let Ok(raw) = std::fs::read_to_string(&self.info_path) {
                if let Ok(parsed) = serde_json::from_str::<UpdateInfo>(&raw) {
                    return parsed;
                }
            }
        }
        UpdateInfo {
            last_updated: None,
            version: self.current_version.clone(),
        }
    }

    pub fn save_update_info(&self, info: &UpdateInfo) {
        if let Ok(json_str) = serde_json::to_string_pretty(info) {
            let _ = std::fs::write(&self.info_path, json_str);
        }
    }

    /// Updates `update-info.json` timestamp when the engine version changes or on first run
    pub fn check_version_change(&self) {
        let mut info = self.load_update_info();
        if info.version != self.current_version || info.last_updated.is_none() {
            info.version = self.current_version.clone();
            info.last_updated = Some(now_ms());
            self.save_update_info(&info);
            info!(
                "[Updater] Version changed to {} or first run. Updated timestamp.",
                self.current_version
            );
        }
    }

    /// Checks GitHub Releases (`eliasventuri/decco.engine`) for a newer version
    pub async fn check_for_updates(&self) {
        info!("[Updater] Checking for update...");
        let Ok(client) = reqwest::Client::builder()
            .user_agent("Decco-Engine-Rust-Updater")
            .timeout(Duration::from_secs(20))
            .build()
        else {
            return;
        };

        let Ok(resp) = client.get(GITHUB_RELEASES_LATEST_API).send().await else {
            warn!("[Updater] Could not reach GitHub Releases API");
            return;
        };

        if !resp.status().is_success() {
            return;
        }

        let Ok(release_json) = resp.json::<serde_json::Value>().await else {
            return;
        };

        let Some(tag_name) = release_json.get("tag_name").and_then(|t| t.as_str()) else {
            return;
        };
        let remote_version = tag_name.trim_start_matches('v');

        if remote_version == self.current_version {
            info!("[Updater] Update not available (up to date: v{}).", self.current_version);
            return;
        }

        info!("[Updater] Update available: {}", remote_version);

        // Find Windows installer asset (.exe)
        if let Some(assets) = release_json.get("assets").and_then(|a| a.as_array()) {
            for asset in assets {
                let name = asset.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let dl_url = asset
                    .get("browser_download_url")
                    .and_then(|u| u.as_str())
                    .unwrap_or("");

                if name.ends_with(".exe") && !dl_url.is_empty() {
                    if let Some(parent) = self.info_path.parent() {
                        let dest = parent.join(format!("decco-engine-setup-{}.exe", remote_version));
                        if Self::download_asset(&client, dl_url, &dest).await.is_ok() {
                            *self.downloaded_installer.write() = Some(dest);
                            *self.update_ready.write() = true;
                            info!("[Updater] Update downloaded. Ready to install.");
                        }
                    }
                    break;
                }
            }
        }
    }

    async fn download_asset(
        client: &reqwest::Client,
        url: &str,
        dest: &Path,
    ) -> anyhow::Result<()> {
        let resp = client.get(url).send().await?;
        let bytes = resp.bytes().await?;
        tokio::fs::write(dest, &bytes).await?;
        Ok(())
    }

    /// Launches the downloaded NSIS installer silently (`/S`) and exits the current process
    pub fn quit_and_install(&self) {
        if let Some(installer_path) = self.downloaded_installer.read().clone() {
            if installer_path.exists() {
                info!(
                    "[Updater] Installing update from {} and restarting...",
                    installer_path.display()
                );
                let _ = std::process::Command::new(&installer_path)
                    .arg("/S")
                    .spawn();
                std::process::exit(0);
            }
        }
    }

    /// Starts the 1-hour periodic update check loop
    pub fn start_periodic_check(self: Arc<Self>) {
        tokio::spawn(async move {
            self.check_for_updates().await;
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                info!("[Updater] Periodic update check...");
                self.check_for_updates().await;
            }
        });
    }
}

