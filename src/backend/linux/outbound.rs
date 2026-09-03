//! Process-wide record of the live outbound drag.
//!
//! The XDND drop router installs itself as the `XdndProxy` of the host
//! toplevel so compositor-bridged drops reach the embedded editor. On Hyprland
//! the same XWM that bridges foreign drops into X also bridges *our own*
//! outbound native Wayland drag back into X. Without this record the router
//! accepts that reflected drag, the editor sees an inbound offer for the file
//! it is currently exporting, and rejecting it cancels the outbound session.
//!
//! Registering the live outbound session here lets the router ignore the
//! reflection: entirely for the duration of a native Wayland session, and by
//! source window for an XDND session that names our own drag source.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use x11rb::protocol::xproto::Window as XWindow;

use crate::NativeProtocol;

/// Identity of the outbound drag this process currently owns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OutboundDrag {
    protocol: NativeProtocol,
    source_window: XWindow,
}

impl OutboundDrag {
    pub(super) const fn new(protocol: NativeProtocol, source_window: XWindow) -> Self {
        Self {
            protocol,
            source_window,
        }
    }
}

/// What the drop router should do with one bridged XDND client message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RouterDecision {
    /// No outbound drag of ours explains this message; route it.
    Route,
    /// A native Wayland drag of ours is live, so every bridged message is ours.
    SuppressedOutboundWayland,
    /// The message names our own drag source window.
    SuppressedSelfSource,
}

impl RouterDecision {
    pub(super) const fn is_suppressed(self) -> bool {
        !matches!(self, Self::Route)
    }
}

/// Decide whether the router may act on a bridged `XdndEnter`, `XdndPosition`
/// or `XdndDrop` whose `data[0]` names `source`.
pub(super) fn evaluate_router_message(
    outbound: Option<OutboundDrag>,
    source: XWindow,
) -> RouterDecision {
    let Some(outbound) = outbound else {
        return RouterDecision::Route;
    };
    if outbound.protocol == NativeProtocol::WaylandDataDevice {
        // The bridged source window belongs to the compositor's XWM, not to
        // us, so the source filter cannot see this case. A live outbound
        // Wayland session is itself the proof that the drag is ours.
        return RouterDecision::SuppressedOutboundWayland;
    }
    if source != x11rb::NONE && source == outbound.source_window {
        return RouterDecision::SuppressedSelfSource;
    }
    RouterDecision::Route
}

static LIVE: Mutex<Option<(u64, OutboundDrag)>> = Mutex::new(None);
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

fn live() -> std::sync::MutexGuard<'static, Option<(u64, OutboundDrag)>> {
    LIVE.lock().unwrap_or_else(|error| error.into_inner())
}

/// Outbound drag currently owned by this process, if any.
pub(super) fn live_outbound_drag() -> Option<OutboundDrag> {
    live().map(|(_, drag)| drag)
}

/// Registration handle for one outbound session.
///
/// The registration is released when the session reports a terminal event and,
/// as a backstop, when the session handle is dropped. Tokens keep a late drop
/// from clearing a newer session's registration.
#[derive(Debug)]
pub(super) struct OutboundGuard {
    token: Option<u64>,
}

impl OutboundGuard {
    pub(super) fn acquire(drag: OutboundDrag) -> Self {
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        *live() = Some((token, drag));
        Self { token: Some(token) }
    }

    pub(super) fn release(&mut self) {
        let Some(token) = self.token.take() else {
            return;
        };
        let mut live = live();
        if live.is_some_and(|(current, _)| current == token) {
            *live = None;
        }
    }
}

impl Drop for OutboundGuard {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        OutboundDrag, OutboundGuard, RouterDecision, evaluate_router_message, live_outbound_drag,
    };
    use crate::NativeProtocol;

    const EDITOR: u32 = 0x0140_0004;
    const XWM_BRIDGE_SOURCE: u32 = 0x0080_0002;
    const FOREIGN_SOURCE: u32 = 0x0220_0007;

    /// The registry is process-wide, so registry tests take this lock.
    static REGISTRY: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn registry_lock() -> std::sync::MutexGuard<'static, ()> {
        REGISTRY.lock().unwrap_or_else(|error| error.into_inner())
    }

    #[test]
    fn routes_foreign_drops_when_no_outbound_drag_is_live() {
        assert_eq!(
            evaluate_router_message(None, FOREIGN_SOURCE),
            RouterDecision::Route
        );
    }

    #[test]
    fn suppresses_every_bridged_message_during_an_outbound_wayland_drag() {
        let outbound = Some(OutboundDrag::new(NativeProtocol::WaylandDataDevice, EDITOR));

        // Hyprland's XWM reflects our Wayland drag under its own source
        // window, which is why the source filter alone is not enough.
        assert_eq!(
            evaluate_router_message(outbound, XWM_BRIDGE_SOURCE),
            RouterDecision::SuppressedOutboundWayland
        );
        assert_eq!(
            evaluate_router_message(outbound, x11rb::NONE),
            RouterDecision::SuppressedOutboundWayland
        );
    }

    #[test]
    fn suppresses_an_xdnd_message_that_names_our_own_drag_source() {
        let outbound = Some(OutboundDrag::new(NativeProtocol::Xdnd, EDITOR));

        assert_eq!(
            evaluate_router_message(outbound, EDITOR),
            RouterDecision::SuppressedSelfSource
        );
        assert_eq!(
            evaluate_router_message(outbound, FOREIGN_SOURCE),
            RouterDecision::Route
        );
        assert_eq!(
            evaluate_router_message(outbound, x11rb::NONE),
            RouterDecision::Route
        );
    }

    #[test]
    fn suppression_flags_agree_with_the_decision() {
        assert!(!RouterDecision::Route.is_suppressed());
        assert!(RouterDecision::SuppressedOutboundWayland.is_suppressed());
        assert!(RouterDecision::SuppressedSelfSource.is_suppressed());
    }

    #[test]
    fn registry_reports_the_live_session_until_it_is_released() {
        let _lock = registry_lock();
        assert_eq!(live_outbound_drag(), None);

        let mut guard =
            OutboundGuard::acquire(OutboundDrag::new(NativeProtocol::WaylandDataDevice, EDITOR));
        assert_eq!(
            live_outbound_drag(),
            Some(OutboundDrag::new(NativeProtocol::WaylandDataDevice, EDITOR))
        );

        guard.release();
        assert_eq!(live_outbound_drag(), None);
        // Releasing twice, and the drop backstop, must both stay quiet.
        guard.release();
        drop(guard);
        assert_eq!(live_outbound_drag(), None);
    }

    #[test]
    fn a_late_drop_does_not_clear_a_newer_session() {
        let _lock = registry_lock();
        let stale = OutboundGuard::acquire(OutboundDrag::new(NativeProtocol::Xdnd, EDITOR));
        let current = OutboundGuard::acquire(OutboundDrag::new(
            NativeProtocol::WaylandDataDevice,
            FOREIGN_SOURCE,
        ));

        drop(stale);

        assert_eq!(
            live_outbound_drag(),
            Some(OutboundDrag::new(
                NativeProtocol::WaylandDataDevice,
                FOREIGN_SOURCE
            ))
        );
        drop(current);
        assert_eq!(live_outbound_drag(), None);
    }
}
