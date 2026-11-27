use crate::input::scanner::{DeviceKind, DiscoveredDevice};
use anyhow::{Context, Result};
use evdev::{Device, EventType, Key, LedType};
use std::collections::HashSet;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Linux ioctl request code for `EVIOCGRAB` (`_IOW('E', 0x90, int)`).
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;

/// Global registry of currently grabbed keyboard raw file descriptors so our
/// `panic::set_hook` and `SIGINT`/`SIGTERM` handler can unconditionally ungrab
/// every physical keyboard even during abnormal termination.
static GRABBED_FDS: OnceLock<Mutex<HashSet<RawFd>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashSet<RawFd>> {
    GRABBED_FDS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn register_grabbed_fd(fd: RawFd) {
    if let Ok(mut set) = registry().lock() {
        set.insert(fd);
    }
}

fn unregister_grabbed_fd(fd: RawFd) {
    if let Ok(mut set) = registry().lock() {
        set.remove(&fd);
    }
}

/// Emergency fail-safe: immediately issue `ioctl(fd, EVIOCGRAB, 0)` on every
/// grabbed keyboard fd so the user's keyboard is NEVER left locked on panic/signal.
pub fn emergency_ungrab_all() {
    if let Ok(mut set) = registry().lock() {
        for &fd in set.iter() {
            unsafe {
                libc::ioctl(fd, EVIOCGRAB as _, 0 as libc::c_int);
            }
        }
        set.clear();
    }
}

/// Install a process-wide panic hook that guarantees physical keyboards are released
/// before unwinding or aborting.
pub fn install_failsafe_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        emergency_ungrab_all();
        eprintln!("[vi-uinput-ime] FAIL-SAFE: Released all EVIOCGRAB keyboard locks before panic.");
        default_hook(panic_info);
    }));
}

/// RAII guard holding an exclusively grabbed `evdev::Device`.
/// Dropping this guard deterministically calls `ungrab()` and removes the fd from the registry.
pub struct GrabbedKeyboardGuard {
    device: Device,
    raw_fd: RawFd,
    name: String,
}

impl GrabbedKeyboardGuard {
    pub fn open_and_grab(dev_info: &DiscoveredDevice) -> Result<Self> {
        let mut device = Device::open(&dev_info.path)
            .with_context(|| format!("failed to open keyboard {}", dev_info.path.display()))?;
        device
            .grab()
            .with_context(|| format!("EVIOCGRAB failed on {}", dev_info.path.display()))?;
        let raw_fd = device.as_raw_fd();
        register_grabbed_fd(raw_fd);
        info!(
            "Exclusively grabbed keyboard '{}' ({})",
            dev_info.name,
            dev_info.path.display()
        );
        Ok(Self {
            device,
            raw_fd,
            name: dev_info.name.clone(),
        })
    }

    /// Query the physical keyboard's current `LED_CAPSL` state via evdev `EVIOCGLED`.
    pub fn caps_lock_led_state(&self) -> Option<bool> {
        match self.device.get_led_state() {
            Ok(leds) => Some(leds.contains(LedType::LED_CAPSL)),
            Err(err) => {
                warn!("Cannot query LED_CAPSL state on '{}': {}", self.name, err);
                None
            }
        }
    }

    pub fn into_event_stream(self) -> Result<(evdev::EventStream, FdUngrabToken)> {
        let raw_fd = self.raw_fd;
        let name = self.name.clone();
        // Prevent Drop from ungrabbing prematurely while EventStream owns the fd;
        // ownership transfers to `FdUngrabToken`.
        let manually = std::mem::ManuallyDrop::new(self);
        // SAFETY: `manually.device` is moved out once and `manually` is never dropped.
        let device = unsafe { std::ptr::read(&manually.device) };
        let stream = device.into_event_stream()?;
        Ok((stream, FdUngrabToken { raw_fd, name }))
    }
}

impl Drop for GrabbedKeyboardGuard {
    fn drop(&mut self) {
        let _ = self.device.ungrab();
        unregister_grabbed_fd(self.raw_fd);
        info!("Released EVIOCGRAB on keyboard '{}'", self.name);
    }
}

/// Companion RAII token dropped when the async keyboard reader task exits (or disconnects).
pub struct FdUngrabToken {
    raw_fd: RawFd,
    name: String,
}

impl Drop for FdUngrabToken {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.raw_fd, EVIOCGRAB as _, 0 as libc::c_int);
        }
        unregister_grabbed_fd(self.raw_fd);
        info!("Released EVIOCGRAB on keyboard '{}' (stream closed)", self.name);
    }
}

/// High-level events sent from grabbed keyboards, read-only mice, and compositor IPC
/// to the central Event Router & Context Manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouterEvent {
    RawKey { code: Key, value: i32 },
    /// Initial hardware CapsLock LED state read from `evdev::Device::get_led_state()`.
    CapsLockSync(bool),
    MouseReset,
    WindowFocusChanged { app_class: String, is_xwayland: bool },
    WorkspaceChanged,
    /// Desktop active layout/IME state (`active = true` ONLY when `vi_uinput` is active).
    LayoutStateChanged { active: bool, layout_id: String },
    /// Emitted internally when the last user-session `--ipc-bridge` client disconnects.
    IpcBridgeDisconnected,
}

/// Spawn an async reader task for a discovered keyboard (exclusive EVIOCGRAB)
/// or mouse (passive read-only, never grabbed).
pub fn spawn_device_reader(
    dev: DiscoveredDevice,
    router_tx: mpsc::UnboundedSender<RouterEvent>,
) {
    match dev.kind {
        DeviceKind::Keyboard => {
            tokio::spawn(async move {
                let guard = match GrabbedKeyboardGuard::open_and_grab(&dev) {
                    Ok(g) => g,
                    Err(err) => {
                        error!("Cannot grab keyboard '{}': {:#}", dev.name, err);
                        return;
                    }
                };

                if let Some(caps_on) = guard.caps_lock_led_state() {
                    info!(
                        "Synced initial hardware CapsLock LED from '{}': {}",
                        dev.name,
                        if caps_on { "ON" } else { "OFF" }
                    );
                    let _ = router_tx.send(RouterEvent::CapsLockSync(caps_on));
                }

                let (mut stream, _ungrab_token) = match guard.into_event_stream() {
                    Ok(pair) => pair,
                    Err(err) => {
                        error!("Failed to create event stream for '{}': {:#}", dev.name, err);
                        return;
                    }
                };

                loop {
                    match stream.next_event().await {
                        Ok(ev) => {
                            if ev.event_type() == EventType::KEY {
                                let _ = router_tx.send(RouterEvent::RawKey {
                                    code: Key::new(ev.code()),
                                    value: ev.value(),
                                });
                            }
                        }
                        Err(err) => {
                            warn!("Keyboard '{}' disconnected or errored: {}", dev.name, err);
                            break;
                        }
                    }
                }
            });
        }
        DeviceKind::Mouse => {
            tokio::spawn(async move {
                // CRITICAL: NEVER call grab() on a mouse device; open strictly read-only.
                let device = match Device::open(&dev.path) {
                    Ok(d) => d,
                    Err(err) => {
                        warn!("Cannot open mouse '{}' read-only: {}", dev.name, err);
                        return;
                    }
                };
                let mut stream = match device.into_event_stream() {
                    Ok(s) => s,
                    Err(err) => {
                        warn!("Cannot stream mouse '{}': {}", dev.name, err);
                        return;
                    }
                };
                info!(
                    "Monitoring mouse '{}' read-only for click-to-reset ({})",
                    dev.name,
                    dev.path.display()
                );

                loop {
                    match stream.next_event().await {
                        Ok(ev) => {
                            if ev.event_type() == EventType::KEY && ev.value() == 1 {
                                let key = Key::new(ev.code());
                                if matches!(key, Key::BTN_LEFT | Key::BTN_RIGHT | Key::BTN_MIDDLE) {
                                    let _ = router_tx.send(RouterEvent::MouseReset);
                                }
                            }
                        }
                        Err(err) => {
                            warn!("Mouse '{}' disconnected: {}", dev.name, err);
                            break;
                        }
                    }
                }
            });
        }
    }
}
