use crate::input::grabber::RouterEvent;
use crate::ipc::WindowMonitor;
use serde::Deserialize;
use std::env;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub struct HyprlandMonitor;

#[derive(Deserialize)]
struct HyprActiveWindow {
    class: Option<String>,
    xwayland: Option<bool>,
}

fn hypr_socket_path(filename: &str) -> Option<PathBuf> {
    let sig = env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let runtime = env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let primary = PathBuf::from(&runtime).join("hypr").join(&sig).join(filename);
    if primary.exists() {
        return Some(primary);
    }
    Some(PathBuf::from("/tmp/hypr").join(sig).join(filename))
}

/// Query Hyprland `.socket.sock` with `j/activewindow` to detect `xwayland: true/false`.
async fn query_active_window_xwayland() -> Option<HyprActiveWindow> {
    let path = hypr_socket_path(".socket.sock")?;
    let mut stream = UnixStream::connect(path).await.ok()?;
    stream.write_all(b"j/activewindow").await.ok()?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf).await.ok()?;
    serde_json::from_str(&buf).ok()
}

impl WindowMonitor for HyprlandMonitor {
    fn name(&self) -> &'static str {
        "Hyprland UNIX Socket2"
    }

    fn spawn(self: Box<Self>, router_tx: mpsc::UnboundedSender<RouterEvent>) {
        tokio::spawn(async move {
            let Some(sock2_path) = hypr_socket_path(".socket2.sock") else {
                warn!("Hyprland socket2 path could not be resolved");
                return;
            };

            loop {
                match UnixStream::connect(&sock2_path).await {
                    Ok(stream) => {
                        let mut reader = BufReader::new(stream).lines();
                        while let Ok(Some(line)) = reader.next_line().await {
                            // Hyprland event format: `activewindow>>WINDOWCLASS,WINDOWTITLE`
                            if let Some(payload) = line.strip_prefix("activewindow>>") {
                                let class_raw = payload.split(',').next().unwrap_or("").trim();
                                let detail = query_active_window_xwayland().await;
                                let app_class = detail
                                    .as_ref()
                                    .and_then(|d| d.class.clone())
                                    .filter(|c| !c.is_empty())
                                    .unwrap_or_else(|| class_raw.to_string());
                                let is_xwayland = detail
                                    .as_ref()
                                    .and_then(|d| d.xwayland)
                                    .unwrap_or(false);

                                debug!(
                                    "Hyprland activewindow changed: class='{}' xwayland={}",
                                    app_class, is_xwayland
                                );
                                let _ = router_tx.send(RouterEvent::WindowFocusChanged {
                                    app_class,
                                    is_xwayland,
                                });
                            } else if line.starts_with("workspace>>")
                                || line.starts_with("workspacev2>>")
                                || line.starts_with("focusedmon>>")
                            {
                                debug!("Hyprland workspace/monitor changed: {}", line);
                                let _ = router_tx.send(RouterEvent::WorkspaceChanged);
                            }
                        }
                    }
                    Err(err) => {
                        warn!("Hyprland socket2 connect failed ({}): {}", sock2_path.display(), err);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
    }
}
