mod config;
mod engine;
mod input;
mod ipc;
mod xkb_gen;

use anyhow::Result;
use config::{AppConfig, ToggleHotkey};
use engine::telex::TelexEngine;
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

/// Telex ONLY intercepts ASCII letter keys (`KEY_A..KEY_Z`).
/// Digits (`KEY_0..KEY_9`, `KEY_KP0..KEY_KP9`) and punctuation always pass straight through untouched.
fn key_to_telex_char(code: Key, shift: bool, caps_lock: bool) -> Option<char> {
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
    }?;
    let upper = shift ^ caps_lock;
    Some(if upper { alpha.to_ascii_uppercase() } else { alpha })
}

struct TelexRouter {
    engine: TelexEngine,
    mods: ModifierState,
    ime_enabled: bool,
    toggle_hotkey: ToggleHotkey,
    /// Tracks every non-modifier key whose `value == 1` press was forwarded as `PassThrough`,
    /// guaranteeing its matching repeat (`value == 2`) and release (`value == 0`) are NEVER swallowed.
    passthrough_held: HashSet<Key>,
}

impl TelexRouter {
    fn new(tone_style: config::ToneStyle, ime_enabled: bool, toggle_hotkey: ToggleHotkey) -> Self {
        Self {
            engine: TelexEngine::new(tone_style),
            mods: ModifierState::default(),
            ime_enabled,
            toggle_hotkey,
            passthrough_held: HashSet::new(),
        }
    }

    fn clear_all(&mut self) {
        self.engine.clear_all();
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
            self.engine.clear_all();
            return (Vec::new(), Some(self.ime_enabled));
        }

        // 5. Strict invalidation & PassThrough when IME disabled, Ctrl/Alt/Super held (e.g. Ctrl+Backspace),
        //    or navigation / linear-flow-breaking key pressed.
        if !self.ime_enabled || self.mods.any_shortcut_modifier() || is_context_reset_key(code) {
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

        // 8. Telex ASCII letters (`a..z`, `A..Z`)
        if let Some(ch) = key_to_telex_char(code, self.mods.shift(), self.mods.caps_lock) {
            if let Some(batch) = self.engine.process_char(ch) {
                return (batch, None);
            }
            if value == 1 {
                self.passthrough_held.insert(code);
            }
            return (vec![OutputItem::PassThrough { code, value }], None);
        }

        // 9. Digits, punctuation, and other untracked keys -> clear_all() and pass through cleanly
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
                .unwrap_or_else(|_| "vi_uinput_ime_telex=info".into()),
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
    let mut router = TelexRouter::new(
        config.general.tone_style,
        config.general.enabled_on_start,
        config.hotkey.toggle,
    );

    info!(
        "vi-uinput-ime-telex started (tone={:?}, enabled={}, default_delay={}ms)",
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
                            info!("Toggled Telex IME -> {}", if enabled { "VI" } else { "EN" });
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
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    fn tap_key(
        router: &mut TelexRouter,
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
    fn telex_router_never_sticks_digits_or_ctrl_shortcuts() {
        let mut router = TelexRouter::new(config::ToneStyle::New, true, ToggleHotkey::CtrlSpace);
        let mut virtual_held: HashSet<Key> = HashSet::new();

        let sequence = [
            (Key::KEY_1, 1),
            (Key::KEY_1, 0),
            (Key::KEY_2, 1),
            (Key::KEY_2, 2),
            (Key::KEY_2, 0),
            (Key::KEY_LEFTCTRL, 1),
            (Key::KEY_A, 1),
            (Key::KEY_LEFTCTRL, 0),
            (Key::KEY_A, 0),
            (Key::KEY_T, 1), (Key::KEY_T, 0),
            (Key::KEY_R, 1), (Key::KEY_R, 0),
            (Key::KEY_A, 1), (Key::KEY_A, 0),
            (Key::KEY_A, 1), (Key::KEY_A, 0),
            (Key::KEY_U, 1), (Key::KEY_U, 0),
        ];

        for (code, val) in sequence {
            let (items, _) = router.handle_raw_key(code, val);
            for item in items {
                if let OutputItem::PassThrough { code: c, value: v } = item {
                    if v == 1 {
                        virtual_held.insert(c);
                    } else if v == 0 {
                        virtual_held.remove(&c);
                    }
                }
            }
        }

        assert!(virtual_held.is_empty(), "Stuck virtual keys: {:?}", virtual_held);
        assert!(router.passthrough_held.is_empty());
    }

    #[test]
    fn telex_router_history_restore_with_held_bs_repeat_and_strict_invalidation() {
        let mut router = TelexRouter::new(config::ToneStyle::New, true, ToggleHotkey::CtrlSpace);
        let mut screen: Vec<char> = Vec::new();
        let mut held: HashSet<Key> = HashSet::new();

        // Type "Tôi Năng" using raw evdev keycodes (Shift+T, o, o, i, Space, Shift+N, a, w, n, g)
        let (items, _) = router.handle_raw_key(Key::KEY_LEFTSHIFT, 1);
        apply_router_output(&mut screen, &mut held, &items);
        tap_key(&mut router, &mut screen, &mut held, Key::KEY_T);
        let (items, _) = router.handle_raw_key(Key::KEY_LEFTSHIFT, 0);
        apply_router_output(&mut screen, &mut held, &items);
        for k in [Key::KEY_O, Key::KEY_O, Key::KEY_I, Key::KEY_SPACE] {
            tap_key(&mut router, &mut screen, &mut held, k);
        }
        let (items, _) = router.handle_raw_key(Key::KEY_LEFTSHIFT, 1);
        apply_router_output(&mut screen, &mut held, &items);
        tap_key(&mut router, &mut screen, &mut held, Key::KEY_N);
        let (items, _) = router.handle_raw_key(Key::KEY_LEFTSHIFT, 0);
        apply_router_output(&mut screen, &mut held, &items);
        for k in [Key::KEY_A, Key::KEY_W, Key::KEY_N, Key::KEY_G] {
            tap_key(&mut router, &mut screen, &mut held, k);
        }
        assert_eq!(screen.iter().collect::<String>(), "Tôi Năng");

        // Hold Backspace (1 initial press + 4 key-repeat ticks = 5 deletes) -> restores "Tôi"
        let (items, _) = router.handle_raw_key(Key::KEY_BACKSPACE, 1);
        apply_router_output(&mut screen, &mut held, &items);
        for _ in 0..4 {
            let (items, _) = router.handle_raw_key(Key::KEY_BACKSPACE, 2);
            apply_router_output(&mut screen, &mut held, &items);
        }
        let (items, _) = router.handle_raw_key(Key::KEY_BACKSPACE, 0);
        apply_router_output(&mut screen, &mut held, &items);
        assert_eq!(screen.iter().collect::<String>(), "Tôi");

        // Press 's' -> transforms restored "Tôi" into "Tối"
        tap_key(&mut router, &mut screen, &mut held, Key::KEY_S);
        assert_eq!(screen.iter().collect::<String>(), "Tối");

        // Press Space, then simulate IPC WindowFocusChanged / MouseReset -> clear_all()
        tap_key(&mut router, &mut screen, &mut held, Key::KEY_SPACE);
        assert_eq!(router.engine.history_len(), 2);
        router.clear_all();
        assert_eq!(router.engine.history_len(), 0);

        // Press Backspace + 'a' -> does NOT modify "Tối"
        tap_key(&mut router, &mut screen, &mut held, Key::KEY_BACKSPACE);
        let batch = tap_key(&mut router, &mut screen, &mut held, Key::KEY_A);
        assert_eq!(batch, vec![OutputItem::AsciiChar('a')]);
        assert_eq!(screen.iter().collect::<String>(), "Tốia");

        // Ctrl+Backspace must also call clear_all()
        tap_key(&mut router, &mut screen, &mut held, Key::KEY_SPACE);
        assert!(router.engine.history_len() > 0);
        let (items, _) = router.handle_raw_key(Key::KEY_LEFTCTRL, 1);
        apply_router_output(&mut screen, &mut held, &items);
        tap_key(&mut router, &mut screen, &mut held, Key::KEY_BACKSPACE);
        let (items, _) = router.handle_raw_key(Key::KEY_LEFTCTRL, 0);
        apply_router_output(&mut screen, &mut held, &items);
        assert_eq!(router.engine.history_len(), 0);

        assert!(held.is_empty(), "Stuck virtual keys: {:?}", held);
        assert!(router.passthrough_held.is_empty());
    }
}
