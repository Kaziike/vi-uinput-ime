use crate::input::grabber::RouterEvent;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Parse `gdbus call ... org.kde.KeyboardLayouts.getLayoutsList` output, e.g.:
/// `([('us', '', 'English (US)'), ('vi_uinput', '', 'Vietnamese (vi-uinput-ime)')],)`
/// Returns the ordered list of layout short names (first element of each 3-tuple).
pub fn parse_kde_layouts_list(raw: &str) -> Vec<String> {
    let mut layouts = Vec::new();
    let Some(start) = raw.find('[') else {
        return layouts;
    };
    let slice = &raw[start + 1..];
    let mut chars = slice.chars().peekable();

    while let Some(c) = chars.next() {
        if c == ']' {
            break;
        }
        if c != '(' {
            continue;
        }
        let mut tuple_strings = Vec::with_capacity(3);
        while let Some(&next) = chars.peek() {
            if next == ')' {
                chars.next();
                break;
            }
            if next == '\'' || next == '"' {
                let quote = chars.next().unwrap();
                let mut val = String::new();
                while let Some(ch) = chars.next() {
                    if ch == '\\' {
                        if let Some(esc) = chars.next() {
                            val.push(esc);
                        }
                    } else if ch == quote {
                        break;
                    } else {
                        val.push(ch);
                    }
                }
                tuple_strings.push(val);
            } else {
                chars.next();
            }
        }
        if let Some(layout_code) = tuple_strings.into_iter().next() {
            layouts.push(layout_code);
        }
    }

    layouts
}

/// Parse KDE layout index from `getLayout` or `layoutChanged` signal output, e.g.:
/// `(uint32 1,)`, `(0,)`, `/Layouts: org.kde.KeyboardLayouts.layoutChanged (uint32 2,)`.
pub fn parse_kde_layout_index(raw: &str) -> Option<usize> {
    let after_paren = raw.rsplit('(').next()?;
    let mut digits = String::new();
    let mut in_uint_prefix = false;

    for token in after_paren.split(|c: char| !c.is_ascii_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        if token.eq_ignore_ascii_case("uint32") || token.eq_ignore_ascii_case("int32") {
            in_uint_prefix = true;
            continue;
        }
        if token.chars().all(|c| c.is_ascii_digit()) {
            digits.push_str(token);
            break;
        }
        if in_uint_prefix {
            break;
        }
    }

    if digits.is_empty() {
        None
    } else {
        digits.parse::<usize>().ok()
    }
}

/// Determine `(active, layout_id)` for KDE Plasma / KWin from the layout list and active index.
pub fn resolve_kde_layout_state(layouts: &[String], index: usize) -> Option<(bool, String)> {
    let layout = layouts.get(index)?.trim();
    let active = layout == "vi_uinput" || layout.starts_with("vi_uinput+");
    Some((active, format!("kde:{}", layout)))
}

async fn gdbus_call_kde(method: &str) -> Option<String> {
    let output = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.kde.keyboard",
            "--object-path",
            "/Layouts",
            "--method",
            method,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Query KDE's current `(layouts, active_index)` via D-Bus `org.kde.keyboard /Layouts`.
pub async fn query_current_kde_state() -> Option<(Vec<String>, usize)> {
    let raw_list = gdbus_call_kde("org.kde.KeyboardLayouts.getLayoutsList").await?;
    let layouts = parse_kde_layouts_list(&raw_list);
    if layouts.is_empty() {
        return None;
    }
    let raw_idx = gdbus_call_kde("org.kde.KeyboardLayouts.getLayout").await?;
    let idx = parse_kde_layout_index(&raw_idx)?;
    Some((layouts, idx))
}

async fn apply_fcitx5_guard(active: bool, layout_id: String) -> (bool, String) {
    if !active {
        return (false, layout_id);
    }
    if let Some(im) = crate::ipc::fcitx5::query_fcitx5_im().await {
        if !crate::ipc::fcitx5::is_fcitx5_allowing_vi_uinput(&im) {
            return (false, format!("fcitx5:{}", im));
        }
    }
    (true, layout_id)
}

async fn emit_kde_state(
    layouts: &[String],
    index: usize,
    last_sent: &mut Option<(bool, String)>,
    router_tx: &mpsc::UnboundedSender<RouterEvent>,
) {
    if let Some((active, layout_id)) = resolve_kde_layout_state(layouts, index) {
        let (final_active, final_id) = apply_fcitx5_guard(active, layout_id).await;
        // On pure KDE X11, guarantee Xorg's live keymap has VI_EIGHT_LEVEL* & <I120>/<I149>
        // BEFORE the daemon exits PassThrough (zero-cost no-op on KDE Wayland).
        if final_active {
            crate::ipc::x11_xkb::ensure_x11_vi_uinput_keymap(Some(layouts), Some(index)).await;
        }
        let state = (final_active, final_id.clone());
        if last_sent.as_ref() != Some(&state) {
            info!(
                "KDE active keyboard layout -> '{}' (vi_uinput active={})",
                final_id, final_active
            );
            *last_sent = Some(state);
            let _ = router_tx.send(RouterEvent::LayoutStateChanged {
                active: final_active,
                layout_id: final_id,
            });
        }
    }
}

/// Spawn an event-driven KDE Plasma / KWin keyboard layout monitor using `gdbus monitor`
/// on `org.kde.keyboard` (`/Layouts`, `org.kde.KeyboardLayouts`).
/// On pure KDE X11, also verifies/heals Xorg's live keymap via `xkbcomp` when `vi_uinput` is
/// active and falls back to `setxkbmap -query` if `org.kde.keyboard` is inactive on single-layout setups.
pub fn spawn_kde_layout_monitor(router_tx: mpsc::UnboundedSender<RouterEvent>) {
    tokio::spawn(async move {
        let mut layouts: Vec<String> = Vec::new();
        let mut last_sent: Option<(bool, String)> = None;

        if let Some(raw_list) = gdbus_call_kde("org.kde.KeyboardLayouts.getLayoutsList").await {
            layouts = parse_kde_layouts_list(&raw_list);
        }
        if let Some(raw_idx) = gdbus_call_kde("org.kde.KeyboardLayouts.getLayout").await {
            if let Some(idx) = parse_kde_layout_index(&raw_idx) {
                emit_kde_state(&layouts, idx, &mut last_sent, &router_tx).await;
            }
        }

        // KDE X11 fallback: when only 1 layout is configured in System Settings, `kded` may not
        // publish `org.kde.keyboard`. Query `setxkbmap -query` so single-layout `vi_uinput` never
        // gets stuck in PassThrough.
        if last_sent.is_none() && crate::ipc::x11_xkb::is_pure_x11_session() {
            if let Some(x11_layouts) = crate::ipc::x11_xkb::query_x11_layouts().await {
                if x11_layouts.len() == 1 {
                    let only = x11_layouts[0].trim();
                    let active = only == "vi_uinput" || only.starts_with("vi_uinput+");
                    let (final_active, final_id) =
                        apply_fcitx5_guard(active, format!("x11:{}", only)).await;
                    if final_active {
                        crate::ipc::x11_xkb::ensure_x11_vi_uinput_keymap(
                            Some(&x11_layouts),
                            Some(0),
                        )
                        .await;
                    }
                    info!(
                        "KDE X11 fallback active layout -> '{}' (vi_uinput active={})",
                        final_id, final_active
                    );
                    last_sent = Some((final_active, final_id.clone()));
                    let _ = router_tx.send(RouterEvent::LayoutStateChanged {
                        active: final_active,
                        layout_id: final_id,
                    });
                }
            }
        }

        loop {
            let mut child = match Command::new("gdbus")
                .args([
                    "monitor",
                    "--session",
                    "--dest",
                    "org.kde.keyboard",
                    "--object-path",
                    "/Layouts",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
            {
                Ok(c) => c,
                Err(err) => {
                    warn!(
                        "Cannot spawn `gdbus monitor` for org.kde.keyboard: {}; retrying in 2s",
                        err
                    );
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            info!("Monitoring KDE keyboard layouts via D-Bus `org.kde.keyboard /Layouts`");
            if let Some(stdout) = child.stdout.take() {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let trimmed = line.trim();
                    debug!("KDE D-Bus event: {}", trimmed);
                    if trimmed.contains("layoutListChanged") {
                        if let Some(raw_list) =
                            gdbus_call_kde("org.kde.KeyboardLayouts.getLayoutsList").await
                        {
                            layouts = parse_kde_layouts_list(&raw_list);
                        }
                        if let Some(raw_idx) =
                            gdbus_call_kde("org.kde.KeyboardLayouts.getLayout").await
                        {
                            if let Some(idx) = parse_kde_layout_index(&raw_idx) {
                                emit_kde_state(&layouts, idx, &mut last_sent, &router_tx).await;
                            }
                        }
                    } else if trimmed.contains("layoutChanged") {
                        if layouts.is_empty() {
                            if let Some(raw_list) =
                                gdbus_call_kde("org.kde.KeyboardLayouts.getLayoutsList").await
                            {
                                layouts = parse_kde_layouts_list(&raw_list);
                            }
                        }
                        let idx = parse_kde_layout_index(trimmed).or({
                            None
                        });
                        let resolved_idx = match idx {
                            Some(i) => Some(i),
                            None => gdbus_call_kde("org.kde.KeyboardLayouts.getLayout")
                                .await
                                .and_then(|s| parse_kde_layout_index(&s)),
                        };
                        if let Some(i) = resolved_idx {
                            emit_kde_state(&layouts, i, &mut last_sent, &router_tx).await;
                        }
                    }
                }
            }

            let _ = child.wait().await;
            warn!("KDE `gdbus monitor` exited; restarting in 2s...");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kde_layouts_list_and_active_index() {
        let raw_single =
            "([('us', '', 'English (US)'), ('vi_uinput', '', 'Vietnamese (vi-uinput-ime)')],)";
        let layouts = parse_kde_layouts_list(raw_single);
        assert_eq!(layouts, vec!["us", "vi_uinput"]);

        assert_eq!(parse_kde_layout_index("(uint32 0,)"), Some(0));
        assert_eq!(parse_kde_layout_index("(uint32 1,)"), Some(1));
        assert_eq!(
            parse_kde_layout_index("/Layouts: org.kde.KeyboardLayouts.layoutChanged (uint32 1,)"),
            Some(1)
        );

        assert_eq!(
            resolve_kde_layout_state(&layouts, 0),
            Some((false, "kde:us".to_string()))
        );
        assert_eq!(
            resolve_kde_layout_state(&layouts, 1),
            Some((true, "kde:vi_uinput".to_string()))
        );

        // Double-quoted format with vi_uinput at index 0
        let raw_double = "([(\"vi_uinput\", \"\", \"Vietnamese\"), (\"us\", \"\", \"US\")],)";
        let layouts2 = parse_kde_layouts_list(raw_double);
        assert_eq!(layouts2, vec!["vi_uinput", "us"]);
        assert_eq!(
            resolve_kde_layout_state(&layouts2, 0),
            Some((true, "kde:vi_uinput".to_string()))
        );
    }
}
