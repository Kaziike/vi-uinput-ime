mod config;
mod engine;
mod input;
mod ipc;
mod xkb_gen;

use anyhow::Result;
use config::{AppConfig, ToggleHotkey};
use engine::vni::VniEngine;
use engine::InputEngine;
use evdev::Key;
use input::emitter::{spawn_fifo_emitter, OutputItem, UinputSink};
use input::grabber::{
    emergency_ungrab_all, install_failsafe_panic_hook, spawn_device_reader, RouterEvent,
};
use input::scanner::{spawn_hotplug_scanner, DeviceFilter};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::{info, warn};

#[derive(Default)]
struct ModifierState {
    ctrl_left: bool,
    ctrl_right: bool,
    alt_left: bool,
    alt_right: bool,
    super_left: bool,
    super_right: bool,
    shift_left: bool,
    shift_right: bool,
    caps_lock: bool,
}

impl ModifierState {
    fn update(&mut self, code: Key, value: i32) -> bool {
        let pressed = value != 0;
        match code {
            Key::KEY_LEFTCTRL => { self.ctrl_left = pressed; true }
            Key::KEY_RIGHTCTRL => { self.ctrl_right = pressed; true }
            Key::KEY_LEFTALT => { self.alt_left = pressed; true }
            Key::KEY_RIGHTALT => { self.alt_right = pressed; true }
            Key::KEY_LEFTMETA => { self.super_left = pressed; true }
            Key::KEY_RIGHTMETA => { self.super_right = pressed; true }
            Key::KEY_LEFTSHIFT => { self.shift_left = pressed; true }
            Key::KEY_RIGHTSHIFT => { self.shift_right = pressed; true }
            Key::KEY_CAPSLOCK => {
                if value == 1 {
                    self.caps_lock = !self.caps_lock;
                }
                true
            }
            _ => false,
        }
    }

    fn ctrl(&self) -> bool {
        self.ctrl_left || self.ctrl_right
    }
    fn alt(&self) -> bool {
        self.alt_left || self.alt_right
    }
    fn super_key(&self) -> bool {
        self.super_left || self.super_right
    }
    fn shift(&self) -> bool {
        self.shift_left || self.shift_right
    }
    fn any_shortcut_modifier(&self) -> bool {
        self.ctrl() || self.alt() || self.super_key()
    }
}

fn is_context_reset_key(code: Key) -> bool {
    matches!(
        code,
        Key::KEY_ESC
            | Key::KEY_TAB
            | Key::KEY_ENTER
            | Key::KEY_KPENTER
            | Key::KEY_UP
            | Key::KEY_DOWN
            | Key::KEY_LEFT
            | Key::KEY_RIGHT
            | Key::KEY_HOME
            | Key::KEY_END
            | Key::KEY_PAGEUP
            | Key::KEY_PAGEDOWN
            | Key::KEY_DELETE
    )
}

/// VNI intercepts ASCII letters (`KEY_A..KEY_Z`) and unshifted top-row digits (`KEY_0..KEY_9`).
/// Numpad keys (`KEY_KP0..KEY_KP9`) and shifted symbols (`!@#$%^&*()`) always pass straight through.
fn key_to_vni_char(code: Key, shift: bool, caps_lock: bool) -> Option<char> {
    let alpha = match code {
        Key::KEY_A => Some('a'),
        Key::KEY_B => Some('b'),
        Key::KEY_C => Some('c'),
        Key::KEY_D => Some('d'),
        Key::KEY_E => Some('e'),
        Key::KEY_F => Some('f'),
        Key::KEY_G => Some('g'),
        Key::KEY_H => Some('h'),
        Key::KEY_I => Some('i'),
        Key::KEY_J => Some('j'),
        Key::KEY_K => Some('k'),
        Key::KEY_L => Some('l'),
        Key::KEY_M => Some('m'),
        Key::KEY_N => Some('n'),
        Key::KEY_O => Some('o'),
        Key::KEY_P => Some('p'),
        Key::KEY_Q => Some('q'),
        Key::KEY_R => Some('r'),
        Key::KEY_S => Some('s'),
        Key::KEY_T => Some('t'),
        Key::KEY_U => Some('u'),
        Key::KEY_V => Some('v'),
        Key::KEY_W => Some('w'),
        Key::KEY_X => Some('x'),
        Key::KEY_Y => Some('y'),
        Key::KEY_Z => Some('z'),
        _ => None,
    };

    if let Some(ch) = alpha {
        let upper = shift ^ caps_lock;
        return Some(if upper { ch.to_ascii_uppercase() } else { ch });
    }

    if !shift {
        return match code {
            Key::KEY_0 => Some('0'),
            Key::KEY_1 => Some('1'),
            Key::KEY_2 => Some('2'),
            Key::KEY_3 => Some('3'),
            Key::KEY_4 => Some('4'),
            Key::KEY_5 => Some('5'),
            Key::KEY_6 => Some('6'),
            Key::KEY_7 => Some('7'),
            Key::KEY_8 => Some('8'),
            Key::KEY_9 => Some('9'),
            _ => None,
        };
    }

    None
}

struct VniRouter {
    engine: VniEngine,
    mods: ModifierState,
    manual_enabled: bool,
    desktop_layout_active: bool,
    sync_desktop_layout: bool,
    toggle_hotkey: ToggleHotkey,
    /// Tracks every non-modifier key whose `value == 1` press was forwarded as `PassThrough`.
    /// Guarantees that its matching repeat (`value == 2`) and release (`value == 0`) are ALWAYS
    /// forwarded cleanly—eliminating stuck number keys (`1111...`) and Shift/Ctrl release traps.
    passthrough_held: HashSet<Key>,
}

impl VniRouter {
    fn new(tone_style: config::ToneStyle, manual_enabled: bool, toggle_hotkey: ToggleHotkey) -> Self {
        Self {
            engine: VniEngine::new(tone_style),
            mods: ModifierState::default(),
            manual_enabled,
            desktop_layout_active: true,
            sync_desktop_layout: false,
            toggle_hotkey,
            passthrough_held: HashSet::new(),
        }
    }

    fn with_desktop_layout_sync(mut self, sync: bool) -> Self {
        self.sync_desktop_layout = sync;
        if sync && self.toggle_hotkey == ToggleHotkey::None {
            // Fail-safe PassThrough until user-session `--ipc-bridge` confirms `vi_uinput` is active
            self.desktop_layout_active = false;
        }
        self
    }

    fn effective_enabled(&self) -> bool {
        self.manual_enabled && self.desktop_layout_active
    }

    fn set_desktop_layout_active(&mut self, active: bool) -> bool {
        self.desktop_layout_active = active;
        self.engine.clear_all();
        self.effective_enabled()
    }

    fn on_ipc_bridge_disconnected(&mut self) -> Option<bool> {
        if self.sync_desktop_layout && self.toggle_hotkey == ToggleHotkey::None {
            self.desktop_layout_active = false;
            self.engine.clear_all();
            return Some(self.effective_enabled());
        }
        None
    }

    fn clear_all(&mut self) {
        self.engine.clear_all();
    }

    fn sync_caps_lock(&mut self, on: bool) {
        self.mods.caps_lock = on;
    }

    fn handle_raw_key(&mut self, code: Key, value: i32) -> (Vec<OutputItem>, Option<bool>) {
        // 1. Always track & forward modifier keys verbatim
        if self.mods.update(code, value) {
            return (vec![OutputItem::PassThrough { code, value }], None);
        }

        // 2. Key release (`value == 0`): ALWAYS forward release iff its press was passed through!
        if value == 0 {
            if self.passthrough_held.remove(&code) {
                return (vec![OutputItem::PassThrough { code, value: 0 }], None);
            }
            return (Vec::new(), None);
        }

        // 3. Key repeat (`value == 2`) for already passed-through keys EXCEPT Backspace/Space
        //    (Backspace and Space must update the history ring buffer on every repeat tick).
        if value == 2
            && !matches!(code, Key::KEY_BACKSPACE | Key::KEY_SPACE)
            && self.passthrough_held.contains(&code)
        {
            return (vec![OutputItem::PassThrough { code, value: 2 }], None);
        }

        // 4. IME toggle hotkey (`none`, `Ctrl+Space`, or `Super+Space`)
        let toggle_hit = code == Key::KEY_SPACE
            && value == 1
            && match self.toggle_hotkey {
                ToggleHotkey::None => false,
                ToggleHotkey::CtrlSpace => {
                    self.mods.ctrl() && !self.mods.alt() && !self.mods.super_key()
                }
                ToggleHotkey::SuperSpace => {
                    self.mods.super_key() && !self.mods.ctrl() && !self.mods.alt()
                }
            };
        if toggle_hit {
            self.manual_enabled = !self.manual_enabled;
            self.engine.clear_all();
            let suppress = if self.mods.super_key() || self.mods.alt() {
                // Inject a harmless non-modifier key tap (KEY_F24) so Mutter/KWin observes
                // `Super down -> F24 down -> F24 up -> Super up` and cancels Overview/Launcher.
                vec![
                    OutputItem::PassThrough {
                        code: Key::KEY_F24,
                        value: 1,
                    },
                    OutputItem::PassThrough {
                        code: Key::KEY_F24,
                        value: 0,
                    },
                ]
            } else {
                Vec::new()
            };
            return (suppress, Some(self.effective_enabled()));
        }

        // 5. Strict invalidation & PassThrough when IME disabled/paused, Ctrl/Alt/Super held
        //    (e.g. Ctrl+Backspace, Ctrl+Space, Super+Space), or navigation key pressed.
        if !self.effective_enabled() || self.mods.any_shortcut_modifier() || is_context_reset_key(code) {
            if value == 1 || value == 2 {
                self.engine.clear_all();
            }
            if value == 1 {
                self.passthrough_held.insert(code);
            }
            return (vec![OutputItem::PassThrough { code, value }], None);
        }

        // 6. Space separator: push active word + Separator(' ') into history ring buffer (including repeat!)
        if code == Key::KEY_SPACE {
            if value == 1 || value == 2 {
                self.engine.process_separator(' ');
            }
            if value == 1 {
                self.passthrough_held.insert(code);
            }
            return (vec![OutputItem::PassThrough { code, value }], None);
        }

        // 7. Physical Backspace (supports Word History Restoration + smooth raw repeat)
        if code == Key::KEY_BACKSPACE {
            if value == 1 || value == 2 {
                if let Some(mut batch) = self.engine.process_backspace() {
                    if self.passthrough_held.remove(&Key::KEY_BACKSPACE) {
                        batch.insert(
                            0,
                            OutputItem::PassThrough {
                                code: Key::KEY_BACKSPACE,
                                value: 0,
                            },
                        );
                    }
                    return (batch, None);
                }
                if value == 2 && !self.passthrough_held.contains(&Key::KEY_BACKSPACE) {
                    return (vec![OutputItem::Backspace], None);
                }
            }
            if value == 1 {
                self.passthrough_held.insert(code);
            }
            return (vec![OutputItem::PassThrough { code, value }], None);
        }

        // 8. VNI letters (`a..z`, `A..Z`) and unshifted top-row digits (`0..9`)
        if let Some(ch) = key_to_vni_char(code, self.mods.shift(), self.mods.caps_lock) {
            if value == 2 && ch.is_ascii_digit() {
                return (Vec::new(), None);
            }
            if let Some(batch) = self.engine.process_char(ch) {
                return (batch, None);
            }
            if value == 1 {
                self.passthrough_held.insert(code);
            }
            return (vec![OutputItem::PassThrough { code, value }], None);
        }

        // 9. Numpad digits, shifted symbols (`!@#`), punctuation -> clear_all() & pass through cleanly
        if value == 1 || value == 2 {
            self.engine.clear_all();
        }
        if value == 1 {
            self.passthrough_held.insert(code);
        }
        (vec![OutputItem::PassThrough { code, value }], None)
    }
}

fn write_state_file(path: &str, enabled: bool) {
    let label = if enabled { "VI\n" } else { "EN\n" };
    if let Some(parent) = PathBuf::from(path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(err) = fs::write(path, label) {
        warn!("Failed to update IME state file '{}': {}", path, err);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    install_failsafe_panic_hook();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "vi_uinput_ime_vni=info".into()),
        )
        .init();

    let args: Vec<String> = env::args().collect();

    if let Some(pos) = args.iter().position(|a| a == "--generate-xkb") {
        let out_path = args
            .get(pos + 1)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("config/vn_virtual.xkb"));
        let xkb = xkb_gen::generate_xkb_keymap();
        fs::write(&out_path, xkb)?;
        info!("Generated 134-char standalone XKB keymap at {}", out_path.display());
        return Ok(());
    }

    if args.iter().any(|a| a == "--install-gnome-xkb") {
        let is_root = unsafe { libc::geteuid() == 0 };
        let base_dir = if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
            PathBuf::from(xdg).join("xkb")
        } else if is_root {
            PathBuf::from("/etc/xkb")
        } else {
            PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".to_string()))
                .join(".config")
                .join("xkb")
        };
        let symbols_dir = base_dir.join("symbols");
        let types_dir = base_dir.join("types");
        let rules_dir = base_dir.join("rules");
        fs::create_dir_all(&symbols_dir)?;
        fs::create_dir_all(&types_dir)?;
        fs::create_dir_all(&rules_dir)?;
        let sym_path = symbols_dir.join("vi_uinput");
        let types_path = types_dir.join("complete");
        let xml_path = rules_dir.join("evdev.xml");
        fs::write(&sym_path, xkb_gen::generate_gnome_xkb_symbols())?;
        fs::write(&types_path, xkb_gen::generate_gnome_xkb_types())?;
        fs::write(&xml_path, xkb_gen::generate_gnome_evdev_xml())?;
        info!("Installed GNOME/KDE XKB RMLVO layout (with CapsLock-immune Levels 5-8):");
        info!("  {}", sym_path.display());
        info!("  {}", types_path.display());
        info!("  {}", xml_path.display());
        return Ok(());
    }

    if args.iter().any(|a| a == "--install-x11-xkb") {
        let root_dir = std::path::Path::new("/usr/share/X11/xkb");
        xkb_gen::install_x11_system_xkb(root_dir)?;
        info!("Installed native X11 / Xorg XKB layout into {}:", root_dir.display());
        info!("  {}/symbols/vi_uinput (multi-group safe)", root_dir.display());
        info!("  {}/types/vi_uinput + patched types/complete", root_dir.display());
        info!("  Registered `vi_uinput` in rules/evdev.xml & rules/evdev.lst");
        return Ok(());
    }

    if args.iter().any(|a| a == "--uninstall-x11-xkb") {
        let root_dir = std::path::Path::new("/usr/share/X11/xkb");
        xkb_gen::uninstall_x11_system_xkb(root_dir)?;
        info!("Removed native X11 / Xorg XKB layout and reverted patches in {}", root_dir.display());
        return Ok(());
    }

    let config = if let Some(pos) = args.iter().position(|a| a == "--config") {
        let path = PathBuf::from(args.get(pos + 1).expect("missing path after --config"));
        AppConfig::load_from_file(&path)?
    } else {
        AppConfig::default()
    };

    if args.iter().any(|a| a == "--ipc-bridge") {
        let socket_path = if let Some(pos) = args.iter().position(|a| a == "--ipc-socket") {
            PathBuf::from(args.get(pos + 1).expect("missing path after --ipc-socket"))
        } else {
            PathBuf::from(&config.general.ipc_socket_path)
        };
        return ipc::bridge::run_ipc_bridge(socket_path).await;
    }

    let sink = UinputSink::create()?;
    let (emitter, _emitter_task) = spawn_fifo_emitter(sink, config.delays.default_ms);

    let (router_tx, mut router_rx) = mpsc::unbounded_channel::<RouterEvent>();
    let (dev_tx, mut dev_rx) = mpsc::unbounded_channel();
    let device_filter = DeviceFilter::new(
        config.general.device_include.clone(),
        config.general.device_exclude.clone(),
    );
    spawn_hotplug_scanner(dev_tx, device_filter)?;

    let device_router_tx = router_tx.clone();
    tokio::spawn(async move {
        while let Some(dev) = dev_rx.recv().await {
            spawn_device_reader(dev, device_router_tx.clone());
        }
    });

    // Listen for desktop user-session `--ipc-bridge` events over UNIX socket
    let ipc_socket_path = PathBuf::from(&config.general.ipc_socket_path);
    let _ipc_server = match ipc::bridge::spawn_ipc_server(&ipc_socket_path, router_tx.clone()) {
        Ok(handle) => Some(handle),
        Err(err) => {
            warn!(
                "Could not bind IPC bridge UNIX socket '{}': {:#} (continuing with direct IPC fallback)",
                ipc_socket_path.display(),
                err
            );
            None
        }
    };

    // Direct IPC fallback when daemon is launched manually inside a desktop session terminal
    ipc::spawn_active_window_monitor(router_tx.clone());

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut router = VniRouter::new(
        config.general.tone_style,
        config.general.enabled_on_start,
        config.hotkey.toggle,
    )
    .with_desktop_layout_sync(config.general.sync_desktop_layout);

    write_state_file(&config.general.state_file, router.effective_enabled());

    info!(
        "vi-uinput-ime-vni started (tone={:?}, toggle={:?}, sync_desktop_layout={}, active={}, default_delay={}ms, ipc_socket={})",
        config.general.tone_style,
        config.hotkey.toggle,
        config.general.sync_desktop_layout,
        router.effective_enabled(),
        config.delays.default_ms,
        config.general.ipc_socket_path
    );

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT — releasing all grabbed keyboards and exiting.");
                emergency_ungrab_all();
                let _ = fs::remove_file(&ipc_socket_path);
                break;
            }
            _ = sigterm.recv() => {
                info!("Received SIGTERM — releasing all grabbed keyboards and exiting.");
                emergency_ungrab_all();
                let _ = fs::remove_file(&ipc_socket_path);
                break;
            }
            Some(event) = router_rx.recv() => {
                match event {
                    RouterEvent::CapsLockSync(on) => {
                        router.sync_caps_lock(on);
                        emitter.enqueue(OutputItem::SyncCapsLock(on));
                    }
                    RouterEvent::LayoutStateChanged { active, layout_id } => {
                        if config.general.sync_desktop_layout {
                            let eff = router.set_desktop_layout_active(active);
                            write_state_file(&config.general.state_file, eff);
                            info!(
                                "Desktop layout '{}' -> VNI IME {}",
                                layout_id,
                                if eff { "ACTIVE (VI)" } else { "PAUSED (PassThrough)" }
                            );
                        }
                    }
                    RouterEvent::IpcBridgeDisconnected => {
                        if let Some(eff) = router.on_ipc_bridge_disconnected() {
                            write_state_file(&config.general.state_file, eff);
                            info!("IPC bridge disconnected -> VNI IME auto-paused (PassThrough)");
                        }
                    }
                    RouterEvent::MouseReset | RouterEvent::WorkspaceChanged => {
                        router.clear_all();
                    }
                    RouterEvent::WindowFocusChanged { app_class, is_xwayland } => {
                        router.clear_all();
                        let delay_ms = config.resolve_delay_ms(&app_class, is_xwayland);
                        emitter.set_delay_ms(delay_ms);
                    }
                    RouterEvent::RawKey { code, value } => {
                        let (items, toggled) = router.handle_raw_key(code, value);
                        if !items.is_empty() {
                            emitter.enqueue_batch(items);
                        }
                        if let Some(enabled) = toggled {
                            write_state_file(&config.general.state_file, enabled);
                            info!("Toggled VNI IME -> {}", if enabled { "VI" } else { "EN" });
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_router_output(
        screen: &mut Vec<char>,
        virtual_held: &mut HashSet<Key>,
        items: &[OutputItem],
    ) {
        for item in items {
            match *item {
                OutputItem::SyncCapsLock(_) => {}
                OutputItem::Backspace => {
                    screen.pop();
                }
                OutputItem::VietnameseChar(c) | OutputItem::AsciiChar(c) => {
                    screen.push(c);
                }
                OutputItem::PassThrough { code, value } => {
                    if value == 1 {
                        virtual_held.insert(code);
                    } else if value == 0 {
                        virtual_held.remove(&code);
                    }
                    if value == 1 || value == 2 {
                        match code {
                            Key::KEY_SPACE => screen.push(' '),
                            Key::KEY_BACKSPACE => {
                                screen.pop();
                            }
                            Key::KEY_6 => screen.push('6'),
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    fn tap_key(
        router: &mut VniRouter,
        screen: &mut Vec<char>,
        virtual_held: &mut HashSet<Key>,
        code: Key,
    ) -> Vec<OutputItem> {
        let (down, _) = router.handle_raw_key(code, 1);
        apply_router_output(screen, virtual_held, &down);
        let (up, _) = router.handle_raw_key(code, 0);
        apply_router_output(screen, virtual_held, &up);
        down
    }

    #[test]
    fn vni_router_types_numbers_and_never_sticks_keys() {
        let mut router = VniRouter::new(config::ToneStyle::New, true, ToggleHotkey::CtrlSpace);
        let mut virtual_held: HashSet<Key> = HashSet::new();
        let mut emitted_digits = 0usize;

        let sequence = [
            (Key::KEY_1, 1), (Key::KEY_1, 0),
            (Key::KEY_2, 1), (Key::KEY_2, 2), (Key::KEY_2, 0),
            (Key::KEY_3, 1), (Key::KEY_3, 0),
            (Key::KEY_B, 1), (Key::KEY_B, 0),
            (Key::KEY_1, 1), (Key::KEY_1, 0),
            (Key::KEY_T, 1), (Key::KEY_T, 0),
            (Key::KEY_R, 1), (Key::KEY_R, 0),
            (Key::KEY_A, 1), (Key::KEY_A, 0),
            (Key::KEY_6, 1), (Key::KEY_6, 2), (Key::KEY_6, 0),
            (Key::KEY_U, 1), (Key::KEY_U, 0),
            (Key::KEY_SPACE, 1), (Key::KEY_SPACE, 0),
            (Key::KEY_A, 1), (Key::KEY_A, 0),
            (Key::KEY_1, 1), (Key::KEY_1, 0),
            (Key::KEY_1, 1), (Key::KEY_1, 0),
            (Key::KEY_LEFTSHIFT, 1),
            (Key::KEY_1, 1),
            (Key::KEY_LEFTSHIFT, 0),
            (Key::KEY_1, 0),
            (Key::KEY_LEFTCTRL, 1),
            (Key::KEY_A, 1),
            (Key::KEY_LEFTCTRL, 0),
            (Key::KEY_A, 0),
        ];

        for (code, val) in sequence {
            let (items, _) = router.handle_raw_key(code, val);
            for item in items {
                match item {
                    OutputItem::PassThrough { code: c, value: v } => {
                        if v == 1 {
                            virtual_held.insert(c);
                            if matches!(c, Key::KEY_1 | Key::KEY_2 | Key::KEY_3) {
                                emitted_digits += 1;
                            }
                        } else if v == 0 {
                            virtual_held.remove(&c);
                        }
                    }
                    OutputItem::AsciiChar(ch) if ch.is_ascii_digit() => {
                        emitted_digits += 1;
                    }
                    _ => {}
                }
            }
        }

        assert_eq!(emitted_digits, 6);
        assert!(virtual_held.is_empty(), "Stuck virtual keys detected: {:?}", virtual_held);
        assert!(router.passthrough_held.is_empty());
    }

    #[test]
    fn vni_router_history_restore_with_held_bs_repeat_and_strict_invalidation() {
        let mut router = VniRouter::new(config::ToneStyle::New, true, ToggleHotkey::CtrlSpace);
        let mut screen: Vec<char> = Vec::new();
        let mut held: HashSet<Key> = HashSet::new();

        // Type "Tôi Năng" in VNI (Shift+T, o, 6, i, Space, Shift+N, a, 8, n, g)
        let (items, _) = router.handle_raw_key(Key::KEY_LEFTSHIFT, 1);
        apply_router_output(&mut screen, &mut held, &items);
        tap_key(&mut router, &mut screen, &mut held, Key::KEY_T);
        let (items, _) = router.handle_raw_key(Key::KEY_LEFTSHIFT, 0);
        apply_router_output(&mut screen, &mut held, &items);
        for k in [Key::KEY_O, Key::KEY_6, Key::KEY_I, Key::KEY_SPACE] {
            tap_key(&mut router, &mut screen, &mut held, k);
        }
        let (items, _) = router.handle_raw_key(Key::KEY_LEFTSHIFT, 1);
        apply_router_output(&mut screen, &mut held, &items);
        tap_key(&mut router, &mut screen, &mut held, Key::KEY_N);
        let (items, _) = router.handle_raw_key(Key::KEY_LEFTSHIFT, 0);
        apply_router_output(&mut screen, &mut held, &items);
        for k in [Key::KEY_A, Key::KEY_8, Key::KEY_N, Key::KEY_G] {
            tap_key(&mut router, &mut screen, &mut held, k);
        }
        assert_eq!(screen.iter().collect::<String>(), "Tôi Năng");

        // Hold Backspace (1 press + 4 repeat ticks = 5 deletes) -> restores "Tôi"
        let (items, _) = router.handle_raw_key(Key::KEY_BACKSPACE, 1);
        apply_router_output(&mut screen, &mut held, &items);
        for _ in 0..4 {
            let (items, _) = router.handle_raw_key(Key::KEY_BACKSPACE, 2);
            apply_router_output(&mut screen, &mut held, &items);
        }
        let (items, _) = router.handle_raw_key(Key::KEY_BACKSPACE, 0);
        apply_router_output(&mut screen, &mut held, &items);
        assert_eq!(screen.iter().collect::<String>(), "Tôi");

        // Press '1' -> transforms restored "Tôi" into "Tối"
        tap_key(&mut router, &mut screen, &mut held, Key::KEY_1);
        assert_eq!(screen.iter().collect::<String>(), "Tối");

        // Press Space, then simulate IPC WindowFocusChanged / MouseReset -> clear_all()
        tap_key(&mut router, &mut screen, &mut held, Key::KEY_SPACE);
        assert_eq!(router.engine.history_len(), 2);
        router.clear_all();
        assert_eq!(router.engine.history_len(), 0);

        // Press Backspace + '6' -> does NOT modify "Tối"; '6' passes through cleanly without sticking!
        tap_key(&mut router, &mut screen, &mut held, Key::KEY_BACKSPACE);
        let batch = tap_key(&mut router, &mut screen, &mut held, Key::KEY_6);
        assert_eq!(
            batch,
            vec![OutputItem::PassThrough {
                code: Key::KEY_6,
                value: 1,
            }]
        );
        assert_eq!(screen.iter().collect::<String>(), "Tối6");

        assert!(held.is_empty(), "Stuck virtual keys: {:?}", held);
        assert!(router.passthrough_held.is_empty());
    }

    #[test]
    fn super_space_toggles_ime_swallows_space_and_emits_f24_tap() {
        let mut router = VniRouter::new(config::ToneStyle::New, true, ToggleHotkey::SuperSpace);
        let mut screen: Vec<char> = Vec::new();
        let mut held: HashSet<Key> = HashSet::new();

        // Hold Super (KEY_LEFTMETA)
        let (meta_down, toggled) = router.handle_raw_key(Key::KEY_LEFTMETA, 1);
        assert_eq!(toggled, None);
        assert_eq!(
            meta_down,
            vec![OutputItem::PassThrough {
                code: Key::KEY_LEFTMETA,
                value: 1,
            }]
        );
        apply_router_output(&mut screen, &mut held, &meta_down);

        // Press Space while Super is held -> toggles IME OFF, swallows Space, emits KEY_F24 tap
        let (space_down, toggled) = router.handle_raw_key(Key::KEY_SPACE, 1);
        assert_eq!(toggled, Some(false));
        assert!(!router.effective_enabled());
        assert_eq!(
            space_down,
            vec![
                OutputItem::PassThrough {
                    code: Key::KEY_F24,
                    value: 1,
                },
                OutputItem::PassThrough {
                    code: Key::KEY_F24,
                    value: 0,
                },
            ]
        );
        apply_router_output(&mut screen, &mut held, &space_down);

        // Release Space -> swallowed cleanly (no stray Space release)
        let (space_up, toggled) = router.handle_raw_key(Key::KEY_SPACE, 0);
        assert_eq!(toggled, None);
        assert!(space_up.is_empty());

        // Release Super -> passed through cleanly
        let (meta_up, toggled) = router.handle_raw_key(Key::KEY_LEFTMETA, 0);
        assert_eq!(toggled, None);
        assert_eq!(
            meta_up,
            vec![OutputItem::PassThrough {
                code: Key::KEY_LEFTMETA,
                value: 0,
            }]
        );
        apply_router_output(&mut screen, &mut held, &meta_up);

        assert!(screen.is_empty(), "Space must be swallowed, got {:?}", screen);
        assert!(held.is_empty(), "Stuck virtual keys: {:?}", held);
        assert!(router.passthrough_held.is_empty());

        // Toggle back ON with Ctrl+Space router -> must NOT emit KEY_F24
        let mut ctrl_router =
            VniRouter::new(config::ToneStyle::New, true, ToggleHotkey::CtrlSpace);
        ctrl_router.handle_raw_key(Key::KEY_LEFTCTRL, 1);
        let (ctrl_space_down, toggled) = ctrl_router.handle_raw_key(Key::KEY_SPACE, 1);
        assert_eq!(toggled, Some(false));
        assert!(ctrl_space_down.is_empty());
    }

    #[test]
    fn desktop_layout_sync_pauses_to_passthrough_and_toggle_none_never_swallows_shortcuts() {
        let mut router = VniRouter::new(config::ToneStyle::New, true, ToggleHotkey::None)
            .with_desktop_layout_sync(true);
        let mut screen: Vec<char> = Vec::new();
        let mut held: HashSet<Key> = HashSet::new();

        // Fail-safe: starts paused until desktop bridge reports `vi_uinput` is active
        assert!(!router.effective_enabled());

        // ToggleHotkey::None must pass BOTH Super+Space (GNOME) and Ctrl+Space (IBus/Fcitx5)
        // straight through verbatim (NO swallowed Space, NO KEY_F24)
        for modifier in [Key::KEY_LEFTMETA, Key::KEY_LEFTCTRL] {
            let (m_down, t1) = router.handle_raw_key(modifier, 1);
            assert_eq!(t1, None);
            apply_router_output(&mut screen, &mut held, &m_down);

            let (sp_down, t2) = router.handle_raw_key(Key::KEY_SPACE, 1);
            assert_eq!(t2, None);
            assert_eq!(
                sp_down,
                vec![OutputItem::PassThrough {
                    code: Key::KEY_SPACE,
                    value: 1,
                }]
            );
            apply_router_output(&mut screen, &mut held, &sp_down);

            let (sp_up, _) = router.handle_raw_key(Key::KEY_SPACE, 0);
            assert_eq!(
                sp_up,
                vec![OutputItem::PassThrough {
                    code: Key::KEY_SPACE,
                    value: 0,
                }]
            );
            apply_router_output(&mut screen, &mut held, &sp_up);

            let (m_up, _) = router.handle_raw_key(modifier, 0);
            apply_router_output(&mut screen, &mut held, &m_up);
        }
        assert!(!router.effective_enabled());
        screen.clear();

        // When paused (e.g. GNOME US layout), typing `s u a 7 3` emits ONLY raw PassThrough
        for k in [Key::KEY_S, Key::KEY_U, Key::KEY_A, Key::KEY_7, Key::KEY_3] {
            let (down, _) = router.handle_raw_key(k, 1);
            assert_eq!(down, vec![OutputItem::PassThrough { code: k, value: 1 }]);
            apply_router_output(&mut screen, &mut held, &down);
            let (up, _) = router.handle_raw_key(k, 0);
            assert_eq!(up, vec![OutputItem::PassThrough { code: k, value: 0 }]);
            apply_router_output(&mut screen, &mut held, &up);
        }

        // Desktop switches to `vi_uinput` -> VNI activates immediately (`sua73` -> "sửa")
        assert!(router.set_desktop_layout_active(true));
        for k in [Key::KEY_S, Key::KEY_U, Key::KEY_A, Key::KEY_7, Key::KEY_3] {
            tap_key(&mut router, &mut screen, &mut held, k);
        }
        assert_eq!(screen.iter().collect::<String>(), "sửa");

        // Desktop switches back to `us` or `ibus:Bamboo` -> clears word buffer & auto-pauses
        assert!(!router.set_desktop_layout_active(false));
        let (down, _) = router.handle_raw_key(Key::KEY_N, 1);
        assert_eq!(
            down,
            vec![OutputItem::PassThrough {
                code: Key::KEY_N,
                value: 1
            }]
        );
        let (up, _) = router.handle_raw_key(Key::KEY_N, 0);
        assert_eq!(
            up,
            vec![OutputItem::PassThrough {
                code: Key::KEY_N,
                value: 0
            }]
        );

        // Desktop switches to `vi_uinput`, then user logs out (IPC bridge disconnects) -> auto-pauses
        assert!(router.set_desktop_layout_active(true));
        assert_eq!(router.on_ipc_bridge_disconnected(), Some(false));
        assert!(!router.effective_enabled());

        assert!(held.is_empty(), "Stuck virtual keys: {:?}", held);
        assert!(router.passthrough_held.is_empty());
    }
}
