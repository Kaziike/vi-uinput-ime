use crate::input::grabber::RouterEvent;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Newline-delimited JSON protocol sent from the user-session `--ipc-bridge` client
/// to the system daemon's UNIX domain socket (`/run/vi-uinput-ime-telex/ipc.sock`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IpcBridgeMessage {
    WindowFocusChanged { app_class: String, is_xwayland: bool },
    WorkspaceChanged,
    LayoutStateChanged { active: bool, layout_id: String },
}

impl IpcBridgeMessage {
    pub fn from_router_event(ev: &RouterEvent) -> Option<Self> {
        match ev {
            RouterEvent::WindowFocusChanged {
                app_class,
                is_xwayland,
            } => Some(Self::WindowFocusChanged {
                app_class: app_class.clone(),
                is_xwayland: *is_xwayland,
            }),
            RouterEvent::WorkspaceChanged => Some(Self::WorkspaceChanged),
            RouterEvent::LayoutStateChanged { active, layout_id } => {
                Some(Self::LayoutStateChanged {
                    active: *active,
                    layout_id: layout_id.clone(),
                })
            }
            _ => None,
        }
    }

    pub fn into_router_event(self) -> RouterEvent {
        match self {
            Self::WindowFocusChanged {
                app_class,
                is_xwayland,
            } => RouterEvent::WindowFocusChanged {
                app_class,
                is_xwayland,
            },
            Self::WorkspaceChanged => RouterEvent::WorkspaceChanged,
            Self::LayoutStateChanged { active, layout_id } => {
                RouterEvent::LayoutStateChanged { active, layout_id }
            }
        }
    }
}

/// Start the daemon-side UNIX domain socket server that receives compositor window/workspace
/// and GNOME/KDE desktop layout JSON events from a desktop user-session `--ipc-bridge` process.
pub fn spawn_ipc_server(
    socket_path: &Path,
    router_tx: mpsc::UnboundedSender<RouterEvent>,
) -> Result<tokio::task::JoinHandle<()>> {
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create IPC socket directory {}", parent.display())
        })?;
    }

    if socket_path.exists() {
        let _ = fs::remove_file(socket_path);
    }

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind IPC UNIX socket {}", socket_path.display()))?;

    // Allow desktop user-session `--ipc-bridge` processes to connect to the `vi-ime` daemon socket.
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o666)).with_context(|| {
        format!(
            "failed to chmod 0666 on IPC UNIX socket {}",
            socket_path.display()
        )
    })?;

    info!(
        "Listening for user-session compositor IPC bridge on {}",
        socket_path.display()
    );

    let active_clients = Arc::new(AtomicUsize::new(0));

    let handle = tokio::spawn(async move {
        let mut client_tasks = tokio::task::JoinSet::new();
        loop {
            while client_tasks.try_join_next().is_some() {}
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let tx = router_tx.clone();
                    let counter = Arc::clone(&active_clients);
                    counter.fetch_add(1, Ordering::SeqCst);

                    client_tasks.spawn(async move {
                        debug!("Accepted IPC bridge client connection");
                        let reader = BufReader::new(stream);
                        let mut lines = reader.lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            match serde_json::from_str::<IpcBridgeMessage>(trimmed) {
                                Ok(msg) => {
                                    let _ = tx.send(msg.into_router_event());
                                }
                                Err(err) => {
                                    warn!(
                                        "Ignoring malformed IPC bridge JSON '{}': {}",
                                        trimmed, err,
                                    );
                                }
                            }
                        }
                        debug!("IPC bridge client disconnected");
                        if counter.fetch_sub(1, Ordering::SeqCst) == 1 {
                            let _ = tx.send(RouterEvent::IpcBridgeDisconnected);
                        }
                    });
                }
                Err(err) => {
                    warn!("IPC UNIX socket accept error: {}", err);
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });

    Ok(handle)
}

async fn connect_and_resync(
    socket_path: &Path,
    last_layout_payload: Option<&str>,
) -> Option<(OwnedReadHalf, OwnedWriteHalf)> {
    match UnixStream::connect(socket_path).await {
        Ok(stream) => {
            let (reader, mut writer) = stream.into_split();
            info!(
                "IPC bridge connected to daemon socket {}",
                socket_path.display()
            );
            if let Some(layout_line) = last_layout_payload {
                if let Err(err) = writer.write_all(layout_line.as_bytes()).await {
                    warn!("IPC bridge failed to resync layout on connect: {}", err);
                    return None;
                }
            }
            Some((reader, writer))
        }
        Err(err) => {
            debug!(
                "IPC bridge cannot connect to {} ({})",
                socket_path.display(),
                err
            );
            None
        }
    }
}

/// Forward `RouterEvent`s from a local channel to the daemon's UNIX socket as newline-delimited
/// JSON, detecting daemon restarts (socket EOF) and automatically reconnecting + re-syncing the
/// latest desktop layout state every `reconnect_interval`.
pub async fn forward_events_to_socket(
    socket_path: PathBuf,
    mut rx: mpsc::UnboundedReceiver<RouterEvent>,
    reconnect_interval: Duration,
) {
    let mut conn: Option<(OwnedReadHalf, OwnedWriteHalf)> = None;
    let mut last_layout_payload: Option<String> = None;
    let mut pending_payload: Option<String> = None;
    let mut eof_buf = [0u8; 16];

    loop {
        // 1. If disconnected, keep trying to reconnect while still accepting newer layout updates.
        if conn.is_none() {
            if let Some(pair) =
                connect_and_resync(&socket_path, last_layout_payload.as_deref()).await
            {
                conn = Some(pair);
                if let Some(pending) = pending_payload.take() {
                    if last_layout_payload.as_deref() != Some(pending.as_str()) {
                        if let Some((_, ref mut writer)) = conn {
                            if writer.write_all(pending.as_bytes()).await.is_err() {
                                conn = None;
                                pending_payload = Some(pending);
                                continue;
                            }
                        }
                    }
                }
                continue;
            }

            tokio::select! {
                maybe_ev = rx.recv() => {
                    match maybe_ev {
                        Some(event) => {
                            if let Some(msg) = IpcBridgeMessage::from_router_event(&event) {
                                if let Ok(mut line) = serde_json::to_string(&msg) {
                                    line.push('\n');
                                    if matches!(msg, IpcBridgeMessage::LayoutStateChanged { .. }) {
                                        last_layout_payload = Some(line.clone());
                                    }
                                    pending_payload = Some(line);
                                }
                            }
                        }
                        None => break,
                    }
                }
                _ = tokio::time::sleep(reconnect_interval) => {}
            }
            continue;
        }

        // 2. While connected, simultaneously wait for outgoing events OR daemon disconnect (EOF).
        let (reader, writer) = conn.as_mut().unwrap();
        tokio::select! {
            maybe_ev = rx.recv() => {
                match maybe_ev {
                    Some(event) => {
                        let Some(msg) = IpcBridgeMessage::from_router_event(&event) else {
                            continue;
                        };
                        let Ok(mut line) = serde_json::to_string(&msg) else {
                            continue;
                        };
                        line.push('\n');
                        if matches!(msg, IpcBridgeMessage::LayoutStateChanged { .. }) {
                            last_layout_payload = Some(line.clone());
                        }
                        if let Err(err) = writer.write_all(line.as_bytes()).await {
                            warn!(
                                "IPC bridge write failed ({}); reconnecting in {:?}...",
                                err, reconnect_interval
                            );
                            pending_payload = Some(line);
                            conn = None;
                            tokio::time::sleep(reconnect_interval).await;
                        }
                    }
                    None => break,
                }
            }
            read_res = reader.read(&mut eof_buf) => {
                match read_res {
                    Ok(0) | Err(_) => {
                        warn!(
                            "Daemon IPC socket disconnected; reconnecting in {:?}...",
                            reconnect_interval
                        );
                        conn = None;
                        tokio::time::sleep(reconnect_interval).await;
                    }
                    Ok(_) => {}
                }
            }
        }
    }
}

/// Run the desktop user-session `--ipc-bridge` mode: auto-detect GNOME / KDE / Hyprland / Sway /
/// X11 / Fcitx5 from the user's real graphical environment variables and forward window/layout
/// events to `socket_path`.
pub async fn run_ipc_bridge(socket_path: PathBuf) -> Result<()> {
    let (tx, rx) = mpsc::unbounded_channel::<RouterEvent>();
    crate::ipc::spawn_active_window_monitor(tx);

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    info!(
        "Starting vi-uinput-ime-telex --ipc-bridge -> {}",
        socket_path.display()
    );

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("IPC bridge received SIGINT — exiting.");
        }
        _ = sigterm.recv() => {
            info!("IPC bridge received SIGTERM — exiting.");
        }
        _ = forward_events_to_socket(socket_path, rx, Duration::from_secs(2)) => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_sock_path(tag: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "vi-uinput-ime-telex-{}-{}-{}.sock",
            tag,
            std::process::id(),
            unique
        ))
    }

    #[tokio::test]
    async fn ipc_bridge_unix_socket_json_roundtrip() {
        let sock_path = unique_sock_path("ipc-roundtrip");

        let (daemon_tx, mut daemon_rx) = mpsc::unbounded_channel::<RouterEvent>();
        let server_task = spawn_ipc_server(&sock_path, daemon_tx).expect("spawn IPC server");

        let (bridge_tx, bridge_rx) = mpsc::unbounded_channel::<RouterEvent>();
        let client_task = tokio::spawn(forward_events_to_socket(
            sock_path.clone(),
            bridge_rx,
            Duration::from_millis(50),
        ));

        let expected = vec![
            RouterEvent::LayoutStateChanged {
                active: true,
                layout_id: "xkb:vi_uinput".to_string(),
            },
            RouterEvent::WindowFocusChanged {
                app_class: "chromium".to_string(),
                is_xwayland: false,
            },
            RouterEvent::WorkspaceChanged,
            RouterEvent::LayoutStateChanged {
                active: false,
                layout_id: "xkb:us".to_string(),
            },
            RouterEvent::WindowFocusChanged {
                app_class: "WeChat".to_string(),
                is_xwayland: true,
            },
        ];

        for ev in &expected {
            bridge_tx.send(ev.clone()).unwrap();
        }
        drop(bridge_tx);

        let mut received = Vec::new();
        for _ in 0..expected.len() {
            let ev = tokio::time::timeout(Duration::from_secs(3), daemon_rx.recv())
                .await
                .expect("timed out waiting for IPC bridge event")
                .expect("daemon channel closed");
            received.push(ev);
        }

        assert_eq!(received, expected);

        // Once the bridge closes cleanly, daemon receives IpcBridgeDisconnected
        let disc = tokio::time::timeout(Duration::from_secs(3), daemon_rx.recv())
            .await
            .expect("timed out waiting for disconnect event")
            .expect("daemon channel closed");
        assert_eq!(disc, RouterEvent::IpcBridgeDisconnected);

        client_task.await.unwrap();
        server_task.abort();
        let _ = fs::remove_file(&sock_path);
    }

    #[tokio::test]
    async fn ipc_bridge_resyncs_last_layout_after_daemon_restart() {
        let sock_path = unique_sock_path("ipc-resync");

        // Start daemon server #1
        let (daemon1_tx, mut daemon1_rx) = mpsc::unbounded_channel::<RouterEvent>();
        let server1 = spawn_ipc_server(&sock_path, daemon1_tx).expect("spawn server 1");

        let (bridge_tx, bridge_rx) = mpsc::unbounded_channel::<RouterEvent>();
        let client_task = tokio::spawn(forward_events_to_socket(
            sock_path.clone(),
            bridge_rx,
            Duration::from_millis(40),
        ));

        let active_layout = RouterEvent::LayoutStateChanged {
            active: true,
            layout_id: "xkb:vi_uinput".to_string(),
        };
        bridge_tx.send(active_layout.clone()).unwrap();

        let first = tokio::time::timeout(Duration::from_secs(3), daemon1_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, active_layout);

        // Simulate daemon restart: stop server #1 and bind server #2 on the same socket path
        server1.abort();
        let _ = fs::remove_file(&sock_path);
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (daemon2_tx, mut daemon2_rx) = mpsc::unbounded_channel::<RouterEvent>();
        let server2 = spawn_ipc_server(&sock_path, daemon2_tx).expect("spawn server 2");

        // Client detects socket EOF automatically, reconnects to server #2, and re-syncs
        // `active_layout` with zero new layout events required.
        let resynced = tokio::time::timeout(Duration::from_secs(3), daemon2_rx.recv())
            .await
            .expect("timed out waiting for automatic layout resync after daemon restart")
            .expect("daemon2 channel closed");
        assert_eq!(
            resynced, active_layout,
            "Bridge must automatically re-send last_layout_payload after daemon restart"
        );

        drop(bridge_tx);
        client_task.await.unwrap();
        server2.abort();
        let _ = fs::remove_file(&sock_path);
    }
}
