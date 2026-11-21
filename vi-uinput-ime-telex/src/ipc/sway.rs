use crate::input::grabber::RouterEvent;
use crate::ipc::WindowMonitor;
use serde::Deserialize;
use std::env;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, warn};

const I3_IPC_MAGIC: &[u8; 6] = b"i3-ipc";
const IPC_SUBSCRIBE: u32 = 2;
const IPC_EVENT_WORKSPACE: u32 = 0x8000_0000;
const IPC_EVENT_WINDOW: u32 = 0x8000_0003;

pub struct SwayMonitor;

#[derive(Deserialize)]
struct SwayWindowEvent {
    change: String,
    container: Option<SwayContainer>,
}

#[derive(Deserialize)]
struct SwayContainer {
    app_id: Option<String>,
    shell: Option<String>,
    window_properties: Option<SwayWindowProps>,
}

#[derive(Deserialize)]
struct SwayWindowProps {
    class: Option<String>,
}

async fn write_ipc_frame(stream: &mut UnixStream, msg_type: u32, payload: &[u8]) -> std::io::Result<()> {
    let mut header = [0u8; 14];
    header[0..6].copy_from_slice(I3_IPC_MAGIC);
    header[6..10].copy_from_slice(&(payload.len() as u32).to_ne_bytes());
    header[10..14].copy_from_slice(&msg_type.to_ne_bytes());
    stream.write_all(&header).await?;
    stream.write_all(payload).await?;
    Ok(())
}

async fn read_ipc_frame(stream: &mut UnixStream) -> std::io::Result<(u32, Vec<u8>)> {
    let mut header = [0u8; 14];
    stream.read_exact(&mut header).await?;
    let len = u32::from_ne_bytes(header[6..10].try_into().unwrap()) as usize;
    let msg_type = u32::from_ne_bytes(header[10..14].try_into().unwrap());
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok((msg_type, payload))
}

impl WindowMonitor for SwayMonitor {
    fn name(&self) -> &'static str {
        "Sway / i3 Native IPC Socket"
    }

    fn spawn(self: Box<Self>, router_tx: mpsc::UnboundedSender<RouterEvent>) {
        tokio::spawn(async move {
            let sock_path = match env::var("SWAYSOCK").or_else(|_| env::var("I3SOCK")) {
                Ok(p) => p,
                Err(_) => return,
            };

            loop {
                match UnixStream::connect(&sock_path).await {
                    Ok(mut stream) => {
                        if let Err(err) =
                            write_ipc_frame(&mut stream, IPC_SUBSCRIBE, br#"["window","workspace"]"#)
                                .await
                        {
                            warn!("Sway IPC subscribe failed: {}", err);
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            continue;
                        }
                        // Consume subscribe acknowledgement reply
                        let _ = read_ipc_frame(&mut stream).await;

                        while let Ok((msg_type, payload)) = read_ipc_frame(&mut stream).await {
                            if msg_type == IPC_EVENT_WORKSPACE {
                                if let Ok(evt) = serde_json::from_slice::<SwayWindowEvent>(&payload)
                                {
                                    if evt.change == "focus" {
                                        debug!("Sway workspace::focus changed");
                                        let _ = router_tx.send(RouterEvent::WorkspaceChanged);
                                    }
                                }
                                continue;
                            }
                            if msg_type != IPC_EVENT_WINDOW {
                                continue;
                            }
                            if let Ok(evt) = serde_json::from_slice::<SwayWindowEvent>(&payload) {
                                if evt.change == "focus" {
                                    if let Some(c) = evt.container {
                                        let is_xwayland =
                                            c.shell.as_deref() == Some("xwayland")
                                                || c.window_properties.is_some();
                                        let app_class = c
                                            .app_id
                                            .filter(|s| !s.is_empty())
                                            .or_else(|| c.window_properties.and_then(|p| p.class))
                                            .unwrap_or_default();

                                        debug!(
                                            "Sway window::focus changed: class='{}' xwayland={}",
                                            app_class, is_xwayland
                                        );
                                        let _ = router_tx.send(RouterEvent::WindowFocusChanged {
                                            app_class,
                                            is_xwayland,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    Err(err) => {
                        warn!("Sway IPC connect failed ({}): {}", sock_path, err);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
    }
}
