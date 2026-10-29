//! Serial-less native Wayland drag source for XWayland plugin editors.
//!
//! Hyprland accepts `wl_data_device.start_drag` with serial zero and a
//! role-less origin surface. This lets an XWayland editor use a private
//! process-wide persistent Wayland worker while the compositor owns delivery
//! to both native Wayland and X11 targets. This is a Hyprland compatibility
//! exception, not authorization to start drags on other compositors.

use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use smithay_client_toolkit::{
    data_device_manager::{
        DataDeviceManagerState, WritePipe,
        data_device::{DataDevice, DataDeviceHandler},
        data_offer::{DataOfferHandler, DragOffer},
        data_source::{DataSourceHandler, DragSource},
    },
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken, channel,
            generic::Generic,
        },
        calloop_wayland_source::WaylandSource,
    },
    seat::{Capability, SeatHandler, SeatState},
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    backend::WaylandError,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_compositor::WlCompositor, wl_data_device::WlDataDevice,
        wl_data_device_manager::DndAction, wl_data_source::WlDataSource, wl_registry,
        wl_seat::WlSeat, wl_shm, wl_surface::WlSurface,
    },
};

use crate::{
    FailureKind, FailureStage, FileDragOffer, FileSet, LinuxOutcome, Outcome, PreviewStatus,
    SessionFailure, SessionReporter, WaylandSourceReporter,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WaylandBridgeError {
    message: Box<str>,
}

impl WaylandBridgeError {
    fn new(message: impl Into<Box<str>>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for WaylandBridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for WaylandBridgeError {}

struct PreparedWaylandBridge {
    connection: Connection,
    event_queue: wayland_client::EventQueue<BridgeState>,
    queue: QueueHandle<BridgeState>,
    compositor: WlCompositor,
    manager: DataDeviceManagerState,
    seat_state: SeatState,
    shm: Shm,
    data_device: DataDevice,
    origin: WlSurface,
}

impl PreparedWaylandBridge {
    fn prepare(initialization: &Initialization) -> Result<Self, WaylandBridgeError> {
        let connection = Connection::connect_to_env()
            .map_err(|error| WaylandBridgeError::new(format!("Wayland connect failed: {error}")))?;
        initialization.attach(&connection)?;
        let (globals, event_queue) =
            registry_queue_init::<BridgeState>(&connection).map_err(|error| {
                WaylandBridgeError::new(format!("Wayland registry failed: {error}"))
            })?;
        let queue = event_queue.handle();
        let compositor: WlCompositor = globals
            .bind(&queue, 1..=6, ())
            .map_err(|error| WaylandBridgeError::new(format!("wl_compositor failed: {error}")))?;
        let manager = DataDeviceManagerState::bind(&globals, &queue)
            .map_err(|error| WaylandBridgeError::new(format!("data device failed: {error}")))?;
        let shm = Shm::bind(&globals, &queue)
            .map_err(|error| WaylandBridgeError::new(format!("wl_shm failed: {error}")))?;
        let seat_state = SeatState::new(&globals, &queue);
        let seat = seat_state
            .seats()
            .next()
            .ok_or_else(|| WaylandBridgeError::new("Wayland session has no wl_seat"))?;
        let data_device = manager.get_data_device(&queue, &seat);
        let origin = compositor.create_surface(&queue, ());
        submission_result(connection.flush())?;
        Ok(Self {
            connection,
            event_queue,
            queue,
            compositor,
            manager,
            seat_state,
            shm,
            data_device,
            origin,
        })
    }
}

const START_TIMEOUT: Duration = Duration::from_secs(1);
static NEXT_DRAG_ID: AtomicU64 = AtomicU64::new(1);
static BRIDGE_RUNTIME: Mutex<Option<BridgeRuntime>> = Mutex::new(None);

// The socket is shut down only when startup has been abandoned. In particular,
// cancelling a drag never disconnects the shared live connection.
#[derive(Default)]
struct Initialization {
    cancelled: AtomicBool,
    socket: Mutex<Option<UnixStream>>,
}

impl Initialization {
    fn attach(&self, connection: &Connection) -> Result<(), WaylandBridgeError> {
        let socket =
            UnixStream::from(connection.as_fd().try_clone_to_owned().map_err(|error| {
                WaylandBridgeError::new(format!("Wayland initialization socket failed: {error}"))
            })?);
        let mut slot = self
            .socket
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.cancelled.load(Ordering::Acquire) {
            let _ = socket.shutdown(std::net::Shutdown::Both);
            return Err(WaylandBridgeError::new(
                "Wayland initialization was cancelled",
            ));
        }
        *slot = Some(socket);
        Ok(())
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(socket) = self
            .socket
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }

    fn wait<T>(
        &self,
        receiver: mpsc::Receiver<Result<T, WaylandBridgeError>>,
        timeout: Duration,
    ) -> Result<T, WaylandBridgeError> {
        match receiver.recv_timeout(timeout) {
            Ok(result) => result,
            Err(error) => {
                self.cancel();
                Err(WaylandBridgeError::new(format!(
                    "Wayland drag initialization failed: {error}"
                )))
            }
        }
    }
}

// A failed launch leaves no cache entry; a dead worker is replaced on the next
// gesture. Concurrent initialization fails promptly instead of waiting behind
// another caller's timeout while holding the editor's event thread.
fn cached_runtime<T: Clone>(
    cache: &Mutex<Option<T>>,
    is_alive: impl FnOnce(&T) -> bool,
    launch: impl FnOnce() -> Result<T, WaylandBridgeError>,
) -> Result<T, WaylandBridgeError> {
    let mut slot = match cache.try_lock() {
        Ok(slot) => slot,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            return Err(WaylandBridgeError::new(
                "Wayland drag initialization is already in progress",
            ));
        }
    };
    if let Some(runtime) = slot.as_ref().filter(|runtime| is_alive(runtime)) {
        return Ok(runtime.clone());
    }
    *slot = None;
    let runtime = launch()?;
    *slot = Some(runtime.clone());
    Ok(runtime)
}

struct WorkerLifetime(Arc<AtomicBool>);
impl Drop for WorkerLifetime {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[derive(Clone)]
struct BridgeRuntime {
    commands: channel::Sender<Command>,
    alive: Arc<AtomicBool>,
}

impl BridgeRuntime {
    fn shared() -> Result<Self, WaylandBridgeError> {
        cached_runtime(
            &BRIDGE_RUNTIME,
            |runtime| runtime.alive.load(Ordering::Acquire),
            Self::launch,
        )
    }

    fn launch() -> Result<Self, WaylandBridgeError> {
        let (commands, command_source) = channel::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let initialization = Arc::new(Initialization::default());
        let worker_initialization = Arc::clone(&initialization);
        let alive = Arc::new(AtomicBool::new(true));
        let lifetime = WorkerLifetime(Arc::clone(&alive));
        thread::Builder::new()
            .name("matari-wayland-dnd".to_owned())
            .spawn(move || {
                let _lifetime = lifetime;
                let mut ready = Some(ready_tx);
                let result = (|| {
                    // Registry discovery and its synchronization run here, never
                    // on the editor thread. Abandoning startup shuts this socket
                    // down so registry_queue_init cannot leave a hung worker.
                    let prepared = PreparedWaylandBridge::prepare(&worker_initialization)?;
                    let mut state = BridgeState {
                        connection: prepared.connection.clone(),
                        queue: prepared.queue,
                        compositor: prepared.compositor,
                        manager: prepared.manager,
                        seat_state: prepared.seat_state,
                        shm: prepared.shm,
                        _data_device: prepared.data_device,
                        origin: prepared.origin,
                        active: None,
                        loop_handle: None,
                        flush_retry: None,
                        flush_pending: false,
                    };
                    run_queue(
                        prepared.connection,
                        prepared.event_queue,
                        command_source,
                        &mut ready,
                        &worker_initialization,
                        &mut state,
                    )
                })();
                if let Some(ready) = ready.take() {
                    let _ = ready.send(result);
                }
            })
            .map_err(|error| {
                WaylandBridgeError::new(format!("Wayland drag thread failed: {error}"))
            })?;
        initialization.wait(ready_rx, START_TIMEOUT)?;
        Ok(Self { commands, alive })
    }
}

pub(super) struct WaylandBridgeSession {
    id: u64,
    commands: channel::Sender<Command>,
    reporter: SessionReporter,
    transfer_ready: Arc<AtomicBool>,
    terminal: Arc<AtomicBool>,
}

impl WaylandBridgeSession {
    pub(super) fn start(
        files: FileSet,
        reporter: SessionReporter,
    ) -> Result<Self, WaylandBridgeError> {
        let mut runtime = BridgeRuntime::shared()?;
        let id = NEXT_DRAG_ID.fetch_add(1, Ordering::Relaxed);
        let transfer_ready = Arc::new(AtomicBool::new(false));
        let terminal = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let command = Command::Start {
            id,
            files,
            reporter: reporter.clone(),
            transfer_ready: Arc::clone(&transfer_ready),
            terminal: Arc::clone(&terminal),
            started: started_tx,
        };
        if let Err(error) = runtime.commands.send(command) {
            // Send returned ownership, so this command was never accepted.
            // A disconnect racing the cache liveness check is safe to retry.
            runtime.alive.store(false, Ordering::Release);
            runtime = BridgeRuntime::shared()?;
            runtime.commands.send(error.0).map_err(|error| {
                WaylandBridgeError::new(format!("Wayland drag command failed: {error}"))
            })?;
        }
        // Drop cancels even a queued start if waiting for the worker fails.
        let session = WaylandBridgeSession {
            id,
            commands: runtime.commands.clone(),
            reporter,
            transfer_ready,
            terminal,
        };
        started_rx.recv_timeout(START_TIMEOUT).map_err(|error| {
            WaylandBridgeError::new(format!("Wayland drag start failed: {error}"))
        })??;
        Ok(session)
    }

    pub(super) fn transfer_complete(&self) -> bool {
        self.transfer_ready.load(Ordering::Acquire)
    }

    pub(super) fn is_terminal(&self) -> bool {
        self.terminal.load(Ordering::Acquire)
    }

    fn signal_cancel(&self) {
        let _ = self.commands.send(Command::Cancel { id: self.id });
    }

    fn finish_cancelled(&self) {
        if !self.terminal.swap(true, Ordering::AcqRel) {
            self.reporter.finish_linux(LinuxOutcome::Cancelled);
        }
    }

    pub(super) fn cancel_stale(&self) {
        self.finish_cancelled();
        self.signal_cancel();
    }

    pub(super) fn cancel(self) {
        self.cancel_stale();
    }
}

fn run_queue(
    connection: Connection,
    event_queue: wayland_client::EventQueue<BridgeState>,
    command_source: channel::Channel<Command>,
    ready: &mut Option<mpsc::SyncSender<Result<(), WaylandBridgeError>>>,
    initialization: &Initialization,
    state: &mut BridgeState,
) -> Result<(), WaylandBridgeError> {
    let mut event_loop = EventLoop::try_new()
        .map_err(|error| WaylandBridgeError::new(format!("event queue failed: {error}")))?;
    state.loop_handle = Some(event_loop.handle());
    WaylandSource::new(connection, event_queue)
        .insert(event_loop.handle())
        .map_err(|error| WaylandBridgeError::new(format!("Wayland source failed: {error}")))?;
    let socket = state
        .connection
        .as_fd()
        .try_clone_to_owned()
        .map_err(|error| {
            WaylandBridgeError::new(format!("Wayland writable socket failed: {error}"))
        })?;
    let token = register_flush_retry(&event_loop.handle(), socket, |state: &mut BridgeState| {
        let complete = submission_result(state.connection.flush())?;
        state.flush_pending = !complete;
        Ok(complete)
    })?;
    state.flush_retry = Some(token);
    state.flush_requests()?;
    let loop_signal = event_loop.get_signal();
    event_loop
        .handle()
        .insert_source(command_source, move |event, _, state| match event {
            channel::Event::Msg(Command::Start {
                id,
                files,
                reporter,
                transfer_ready,
                terminal,
                started,
            }) => {
                let result = state.start(id, files, reporter, transfer_ready, terminal);
                if started.send(result).is_err() {
                    state.cancel(id);
                }
            }
            channel::Event::Msg(Command::Cancel { id }) => state.cancel(id),
            channel::Event::Closed => {
                state.finish_active(LinuxOutcome::Cancelled);
                loop_signal.stop();
            }
        })
        .map_err(|error| WaylandBridgeError::new(format!("cancel source failed: {error}")))?;
    if initialization.cancelled.load(Ordering::Acquire) {
        return Err(WaylandBridgeError::new(
            "Wayland initialization was cancelled",
        ));
    }
    ready
        .take()
        .unwrap()
        .send(Ok(()))
        .map_err(|_| WaylandBridgeError::new("Wayland initialization receiver was dropped"))?;
    // Startup succeeded; do not retain a shutdown handle for the live worker.
    initialization
        .socket
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    event_loop
        .run(None, state, |_| {})
        .map_err(|error| WaylandBridgeError::new(format!("Wayland dispatch failed: {error}")))
}

impl fmt::Debug for WaylandBridgeSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WaylandBridgeSession")
            .field("transfer_ready", &self.transfer_complete())
            .field("terminal", &self.is_terminal())
            .finish()
    }
}

impl Drop for WaylandBridgeSession {
    fn drop(&mut self) {
        if !self.is_terminal() {
            self.cancel_stale();
        }
    }
}

enum Command {
    Start {
        id: u64,
        files: FileSet,
        reporter: SessionReporter,
        transfer_ready: Arc<AtomicBool>,
        terminal: Arc<AtomicBool>,
        started: mpsc::SyncSender<Result<(), WaylandBridgeError>>,
    },
    Cancel {
        id: u64,
    },
}

struct ActiveDrag {
    id: u64,
    source: DragSource,
    _icon: Option<DragIcon>,
    offers: Vec<FileDragOffer>,
    reporter: WaylandSourceReporter,
    transfer_ready: Arc<AtomicBool>,
    terminal: Arc<AtomicBool>,
    transfers: Vec<(RawFd, RegistrationToken)>,
}

struct BridgeState {
    connection: Connection,
    queue: QueueHandle<BridgeState>,
    compositor: WlCompositor,
    manager: DataDeviceManagerState,
    seat_state: SeatState,
    shm: Shm,
    _data_device: DataDevice,
    origin: WlSurface,
    active: Option<ActiveDrag>,
    loop_handle: Option<LoopHandle<'static, BridgeState>>,
    flush_retry: Option<RegistrationToken>,
    flush_pending: bool,
}

impl BridgeState {
    fn start(
        &mut self,
        id: u64,
        files: FileSet,
        reporter: SessionReporter,
        transfer_ready: Arc<AtomicBool>,
        terminal: Arc<AtomicBool>,
    ) -> Result<(), WaylandBridgeError> {
        if terminal.load(Ordering::Acquire) {
            return Err(WaylandBridgeError::new("Wayland drag start was cancelled"));
        }
        if self.active.is_some() {
            self.finish_active(LinuxOutcome::Cancelled);
        }
        let offers = files.offers();
        let icon = files
            .preview()
            .map(|preview| DragIcon::new(&self.compositor, &self.shm, &self.queue, preview))
            .transpose()?;
        if icon.is_some() {
            reporter.preview(PreviewStatus::Attached);
        }
        let source = self.manager.create_drag_and_drop_source(
            &self.queue,
            offers.iter().map(FileDragOffer::mime_type),
            DndAction::Copy,
        );
        // The caller can time out while preview resources are prepared.
        if terminal.load(Ordering::Acquire) {
            return Err(WaylandBridgeError::new("Wayland drag start was cancelled"));
        }
        source.start_drag(
            &self._data_device,
            &self.origin,
            icon.as_ref().map(|icon| &icon.surface),
            0,
        );
        if let Some(icon) = &icon {
            icon.show();
        }
        self.active = Some(ActiveDrag {
            id,
            source,
            _icon: icon,
            offers,
            reporter: WaylandSourceReporter::new(reporter),
            transfer_ready,
            terminal,
            transfers: Vec::new(),
        });
        // Submission does not imply compositor acceptance. WaylandSource
        // dispatches responses and retries a backpressured flush; waiting for
        // a roundtrip here would stall every drag and cancellation command.
        if let Err(error) = self.flush_requests() {
            self.fail_active();
            return Err(error);
        }
        // A caller timeout can race request submission. Tear down the source
        // rather than returning a successful session that is already terminal.
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.terminal.load(Ordering::Acquire))
        {
            self.cancel(id);
            return Err(WaylandBridgeError::new("Wayland drag start was cancelled"));
        }
        Ok(())
    }

    fn flush_requests(&mut self) -> Result<(), WaylandBridgeError> {
        let complete = submission_result(self.connection.flush())?;
        if let (Some(handle), Some(token)) = (&self.loop_handle, self.flush_retry) {
            if !complete && !self.flush_pending {
                handle.enable(&token).map_err(|error| {
                    WaylandBridgeError::new(format!("Wayland flush retry failed: {error}"))
                })?;
            } else if complete && self.flush_pending {
                handle.disable(&token).map_err(|error| {
                    WaylandBridgeError::new(format!("Wayland flush retry disable failed: {error}"))
                })?;
            }
        }
        self.flush_pending = !complete;
        Ok(())
    }

    fn owns(&self, source: &WlDataSource) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.source.inner() == source)
    }

    fn cancel(&mut self, id: u64) {
        if self.active.as_ref().is_some_and(|active| active.id == id) {
            self.finish_active(LinuxOutcome::Cancelled);
        }
    }

    fn sync_transfer_ready(active: &ActiveDrag) {
        if active.reporter.is_transfer_ready() {
            active.transfer_ready.store(true, Ordering::Release);
        }
    }

    fn finish(&mut self, outcome: LinuxOutcome) {
        self.finish_active(outcome);
    }

    fn finish_active(&mut self, outcome: LinuxOutcome) {
        let Some(active) = self.active.take() else {
            return;
        };
        self.remove_transfers(&active);
        if !active.terminal.swap(true, Ordering::AcqRel) {
            active.reporter.finish_linux(outcome);
        }
        drop(active);
        let _ = self.flush_requests();
    }

    fn fail_active(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        self.remove_transfers(&active);
        if !active.terminal.swap(true, Ordering::AcqRel) {
            active.reporter.finish(Outcome::Failed(SessionFailure {
                stage: FailureStage::Transfer,
                kind: FailureKind::NativeFailure,
                native_code: None,
                native_effect: None,
            }));
        }
    }

    fn remove_transfers(&self, active: &ActiveDrag) {
        if let Some(handle) = &self.loop_handle {
            for (_, token) in &active.transfers {
                handle.remove(*token);
            }
        }
    }

    fn send_payload(&mut self, mime: &str, pipe: WritePipe) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let Some(offer) = active.offers.iter().find(|offer| offer.mime_type() == mime) else {
            return;
        };
        let Some(handle) = &self.loop_handle else {
            return;
        };
        let id = active.id;
        let data = offer.data().to_vec();
        let terminal = Arc::clone(&active.terminal);
        if let Some(transfer) =
            register_payload_transfer(handle, pipe, data, terminal, move |state, fd, result| {
                let Some(active) = state.active.as_mut().filter(|active| active.id == id) else {
                    return;
                };
                active.transfers.retain(|(pending_fd, _)| *pending_fd != fd);
                if result.is_some_and(|result| result.is_ok()) {
                    active.reporter.bridge_payload_transferred();
                    Self::sync_transfer_ready(active);
                }
            })
        {
            active.transfers.push(transfer);
        }
    }
}

impl Drop for BridgeState {
    fn drop(&mut self) {
        self.fail_active();
        self.origin.destroy();
    }
}

fn submission_result(result: Result<(), WaylandError>) -> Result<bool, WaylandBridgeError> {
    match result {
        Ok(()) => Ok(true),
        // Pending bytes stay in the connection's queue. The event source
        // flushes again before sleeping and after Wayland event dispatch.
        Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(WaylandBridgeError::new(format!(
            "Wayland drag submission failed: {error}"
        ))),
    }
}

fn register_flush_retry<State: 'static>(
    handle: &LoopHandle<'static, State>,
    socket: OwnedFd,
    mut flush: impl FnMut(&mut State) -> Result<bool, WaylandBridgeError> + 'static,
) -> Result<RegistrationToken, WaylandBridgeError> {
    let token = handle
        .insert_source(
            Generic::new(socket, Interest::WRITE, Mode::Level),
            move |_, _, state| match flush(state) {
                Ok(true) => Ok(PostAction::Disable),
                Ok(false) => Ok(PostAction::Continue),
                Err(error) => Err(io::Error::other(error)),
            },
        )
        .map_err(|error| {
            WaylandBridgeError::new(format!("Wayland writable source failed: {error}"))
        })?;
    handle.disable(&token).map_err(|error| {
        WaylandBridgeError::new(format!("Wayland writable source disable failed: {error}"))
    })?;
    Ok(token)
}

// Both production and pipe tests use this registration and dispatch path.
// None means cancellation; Some reports successful or failed pipe delivery.
fn register_payload_transfer<State: 'static>(
    handle: &LoopHandle<'static, State>,
    pipe: WritePipe,
    data: Vec<u8>,
    terminal: Arc<AtomicBool>,
    mut finished: impl FnMut(&mut State, RawFd, Option<io::Result<()>>) + 'static,
) -> Option<(RawFd, RegistrationToken)> {
    if terminal.load(Ordering::Acquire) {
        return None;
    }
    let flags = fcntl_getfl(&pipe).ok()?;
    fcntl_setfl(&pipe, flags | OFlags::NONBLOCK).ok()?;
    let fd = pipe.as_raw_fd();
    let mut written = 0;
    let token = handle
        .insert_source(pipe, move |(), file, state| {
            if terminal.load(Ordering::Acquire) {
                finished(state, fd, None);
                return PostAction::Remove;
            }
            // Writing through &File keeps calloop's registered descriptor owned.
            let result = write_payload(&mut &**file, &data, &mut written);
            if matches!(result, Ok(false)) {
                return PostAction::Continue;
            }
            if terminal.load(Ordering::Acquire) {
                finished(state, fd, None);
            } else {
                finished(state, fd, Some(result.map(|_| ())));
            }
            PostAction::Remove
        })
        .ok()?;
    Some((fd, token))
}

fn write_payload(writer: &mut impl Write, data: &[u8], written: &mut usize) -> io::Result<bool> {
    if *written == data.len() {
        return Ok(true);
    }
    // Bound each dispatch so a fast reader cannot monopolize the shared worker.
    let end = data.len().min(written.saturating_add(64 * 1024));
    match writer.write(&data[*written..end]) {
        Ok(0) => Err(io::ErrorKind::WriteZero.into()),
        Ok(count) => {
            *written += count;
            Ok(*written == data.len())
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

struct DragIcon {
    surface: WlSurface,
    _pool: SlotPool,
    _buffer: Buffer,
    icon: crate::WaylandIcon,
}

impl DragIcon {
    fn show(&self) {
        let icon = &self.icon;
        if self.surface.version() >= 3 {
            self.surface.set_buffer_scale(icon.scale);
        }
        if self.surface.version() >= 5 {
            self.surface.attach(Some(self._buffer.wl_buffer()), 0, 0);
            self.surface.offset(icon.offset[0], icon.offset[1]);
        } else {
            self.surface.attach(
                Some(self._buffer.wl_buffer()),
                icon.offset[0],
                icon.offset[1],
            );
        }
        if self.surface.version() >= 4 {
            self.surface
                .damage_buffer(0, 0, icon.buffer_size[0], icon.buffer_size[1]);
        } else {
            self.surface
                .damage(0, 0, icon.logical_size[0], icon.logical_size[1]);
        }
        self.surface.commit();
    }

    fn new(
        compositor: &WlCompositor,
        shm: &Shm,
        queue: &QueueHandle<BridgeState>,
        preview: &crate::DragPreview,
    ) -> Result<Self, WaylandBridgeError> {
        preview
            .validate()
            .map_err(|error| WaylandBridgeError::new(error.to_string()))?;
        let scale = if compositor.version() >= 3 {
            preview.backing_scale().ceil().max(1.0) as i32
        } else {
            1
        };
        let icon = preview
            .wayland_icon(scale)
            .map_err(|error| WaylandBridgeError::new(error.to_string()))?;
        let [width, height] = icon.buffer_size;
        let stride = width * 4;
        let mut pool = SlotPool::new(icon.pixels.len(), shm)
            .map_err(|error| WaylandBridgeError::new(format!("drag icon pool failed: {error}")))?;
        let (buffer, canvas) = pool
            .create_buffer(width, height, stride, wl_shm::Format::Argb8888)
            .map_err(|error| WaylandBridgeError::new(format!("drag icon failed: {error}")))?;
        canvas.copy_from_slice(&icon.pixels);
        // No global pointer position is needed: the compositor places this
        // surface relative to the drag hotspot. Commit after start_drag assigns
        // the icon role, never before it.
        Ok(Self {
            surface: compositor.create_surface(queue, ()),
            _pool: pool,
            _buffer: buffer,
            icon,
        })
    }
}

impl Drop for DragIcon {
    fn drop(&mut self) {
        self.surface.destroy();
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for BridgeState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: <wl_registry::WlRegistry as Proxy>::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCompositor, ()> for BridgeState {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: <WlCompositor as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSurface, ()> for BridgeState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSurface,
        _event: <WlSurface as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl SeatHandler for BridgeState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _connection: &Connection, _queue: &QueueHandle<Self>, _seat: WlSeat) {}

    fn new_capability(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _seat: WlSeat,
        _capability: Capability,
    ) {
    }

    fn remove_capability(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _seat: WlSeat,
        _capability: Capability,
    ) {
    }

    fn remove_seat(&mut self, _connection: &Connection, _queue: &QueueHandle<Self>, _seat: WlSeat) {
    }
}

impl ShmHandler for BridgeState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl DataDeviceHandler for BridgeState {
    fn enter(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _device: &WlDataDevice,
        _x: f64,
        _y: f64,
        _surface: &WlSurface,
    ) {
    }

    fn leave(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _device: &WlDataDevice,
    ) {
    }

    fn motion(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _device: &WlDataDevice,
        _x: f64,
        _y: f64,
    ) {
    }

    fn selection(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _device: &WlDataDevice,
    ) {
    }

    fn drop_performed(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _device: &WlDataDevice,
    ) {
    }
}

impl DataOfferHandler for BridgeState {
    fn source_actions(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        offer: &mut DragOffer,
        actions: DndAction,
    ) {
        offer.set_actions(actions, DndAction::Copy);
    }

    fn selected_action(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _offer: &mut DragOffer,
        _actions: DndAction,
    ) {
    }
}

impl DataSourceHandler for BridgeState {
    fn accept_mime(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _source: &WlDataSource,
        _mime: Option<String>,
    ) {
    }

    fn send_request(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        pipe: WritePipe,
    ) {
        if self.owns(source) {
            self.send_payload(&mime, pipe);
        }
    }

    fn cancelled(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        source: &WlDataSource,
    ) {
        if self.owns(source) {
            self.finish(LinuxOutcome::Cancelled);
        }
    }

    fn dnd_dropped(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        source: &WlDataSource,
    ) {
        if self.owns(source)
            && let Some(active) = self.active.as_mut()
        {
            active.reporter.drop_performed();
            Self::sync_transfer_ready(active);
        }
    }

    fn dnd_finished(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        source: &WlDataSource,
    ) {
        if self.owns(source) {
            let outcome =
                finished_outcome(self.active.as_ref().unwrap().reporter.is_transfer_ready());
            self.finish(outcome);
        }
    }

    fn action(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _source: &WlDataSource,
        _action: DndAction,
    ) {
    }
}

smithay_client_toolkit::delegate_dispatch2!(BridgeState);

fn finished_outcome(transfer_ready: bool) -> LinuxOutcome {
    if transfer_ready {
        LinuxOutcome::Exported
    } else {
        LinuxOutcome::Indeterminate
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{PipeReader, Read};
    use std::os::fd::OwnedFd;

    use super::*;

    struct RegistryState;
    impl Dispatch<wl_registry::WlRegistry, ()> for RegistryState {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: <wl_registry::WlRegistry as Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for RegistryState {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: <wl_registry::WlRegistry as Proxy>::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    #[derive(Default)]
    struct TransferState {
        complete: bool,
        failed: bool,
        cancelled: bool,
        command_received: bool,
        flush_calls: usize,
    }

    fn pipe_transfer(
        data: Vec<u8>,
    ) -> (
        EventLoop<'static, TransferState>,
        PipeReader,
        RegistrationToken,
    ) {
        let (reader, writer) = io::pipe().unwrap();
        let pipe = WritePipe::from(OwnedFd::from(writer));
        fcntl_setfl(&reader, fcntl_getfl(&reader).unwrap() | OFlags::NONBLOCK).unwrap();
        let event_loop = EventLoop::try_new().unwrap();
        let (_, token) = register_payload_transfer(
            &event_loop.handle(),
            pipe,
            data,
            Arc::new(AtomicBool::new(false)),
            |state: &mut TransferState, _, result| match result {
                Some(Ok(())) => state.complete = true,
                Some(Err(_)) => state.failed = true,
                None => state.cancelled = true,
            },
        )
        .unwrap();
        (event_loop, reader, token)
    }

    fn drain(reader: &mut PipeReader, output: &mut Vec<u8>) -> bool {
        let mut buffer = [0; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return true,
                Ok(count) => output.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return false,
                Err(error) => panic!("pipe read failed: {error}"),
            }
        }
    }

    #[test]
    fn failed_initialization_is_retryable_and_concurrent_initialization_returns_promptly() {
        let cache = Mutex::new(None::<usize>);
        assert!(
            cached_runtime(
                &cache,
                |_| true,
                || Err(WaylandBridgeError::new("unavailable"))
            )
            .is_err()
        );
        assert_eq!(cached_runtime(&cache, |_| true, || Ok(2)).unwrap(), 2);
        assert_eq!(
            cached_runtime(&cache, |_| true, || panic!("live runtime must be reused")).unwrap(),
            2
        );
        let _guard = cache.lock().unwrap();
        assert!(
            cached_runtime(&cache, |_| true, || panic!("must not launch concurrently")).is_err()
        );
    }

    #[test]
    fn dead_worker_channel_is_replaced_without_reusing_its_queue() {
        let (commands, old_source) = channel::channel();
        let alive = Arc::new(AtomicBool::new(true));
        let lifetime = WorkerLifetime(Arc::clone(&alive));
        let old = BridgeRuntime { commands, alive };
        let cache = Mutex::new(Some(old.clone()));
        drop(old_source);
        drop(lifetime);
        assert!(old.commands.send(Command::Cancel { id: 1 }).is_err());
        let (commands, fresh_source) = channel::channel();
        let fresh = cached_runtime(
            &cache,
            |runtime| runtime.alive.load(Ordering::Acquire),
            || {
                Ok(BridgeRuntime {
                    commands,
                    alive: Arc::new(AtomicBool::new(true)),
                })
            },
        )
        .unwrap();
        fresh.commands.send(Command::Cancel { id: 2 }).unwrap();
        assert!(matches!(
            fresh_source.try_recv(),
            Ok(Command::Cancel { id: 2 })
        ));
    }

    #[test]
    fn initialization_timeout_interrupts_a_registry_with_no_reply() {
        let (socket, mut peer) = UnixStream::pair().unwrap();
        let initialization = Arc::new(Initialization::default());
        let worker_initialization = Arc::clone(&initialization);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let connection = Connection::from_socket(socket).unwrap();
            worker_initialization.attach(&connection).unwrap();
            let result = registry_queue_init::<RegistryState>(&connection)
                .map(|_| ())
                .map_err(|error| WaylandBridgeError::new(error.to_string()));
            let failed = result.is_err();
            let abandoned = ready_tx.send(result).is_err();
            done_tx.send((failed, abandoned)).unwrap();
        });
        // Observe a registry request first, proving timeout interrupts an
        // already-blocked roundtrip rather than only preventing startup.
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut message = [0; 128];
        assert!(peer.read(&mut message).unwrap() > 0);
        assert!(
            initialization
                .wait(ready_rx, Duration::from_millis(10))
                .is_err()
        );
        assert!(initialization.cancelled.load(Ordering::Acquire));
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            (true, true)
        );
        worker.join().unwrap();
    }

    #[test]
    fn cancelled_initialization_rejects_a_late_connection() {
        let initialization = Initialization::default();
        initialization.cancel();
        let (socket, mut peer) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        assert!(initialization.attach(&connection).is_err());
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    }

    #[test]
    fn outgoing_socket_backpressure_preserves_commands_and_resumes_on_writable_readiness() {
        let (mut socket, mut peer) = UnixStream::pair().unwrap();
        socket.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        let padding = [0_u8; 16 * 1024];
        loop {
            match socket.write(&padding) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("socket fill failed: {error}"),
            }
        }
        let connection = Connection::from_socket(socket).unwrap();
        let queue = connection.new_event_queue::<RegistryState>();
        let _registry = connection.display().get_registry(&queue.handle(), ());
        assert!(!submission_result(connection.flush()).unwrap());
        let mut event_loop = EventLoop::try_new().unwrap();
        let flush_connection = connection.clone();
        let token = register_flush_retry(
            &event_loop.handle(),
            connection.as_fd().try_clone_to_owned().unwrap(),
            move |state: &mut TransferState| {
                state.flush_calls += 1;
                let complete = submission_result(flush_connection.flush())?;
                state.complete = complete;
                Ok(complete)
            },
        )
        .unwrap();
        event_loop.handle().enable(&token).unwrap();
        let (commands, source) = channel::channel();
        event_loop
            .handle()
            .insert_source(source, |event, _, state| {
                if matches!(event, channel::Event::Msg(())) {
                    state.command_received = true;
                }
            })
            .unwrap();
        commands.send(()).unwrap();
        let mut state = TransferState::default();
        event_loop
            .dispatch(Duration::from_millis(10), &mut state)
            .unwrap();
        assert!(state.command_received && !state.complete);
        assert_eq!(state.flush_calls, 0, "a full socket must not spin on retry");
        loop {
            match peer.read(&mut [0; 16 * 1024]) {
                Ok(0) => panic!("connection closed during pending submission"),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("socket drain failed: {error}"),
            }
        }
        for _ in 0..8 {
            event_loop
                .dispatch(Duration::from_millis(10), &mut state)
                .unwrap();
            if state.complete {
                break;
            }
        }
        assert!(state.complete && state.flush_calls > 0);
        let calls = state.flush_calls;
        event_loop.dispatch(Duration::ZERO, &mut state).unwrap();
        assert_eq!(
            state.flush_calls, calls,
            "successful retry source must be disabled"
        );
    }

    #[test]
    fn submission_backpressure_is_pending_but_fatal_errors_are_rejected() {
        assert!(submission_result(Ok(())).is_ok());
        assert!(submission_result(Err(WaylandError::Io(io::ErrorKind::WouldBlock.into()))).is_ok());
        for kind in [io::ErrorKind::BrokenPipe, io::ErrorKind::ConnectionReset] {
            assert!(submission_result(Err(WaylandError::Io(kind.into()))).is_err());
        }
        let protocol_error = wayland_client::backend::protocol::ProtocolError {
            code: 1,
            object_id: 1,
            object_interface: "wl_display".into(),
            message: "test protocol failure".into(),
        };
        assert!(submission_result(Err(WaylandError::Protocol(protocol_error))).is_err());
    }

    #[test]
    fn request_submission_does_not_require_a_compositor_reply() {
        let (socket, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let queue = connection.new_event_queue::<RegistryState>();
        let _registry = connection.display().get_registry(&queue.handle(), ());
        // The peer never sends an event or acknowledges synchronization.
        submission_result(connection.flush()).unwrap();
        let mut message = [0; 64];
        assert!(peer.read(&mut message).unwrap() > 0);
    }

    #[test]
    fn nonreading_target_does_not_block_worker_commands_or_report_completion() {
        let (mut event_loop, mut reader, token) = pipe_transfer(vec![42; 2 * 1024 * 1024]);
        let mut state = TransferState::default();
        event_loop.dispatch(Duration::ZERO, &mut state).unwrap();
        assert!(!state.complete);
        assert_eq!(
            finished_outcome(state.complete),
            LinuxOutcome::Indeterminate
        );

        let (commands, source) = channel::channel();
        let handle = event_loop.handle().downgrade();
        event_loop
            .handle()
            .insert_source(source, move |event, _, state| {
                if let channel::Event::Msg(()) = event {
                    state.command_received = true;
                    handle.upgrade().unwrap().remove(token);
                }
            })
            .unwrap();
        commands.send(()).unwrap();
        for _ in 0..8 {
            event_loop
                .dispatch(Duration::from_millis(10), &mut state)
                .unwrap();
            if state.command_received {
                break;
            }
        }
        assert!(state.command_received);
        assert!(!state.complete && !state.failed);
        let mut received = Vec::new();
        assert!(
            drain(&mut reader, &mut received),
            "cancellation must close the pipe"
        );
        assert!(!received.is_empty() && received.len() < 2 * 1024 * 1024);
        assert!(received.iter().all(|byte| *byte == 42));
    }

    #[test]
    fn partial_pipe_writes_resume_and_complete_only_after_all_bytes() {
        let data: Vec<u8> = (0..2 * 1024 * 1024).map(|index| index as u8).collect();
        let (mut event_loop, mut reader, _) = pipe_transfer(data.clone());
        let mut state = TransferState::default();
        let mut received = Vec::new();
        for _ in 0..4096 {
            event_loop.dispatch(Duration::ZERO, &mut state).unwrap();
            let eof = drain(&mut reader, &mut received);
            if state.complete {
                assert_eq!(received.len(), data.len());
                assert!(eof, "completion must close the pipe");
                break;
            }
            assert!(received.len() < data.len());
            assert!(!eof);
        }
        assert!(state.complete && !state.failed);
        assert_eq!(received, data);
        assert_eq!(finished_outcome(state.complete), LinuxOutcome::Exported);
    }

    #[test]
    fn closed_receiver_removes_transfer_without_completion() {
        let (mut event_loop, reader, _) = pipe_transfer(vec![42; 2 * 1024 * 1024]);
        let mut state = TransferState::default();
        event_loop.dispatch(Duration::ZERO, &mut state).unwrap();
        drop(reader);
        for _ in 0..8 {
            event_loop.dispatch(Duration::ZERO, &mut state).unwrap();
            if state.failed {
                break;
            }
        }
        assert!(state.failed && !state.complete);
    }

    #[test]
    fn cancelled_transfer_is_rejected_and_closes_pipe_before_registration() {
        let (mut reader, writer) = io::pipe().unwrap();
        fcntl_setfl(&reader, fcntl_getfl(&reader).unwrap() | OFlags::NONBLOCK).unwrap();
        let mut event_loop = EventLoop::<TransferState>::try_new().unwrap();
        let transfer = register_payload_transfer(
            &event_loop.handle(),
            WritePipe::from(OwnedFd::from(writer)),
            vec![42; 2 * 1024 * 1024],
            Arc::new(AtomicBool::new(true)),
            |_, _, _| panic!("cancelled transfer must never be dispatched"),
        );
        assert!(transfer.is_none());
        event_loop
            .dispatch(Duration::ZERO, &mut TransferState::default())
            .unwrap();
        let mut received = Vec::new();
        assert!(drain(&mut reader, &mut received));
        assert!(received.is_empty());
    }

    #[test]
    fn cancellation_after_registration_closes_pipe_without_writing_or_completing() {
        let (mut reader, writer) = io::pipe().unwrap();
        fcntl_setfl(&reader, fcntl_getfl(&reader).unwrap() | OFlags::NONBLOCK).unwrap();
        let mut event_loop = EventLoop::try_new().unwrap();
        let terminal = Arc::new(AtomicBool::new(false));
        register_payload_transfer(
            &event_loop.handle(),
            WritePipe::from(OwnedFd::from(writer)),
            vec![42; 2 * 1024 * 1024],
            Arc::clone(&terminal),
            |state: &mut TransferState, _, result| {
                assert!(result.is_none());
                state.cancelled = true;
            },
        )
        .unwrap();
        terminal.store(true, Ordering::Release);
        let mut state = TransferState::default();
        event_loop.dispatch(Duration::ZERO, &mut state).unwrap();
        assert!(state.cancelled && !state.complete && !state.failed);
        let mut received = Vec::new();
        assert!(drain(&mut reader, &mut received));
        assert!(received.is_empty());
    }

    #[test]
    fn writes_are_bounded_per_dispatch() {
        let data = vec![42; 128 * 1024];
        let mut output = Vec::new();
        let mut written = 0;
        assert!(!write_payload(&mut output, &data, &mut written).unwrap());
        assert_eq!(written, 64 * 1024);
        assert!(write_payload(&mut output, &data, &mut written).unwrap());
        assert_eq!(output, data);
    }

    #[test]
    fn retryable_and_zero_writes_preserve_the_offset() {
        struct Writer(VecDeque<io::Result<usize>>);
        impl Write for Writer {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                self.0.pop_front().unwrap()
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = Writer(VecDeque::from([
            Ok(2),
            Err(io::ErrorKind::Interrupted.into()),
            Err(io::ErrorKind::WouldBlock.into()),
            Ok(0),
        ]));
        let mut written = 0;
        for _ in 0..3 {
            assert!(!write_payload(&mut writer, b"hello", &mut written).unwrap());
            assert_eq!(written, 2);
        }
        assert_eq!(
            write_payload(&mut writer, b"hello", &mut written)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WriteZero,
        );
        assert_eq!(written, 2);
    }
}
