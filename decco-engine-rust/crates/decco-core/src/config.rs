use regex::Regex;
use std::path::{Path, PathBuf};

/// Primary default port (synced with desktop engine)
pub const DEFAULT_PRIMARY_PORT: u16 = 18889;

/// All ports the engine attempts to bind to (Windows & Android)
pub const PORTS_TO_TRY: &[u16] = &[18889, 18888, 28888, 38888, 48888, 8888];

/// HLS Segment duration in seconds
pub const SEGMENT_DURATION: u64 = 10;

/// Cache expiration TTL: 72 hours in milliseconds
pub const CACHE_MAX_AGE_MS: u64 = 72 * 60 * 60 * 1000;

/// Grace period before destroying a stopped stream (6 seconds)
pub const STOP_GRACE_PERIOD_MS: u64 = 6000;

/// Delay after stream stops before auto-resuming paused downloads (2 seconds)
pub const AUTO_RESUME_DELAY_MS: u64 = 2000;

/// Timeout before marking a 0-speed/0-peers download as stalled (45 seconds)
pub const STALLED_TIMEOUT_MS: u64 = 45000;

/// Critical head & sliding readahead target size (24 MB)
pub const READAHEAD_WINDOW_BYTES: u64 = 24 * 1024 * 1024;

/// High-availability public BitTorrent trackers (synced with Decco Engine 2026)
pub const TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.demonii.com:1337/announce",
    "udp://exodus.desync.com:6969/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://tracker.openbittorrent.com:6969/announce",
    "udp://tracker.moeking.me:6969/announce",
    "udp://explodie.org:6969/announce",
    "udp://tracker1.bt.moack.co.kr:80/announce",
    "udp://tracker.theoks.net:6969/announce",
    "udp://tracker.tiny-vps.com:6969/announce",
    "udp://tracker.dler.org:6969/announce",
    "udp://opentracker.io:6969/announce",
    "udp://open.free-tracker.ga:6969/announce",
    "udp://opentor.net:6969",
    "udp://p4p.arenabg.com:1337/announce",
    "udp://movies.zsw.ca:6969/announce",
    "http://tracker.opentrackr.org:1337/announce",
    "http://tracker.openbittorrent.com:80/announce",
    "http://open.acgnxtracker.com:80/announce",
    "http://tracker.dler.org:6969/announce",
    "https://tracker.tamersunion.org:443/announce",
    "https://tracker.gbitt.info:443/announce",
    "http://bt.t-ru.org/ann?magnet",
    "http://bt2.t-ru.org/ann?magnet",
    "http://bt3.t-ru.org/ann?magnet",
    "http://bt4.t-ru.org/ann?magnet",
];

/// Supported video extensions across streaming and multipack cleanup
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "avi", "webm", "ts", "mov", "wmv", "flv", "m4v", "3gp", "mpg", "mpeg", "ogv",
];

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Base userData directory
    pub base_dir: PathBuf,
    /// Temporary streaming cache directory (`<base_dir>/downloads`)
    pub streaming_cache_dir: PathBuf,
    /// Permanent downloads directory (`<base_dir>/completed-downloads`)
    pub completed_downloads_dir: PathBuf,
    /// Path to `cache-meta.json`
    pub cache_meta_path: PathBuf,
    /// Path to `downloads-meta.json`
    pub downloads_meta_path: PathBuf,
    /// Path to `update-info.json`
    pub update_info_path: PathBuf,
    /// Path to `decco-engine.log`
    pub log_file_path: PathBuf,
    /// Android TV low-memory/connection mode
    pub is_tv_mode: bool,
    /// Ports to bind HTTP server
    pub ports: Vec<u16>,
}

impl EngineConfig {
    pub fn new(base_dir: PathBuf, is_tv_mode: bool) -> Self {
        let streaming_cache_dir = base_dir.join("downloads");
        let completed_downloads_dir = base_dir.join("completed-downloads");
        let cache_meta_path = streaming_cache_dir.join("cache-meta.json");
        let downloads_meta_path = completed_downloads_dir.join("downloads-meta.json");
        let update_info_path = streaming_cache_dir.join("update-info.json");
        let log_file_path = streaming_cache_dir.join("decco-engine.log");

        let _ = std::fs::create_dir_all(&streaming_cache_dir);
        let _ = std::fs::create_dir_all(&completed_downloads_dir);

        Self {
            base_dir,
            streaming_cache_dir,
            completed_downloads_dir,
            cache_meta_path,
            downloads_meta_path,
            update_info_path,
            log_file_path,
            is_tv_mode,
            ports: PORTS_TO_TRY.to_vec(),
        }
    }

    pub fn default_desktop() -> Self {
        let base_dir = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("decco-engine");
        Self::new(base_dir, false)
    }
}

/// Checks if a filename has a supported video extension
pub fn is_video_file(file_name: &str) -> bool {
    Path::new(file_name)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| VIDEO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Returns the MIME content-type for a video filename
pub fn get_video_content_type(file_name: &str) -> &'static str {
    let ext = Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "ts" => "video/mp2t",
        "mov" => "video/quicktime",
        _ => "video/mp4",
    }
}

/// Builds regex to match episode patterns like S05E06, S5E6, 5x06, s05.e06, etc.
pub fn build_episode_pattern(season: u32, episode: u32) -> Option<Regex> {
    let pattern = format!(
        r"(?i)(s0?{s}[.\s_-]?e0?{e}\b)|(\b0?{s}x0?{e}\b)",
        s = season,
        e = episode
    );
    Regex::new(&pattern).ok()
}

/// Build composite download ID (`<hash>_s<season>e<episode>`, `<hash>_f<fileIdx>`, or `<hash>`)
pub fn get_download_id(
    hash: &str,
    season: Option<u32>,
    episode: Option<u32>,
    file_idx: Option<usize>,
) -> String {
    let clean_hash = hash.trim().to_ascii_lowercase();
    if let (Some(s), Some(e)) = (season, episode) {
        if s > 0 || e > 0 {
            return format!("{}_s{}e{}", clean_hash, s, e);
        }
    }
    if let Some(idx) = file_idx {
        return format!("{}_f{}", clean_hash, idx);
    }
    clean_hash
}

/// Build full magnet URI with all default trackers
pub fn build_magnet_uri(hash: &str) -> String {
    let clean_hash = hash.trim().to_ascii_lowercase();
    let encoded_trackers: Vec<String> = TRACKERS
        .iter()
        .map(|tr| {
            percent_encoding::utf8_percent_encode(tr, percent_encoding::NON_ALPHANUMERIC)
                .to_string()
        })
        .collect();
    format!(
        "magnet:?xt=urn:btih:{}&tr={}",
        clean_hash,
        encoded_trackers.join("&tr=")
    )
}

/// Recursively find the largest video file in a directory
pub fn find_largest_video_file(dir: &Path) -> Option<PathBuf> {
    if !dir.exists() {
        return None;
    }
    let mut largest_file: Option<PathBuf> = None;
    let mut largest_size: u64 = 0;

    fn scan(current: &Path, largest_file: &mut Option<PathBuf>, largest_size: &mut u64) {
        if let Ok(entries) = std::fs::read_dir(current) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    scan(&path, largest_file, largest_size);
                } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if is_video_file(name) {
                        if let Ok(meta) = entry.metadata() {
                            if meta.len() >= *largest_size {
                                *largest_size = meta.len();
                                *largest_file = Some(path);
                            }
                        }
                    }
                }
            }
        }
    }

    scan(dir, &mut largest_file, &mut largest_size);
    largest_file
}

/// Recursively find an exact video file by name in a directory, falling back to the largest video file
pub fn find_exact_video_file(dir: &Path, target_name: Option<&str>) -> Option<PathBuf> {
    if !dir.exists() {
        return None;
    }
    if let Some(wanted) = target_name {
        let mut found: Option<PathBuf> = None;
        fn scan_exact(current: &Path, wanted: &str, found: &mut Option<PathBuf>) {
            if found.is_some() {
                return;
            }
            if let Ok(entries) = std::fs::read_dir(current) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        scan_exact(&path, wanted, found);
                    } else if path.file_name().and_then(|n| n.to_str()) == Some(wanted) {
                        *found = Some(path);
                        return;
                    }
                }
            }
        }
        scan_exact(dir, wanted, &mut found);
        if found.is_some() {
            return found;
        }
    }
    find_largest_video_file(dir)
}

/// Current epoch timestamp in milliseconds
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

