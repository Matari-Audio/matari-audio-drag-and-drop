//! Matari's toolkit-neutral drag-and-drop API.
//!
//! Integrations own one [`Controller`] per editor and implement
//! [`ToolkitAdapter`] at their live window callback. Native protocol runtimes
//! consume owned start tickets and report lifecycle events back to that
//! controller; no process-global queue or compatibility bridge is involved.

use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use raw_window_handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawWindowHandle, WindowHandle,
};

/// A validated, non-empty snapshot of readable regular files.
#[derive(Debug)]
pub struct FileSet {
    paths: Box<[PathBuf]>,
    preview: Option<crate::DragPreview>,
}

impl FileSet {
    /// Validate and own a set of file-backed assets.
    ///
    /// Validation proves that every path is absolute, identifies a regular
    /// file, and can be opened for reading at construction time. Native
    /// backends must still handle later filesystem changes.
    pub fn try_from_paths(paths: impl IntoIterator<Item = PathBuf>) -> Result<Self, FileSetError> {
        let paths = paths.into_iter().collect::<Vec<_>>();
        if paths.is_empty() {
            return Err(FileSetError::Empty);
        }

        for path in &paths {
            if !path.is_absolute() {
                return Err(FileSetError::Relative(path.clone()));
            }
            let metadata = path.metadata().map_err(|source| FileSetError::Metadata {
                path: path.clone(),
                source,
            })?;
            if !metadata.is_file() {
                return Err(FileSetError::NotFile(path.clone()));
            }
            File::open(path).map_err(|source| FileSetError::Unreadable {
                path: path.clone(),
                source,
            })?;
        }

        Ok(Self {
            paths: paths.into_boxed_slice(),
            preview: None,
        })
    }

    /// Attach source-side preview data to the file set.
    #[must_use]
    pub fn with_preview(mut self, preview: crate::DragPreview) -> Self {
        self.preview = Some(preview);
        self
    }

    /// Validated paths.
    #[must_use]
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Iterate over validated paths without exposing the backing collection.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Path> {
        self.paths.iter().map(PathBuf::as_path)
    }

    /// Build the standard cross-desktop file MIME payloads for a native runtime.
    #[must_use]
    pub fn offers(&self) -> Vec<crate::FileDragOffer> {
        crate::FileDragPayloadData::from_validated(self.paths.to_vec()).offers()
    }

    /// Source-side preview data, when supplied by the application.
    #[must_use]
    pub fn preview(&self) -> Option<&crate::DragPreview> {
        self.preview.as_ref()
    }

    /// Consume the validated paths.
    #[must_use]
    pub fn into_paths(self) -> Vec<PathBuf> {
        self.paths.into_vec()
    }

    pub(crate) fn into_parts(self) -> (Vec<PathBuf>, Option<crate::DragPreview>) {
        (self.paths.into_vec(), self.preview)
    }
}

/// File-set validation failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum FileSetError {
    /// No paths were supplied.
    Empty,
    /// A path was not absolute.
    Relative(PathBuf),
    /// Filesystem metadata could not be read.
    Metadata {
        /// Rejected path.
        path: PathBuf,
        /// Filesystem error.
        source: std::io::Error,
    },
    /// A path did not identify a regular file.
    NotFile(PathBuf),
    /// A regular file could not be opened for reading.
    Unreadable {
        /// Rejected path.
        path: PathBuf,
        /// Filesystem error.
        source: std::io::Error,
    },
}

impl fmt::Display for FileSetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("file set is empty"),
            Self::Relative(path) => {
                write!(formatter, "file path is not absolute: {}", path.display())
            }
            Self::Metadata { path, source } => {
                write!(
                    formatter,
                    "file metadata is unavailable for {}: {source}",
                    path.display()
                )
            }
            Self::NotFile(path) => {
                write!(formatter, "path is not a regular file: {}", path.display())
            }
            Self::Unreadable { path, source } => {
                write!(
                    formatter,
                    "file is not readable: {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl Error for FileSetError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Metadata { source, .. } | Self::Unreadable { source, .. } => Some(source),
            Self::Empty | Self::Relative(_) | Self::NotFile(_) => None,
        }
    }
}

/// Outbound session marker.
#[derive(Debug)]
pub enum Outbound {}

/// Inbound session marker.
#[derive(Debug)]
pub enum Inbound {}

/// Direction-typed drag session identity.
pub struct SessionId<D> {
    raw: NonZeroU64,
    direction: PhantomData<fn() -> D>,
}

impl<D> SessionId<D> {
    fn new(raw: NonZeroU64) -> Self {
        Self {
            raw,
            direction: PhantomData,
        }
    }

    /// Controller-local numeric identity.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.raw.get()
    }
}

impl<D> Copy for SessionId<D> {}

impl<D> Clone for SessionId<D> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<D> fmt::Debug for SessionId<D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("SessionId").field(&self.raw).finish()
    }
}

impl<D> PartialEq for SessionId<D> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<D> Eq for SessionId<D> {}

impl<D> Hash for SessionId<D> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

/// Native protocol used by one exact route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NativeProtocol {
    /// Windows OLE drag-and-drop.
    Ole,
    /// macOS AppKit dragging.
    AppKit,
    /// X11 XDND.
    Xdnd,
    /// Native Wayland data-device.
    WaylandDataDevice,
}

/// Native source context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SourceContext {
    /// Host-embedded Win32 editor.
    EmbeddedWin32,
    /// Detached Win32 editor.
    DetachedWin32,
    /// Host-embedded AppKit editor.
    EmbeddedAppKit,
    /// Detached AppKit editor.
    DetachedAppKit,
    /// Host-embedded X11 or XWayland editor.
    EmbeddedX11,
    /// Detached X11 or XWayland editor.
    DetachedX11,
    /// Native Wayland surface.
    Wayland,
}

/// Route selected for a live session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionRoute {
    /// Native protocol.
    pub protocol: NativeProtocol,
    /// Native source.
    pub source: SourceContext,
}

/// Stable failure stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureStage {
    /// Adapter did not schedule the owned ticket.
    Adapter,
    /// Native setup failed before commitment.
    Start,
    /// Native transfer failed after commitment.
    Transfer,
}

/// Stable failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureKind {
    /// Toolkit abandoned an owned start ticket.
    AdapterAbandoned,
    /// Native backend rejected the operation.
    NativeRejected,
    /// Native backend failed.
    NativeFailure,
}

/// Typed session failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionFailure {
    /// Stage that failed.
    pub stage: FailureStage,
    /// Stable failure category.
    pub kind: FailureKind,
    /// Native status code when the platform exposes one (for example the
    /// Windows `DoDragDrop` `HRESULT` on a transfer failure).
    pub native_code: Option<i32>,
    /// Native drop effect when the platform reports one (Windows
    /// `DROPEFFECT` bits on a transfer failure).
    pub native_effect: Option<u32>,
}

/// Native drag-preview state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewStatus {
    /// The native drag protocol accepted the preview.
    Attached,
    /// The file drag continues without a preview.
    Unavailable {
        /// Stable failing operation.
        stage: PreviewFailureStage,
        /// Native status code when the platform exposes one.
        native_code: Option<i32>,
    },
}

/// Stable drag-preview failure stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewFailureStage {
    /// Native image allocation failed.
    Bitmap,
    /// Native drag-image helper creation failed.
    Helper,
    /// The native protocol rejected the image.
    Attach,
    /// A live X11 preview could not follow the pointer.
    Move,
    /// A live X11 preview could not redraw.
    Redraw,
}

/// Terminal session outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Outcome {
    /// Target copied the files.
    Copied,
    /// User or target cancelled.
    Cancelled,
    /// Target rejected the files.
    Rejected,
    /// Linux-specific terminal result.
    Linux(LinuxOutcome),
    /// A typed failure occurred.
    Failed(SessionFailure),
}

/// Which Linux peer rejected a transfer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxRejector {
    /// The destination application rejected the offer.
    Target,
    /// The compositor rejected the source or operation.
    Compositor,
}

/// Linux setup failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxFailure {
    /// The required protocol is unavailable in this session.
    Unsupported,
    /// Protocol setup failed before a transfer could start.
    Setup,
}

/// Typed terminal result from an X11, XWayland, or Wayland runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LinuxOutcome {
    /// The destination successfully imported the exported files.
    Exported,
    /// A peer rejected the transfer.
    Rejected(LinuxRejector),
    /// The route was unsupported or could not be set up.
    Failed(LinuxFailure),
    /// The user or protocol cancelled the transfer.
    Cancelled,
    /// Data was transferred, but the target never confirmed its final action.
    Indeterminate,
}

/// Typed controller event.
#[derive(Debug)]
#[non_exhaustive]
pub enum SessionEvent {
    /// Native outbound session committed.
    OutboundStarted {
        /// Session identity.
        session: SessionId<Outbound>,
        /// Selected route.
        route: SessionRoute,
    },
    /// Native drag-preview attachment state.
    Preview {
        /// Session identity.
        session: SessionId<Outbound>,
        /// Selected native route.
        route: SessionRoute,
        /// Preview state.
        status: PreviewStatus,
    },
    /// Target requested transfer data.
    DataRequested {
        /// Session identity.
        session: SessionId<Outbound>,
    },
    /// Native protocol reported a performed drop.
    DropPerformed {
        /// Session identity.
        session: SessionId<Outbound>,
    },
    /// Single durable outbound terminal event.
    OutboundTerminal {
        /// Session identity.
        session: SessionId<Outbound>,
        /// Terminal outcome.
        outcome: Outcome,
    },
    /// Inbound file offer entered the editor.
    InboundOffered {
        /// Session identity.
        session: SessionId<Inbound>,
        /// Native route.
        route: SessionRoute,
    },
    /// Accepted inbound files were dropped.
    InboundReceived {
        /// Session identity.
        session: SessionId<Inbound>,
        /// Validated files.
        files: FileSet,
    },
    /// Inbound offer was rejected.
    InboundRejected {
        /// Session identity.
        session: SessionId<Inbound>,
        /// Stable rejection reason.
        reason: RejectReason,
    },
}

/// Owned events from one controller update.
#[derive(Debug, Default)]
pub struct Update {
    events: Box<[SessionEvent]>,
}

impl Update {
    /// Borrow all events.
    #[must_use]
    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    /// Consume the update.
    pub fn into_events(self) -> impl ExactSizeIterator<Item = SessionEvent> {
        self.events.into_vec().into_iter()
    }
}

#[derive(Debug)]
enum TicketEvent {
    Started {
        session: SessionId<Outbound>,
        route: SessionRoute,
    },
    Preview {
        session: SessionId<Outbound>,
        route: SessionRoute,
        status: PreviewStatus,
    },
    DataRequested(SessionId<Outbound>),
    DropPerformed(SessionId<Outbound>),
    TransferReady(SessionId<Outbound>),
    Terminal {
        session: SessionId<Outbound>,
        outcome: Outcome,
    },
}

struct ReporterDelivery {
    events: EventPort,
    state: Mutex<DeliveryState>,
}

#[derive(Clone)]
struct EventPort {
    sender: mpsc::Sender<TicketEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl EventPort {
    fn send(&self, event: TicketEvent) {
        let sent = self.send_without_wake(event);
        self.wake_if(sent);
    }

    fn send_without_wake(&self, event: TicketEvent) -> bool {
        self.sender.send(event).is_ok()
    }

    fn wake_if(&self, sent: bool) {
        if sent {
            (self.wake)();
        }
    }
}

enum DeliveryState {
    Pending(Vec<TicketEvent>),
    Committed,
    Aborted,
}

impl ReporterDelivery {
    fn new(events: EventPort) -> Self {
        Self {
            events,
            state: Mutex::new(DeliveryState::Pending(Vec::new())),
        }
    }

    fn emit(&self, event: TicketEvent) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sent = match &mut *state {
            DeliveryState::Pending(pending) => {
                pending.push(event);
                false
            }
            DeliveryState::Committed => self.events.send_without_wake(event),
            DeliveryState::Aborted => false,
        };
        drop(state);
        self.events.wake_if(sent);
    }

    fn commit(&self, started: TicketEvent) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let DeliveryState::Pending(pending) = &mut *state else {
            return;
        };
        let mut sent = self.events.send_without_wake(started);
        for event in pending.drain(..) {
            sent |= self.events.send_without_wake(event);
        }
        *state = DeliveryState::Committed;
        drop(state);
        self.events.wake_if(sent);
    }

    fn abort(&self) {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = DeliveryState::Aborted;
    }
}

/// Owned, non-cloneable authority to start exactly one outbound session.
#[must_use = "a start ticket must be started, scheduled, or explicitly rejected"]
pub struct StartTicket {
    inner: Option<TicketInner>,
}

struct TicketInner {
    session: SessionId<Outbound>,
    files: FileSet,
    route: SessionRoute,
    events: EventPort,
}

impl StartTicket {
    /// Commit through a native runtime using a live raw-window-handle origin.
    ///
    /// Native Wayland must use [`Self::start_wayland`] so the toolkit's
    /// existing queue supplies the exact initiating press.
    pub fn start<R>(
        mut self,
        runtime: &mut R,
        origin: DragOrigin<'_>,
    ) -> Result<(), ProtocolError<R::Error>>
    where
        R: NativeRuntimePort,
    {
        let Some(inner) = self.inner.take() else {
            return Err(ProtocolError::ConsumedTicket);
        };
        let terminal = Arc::new(AtomicBool::new(false));
        let delivery = Arc::new(ReporterDelivery::new(inner.events.clone()));
        let reporter = SessionReporter {
            session: inner.session,
            route: inner.route,
            terminal: Arc::clone(&terminal),
            delivery: Arc::clone(&delivery),
        };
        match runtime.start_drag(origin, inner.files, reporter) {
            Ok(route) if route == inner.route => {
                delivery.commit(TicketEvent::Started {
                    session: inner.session,
                    route,
                });
                Ok(())
            }
            Ok(actual) => {
                delivery.abort();
                terminal.store(true, Ordering::Release);
                send_terminal(
                    &inner.events,
                    inner.session,
                    FailureStage::Start,
                    FailureKind::NativeRejected,
                );
                Err(ProtocolError::RouteMismatch {
                    selected: inner.route,
                    actual,
                })
            }
            Err(source) => {
                delivery.abort();
                terminal.store(true, Ordering::Release);
                send_terminal(
                    &inner.events,
                    inner.session,
                    FailureStage::Start,
                    FailureKind::NativeFailure,
                );
                Err(ProtocolError::Runtime(source))
            }
        }
    }

    /// Commit the adapter-selected route for an X11 or XWayland editor.
    ///
    /// Adapters select the compositor-compatible protocol via
    /// [`crate::x11_outbound_protocol`]. Other sessions select XDND on
    /// the supplied X11 event queue.
    #[cfg(all(target_family = "unix", not(target_os = "macos")))]
    pub fn start_x11<C>(
        mut self,
        connection: &C,
        origin: DragOrigin<'_>,
        press: crate::X11PointerEvent,
    ) -> Result<crate::X11Session, crate::X11StartError>
    where
        C: x11rb::connection::Connection,
    {
        let Some(inner) = self.inner.take() else {
            return Err(crate::X11SessionError::new(
                "start ticket was already consumed",
            ));
        };
        let route_is_xdnd = inner.route.protocol == NativeProtocol::Xdnd
            && matches!(
                inner.route.source,
                SourceContext::EmbeddedX11 | SourceContext::DetachedX11
            );
        let route_is_serialless_wayland = inner.route.protocol == NativeProtocol::WaylandDataDevice
            && matches!(
                inner.route.source,
                SourceContext::EmbeddedX11 | SourceContext::DetachedX11
            );
        if !route_is_xdnd && !route_is_serialless_wayland {
            send_terminal(
                &inner.events,
                inner.session,
                FailureStage::Start,
                FailureKind::NativeRejected,
            );
            return Err(crate::X11SessionError::new(
                "selected route is not valid for an X11 or XWayland editor",
            ));
        }

        let terminal = Arc::new(AtomicBool::new(false));
        let delivery = Arc::new(ReporterDelivery::new(inner.events.clone()));
        let reporter = SessionReporter {
            session: inner.session,
            route: inner.route,
            terminal: Arc::clone(&terminal),
            delivery: Arc::clone(&delivery),
        };
        match crate::X11Session::start(
            connection,
            origin,
            inner.files,
            reporter,
            press,
            inner.route,
        ) {
            Ok(session) => {
                let route = session.route();
                delivery.commit(TicketEvent::Started {
                    session: inner.session,
                    route,
                });
                Ok(session)
            }
            Err(error) => {
                delivery.abort();
                terminal.store(true, Ordering::Release);
                send_terminal(
                    &inner.events,
                    inner.session,
                    FailureStage::Start,
                    FailureKind::NativeFailure,
                );
                Err(error)
            }
        }
    }

    /// Commit native Wayland through the toolkit's existing event queue.
    ///
    /// The consumed runtime must own the initiating pointer authority and the
    /// live surface, seat, connection, and queue that produced it. It must
    /// retain the supplied reporter until the native session reaches one
    /// terminal outcome.
    pub fn start_wayland<R>(mut self, runtime: R) -> Result<(), WaylandStartError<R::Error>>
    where
        R: WaylandRuntimePort,
    {
        let Some(inner) = self.inner.take() else {
            return Err(WaylandStartError::ConsumedTicket);
        };
        let terminal = Arc::new(AtomicBool::new(false));
        let delivery = Arc::new(ReporterDelivery::new(inner.events.clone()));
        let reporter = SessionReporter {
            session: inner.session,
            route: inner.route,
            terminal: Arc::clone(&terminal),
            delivery: Arc::clone(&delivery),
        };
        match runtime.start_drag(inner.files, reporter) {
            Ok(route) if route == inner.route => {
                delivery.commit(TicketEvent::Started {
                    session: inner.session,
                    route,
                });
                Ok(())
            }
            Ok(actual) => {
                delivery.abort();
                terminal.store(true, Ordering::Release);
                send_terminal(
                    &inner.events,
                    inner.session,
                    FailureStage::Start,
                    FailureKind::NativeRejected,
                );
                Err(WaylandStartError::RouteMismatch {
                    selected: inner.route,
                    actual,
                })
            }
            Err(source) => {
                delivery.abort();
                terminal.store(true, Ordering::Release);
                send_terminal(
                    &inner.events,
                    inner.session,
                    FailureStage::Start,
                    FailureKind::NativeFailure,
                );
                Err(WaylandStartError::Runtime(source))
            }
        }
    }

    /// Return this exact ticket when an adapter cannot schedule it.
    #[must_use]
    pub fn reject<E>(self, error: E) -> RejectedStart<E> {
        RejectedStart {
            ticket: self,
            error,
        }
    }

    /// Route selected by the controller for this ticket.
    #[must_use]
    pub fn route(&self) -> Option<SessionRoute> {
        self.inner.as_ref().map(|inner| inner.route)
    }

    fn disarm(&mut self) {
        self.inner = None;
    }
}

impl fmt::Debug for StartTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StartTicket")
            .field("pending", &self.inner.is_some())
            .finish()
    }
}

impl Drop for StartTicket {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        send_terminal(
            &inner.events,
            inner.session,
            FailureStage::Adapter,
            FailureKind::AdapterAbandoned,
        );
    }
}

/// Rejected start retaining the controller's original ticket.
#[derive(Debug)]
pub struct RejectedStart<E> {
    ticket: StartTicket,
    error: E,
}

impl<E> RejectedStart<E> {
    /// Adapter error.
    #[must_use]
    pub fn error(&self) -> &E {
        &self.error
    }

    /// Transform the adapter error while retaining the original start ticket.
    #[must_use]
    pub fn map_error<F>(self, map: impl FnOnce(E) -> F) -> RejectedStart<F> {
        RejectedStart {
            ticket: self.ticket,
            error: map(self.error),
        }
    }

    fn into_parts(self) -> (StartTicket, E) {
        (self.ticket, self.error)
    }
}

/// Short-lived native drag origin.
pub struct DragOrigin<'a> {
    display: DisplayHandle<'a>,
    window: WindowHandle<'a>,
    activation: Activation,
}

#[derive(Clone, Copy)]
enum Activation {
    Window,
    AppKit(NonNull<c_void>),
}

impl DragOrigin<'_> {
    /// Build an origin for routes where live window handles are sufficient.
    pub fn from_window<W>(window: &W) -> Result<DragOrigin<'_>, OriginError>
    where
        W: HasDisplayHandle + HasWindowHandle + ?Sized,
    {
        let display = window.display_handle().map_err(OriginError::Handle)?;
        let native = window.window_handle().map_err(OriginError::Handle)?;
        match native.as_raw() {
            RawWindowHandle::AppKit(_) => Err(OriginError::MissingAppKitEvent),
            RawWindowHandle::Wayland(_) => Err(OriginError::NativeWaylandUsesRuntime),
            RawWindowHandle::Win32(_)
            | RawWindowHandle::WinRt(_)
            | RawWindowHandle::Xlib(_)
            | RawWindowHandle::Xcb(_) => Ok(DragOrigin {
                display,
                window: native,
                activation: Activation::Window,
            }),
            _ => Err(OriginError::UnsupportedWindow),
        }
    }

    /// Build an AppKit origin from the initiating native event.
    ///
    /// # Safety
    ///
    /// `event` must be a live `NSEvent` from the same AppKit main-thread
    /// callback and editor as `window`, and remain valid until
    /// [`StartTicket::start`] returns.
    pub unsafe fn with_appkit_event<W>(
        window: &W,
        event: NonNull<c_void>,
    ) -> Result<DragOrigin<'_>, OriginError>
    where
        W: HasDisplayHandle + HasWindowHandle + ?Sized,
    {
        let display = window.display_handle().map_err(OriginError::Handle)?;
        let native = window.window_handle().map_err(OriginError::Handle)?;
        if !matches!(native.as_raw(), RawWindowHandle::AppKit(_)) {
            return Err(OriginError::UnsupportedWindow);
        }
        Ok(DragOrigin {
            display,
            window: native,
            activation: Activation::AppKit(event),
        })
    }

    /// Live display handle.
    #[must_use]
    pub const fn display_handle(&self) -> DisplayHandle<'_> {
        self.display
    }

    /// Live window handle.
    #[must_use]
    pub const fn window_handle(&self) -> WindowHandle<'_> {
        self.window
    }

    /// Initiating AppKit event when this is an AppKit origin.
    #[must_use]
    pub const fn appkit_event(&self) -> Option<NonNull<c_void>> {
        match self.activation {
            Activation::AppKit(event) => Some(event),
            Activation::Window => None,
        }
    }
}

impl fmt::Debug for DragOrigin<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DragOrigin")
            .field("window", &self.window.as_raw())
            .finish_non_exhaustive()
    }
}

/// Drag-origin construction failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum OriginError {
    /// Raw-window-handle access failed.
    Handle(raw_window_handle::HandleError),
    /// AppKit requires the initiating `NSEvent`.
    MissingAppKitEvent,
    /// Native Wayland must start through an event-scoped runtime port.
    NativeWaylandUsesRuntime,
    /// The window backend is not supported.
    UnsupportedWindow,
}

impl fmt::Display for OriginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Handle(source) => write!(formatter, "native handle is unavailable: {source}"),
            Self::MissingAppKitEvent => {
                formatter.write_str("AppKit drag origin is missing NSEvent")
            }
            Self::NativeWaylandUsesRuntime => {
                formatter.write_str("native Wayland drag requires an event-scoped runtime")
            }
            Self::UnsupportedWindow => formatter.write_str("window backend is unsupported"),
        }
    }
}

impl Error for OriginError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

/// Event reporter retained by a native protocol runtime.
///
/// A runtime must call exactly one terminal method. Repeated terminal calls
/// are ignored.
#[derive(Clone)]
pub struct SessionReporter {
    session: SessionId<Outbound>,
    route: SessionRoute,
    terminal: Arc<AtomicBool>,
    delivery: Arc<ReporterDelivery>,
}

impl SessionReporter {
    /// Report the non-terminal native preview state.
    pub fn preview(&self, status: PreviewStatus) {
        if !self.terminal.load(Ordering::Acquire) {
            self.delivery.emit(TicketEvent::Preview {
                session: self.session,
                route: self.route,
                status,
            });
        }
    }

    /// Report that the target requested file data.
    pub fn data_requested(&self) {
        if !self.terminal.load(Ordering::Acquire) {
            self.delivery.emit(TicketEvent::DataRequested(self.session));
        }
    }

    /// Report native drop-performed.
    pub fn drop_performed(&self) {
        if !self.terminal.load(Ordering::Acquire) {
            self.delivery.emit(TicketEvent::DropPerformed(self.session));
        }
    }

    /// Report that accepted protocol state and transferred data make this
    /// session replaceable while the native target retains final authority.
    pub fn transfer_ready(&self) {
        if !self.terminal.load(Ordering::Acquire) {
            self.delivery.emit(TicketEvent::TransferReady(self.session));
        }
    }

    /// Finish a Linux session with a typed terminal result.
    pub fn finish_linux(&self, outcome: LinuxOutcome) {
        self.finish(Outcome::Linux(outcome));
    }

    /// Finish the session once.
    pub fn finish(&self, outcome: Outcome) {
        if !self.terminal.swap(true, Ordering::AcqRel) {
            self.delivery.emit(TicketEvent::Terminal {
                session: self.session,
                outcome,
            });
        }
    }
}

impl fmt::Debug for SessionReporter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionReporter")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// Reporter retained by a non-Wayland native runtime.
pub type NativeReporter = SessionReporter;

/// Reporter retained by a native Wayland runtime.
pub type WaylandReporter = SessionReporter;

/// Native protocol runtime owned by a toolkit or platform adapter.
pub trait NativeRuntimePort {
    /// Native start failure.
    type Error: Error + Send + Sync + 'static;

    /// Start from the exact live origin supplied by the toolkit callback.
    ///
    /// On success, retain `reporter` until one terminal outcome. Synchronous
    /// runtimes may report before returning; the controller buffers those
    /// events until the start commits.
    fn start_drag(
        &mut self,
        origin: DragOrigin<'_>,
        files: FileSet,
        reporter: NativeReporter,
    ) -> Result<SessionRoute, Self::Error>;
}

/// Built-in OLE, AppKit, or XDND runtime selected from the live window origin.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeRuntime;

impl NativeRuntimePort for NativeRuntime {
    type Error = NativeStartError;

    fn start_drag(
        &mut self,
        origin: DragOrigin<'_>,
        files: FileSet,
        reporter: NativeReporter,
    ) -> Result<SessionRoute, Self::Error> {
        crate::backend::start_reported_file_drag(origin, files, reporter)
    }
}

/// Failure to start a built-in native runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeStartError {
    message: Box<str>,
}

impl NativeStartError {
    pub(crate) fn new(message: impl Into<Box<str>>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for NativeStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for NativeStartError {}

/// Existing native Wayland runtime owned by a toolkit/surface system.
pub trait WaylandRuntimePort {
    /// Native start failure.
    type Error: Error + Send + Sync + 'static;

    /// Start on this runtime's current queue.
    ///
    /// The adapter is consumed because it owns the exact initiating pointer
    /// authority for this attempt. On success, retain `reporter` until one
    /// terminal outcome. Events reported before this method returns are
    /// buffered until the start commits.
    fn start_drag(
        self,
        files: FileSet,
        reporter: WaylandReporter,
    ) -> Result<SessionRoute, Self::Error>;
}

/// Native Wayland start failure.
#[derive(Debug)]
pub enum WaylandStartError<E> {
    /// Ticket was already consumed.
    ConsumedTicket,
    /// Runtime started a route other than the controller-selected route.
    RouteMismatch {
        /// Controller-selected route.
        selected: SessionRoute,
        /// Runtime-returned route.
        actual: SessionRoute,
    },
    /// Runtime rejected the start.
    Runtime(E),
}

impl<E: fmt::Display> fmt::Display for WaylandStartError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConsumedTicket => formatter.write_str("start ticket was already consumed"),
            Self::RouteMismatch { selected, actual } => {
                write!(
                    formatter,
                    "Wayland runtime route mismatch: selected {selected:?}, got {actual:?}"
                )
            }
            Self::Runtime(source) => write!(formatter, "Wayland runtime rejected drag: {source}"),
        }
    }
}

impl<E: Error + 'static> Error for WaylandStartError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Runtime(source) => Some(source),
            Self::ConsumedTicket | Self::RouteMismatch { .. } => None,
        }
    }
}

/// Native protocol start failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum ProtocolError<E> {
    /// Ticket was already consumed.
    ConsumedTicket,
    /// Runtime started a route other than the controller-selected route.
    RouteMismatch {
        /// Controller-selected route.
        selected: SessionRoute,
        /// Runtime-returned route.
        actual: SessionRoute,
    },
    /// Runtime rejected the start.
    Runtime(E),
}

impl<E: fmt::Display> fmt::Display for ProtocolError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConsumedTicket => formatter.write_str("start ticket was already consumed"),
            Self::RouteMismatch { selected, actual } => {
                write!(
                    formatter,
                    "native runtime route mismatch: selected {selected:?}, got {actual:?}"
                )
            }
            Self::Runtime(source) => write!(formatter, "native runtime rejected drag: {source}"),
        }
    }
}

impl<E: Error + 'static> Error for ProtocolError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Runtime(source) => Some(source),
            Self::ConsumedTicket | Self::RouteMismatch { .. } => None,
        }
    }
}

/// Synchronous inbound decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InboundDecision {
    /// Accept with copy semantics.
    AcceptCopy,
    /// Reject with a stable reason.
    Reject(RejectReason),
}

/// Stable inbound rejection reason.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RejectReason {
    /// Offer did not contain files.
    UnsupportedOffer,
    /// Product policy rejected the files.
    ProductPolicy,
    /// Dropped files failed validation.
    InvalidFiles,
    /// Offer left or was cancelled.
    Cancelled,
}

/// Immediate native event disposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboundDisposition {
    /// Event was not a drag-and-drop event.
    Unhandled,
    /// Native callback must accept copy.
    AcceptCopy,
    /// Native callback must reject.
    Reject,
}

/// Borrowed inbound offer.
#[derive(Clone, Copy, Debug)]
pub struct InboundOffer<'a> {
    /// Offered file paths, when already available.
    pub paths: &'a [PathBuf],
    /// Native route.
    pub route: SessionRoute,
}

/// Event-scoped inbound handler driven by a toolkit adapter.
pub trait InboundHandler {
    /// Decide an entered offer exactly once.
    fn decide(&mut self, offer: InboundOffer<'_>) -> InboundDecision;
    /// Deliver dropped files and return the native callback disposition.
    fn dropped(&mut self, files: &[PathBuf]) -> InboundDisposition;
    /// Offer left the editor.
    fn left(&mut self);
    /// Native operation was cancelled.
    fn cancelled(&mut self);
}

/// Toolkit/window adapter contract.
pub trait ToolkitAdapter {
    /// Toolkit adapter failure.
    type Error: Error + Send + Sync + 'static;

    /// Exact outbound route available in the current callback.
    fn outbound_route(&self) -> Option<SessionRoute>;

    /// Schedule or immediately consume an owned outbound start.
    fn schedule_outbound(&mut self, ticket: StartTicket) -> Result<(), RejectedStart<Self::Error>>;

    /// Drive one native inbound event synchronously.
    fn drive_inbound(
        &mut self,
        handler: &mut dyn InboundHandler,
    ) -> Result<InboundDisposition, Self::Error>;
}

/// Error while driving a native inbound event.
#[derive(Debug)]
pub struct InboundError<E> {
    /// Safe disposition to return to the native callback.
    pub disposition: InboundDisposition,
    /// Adapter error.
    pub source: E,
}

impl<E: fmt::Display> fmt::Display for InboundError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "inbound adapter failed: {}", self.source)
    }
}

impl<E: Error + 'static> Error for InboundError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Immediate outbound start failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum StartError<E> {
    /// Another outbound session is active.
    Busy {
        /// Active session.
        active: SessionId<Outbound>,
    },
    /// No available route was reported.
    NoRoute,
    /// Adapter rejected the original ticket before scheduling.
    Adapter(E),
}

impl<E: fmt::Display> fmt::Display for StartError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy { active } => {
                write!(formatter, "outbound session {} is active", active.get())
            }
            Self::NoRoute => formatter.write_str("no outbound drag route is available"),
            Self::Adapter(source) => write!(formatter, "toolkit adapter rejected start: {source}"),
        }
    }
}

impl<E: Error + 'static> Error for StartError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Adapter(source) => Some(source),
            _ => None,
        }
    }
}

/// One editor-owned drag-and-drop controller.
pub struct Controller {
    events: EventPort,
    events_rx: mpsc::Receiver<TicketEvent>,
    pending: Vec<SessionEvent>,
    active_outbound: Option<ActiveOutbound>,
    active_inbound: Option<SessionId<Inbound>>,
    next_session: NonZeroU64,
}

#[derive(Clone, Copy)]
struct ActiveOutbound {
    session: SessionId<Outbound>,
    data_requested: bool,
    drop_performed: bool,
    replaceable: bool,
}

impl Controller {
    /// Create an editor-owned controller.
    #[must_use]
    pub fn new() -> Self {
        Self::with_wake(Arc::new(|| {}))
    }

    /// Create an editor-owned controller that wakes the GUI event loop after
    /// native lifecycle delivery.
    #[must_use]
    pub fn with_wake(wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (events_tx, events_rx) = mpsc::channel();
        Self {
            events: EventPort {
                sender: events_tx,
                wake,
            },
            events_rx,
            pending: Vec::new(),
            active_outbound: None,
            active_inbound: None,
            next_session: NonZeroU64::MIN,
        }
    }

    /// Begin one outbound file-backed session.
    pub fn start_outbound<A: ToolkitAdapter>(
        &mut self,
        adapter: &mut A,
        files: FileSet,
    ) -> Result<SessionId<Outbound>, StartError<A::Error>> {
        if let Some(active) = self.active_outbound
            && !active.replaceable
        {
            return Err(StartError::Busy {
                active: active.session,
            });
        }

        let Some(route) = adapter.outbound_route() else {
            return Err(StartError::NoRoute);
        };

        if let Some(active) = self.active_outbound.take() {
            self.pending.push(SessionEvent::OutboundTerminal {
                session: active.session,
                outcome: Outcome::Linux(LinuxOutcome::Indeterminate),
            });
        }

        let session = SessionId::new(take_session_id(&mut self.next_session));
        let ticket = StartTicket {
            inner: Some(TicketInner {
                session,
                files,
                route,
                events: self.events.clone(),
            }),
        };
        self.active_outbound = Some(ActiveOutbound {
            session,
            data_requested: false,
            drop_performed: false,
            replaceable: false,
        });

        match adapter.schedule_outbound(ticket) {
            Ok(()) => Ok(session),
            Err(rejected) => {
                let (mut ticket, error) = rejected.into_parts();
                ticket.disarm();
                self.active_outbound = None;
                Err(StartError::Adapter(error))
            }
        }
    }

    /// Drive one event-scoped inbound offer.
    pub fn handle_inbound<A, D>(
        &mut self,
        adapter: &mut A,
        mut decide: D,
    ) -> Result<InboundDisposition, InboundError<A::Error>>
    where
        A: ToolkitAdapter,
        D: for<'a> FnMut(&InboundOffer<'a>) -> InboundDecision,
    {
        let mut handler = ControllerInbound {
            active: &mut self.active_inbound,
            pending: &mut self.pending,
            decide: &mut decide,
            next_session: &mut self.next_session,
        };
        adapter
            .drive_inbound(&mut handler)
            .map_err(|source| InboundError {
                disposition: InboundDisposition::Reject,
                source,
            })
    }

    /// Drain owned events reported by this controller's protocol runtimes.
    pub fn update(&mut self) -> Update {
        while let Ok(event) = self.events_rx.try_recv() {
            self.accept_ticket_event(event);
        }
        Update {
            events: std::mem::take(&mut self.pending).into_boxed_slice(),
        }
    }

    /// Current outbound session.
    #[must_use]
    pub fn active_outbound(&self) -> Option<SessionId<Outbound>> {
        self.active_outbound.map(|active| active.session)
    }

    /// Whether a native outbound session must block a new drag gesture.
    #[must_use]
    pub fn outbound_in_flight(&self) -> bool {
        self.active_outbound
            .is_some_and(|active| !active.replaceable)
    }

    fn accept_ticket_event(&mut self, event: TicketEvent) {
        match event {
            TicketEvent::Started { session, route } if self.is_live(session) => {
                self.pending
                    .push(SessionEvent::OutboundStarted { session, route });
            }
            TicketEvent::Preview {
                session,
                route,
                status,
            } if self.is_live(session) => {
                self.pending.push(SessionEvent::Preview {
                    session,
                    route,
                    status,
                });
            }
            TicketEvent::DataRequested(session) => {
                if let Some(active) = &mut self.active_outbound
                    && active.session == session
                    && !active.data_requested
                {
                    active.data_requested = true;
                    self.pending.push(SessionEvent::DataRequested { session });
                }
            }
            TicketEvent::DropPerformed(session) => {
                if let Some(active) = &mut self.active_outbound
                    && active.session == session
                    && !active.drop_performed
                {
                    active.drop_performed = true;
                    self.pending.push(SessionEvent::DropPerformed { session });
                }
            }
            TicketEvent::TransferReady(session) => {
                if let Some(active) = &mut self.active_outbound
                    && active.session == session
                {
                    active.replaceable = true;
                }
            }
            TicketEvent::Terminal { session, outcome } if self.is_live(session) => {
                self.finish_outbound(session, outcome);
            }
            TicketEvent::Started { .. }
            | TicketEvent::Preview { .. }
            | TicketEvent::Terminal { .. } => {}
        }
    }

    fn is_live(&self, session: SessionId<Outbound>) -> bool {
        self.active_outbound
            .is_some_and(|active| active.session == session)
    }

    fn finish_outbound(&mut self, session: SessionId<Outbound>, outcome: Outcome) {
        let Some(active) = self.active_outbound else {
            return;
        };
        if active.session != session {
            return;
        }
        self.pending
            .push(SessionEvent::OutboundTerminal { session, outcome });
        self.active_outbound = None;
    }
}

impl Default for Controller {
    fn default() -> Self {
        Self::new()
    }
}

struct ControllerInbound<'a, D> {
    active: &'a mut Option<SessionId<Inbound>>,
    pending: &'a mut Vec<SessionEvent>,
    decide: &'a mut D,
    next_session: &'a mut NonZeroU64,
}

impl<D> InboundHandler for ControllerInbound<'_, D>
where
    D: for<'a> FnMut(&InboundOffer<'a>) -> InboundDecision,
{
    fn decide(&mut self, offer: InboundOffer<'_>) -> InboundDecision {
        let session = self
            .active
            .get_or_insert_with(|| SessionId::new(take_session_id(self.next_session)))
            .to_owned();
        self.pending.push(SessionEvent::InboundOffered {
            session,
            route: offer.route,
        });
        let decision = (self.decide)(&offer);
        if let InboundDecision::Reject(reason) = decision {
            self.pending
                .push(SessionEvent::InboundRejected { session, reason });
            *self.active = None;
        }
        decision
    }

    fn dropped(&mut self, files: &[PathBuf]) -> InboundDisposition {
        let session = self
            .active
            .take()
            .unwrap_or_else(|| SessionId::new(take_session_id(self.next_session)));
        match FileSet::try_from_paths(files.iter().cloned()) {
            Ok(files) => {
                self.pending
                    .push(SessionEvent::InboundReceived { session, files });
                InboundDisposition::AcceptCopy
            }
            Err(_) => {
                self.pending.push(SessionEvent::InboundRejected {
                    session,
                    reason: RejectReason::InvalidFiles,
                });
                InboundDisposition::Reject
            }
        }
    }

    fn left(&mut self) {
        self.reject_cancelled();
    }

    fn cancelled(&mut self) {
        self.reject_cancelled();
    }
}

impl<D> ControllerInbound<'_, D> {
    fn reject_cancelled(&mut self) {
        if let Some(session) = self.active.take() {
            self.pending.push(SessionEvent::InboundRejected {
                session,
                reason: RejectReason::Cancelled,
            });
        }
    }
}

fn take_session_id(next: &mut NonZeroU64) -> NonZeroU64 {
    let current = *next;
    *next = NonZeroU64::new(current.get().wrapping_add(1)).unwrap_or(NonZeroU64::MIN);
    current
}

fn send_terminal(
    events: &EventPort,
    session: SessionId<Outbound>,
    stage: FailureStage,
    kind: FailureKind,
) {
    events.send(TicketEvent::Terminal {
        session,
        outcome: Outcome::Failed(SessionFailure {
            stage,
            kind,
            native_code: None,
            native_effect: None,
        }),
    });
}
