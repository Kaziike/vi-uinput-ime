use crate::input::grabber::RouterEvent;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Parse `gsettings` GVariant output for `org.gnome.desktop.input-sources` (`mru-sources` or `sources`),
/// e.g. `[('xkb', 'vi_uinput'), ('xkb', 'us')]`, `mru-sources: [('ibus', 'Bamboo')]`, or `@a(ss) []`.
pub fn parse_gsettings_input_sources(raw: &str) -> Vec<(String, String)> {
    let mut results = Vec::new();
    let mut chars = raw.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '(' {
            continue;
        }
        let mut tuple_strings = Vec::with_capacity(2);
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
        if tuple_strings.len() >= 2 {
            results.push((tuple_strings[0].clone(), tuple_strings[1].clone()));
        }
    }

    results
}

/// Returns `true` ONLY when the active GNOME input source is the XKB `vi_uinput` layout
/// (never `xkb:us`, never `ibus:*`, etc.).
pub fn is_vi_uinput_source(source_type: &str, source_id: &str) -> bool {
    let ty = source_type.trim();
    let id = source_id.trim();
    ty == "xkb" && (id == "vi_uinput" || id.starts_with("vi_uinput+"))
}

/// Determine `(active, layout_id)` from `mru-sources` (preferred) with fallback to `sources[0]`
/// when `mru-sources` is empty on a fresh login.
pub fn resolve_gnome_layout_state(mru_raw: &str, sources_raw: &str) -> Option<(bool, String)> {
    let mru = parse_gsettings_input_sources(mru_raw);
    let primary = mru.first().cloned().or_else(|| {
        let sources = parse_gsettings_input_sources(sources_raw);
        sources.into_iter().next()
    })?;

    let active = is_vi_uinput_source(&primary.0, &primary.1);
    let layout_id = format!("{}:{}", primary.0, primary.1);
    Some((active, layout_id))
}

async fn gsettings_get(key: &str) -> Option<String> {
    let output = Command::new("gsettings")
        .args(["get", "org.gnome.desktop.input-sources", key])
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

async fn emit_current_gnome_state(
    mru_override: Option<&str>,
    sources_override: Option<&str>,
    last_sent: &mut Option<(bool, String)>,
    router_tx: &mpsc::UnboundedSender<RouterEvent>,
) {
    let mru_raw = match mru_override {
        Some(s) => s.to_string(),
        None => gsettings_get("mru-sources").await.unwrap_or_default(),
    };
    let sources_raw = match sources_override {
        Some(s) => s.to_string(),
        None => gsettings_get("sources").await.unwrap_or_default(),
    };

    if let Some((active, layout_id)) = resolve_gnome_layout_state(&mru_raw, &sources_raw) {
        let (final_active, final_id) = apply_fcitx5_guard(active, layout_id).await;
        let state = (final_active, final_id.clone());
        if last_sent.as_ref() != Some(&state) {
            info!(
                "GNOME active input source -> '{}' (vi_uinput active={})",
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

/// Spawn an event-driven GNOME / Mutter input-source monitor using `gsettings monitor`.
/// Emits `RouterEvent::LayoutStateChanged` immediately on startup and whenever the user switches
/// between `vi_uinput`, `us`, `ibus:*`, or any other input source.
pub fn spawn_gnome_layout_monitor(router_tx: mpsc::UnboundedSender<RouterEvent>) {
    tokio::spawn(async move {
        let mut last_sent: Option<(bool, String)> = None;
        emit_current_gnome_state(None, None, &mut last_sent, &router_tx).await;

        loop {
            let mut child = match Command::new("gsettings")
                .args(["monitor", "org.gnome.desktop.input-sources"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
            {
                Ok(c) => c,
                Err(err) => {
                    warn!(
                        "Cannot spawn `gsettings monitor org.gnome.desktop.input-sources`: {}; retrying in 2s",
                        err
                    );
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            info!("Monitoring GNOME input sources via `gsettings monitor org.gnome.desktop.input-sources`");
            if let Some(stdout) = child.stdout.take() {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let trimmed = line.trim();
                    debug!("gsettings monitor event: {}", trimmed);
                    if let Some(rest) = trimmed.strip_prefix("mru-sources:") {
                        emit_current_gnome_state(Some(rest), None, &mut last_sent, &router_tx)
                            .await;
                    } else if let Some(rest) = trimmed.strip_prefix("sources:") {
                        emit_current_gnome_state(None, Some(rest), &mut last_sent, &router_tx)
                            .await;
                    }
                }
            }

            let _ = child.wait().await;
            warn!("`gsettings monitor` exited; restarting in 2s...");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gnome_mru_sources_and_ibus_entries_accurately() {
        assert_eq!(
            resolve_gnome_layout_state(
                "[('xkb', 'vi_uinput'), ('xkb', 'us')]",
                "[('xkb', 'us'), ('xkb', 'vi_uinput')]",
            ),
            Some((true, "xkb:vi_uinput".to_string()))
        );

        assert_eq!(
            resolve_gnome_layout_state(
                "[('xkb', 'us'), ('xkb', 'vi_uinput')]",
                "[('xkb', 'us'), ('xkb', 'vi_uinput')]",
            ),
            Some((false, "xkb:us".to_string()))
        );

        assert_eq!(
            resolve_gnome_layout_state(
                "[('ibus', 'Bamboo'), ('xkb', 'vi_uinput')]",
                "[('xkb', 'vi_uinput'), ('ibus', 'Bamboo')]",
            ),
            Some((false, "ibus:Bamboo".to_string()))
        );

        // Fresh login where mru-sources is empty -> falls back to sources[0]
        assert_eq!(
            resolve_gnome_layout_state("@a(ss) []", "[('xkb', 'vi_uinput'), ('xkb', 'us')]"),
            Some((true, "xkb:vi_uinput".to_string()))
        );
        assert_eq!(
            resolve_gnome_layout_state("@a(ss) []", "[('xkb', 'us'), ('xkb', 'vi_uinput')]"),
            Some((false, "xkb:us".to_string()))
        );

        // Variant suffix on vi_uinput still matches
        assert_eq!(
            resolve_gnome_layout_state("mru-sources: [('xkb', 'vi_uinput+basic')]", ""),
            Some((true, "xkb:vi_uinput+basic".to_string()))
        );
    }
}
