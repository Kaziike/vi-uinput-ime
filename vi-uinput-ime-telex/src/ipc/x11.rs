use crate::input::grabber::RouterEvent;
use crate::ipc::WindowMonitor;
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub struct X11Monitor;

impl WindowMonitor for X11Monitor {
    fn name(&self) -> &'static str {
        "X11 Root PropertyNotify (_NET_ACTIVE_WINDOW)"
    }

    fn spawn(self: Box<Self>, router_tx: mpsc::UnboundedSender<RouterEvent>) {
        #[cfg(feature = "x11")]
        {
            std::thread::spawn(move || {
                if let Err(err) = run_x11_loop(router_tx) {
                    warn!("X11 window monitor stopped: {:#}", err);
                }
            });
        }

        #[cfg(not(feature = "x11"))]
        {
            let _ = router_tx;
            warn!("vi-uinput-ime built without `x11` feature; X11 window monitor disabled.");
        }
    }
}

#[cfg(feature = "x11")]
fn run_x11_loop(router_tx: mpsc::UnboundedSender<RouterEvent>) -> anyhow::Result<()> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;
    use x11rb::protocol::Event;

    let (conn, screen_num) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen_num].root;

    let net_active = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW")?.reply()?.atom;
    let wm_class: Atom = AtomEnum::WM_CLASS.into();

    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    conn.flush()?;

    loop {
        let event = conn.wait_for_event()?;
        if let Event::PropertyNotify(prop) = event {
            if prop.atom != net_active {
                continue;
            }
            let reply = conn
                .get_property(false, root, net_active, AtomEnum::WINDOW, 0, 1)?
                .reply()?;
            let Some(win_id) = reply.value32().and_then(|mut it| it.next()) else {
                continue;
            };
            if win_id == 0 {
                continue;
            }

            let class_reply = conn
                .get_property(false, win_id, wm_class, AtomEnum::STRING, 0, 64)?
                .reply()?;
            // WM_CLASS contains `instance\0Class\0`; extract second token or fallback to first
            let raw = String::from_utf8_lossy(&class_reply.value);
            let parts: Vec<&str> = raw.split('\0').filter(|s| !s.is_empty()).collect();
            let app_class = parts
                .get(1)
                .or_else(|| parts.first())
                .copied()
                .unwrap_or("")
                .to_string();

            debug!("X11 _NET_ACTIVE_WINDOW changed: class='{}'", app_class);
            let _ = router_tx.send(RouterEvent::WindowFocusChanged {
                app_class,
                is_xwayland: true,
            });
        }
    }
}
