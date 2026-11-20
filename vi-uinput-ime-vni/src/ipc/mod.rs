pub mod hyprland;
pub mod sway;
pub mod x11;

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

/// Auto-detect the active Linux display server / compositor from session environment variables
/// and spawn the matching IPC window monitor.
pub fn spawn_active_window_monitor(router_tx: mpsc::UnboundedSender<RouterEvent>) {
    let desktop = env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_uppercase();
    let is_wayland = env::var("WAYLAND_DISPLAY").is_ok();
    let is_gnome_wayland = is_wayland && desktop.contains("GNOME");
    let is_kde_wayland = is_wayland && desktop.contains("KDE");

    let monitor: Option<Box<dyn WindowMonitor>> = if env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok()
    {
        Some(Box::new(hyprland::HyprlandMonitor))
    } else if env::var("SWAYSOCK").is_ok() || env::var("I3SOCK").is_ok() {
        Some(Box::new(sway::SwayMonitor))
    } else if is_gnome_wayland {
        info!("Detected GNOME/Mutter Wayland session — ensure RMLVO layout `vi_uinput` is active via `vi-uinput-ime --install-gnome-xkb`.");
        None
    } else if is_kde_wayland {
        info!("Detected KDE Plasma / KWin Wayland session — ensure RMLVO layout `vi_uinput` is installed via `vi-uinput-ime --install-gnome-xkb` and enabled in System Settings -> Keyboard -> Layouts.");
        None
    } else if env::var("DISPLAY").is_ok() {
        Some(Box::new(x11::X11Monitor))
    } else {
        None
    };

    if let Some(m) = monitor {
        info!("Starting compositor IPC window monitor: {}", m.name());
        m.spawn(router_tx);
    } else {
        info!("No HYPRLAND_INSTANCE_SIGNATURE, SWAYSOCK, or DISPLAY found; running with default FIFO delay profile.");
    }
}
