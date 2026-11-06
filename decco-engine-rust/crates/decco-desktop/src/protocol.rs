use tracing::{info, warn};

/// Registers the `decco://` custom URI scheme and Windows Auto-Start (`HKCU\...\Run`)
pub fn register_windows_integration() {
    #[cfg(target_os = "windows")]
    {
        use std::env;
        use winreg::enums::*;
        use winreg::RegKey;

        let Ok(exe_path) = env::current_exe() else {
            return;
        };
        let exe_str = exe_path.to_string_lossy();

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);

        // 1. Auto-start on login: HKCU\Software\Microsoft\Windows\CurrentVersion\Run
        if let Ok((run_key, _)) =
            hkcu.create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")
        {
            let reg_value = format!("\"{}\" --hidden", exe_str);
            match run_key.set_value("DeccoEngine", &reg_value) {
                Ok(()) => info!("[AutoStart] Registry entry set successfully: {}", reg_value),
                Err(e) => warn!("[AutoStart] Failed to write registry: {}", e),
            }
        }

        // 2. Custom Protocol Client: HKCU\Software\Classes\decco
        if let Ok((proto_key, _)) = hkcu.create_subkey(r"Software\Classes\decco") {
            let _ = proto_key.set_value("", &"URL:Decco Protocol");
            let _ = proto_key.set_value("URL Protocol", &"");

            if let Ok((icon_key, _)) = proto_key.create_subkey("DefaultIcon") {
                let _ = icon_key.set_value("", &format!("\"{}\",0", exe_str));
            }

            if let Ok((cmd_key, _)) = proto_key.create_subkey(r"shell\open\command") {
                let cmd_val = format!("\"{}\" \"%1\"", exe_str);
                let _ = cmd_key.set_value("", &cmd_val);
            }
        }
    }
}

/// When a second instance is launched with a `decco://` URL, forwards the link to the primary running instance via HTTP
pub async fn forward_link_to_running_instance(url: &str, primary_port: u16) {
    let engine = decco_core::DeccoEngine::new(decco_core::config::EngineConfig::default_desktop());
    // Parse and trigger /start/:hash on loopback
    if let Some(rest) = url.strip_prefix("decco://") {
        let hash: String = rest
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if !hash.is_empty() {
            let target = format!("http://127.0.0.1:{}/start/{}", primary_port, hash);
            let _ = reqwest::get(&target).await;
            return;
        }
    }
    engine.handle_protocol_link(url).await;
}

