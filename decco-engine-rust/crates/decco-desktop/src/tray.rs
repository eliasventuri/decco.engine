use crate::updater::UpdaterState;
use decco_core::DeccoEngine;
use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

pub struct TrayMenuIds {
    pub check_updates: muda::MenuId,
    pub restart_or_install: muda::MenuId,
    pub manage_downloads: muda::MenuId,
    pub toggle_lan: muda::MenuId,
    pub allow_new_tv: muda::MenuId,
    pub revoke_tvs: muda::MenuId,
    pub lan_check_item: CheckMenuItem,
    pub clear_cache: muda::MenuId,
    pub quit: muda::MenuId,
}

fn load_tray_icon() -> Icon {
    let candidates = ["icon.png", "../icon.png", "build/icon.ico", "../build/icon.ico"];
    for candidate in candidates {
        let p = Path::new(candidate);
        if p.exists() {
            if let Ok(img) = image::open(p) {
                let rgba = img.into_rgba8();
                let (w, h) = rgba.dimensions();
                if let Ok(ic) = Icon::from_rgba(rgba.into_raw(), w, h) {
                    return ic;
                }
            }
        }
    }

    // Embedded Decco icon so tray always shows the real icon regardless of working directory
    let embedded_png = include_bytes!("../../../../icon.png");
    if let Ok(img) = image::load_from_memory(embedded_png) {
        let rgba = img.into_rgba8();
        let (w, h) = rgba.dimensions();
        if let Ok(ic) = Icon::from_rgba(rgba.into_raw(), w, h) {
            return ic;
        }
    }

    // Fallback 16x16 red/dark icon if decoding fails
    let mut rgba = Vec::with_capacity(16 * 16 * 4);
    for _ in 0..(16 * 16) {
        rgba.extend_from_slice(&[229, 9, 20, 255]);
    }
    Icon::from_rgba(rgba, 16, 16).expect("valid fallback icon")
}

pub fn create_system_tray(
    updater: &Arc<UpdaterState>,
    lan_enabled_initial: bool,
) -> anyhow::Result<(TrayIcon, TrayMenuIds)> {
    let version = &updater.current_version;
    let info = updater.load_update_info();
    let last_updated_label = match info.last_updated {
        Some(ts) => format!("Last Updated: {} (epoch ms)", ts),
        None => "Last Updated: Never".to_string(),
    };

    let is_update_ready = *updater.update_ready.read();

    let menu = Menu::new();
    let item_version = MenuItem::new(format!("Decco Engine: v{}", version), false, None);
    let item_last_updated = MenuItem::new(last_updated_label, false, None);
    let item_check_updates = MenuItem::new("Check for Updates", true, None);
    let item_restart = if is_update_ready {
        MenuItem::new("Install Update and Restart", true, None)
    } else {
        MenuItem::new("Restart", true, None)
    };
    let item_manage_downloads = MenuItem::new("Manage Downloads", true, None);
    let item_toggle_lan = CheckMenuItem::new(
        "Share Storage with TV (Local Network)",
        true,
        lan_enabled_initial,
        None,
    );
    let item_allow_new_tv = MenuItem::new("Allow New TV Device (60s)", true, None);
    let item_revoke_tvs = MenuItem::new("Disconnect Paired TVs", true, None);
    let item_clear_cache = MenuItem::new("Clear Cache", true, None);
    let item_quit = MenuItem::new("Quit Engine", true, None);

    let _ = menu.append(&item_version);
    let _ = menu.append(&item_last_updated);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&item_check_updates);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&item_toggle_lan);
    let _ = menu.append(&item_allow_new_tv);
    let _ = menu.append(&item_revoke_tvs);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&item_restart);
    let _ = menu.append(&item_manage_downloads);
    let _ = menu.append(&item_clear_cache);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&item_quit);

    let tooltip = if is_update_ready {
        format!("Decco Engine v{} (Update Ready)", version)
    } else {
        format!("Decco Engine v{}", version)
    };

    let icon = load_tray_icon();
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(tooltip)
        .with_icon(icon)
        .build()?;

    let ids = TrayMenuIds {
        check_updates: item_check_updates.id().clone(),
        restart_or_install: item_restart.id().clone(),
        manage_downloads: item_manage_downloads.id().clone(),
        toggle_lan: item_toggle_lan.id().clone(),
        allow_new_tv: item_allow_new_tv.id().clone(),
        revoke_tvs: item_revoke_tvs.id().clone(),
        lan_check_item: item_toggle_lan,
        clear_cache: item_clear_cache.id().clone(),
        quit: item_quit.id().clone(),
    };

    Ok((tray, ids))
}

pub fn handle_tray_menu_event(
    event: MenuEvent,
    ids: &TrayMenuIds,
    engine: &DeccoEngine,
    updater: &Arc<UpdaterState>,
) {
    if event.id == ids.check_updates {
        info!("[Tray] User requested update check");
        let upd = Arc::clone(updater);
        tokio::spawn(async move {
            upd.check_for_updates().await;
        });
    } else if event.id == ids.toggle_lan {
        let new_enabled = ids.lan_check_item.is_checked();
        engine.state.lan_mgr.set_enabled(new_enabled);
    } else if event.id == ids.allow_new_tv {
        engine.state.lan_mgr.open_temporary_pairing_window(60);
    } else if event.id == ids.revoke_tvs {
        engine.state.lan_mgr.revoke_all_paired_devices();
    } else if event.id == ids.restart_or_install {
        if *updater.update_ready.read() {
            updater.quit_and_install();
        } else {
            info!("[Tray] Restarting Decco Engine...");
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::process::Command::new(exe).arg("--restarted").spawn();
            }
            std::process::exit(0);
        }
    } else if event.id == ids.manage_downloads {
        if let Err(e) = open::that("https://decco.tv/downloads") {
            warn!("[Tray] Failed to open browser: {}", e);
        }
    } else if event.id == ids.clear_cache {
        let _ = engine.clear_cache();
    } else if event.id == ids.quit {
        info!("[Tray] Quitting Decco Engine...");
        std::process::exit(0);
    }
}

