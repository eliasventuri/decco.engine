#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod protocol;
mod tray;
mod updater;

use decco_core::{
    config::{EngineConfig, DEFAULT_PRIMARY_PORT},
    DeccoEngine,
};
use muda::MenuEvent;
use single_instance::SingleInstance;
use std::sync::Arc;
use tracing::{info, warn};
use updater::UpdaterState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let protocol_url = args.iter().find(|a| a.starts_with("decco://")).cloned();

    // Enforce Single Instance Lock
    let instance = SingleInstance::new("com.decco.engine.single-instance")?;
    if !instance.is_single() {
        if let Some(url) = protocol_url {
            protocol::forward_link_to_running_instance(&url, DEFAULT_PRIMARY_PORT).await;
        }
        return Ok(());
    }

    // Register Windows Protocol (`decco://`) and Auto-Start (`HKCU\...\Run`)
    protocol::register_windows_integration();

    let config = EngineConfig::default_desktop();
    let updater = Arc::new(UpdaterState::new(config.update_info_path.clone()));

    let engine = DeccoEngine::new(config);
    let bound_ports = engine.start().await?;
    info!(
        "[Engine] Decco Engine Rust v{} active on ports: {:?}",
        env!("CARGO_PKG_VERSION"),
        bound_ports
    );

    if let Some(url) = protocol_url {
        engine.handle_protocol_link(&url).await;
    }

    // Start 1-hour periodic GitHub Releases update checker
    Arc::clone(&updater).start_periodic_check();

    // Initialize System Tray & Win32 message pump
    match tray::create_system_tray(&updater) {
        Ok((_tray_icon, menu_ids)) => {
            let menu_channel = MenuEvent::receiver();

            #[cfg(target_os = "windows")]
            {
                use windows_sys::Win32::UI::WindowsAndMessaging::{
                    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
                };

                loop {
                    unsafe {
                        let mut msg: MSG = std::mem::zeroed();
                        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }

                    while let Ok(event) = menu_channel.try_recv() {
                        tray::handle_tray_menu_event(event, &menu_ids, &engine, &updater);
                    }

                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }

            #[cfg(not(target_os = "windows"))]
            {
                loop {
                    while let Ok(event) = menu_channel.try_recv() {
                        tray::handle_tray_menu_event(event, &menu_ids, &engine, &updater);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
        Err(e) => {
            warn!("[Tray] Running headless without system tray: {}", e);
            tokio::signal::ctrl_c().await?;
        }
    }

    Ok(())
}

