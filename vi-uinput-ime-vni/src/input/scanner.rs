use crate::xkb_gen::VIRTUAL_KBD_NAME;
use anyhow::Result;
use evdev::{AbsoluteAxisType, Device, Key, RelativeAxisType};
use inotify::{Inotify, WatchMask};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// Physical keyboard with A..Z keys (and no pointer axes unless explicitly whitelisted) -> grab exclusively (`EVIOCGRAB`).
    Keyboard,
    /// Mouse / touchpad with pointer buttons & axes -> open read-only (NEVER grab).
    Mouse,
}

#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    pub path: PathBuf,
    pub name: String,
    pub kind: DeviceKind,
}

/// Optional device include/exclude filter matching device names, `/dev/input/event*` paths,
/// or `/dev/input/by-id/*` symlinks (case-insensitive substring or exact match).
#[derive(Debug, Clone, Default)]
pub struct DeviceFilter {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl DeviceFilter {
    pub fn new(include: Vec<String>, exclude: Vec<String>) -> Self {
        Self { include, exclude }
    }

    fn matches_any(patterns: &[String], name: &str, path: &Path, by_id_paths: &[String]) -> bool {
        let name_lower = name.to_ascii_lowercase();
        let path_str = path.to_string_lossy().to_ascii_lowercase();

        patterns.iter().any(|raw| {
            let pat = raw.trim().to_ascii_lowercase();
            if pat.is_empty() {
                return false;
            }
            if name_lower.contains(&pat) || path_str == pat || path_str.contains(&pat) {
                return true;
            }
            by_id_paths
                .iter()
                .any(|p| p.to_ascii_lowercase().contains(&pat))
        })
    }

    pub fn is_excluded(&self, name: &str, path: &Path, by_id_paths: &[String]) -> bool {
        Self::matches_any(&self.exclude, name, path, by_id_paths)
    }

    pub fn is_included(&self, name: &str, path: &Path, by_id_paths: &[String]) -> bool {
        Self::matches_any(&self.include, name, path, by_id_paths)
    }
}

/// Extracted evdev capability summary so device classification can be unit-tested deterministically.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeviceCapabilities {
    pub has_alpha_keys: bool,
    pub has_mouse_buttons: bool,
    pub has_rel_xy: bool,
    pub has_abs_pointer: bool,
}

impl DeviceCapabilities {
    pub fn has_pointer_axes(&self) -> bool {
        self.has_rel_xy || self.has_abs_pointer
    }
}

/// Inspect a real Linux `evdev::Device` to extract keyboard and pointer capability flags.
pub fn extract_capabilities(device: &Device) -> DeviceCapabilities {
    let keys = device.supported_keys();

    let has_alpha_keys = keys.map_or(false, |k| {
        (Key::KEY_Q.code()..=Key::KEY_P.code()).all(|c| k.contains(Key::new(c)))
            && (Key::KEY_A.code()..=Key::KEY_L.code()).all(|c| k.contains(Key::new(c)))
            && (Key::KEY_Z.code()..=Key::KEY_M.code()).all(|c| k.contains(Key::new(c)))
    });

    let has_mouse_buttons = keys.map_or(false, |k| {
        k.contains(Key::BTN_LEFT) || k.contains(Key::BTN_RIGHT) || k.contains(Key::BTN_MIDDLE)
    });

    let has_rel_xy = device.supported_relative_axes().map_or(false, |rel| {
        rel.contains(RelativeAxisType::REL_X) && rel.contains(RelativeAxisType::REL_Y)
    });

    let has_abs_xy = device.supported_absolute_axes().map_or(false, |abs| {
        abs.contains(AbsoluteAxisType::ABS_X) && abs.contains(AbsoluteAxisType::ABS_Y)
    });
    let has_touch_or_left = keys.map_or(false, |k| {
        k.contains(Key::BTN_TOUCH) || k.contains(Key::BTN_LEFT)
    });
    let has_abs_pointer = has_abs_xy && has_touch_or_left;

    DeviceCapabilities {
        has_alpha_keys,
        has_mouse_buttons,
        has_rel_xy,
        has_abs_pointer,
    }
}

/// Resolve all `/dev/input/by-id/*` symlink paths pointing to `event_path`.
pub fn resolve_by_id_paths(event_path: &Path) -> Vec<String> {
    let Ok(target_canon) = fs::canonicalize(event_path) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir("/dev/input/by-id") else {
        return Vec::new();
    };
    let mut matches = Vec::new();
    for entry in entries.flatten() {
        let link_path = entry.path();
        if let Ok(resolved) = fs::canonicalize(&link_path) {
            if resolved == target_canon {
                matches.push(link_path.to_string_lossy().into_owned());
            }
        }
    }
    matches
}

/// Pure classification logic for input devices:
/// - Own virtual device (`vi-uinput-virtual-kbd`) -> always ignored.
/// - Excluded via `device_exclude` -> ignored.
/// - `has_alpha_keys && !has_pointer_axes` -> `DeviceKind::Keyboard` (supports QMK/VIA keyboards
///   that advertise `BTN_LEFT`/`BTN_RIGHT` for Mouse Keys without real pointer axes).
/// - `has_alpha_keys && has_pointer_axes` (combo keyboard+mouse dongle) -> logs warning and NEVER
///   grabs unless explicitly whitelisted in `device_include` (falls back to read-only `Mouse` if
///   it has mouse buttons).
/// - `has_mouse_buttons && has_pointer_axes` -> `DeviceKind::Mouse` (read-only, never grabbed).
pub fn classify_capabilities(
    name: &str,
    path: &Path,
    by_id_paths: &[String],
    caps: DeviceCapabilities,
    filter: &DeviceFilter,
) -> Option<DeviceKind> {
    // Critical anti-feedback guard
    if name == VIRTUAL_KBD_NAME {
        debug!("Skipping own virtual device {} ({})", name, path.display());
        return None;
    }

    if filter.is_excluded(name, path, by_id_paths) {
        info!(
            "Skipping excluded input device '{}' ({})",
            name,
            path.display()
        );
        return None;
    }

    let explicitly_included = filter.is_included(name, path, by_id_paths);
    let has_pointer_axes = caps.has_pointer_axes();

    if caps.has_alpha_keys && !has_pointer_axes {
        if !filter.include.is_empty() && !explicitly_included {
            debug!(
                "Skipping keyboard '{}' ({}) — not in device_include whitelist",
                name,
                path.display()
            );
            return None;
        }
        return Some(DeviceKind::Keyboard);
    }

    if caps.has_alpha_keys && has_pointer_axes {
        if explicitly_included {
            warn!(
                "Combo keyboard+pointer device '{}' ({}) explicitly matched `device_include`; grabbing exclusively.",
                name,
                path.display()
            );
            return Some(DeviceKind::Keyboard);
        }
        warn!(
            "Detected combo keyboard+pointer device '{}' ({}, by-id={:?}) with BOTH alpha keys and pointer axes. \
             Skipping exclusive grab to avoid freezing mouse/touchpad; add its name or /dev/input/by-id/... path to `device_include` if you explicitly want to grab it.",
            name,
            path.display(),
            by_id_paths
        );
        if caps.has_mouse_buttons {
            return Some(DeviceKind::Mouse);
        }
        return None;
    }

    if caps.has_mouse_buttons && has_pointer_axes {
        return Some(DeviceKind::Mouse);
    }

    None
}

/// Classify a `/dev/input/event*` device node.
/// Strictly ignores our own virtual keyboard `"vi-uinput-virtual-kbd"` to prevent feedback loops.
pub fn classify_device(path: &Path, filter: &DeviceFilter) -> Option<DiscoveredDevice> {
    let device = Device::open(path).ok()?;
    let name = device.name().unwrap_or("unknown").to_string();
    let caps = extract_capabilities(&device);
    let by_id_paths = resolve_by_id_paths(path);

    let kind = classify_capabilities(&name, path, &by_id_paths, caps, filter)?;
    Some(DiscoveredDevice {
        path: path.to_path_buf(),
        name,
        kind,
    })
}

/// Scan all currently attached `/dev/input/event*` devices.
pub fn scan_existing_devices(filter: &DeviceFilter) -> Vec<DiscoveredDevice> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir("/dev/input") else {
        warn!("Cannot read /dev/input — check permissions or udev rules");
        return found;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(fname) = path.file_name().and_then(|s| s.to_str()) {
            if fname.starts_with("event") {
                if let Some(dev) = classify_device(&path, filter) {
                    found.push(dev);
                }
            }
        }
    }
    found
}

/// Spawn an async `inotify` watcher on `/dev/input` that discovers hotplugged keyboards and mice
/// and sends them to the Grabber supervisor.
pub fn spawn_hotplug_scanner(
    tx: mpsc::UnboundedSender<DiscoveredDevice>,
    filter: DeviceFilter,
) -> Result<()> {
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for dev in scan_existing_devices(&filter) {
        info!(
            "Initial input device detected: {:?} '{}' ({})",
            dev.kind,
            dev.name,
            dev.path.display()
        );
        seen.insert(dev.path.clone());
        let _ = tx.send(dev);
    }

    let inotify = Inotify::init()?;
    inotify.watches().add(
        "/dev/input",
        WatchMask::CREATE | WatchMask::ATTRIB | WatchMask::DELETE,
    )?;

    tokio::spawn(async move {
        let mut buffer = [0u8; 4096];
        let Ok(mut stream) = inotify.into_event_stream(&mut buffer) else {
            warn!("Failed to start inotify async stream on /dev/input");
            return;
        };

        while let Some(event_res) = stream.next().await {
            let Ok(event) = event_res else { continue };
            let Some(name_os) = event.name else { continue };
            let name = name_os.to_string_lossy();
            if !name.starts_with("event") {
                continue;
            }
            let path = PathBuf::from("/dev/input").join(name.as_ref());

            if event.mask.contains(inotify::EventMask::DELETE) {
                seen.remove(&path);
                continue;
            }

            if seen.contains(&path) {
                continue;
            }

            // Brief settle time for udev rule to apply group/mode permissions on newly plugged USB devices
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            if let Some(dev) = classify_device(&path, &filter) {
                info!(
                    "Hotplugged device detected: {:?} '{}' ({})",
                    dev.kind,
                    dev.name,
                    dev.path.display()
                );
                seen.insert(path);
                let _ = tx.send(dev);
            }
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qmk_keyboard_with_mouse_keys_and_no_pointer_axes_is_classified_as_keyboard() {
        let filter = DeviceFilter::default();
        let qmk_caps = DeviceCapabilities {
            has_alpha_keys: true,
            has_mouse_buttons: true, // QMK/VIA Mouse Keys advertises BTN_LEFT/BTN_RIGHT
            has_rel_xy: false,
            has_abs_pointer: false,
        };
        let kind = classify_capabilities(
            "Keychron Q1 Pro QMK/VIA",
            Path::new("/dev/input/event4"),
            &["/dev/input/by-id/usb-Keychron_Q1_Pro-event-kbd".to_string()],
            qmk_caps,
            &filter,
        );
        assert_eq!(kind, Some(DeviceKind::Keyboard));
    }

    #[test]
    fn mouse_and_touchpad_with_pointer_axes_are_classified_as_mouse() {
        let filter = DeviceFilter::default();
        let mouse_caps = DeviceCapabilities {
            has_alpha_keys: false,
            has_mouse_buttons: true,
            has_rel_xy: true,
            has_abs_pointer: false,
        };
        assert_eq!(
            classify_capabilities(
                "Logitech G Pro Wireless",
                Path::new("/dev/input/event7"),
                &[],
                mouse_caps,
                &filter,
            ),
            Some(DeviceKind::Mouse)
        );

        let touchpad_caps = DeviceCapabilities {
            has_alpha_keys: false,
            has_mouse_buttons: true,
            has_rel_xy: false,
            has_abs_pointer: true,
        };
        assert_eq!(
            classify_capabilities(
                "ELAN0676:00 Touchpad",
                Path::new("/dev/input/event9"),
                &[],
                touchpad_caps,
                &filter,
            ),
            Some(DeviceKind::Mouse)
        );
    }

    #[test]
    fn combo_dongle_not_grabbed_unless_whitelisted_and_exclude_overrides() {
        let combo_caps = DeviceCapabilities {
            has_alpha_keys: true,
            has_mouse_buttons: true,
            has_rel_xy: true,
            has_abs_pointer: false,
        };
        let by_id = vec!["/dev/input/by-id/usb-Logitech_USB_Receiver-event-if02".to_string()];

        // Default: combo device must NOT be grabbed as Keyboard (falls back to read-only Mouse)
        assert_eq!(
            classify_capabilities(
                "Logitech USB Receiver",
                Path::new("/dev/input/event10"),
                &by_id,
                combo_caps,
                &DeviceFilter::default(),
            ),
            Some(DeviceKind::Mouse)
        );

        // Explicitly whitelisted by /dev/input/by-id path -> grabbed as Keyboard
        let include_filter = DeviceFilter::new(
            vec!["/dev/input/by-id/usb-Logitech_USB_Receiver-event-if02".to_string()],
            vec![],
        );
        assert_eq!(
            classify_capabilities(
                "Logitech USB Receiver",
                Path::new("/dev/input/event10"),
                &by_id,
                combo_caps,
                &include_filter,
            ),
            Some(DeviceKind::Keyboard)
        );

        // Excluded keyboard -> ignored
        let exclude_filter = DeviceFilter::new(vec![], vec!["Keychron".to_string()]);
        let kbd_caps = DeviceCapabilities {
            has_alpha_keys: true,
            has_mouse_buttons: false,
            has_rel_xy: false,
            has_abs_pointer: false,
        };
        assert_eq!(
            classify_capabilities(
                "Keychron Q1 Pro",
                Path::new("/dev/input/event4"),
                &[],
                kbd_caps,
                &exclude_filter,
            ),
            None
        );

        // Own virtual keyboard is always skipped
        assert_eq!(
            classify_capabilities(
                VIRTUAL_KBD_NAME,
                Path::new("/dev/input/event99"),
                &[],
                kbd_caps,
                &DeviceFilter::default(),
            ),
            None
        );
    }
}
