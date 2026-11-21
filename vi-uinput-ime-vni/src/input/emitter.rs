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
    /// Emit a complete Backspace press + SYN + release + SYN cycle.
    Backspace,
    /// Emit a precomposed Vietnamese character via `vi-uinput-virtual-kbd` XKB Levels 5–8.
    VietnameseChar(char),
    /// Emit a plain ASCII character (with Shift wrap when uppercase or shifted symbol).
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
/// preserving any physically held Shift modifier so fast Shift rollover never gets clobbered.
#[allow(dead_code)]
pub fn encode_output_item(item: &OutputItem) -> Vec<Vec<InputEvent>> {
    encode_output_item_with_shift(item, false)
}

pub fn encode_output_item_with_shift(
    item: &OutputItem,
    phys_shift_held: bool,
) -> Vec<Vec<InputEvent>> {
    let syn = || InputEvent::new(EventType::SYNCHRONIZATION, 0, 0);
    let key_ev = |code: Key, val: i32| InputEvent::new(EventType::KEY, code.code(), val);

    match item {
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
            if let Some((key, need_shift)) = ascii_to_key(*ch) {
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
        '0' => Key::KEY_0,
        '1' => Key::KEY_1,
        '2' => Key::KEY_2,
        '3' => Key::KEY_3,
        '4' => Key::KEY_4,
        '5' => Key::KEY_5,
        '6' => Key::KEY_6,
        '7' => Key::KEY_7,
        '8' => Key::KEY_8,
        '9' => Key::KEY_9,
        ' ' => Key::KEY_SPACE,
        '-' => Key::KEY_MINUS,
        '=' => Key::KEY_EQUAL,
        '[' => Key::KEY_LEFTBRACE,
        ']' => Key::KEY_RIGHTBRACE,
        ';' => Key::KEY_SEMICOLON,
        '\'' => Key::KEY_APOSTROPHE,
        '`' => Key::KEY_GRAVE,
        ',' => Key::KEY_COMMA,
        '.' => Key::KEY_DOT,
        '/' => Key::KEY_SLASH,
        '\\' => Key::KEY_BACKSLASH,
        _ => return None,
    };
    Some((key, shift))
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

        while let Some(item) = rx.recv().await {
            if let OutputItem::PassThrough { code, value } = item {
                if code == Key::KEY_LEFTSHIFT {
                    phys_lshift = value != 0;
                } else if code == Key::KEY_RIGHTSHIFT {
                    phys_rshift = value != 0;
                }
            }

            let is_synthetic = !matches!(item, OutputItem::PassThrough { .. });
            let frames = encode_output_item_with_shift(&item, phys_lshift || phys_rshift);

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
    use crate::engine::vni::VniEngine;
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

    /// Simulates an ultra-fast 0ms VNI burst: `t`, `o`, `i`, `6`, `1`, `Space` while the FIFO
    /// worker is running with `delay = 5ms`. Verifies `Space` NEVER overtakes the delayed
    /// synthetic `BS, BS, ố, i` batch (output is always `tối `, never `tố i`).
    #[tokio::test]
    async fn strict_fifo_prevents_space_overtaking_delayed_synthetic_batch() {
        let log = Arc::new(Mutex::new(Vec::<InputEvent>::new()));
        let sink = RecordingSink {
            log: Arc::clone(&log),
        };

        let (handle, worker) = spawn_fifo_emitter(sink, 5);
        let mut engine = VniEngine::new(ToneStyle::New);

        // User bursts `t`, `o`, `i`, `6`, `1`, `Space` with 0ms delay between physical presses
        for ch in ['t', 'o', 'i', '6', '1'] {
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
