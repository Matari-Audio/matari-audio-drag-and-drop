//! One-struct X11/XWayland integration for a toolkit that owns an X11 queue.

use std::error::Error;
use std::fmt;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{KeyButMask, Window as XWindow};

use crate::{
    DragOrigin, RejectedStart, StartTicket, X11DropRouter, X11PointerEvent, X11Session,
    X11SessionError, X11SessionStatus,
};

/// Why this host refused to start an outbound drag.
///
/// A refusal means the [`StartTicket`] was never consumed: it is returned
/// inside the [`RejectedStart`] so the controller can report the failure
/// against the session the caller intended to start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum X11HostError {
    /// A previous outbound drag from this editor has not finished transferring.
    Busy,
    /// No live primary-button press backs this start.
    ///
    /// XDND and the Hyprland compatibility route both need the timestamp and
    /// root position of a real pointer event that the user is still holding.
    /// Starting a drag from a keyboard shortcut, a timer, or a released button
    /// cannot produce one.
    PointerAuthorityUnavailable,
}

impl fmt::Display for X11HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => formatter.write_str("a previous outbound drag is still transferring"),
            Self::PointerAuthorityUnavailable => {
                formatter.write_str("no held primary-button press backs this drag")
            }
        }
    }
}

impl Error for X11HostError {}

/// Everything one X11 editor window needs from Matari, in one owned value.
///
/// The host tracks three things a toolkit would otherwise duplicate:
///
/// - **Pointer authority.** The last root-space pointer event and whether the
///   primary button is still held, so [`start_drag`](Self::start_drag) can be
///   called from a GUI callback that no longer has the raw event.
/// - **The outbound session.** At most one live [`X11Session`], driven from
///   the toolkit's own events and superseded cleanly when a new drag starts.
/// - **The inbound drop router.** Installed only on compositors that need it
///   (see [`attach`](Self::attach)), driven ahead of the toolkit's own event
///   handling, and torn down on [`shutdown`](Self::shutdown).
///
/// # Integration
///
/// ```no_run
/// # use matari_audio_drag_and_drop::toolkit::X11Host;
/// # use x11rb::connection::Connection;
/// # fn integrate<C: Connection>(conn: &C, editor_window: u32, event: &x11rb::protocol::Event) {
/// // once, after the editor window is mapped
/// let mut matari = X11Host::new();
/// if let Err(error) = matari.attach(conn, editor_window) {
///     eprintln!("matari: inbound drop routing unavailable: {error}");
/// }
///
/// // first thing in the toolkit's event handler
/// match matari.handle_event(conn, event) {
///     Ok(true) => return, // the event was Matari's, not the toolkit's
///     Ok(false) => {}
///     Err(error) => eprintln!("matari: {error}"),
/// }
///
/// // when the window closes
/// matari.shutdown(conn);
/// # }
/// ```
#[derive(Default)]
pub struct X11Host {
    router: Option<X11DropRouter>,
    outbound: Option<X11Session>,
    last_pointer: Option<X11PointerEvent>,
    primary_button_down: bool,
}

impl fmt::Debug for X11Host {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("X11Host")
            .field("router_active", &self.router.is_some())
            .field("outbound_in_flight", &self.outbound_in_flight())
            .field("primary_button_down", &self.primary_button_down)
            .field("pointer_authority", &self.last_pointer.is_some())
            .finish()
    }
}

impl X11Host {
    /// Create a detached host. Performs no I/O.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            router: None,
            outbound: None,
            last_pointer: None,
            primary_button_down: false,
        }
    }

    /// Install inbound drop routing for `editor_window`, if this session needs it.
    ///
    /// Returns `Ok(false)` on every compositor whose XWayland bridge delivers
    /// XDND to the embedded editor directly — KWin, Mutter, COSMIC, and plain
    /// X11 — where routing would be redundant. `Ok(true)` means the host now
    /// owns a router that [`handle_event`](Self::handle_event) must be fed and
    /// [`shutdown`](Self::shutdown) must tear down.
    ///
    /// A router is required under Hyprland's serial-less native Wayland
    /// compatibility bridge, which delivers the drop to the host's toplevel
    /// window and never to the `XdndAware` plug-in child inside it. Without a
    /// router an embedded editor there receives no inbound drag events at all,
    /// however correctly the rest of the integration is wired.
    ///
    /// Calling this again replaces any router already installed.
    ///
    /// # Errors
    ///
    /// Returns the X11 failure when the router is needed but could not be
    /// installed. The host stays usable for outbound drags; inbound routing is
    /// simply absent, exactly as if this session did not need one.
    pub fn attach<C: Connection>(
        &mut self,
        conn: &C,
        editor_window: XWindow,
    ) -> Result<bool, X11SessionError> {
        self.detach_router(conn);
        self.router = X11DropRouter::install(conn, editor_window)?;
        Ok(self.router.is_some())
    }

    /// Whether this host installed an inbound drop router.
    #[must_use]
    pub const fn router_active(&self) -> bool {
        self.router.is_some()
    }

    /// Whether an outbound drag started here has not finished transferring.
    #[must_use]
    pub fn outbound_in_flight(&self) -> bool {
        self.outbound
            .as_ref()
            .is_some_and(|session| !session.transfer_complete())
    }

    /// Whether [`start_drag`](Self::start_drag) would be accepted right now.
    ///
    /// Poll this to decide whether a drag gesture is offerable — greying out a
    /// drag handle, say — rather than starting a drag to discover it is not.
    #[must_use]
    pub fn can_start_drag(&self) -> bool {
        !self.outbound_in_flight() && self.primary_button_down && self.last_pointer.is_some()
    }

    /// Drive one raw X11 event, ahead of the toolkit's own handling.
    ///
    /// Feed this **every** event the toolkit receives, before it inspects the
    /// event itself. The host uses motion, press, and release events to track
    /// pointer authority even when no drag is running, so skipping events
    /// makes later [`start_drag`](Self::start_drag) calls fail with
    /// [`X11HostError::PointerAuthorityUnavailable`].
    ///
    /// Returns `Ok(true)` when the event was addressed to Matari's own
    /// windows. The toolkit must then stop processing that event.
    ///
    /// # Errors
    ///
    /// Returns the native failure after the host has already torn down
    /// whichever piece failed — a failed router is uninstalled, a failed
    /// outbound session is cancelled. The host stays usable and the event was
    /// not consumed, so the toolkit should log and carry on handling it.
    pub fn handle_event<C: Connection>(
        &mut self,
        conn: &C,
        event: &Event,
    ) -> Result<bool, X11SessionError> {
        let routed = match self.router.as_mut().map(|r| r.handle_event(conn, event)) {
            Some(Ok(consumed)) => Ok(consumed),
            Some(Err(error)) => {
                self.detach_router(conn);
                Err(error)
            }
            None => Ok(false),
        };
        if matches!(routed, Ok(true)) {
            return routed;
        }

        self.observe_pointer(event);
        let driven = self.drive_outbound(conn, event);
        if matches!(event, Event::ButtonRelease(event) if event.detail == PRIMARY_BUTTON) {
            self.primary_button_down = false;
        }

        // A router teardown is the more actionable of the two, so it wins.
        routed.and(driven.map(|()| false))
    }

    /// Commit an outbound [`StartTicket`] against this window's live X11 queue.
    ///
    /// `origin` must resolve to this editor's own window; build it with
    /// [`DragOrigin::from_window`].
    ///
    /// `Ok(())` means the ticket was consumed, not that the native drag
    /// succeeded. A native start failure is reported through the controller's
    /// own event stream — the ticket's reporter has already emitted a terminal
    /// event by the time this returns — because by then the session exists and
    /// failing it is a lifecycle event, not a rejected request.
    ///
    /// # Errors
    ///
    /// Returns the unconsumed ticket inside a [`RejectedStart`] when this host
    /// cannot accept a drag at all; see [`X11HostError`].
    pub fn start_drag<C: Connection>(
        &mut self,
        conn: &C,
        origin: DragOrigin<'_>,
        ticket: StartTicket,
    ) -> Result<(), RejectedStart<X11HostError>> {
        if self.outbound_in_flight() {
            return Err(ticket.reject(X11HostError::Busy));
        }
        let Some(press) = self.last_pointer.filter(|_| self.primary_button_down) else {
            return Err(ticket.reject(X11HostError::PointerAuthorityUnavailable));
        };

        if let Some(session) = self.outbound.take() {
            let _ = session.supersede(conn);
        }
        self.outbound = ticket.start_x11(conn, origin, press).ok();
        Ok(())
    }

    /// Cancel any live drag and remove the inbound router.
    ///
    /// Call this before the editor window is destroyed, while the X11
    /// connection is still open. Idempotent, and the host may be
    /// [`attach`](Self::attach)ed again afterwards.
    pub fn shutdown<C: Connection>(&mut self, conn: &C) {
        if let Some(session) = self.outbound.take() {
            let _ = session.cancel(conn);
        }
        self.detach_router(conn);
        self.last_pointer = None;
        self.primary_button_down = false;
    }

    fn detach_router<C: Connection>(&mut self, conn: &C) {
        if let Some(router) = self.router.take() {
            let _ = router.uninstall(conn);
        }
    }

    fn observe_pointer(&mut self, event: &Event) {
        self.last_pointer = Some(match event {
            Event::MotionNotify(event) => {
                self.primary_button_down = event.state.contains(KeyButMask::BUTTON1);
                X11PointerEvent {
                    root: event.root,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    time: event.time,
                }
            }
            Event::ButtonPress(event) => {
                if event.detail == PRIMARY_BUTTON {
                    self.primary_button_down = true;
                }
                X11PointerEvent {
                    root: event.root,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    time: event.time,
                }
            }
            Event::ButtonRelease(event) => X11PointerEvent {
                root: event.root,
                root_x: event.root_x,
                root_y: event.root_y,
                time: event.time,
            },
            _ => return,
        });
    }

    fn drive_outbound<C: Connection>(
        &mut self,
        conn: &C,
        event: &Event,
    ) -> Result<(), X11SessionError> {
        let Some(mut session) = self.outbound.take() else {
            return Ok(());
        };
        match session.handle_event(conn, event) {
            Ok(X11SessionStatus::Active) => {
                self.outbound = Some(session);
                Ok(())
            }
            Ok(X11SessionStatus::Finished) => Ok(()),
            Err(error) => {
                let _ = session.cancel(conn);
                Err(error)
            }
        }
    }
}

/// X11 button number of the primary mouse button.
const PRIMARY_BUTTON: u8 = 1;

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xproto::{
        ButtonPressEvent, ButtonReleaseEvent, Motion, MotionNotifyEvent,
    };

    /// The pointer fields the host reads, with the rest of the wire struct zeroed.
    fn pointer(detail: u8, time: u32, state: KeyButMask) -> ButtonPressEvent {
        ButtonPressEvent {
            response_type: 0,
            detail,
            sequence: 0,
            time,
            root: 1,
            event: 0,
            child: 0,
            root_x: 10,
            root_y: 20,
            event_x: 0,
            event_y: 0,
            state,
            same_screen: true,
        }
    }

    fn press(button: u8) -> Event {
        Event::ButtonPress(pointer(button, 42, KeyButMask::default()))
    }

    fn release(button: u8) -> Event {
        Event::ButtonRelease(ButtonReleaseEvent::from(pointer(
            button,
            43,
            KeyButMask::default(),
        )))
    }

    fn motion(button1_held: bool) -> Event {
        let held = if button1_held {
            KeyButMask::BUTTON1
        } else {
            KeyButMask::default()
        };
        let base = pointer(0, 44, held);
        Event::MotionNotify(MotionNotifyEvent {
            response_type: base.response_type,
            detail: Motion::NORMAL,
            sequence: base.sequence,
            time: base.time,
            root: base.root,
            event: base.event,
            child: base.child,
            root_x: base.root_x,
            root_y: base.root_y,
            event_x: base.event_x,
            event_y: base.event_y,
            state: base.state,
            same_screen: base.same_screen,
        })
    }

    #[test]
    fn a_fresh_host_has_no_pointer_authority() {
        let host = X11Host::new();
        assert!(!host.can_start_drag());
        assert!(!host.router_active());
        assert!(!host.outbound_in_flight());
    }

    #[test]
    fn a_held_primary_press_grants_authority() {
        let mut host = X11Host::new();
        host.observe_pointer(&press(PRIMARY_BUTTON));
        assert!(host.can_start_drag());
    }

    #[test]
    fn a_secondary_press_never_grants_authority() {
        let mut host = X11Host::new();
        host.observe_pointer(&press(3));
        assert!(!host.can_start_drag());
    }

    #[test]
    fn releasing_the_primary_button_revokes_authority() {
        let mut host = X11Host::new();
        host.observe_pointer(&press(PRIMARY_BUTTON));
        host.observe_pointer(&release(PRIMARY_BUTTON));
        // `handle_event` clears the held flag after the session has seen the
        // release, so the release itself still carries usable coordinates.
        assert!(host.can_start_drag());
        host.primary_button_down = false;
        assert!(!host.can_start_drag());
    }

    #[test]
    fn motion_without_button_one_revokes_authority() {
        let mut host = X11Host::new();
        host.observe_pointer(&press(PRIMARY_BUTTON));

        host.observe_pointer(&motion(true));
        assert!(host.can_start_drag());
        assert_eq!(host.last_pointer.map(|pointer| pointer.time), Some(44));

        // Motion with button 1 clear means the release was delivered
        // elsewhere; authority must not survive it.
        host.observe_pointer(&motion(false));
        assert!(!host.can_start_drag());
    }
}
