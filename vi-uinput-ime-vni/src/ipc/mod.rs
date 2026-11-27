pub mod bridge;
pub mod fcitx5;
pub mod gnome;
pub mod hyprland;
pub mod kde;
pub mod sway;
pub mod x11;
pub mod x11_xkb;

use crate::input::grabber::RouterEvent;
use std::env;
use tokio::sync::mpsc;
use tracing::info;

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowFocusInfo {
    pub app_class: String,
    pub is_xwayland: bool,
}

/// Unified trait implemented by Hyprland, Sway/i3, and X11 compositor monitors.
pub trait WindowMonitor: Send + Sync {
    fn name(&self) -> &'static str;
    fn spawn(self: Box<Self>, router_tx: mpsc::UnboundedSender<RouterEvent>);
}

/// Auto-detect the active Linux display server / desktop environment from session environment
/// variables and spawn both the desktop layout monitor (GNOME / KDE / Fcitx5 guard) and the
/// active-window focus monitor (Hyprland / Sway / X11).
pub fn spawn_active_window_monitor(router_tx: mpsc::UnboundedSender<RouterEvent>) {
    let desktop = env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_uppercase();
    let is_gnome = desktop.contains("GNOME");
    let is_kde = desktop.contains("KDE");
    let mut started_any = false;

    // 1. Desktop active layout auto-sync monitors (GNOME/Mutter & KDE/KWin + optional Fcitx5 guard)
    if is_gnome {
        info!("Detected GNOME/Mutter session — starting real-time input-source layout sync (`vi_uinput` <-> US/IBus/Fcitx5).");
        gnome::spawn_gnome_layout_monitor(router_tx.clone());
        fcitx5::spawn_fcitx5_guard_if_running(router_tx.clone());
        started_any = true;
    } else if is_kde {
        info!("Detected KDE Plasma / KWin session — starting real-time D-Bus keyboard layout sync (`vi_uinput` <-> US/Fcitx5).");
        kde::spawn_kde_layout_monitor(router_tx.clone());
        fcitx5::spawn_fcitx5_guard_if_running(router_tx.clone());
        started_any = true;
    }

    // 2. Active-window focus monitors for per-app FIFO delay adaptation (Hyprland / Sway / X11)
    let monitor: Option<Box<dyn WindowMonitor>> = if env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok()
    {
        Some(Box::new(hyprland::HyprlandMonitor))
    } else if env::var("SWAYSOCK").is_ok() || env::var("I3SOCK").is_ok() {
        Some(Box::new(sway::SwayMonitor))
    } else if env::var("DISPLAY").is_ok() && !is_gnome && !is_kde {
        Some(Box::new(x11::X11Monitor))
    } else if env::var("DISPLAY").is_ok() && env::var("WAYLAND_DISPLAY").is_err() {
        // Pure X11 GNOME/KDE session: also track active X11 window class for per-app delays
        Some(Box::new(x11::X11Monitor))
    } else {
        None
    };

    if let Some(m) = monitor {
        info!("Starting compositor IPC window monitor: {}", m.name());
        m.spawn(router_tx);
        started_any = true;
    }

    if !started_any {
        info!("No direct desktop session environment (GNOME/KDE/Hyprland/Sway/X11) in process env; waiting for user-session `--ipc-bridge` events.");
    }
}
