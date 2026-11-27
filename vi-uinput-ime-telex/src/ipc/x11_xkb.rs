use crate::xkb_gen;
use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{debug, info, warn};

const HEAL_COOLDOWN: Duration = Duration::from_millis(700);

fn last_heal_instant() -> &'static Mutex<Option<Instant>> {
    static LAST_HEAL: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    LAST_HEAL.get_or_init(|| Mutex::new(None))
}

/// Returns `true` only inside a pure X11 graphical session (`DISPLAY` set, `WAYLAND_DISPLAY` unset).
pub fn is_pure_x11_session() -> bool {
    let has_display = env::var("DISPLAY")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let has_wayland = env::var("WAYLAND_DISPLAY")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    has_display && !has_wayland
}

/// Inspect an `xkbcomp $DISPLAY -` dump and verify whether Xorg's live keymap currently has
/// all required `vi_uinput` definitions (CapsLock-immune `VI_EIGHT_LEVEL*` types plus `<I120>`
/// -> `ISO_Level5_Shift` / `Mod3` and `<I149>` -> `ISO_Level3_Shift` / `Mod5`).
///
/// Returning `true` allows `--ipc-bridge` to skip redundant `xkbcomp` reloads when `/usr/share/X11/xkb`
/// is already installed and untouched, preventing any `MappingNotify` feedback loop.
pub fn is_x11_keymap_healthy(xkb_dump: &str) -> bool {
    let has_types = xkb_dump.contains("type \"VI_EIGHT_LEVEL\"")
        && xkb_dump.contains("type \"VI_EIGHT_LEVEL_ALPHA\"");
    let has_syms =
        xkb_dump.contains("ISO_Level5_Shift") && xkb_dump.contains("ISO_Level3_Shift");
    let has_mod3 = xkb_dump
        .lines()
        .any(|l| l.contains("modifier_map Mod3") && l.contains("<I120>"));
    let has_mod5 = xkb_dump
        .lines()
        .any(|l| l.contains("modifier_map Mod5") && l.contains("<I149>"));

    has_types && has_syms && has_mod3 && has_mod5
}

/// Extract a named field (e.g. `"layout"`, `"variant"`, `"options"`) from `setxkbmap -query` output.
pub fn parse_setxkbmap_query_field(query_stdout: &str, field: &str) -> Option<String> {
    let prefix = format!("{}:", field);
    for line in query_stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            let val = rest.trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// Parse the comma-separated `layout:` list from `setxkbmap -query` output.
pub fn parse_setxkbmap_query_layouts(query_stdout: &str) -> Vec<String> {
    parse_setxkbmap_query_field(query_stdout, "layout")
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Build an XKB blueprint (`xkb_keymap { ... }`) preserving a multi-group layout list
/// (e.g. `["us", "vi_uinput"]` -> `pc+us+vi_uinput:2+inet(evdev)`) as a fallback when `setxkbmap -print`
/// is unavailable or fails.
pub fn build_xkbcomp_blueprint(layouts: &[String]) -> String {
    let mut symbols_parts = vec!["pc".to_string()];
    if layouts.is_empty() {
        symbols_parts.push("vi_uinput".to_string());
    } else {
        for (idx, layout) in layouts.iter().enumerate() {
            let clean = layout.trim();
            if clean.is_empty() {
                continue;
            }
            if idx == 0 {
                symbols_parts.push(clean.to_string());
            } else {
                symbols_parts.push(format!("{}:{}", clean, idx + 1));
            }
        }
    }
    symbols_parts.push("inet(evdev)".to_string());

    format!(
        "xkb_keymap {{\n\
         \txkb_keycodes  {{ include \"evdev+aliases(qwerty)\" }};\n\
         \txkb_types     {{ include \"complete\" }};\n\
         \txkb_compat    {{ include \"complete\" }};\n\
         \txkb_symbols   {{ include \"{}\" }};\n\
         \txkb_geometry  {{ include \"pc(pc104)\" }};\n\
         }};\n",
        symbols_parts.join("+")
    )
}

/// Materialize an always-up-to-date runtime XKB overlay directory containing `symbols/vi_uinput`
/// and `types/complete` (with `VI_EIGHT_LEVEL` and `VI_EIGHT_LEVEL_ALPHA`) so `xkbcomp -I<dir>`
/// works on X11 even if `/usr/share/X11/xkb` was not patched or was overwritten by a distro update.
pub fn ensure_runtime_xkb_dir() -> Result<PathBuf> {
    let base = if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime).join("vi-uinput-ime-xkb")
    } else {
        let uid = unsafe { libc::geteuid() };
        env::temp_dir().join(format!("vi-uinput-ime-xkb-{}", uid))
    };

    let sym_dir = base.join("symbols");
    let types_dir = base.join("types");
    fs::create_dir_all(&sym_dir)
        .with_context(|| format!("failed to create {}", sym_dir.display()))?;
    fs::create_dir_all(&types_dir)
        .with_context(|| format!("failed to create {}", types_dir.display()))?;

    let sym_path = sym_dir.join("vi_uinput");
    let expected_sym = xkb_gen::generate_gnome_xkb_symbols();
    if fs::read_to_string(&sym_path).ok().as_deref() != Some(expected_sym.as_str()) {
        fs::write(&sym_path, &expected_sym)
            .with_context(|| format!("failed to write {}", sym_path.display()))?;
    }

    let types_path = types_dir.join("complete");
    let expected_types = xkb_gen::generate_gnome_xkb_types();
    if fs::read_to_string(&types_path).ok().as_deref() != Some(expected_types) {
        fs::write(&types_path, expected_types)
            .with_context(|| format!("failed to write {}", types_path.display()))?;
    }

    Ok(base)
}

/// Query current X11 layout list via `setxkbmap -query` (used both for keymap self-healing and
/// as a KDE X11 fallback when `org.kde.keyboard` D-Bus is inactive on single-layout setups).
pub async fn query_x11_setxkbmap_raw() -> Option<String> {
    let output = Command::new("setxkbmap")
        .arg("-query")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

pub async fn query_x11_layouts() -> Option<Vec<String>> {
    let raw = query_x11_setxkbmap_raw().await?;
    let layouts = parse_setxkbmap_query_layouts(&raw);
    if layouts.is_empty() {
        None
    } else {
        Some(layouts)
    }
}

async fn dump_current_x11_keymap(display: &str) -> Option<String> {
    let output = Command::new("xkbcomp")
        .args(["-w", "0", display, "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn generate_blueprint_with_setxkbmap(
    layouts: &[String],
    variant: Option<&str>,
    options: Option<&str>,
) -> Option<String> {
    let layout_csv = layouts.join(",");
    let mut cmd = Command::new("setxkbmap");
    cmd.args(["-layout", &layout_csv]);
    if let Some(v) = variant {
        cmd.args(["-variant", v]);
    }
    if let Some(o) = options {
        cmd.args(["-option", "", "-option", o]);
    }
    cmd.arg("-print");
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    if text.contains("xkb_keymap") {
        Some(text)
    } else {
        None
    }
}

/// Ensure the live Xorg X11 keymap on `$DISPLAY` contains valid `VI_EIGHT_LEVEL*` types and
/// `<I120>`/`<I149>` level modifiers whenever `vi_uinput` is active or Xorg resets keymaps
/// (e.g. Virtual Core Keyboard reset or hotplug of `vi-uinput-virtual-kbd`).
///
/// Returns `true` if a self-healing `xkbcomp` reload was performed, `false` if skipped/already healthy.
pub async fn ensure_x11_vi_uinput_keymap(
    layouts_hint: Option<&[String]>,
    active_group_hint: Option<usize>,
) -> bool {
    if !is_pure_x11_session() {
        return false;
    }
    let Ok(x11_display) = env::var("DISPLAY") else {
        return false;
    };

    // Cooldown prevents re-triggering on the `MappingNotify` emitted by our own `xkbcomp` call.
    if let Ok(guard) = last_heal_instant().lock() {
        if let Some(prev) = *guard {
            if prev.elapsed() < HEAL_COOLDOWN {
                debug!("Skipping X11 keymap check within self-heal cooldown");
                return false;
            }
        }
    }

    if let Some(dump) = dump_current_x11_keymap(&x11_display).await {
        if is_x11_keymap_healthy(&dump) {
            debug!(
                "X11 keymap on {} is already healthy (VI_EIGHT_LEVEL + <I120>/<I149> intact)",
                x11_display
            );
            return false;
        }
    }

    let query_raw = query_x11_setxkbmap_raw().await.unwrap_or_default();
    let queried_layouts = parse_setxkbmap_query_layouts(&query_raw);
    let variant = parse_setxkbmap_query_field(&query_raw, "variant");
    let options = parse_setxkbmap_query_field(&query_raw, "options");

    let target_layouts: Vec<String> = match layouts_hint {
        Some(list) if !list.is_empty() && list.iter().any(|l| l == "vi_uinput" || l.starts_with("vi_uinput+")) => {
            list.to_vec()
        }
        _ if queried_layouts.iter().any(|l| l == "vi_uinput" || l.starts_with("vi_uinput+")) => {
            queried_layouts
        }
        _ => {
            // Only heal when `vi_uinput` is actually part of the user's configured layout list.
            debug!("No `vi_uinput` layout present in X11 layout list; skipping xkbcomp heal");
            return false;
        }
    };

    let runtime_xkb_dir = match ensure_runtime_xkb_dir() {
        Ok(dir) => dir,
        Err(err) => {
            warn!("Failed to prepare runtime XKB overlay dir for X11 heal: {:#}", err);
            return false;
        }
    };

    let blueprint = generate_blueprint_with_setxkbmap(
        &target_layouts,
        variant.as_deref(),
        options.as_deref(),
    )
    .await
    .unwrap_or_else(|| build_xkbcomp_blueprint(&target_layouts));

    // Stamp cooldown before invoking xkbcomp so the resulting MappingNotify is ignored.
    if let Ok(mut guard) = last_heal_instant().lock() {
        *guard = Some(Instant::now());
    }

    let include_arg = format!("-I{}", runtime_xkb_dir.display());
    let mut child = match Command::new("xkbcomp")
        .args([include_arg.as_str(), "-I/usr/share/X11/xkb", "-w", "0", "-", &x11_display])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(err) => {
            warn!("Cannot run `xkbcomp` to heal X11 keymap on {}: {}", x11_display, err);
            return false;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(blueprint.as_bytes()).await;
    }

    match child.wait_with_output().await {
        Ok(out) if out.status.success() => {
            info!(
                "Self-healed X11 keymap on {} via xkbcomp (layouts={})",
                x11_display,
                target_layouts.join(",")
            );
            // Restore active KDE layout group if KDE switched to a non-zero group (e.g. `us,vi_uinput`).
            if let Some(group_idx) = active_group_hint {
                if group_idx > 0 {
                    let _ = Command::new("gdbus")
                        .args([
                            "call",
                            "--session",
                            "--dest",
                            "org.kde.keyboard",
                            "--object-path",
                            "/Layouts",
                            "--method",
                            "org.kde.KeyboardLayouts.setLayout",
                            &group_idx.to_string(),
                        ])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .await;
                }
            }
            true
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            warn!("`xkbcomp` X11 heal exited with {}: {}", out.status, stderr.trim());
            false
        }
        Err(err) => {
            warn!("Failed waiting for `xkbcomp`: {}", err);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_healthy_vs_broken_x11_keymap_dumps() {
        let healthy = r#"
xkb_keymap {
    xkb_types "complete" {
        type "VI_EIGHT_LEVEL" { modifiers = Shift+Lock+LevelThree+LevelFive; };
        type "VI_EIGHT_LEVEL_ALPHA" { modifiers = Shift+Lock+LevelThree+LevelFive; };
    };
    xkb_symbols "pc+us+vi_uinput:2+inet(evdev)" {
        key <I120> { symbols[Group2]= [ ISO_Level5_Shift ] };
        key <I149> { symbols[Group2]= [ ISO_Level3_Shift ] };
        modifier_map Mod3 { <I120> };
        modifier_map Mod5 { <I149> };
    };
};
"#;
        assert!(is_x11_keymap_healthy(healthy));

        // Missing VI_EIGHT_LEVEL types (exact symptom when only symbols/vi_uinput is copied to /usr/share/X11/xkb)
        let missing_types = r#"
xkb_keymap {
    xkb_types "complete" { type "ONE_LEVEL" { modifiers = none; }; };
    xkb_symbols "pc+us+vi_uinput:2+inet(evdev)" {
        key <I120> { symbols[Group2]= [ ISO_Level5_Shift ] };
        key <I149> { symbols[Group2]= [ ISO_Level3_Shift ] };
        modifier_map Mod3 { <I120> };
        modifier_map Mod5 { <I149> };
    };
};
"#;
        assert!(!is_x11_keymap_healthy(missing_types));

        // Plain US reset by Xorg
        let reset_us = "xkb_keymap { xkb_symbols \"pc+us+inet(evdev)\" { key <AE01> { [ 1, exclam ] }; }; };";
        assert!(!is_x11_keymap_healthy(reset_us));
    }

    #[test]
    fn parses_setxkbmap_query_and_builds_multigroup_blueprint() {
        let query = "\
rules:      evdev
model:      pc105
layout:     us,vi_uinput
variant:    ,
options:    grp:win_space_toggle
";
        assert_eq!(
            parse_setxkbmap_query_layouts(query),
            vec!["us".to_string(), "vi_uinput".to_string()]
        );
        assert_eq!(
            parse_setxkbmap_query_field(query, "options"),
            Some("grp:win_space_toggle".to_string())
        );

        let bp = build_xkbcomp_blueprint(&["us".to_string(), "vi_uinput".to_string()]);
        assert!(bp.contains("include \"pc+us+vi_uinput:2+inet(evdev)\""));
        assert!(bp.contains("xkb_types     { include \"complete\" };"));
    }
}
