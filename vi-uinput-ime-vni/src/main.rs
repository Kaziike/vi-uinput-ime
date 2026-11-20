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
use input::scanner::spawn_hotplug_scanner;
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
    ime_enabled: bool,
    toggle_hotkey: ToggleHotkey,
    /// Tracks every non-modifier key whose `value == 1` press was forwarded as `PassThrough`.
    /// Guarantees that its matching repeat (`value == 2`) and release (`value == 0`) are ALWAYS
    /// forwarded cleanly—eliminating stuck number keys (`1111...`) and Shift/Ctrl release traps.
    passthrough_held: HashSet<Key>,
}

impl VniRouter {
    fn new(tone_style: config::ToneStyle, ime_enabled: bool, toggle_hotkey: ToggleHotkey) -> Self {
        Self {
            engine: VniEngine::new(tone_style),
            mods: ModifierState::default(),
            ime_enabled,
            toggle_hotkey,
            passthrough_held: HashSet::new(),
        }
    }

    fn handle_raw_key(&mut self, code: Key, value: i32) -> (Vec<OutputItem>, Option<bool>) {
        // 1. Always track & forward modifier keys verbatim
        if self.mods.update(code, value) {
            return (vec![OutputItem::PassThrough { code, value }], None);
        }

        // 2. Key release (`value == 0`): ALWAYS forward release iff its press was passed through!
        // This guarantees zero stuck keys even if Shift/Ctrl/IME state changed while holding a key.
        if value == 0 {
            if self.passthrough_held.remove(&code) {
                return (vec![OutputItem::PassThrough { code, value: 0 }], None);
            }
            return (Vec::new(), None);
        }

        // 3. Key repeat (`value == 2`) for an already passed-through key (e.g. holding `1` -> `1111`):
        // keep forwarding verbatim without re-entering VNI engine.
        if value == 2 && self.passthrough_held.contains(&code) {
            return (vec![OutputItem::PassThrough { code, value: 2 }], None);
        }

        // 4. IME toggle hotkey (`Ctrl+Space` or `Super+Space`)
        let toggle_hit = code == Key::KEY_SPACE
            && value == 1
            && match self.toggle_hotkey {
                ToggleHotkey::CtrlSpace => {
                    self.mods.ctrl() && !self.mods.alt() && !self.mods.super_key()
                }
                ToggleHotkey::SuperSpace => {
                    self.mods.super_key() && !self.mods.ctrl() && !self.mods.alt()
                }
            };
        if toggle_hit {
            self.ime_enabled = !self.ime_enabled;
            self.engine.reset();
            return (Vec::new(), Some(self.ime_enabled));
        }

        // 5. PassThrough when IME disabled, shortcut modifier held, or navigation reset key
        if !self.ime_enabled || self.mods.any_shortcut_modifier() || is_context_reset_key(code) {
            if value == 1 {
                self.engine.reset();
                self.passthrough_held.insert(code);
            }
            return (vec![OutputItem::PassThrough { code, value }], None);
        }

        // 6. Physical Backspace
        if code == Key::KEY_BACKSPACE {
            if value == 1 || value == 2 {
                self.engine.process_backspace();
            }
            if value == 1 {
                self.passthrough_held.insert(code);
            }
            return (vec![OutputItem::PassThrough { code, value }], None);
        }

        // 7. VNI letters (`a..z`, `A..Z`) and unshifted top-row digits (`0..9`)
        if let Some(ch) = key_to_vni_char(code, self.mods.shift(), self.mods.caps_lock) {
            // Do not auto-repeat a consumed VNI modifier digit (prevents accidental undo spam)
            if value == 2 && ch.is_ascii_digit() {
                return (Vec::new(), None);
            }
            if let Some(batch) = self.engine.process_char(ch) {
                return (batch, None);
            }
            // Engine returned None (standalone number like `123`, or non-applicable digit like `b1`):
            // forward as raw PassThrough AND register in `passthrough_held` so its release is guaranteed!
            if value == 1 {
                self.passthrough_held.insert(code);
            }
            return (vec![OutputItem::PassThrough { code, value }], None);
        }

        // 8. Numpad digits, shifted symbols (`!@#`), Space, punctuation -> reset & pass through cleanly
        if value == 1 {
            self.engine.reset();
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
        let rules_dir = base_dir.join("rules");
        fs::create_dir_all(&symbols_dir)?;
        fs::create_dir_all(&rules_dir)?;
        let sym_path = symbols_dir.join("vi_uinput");
        let xml_path = rules_dir.join("evdev.xml");
        fs::write(&sym_path, xkb_gen::generate_gnome_xkb_symbols())?;
        fs::write(&xml_path, xkb_gen::generate_gnome_evdev_xml())?;
        info!("Installed GNOME/KDE XKB RMLVO layout:");
        info!("  {}", sym_path.display());
        info!("  {}", xml_path.display());
        return Ok(());
    }

    let config = if let Some(pos) = args.iter().position(|a| a == "--config") {
        let path = PathBuf::from(args.get(pos + 1).expect("missing path after --config"));
        AppConfig::load_from_file(&path)?
    } else {
        AppConfig::default()
    };

    write_state_file(&config.general.state_file, config.general.enabled_on_start);

    let sink = UinputSink::create()?;
    let (emitter, _emitter_task) = spawn_fifo_emitter(sink, config.delays.default_ms);

    let (router_tx, mut router_rx) = mpsc::unbounded_channel::<RouterEvent>();
    let (dev_tx, mut dev_rx) = mpsc::unbounded_channel();
    spawn_hotplug_scanner(dev_tx)?;

    let device_router_tx = router_tx.clone();
    tokio::spawn(async move {
        while let Some(dev) = dev_rx.recv().await {
            spawn_device_reader(dev, device_router_tx.clone());
        }
    });

    ipc::spawn_active_window_monitor(router_tx.clone());

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut router = VniRouter::new(
        config.general.tone_style,
        config.general.enabled_on_start,
        config.hotkey.toggle,
    );

    info!(
        "vi-uinput-ime-vni started (tone={:?}, enabled={}, default_delay={}ms)",
        config.general.tone_style, config.general.enabled_on_start, config.delays.default_ms
    );

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT — releasing all grabbed keyboards and exiting.");
                emergency_ungrab_all();
                break;
            }
            _ = sigterm.recv() => {
                info!("Received SIGTERM — releasing all grabbed keyboards and exiting.");
                emergency_ungrab_all();
                break;
            }
            Some(event) = router_rx.recv() => {
                match event {
                    RouterEvent::MouseReset => {
                        router.engine.reset();
                    }
                    RouterEvent::WindowFocusChanged { app_class, is_xwayland } => {
                        router.engine.reset();
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

    #[test]
    fn vni_router_types_numbers_and_never_sticks_keys() {
        let mut router = VniRouter::new(config::ToneStyle::New, true, ToggleHotkey::CtrlSpace);
        let mut virtual_held: HashSet<Key> = HashSet::new();
        let mut emitted_digits = 0usize;

        let sequence = [
            // 1. Standalone digits `1`, `2`, `3` (including repeat) -> must pass through press+release
            (Key::KEY_1, 1), (Key::KEY_1, 0),
            (Key::KEY_2, 1), (Key::KEY_2, 2), (Key::KEY_2, 0),
            (Key::KEY_3, 1), (Key::KEY_3, 0),
            // 2. Non-applicable digit after consonant (`b1`) -> `1` press AND release both forwarded!
            (Key::KEY_B, 1), (Key::KEY_B, 0),
            (Key::KEY_1, 1), (Key::KEY_1, 0),
            // 3. Valid VNI word `tra6u` (with accidental repeat on `6`) -> `trâu`
            (Key::KEY_T, 1), (Key::KEY_T, 0),
            (Key::KEY_R, 1), (Key::KEY_R, 0),
            (Key::KEY_A, 1), (Key::KEY_A, 0),
            (Key::KEY_6, 1), (Key::KEY_6, 2), (Key::KEY_6, 0),
            (Key::KEY_U, 1), (Key::KEY_U, 0),
            (Key::KEY_SPACE, 1), (Key::KEY_SPACE, 0),
            // 4. Double-tap undo `a11` -> `a1`
            (Key::KEY_A, 1), (Key::KEY_A, 0),
            (Key::KEY_1, 1), (Key::KEY_1, 0),
            (Key::KEY_1, 1), (Key::KEY_1, 0),
            // 5. Shift+1 (`!`) where Shift is released BEFORE `1` (classic stuck-key trap!)
            (Key::KEY_LEFTSHIFT, 1),
            (Key::KEY_1, 1),
            (Key::KEY_LEFTSHIFT, 0),
            (Key::KEY_1, 0),
            // 6. Ctrl+A where Ctrl is released BEFORE `A`
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

        // Expect: 1, 2, 3 (standalone) + 1 (after b) + 1 (undo a11) + 1 (Shift+1) = 6 digits emitted
        assert_eq!(emitted_digits, 6);
        assert!(virtual_held.is_empty(), "Stuck virtual keys detected: {:?}", virtual_held);
        assert!(router.passthrough_held.is_empty());
    }
}
