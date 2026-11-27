use crate::xkb_gen::{
    lookup_vn_char, LEVEL3_MODIFIER_KEY, LEVEL5_MODIFIER_KEY, VIRTUAL_KBD_NAME,
    VIRTUAL_KBD_PRODUCT_ID, VIRTUAL_KBD_VENDOR_ID, VN_KEY_SLOTS,
};
use anyhow::{Context, Result};
use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AttributeSet, BusType, EventType, InputEvent, InputId, Key};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

/// Every outgoing keyboard event—both transformed Vietnamese keystroke batches AND
/// untouched pass-through keys (`Space`, `Enter`, arrows, modifiers)—MUST traverse
/// this single strict FIFO queue so synthetic delays can never reorder keystrokes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputItem {
    /// Forward raw key event verbatim from the physical keyboard (press=1, repeat=2, release=0).
    PassThrough { code: Key, value: i32 },
    /// Synchronize the FIFO worker's tracked CapsLock state (e.g. from hardware `LED_CAPSL` on startup).
    /// Control-only item: emits no evdev events and incurs zero delay.
    SyncCapsLock(bool),
    /// Emit a complete Backspace press + SYN + release + SYN cycle.
    Backspace,
    /// Emit a precomposed Vietnamese character via `vi-uinput-virtual-kbd` XKB Levels 5–8.
    VietnameseChar(char),
    /// Emit a plain ASCII character (with Shift adjusted for physical Shift + CapsLock state).
    AsciiChar(char),
}

/// Sink abstraction allowing the Strict FIFO Worker to drive either a real Linux `/dev/uinput`
/// virtual device or an in-memory mock sink in deterministic unit tests.
pub trait EventSink: Send + 'static {
    fn emit_events(&mut self, events: &[InputEvent]) -> Result<()>;
}

/// Real `/dev/uinput` virtual keyboard device (`"vi-uinput-virtual-kbd"`).
pub struct UinputSink {
    device: VirtualDevice,
}

impl UinputSink {
    pub fn create() -> Result<Self> {
        let mut keys = AttributeSet::<Key>::new();
        // Register full standard keyboard key range (ESC..KEY_F24 + modifiers + navigation)
        for code in 1u16..=248u16 {
            keys.insert(Key::new(code));
        }
        // Explicitly register KEY_F24 (used to suppress Super/Alt tap Overview/Launcher on toggle)
        keys.insert(Key::KEY_F24);
        // Guarantee all 34 VN XKB carrier slots and Level5/Level3 modifiers are registered
        keys.insert(LEVEL5_MODIFIER_KEY);
        keys.insert(LEVEL3_MODIFIER_KEY);
        for slot in VN_KEY_SLOTS {
            keys.insert(slot.key);
        }

        let input_id = InputId::new(BusType::BUS_VIRTUAL, VIRTUAL_KBD_VENDOR_ID, VIRTUAL_KBD_PRODUCT_ID, 1);

        let device = VirtualDeviceBuilder::new()
            .context("failed to open /dev/uinput — verify udev rule 99-vi-uinput-ime.rules")?
            .name(VIRTUAL_KBD_NAME)
            .input_id(input_id)
            .with_keys(&keys)?
            .build()
            .context("failed to register virtual uinput keyboard device")?;

        Ok(Self { device })
    }
}

impl EventSink for UinputSink {
    fn emit_events(&mut self, events: &[InputEvent]) -> Result<()> {
        self.device.emit(events)?;
        Ok(())
    }
}

/// Convert a single `OutputItem` into standard Linux `EV_KEY` + `EV_SYN (SYN_REPORT)` frames,
/// assuming no held Shift and CapsLock OFF.
#[allow(dead_code)]
pub fn encode_output_item(item: &OutputItem) -> Vec<Vec<InputEvent>> {
    encode_output_item_with_modifiers(item, false, false)
}

#[allow(dead_code)]
pub fn encode_output_item_with_shift(
    item: &OutputItem,
    phys_shift_held: bool,
) -> Vec<Vec<InputEvent>> {
    encode_output_item_with_modifiers(item, phys_shift_held, false)
}

/// Convert a single `OutputItem` into Linux `EV_KEY` + `SYN_REPORT` frames while accounting for:
/// - `phys_shift_held`: preserves any physically held Shift modifier so fast Shift rollover never breaks.
/// - `worker_caps_lock`: for ASCII alphabetic letters (`a-z`, `A-Z`), XORs `need_shift` with
///   `worker_caps_lock` because the OS applies CapsLock to Level 1–2 letters.
///   ASCII digits/symbols (`!@#$...`) are NOT XORed, and `VietnameseChar` uses `combo.shift` directly
///   because XKB Levels 5–8 (`VI_EIGHT_LEVEL` / `VI_EIGHT_LEVEL_ALPHA`) are 100% immune to CapsLock.
pub fn encode_output_item_with_modifiers(
    item: &OutputItem,
    phys_shift_held: bool,
    worker_caps_lock: bool,
) -> Vec<Vec<InputEvent>> {
    let syn = || InputEvent::new(EventType::SYNCHRONIZATION, 0, 0);
    let key_ev = |code: Key, val: i32| InputEvent::new(EventType::KEY, code.code(), val);

    match item {
        OutputItem::SyncCapsLock(_) => Vec::new(),
        OutputItem::PassThrough { code, value } => {
            vec![vec![key_ev(*code, *value), syn()]]
        }
        OutputItem::Backspace => {
            vec![
                vec![key_ev(Key::KEY_BACKSPACE, 1), syn()],
                vec![key_ev(Key::KEY_BACKSPACE, 0), syn()],
            ]
        }
        OutputItem::VietnameseChar(ch) => {
            if let Some(combo) = lookup_vn_char(*ch) {
                let mut frames = Vec::with_capacity(8);
                // XKB Levels 5–8 ignore Lock/CapsLock completely -> always use combo.shift directly!
                let temp_release_shift = phys_shift_held && !combo.shift;
                let temp_press_shift = !phys_shift_held && combo.shift;

                if temp_release_shift {
                    frames.push(vec![key_ev(Key::KEY_LEFTSHIFT, 0), syn()]);
                }
                if combo.level5 {
                    frames.push(vec![key_ev(LEVEL5_MODIFIER_KEY, 1), syn()]);
                }
                if combo.level3 {
                    frames.push(vec![key_ev(LEVEL3_MODIFIER_KEY, 1), syn()]);
                }
                if temp_press_shift {
                    frames.push(vec![key_ev(Key::KEY_LEFTSHIFT, 1), syn()]);
                }
                frames.push(vec![key_ev(combo.key, 1), syn()]);
                frames.push(vec![key_ev(combo.key, 0), syn()]);
                if temp_press_shift {
                    frames.push(vec![key_ev(Key::KEY_LEFTSHIFT, 0), syn()]);
                }
                if combo.level3 {
                    frames.push(vec![key_ev(LEVEL3_MODIFIER_KEY, 0), syn()]);
                }
                if combo.level5 {
                    frames.push(vec![key_ev(LEVEL5_MODIFIER_KEY, 0), syn()]);
                }
                if temp_release_shift {
                    frames.push(vec![key_ev(Key::KEY_LEFTSHIFT, 1), syn()]);
                }
                frames
            } else {
                warn!("VietnameseChar '{}' not found in 134-char XKB map", ch);
                Vec::new()
            }
        }
        OutputItem::AsciiChar(ch) => {
            if let Some((key, base_shift)) = ascii_to_key(*ch) {
                let need_shift = if ch.is_ascii_alphabetic() {
                    base_shift ^ worker_caps_lock
                } else {
                    base_shift
                };
                let mut frames = Vec::with_capacity(4);
                let temp_release_shift = phys_shift_held && !need_shift;
                let temp_press_shift = !phys_shift_held && need_shift;

                if temp_release_shift {
                    frames.push(vec![key_ev(Key::KEY_LEFTSHIFT, 0), syn()]);
                }
                if temp_press_shift {
                    frames.push(vec![key_ev(Key::KEY_LEFTSHIFT, 1), syn()]);
                }
                frames.push(vec![key_ev(key, 1), syn()]);
                frames.push(vec![key_ev(key, 0), syn()]);
                if temp_press_shift {
                    frames.push(vec![key_ev(Key::KEY_LEFTSHIFT, 0), syn()]);
                }
                if temp_release_shift {
                    frames.push(vec![key_ev(Key::KEY_LEFTSHIFT, 1), syn()]);
                }
                frames
            } else {
                Vec::new()
            }
        }
    }
}

pub fn ascii_to_key(ch: char) -> Option<(Key, bool)> {
    if ch.is_ascii_alphabetic() {
        let shift = ch.is_ascii_uppercase();
        let key = match ch.to_ascii_lowercase() {
            'a' => Key::KEY_A,
            'b' => Key::KEY_B,
            'c' => Key::KEY_C,
            'd' => Key::KEY_D,
            'e' => Key::KEY_E,
            'f' => Key::KEY_F,
            'g' => Key::KEY_G,
            'h' => Key::KEY_H,
            'i' => Key::KEY_I,
            'j' => Key::KEY_J,
            'k' => Key::KEY_K,
            'l' => Key::KEY_L,
            'm' => Key::KEY_M,
            'n' => Key::KEY_N,
            'o' => Key::KEY_O,
            'p' => Key::KEY_P,
            'q' => Key::KEY_Q,
            'r' => Key::KEY_R,
            's' => Key::KEY_S,
            't' => Key::KEY_T,
            'u' => Key::KEY_U,
            'v' => Key::KEY_V,
            'w' => Key::KEY_W,
            'x' => Key::KEY_X,
            'y' => Key::KEY_Y,
            'z' => Key::KEY_Z,
            _ => return None,
        };
        return Some((key, shift));
    }

    match ch {
        '0' => Some((Key::KEY_0, false)),
        '1' => Some((Key::KEY_1, false)),
        '2' => Some((Key::KEY_2, false)),
        '3' => Some((Key::KEY_3, false)),
        '4' => Some((Key::KEY_4, false)),
        '5' => Some((Key::KEY_5, false)),
        '6' => Some((Key::KEY_6, false)),
        '7' => Some((Key::KEY_7, false)),
        '8' => Some((Key::KEY_8, false)),
        '9' => Some((Key::KEY_9, false)),
        ' ' => Some((Key::KEY_SPACE, false)),
        '-' => Some((Key::KEY_MINUS, false)),
        '=' => Some((Key::KEY_EQUAL, false)),
        '[' => Some((Key::KEY_LEFTBRACE, false)),
        ']' => Some((Key::KEY_RIGHTBRACE, false)),
        ';' => Some((Key::KEY_SEMICOLON, false)),
        '\'' => Some((Key::KEY_APOSTROPHE, false)),
        '`' => Some((Key::KEY_GRAVE, false)),
        ',' => Some((Key::KEY_COMMA, false)),
        '.' => Some((Key::KEY_DOT, false)),
        '/' => Some((Key::KEY_SLASH, false)),
        '\\' => Some((Key::KEY_BACKSLASH, false)),
        // Shifted US QWERTY ASCII symbols
        '!' => Some((Key::KEY_1, true)),
        '@' => Some((Key::KEY_2, true)),
        '#' => Some((Key::KEY_3, true)),
        '$' => Some((Key::KEY_4, true)),
        '%' => Some((Key::KEY_5, true)),
        '^' => Some((Key::KEY_6, true)),
        '&' => Some((Key::KEY_7, true)),
        '*' => Some((Key::KEY_8, true)),
        '(' => Some((Key::KEY_9, true)),
        ')' => Some((Key::KEY_0, true)),
        '_' => Some((Key::KEY_MINUS, true)),
        '+' => Some((Key::KEY_EQUAL, true)),
        '{' => Some((Key::KEY_LEFTBRACE, true)),
        '}' => Some((Key::KEY_RIGHTBRACE, true)),
        '|' => Some((Key::KEY_BACKSLASH, true)),
        ':' => Some((Key::KEY_SEMICOLON, true)),
        '"' => Some((Key::KEY_APOSTROPHE, true)),
        '~' => Some((Key::KEY_GRAVE, true)),
        '<' => Some((Key::KEY_COMMA, true)),
        '>' => Some((Key::KEY_DOT, true)),
        '?' => Some((Key::KEY_SLASH, true)),
        _ => None,
    }
}

/// Handle to enqueue atomic keystroke batches into the single Strict FIFO Worker.
#[derive(Clone)]
pub struct FifoEmitterHandle {
    tx: mpsc::UnboundedSender<OutputItem>,
    delay_ms: Arc<AtomicU64>,
}

impl FifoEmitterHandle {
    pub fn enqueue(&self, item: OutputItem) {
        if let Err(err) = self.tx.send(item) {
            error!("FIFO emitter queue closed unexpectedly: {}", err);
        }
    }

    pub fn enqueue_batch(&self, items: Vec<OutputItem>) {
        for item in items {
            self.enqueue(item);
        }
    }

    pub fn set_delay_ms(&self, ms: u64) {
        self.delay_ms.store(ms, Ordering::Relaxed);
        debug!("Updated FIFO emitter dynamic delay to {}ms", ms);
    }

    #[allow(dead_code)]
    pub fn current_delay_ms(&self) -> u64 {
        self.delay_ms.load(Ordering::Relaxed)
    }
}

/// Spawn the single-consumer Strict FIFO Worker task that drains `OutputItem`s strictly
/// in arrival order, sleeping `current_delay_ms` after every synthetic keystroke release.
pub fn spawn_fifo_emitter<S: EventSink>(
    mut sink: S,
    initial_delay_ms: u64,
) -> (FifoEmitterHandle, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<OutputItem>();
    let delay_ms = Arc::new(AtomicU64::new(initial_delay_ms));
    let worker_delay = Arc::clone(&delay_ms);

    let join = tokio::spawn(async move {
        let mut phys_lshift = false;
        let mut phys_rshift = false;
        let mut worker_caps_lock = false;

        while let Some(item) = rx.recv().await {
            match item {
                OutputItem::SyncCapsLock(on) => {
                    worker_caps_lock = on;
                    debug!("FIFO worker synced worker_caps_lock={}", on);
                    continue;
                }
                OutputItem::PassThrough { code, value } => {
                    if code == Key::KEY_LEFTSHIFT {
                        phys_lshift = value != 0;
                    } else if code == Key::KEY_RIGHTSHIFT {
                        phys_rshift = value != 0;
                    } else if code == Key::KEY_CAPSLOCK && value == 1 {
                        worker_caps_lock = !worker_caps_lock;
                        debug!(
                            "FIFO worker toggled worker_caps_lock={}",
                            worker_caps_lock
                        );
                    }
                }
                _ => {}
            }

            let is_synthetic = !matches!(item, OutputItem::PassThrough { .. });
            let frames = encode_output_item_with_modifiers(
                &item,
                phys_lshift || phys_rshift,
                worker_caps_lock,
            );

            for frame in frames {
                if let Err(err) = sink.emit_events(&frame) {
                    error!("uinput emit_events failed: {}", err);
                }
            }

            if is_synthetic {
                let ms = worker_delay.load(Ordering::Relaxed);
                if ms > 0 {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
            }
        }
    });

    (FifoEmitterHandle { tx, delay_ms }, join)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToneStyle;
    use crate::engine::telex::TelexEngine;
    use crate::engine::InputEngine;
    use std::sync::Mutex;

    struct RecordingSink {
        log: Arc<Mutex<Vec<InputEvent>>>,
    }

    impl EventSink for RecordingSink {
        fn emit_events(&mut self, events: &[InputEvent]) -> Result<()> {
            let mut guard = self.log.lock().unwrap();
            guard.extend_from_slice(events);
            Ok(())
        }
    }

    fn key_events(frames: &[Vec<InputEvent>]) -> Vec<(Key, i32)> {
        frames
            .iter()
            .flatten()
            .filter(|ev| ev.event_type() == EventType::KEY)
            .map(|ev| (Key::new(ev.code()), ev.value()))
            .collect()
    }

    #[test]
    fn caps_lock_encoding_for_ascii_letters_symbols_and_vietnamese_chars() {
        // CapsLock ON, physical Shift NOT held:
        // 1. AsciiChar('T') -> NO Shift pressed (CapsLock already capitalizes 't' -> 'T')
        let upper_t = key_events(&encode_output_item_with_modifiers(
            &OutputItem::AsciiChar('T'),
            false,
            true,
        ));
        assert_eq!(upper_t, vec![(Key::KEY_T, 1), (Key::KEY_T, 0)]);

        // 2. AsciiChar('t') -> MUST temporarily press Shift then release (Shift + CapsLock -> 't')
        let lower_t = key_events(&encode_output_item_with_modifiers(
            &OutputItem::AsciiChar('t'),
            false,
            true,
        ));
        assert_eq!(
            lower_t,
            vec![
                (Key::KEY_LEFTSHIFT, 1),
                (Key::KEY_T, 1),
                (Key::KEY_T, 0),
                (Key::KEY_LEFTSHIFT, 0),
            ]
        );

        // 3. AsciiChar('!') -> symbol MUST keep Shift even when CapsLock ON
        let bang = key_events(&encode_output_item_with_modifiers(
            &OutputItem::AsciiChar('!'),
            false,
            true,
        ));
        assert_eq!(
            bang,
            vec![
                (Key::KEY_LEFTSHIFT, 1),
                (Key::KEY_1, 1),
                (Key::KEY_1, 0),
                (Key::KEY_LEFTSHIFT, 0),
            ]
        );

        // 4. VietnameseChar('đ') with CapsLock ON -> KEY_MACRO + carrier, NO Shift
        let combo_d_lower = lookup_vn_char('đ').unwrap();
        let vn_lower = key_events(&encode_output_item_with_modifiers(
            &OutputItem::VietnameseChar('đ'),
            false,
            true,
        ));
        assert_eq!(
            vn_lower,
            vec![
                (LEVEL5_MODIFIER_KEY, 1),
                (combo_d_lower.key, 1),
                (combo_d_lower.key, 0),
                (LEVEL5_MODIFIER_KEY, 0),
            ]
        );

        // 5. VietnameseChar('Đ') with CapsLock ON -> KEY_MACRO + Shift + carrier
        let combo_d_upper = lookup_vn_char('Đ').unwrap();
        let vn_upper = key_events(&encode_output_item_with_modifiers(
            &OutputItem::VietnameseChar('Đ'),
            false,
            true,
        ));
        assert_eq!(
            vn_upper,
            vec![
                (LEVEL5_MODIFIER_KEY, 1),
                (Key::KEY_LEFTSHIFT, 1),
                (combo_d_upper.key, 1),
                (combo_d_upper.key, 0),
                (Key::KEY_LEFTSHIFT, 0),
                (LEVEL5_MODIFIER_KEY, 0),
            ]
        );
    }

    #[tokio::test]
    async fn fifo_worker_syncs_and_toggles_caps_lock_in_strict_order() {
        let log = Arc::new(Mutex::new(Vec::<InputEvent>::new()));
        let sink = RecordingSink {
            log: Arc::clone(&log),
        };
        let (handle, worker) = spawn_fifo_emitter(sink, 0);

        // Sync initial CapsLock = ON -> 'T' needs no Shift, 't' needs Shift
        handle.enqueue(OutputItem::SyncCapsLock(true));
        handle.enqueue(OutputItem::AsciiChar('T'));
        handle.enqueue(OutputItem::AsciiChar('t'));
        // User toggles CapsLock OFF in the same FIFO queue -> 'T' now needs Shift
        handle.enqueue(OutputItem::PassThrough {
            code: Key::KEY_CAPSLOCK,
            value: 1,
        });
        handle.enqueue(OutputItem::PassThrough {
            code: Key::KEY_CAPSLOCK,
            value: 0,
        });
        handle.enqueue(OutputItem::AsciiChar('T'));

        drop(handle);
        worker.await.unwrap();

        let keys: Vec<(Key, i32)> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|ev| ev.event_type() == EventType::KEY)
            .map(|ev| (Key::new(ev.code()), ev.value()))
            .collect();

        assert_eq!(
            keys,
            vec![
                // 'T' with CapsLock ON -> bare KEY_T
                (Key::KEY_T, 1),
                (Key::KEY_T, 0),
                // 't' with CapsLock ON -> Shift + KEY_T
                (Key::KEY_LEFTSHIFT, 1),
                (Key::KEY_T, 1),
                (Key::KEY_T, 0),
                (Key::KEY_LEFTSHIFT, 0),
                // CapsLock toggle OFF
                (Key::KEY_CAPSLOCK, 1),
                (Key::KEY_CAPSLOCK, 0),
                // 'T' with CapsLock OFF -> Shift + KEY_T
                (Key::KEY_LEFTSHIFT, 1),
                (Key::KEY_T, 1),
                (Key::KEY_T, 0),
                (Key::KEY_LEFTSHIFT, 0),
            ]
        );
    }

    /// Simulates an ultra-fast 0ms burst: `t`, `o`, `i`, `o`, `s`, `Space` while the FIFO
    /// worker is running with `delay = 5ms`. Verifies `Space` NEVER overtakes the delayed
    /// synthetic `BS, BS, ố, i` batch (output is always `tối `, never `tố i`).
    #[tokio::test]
    async fn strict_fifo_prevents_space_overtaking_delayed_synthetic_batch() {
        let log = Arc::new(Mutex::new(Vec::<InputEvent>::new()));
        let sink = RecordingSink {
            log: Arc::clone(&log),
        };

        let (handle, worker) = spawn_fifo_emitter(sink, 5);
        let mut engine = TelexEngine::new(ToneStyle::New);

        // User bursts `t`, `o`, `i`, `o`, `s`, `Space` with 0ms delay between physical presses
        for ch in ['t', 'o', 'i', 'o', 's'] {
            if let Some(batch) = engine.process_char(ch) {
                handle.enqueue_batch(batch);
            }
        }
        // Space goes through the EXACT SAME single FIFO queue as PassThrough
        engine.process_separator(' ');
        handle.enqueue(OutputItem::PassThrough {
            code: Key::KEY_SPACE,
            value: 1,
        });
        handle.enqueue(OutputItem::PassThrough {
            code: Key::KEY_SPACE,
            value: 0,
        });

        // Close sender so worker finishes draining every queued item in order
        drop(handle);
        worker.await.unwrap();

        // Replay recorded EV_KEY press events through our XKB Levels 5–8 decoder
        let events = log.lock().unwrap();
        let mut level5 = false;
        let mut level3 = false;
        let mut shift = false;
        let mut output: Vec<char> = Vec::new();
        let mut decoded_ops: Vec<String> = Vec::new();

        for ev in events.iter() {
            if ev.event_type() != EventType::KEY {
                continue;
            }
            let key = Key::new(ev.code());
            let val = ev.value();
            if key == LEVEL5_MODIFIER_KEY {
                level5 = val != 0;
                continue;
            }
            if key == LEVEL3_MODIFIER_KEY {
                level3 = val != 0;
                continue;
            }
            if key == Key::KEY_LEFTSHIFT {
                shift = val != 0;
                continue;
            }
            if val != 1 {
                continue;
            }
            if key == Key::KEY_BACKSPACE {
                output.pop();
                decoded_ops.push("BS".to_string());
            } else if key == Key::KEY_SPACE {
                output.push(' ');
                decoded_ops.push("Space".to_string());
            } else if level5 {
                let slot = VN_KEY_SLOTS.iter().find(|s| s.key == key).unwrap();
                let ch = if level3 {
                    let (lo, up) = slot.l78.expect("slot missing Level7/8 pair");
                    if shift { up } else { lo }
                } else if shift {
                    slot.l6_upper
                } else {
                    slot.l5_lower
                };
                output.push(ch);
                decoded_ops.push(ch.to_string());
            } else {
                let ch = match key {
                    Key::KEY_T => 't',
                    Key::KEY_O => 'o',
                    Key::KEY_I => 'i',
                    _ => '?',
                };
                output.push(ch);
                decoded_ops.push(ch.to_string());
            }
        }

        let final_text: String = output.into_iter().collect();
        assert_eq!(final_text, "tối ");
        assert_eq!(
            decoded_ops,
            vec!["t", "o", "i", "BS", "BS", "ô", "i", "BS", "BS", "ố", "i", "Space"]
        );
    }
}
