use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToneStyle {
    /// Modern placement: hoà, thuý, uỷ
    New,
    /// Traditional placement: hòa, thúy, ủy
    Old,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToggleHotkey {
    /// Disable daemon-owned toggle hotkey so desktop switchers (GNOME/KDE/IBus/Fcitx5) never conflict.
    #[serde(rename = "none")]
    None,
    #[serde(rename = "ctrl+space")]
    CtrlSpace,
    #[serde(rename = "super+space")]
    SuperSpace,
}

fn default_ipc_socket_path() -> String {
    "/run/vi-uinput-ime-vni/ipc.sock".to_string()
}

fn default_sync_desktop_layout() -> bool {
    true
}

fn default_auto_restore_non_vn() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    pub tone_style: ToneStyle,
    pub enabled_on_start: bool,
    /// Automatically pause to 100% PassThrough whenever desktop layout != `vi_uinput`
    /// (GNOME Input Sources / KDE Keyboard Layouts / Fcitx5 guard via `--ipc-bridge`).
    #[serde(default = "default_sync_desktop_layout")]
    pub sync_desktop_layout: bool,
    /// Automatically restore non-Vietnamese words (e.g. English/code identifiers) back to raw ASCII.
    #[serde(default = "default_auto_restore_non_vn")]
    pub auto_restore_non_vn: bool,
    pub state_file: String,
    pub xkb_keymap_path: String,
    #[serde(default = "default_ipc_socket_path")]
    pub ipc_socket_path: String,
    #[serde(default)]
    pub device_include: Vec<String>,
    #[serde(default)]
    pub device_exclude: Vec<String>,
    /// Optional case-insensitive window class substrings (Hyprland/Sway/X11 IPC) where the IME
    /// automatically pauses to 100% raw PassThrough (useful for pure-action games without VI chat).
    #[serde(default)]
    pub passthrough_apps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeyConfig {
    pub toggle: ToggleHotkey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelayConfig {
    pub default_ms: u64,
    pub xwayland_ms: u64,
    #[serde(default)]
    pub apps: HashMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub general: GeneralConfig,
    pub hotkey: HotkeyConfig,
    pub delays: DelayConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        let mut apps = HashMap::new();
        for (k, v) in [
            ("foot", 0),
            ("alacritty", 1),
            ("kitty", 1),
            ("wezterm", 1),
            ("ghostty", 1),
            ("nautilus", 1),
            ("dolphin", 1),
            ("chromium", 4),
            ("google-chrome", 4),
            ("brave-browser", 4),
            ("firefox", 3),
            ("code", 5),
            ("code-oss", 5),
            ("codium", 5),
            ("discord", 5),
            ("slack", 5),
            ("obsidian", 4),
            ("steam", 8),
            ("jetbrains", 7),
            ("libreoffice", 6),
        ] {
            apps.insert(k.to_string(), v);
        }

        Self {
            general: GeneralConfig {
                tone_style: ToneStyle::New,
                enabled_on_start: true,
                sync_desktop_layout: default_sync_desktop_layout(),
                auto_restore_non_vn: default_auto_restore_non_vn(),
                state_file: "/run/vi-uinput-ime-vni/state".to_string(),
                xkb_keymap_path: "/etc/vi-uinput-ime/vn_virtual.xkb".to_string(),
                ipc_socket_path: default_ipc_socket_path(),
                device_include: Vec::new(),
                device_exclude: Vec::new(),
                passthrough_apps: Vec::new(),
            },
            hotkey: HotkeyConfig {
                toggle: ToggleHotkey::None,
            },
            delays: DelayConfig {
                default_ms: 2,
                xwayland_ms: 8,
                apps,
            },
        }
    }
}

impl AppConfig {
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;
        let parsed: AppConfig = toml::from_str(&raw)
            .with_context(|| format!("failed to parse TOML config: {}", path.display()))?;
        Ok(parsed)
    }

    pub fn resolve_delay_ms(&self, app_class: &str, is_xwayland: bool) -> u64 {
        let needle = app_class.trim().to_ascii_lowercase();
        if !needle.is_empty() {
            if let Some(&ms) = self.delays.apps.get(&needle) {
                return ms;
            }
            let mut best: Option<(usize, u64)> = None;
            for (pattern, &ms) in &self.delays.apps {
                let pat = pattern.to_ascii_lowercase();
                if needle.contains(&pat) {
                    let len = pat.len();
                    if best.map_or(true, |(best_len, _)| len > best_len) {
                        best = Some((len, ms));
                    }
                }
            }
            if let Some((_, ms)) = best {
                return ms;
            }
        }
        if is_xwayland {
            return self.delays.xwayland_ms;
        }
        self.delays.default_ms
    }

    pub fn is_passthrough_app(&self, app_class: &str) -> bool {
        let needle = app_class.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return false;
        }
        self.general.passthrough_apps.iter().any(|raw| {
            let pat = raw.trim().to_ascii_lowercase();
            !pat.is_empty() && needle.contains(&pat)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_toml_and_resolves_app_delays() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.hotkey.toggle, ToggleHotkey::None);
        assert!(cfg.general.sync_desktop_layout);
        assert!(cfg.general.auto_restore_non_vn);
        assert_eq!(cfg.resolve_delay_ms("foot", false), 0);
        assert_eq!(cfg.resolve_delay_ms("Alacritty", false), 1);
        assert_eq!(cfg.resolve_delay_ms("chromium-browser", false), 4);
        assert_eq!(cfg.resolve_delay_ms("Code", false), 5);
        assert_eq!(cfg.resolve_delay_ms("unknown-x11-app", true), 8);
        assert_eq!(cfg.resolve_delay_ms("unknown-wayland-app", false), 2);

        let default_toml = include_str!("../config/default_config.toml");
        let parsed: AppConfig = toml::from_str(default_toml).expect("parse default_config.toml");
        assert_eq!(parsed.hotkey.toggle, ToggleHotkey::None);
        assert!(parsed.general.sync_desktop_layout);
        assert!(parsed.general.auto_restore_non_vn);
        assert!(parsed.general.passthrough_apps.is_empty());
        assert!(!parsed.is_passthrough_app("steam_app_1568590"));

        let mut with_games = parsed;
        with_games.general.passthrough_apps = vec!["steam_app_".to_string(), "Minecraft".to_string()];
        assert!(with_games.is_passthrough_app("steam_app_1568590"));
        assert!(with_games.is_passthrough_app("Minecraft* 1.21.4"));
        assert!(!with_games.is_passthrough_app("firefox"));
    }
}
