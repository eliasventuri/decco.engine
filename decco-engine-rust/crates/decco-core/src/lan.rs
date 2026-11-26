use crate::config::{now_ms, EngineConfig};
use parking_lot::{Mutex, RwLock};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use sysinfo::Networks;
use tracing::{info, warn};

const PAIRING_RATE_LIMIT_MAX: u32 = 10;
const PAIRING_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
const UDP_RATE_LIMIT_MAX: u32 = 15;
const UDP_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(10);
const SUBNET_CACHE_TTL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedLanDevice {
    pub device_id: String,
    pub device_name: String,
    pub token_sha256_hex: String,
    pub user_id: Option<String>,
    pub paired_at_ms: u64,
    pub last_seen_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanSecurityStore {
    pub enabled: bool,
    pub node_id: String,
    pub device_name: String,
    pub cert_der_hex: String,
    pub key_pkcs8_der_hex: String,
    pub cert_sha256: String,
    pub owner_user_id: Option<String>,
    pub owner_email: Option<String>,
    #[serde(default)]
    pub paired_devices: HashMap<String, PairedLanDevice>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LanStatusSummary {
    pub enabled: bool,
    pub https_port: u16,
    pub discovery_port: u16,
    pub node_id: String,
    pub device_name: String,
    pub cert_sha256: String,
    pub has_owner: bool,
    pub owner_email: Option<String>,
    pub paired_devices_count: usize,
    pub local_ips: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairRequest {
    pub device_id: String,
    pub device_name: Option<String>,
    pub session_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairResponse {
    pub status: &'static str,
    pub token: String,
    pub node_id: String,
    pub device_name: String,
    pub cert_sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindOwnerRequest {
    pub session_token: String,
}

pub struct LanSecurityManager {
    config_path: PathBuf,
    store: RwLock<LanSecurityStore>,
    save_lock: Mutex<()>,
    pub bound_https_port: RwLock<u16>,
    pub bound_discovery_port: RwLock<u16>,
    pub allow_loopback_peers_for_test: RwLock<bool>,
    open_pairing_until_ms: RwLock<u64>,
    pair_rate_limits: Mutex<HashMap<IpAddr, (Instant, u32)>>,
    udp_rate_limits: Mutex<HashMap<IpAddr, (Instant, u32)>>,
    subnet_cache: RwLock<Option<(Instant, Vec<(Ipv4Addr, u8)>)>>,
}

impl LanSecurityManager {
    pub fn new(config: &EngineConfig) -> Self {
        let default_enabled = !config.is_tv_mode && config.lan_tls_port > 0;
        let store = Self::load_or_generate(&config.lan_security_path, default_enabled);
        Self {
            config_path: config.lan_security_path.clone(),
            store: RwLock::new(store),
            save_lock: Mutex::new(()),
            bound_https_port: RwLock::new(config.lan_tls_port),
            bound_discovery_port: RwLock::new(config.lan_discovery_port),
            allow_loopback_peers_for_test: RwLock::new(false),
            open_pairing_until_ms: RwLock::new(0),
            pair_rate_limits: Mutex::new(HashMap::new()),
            udp_rate_limits: Mutex::new(HashMap::new()),
            subnet_cache: RwLock::new(None),
        }
    }

    fn detect_device_name() -> String {
        let raw = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "Desktop".to_string());
        let cleaned: String = raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == ' ')
            .take(32)
            .collect();
        let trimmed = cleaned.trim();
        if trimmed.is_empty() {
            "PC Desktop".to_string()
        } else {
            format!("PC ({trimmed})")
        }
    }

    fn generate_certificate_material() -> anyhow::Result<(String, String, String)> {
        let rcgen::CertifiedKey { cert, key_pair } = rcgen::generate_simple_self_signed(vec![
            "decco-lan.local".to_string(),
            "localhost".to_string(),
        ])?;
        let cert_der = cert.der().as_ref();
        let key_der = key_pair.serialize_der();
        let cert_sha256 = sha256_hex(cert_der);
        Ok((
            hex::encode(cert_der),
            hex::encode(&key_der),
            cert_sha256,
        ))
    }

    fn load_or_generate(path: &Path, default_enabled: bool) -> LanSecurityStore {
        if path.exists() {
            if let Ok(raw) = std::fs::read_to_string(path) {
                if let Ok(mut parsed) = serde_json::from_str::<LanSecurityStore>(&raw) {
                    if !parsed.cert_der_hex.is_empty()
                        && !parsed.key_pkcs8_der_hex.is_empty()
                        && !parsed.cert_sha256.is_empty()
                        && !parsed.node_id.is_empty()
                    {
                        if parsed.device_name.trim().is_empty() {
                            parsed.device_name = Self::detect_device_name();
                        }
                        return parsed;
                    }
                }
            }
        }

        let (cert_der_hex, key_pkcs8_der_hex, cert_sha256) =
            Self::generate_certificate_material().expect("Failed to generate LAN TLS certificate");
        let node_id = random_hex(16);
        let store = LanSecurityStore {
            enabled: default_enabled,
            node_id,
            device_name: Self::detect_device_name(),
            cert_der_hex,
            key_pkcs8_der_hex,
            cert_sha256,
            owner_user_id: None,
            owner_email: None,
            paired_devices: HashMap::new(),
        };
        Self::persist_store(path, &store);
        store
    }

    fn persist_store(path: &Path, store: &LanSecurityStore) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(serialized) = serde_json::to_string_pretty(store) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, serialized.as_bytes()).is_ok() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
                }
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    fn save(&self) {
        let _guard = self.save_lock.lock();
        let snapshot = self.store.read().clone();
        Self::persist_store(&self.config_path, &snapshot);
    }

    pub fn is_enabled(&self) -> bool {
        self.store.read().enabled
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.store.write().enabled = enabled;
        self.save();
        info!("[LAN] LAN storage sharing set to enabled={}", enabled);
    }

    pub fn open_temporary_pairing_window(&self, duration_secs: u64) {
        let until = now_ms().saturating_add(duration_secs * 1000);
        *self.open_pairing_until_ms.write() = until;
        info!(
            "[LAN] Opened temporary TV pairing window for {} seconds",
            duration_secs
        );
    }

    pub fn revoke_all_paired_devices(&self) {
        {
            let mut guard = self.store.write();
            guard.paired_devices.clear();
        }
        self.save();
        info!("[LAN] Revoked all paired LAN TV devices");
    }

    pub fn build_tls_acceptor(&self) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
        let (cert_der, key_der) = {
            let guard = self.store.read();
            (
                hex::decode(&guard.cert_der_hex)?,
                hex::decode(&guard.key_pkcs8_der_hex)?,
            )
        };

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server_config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(cert_der)],
                rustls::pki_types::PrivateKeyDer::Pkcs8(
                    rustls::pki_types::PrivatePkcs8KeyDer::from(key_der),
                ),
            )?;

        Ok(tokio_rustls::TlsAcceptor::from(Arc::new(server_config)))
    }

    pub fn local_private_subnets(&self) -> Vec<(Ipv4Addr, u8)> {
        {
            let cache = self.subnet_cache.read();
            if let Some((ts, ref list)) = *cache {
                if ts.elapsed() < SUBNET_CACHE_TTL {
                    return list.clone();
                }
            }
        }

        let networks = Networks::new_with_refreshed_list();
        let mut subnets = Vec::new();
        for (_name, data) in &networks {
            for net in data.ip_networks() {
                if let IpAddr::V4(v4) = net.addr {
                    if v4.is_private() && !v4.is_loopback() && !v4.is_link_local() {
                        // Clamp prefix to at least /16 so overly broad masks never expose outside LAN
                        let safe_prefix = net.prefix.clamp(16, 30);
                        if !subnets.iter().any(|(ip, _)| *ip == v4) {
                            subnets.push((v4, safe_prefix));
                        }
                    }
                }
            }
        }

        *self.subnet_cache.write() = Some((Instant::now(), subnets.clone()));
        subnets
    }

    pub fn status_summary(&self) -> LanStatusSummary {
        let local_ips = self
            .local_private_subnets()
            .into_iter()
            .map(|(ip, _)| ip.to_string())
            .collect();
        let guard = self.store.read();
        LanStatusSummary {
            enabled: guard.enabled,
            https_port: *self.bound_https_port.read(),
            discovery_port: *self.bound_discovery_port.read(),
            node_id: guard.node_id.clone(),
            device_name: guard.device_name.clone(),
            cert_sha256: guard.cert_sha256.clone(),
            has_owner: guard.owner_user_id.is_some(),
            owner_email: guard.owner_email.clone(),
            paired_devices_count: guard.paired_devices.len(),
            local_ips,
        }
    }

    pub fn is_allowed_lan_peer(&self, peer_ip: IpAddr) -> bool {
        let normalized_v4 = match peer_ip {
            IpAddr::V4(v4) => v4,
            IpAddr::V6(v6) => {
                if *self.allow_loopback_peers_for_test.read() && v6.is_loopback() {
                    return true;
                }
                match v6.to_ipv4_mapped() {
                    Some(mapped) => mapped,
                    None => return false,
                }
            }
        };

        if *self.allow_loopback_peers_for_test.read() && normalized_v4.is_loopback() {
            return true;
        }

        if !normalized_v4.is_private() || normalized_v4.is_loopback() || normalized_v4.is_link_local() {
            return false;
        }

        let subnets = self.local_private_subnets();
        if subnets.is_empty() {
            return false;
        }

        let peer_bits = u32::from(normalized_v4);
        subnets.into_iter().any(|(iface_ip, prefix)| {
            let mask = if prefix >= 32 {
                u32::MAX
            } else {
                !0u32 << (32 - prefix)
            };
            (peer_bits & mask) == (u32::from(iface_ip) & mask)
        })
    }

    pub fn check_udp_rate_limit(&self, ip: IpAddr) -> bool {
        check_rate_limit(
            &self.udp_rate_limits,
            ip,
            UDP_RATE_LIMIT_MAX,
            UDP_RATE_LIMIT_WINDOW,
        )
    }

    pub fn check_pair_rate_limit(&self, ip: IpAddr) -> bool {
        check_rate_limit(
            &self.pair_rate_limits,
            ip,
            PAIRING_RATE_LIMIT_MAX,
            PAIRING_RATE_LIMIT_WINDOW,
        )
    }

    pub fn validate_bearer_token(&self, token: &str) -> bool {
        let trimmed = token.trim();
        if trimmed.len() < 32 {
            return false;
        }
        let candidate_digest = Sha256::digest(trimmed.as_bytes());
        let candidate_bytes = candidate_digest.as_slice();

        let mut matched_device_id: Option<String> = None;
        {
            let guard = self.store.read();
            if !guard.enabled {
                return false;
            }
            for dev in guard.paired_devices.values() {
                if let Ok(expected_bytes) = hex::decode(&dev.token_sha256_hex) {
                    if constant_time_eq(candidate_bytes, &expected_bytes) {
                        matched_device_id = Some(dev.device_id.clone());
                        break;
                    }
                }
            }
        }

        if let Some(dev_id) = matched_device_id {
            let now = now_ms();
            let mut guard = self.store.write();
            if let Some(dev) = guard.paired_devices.get_mut(&dev_id) {
                if now.saturating_sub(dev.last_seen_ms) > 60_000 {
                    dev.last_seen_ms = now;
                }
            }
            true
        } else {
            false
        }
    }

    pub async fn verify_decco_account_token(
        http_client: &reqwest::Client,
        session_token: &str,
    ) -> Option<(String, String)> {
        let clean = session_token.trim();
        if clean.len() < 12 || clean.len() > 512 {
            return None;
        }

        let endpoints = [
            "https://www.decco.tv/api/user/profile",
            "https://decco.tv/api/user/profile",
        ];

        for url in endpoints {
            let res = http_client
                .get(url)
                .header("Authorization", format!("Bearer {clean}"))
                .header("User-Agent", "DeccoEngine/2.0")
                .timeout(Duration::from_secs(6))
                .send()
                .await;

            if let Ok(resp) = res {
                if resp.status().is_success() {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        let id = json
                            .get("id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty());
                        let email = json
                            .get("email")
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim().to_string())
                            .unwrap_or_default();
                        if let Some(uid) = id {
                            return Some((uid, email));
                        }
                    }
                }
            }
        }
        None
    }

    pub async fn bind_owner_from_session(
        &self,
        http_client: &reqwest::Client,
        session_token: &str,
    ) -> Result<(String, String), &'static str> {
        let Some((user_id, email)) =
            Self::verify_decco_account_token(http_client, session_token).await
        else {
            return Err("Invalid or expired Decco session token");
        };

        {
            let mut guard = self.store.write();
            let changed = guard.owner_user_id.as_deref() != Some(user_id.as_str());
            guard.owner_user_id = Some(user_id.clone());
            if !email.is_empty() {
                guard.owner_email = Some(email.clone());
            }
            if changed {
                // Remove any device explicitly tied to a different Decco account
                guard.paired_devices.retain(|_, dev| {
                    dev.user_id.is_none() || dev.user_id.as_deref() == Some(user_id.as_str())
                });
            }
        }
        self.save();
        info!("[LAN] Bound LAN Engine owner to user_id={}", user_id);
        Ok((user_id, email))
    }

    pub fn bind_owner_direct_for_test(&self, user_id: &str, email: &str) {
        let mut guard = self.store.write();
        guard.owner_user_id = Some(user_id.to_string());
        guard.owner_email = Some(email.to_string());
    }

    pub async fn pair_tv_device(
        &self,
        http_client: &reqwest::Client,
        req: PairRequest,
    ) -> Result<PairResponse, (axum::http::StatusCode, &'static str)> {
        use axum::http::StatusCode;

        let clean_device_id: String = req
            .device_id
            .trim()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .take(96)
            .collect();
        if clean_device_id.len() < 8 {
            return Err((StatusCode::BAD_REQUEST, "Invalid deviceId"));
        }

        let clean_device_name: String = req
            .device_name
            .unwrap_or_else(|| "Android TV".to_string())
            .chars()
            .filter(|c| c.is_ascii_graphic() || *c == ' ')
            .take(48)
            .collect::<String>()
            .trim()
            .to_string();
        let final_name = if clean_device_name.is_empty() {
            "Android TV".to_string()
        } else {
            clean_device_name
        };

        let verified_account = match req
            .session_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(tok) => Self::verify_decco_account_token(http_client, tok).await,
            None => None,
        };

        let now = now_ms();
        let temp_window_open = now < *self.open_pairing_until_ms.read();

        let (node_id, pc_name, cert_sha256, raw_token) = {
            let mut guard = self.store.write();
            if !guard.enabled {
                return Err((StatusCode::FORBIDDEN, "LAN sharing is disabled on this PC"));
            }

            let authorized = match (&guard.owner_user_id, &verified_account) {
                // Both PC and TV have verified Decco accounts -> MUST match exact same account
                (Some(owner_uid), Some((tv_uid, _))) => owner_uid == tv_uid,
                // PC has owner bound, TV sent an invalid/different token or is unauthenticated:
                // Allow ONLY if zero devices have ever paired yet (first-TV TOFU) or user opened 60s Tray window,
                // AND the TV did not present a non-empty invalid token.
                (Some(_), None) => {
                    let sent_token = req
                        .session_token
                        .as_deref()
                        .map(|s| !s.trim().is_empty())
                        .unwrap_or(false);
                    !sent_token && (guard.paired_devices.is_empty() || temp_window_open)
                }
                // PC has no owner bound yet, TV is logged into a verified Decco account:
                // If no devices paired yet (or temp window open), bind PC owner to TV's Decco account!
                (None, Some((tv_uid, tv_email))) => {
                    if guard.paired_devices.is_empty() || temp_window_open {
                        guard.owner_user_id = Some(tv_uid.clone());
                        if !tv_email.is_empty() {
                            guard.owner_email = Some(tv_email.clone());
                        }
                        true
                    } else {
                        false
                    }
                }
                // Pure local/offline mode (neither PC nor TV signed in):
                // Allow first TV on private subnet (TOFU) or explicit 60s Tray window; lock out strangers afterwards.
                (None, None) => {
                    guard.paired_devices.is_empty()
                        || guard.paired_devices.contains_key(&clean_device_id)
                        || temp_window_open
                }
            };

            if !authorized {
                warn!(
                    "[LAN Security] Rejected pairing attempt for device_id={} ({})",
                    clean_device_id, final_name
                );
                return Err((
                    StatusCode::FORBIDDEN,
                    "Pairing rejected: sign in with the same Decco account on PC and TV, or click 'Allow New TV Device' in the PC tray icon",
                ));
            }

            let issued_token = format!("dlan_{}", random_hex(32));
            let token_sha256_hex = sha256_hex(issued_token.as_bytes());
            let device_entry = PairedLanDevice {
                device_id: clean_device_id.clone(),
                device_name: final_name.clone(),
                token_sha256_hex,
                user_id: verified_account.map(|(uid, _)| uid),
                paired_at_ms: now,
                last_seen_ms: now,
            };
            guard.paired_devices.insert(clean_device_id.clone(), device_entry);

            (
                guard.node_id.clone(),
                guard.device_name.clone(),
                guard.cert_sha256.clone(),
                issued_token,
            )
        };

        // Close temporary pairing window once used
        if temp_window_open {
            *self.open_pairing_until_ms.write() = 0;
        }
        self.save();

        info!(
            "[LAN] Paired TV device '{}' ({}) successfully",
            final_name, clean_device_id
        );

        Ok(PairResponse {
            status: "paired",
            token: raw_token,
            node_id,
            device_name: pc_name,
            cert_sha256,
        })
    }

    pub fn build_discovery_reply(&self, nonce: &str) -> Option<String> {
        let guard = self.store.read();
        if !guard.enabled {
            return None;
        }
        let https_port = *self.bound_https_port.read();
        if https_port == 0 {
            return None;
        }
        let safe_nonce: String = nonce
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .take(64)
            .collect();

        Some(
            serde_json::json!({
                "decco": "node",
                "v": 1,
                "nodeId": guard.node_id,
                "deviceName": guard.device_name,
                "httpsPort": https_port,
                "certSha256": guard.cert_sha256,
                "hasOwner": guard.owner_user_id.is_some(),
                "nonce": safe_nonce,
            })
            .to_string(),
        )
    }
}

fn check_rate_limit(
    map: &Mutex<HashMap<IpAddr, (Instant, u32)>>,
    ip: IpAddr,
    max_requests: u32,
    window: Duration,
) -> bool {
    let now = Instant::now();
    let mut guard = map.lock();
    if guard.len() > 256 {
        guard.retain(|_, (ts, _)| now.duration_since(*ts) < window);
    }
    let entry = guard.entry(ip).or_insert((now, 0));
    if now.duration_since(entry.0) >= window {
        *entry = (now, 1);
        true
    } else if entry.1 < max_requests {
        entry.1 += 1;
        true
    } else {
        false
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn random_hex(num_bytes: usize) -> String {
    let mut buf = vec![0u8; num_bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Spawns the UDP broadcast discovery responder on `0.0.0.0:<discovery_port>`
pub async fn spawn_lan_udp_discovery(
    lan_mgr: Arc<LanSecurityManager>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let port = *lan_mgr.bound_discovery_port.read();
    if port == 0 {
        return;
    }

    let bind_addr = SocketAddr::from(([0, 0, 0, 0], port));
    let socket = match tokio::net::UdpSocket::bind(bind_addr).await {
        Ok(s) => {
            info!("[LAN Discovery] UDP responder listening on 0.0.0.0:{}", port);
            s
        }
        Err(e) => {
            warn!(
                "[LAN Discovery] Could not bind UDP 0.0.0.0:{}: {}",
                port, e
            );
            *lan_mgr.bound_discovery_port.write() = 0;
            return;
        }
    };

    let mut buf = [0u8; 1024];
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            recv_res = socket.recv_from(&mut buf) => {
                let Ok((len, peer_addr)) = recv_res else { continue };
                if len > 512 || !lan_mgr.is_enabled() {
                    continue;
                }
                if !lan_mgr.is_allowed_lan_peer(peer_addr.ip()) {
                    continue;
                }
                if !lan_mgr.check_udp_rate_limit(peer_addr.ip()) {
                    continue;
                }
                let Ok(raw_str) = std::str::from_utf8(&buf[..len]) else { continue };
                let Ok(parsed) = serde_json::from_str::<serde_json::Value>(raw_str) else { continue };
                if parsed.get("decco").and_then(|v| v.as_str()) != Some("discover") {
                    continue;
                }
                let nonce = parsed.get("nonce").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(reply) = lan_mgr.build_discovery_reply(nonce) {
                    let _ = socket.send_to(reply.as_bytes(), peer_addr).await;
                }
            }
        }
    }
}
