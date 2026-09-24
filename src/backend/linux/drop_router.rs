//! Event-driven XDND proxy routing for plug-in editors embedded in XWayland hosts.

use std::sync::{Mutex, MutexGuard, PoisonError};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt, CreateWindowAux,
    EventMask, PropMode, Window as XWindow, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use super::{X11SessionError, X11WaylandBridge, XDND_VERSION, atom, x11_error, x11_wayland_bridge};

/// Outbound drags started by this crate, each as its XDND source window.
///
/// `None` marks a native Wayland source: the compositor's XWM bridges that drag
/// back into X under a window of its own, so no source filter can recognise the
/// reflection and the router must stand down for the whole gesture.
static OUTBOUND: Mutex<Vec<Option<XWindow>>> = Mutex::new(Vec::new());

/// Registration of one live outbound drag, released when this guard drops.
pub(super) struct OutboundGuard(Option<XWindow>);

impl OutboundGuard {
    /// Register a live outbound drag, `None` for a native Wayland source.
    pub(super) fn register(source: Option<XWindow>) -> Self {
        live_outbound().push(source);
        Self(source)
    }
}

impl Drop for OutboundGuard {
    fn drop(&mut self) {
        let mut live = live_outbound();
        if let Some(index) = live.iter().position(|entry| *entry == self.0) {
            live.swap_remove(index);
        }
    }
}

fn live_outbound() -> MutexGuard<'static, Vec<Option<XWindow>>> {
    OUTBOUND.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether a live outbound drag of ours explains this XDND source window.
fn is_self_drag(live: &[Option<XWindow>], source: XWindow) -> bool {
    live.iter()
        .any(|entry| entry.is_none_or(|window| window == source))
}

struct RouterAtoms {
    xdnd_aware: Atom,
    xdnd_proxy: Atom,
    xdnd_enter: Atom,
    xdnd_position: Atom,
    xdnd_leave: Atom,
    xdnd_drop: Atom,
}

impl RouterAtoms {
    fn new<C: Connection>(conn: &C) -> Result<Self, X11SessionError> {
        Ok(Self {
            xdnd_aware: atom(conn, b"XdndAware")?,
            xdnd_proxy: atom(conn, b"XdndProxy")?,
            xdnd_enter: atom(conn, b"XdndEnter")?,
            xdnd_position: atom(conn, b"XdndPosition")?,
            xdnd_leave: atom(conn, b"XdndLeave")?,
            xdnd_drop: atom(conn, b"XdndDrop")?,
        })
    }
}

/// Routes compositor-bridged drops from an XWayland host to its embedded
/// `XdndAware` plug-in child on the toolkit's existing X11 event queue.
#[must_use = "the toolkit must drive and uninstall the router on its X11 event queue"]
pub struct X11DropRouter {
    root: XWindow,
    atoms: RouterAtoms,
    router_window: XWindow,
    toplevel: XWindow,
    observed_proxy: Option<XWindow>,
    active_enter: Option<[u32; 5]>,
    current_target: Option<XWindow>,
}

impl X11DropRouter {
    /// Install the router when the live XWayland bridge requires one.
    ///
    /// `Ok(None)` means this X11 session does not use Hyprland's native
    /// Wayland-to-XWayland bridge.
    pub fn install<C: Connection>(
        conn: &C,
        editor_window: XWindow,
    ) -> Result<Option<Self>, X11SessionError> {
        if x11_wayland_bridge() != X11WaylandBridge::HyprlandSeriallessCompat {
            return Ok(None);
        }

        let (root, toplevel) = root_and_toplevel(conn, editor_window)?;
        let screen = conn
            .setup()
            .roots
            .iter()
            .find(|screen| screen.root == root)
            .ok_or_else(|| X11SessionError::new("X11 drop router root has no matching screen"))?;
        let atoms = RouterAtoms::new(conn)?;
        let router_window = conn.generate_id().map_err(x11_error)?;
        conn.create_window(
            screen.root_depth,
            router_window,
            root,
            -100,
            -100,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new().override_redirect(1),
        )
        .map_err(x11_error)?;
        conn.change_property32(
            PropMode::REPLACE,
            router_window,
            atoms.xdnd_aware,
            AtomEnum::ATOM,
            &[XDND_VERSION],
        )
        .map_err(x11_error)?;
        conn.change_property32(
            PropMode::REPLACE,
            router_window,
            atoms.xdnd_proxy,
            AtomEnum::WINDOW,
            &[router_window],
        )
        .map_err(x11_error)?;
        select_additional_events(conn, toplevel, EventMask::PROPERTY_CHANGE)?;

        let mut router = Self {
            root,
            atoms,
            router_window,
            toplevel,
            observed_proxy: None,
            active_enter: None,
            current_target: None,
        };
        router.refresh_claim(conn)?;
        conn.flush().map_err(x11_error)?;
        Ok(Some(router))
    }

    /// Drive one X11 event. Returns `true` when the router consumed it.
    pub fn handle_event<C: Connection>(
        &mut self,
        conn: &C,
        event: &Event,
    ) -> Result<bool, X11SessionError> {
        match event {
            Event::PropertyNotify(event)
                if event.window == self.toplevel && event.atom == self.atoms.xdnd_proxy =>
            {
                self.refresh_claim(conn)?;
            }
            Event::DestroyNotify(event) if Some(event.window) == self.observed_proxy => {
                self.refresh_claim(conn)?;
            }
            Event::ClientMessage(event) if event.window == self.router_window => {
                self.handle_client_message(conn, event)?;
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }

    /// Remove this router without disturbing a proxy installed by another client.
    pub fn uninstall<C: Connection>(mut self, conn: &C) -> Result<(), X11SessionError> {
        if let Some(target) = self.current_target.take()
            && let Some(enter) = self.active_enter
        {
            let _ = self.forward(conn, target, self.atoms.xdnd_leave, [enter[0], 0, 0, 0, 0]);
        }
        if self.read_proxy(conn, self.toplevel).ok().flatten() == Some(self.router_window) {
            let _ = conn.delete_property(self.toplevel, self.atoms.xdnd_proxy);
        }
        conn.destroy_window(self.router_window).map_err(x11_error)?;
        conn.flush().map_err(x11_error)
    }

    fn handle_client_message<C: Connection>(
        &mut self,
        conn: &C,
        event: &ClientMessageEvent,
    ) -> Result<(), X11SessionError> {
        if event.format != 32 {
            return Ok(());
        }
        let data = event.data.as_data32();
        if event.type_ != self.atoms.xdnd_leave && is_self_drag(&live_outbound(), data[0]) {
            return self.abandon_route(conn);
        }
        if event.type_ == self.atoms.xdnd_enter {
            self.active_enter = Some(data);
            self.current_target = None;
        } else if event.type_ == self.atoms.xdnd_position {
            let packed = data[2];
            let target = self.resolve_target(conn, (packed >> 16) as i16, packed as i16)?;
            if self.current_target != Some(target) {
                self.switch_target(conn, target, data[0])?;
            }
            self.forward(conn, target, self.atoms.xdnd_position, data)?;
        } else if event.type_ == self.atoms.xdnd_leave {
            if let Some(target) = self.current_target.take() {
                self.forward(conn, target, self.atoms.xdnd_leave, data)?;
            }
            self.active_enter = None;
        } else if event.type_ == self.atoms.xdnd_drop {
            if let Some(target) = self.current_target {
                self.forward(conn, target, self.atoms.xdnd_drop, data)?;
            }
            self.active_enter = None;
            self.current_target = None;
        }
        Ok(())
    }

    /// Drop a bridged reflection of our own drag, leaving no phantom offer.
    fn abandon_route<C: Connection>(&mut self, conn: &C) -> Result<(), X11SessionError> {
        if let Some(target) = self.current_target.take()
            && let Some(enter) = self.active_enter
        {
            self.forward(conn, target, self.atoms.xdnd_leave, [enter[0], 0, 0, 0, 0])?;
        }
        self.active_enter = None;
        Ok(())
    }

    fn switch_target<C: Connection>(
        &mut self,
        conn: &C,
        target: XWindow,
        source: XWindow,
    ) -> Result<(), X11SessionError> {
        if let Some(previous) = self.current_target.take() {
            self.forward(conn, previous, self.atoms.xdnd_leave, [source, 0, 0, 0, 0])?;
        }
        if let Some(enter) = self.active_enter {
            self.forward(conn, target, self.atoms.xdnd_enter, enter)?;
        }
        self.current_target = Some(target);
        Ok(())
    }

    fn resolve_target<C: Connection>(
        &self,
        conn: &C,
        root_x: i16,
        root_y: i16,
    ) -> Result<XWindow, X11SessionError> {
        let mut window = self.root;
        loop {
            let reply = conn
                .translate_coordinates(self.root, window, root_x, root_y)
                .map_err(x11_error)?
                .reply()
                .map_err(x11_error)?;
            if reply.child == x11rb::NONE || reply.child == self.router_window {
                break;
            }
            window = reply.child;
        }

        let mut candidate = window;
        loop {
            if candidate != self.router_window && self.is_xdnd_aware(conn, candidate)? {
                return Ok(candidate);
            }
            if candidate == self.root {
                break;
            }
            let tree = conn
                .query_tree(candidate)
                .map_err(x11_error)?
                .reply()
                .map_err(x11_error)?;
            if tree.parent == x11rb::NONE {
                break;
            }
            candidate = tree.parent;
        }
        Ok(self.toplevel)
    }

    fn forward<C: Connection>(
        &self,
        conn: &C,
        target: XWindow,
        message_type: Atom,
        data: [u32; 5],
    ) -> Result<(), X11SessionError> {
        let event = ClientMessageEvent::new(32, target, message_type, data);
        conn.send_event(false, target, EventMask::NO_EVENT, event)
            .map_err(x11_error)?;
        conn.flush().map_err(x11_error)
    }

    fn is_xdnd_aware<C: Connection>(
        &self,
        conn: &C,
        window: XWindow,
    ) -> Result<bool, X11SessionError> {
        let property = conn
            .get_property(false, window, self.atoms.xdnd_aware, AtomEnum::ANY, 0, 1)
            .map_err(x11_error)?
            .reply()
            .map_err(x11_error)?;
        Ok(property.value_len > 0)
    }

    fn refresh_claim<C: Connection>(&mut self, conn: &C) -> Result<(), X11SessionError> {
        let proxy = self.read_proxy(conn, self.toplevel)?;
        if proxy == Some(self.router_window) {
            self.observed_proxy = None;
            return Ok(());
        }
        if let Some(proxy) = proxy
            && self.window_alive(conn, proxy)
            && self.read_proxy(conn, proxy)? == Some(proxy)
        {
            select_additional_events(conn, proxy, EventMask::STRUCTURE_NOTIFY)?;
            self.observed_proxy = Some(proxy);
            return Ok(());
        }

        conn.change_property32(
            PropMode::REPLACE,
            self.toplevel,
            self.atoms.xdnd_proxy,
            AtomEnum::WINDOW,
            &[self.router_window],
        )
        .map_err(x11_error)?;
        self.observed_proxy = None;
        conn.flush().map_err(x11_error)
    }

    fn read_proxy<C: Connection>(
        &self,
        conn: &C,
        window: XWindow,
    ) -> Result<Option<XWindow>, X11SessionError> {
        let property = conn
            .get_property(false, window, self.atoms.xdnd_proxy, AtomEnum::WINDOW, 0, 1)
            .map_err(x11_error)?
            .reply()
            .map_err(x11_error)?;
        Ok(property.value32().and_then(|mut values| values.next()))
    }

    fn window_alive<C: Connection>(&self, conn: &C, window: XWindow) -> bool {
        conn.get_geometry(window)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some()
    }
}

fn select_additional_events<C: Connection>(
    conn: &C,
    window: XWindow,
    events: EventMask,
) -> Result<(), X11SessionError> {
    let current = conn
        .get_window_attributes(window)
        .map_err(x11_error)?
        .reply()
        .map_err(x11_error)?
        .your_event_mask;
    conn.change_window_attributes(
        window,
        &ChangeWindowAttributesAux::new().event_mask(current | events),
    )
    .map_err(x11_error)?;
    Ok(())
}

fn root_and_toplevel<C: Connection>(
    conn: &C,
    editor_window: XWindow,
) -> Result<(XWindow, XWindow), X11SessionError> {
    let mut window = editor_window;
    for _ in 0..64 {
        let tree = conn
            .query_tree(window)
            .map_err(x11_error)?
            .reply()
            .map_err(x11_error)?;
        if tree.parent == tree.root || tree.parent == x11rb::NONE {
            return Ok((tree.root, window));
        }
        window = tree.parent;
    }
    Err(X11SessionError::new(
        "X11 drop router exceeded the host window ancestry limit",
    ))
}

#[cfg(test)]
mod tests {
    use super::{OutboundGuard, is_self_drag, live_outbound};

    #[test]
    fn foreign_sources_route_when_no_outbound_drag_is_live() {
        assert!(!is_self_drag(&[], 0x40));
    }

    #[test]
    fn a_live_xdnd_source_suppresses_only_its_own_window() {
        assert!(is_self_drag(&[Some(0x40)], 0x40));
        assert!(!is_self_drag(&[Some(0x40)], 0x41));
    }

    #[test]
    fn a_live_wayland_source_suppresses_every_bridged_source() {
        assert!(is_self_drag(&[None], 0x41));
    }

    #[test]
    fn each_guard_releases_exactly_one_registration() {
        let outer = OutboundGuard::register(Some(0x40));
        let inner = OutboundGuard::register(Some(0x40));
        assert!(is_self_drag(&live_outbound(), 0x40));
        drop(inner);
        assert!(is_self_drag(&live_outbound(), 0x40));
        drop(outer);
        assert!(!is_self_drag(&live_outbound(), 0x40));
    }
}
