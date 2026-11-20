use crate::xkb_gen::VIRTUAL_KBD_NAME;
use anyhow::Result;
use evdev::{Device, Key};
use inotify::{Inotify, WatchMask};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// Physical keyboard with A..Z keys -> grab exclusively (`EVIOCGRAB`).
    Keyboard,
    /// Mouse / touchpad with BTN_LEFT/BTN_RIGHT -> open read-only (NEVER grab).
    Mouse,
}

#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    pub path: PathBuf,
    pub name: String,
    pub kind: DeviceKind,
}

/// Classify a `/dev/input/event*` device node.
/// Strictly ignores our own virtual keyboard `"vi-uinput-virtual-kbd"` to prevent feedback loops.
pub fn classify_device(path: &Path) -> Option<DiscoveredDevice> {
    let device = Device::open(path).ok()?;
    let name = device.name().unwrap_or("unknown").to_string();

    // Critical anti-feedback guard
    if name == VIRTUAL_KBD_NAME {
        debug!("Skipping own virtual device {} ({})", name, path.display());
        return None;
    }

    let keys = device.supported_keys()?;

    let has_alpha_keys = (Key::KEY_Q.code()..=Key::KEY_P.code()).all(|c| keys.contains(Key::new(c)))
        && (Key::KEY_A.code()..=Key::KEY_L.code()).all(|c| keys.contains(Key::new(c)))
        && (Key::KEY_Z.code()..=Key::KEY_M.code()).all(|c| keys.contains(Key::new(c)));

    let has_mouse_buttons = keys.contains(Key::BTN_LEFT) || keys.contains(Key::BTN_RIGHT);

    if has_alpha_keys && !has_mouse_buttons {
        return Some(DiscoveredDevice {
            path: path.to_path_buf(),
            name,
            kind: DeviceKind::Keyboard,
        });
    }

    if has_mouse_buttons {
        return Some(DiscoveredDevice {
            path: path.to_path_buf(),
            name,
            kind: DeviceKind::Mouse,
        });
    }

    None
}

/// Scan all currently attached `/dev/input/event*` devices.
pub fn scan_existing_devices() -> Vec<DiscoveredDevice> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir("/dev/input") else {
        warn!("Cannot read /dev/input — check permissions or udev rules");
        return found;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(fname) = path.file_name().and_then(|s| s.to_str()) {
            if fname.starts_with("event") {
                if let Some(dev) = classify_device(&path) {
                    found.push(dev);
                }
            }
        }
    }
    found
}

/// Spawn an async `inotify` watcher on `/dev/input` that discovers hotplugged keyboards and mice
/// and sends them to the Grabber supervisor.
pub fn spawn_hotplug_scanner(tx: mpsc::UnboundedSender<DiscoveredDevice>) -> Result<()> {
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for dev in scan_existing_devices() {
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
            if let Some(dev) = classify_device(&path) {
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
