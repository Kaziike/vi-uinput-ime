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
use input::scanner::{spawn_hotplug_scanner, DeviceFilter};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Holding any non-Backspace key continuously for >= 200ms indicates intentional key-hold
/// (e.g. WASD game movement) rather than Vietnamese syllable typing (typical dwell 40–140ms).
const HOLD_RESET_THRESHOLD: Duration = Duration::from_millis(200);

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
    manual_enabled: bool,
    desktop_layout_active: bool,
    window_passthrough: bool,
    sync_desktop_layout: bool,
    toggle_hotkey: ToggleHotkey,
    /// Tracks every non-modifier key whose `value == 1` press was forwarded as `PassThrough`,
    /// guaranteeing its matching repeat (`value == 2`) and release (`value == 0`) are NEVER swallowed.
    passthrough_held: HashSet<Key>,
    /// Timestamp when each currently held `PassThrough` key was pressed, used to distinguish
    /// intentional key holds (>= 200ms, e.g. WASD movement) from fast typing rollover (< 200ms).
    held_since: HashMap<Key, Instant>,
}

impl TelexRouter {
    fn new(tone_style: config::ToneStyle, manual_enabled: bool, toggle_hotkey: ToggleHotkey) -> Self {
        Self {
            engine: TelexEngine::new(tone_style),
            mods: ModifierState::default(),
            manual_enabled,
            desktop_layout_active: true,
            window_passthrough: false,
            sync_desktop_layout: false,
            toggle_hotkey,
            passthrough_held: HashSet::new(),
            held_since: HashMap::new(),
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

    fn with_auto_restore_non_vn(mut self, enabled: bool) -> Self {
        self.engine.set_auto_restore_non_vn(enabled);
        self
    }

    fn effective_enabled(&self) -> bool {
        self.manual_enabled && self.desktop_layout_active && !self.window_passthrough
    }

    fn set_desktop_layout_active(&mut self, active: bool) -> bool {
        self.desktop_layout_active = active;
        self.engine.clear_all();
        self.effective_enabled()
    }

    fn set_window_passthrough(&mut self, passthrough: bool) -> bool {
        self.window_passthrough = passthrough;
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

    fn mark_passthrough_down(&mut self, code: Key, now: Instant) {
        self.passthrough_held.insert(code);
        self.held_since.insert(code, now);
    }

    fn any_key_held_long(&self, now: Instant) -> bool {
        self.held_since.iter().any(|(&k, &pressed_at)| {
            k != Key::KEY_BACKSPACE
                && now.saturating_duration_since(pressed_at) >= HOLD_RESET_THRESHOLD
        })
    }

    fn handle_raw_key(&mut self, code: Key, value: i32) -> (Vec<OutputItem>, Option<bool>) {
        self.handle_raw_key_at(code, value, Instant::now())
    }

    fn handle_raw_key_at(
        &mut self,
        code: Key,
        value: i32,
        now: Instant,
    ) -> (Vec<OutputItem>, Option<bool>) {
        // 1. Always track & forward modifier keys verbatim
        if self.mods.update(code, value) {
            return (vec![OutputItem::PassThrough { code, value }], None);
        }

        // 2. Key release (`value == 0`): ALWAYS forward release iff its press was passed through!
        //    If a non-Backspace key was held continuously for >= 200ms (e.g. WASD movement),
        //    invalidate the syllable buffer so the next directional key never turns into ă/á/đ.
        if value == 0 {
            if let Some(pressed_at) = self.held_since.remove(&code) {
                if code != Key::KEY_BACKSPACE
                    && now.saturating_duration_since(pressed_at) >= HOLD_RESET_THRESHOLD
                {
                    self.engine.clear_all();
                }
            }
            if self.passthrough_held.remove(&code) {
                return (vec![OutputItem::PassThrough { code, value: 0 }], None);
            }
            return (Vec::new(), None);
        }

        // 3. Key repeat (`value == 2`) for already passed-through keys EXCEPT Backspace/Space
        //    (Backspace and Space must update the history ring buffer on every repeat tick).
        //    Holding a key until OS/kernel repeat fires clearly indicates key-hold, not syllable
        //    composition, so clear pending syllable state while forwarding smooth raw repeat.
        if value == 2
            && !matches!(code, Key::KEY_BACKSPACE | Key::KEY_SPACE)
            && self.passthrough_held.contains(&code)
        {
            self.engine.clear_all();
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
                self.mark_passthrough_down(code, now);
            }
            return (vec![OutputItem::PassThrough { code, value }], None);
        }

        // 6. Space separator: run final phonology check (if invalid, enqueue restore diff BEFORE Space
        //    on the same single FIFO queue) and push active word + Separator(' ') into history ring buffer.
        if code == Key::KEY_SPACE {
            let mut items = Vec::new();
            if value == 1 || value == 2 {
                if value == 1 && self.any_key_held_long(now) {
                    self.engine.clear_all();
                }
                if let Some(restore_batch) = self.engine.process_separator(' ') {
                    items.extend(restore_batch);
                }
            }
            if value == 1 {
                self.mark_passthrough_down(code, now);
            }
            items.push(OutputItem::PassThrough { code, value });
            return (items, None);
        }

        // 7. Physical Backspace (supports Word History Restoration + smooth raw repeat)
        if code == Key::KEY_BACKSPACE {
            if value == 1 || value == 2 {
                if let Some(mut batch) = self.engine.process_backspace() {
                    self.held_since.remove(&Key::KEY_BACKSPACE);
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
                self.mark_passthrough_down(code, now);
            }
            return (vec![OutputItem::PassThrough { code, value }], None);
        }

        // 8. Telex ASCII letters (`a..z`, `A..Z`)
        if let Some(ch) = key_to_telex_char(code, self.mods.shift(), self.mods.caps_lock) {
            // Repeat on a letter whose initial press was consumed by a transform (e.g. quick
            // diagonal `A+W` held in a game): promote into a real continuous PassThrough hold.
            if value == 2 {
                self.engine.clear_all();
                self.mark_passthrough_down(code, now);
                return (vec![OutputItem::PassThrough { code, value: 1 }], None);
            }
            // If another key is already held >= 200ms (e.g. walking left with A, then pressing W/S),
            // clear pending syllable first so diagonal movement passes straight through.
            if value == 1 && self.any_key_held_long(now) {
                self.engine.clear_all();
            }
            if let Some(mut batch) = self.engine.process_char(ch) {
                // Hold-preserving PassThrough: whenever the batch ends with the exact raw ASCII
                // character of the physically pressed key (all plain untransformed letters like
                // W/A/S/D as well as auto-restore/undo tails), emit PassThrough { value: 1 } so
                // the virtual key stays genuinely held until physical release (critical for games).
                if value == 1 && batch.last() == Some(&OutputItem::AsciiChar(ch)) {
                    batch.pop();
                    self.mark_passthrough_down(code, now);
                    batch.push(OutputItem::PassThrough { code, value: 1 });
                }
                return (batch, None);
            }
            if value == 1 {
                self.mark_passthrough_down(code, now);
            }
            return (vec![OutputItem::PassThrough { code, value }], None);
        }

        // 9. Digits, punctuation, and other untracked keys -> clear_all() and pass through cleanly
        if value == 1 || value == 2 {
            self.engine.clear_all();
        }
        if value == 1 {
            self.mark_passthrough_down(code, now);
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
    let mut router = TelexRouter::new(
        config.general.tone_style,
        config.general.enabled_on_start,
        config.hotkey.toggle,
    )
    .with_desktop_layout_sync(config.general.sync_desktop_layout)
    .with_auto_restore_non_vn(config.general.auto_restore_non_vn);

    write_state_file(&config.general.state_file, router.effective_enabled());

    info!(
        "vi-uinput-ime-telex started (tone={:?}, toggle={:?}, sync_desktop_layout={}, auto_restore_non_vn={}, active={}, default_delay={}ms, ipc_socket={})",
        config.general.tone_style,
        config.hotkey.toggle,
        config.general.sync_desktop_layout,
        config.general.auto_restore_non_vn,
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
                                "Desktop layout '{}' -> Telex IME {}",
                                layout_id,
                                if eff { "ACTIVE (VI)" } else { "PAUSED (PassThrough)" }
                            );
                        }
                    }
                    RouterEvent::IpcBridgeDisconnected => {
                        if let Some(eff) = router.on_ipc_bridge_disconnected() {
                            write_state_file(&config.general.state_file, eff);
                            info!("IPC bridge disconnected -> Telex IME auto-paused (PassThrough)");
                        }
                    }
                    RouterEvent::MouseReset | RouterEvent::WorkspaceChanged => {
                        router.clear_all();
                    }
                    RouterEvent::WindowFocusChanged { app_class, is_xwayland } => {
                        let prev_eff = router.effective_enabled();
                        let new_eff =
                            router.set_window_passthrough(config.is_passthrough_app(&app_class));
                        if new_eff != prev_eff {
                            write_state_file(&config.general.state_file, new_eff);
                            info!(
                                "Window '{}' -> Telex IME {}",
                                app_class,
                                if new_eff {
                                    "ACTIVE (VI)"
                                } else {
                                    "PAUSED (App PassThrough)"
                                }
                            );
                        }
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
                        let shift = virtual_held.contains(&Key::KEY_LEFTSHIFT)
                            || virtual_held.contains(&Key::KEY_RIGHTSHIFT);
                        match code {
                            Key::KEY_SPACE => screen.push(' '),
                            Key::KEY_BACKSPACE => {
                                screen.pop();
                            }
                            other => {
                                if let Some(ch) = key_to_telex_char(other, shift, false) {
                                    screen.push(ch);
                                }
                            }
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

        // Press Backspace + 'a' -> does NOT modify "Tối" (passes through cleanly with hold support)
        tap_key(&mut router, &mut screen, &mut held, Key::KEY_BACKSPACE);
        let batch = tap_key(&mut router, &mut screen, &mut held, Key::KEY_A);
        assert_eq!(
            batch,
            vec![OutputItem::PassThrough {
                code: Key::KEY_A,
                value: 1,
            }]
        );
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

    #[test]
    fn super_space_toggles_ime_swallows_space_and_emits_f24_tap() {
        let mut router = TelexRouter::new(config::ToneStyle::New, true, ToggleHotkey::SuperSpace);
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
            TelexRouter::new(config::ToneStyle::New, true, ToggleHotkey::CtrlSpace);
        ctrl_router.handle_raw_key(Key::KEY_LEFTCTRL, 1);
        let (ctrl_space_down, toggled) = ctrl_router.handle_raw_key(Key::KEY_SPACE, 1);
        assert_eq!(toggled, Some(false));
        assert!(ctrl_space_down.is_empty());
    }

    #[test]
    fn desktop_layout_sync_pauses_to_passthrough_and_toggle_none_never_swallows_shortcuts() {
        let mut router = TelexRouter::new(config::ToneStyle::New, true, ToggleHotkey::None)
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

        // When paused (e.g. GNOME US layout), typing `t e s t` emits ONLY raw PassThrough
        for k in [Key::KEY_T, Key::KEY_E, Key::KEY_S, Key::KEY_T] {
            let (down, _) = router.handle_raw_key(k, 1);
            assert_eq!(down, vec![OutputItem::PassThrough { code: k, value: 1 }]);
            apply_router_output(&mut screen, &mut held, &down);
            let (up, _) = router.handle_raw_key(k, 0);
            assert_eq!(up, vec![OutputItem::PassThrough { code: k, value: 0 }]);
            apply_router_output(&mut screen, &mut held, &up);
        }
        assert_eq!(screen.iter().collect::<String>(), "test");
        screen.clear();

        // Desktop switches to `vi_uinput` -> Telex activates immediately (`suawr` -> "sửa")
        assert!(router.set_desktop_layout_active(true));
        for k in [Key::KEY_S, Key::KEY_U, Key::KEY_A, Key::KEY_W, Key::KEY_R] {
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

    #[test]
    fn telex_router_space_enqueues_auto_restore_batch_strictly_before_space_passthrough() {
        let mut router = TelexRouter::new(config::ToneStyle::New, true, ToggleHotkey::None)
            .with_auto_restore_non_vn(true);
        let mut screen: Vec<char> = Vec::new();
        let mut held: HashSet<Key> = HashSet::new();

        for k in [Key::KEY_P, Key::KEY_O, Key::KEY_R, Key::KEY_T] {
            tap_key(&mut router, &mut screen, &mut held, k);
        }
        assert_eq!(screen.iter().collect::<String>(), "pỏt");

        let space_batch = tap_key(&mut router, &mut screen, &mut held, Key::KEY_SPACE);
        assert_eq!(
            space_batch,
            vec![
                OutputItem::Backspace,
                OutputItem::Backspace,
                OutputItem::AsciiChar('o'),
                OutputItem::AsciiChar('r'),
                OutputItem::AsciiChar('t'),
                OutputItem::PassThrough {
                    code: Key::KEY_SPACE,
                    value: 1,
                },
            ]
        );
        assert_eq!(screen.iter().collect::<String>(), "port ");
        assert!(held.is_empty(), "Stuck virtual keys: {:?}", held);
        assert!(router.passthrough_held.is_empty());
    }

    #[test]
    fn game_wasd_holds_stay_continuously_down_without_stutter_or_diacritic_corruption() {
        let mut router = TelexRouter::new(config::ToneStyle::New, true, ToggleHotkey::None);
        let mut screen: Vec<char> = Vec::new();
        let mut held: HashSet<Key> = HashSet::new();
        let t0 = Instant::now();

        // 1. Minecraft / Goose Goose Duck: hold W, A, S, D individually -> MUST stay held as PassThrough
        //    (never premature value=0!) and repeat value=2 must NOT turn `a`->`â` or `d`->`đ`.
        for (idx, k) in [Key::KEY_W, Key::KEY_A, Key::KEY_S, Key::KEY_D]
            .into_iter()
            .enumerate()
        {
            let base = t0 + Duration::from_secs(idx as u64);
            let (down, _) = router.handle_raw_key_at(k, 1, base);
            assert_eq!(down, vec![OutputItem::PassThrough { code: k, value: 1 }]);
            apply_router_output(&mut screen, &mut held, &down);
            assert!(held.contains(&k), "{:?} must remain held on virtual keyboard", k);

            // Repeat ticks while held -> smooth raw repeat, zero Backspace / VietnameseChar!
            for tick in 1..=5 {
                let (rep, _) = router.handle_raw_key_at(
                    k,
                    2,
                    base + Duration::from_millis(250 + tick * 33),
                );
                assert_eq!(rep, vec![OutputItem::PassThrough { code: k, value: 2 }]);
                assert!(held.contains(&k));
            }

            let (up, _) = router.handle_raw_key_at(k, 0, base + Duration::from_millis(500));
            assert_eq!(up, vec![OutputItem::PassThrough { code: k, value: 0 }]);
            apply_router_output(&mut screen, &mut held, &up);
            assert!(!held.contains(&k));
        }

        // 2. Strafe left (hold A >= 200ms), then press W while still holding A (diagonal up-left):
        //    W must pass through continuously (NOT turn into `ă` / KEY_6!).
        let t_diag = t0 + Duration::from_secs(10);
        let (a_down, _) = router.handle_raw_key_at(Key::KEY_A, 1, t_diag);
        apply_router_output(&mut screen, &mut held, &a_down);
        let (w_down, _) =
            router.handle_raw_key_at(Key::KEY_W, 1, t_diag + Duration::from_millis(250));
        assert_eq!(
            w_down,
            vec![OutputItem::PassThrough {
                code: Key::KEY_W,
                value: 1,
            }]
        );
        apply_router_output(&mut screen, &mut held, &w_down);
        assert!(held.contains(&Key::KEY_A) && held.contains(&Key::KEY_W));

        // Release both, then press S immediately -> passes through (NOT `á` / KEY_2!)
        let (a_up, _) =
            router.handle_raw_key_at(Key::KEY_A, 0, t_diag + Duration::from_millis(400));
        apply_router_output(&mut screen, &mut held, &a_up);
        let (w_up, _) =
            router.handle_raw_key_at(Key::KEY_W, 0, t_diag + Duration::from_millis(420));
        apply_router_output(&mut screen, &mut held, &w_up);

        // Hold A for 250ms, release A, then immediately press S -> must NOT turn into `á`!
        let t_seq = t0 + Duration::from_secs(15);
        let (a_down2, _) = router.handle_raw_key_at(Key::KEY_A, 1, t_seq);
        apply_router_output(&mut screen, &mut held, &a_down2);
        let (a_up2, _) =
            router.handle_raw_key_at(Key::KEY_A, 0, t_seq + Duration::from_millis(250));
        apply_router_output(&mut screen, &mut held, &a_up2);
        let (s_down, _) =
            router.handle_raw_key_at(Key::KEY_S, 1, t_seq + Duration::from_millis(270));
        assert_eq!(
            s_down,
            vec![OutputItem::PassThrough {
                code: Key::KEY_S,
                value: 1,
            }]
        );
        apply_router_output(&mut screen, &mut held, &s_down);
        let (s_up, _) =
            router.handle_raw_key_at(Key::KEY_S, 0, t_seq + Duration::from_millis(350));
        apply_router_output(&mut screen, &mut held, &s_up);

        // 3. Instant diagonal A+W within 20ms, then user keeps holding W past repeat delay:
        //    value == 2 promotes W into a real continuous PassThrough hold with zero stuck keys.
        let t_inst = t0 + Duration::from_secs(20);
        let (ia_down, _) = router.handle_raw_key_at(Key::KEY_A, 1, t_inst);
        apply_router_output(&mut screen, &mut held, &ia_down);
        let (iw_down, _) =
            router.handle_raw_key_at(Key::KEY_W, 1, t_inst + Duration::from_millis(20));
        apply_router_output(&mut screen, &mut held, &iw_down);
        let (iw_rep, _) =
            router.handle_raw_key_at(Key::KEY_W, 2, t_inst + Duration::from_millis(270));
        assert_eq!(
            iw_rep,
            vec![OutputItem::PassThrough {
                code: Key::KEY_W,
                value: 1,
            }]
        );
        apply_router_output(&mut screen, &mut held, &iw_rep);
        assert!(held.contains(&Key::KEY_A) && held.contains(&Key::KEY_W));
        let (ia_up, _) =
            router.handle_raw_key_at(Key::KEY_A, 0, t_inst + Duration::from_millis(500));
        apply_router_output(&mut screen, &mut held, &ia_up);
        let (iw_up, _) =
            router.handle_raw_key_at(Key::KEY_W, 0, t_inst + Duration::from_millis(510));
        apply_router_output(&mut screen, &mut held, &iw_up);

        assert!(held.is_empty(), "Stuck virtual keys: {:?}", held);
        assert!(router.passthrough_held.is_empty());
        assert!(router.held_since.is_empty());
    }

    #[test]
    fn fast_typing_rollover_and_long_backspace_history_restore_still_work() {
        let mut router = TelexRouter::new(config::ToneStyle::New, true, ToggleHotkey::None);
        let mut screen: Vec<char> = Vec::new();
        let mut held: HashSet<Key> = HashSet::new();
        let t0 = Instant::now();

        // Fast rollover typing `toans` (`n` still held when `s` is pressed 30ms later) -> "toán"
        for (k, down_ms, up_ms) in [
            (Key::KEY_T, 0, 60),
            (Key::KEY_O, 40, 100),
            (Key::KEY_A, 80, 140),
        ] {
            let (d, _) = router.handle_raw_key_at(k, 1, t0 + Duration::from_millis(down_ms));
            apply_router_output(&mut screen, &mut held, &d);
            let (u, _) = router.handle_raw_key_at(k, 0, t0 + Duration::from_millis(up_ms));
            apply_router_output(&mut screen, &mut held, &u);
        }
        let (n_down, _) = router.handle_raw_key_at(Key::KEY_N, 1, t0 + Duration::from_millis(150));
        apply_router_output(&mut screen, &mut held, &n_down);
        // `s` pressed while `n` is still physically held (30ms < 200ms threshold!)
        let (s_down, _) = router.handle_raw_key_at(Key::KEY_S, 1, t0 + Duration::from_millis(180));
        apply_router_output(&mut screen, &mut held, &s_down);
        let (n_up, _) = router.handle_raw_key_at(Key::KEY_N, 0, t0 + Duration::from_millis(200));
        apply_router_output(&mut screen, &mut held, &n_up);
        let (s_up, _) = router.handle_raw_key_at(Key::KEY_S, 0, t0 + Duration::from_millis(210));
        apply_router_output(&mut screen, &mut held, &s_up);
        assert_eq!(screen.iter().collect::<String>(), "toán");

        // Space + "ab", then hold Backspace for 400ms (>= 200ms) -> MUST STILL restore "toán"
        for (i, k) in [Key::KEY_SPACE, Key::KEY_A, Key::KEY_B].into_iter().enumerate() {
            let base = t0 + Duration::from_millis(250 + i as u64 * 40);
            let (d, _) = router.handle_raw_key_at(k, 1, base);
            apply_router_output(&mut screen, &mut held, &d);
            let (u, _) = router.handle_raw_key_at(k, 0, base + Duration::from_millis(30));
            apply_router_output(&mut screen, &mut held, &u);
        }
        assert_eq!(screen.iter().collect::<String>(), "toán ab");

        let bs_start = t0 + Duration::from_millis(500);
        let (bs1, _) = router.handle_raw_key_at(Key::KEY_BACKSPACE, 1, bs_start);
        apply_router_output(&mut screen, &mut held, &bs1);
        let (bs2, _) = router.handle_raw_key_at(Key::KEY_BACKSPACE, 2, bs_start + Duration::from_millis(250));
        apply_router_output(&mut screen, &mut held, &bs2);
        let (bs3, _) = router.handle_raw_key_at(Key::KEY_BACKSPACE, 2, bs_start + Duration::from_millis(300));
        apply_router_output(&mut screen, &mut held, &bs3);
        let (bs_up, _) = router.handle_raw_key_at(Key::KEY_BACKSPACE, 0, bs_start + Duration::from_millis(400));
        apply_router_output(&mut screen, &mut held, &bs_up);
        assert_eq!(screen.iter().collect::<String>(), "toán");

        // Pressing 'z' removes tone on restored "toán" -> "toan"
        let (z_down, _) = router.handle_raw_key_at(Key::KEY_Z, 1, bs_start + Duration::from_millis(450));
        apply_router_output(&mut screen, &mut held, &z_down);
        let (z_up, _) = router.handle_raw_key_at(Key::KEY_Z, 0, bs_start + Duration::from_millis(480));
        apply_router_output(&mut screen, &mut held, &z_up);
        assert_eq!(screen.iter().collect::<String>(), "toan");

        // Window passthrough pauses & resumes cleanly
        assert!(!router.set_window_passthrough(true));
        let (d, _) = router.handle_raw_key(Key::KEY_A, 1);
        assert_eq!(d, vec![OutputItem::PassThrough { code: Key::KEY_A, value: 1 }]);
        let (u, _) = router.handle_raw_key(Key::KEY_A, 0);
        assert_eq!(u, vec![OutputItem::PassThrough { code: Key::KEY_A, value: 0 }]);
        assert!(router.set_window_passthrough(false));

        assert!(held.is_empty(), "Stuck virtual keys: {:?}", held);
        assert!(router.passthrough_held.is_empty());
        assert!(router.held_since.is_empty());
    }
}
