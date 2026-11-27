use crate::input::grabber::RouterEvent;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, info};

/// Parse `gdbus call ... org.fcitx.Fcitx5.CurrentInputMethod` output, e.g. `('keyboard-vi_uinput',)`
/// or `("unikey",)`.
pub fn parse_fcitx5_current_im(raw: &str) -> Option<String> {
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\'' || c == '"' {
            let quote = c;
            let mut val = String::new();
            for ch in chars.by_ref() {
                if ch == quote {
                    return Some(val);
                }
                val.push(ch);
            }
        }
    }
    None
}

/// Returns `true` ONLY when Fcitx5's active input method is the plain XKB `keyboard-vi_uinput`
/// layout (pauses `vi-uinput-ime` whenever `keyboard-us`, `unikey`, `bamboo`, `mozc`, `rime`, etc.
/// is active inside Fcitx5).
pub fn is_fcitx5_allowing_vi_uinput(current_im: &str) -> bool {
    let im = current_im.trim();
    im == "keyboard-vi_uinput" || im.starts_with("keyboard-vi_uinput-")
}

/// Query the currently active Fcitx5 input method if Fcitx5 is running on the session D-Bus.
/// Returns `None` silently when Fcitx5 is not installed or not running.
pub async fn query_fcitx5_im() -> Option<String> {
    let output = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.fcitx.Fcitx5",
            "--object-path",
            "/controller",
            "--method",
            "org.fcitx.Fcitx.Controller1.CurrentInputMethod",
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
    parse_fcitx5_current_im(&String::from_utf8_lossy(&output.stdout))
}

/// Passive Fcitx5 coexistence guard: if Fcitx5 is running on the user's session D-Bus,
/// monitors `org.fcitx.Fcitx5` signals (e.g. StatusNotifierItem updates) with a lightweight
/// fallback check and immediately emits `LayoutStateChanged` whenever the user switches between
/// `keyboard-vi_uinput` and any other Fcitx5 engine (`keyboard-us`, `bamboo`, `unikey`, etc.).
/// If Fcitx5 is not running, this task exits immediately without interfering or polling.
pub fn spawn_fcitx5_guard_if_running(router_tx: mpsc::UnboundedSender<RouterEvent>) {
    tokio::spawn(async move {
        let Some(initial_im) = query_fcitx5_im().await else {
            debug!("Fcitx5 not detected on session D-Bus; skipping Fcitx5 guard");
            return;
        };

        let mut last_im = initial_im.clone();
        let initial_active = is_fcitx5_allowing_vi_uinput(&initial_im);
        info!(
            "Detected running Fcitx5 (current IM='{}', vi_uinput allowed={})",
            initial_im, initial_active
        );
        let _ = router_tx.send(RouterEvent::LayoutStateChanged {
            active: initial_active,
            layout_id: format!("fcitx5:{}", initial_im),
        });

        loop {
            let mut child = match Command::new("gdbus")
                .args(["monitor", "--session", "--dest", "org.fcitx.Fcitx5"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
            {
                Ok(c) => c,
                Err(_) => return,
            };

            let mut fallback_tick = tokio::time::interval(Duration::from_millis(500));
            fallback_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            if let Some(stdout) = child.stdout.take() {
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    tokio::select! {
                        line_res = lines.next_line() => {
                            match line_res {
                                Ok(Some(line)) => {
                                    debug!("Fcitx5 D-Bus event: {}", line.trim());
                                }
                                _ => break,
                            }
                        }
                        _ = fallback_tick.tick() => {}
                    }

                    match query_fcitx5_im().await {
                        Some(im) => {
                            if im != last_im {
                                let active = is_fcitx5_allowing_vi_uinput(&im);
                                info!(
                                    "Fcitx5 switched IM -> '{}' (vi_uinput active={})",
                                    im, active
                                );
                                last_im = im.clone();
                                let _ = router_tx.send(RouterEvent::LayoutStateChanged {
                                    active,
                                    layout_id: format!("fcitx5:{}", im),
                                });
                            }
                        }
                        None => {
                            debug!("Fcitx5 exited from session D-Bus; stopping Fcitx5 guard");
                            return;
                        }
                    }
                }
            }

            let _ = child.wait().await;
            tokio::time::sleep(Duration::from_secs(2)).await;
            if query_fcitx5_im().await.is_none() {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fcitx5_im_and_allows_only_keyboard_vi_uinput() {
        assert_eq!(
            parse_fcitx5_current_im("('keyboard-vi_uinput',)\n"),
            Some("keyboard-vi_uinput".to_string())
        );
        assert_eq!(
            parse_fcitx5_current_im("(\"bamboo\",)"),
            Some("bamboo".to_string())
        );

        assert!(is_fcitx5_allowing_vi_uinput("keyboard-vi_uinput"));
        assert!(is_fcitx5_allowing_vi_uinput("keyboard-vi_uinput-basic"));
        assert!(!is_fcitx5_allowing_vi_uinput("keyboard-us"));
        assert!(!is_fcitx5_allowing_vi_uinput("bamboo"));
        assert!(!is_fcitx5_allowing_vi_uinput("unikey"));
    }
}
