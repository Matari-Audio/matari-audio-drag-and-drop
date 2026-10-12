use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

mod drop_router;
mod mime;
mod preview;
mod wayland_bridge;

use drop_router::OutboundGuard;
pub use drop_router::X11DropRouter;

use crate::{
    DragOrigin, FileDragPayloadData, FileSet, LinuxOutcome, LinuxRejector, NativeProtocol,
    PreviewFailureStage, PreviewStatus, SessionReporter, SessionRoute, SourceContext,
};
use mime::MimeTargets;
use preview::PreviewWindow;
use raw_window_handle::RawWindowHandle;
use wayland_bridge::WaylandBridgeSession;
use x11rb::CURRENT_TIME;
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ButtonReleaseEvent, ClientMessageEvent, ConnectionExt, CreateWindowAux,
    EventMask, GrabMode, GrabStatus, PropMode, SelectionNotifyEvent, SelectionRequestEvent,
    Window as XWindow, WindowClass,
};
use x11rb::protocol::{ErrorKind, Event};
use x11rb::wrapper::ConnectionExt as _;

const XDND_VERSION: u32 = 5;
const STATUS_ACCEPT: u32 = 1;

/// Cross-display bridge available to an X11 or XWayland editor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum X11WaylandBridge {
    /// KWin or Mutter translates the editor's canonical XDND source.
    CanonicalXdnd,
    /// Hyprland accepts a persistent native Wayland source without a Wayland press serial.
    HyprlandSeriallessCompat,
    /// No supported X11-source to native-Wayland bridge is known.
    Unavailable,
}

/// Live evidence used to classify an X11 editor's Wayland bridge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum X11BridgeEvidence {
    /// The X server is owned by xwayland-satellite, which does not bridge DND.
    XwaylandSatellite,
    /// The live X server and session both identify Hyprland.
    HyprlandInstance,
    /// The live X window manager identifies a canonical XDND bridge.
    CanonicalWindowManager,
    /// The desktop environment indicates a canonical XDND bridge.
    DesktopEnvironment,
    /// No supported cross-display bridge was identified.
    None,
}

impl X11BridgeEvidence {
    /// Human-readable route evidence for diagnostics.
    #[must_use]
    pub const fn summary(self) -> &'static str {
        match self {
            Self::XwaylandSatellite => "xwayland-satellite",
            Self::HyprlandInstance => "live Hyprland XWM and instance",
            Self::CanonicalWindowManager => "canonical X window manager",
            Self::DesktopEnvironment => "desktop environment hint",
            Self::None => "no known cross-display bridge",
        }
    }
}

/// Detected X11-to-Wayland bridge and the evidence behind it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct X11BridgeReport {
    /// Bridge available to the editor.
    pub bridge: X11WaylandBridge,
    /// Live or environmental evidence used for the decision.
    pub evidence: X11BridgeEvidence,
}

impl X11BridgeReport {
    /// Native protocol selected by this detection snapshot.
    #[must_use]
    pub const fn protocol(self) -> NativeProtocol {
        match self.bridge {
            X11WaylandBridge::HyprlandSeriallessCompat => NativeProtocol::WaylandDataDevice,
            X11WaylandBridge::CanonicalXdnd | X11WaylandBridge::Unavailable => NativeProtocol::Xdnd,
        }
    }
}

/// Detect the compositor bridge available to this process.
#[must_use]
pub fn x11_wayland_bridge() -> X11WaylandBridge {
    x11_bridge_report().bridge
}

/// Detect the compositor bridge and retain the evidence used to select it.
#[must_use]
pub fn x11_bridge_report() -> X11BridgeReport {
    let window_manager = live_x11_window_manager();
    if window_manager
        .as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case(b"xwayland-satellite"))
    {
        return X11BridgeReport {
            bridge: X11WaylandBridge::Unavailable,
            evidence: X11BridgeEvidence::XwaylandSatellite,
        };
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return X11BridgeReport {
            bridge: X11WaylandBridge::Unavailable,
            evidence: X11BridgeEvidence::None,
        };
    }
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
        && window_manager
            .as_deref()
            .and_then(|name| name.get(..b"Hyprland".len()))
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"Hyprland"))
    {
        return X11BridgeReport {
            bridge: X11WaylandBridge::HyprlandSeriallessCompat,
            evidence: X11BridgeEvidence::HyprlandInstance,
        };
    }
    if window_manager
        .as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case(b"Smithay X WM"))
    {
        return X11BridgeReport {
            bridge: X11WaylandBridge::CanonicalXdnd,
            evidence: X11BridgeEvidence::CanonicalWindowManager,
        };
    }
    let desktop = [
        std::env::var_os("XDG_CURRENT_DESKTOP"),
        std::env::var_os("XDG_SESSION_DESKTOP"),
    ];
    if desktop.iter().flatten().any(|desktop| {
        let desktop = desktop.to_string_lossy().to_ascii_lowercase();
        desktop.contains("kde") || desktop.contains("plasma") || desktop.contains("gnome")
    }) {
        X11BridgeReport {
            bridge: X11WaylandBridge::CanonicalXdnd,
            evidence: X11BridgeEvidence::DesktopEnvironment,
        }
    } else {
        X11BridgeReport {
            bridge: X11WaylandBridge::Unavailable,
            evidence: X11BridgeEvidence::None,
        }
    }
}

/// Native protocol an X11 or XWayland editor should start.
#[must_use]
pub fn x11_outbound_protocol() -> NativeProtocol {
    x11_bridge_report().protocol()
}

fn live_x11_window_manager() -> Option<Vec<u8>> {
    let (connection, screen_index) = x11rb::connect(None).ok()?;
    let root = connection.setup().roots.get(screen_index)?.root;
    let supporting_window_atom = atom(&connection, b"_NET_SUPPORTING_WM_CHECK").ok()?;
    let supporting_window = connection
        .get_property(false, root, supporting_window_atom, AtomEnum::WINDOW, 0, 1)
        .ok()?
        .reply()
        .ok()?
        .value32()?
        .next()?;
    let name_atom = atom(&connection, b"_NET_WM_NAME").ok()?;
    connection
        .get_property(false, supporting_window, name_atom, AtomEnum::ANY, 0, 64)
        .ok()?
        .reply()
        .ok()
        .map(|reply| reply.value)
}

/// Authoritative pointer state from the X11 event that initiated a drag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct X11PointerEvent {
    /// Root window containing the pointer.
    pub root: XWindow,
    /// Pointer position along the root window's horizontal axis.
    pub root_x: i16,
    /// Pointer position along the root window's vertical axis.
    pub root_y: i16,
    /// Server timestamp from the initiating pointer event.
    pub time: u32,
}

/// Whether an event-scoped X11 drag still owns a live session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum X11SessionStatus {
    /// The session still owns an active native drag.
    Active,
    /// The session reached a terminal native event.
    Finished,
}

/// Failure while starting or driving an event-scoped X11 session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct X11SessionError {
    message: Box<str>,
}

impl X11SessionError {
    pub(crate) fn new(message: impl Into<Box<str>>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for X11SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for X11SessionError {}

/// Failure to start an event-scoped X11 session.
pub type X11StartError = X11SessionError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct XdndTarget {
    logical: XWindow,
    recipient: XWindow,
    version: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TargetAcceptance {
    Unknown,
    Accepted(XWindow),
    Rejected(XWindow),
}

struct XdndAtoms {
    xdnd_selection: Atom,
    xdnd_enter: Atom,
    xdnd_leave: Atom,
    xdnd_position: Atom,
    xdnd_status: Atom,
    xdnd_drop: Atom,
    xdnd_finished: Atom,
    xdnd_aware: Atom,
    xdnd_proxy: Atom,
    xdnd_type_list: Atom,
    xdnd_action_copy: Atom,
    text_uri_list: Atom,
    text_uri_list_utf8: Atom,
    text_x_uri: Atom,
    application_x_kde4_urilist: Atom,
    x_special_gnome_copied_files: Atom,
    text_plain: Atom,
    text_plain_utf8: Atom,
    targets: Atom,
    utf8_string: Atom,
    string: Atom,
}

impl XdndAtoms {
    fn new<C: Connection>(conn: &C) -> Result<Self, X11SessionError> {
        Ok(Self {
            xdnd_selection: atom(conn, b"XdndSelection")?,
            xdnd_enter: atom(conn, b"XdndEnter")?,
            xdnd_leave: atom(conn, b"XdndLeave")?,
            xdnd_position: atom(conn, b"XdndPosition")?,
            xdnd_status: atom(conn, b"XdndStatus")?,
            xdnd_drop: atom(conn, b"XdndDrop")?,
            xdnd_finished: atom(conn, b"XdndFinished")?,
            xdnd_aware: atom(conn, b"XdndAware")?,
            xdnd_proxy: atom(conn, b"XdndProxy")?,
            xdnd_type_list: atom(conn, b"XdndTypeList")?,
            xdnd_action_copy: atom(conn, b"XdndActionCopy")?,
            text_uri_list: atom(conn, b"text/uri-list")?,
            text_uri_list_utf8: atom(conn, b"text/uri-list;charset=utf-8")?,
            text_x_uri: atom(conn, b"text/x-uri")?,
            application_x_kde4_urilist: atom(conn, b"application/x-kde4-urilist")?,
            x_special_gnome_copied_files: atom(conn, b"x-special/gnome-copied-files")?,
            text_plain: atom(conn, b"text/plain")?,
            text_plain_utf8: atom(conn, b"text/plain;charset=utf-8")?,
            targets: atom(conn, b"TARGETS")?,
            utf8_string: atom(conn, b"UTF8_STRING")?,
            string: AtomEnum::STRING.into(),
        })
    }

    fn mime_targets(&self) -> MimeTargets {
        MimeTargets {
            text_uri_list: self.text_uri_list,
            text_uri_list_utf8: self.text_uri_list_utf8,
            text_x_uri: self.text_x_uri,
            kde_uri_list: self.application_x_kde4_urilist,
            gnome_copied_files: self.x_special_gnome_copied_files,
            text_plain: self.text_plain,
            text_plain_utf8: self.text_plain_utf8,
            utf8_string: self.utf8_string,
            string: self.string,
        }
    }
}

/// Outbound session selected for an X11 or XWayland editor.
///
/// Hyprland uses a persistent native Wayland data-device runtime. Other
/// desktops use XDND on the toolkit's X11 event queue.
#[must_use = "the toolkit must drive this session or explicitly cancel it before teardown"]
pub struct X11Session {
    inner: LinuxSession,
    route: SessionRoute,
    /// Keeps [`X11DropRouter`] from routing this drag back to the editor.
    outbound: Option<OutboundGuard>,
}

enum LinuxSession {
    Xdnd(Box<XdndSession>),
    Wayland {
        session: WaylandBridgeSession,
        released: bool,
    },
}

impl fmt::Debug for X11Session {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            LinuxSession::Xdnd(session) => session.fmt(formatter),
            LinuxSession::Wayland { session, released } => formatter
                .debug_struct("X11WaylandSession")
                .field("session", session)
                .field("released", released)
                .finish(),
        }
    }
}

impl X11Session {
    pub(crate) fn start<C: Connection>(
        conn: &C,
        origin: DragOrigin<'_>,
        files: FileSet,
        reporter: SessionReporter,
        press: X11PointerEvent,
        route: SessionRoute,
    ) -> Result<Self, X11StartError> {
        let source_window = x11_source_window(origin)?;
        if press.root == x11rb::NONE || press.time == CURRENT_TIME {
            return Err(X11SessionError::new(
                "event-scoped drag requires the initiating X11 event root and timestamp",
            ));
        }
        if route.protocol == NativeProtocol::WaylandDataDevice
            && matches!(
                route.source,
                SourceContext::EmbeddedX11 | SourceContext::DetachedX11
            )
        {
            if x11_outbound_protocol() != NativeProtocol::WaylandDataDevice {
                return Err(X11SessionError::new(
                    "serial-less native Wayland is unavailable",
                ));
            }
            capture_pointer(conn, source_window, press.time)?;
            return match WaylandBridgeSession::start(files, reporter) {
                Ok(session) => Ok(Self {
                    inner: LinuxSession::Wayland {
                        session,
                        released: false,
                    },
                    route,
                    outbound: Some(OutboundGuard::register(None)),
                }),
                Err(error) => {
                    let _ = release_pointer(conn, press.time);
                    Err(X11SessionError::new(error.to_string()))
                }
            };
        }
        if route.protocol != NativeProtocol::Xdnd
            || !matches!(
                route.source,
                SourceContext::EmbeddedX11 | SourceContext::DetachedX11
            )
        {
            return Err(X11SessionError::new(
                "selected route is not valid for an X11 or XWayland editor",
            ));
        }

        XdndSession::start(conn, source_window, files, reporter, press).map(|session| Self {
            inner: LinuxSession::Xdnd(Box::new(session)),
            route,
            // XDND owns a registration for each native source epoch.
            outbound: None,
        })
    }

    /// Native protocol selected for this session.
    #[must_use]
    pub const fn route(&self) -> SessionRoute {
        self.route
    }

    /// Drive an XDND session from the toolkit's event queue.
    ///
    /// Native Wayland sessions run on their own blocking protocol queue, so
    /// X11 events only provide an opportunity to observe terminal state.
    pub fn handle_event<C: Connection>(
        &mut self,
        conn: &C,
        event: &Event,
    ) -> Result<X11SessionStatus, X11SessionError> {
        let status = self.drive(conn, event);
        if !matches!(status, Ok(X11SessionStatus::Active)) || self.transfer_complete() {
            self.outbound = None;
        }
        status
    }

    /// Advance protocol deadlines even when the X11 queue is idle.
    ///
    /// Call from the owning event loop's heartbeat (at least every 100 ms).
    /// Missing Status cancels the drag after two seconds; a missing Finished
    /// retires it after twenty seconds without reporting a successful import.
    pub fn poll<C: Connection>(&mut self, conn: &C) -> Result<X11SessionStatus, X11SessionError> {
        self.poll_at(conn, Instant::now())
    }

    /// Advance deadlines using a supplied monotonic clock instant.
    ///
    /// Event loops with a shared clock may pass their current instant here;
    /// deterministic protocol tests can advance it without sleeping.
    pub fn poll_at<C: Connection>(
        &mut self,
        conn: &C,
        now: Instant,
    ) -> Result<X11SessionStatus, X11SessionError> {
        let status = match &mut self.inner {
            LinuxSession::Xdnd(session) => session.poll_at(conn, now),
            LinuxSession::Wayland { session, released } => {
                Ok(if session.is_terminal() && *released {
                    X11SessionStatus::Finished
                } else {
                    X11SessionStatus::Active
                })
            }
        };
        if !matches!(status, Ok(X11SessionStatus::Active)) || self.transfer_complete() {
            self.outbound = None;
        }
        status
    }

    fn drive<C: Connection>(
        &mut self,
        conn: &C,
        event: &Event,
    ) -> Result<X11SessionStatus, X11SessionError> {
        match &mut self.inner {
            LinuxSession::Xdnd(session) => session.handle_event(conn, event),
            LinuxSession::Wayland { session, released } => {
                if let Event::ButtonRelease(event) = event
                    && event.detail == 1
                {
                    release_pointer(conn, event.time)?;
                    *released = true;
                } else if *released
                    && matches!(event, Event::ButtonPress(event) if event.detail == 1)
                {
                    session.cancel_stale();
                    return Ok(X11SessionStatus::Finished);
                }
                Ok(if session.is_terminal() && *released {
                    X11SessionStatus::Finished
                } else {
                    X11SessionStatus::Active
                })
            }
        }
    }

    /// Whether protocol evidence permits a new drag gesture.
    ///
    /// The native source remains alive to serve late MIME requests.
    #[must_use]
    pub fn transfer_complete(&self) -> bool {
        match &self.inner {
            LinuxSession::Xdnd(session) => session.transfer_complete(),
            LinuxSession::Wayland { session, .. } => {
                session.transfer_complete() || session.is_terminal()
            }
        }
    }

    /// Cancel from an authoritative toolkit teardown or gesture-cancel event.
    pub fn cancel<C: Connection>(self, conn: &C) -> Result<(), X11SessionError> {
        match self.inner {
            LinuxSession::Xdnd(session) => (*session).cancel(conn),
            LinuxSession::Wayland { session, released } => {
                let result = if released {
                    Ok(())
                } else {
                    release_pointer(conn, CURRENT_TIME)
                };
                session.cancel();
                result
            }
        }
    }

    /// Retire this handle when a transfer-ready session is superseded.
    pub fn supersede<C: Connection>(self, conn: &C) -> Result<(), X11SessionError> {
        match self.inner {
            LinuxSession::Xdnd(session) => (*session).supersede(conn),
            LinuxSession::Wayland { session, released } => {
                let result = if released {
                    Ok(())
                } else {
                    release_pointer(conn, CURRENT_TIME)
                };
                session.cancel();
                result
            }
        }
    }
}

fn capture_pointer<C: Connection>(
    conn: &C,
    source_window: XWindow,
    time: u32,
) -> Result<(), X11SessionError> {
    let status = conn
        .grab_pointer(
            true,
            source_window,
            EventMask::POINTER_MOTION | EventMask::BUTTON_RELEASE,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            x11rb::NONE,
            time,
        )
        .map_err(x11_error)?
        .reply()
        .map_err(x11_error)?
        .status;
    if status != GrabStatus::SUCCESS {
        return Err(X11SessionError::new(format!(
            "could not capture the drag pointer: {status:?}"
        )));
    }
    Ok(())
}

fn release_pointer<C: Connection>(conn: &C, time: u32) -> Result<(), X11SessionError> {
    conn.ungrab_pointer(time).map_err(x11_error)?;
    conn.flush().map_err(x11_error)
}

fn x11_source_window(origin: DragOrigin<'_>) -> Result<XWindow, X11StartError> {
    match origin.window_handle().as_raw() {
        RawWindowHandle::Xlib(handle) if handle.window != 0 => Ok(handle.window as XWindow),
        RawWindowHandle::Xcb(handle) => Ok(handle.window.get()),
        _ => Err(X11SessionError::new(
            "event-scoped drag requires an X11 or XWayland origin",
        )),
    }
}

/// XDND state owned by the toolkit event loop that owns the X11 connection.
struct XdndSession {
    atoms: XdndAtoms,
    root: XWindow,
    source_window: XWindow,
    editor_window: XWindow,
    source_windows: Vec<XWindow>,
    source_guards: Vec<OutboundGuard>,
    source_used: bool,
    selection_time: u32,
    file_payload: FileDragPayloadData,
    current_target: Option<XdndTarget>,
    target_acceptance: TargetAcceptance,
    awaiting_status: bool,
    last_position: Option<(i16, i16)>,
    queued_position: Option<(i16, i16, u32)>,
    status_deadline: Option<Instant>,
    finished_deadline: Option<Instant>,
    position_deadline: Option<Instant>,
    released_target: Option<XdndTarget>,
    drop_target: Option<XdndTarget>,
    payload_target: Option<XWindow>,
    transfer_complete: bool,
    pointer_grabbed: bool,
    preview: Option<PreviewWindow>,
    last_event_time: u32,
    reporter: SessionReporter,
    finished: bool,
}

impl fmt::Debug for XdndSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("X11Session")
            .field("source_window", &self.source_window)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl XdndSession {
    const STATUS_TIMEOUT: Duration = Duration::from_secs(2);
    const FINISHED_TIMEOUT: Duration = Duration::from_secs(20);

    fn start<C: Connection>(
        conn: &C,
        editor_window: XWindow,
        files: FileSet,
        reporter: SessionReporter,
        press: X11PointerEvent,
    ) -> Result<Self, X11StartError> {
        let (paths, preview) = files.into_parts();
        let atoms = XdndAtoms::new(conn)?;
        let source_window = create_source_window(conn, press.root)?;
        conn.set_selection_owner(source_window, atoms.xdnd_selection, press.time)
            .map_err(|error| {
                let _ = conn.destroy_window(source_window);
                x11_error(error)
            })?;
        let owner = match conn
            .get_selection_owner(atoms.xdnd_selection)
            .map_err(x11_error)
            .and_then(|cookie| cookie.reply().map_err(x11_error))
        {
            Ok(reply) => reply.owner,
            Err(error) => {
                let _ = conn.set_selection_owner(x11rb::NONE, atoms.xdnd_selection, press.time);
                let _ = conn.destroy_window(source_window);
                let _ = conn.flush();
                return Err(error);
            }
        };
        if owner != source_window {
            let _ = conn.destroy_window(source_window);
            return Err(X11SessionError::new("could not own XdndSelection"));
        }
        if let Err(error) = capture_pointer(conn, editor_window, press.time) {
            let _ = conn.set_selection_owner(x11rb::NONE, atoms.xdnd_selection, press.time);
            let _ = conn.destroy_window(source_window);
            let _ = conn.flush();
            return Err(error);
        }

        let preview = preview.and_then(|preview| {
            match PreviewWindow::new(conn, press.root, press.root_x, press.root_y, &preview) {
                Ok(window) => {
                    reporter.preview(PreviewStatus::Attached);
                    Some(window)
                }
                Err(_) => {
                    reporter.preview(PreviewStatus::Unavailable {
                        stage: PreviewFailureStage::Attach,
                        native_code: None,
                    });
                    None
                }
            }
        });
        let mut session = Self {
            atoms,
            root: press.root,
            source_window,
            editor_window,
            source_windows: vec![source_window],
            source_guards: vec![OutboundGuard::register(Some(source_window))],
            source_used: false,
            selection_time: press.time,
            file_payload: FileDragPayloadData::from_validated(paths),
            current_target: None,
            target_acceptance: TargetAcceptance::Unknown,
            awaiting_status: false,
            last_position: None,
            queued_position: None,
            status_deadline: None,
            finished_deadline: None,
            position_deadline: None,
            released_target: None,
            drop_target: None,
            payload_target: None,
            transfer_complete: false,
            pointer_grabbed: true,
            preview,
            last_event_time: press.time,
            reporter,
            finished: false,
        };
        if let Err(error) = session.update_target(conn, press.root_x, press.root_y) {
            session.release_pointer(conn);
            if let Some(preview) = session.preview.take() {
                preview.destroy(conn);
            }
            let _ = session.release_selection(conn);
            session.destroy_source_windows(conn);
            let _ = conn.flush();
            return Err(error);
        }
        if let Err(error) = conn.flush().map_err(x11_error) {
            session.release_pointer(conn);
            if let Some(preview) = session.preview.take() {
                preview.destroy(conn);
            }
            let _ = session.release_selection(conn);
            session.destroy_source_windows(conn);
            let _ = conn.flush();
            return Err(error);
        }
        Ok(session)
    }

    /// Drive the session from one event taken from the owning X11 queue.
    pub fn handle_event<C: Connection>(
        &mut self,
        conn: &C,
        event: &Event,
    ) -> Result<X11SessionStatus, X11SessionError> {
        if self.finished {
            return Ok(X11SessionStatus::Finished);
        }

        if self.poll_at(conn, Instant::now())? == X11SessionStatus::Finished {
            return Ok(X11SessionStatus::Finished);
        }

        if let Err(error) = self.drive_event(conn, event) {
            self.release_pointer(conn);
            if let Some(preview) = self.preview.take() {
                preview.destroy(conn);
            }
            let _ = self.leave_current_target(conn);
            let _ = self.release_selection(conn);
            let _ = conn.flush();
            self.finished = true;
            self.destroy_source_windows(conn);
            self.reporter
                .finish_linux(LinuxOutcome::Failed(crate::LinuxFailure::Setup));
            return Err(error);
        }

        Ok(if self.finished {
            X11SessionStatus::Finished
        } else {
            X11SessionStatus::Active
        })
    }

    /// Whether protocol evidence already completed the user-visible transfer.
    ///
    /// The session remains alive after this point so targets may request
    /// additional representations before sending `XdndFinished`.
    #[must_use]
    pub const fn transfer_complete(&self) -> bool {
        self.transfer_complete
    }

    fn drive_event<C: Connection>(
        &mut self,
        conn: &C,
        event: &Event,
    ) -> Result<(), X11SessionError> {
        match event {
            Event::MotionNotify(event)
                if event.root == self.root && self.released_target.is_none() =>
            {
                self.note_event_time(event.time);
                self.move_preview(conn, event.root_x, event.root_y);
                self.update_target(conn, event.root_x, event.root_y)?;
            }
            Event::ButtonRelease(event)
                if event.detail == 1
                    && event.root == self.root
                    && self.released_target.is_none() =>
            {
                self.handle_release(conn, event)?;
            }
            Event::ClientMessage(event) if event.type_ == self.atoms.xdnd_status => {
                self.handle_status(conn, event)?;
            }
            Event::ClientMessage(event) if event.type_ == self.atoms.xdnd_finished => {
                self.handle_finished(conn, event)?;
            }
            Event::SelectionRequest(event) => {
                self.handle_selection_request(conn, event)?;
            }
            Event::Expose(event)
                if self
                    .preview
                    .as_ref()
                    .is_some_and(|preview| preview.window == event.window) =>
            {
                self.redraw_preview(conn);
            }
            Event::SelectionClear(event)
                if event.selection == self.atoms.xdnd_selection
                    && event.owner == self.source_window =>
            {
                self.finish(conn, LinuxOutcome::Cancelled)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn poll_at<C: Connection>(
        &mut self,
        conn: &C,
        now: Instant,
    ) -> Result<X11SessionStatus, X11SessionError> {
        if !self.finished {
            if self
                .finished_deadline
                .is_some_and(|deadline| now >= deadline)
            {
                // A sent Drop can have taken effect even without Finished.
                self.finish(conn, LinuxOutcome::Indeterminate)?;
            } else if self.status_deadline.is_some_and(|deadline| now >= deadline) {
                let leave = self.leave_current_target(conn);
                let finish = self.finish(conn, LinuxOutcome::Cancelled);
                leave.and(finish)?;
            } else if self.released_target.is_none()
                && !self.awaiting_status
                && self
                    .position_deadline
                    .is_some_and(|deadline| now >= deadline)
                && let (Some(target), Some((x, y))) = (self.current_target, self.last_position)
            {
                // Targets use periodic positions for stationary scroll zones.
                self.last_position = None;
                self.send_position(conn, target, x, y, self.last_event_time)?;
            }
        }
        Ok(if self.finished {
            X11SessionStatus::Finished
        } else {
            X11SessionStatus::Active
        })
    }

    fn destroy_source_windows<C: Connection>(&mut self, conn: &C) {
        for window in self.source_windows.drain(..) {
            let _ = conn.destroy_window(window);
        }
        self.source_guards.clear();
        let _ = conn.flush();
    }

    fn release_selection<C: Connection>(&self, conn: &C) -> Result<(), X11SessionError> {
        let owner = conn
            .get_selection_owner(self.atoms.xdnd_selection)
            .map_err(x11_error)?
            .reply()
            .map_err(x11_error)?
            .owner;
        if self.source_windows.contains(&owner) {
            conn.set_selection_owner(x11rb::NONE, self.atoms.xdnd_selection, CURRENT_TIME)
                .map_err(x11_error)?;
        }
        Ok(())
    }

    fn rotate_source<C: Connection>(&mut self, conn: &C) -> Result<(), X11SessionError> {
        if !self.source_used {
            return Ok(());
        }
        let source = create_source_window(conn, self.root)?;
        self.source_windows.push(source);
        self.source_guards
            .push(OutboundGuard::register(Some(source)));
        conn.set_selection_owner(source, self.atoms.xdnd_selection, self.last_event_time)
            .map_err(x11_error)?;
        let owner = conn
            .get_selection_owner(self.atoms.xdnd_selection)
            .map_err(x11_error)?
            .reply()
            .map_err(x11_error)?
            .owner;
        if owner != source {
            return Err(X11SessionError::new(
                "could not rotate the XDND source epoch",
            ));
        }
        self.source_window = source;
        self.selection_time = self.last_event_time;
        self.source_used = false;
        Ok(())
    }

    /// Cancel from an authoritative toolkit teardown or gesture-cancel event.
    pub fn cancel<C: Connection>(mut self, conn: &C) -> Result<(), X11SessionError> {
        if !self.finished {
            if self.drop_target.is_some() {
                return self.finish(conn, LinuxOutcome::Indeterminate);
            }
            self.release_pointer(conn);
            if let Some(preview) = self.preview.take() {
                preview.destroy(conn);
            }
            let leave = self.leave_current_target(conn);
            let finish = self.finish(conn, LinuxOutcome::Cancelled);
            leave.and(finish)?;
        }
        Ok(())
    }

    /// Retire a transfer-ready session when a new drag gesture supersedes it.
    ///
    /// A drop was already sent, so this deliberately does not send
    /// `XdndLeave`. The target action remains unconfirmed.
    pub fn supersede<C: Connection>(mut self, conn: &C) -> Result<(), X11SessionError> {
        if !self.finished {
            self.finish(conn, LinuxOutcome::Indeterminate)?;
        }
        Ok(())
    }

    fn note_event_time(&mut self, time: u32) {
        if time != CURRENT_TIME {
            self.last_event_time = time;
        }
    }

    fn release_pointer<C: Connection>(&mut self, conn: &C) {
        if self.pointer_grabbed {
            let _ = conn.ungrab_pointer(self.last_event_time);
            self.pointer_grabbed = false;
        }
    }

    fn move_preview<C: Connection>(&mut self, conn: &C, root_x: i16, root_y: i16) {
        let failed = self
            .preview
            .as_ref()
            .is_some_and(|preview| preview.move_to(conn, root_x, root_y).is_err());
        if failed && let Some(preview) = self.preview.take() {
            preview.destroy(conn);
            self.reporter.preview(PreviewStatus::Unavailable {
                stage: PreviewFailureStage::Move,
                native_code: None,
            });
        }
    }

    fn redraw_preview<C: Connection>(&mut self, conn: &C) {
        let failed = self
            .preview
            .as_ref()
            .is_some_and(|preview| preview.draw(conn).is_err());
        if failed && let Some(preview) = self.preview.take() {
            preview.destroy(conn);
            self.reporter.preview(PreviewStatus::Unavailable {
                stage: PreviewFailureStage::Redraw,
                native_code: None,
            });
        }
    }

    fn update_target<C: Connection>(
        &mut self,
        conn: &C,
        root_x: i16,
        root_y: i16,
    ) -> Result<(), X11SessionError> {
        let target = self.find_xdnd_target(conn, root_x, root_y)?;
        if target != self.current_target {
            self.leave_current_target(conn)?;
            self.current_target = target;
            if let Some(target) = target {
                self.rotate_source(conn)?;
                self.send_enter(conn, target)?;
                self.source_used = true;
            }
        }
        if let Some(target) = self.current_target {
            self.send_position(conn, target, root_x, root_y, self.last_event_time)?;
        }
        Ok(())
    }

    fn handle_release<C: Connection>(
        &mut self,
        conn: &C,
        event: &ButtonReleaseEvent,
    ) -> Result<(), X11SessionError> {
        self.note_event_time(event.time);
        self.release_pointer(conn);
        if let Some(preview) = self.preview.take() {
            preview.destroy(conn);
        }
        self.update_target(conn, event.root_x, event.root_y)?;
        let Some(target) = self.current_target else {
            return self.finish(conn, LinuxOutcome::Cancelled);
        };
        self.released_target = Some(target);
        self.maybe_complete_release(conn)
    }

    fn send_drop<C: Connection>(
        &mut self,
        conn: &C,
        target: XdndTarget,
    ) -> Result<(), X11SessionError> {
        self.drop_target = Some(target);
        self.finished_deadline = Some(Instant::now() + Self::FINISHED_TIMEOUT);
        self.send_client_message(
            conn,
            target,
            self.atoms.xdnd_drop,
            [self.source_window, 0, self.last_event_time, 0, 0],
        )?;
        self.reporter.drop_performed();
        if self.payload_target == Some(target.logical) {
            self.mark_transfer_ready();
        }
        Ok(())
    }

    fn handle_status<C: Connection>(
        &mut self,
        conn: &C,
        event: &ClientMessageEvent,
    ) -> Result<(), X11SessionError> {
        if event.format != 32 || event.window != self.source_window {
            return Ok(());
        }
        let data = event.data.as_data32();
        let target = data[0];
        if !self.awaiting_status
            || self
                .current_target
                .is_none_or(|current| current.logical != target)
        {
            return Ok(());
        }
        self.awaiting_status = false;
        self.status_deadline = None;
        let rectangle = data[3];
        self.position_deadline = (self.released_target.is_none()
            && (data[1] & 2 != 0 || rectangle >> 16 == 0 || rectangle & 0xffff == 0))
            .then(|| Instant::now() + Duration::from_millis(100));
        let accepted =
            data[1] & STATUS_ACCEPT == STATUS_ACCEPT && data[4] == self.atoms.xdnd_action_copy;
        self.target_acceptance = if accepted {
            TargetAcceptance::Accepted(target)
        } else {
            TargetAcceptance::Rejected(target)
        };
        if let Some((x, y, time)) = self.queued_position.take() {
            self.send_position(conn, self.current_target.unwrap(), x, y, time)?;
        }
        self.maybe_complete_release(conn)
    }

    fn handle_finished<C: Connection>(
        &mut self,
        conn: &C,
        event: &ClientMessageEvent,
    ) -> Result<(), X11SessionError> {
        if event.format != 32 || event.window != self.source_window {
            return Ok(());
        }
        let data = event.data.as_data32();
        let Some(target) = self.drop_target.filter(|target| target.logical == data[0]) else {
            return Ok(());
        };
        // XDND 3/4 have no success bit or performed-action field.
        let outcome =
            if target.version < 5 || (data[1] & 1 == 1 && data[2] == self.atoms.xdnd_action_copy) {
                LinuxOutcome::Exported
            } else {
                LinuxOutcome::Rejected(LinuxRejector::Target)
            };
        self.finish(conn, outcome)
    }

    fn handle_selection_request<C: Connection>(
        &mut self,
        conn: &C,
        event: &SelectionRequestEvent,
    ) -> Result<(), X11SessionError> {
        if event.selection != self.atoms.xdnd_selection || event.owner != self.source_window {
            return Ok(());
        }
        if event.time != CURRENT_TIME && (event.time.wrapping_sub(self.selection_time) as i32) < 0 {
            self.send_selection_notify(conn, event, AtomEnum::NONE.into())?;
            conn.flush().map_err(x11_error)?;
            return Ok(());
        }
        let property = if event.property == AtomEnum::NONE.into() {
            event.target
        } else {
            event.property
        };

        if event.target == self.atoms.targets {
            let mut targets = vec![self.atoms.targets];
            targets.extend(self.offered_targets());
            conn.change_property32(
                PropMode::REPLACE,
                event.requestor,
                property,
                AtomEnum::ATOM,
                &targets,
            )
            .map_err(x11_error)?;
            self.send_selection_notify(conn, event, property)?;
        } else if self.is_payload_target(event.target) {
            self.reporter.data_requested();
            self.payload_target = self.current_target.map(|target| target.logical);
            let payload = if event.target == self.atoms.x_special_gnome_copied_files {
                self.file_payload.gnome_copied_files()
            } else if event.target == self.atoms.text_plain_utf8
                || event.target == self.atoms.text_plain
                || event.target == self.atoms.utf8_string
                || event.target == self.atoms.string
            {
                self.file_payload.plain_file_list()
            } else {
                self.file_payload.uri_list()
            };
            conn.change_property8(
                PropMode::REPLACE,
                event.requestor,
                property,
                event.target,
                payload,
            )
            .map_err(x11_error)?;
            self.send_selection_notify(conn, event, property)?;
        } else {
            self.send_selection_notify(conn, event, AtomEnum::NONE.into())?;
        }
        conn.flush().map_err(x11_error)?;
        if self
            .drop_target
            .is_some_and(|target| self.payload_target == Some(target.logical))
        {
            self.mark_transfer_ready();
        }
        Ok(())
    }

    fn maybe_complete_release<C: Connection>(&mut self, conn: &C) -> Result<(), X11SessionError> {
        let Some(target) = self.released_target else {
            return Ok(());
        };
        if self.drop_target.is_some() || self.awaiting_status {
            return Ok(());
        }
        match self.target_acceptance {
            TargetAcceptance::Rejected(logical) if logical == target.logical => {
                self.leave_current_target(conn)?;
                self.finish(conn, LinuxOutcome::Rejected(LinuxRejector::Target))
            }
            TargetAcceptance::Accepted(logical) if logical == target.logical => {
                self.send_drop(conn, target)
            }
            TargetAcceptance::Unknown
            | TargetAcceptance::Accepted(_)
            | TargetAcceptance::Rejected(_) => Ok(()),
        }
    }

    fn send_selection_notify<C: Connection>(
        &self,
        conn: &C,
        request: &SelectionRequestEvent,
        property: Atom,
    ) -> Result<(), X11SessionError> {
        let event = SelectionNotifyEvent {
            response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property,
        };
        conn.send_event(false, request.requestor, EventMask::NO_EVENT, event)
            .map(|_| ())
            .map_err(x11_error)
    }

    fn find_xdnd_target<C: Connection>(
        &self,
        conn: &C,
        root_x: i16,
        root_y: i16,
    ) -> Result<Option<XdndTarget>, X11SessionError> {
        let mut candidate = self.root;
        loop {
            // The server's live pointer can have moved since this queued
            // event, especially once release ends the grab. Use its snapshot.
            let pointer = conn
                .translate_coordinates(self.root, candidate, root_x, root_y)
                .map_err(x11_error)?
                .reply()
                .map_err(x11_error)?;
            if pointer.child == x11rb::NONE {
                break;
            }
            candidate = pointer.child;
            if candidate == self.editor_window || self.source_windows.contains(&candidate) {
                return Ok(None);
            }
        }

        let mut window = (candidate != self.root).then_some(candidate);
        while let Some(candidate) = window {
            if candidate == self.editor_window || self.source_windows.contains(&candidate) {
                return Ok(None);
            }
            if let Some(target) = self.target(conn, candidate)? {
                return Ok(Some(target));
            }
            window = self.parent_of(conn, candidate)?;
        }
        Ok(None)
    }

    fn parent_of<C: Connection>(
        &self,
        conn: &C,
        window: XWindow,
    ) -> Result<Option<XWindow>, X11SessionError> {
        let tree = conn
            .query_tree(window)
            .map_err(x11_error)?
            .reply()
            .map_err(x11_error)?;
        Ok((tree.parent != x11rb::NONE).then_some(tree.parent))
    }

    fn xdnd_version<C: Connection>(
        &self,
        conn: &C,
        window: XWindow,
    ) -> Result<Option<u32>, X11SessionError> {
        let property = match conn
            .get_property(false, window, self.atoms.xdnd_aware, AtomEnum::ATOM, 0, 1)
            .map_err(x11_error)?
            .reply()
        {
            Ok(property) => property,
            Err(ReplyError::X11Error(error)) if error.error_kind == ErrorKind::Window => {
                return Ok(None);
            }
            Err(error) => return Err(x11_error(error)),
        };
        Ok(property
            .value32()
            .and_then(|mut values| values.next())
            .filter(|version| *version >= 3)
            .map(|version| version.min(XDND_VERSION)))
    }

    fn target<C: Connection>(
        &self,
        conn: &C,
        logical: XWindow,
    ) -> Result<Option<XdndTarget>, X11SessionError> {
        let recipient = match self.xdnd_proxy(conn, logical)? {
            Some(proxy) if self.xdnd_proxy(conn, proxy)? == Some(proxy) => proxy,
            _ => logical,
        };
        Ok(self
            .xdnd_version(conn, recipient)?
            .map(|version| XdndTarget {
                logical,
                recipient,
                version,
            }))
    }

    fn xdnd_proxy<C: Connection>(
        &self,
        conn: &C,
        window: XWindow,
    ) -> Result<Option<XWindow>, X11SessionError> {
        let property = match conn
            .get_property(false, window, self.atoms.xdnd_proxy, AtomEnum::WINDOW, 0, 1)
            .map_err(x11_error)?
            .reply()
        {
            Ok(property) => property,
            // A stale proxy left behind after its owner crashes must be ignored.
            Err(ReplyError::X11Error(error)) if error.error_kind == ErrorKind::Window => {
                return Ok(None);
            }
            Err(error) => return Err(x11_error(error)),
        };
        if property.value_len != 1 || property.bytes_after != 0 {
            return Ok(None);
        }
        Ok(property.value32().and_then(|mut values| values.next()))
    }

    fn send_enter<C: Connection>(
        &self,
        conn: &C,
        target: XdndTarget,
    ) -> Result<(), X11SessionError> {
        let targets = self.offered_targets();
        let enter = self.atoms.mime_targets().enter_targets(&targets);
        conn.change_property32(
            PropMode::REPLACE,
            self.source_window,
            self.atoms.xdnd_type_list,
            AtomEnum::ATOM,
            &targets,
        )
        .map_err(x11_error)?;
        self.send_client_message(
            conn,
            target,
            self.atoms.xdnd_enter,
            [
                self.source_window,
                (target.version << 24) | 1,
                enter[0],
                enter[1],
                enter[2],
            ],
        )
    }

    fn send_position<C: Connection>(
        &mut self,
        conn: &C,
        target: XdndTarget,
        root_x: i16,
        root_y: i16,
        time: u32,
    ) -> Result<(), X11SessionError> {
        if self.awaiting_status {
            self.queued_position =
                (self.last_position != Some((root_x, root_y))).then_some((root_x, root_y, time));
            return Ok(());
        }
        if self.last_position == Some((root_x, root_y)) {
            return Ok(());
        }
        let xy = ((root_x as u32) << 16) | u32::from(root_y as u16);
        self.send_client_message(
            conn,
            target,
            self.atoms.xdnd_position,
            [self.source_window, 0, xy, time, self.atoms.xdnd_action_copy],
        )?;
        // Status has no sequence number: keep only one position in flight.
        self.awaiting_status = true;
        self.position_deadline = None;
        self.status_deadline = Some(Instant::now() + Self::STATUS_TIMEOUT);
        self.last_position = Some((root_x, root_y));
        self.target_acceptance = TargetAcceptance::Unknown;
        Ok(())
    }

    fn leave_current_target<C: Connection>(&mut self, conn: &C) -> Result<(), X11SessionError> {
        if let Some(target) = self.current_target.take() {
            self.send_client_message(
                conn,
                target,
                self.atoms.xdnd_leave,
                [self.source_window, 0, 0, 0, 0],
            )?;
        }
        self.target_acceptance = TargetAcceptance::Unknown;
        self.awaiting_status = false;
        self.status_deadline = None;
        self.position_deadline = None;
        self.last_position = None;
        self.queued_position = None;
        self.payload_target = None;
        Ok(())
    }

    fn send_client_message<C: Connection>(
        &self,
        conn: &C,
        target: XdndTarget,
        message_type: Atom,
        data: [u32; 5],
    ) -> Result<(), X11SessionError> {
        let event = ClientMessageEvent::new(32, target.logical, message_type, data);
        conn.send_event(false, target.recipient, EventMask::NO_EVENT, event)
            .map_err(x11_error)?;
        conn.flush().map_err(x11_error)
    }

    fn offered_targets(&self) -> Vec<Atom> {
        self.atoms.mime_targets().offered_targets()
    }

    fn is_payload_target(&self, target: Atom) -> bool {
        self.offered_targets().contains(&target)
    }

    fn mark_transfer_ready(&mut self) {
        if self.transfer_complete {
            return;
        }
        self.transfer_complete = true;
        self.reporter.transfer_ready();
    }

    fn finish<C: Connection>(
        &mut self,
        conn: &C,
        outcome: LinuxOutcome,
    ) -> Result<(), X11SessionError> {
        self.release_pointer(conn);
        if let Some(preview) = self.preview.take() {
            preview.destroy(conn);
        }
        let selection = self.release_selection(conn);
        self.finished = true;
        self.destroy_source_windows(conn);
        self.reporter.finish_linux(outcome);
        selection.and(conn.flush().map_err(x11_error))
    }
}

fn create_source_window<C: Connection>(
    conn: &C,
    root: XWindow,
) -> Result<XWindow, X11SessionError> {
    let source = conn.generate_id().map_err(x11_error)?;
    conn.create_window(
        0,
        source,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )
    .map_err(x11_error)?
    .check()
    .map_err(x11_error)?;
    Ok(source)
}

impl Drop for XdndSession {
    fn drop(&mut self) {
        if !self.finished {
            self.reporter.finish_linux(LinuxOutcome::Cancelled);
        }
    }
}

fn atom<C: Connection>(conn: &C, name: &[u8]) -> Result<Atom, X11SessionError> {
    conn.intern_atom(false, name)
        .map_err(x11_error)?
        .reply()
        .map(|reply| reply.atom)
        .map_err(x11_error)
}

fn x11_error(error: impl fmt::Display) -> X11SessionError {
    X11SessionError::new(error.to_string())
}
