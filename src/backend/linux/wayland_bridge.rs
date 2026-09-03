//! Serial-less native Wayland drag source for XWayland plugin editors.
//!
//! Hyprland accepts `wl_data_device.start_drag` with serial zero and a
//! role-less origin surface. This lets an XWayland editor use a private
//! Wayland connection while the compositor owns delivery to both native
//! Wayland and X11 targets.

use std::error::Error;
use std::fmt;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread;
use std::time::Duration;

use smithay_client_toolkit::{
    data_device_manager::{
        DataDeviceManagerState, WritePipe,
        data_device::{DataDevice, DataDeviceHandler},
        data_offer::{DataOfferHandler, DragOffer},
        data_source::{DataSourceHandler, DragSource},
    },
    reexports::{
        calloop::{EventLoop, channel},
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
    fn prepare() -> Result<Self, WaylandBridgeError> {
        let connection = Connection::connect_to_env()
            .map_err(|error| WaylandBridgeError::new(format!("Wayland connect failed: {error}")))?;
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
        connection
            .flush()
            .map_err(|error| WaylandBridgeError::new(format!("Wayland flush failed: {error}")))?;
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

static NEXT_DRAG_ID: AtomicU64 = AtomicU64::new(1);
static BRIDGE_RUNTIME: OnceLock<Result<BridgeRuntime, WaylandBridgeError>> = OnceLock::new();

struct BridgeRuntime {
    commands: channel::Sender<Command>,
}

impl BridgeRuntime {
    fn shared() -> Result<&'static Self, WaylandBridgeError> {
        BRIDGE_RUNTIME
            .get_or_init(Self::launch)
            .as_ref()
            .map_err(Clone::clone)
    }

    fn launch() -> Result<Self, WaylandBridgeError> {
        let prepared = PreparedWaylandBridge::prepare()?;
        let (commands, command_source) = channel::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("matari-wayland-dnd".to_owned())
            .spawn(move || {
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
                };
                if run_queue(
                    prepared.connection,
                    prepared.event_queue,
                    command_source,
                    ready_tx,
                    &mut state,
                )
                .is_err()
                {
                    state.fail_active();
                }
            })
            .map_err(|error| {
                WaylandBridgeError::new(format!("Wayland drag thread failed: {error}"))
            })?;
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|error| {
                WaylandBridgeError::new(format!("Wayland drag runtime failed: {error}"))
            })??;
        Ok(Self { commands })
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
        let runtime = BridgeRuntime::shared()?;
        let id = NEXT_DRAG_ID.fetch_add(1, Ordering::Relaxed);
        let transfer_ready = Arc::new(AtomicBool::new(false));
        let terminal = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        runtime
            .commands
            .send(Command::Start {
                id,
                files,
                reporter: reporter.clone(),
                transfer_ready: Arc::clone(&transfer_ready),
                terminal: Arc::clone(&terminal),
                started: started_tx,
            })
            .map_err(|error| {
                WaylandBridgeError::new(format!("Wayland drag command failed: {error}"))
            })?;
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|error| {
                WaylandBridgeError::new(format!("Wayland drag start failed: {error}"))
            })??;
        Ok(WaylandBridgeSession {
            id,
            commands: runtime.commands.clone(),
            reporter,
            transfer_ready,
            terminal,
        })
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
    ready: mpsc::SyncSender<Result<(), WaylandBridgeError>>,
    state: &mut BridgeState,
) -> Result<(), WaylandBridgeError> {
    let mut event_loop = EventLoop::try_new()
        .map_err(|error| WaylandBridgeError::new(format!("event queue failed: {error}")))?;
    WaylandSource::new(connection, event_queue)
        .insert(event_loop.handle())
        .map_err(|error| WaylandBridgeError::new(format!("Wayland source failed: {error}")))?;
    event_loop
        .handle()
        .insert_source(command_source, |event, _, state| {
            if let channel::Event::Msg(command) = event {
                match command {
                    Command::Start {
                        id,
                        files,
                        reporter,
                        transfer_ready,
                        terminal,
                        started,
                    } => {
                        let result = state.start(id, files, reporter, transfer_ready, terminal);
                        let _ = started.send(result);
                    }
                    Command::Cancel { id } => state.cancel(id),
                }
            }
        })
        .map_err(|error| WaylandBridgeError::new(format!("cancel source failed: {error}")))?;
    let _ = ready.send(Ok(()));
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
        source.start_drag(
            &self._data_device,
            &self.origin,
            icon.as_ref().map(|icon| &icon.surface),
            0,
        );
        self.active = Some(ActiveDrag {
            id,
            source,
            _icon: icon,
            offers,
            reporter: WaylandSourceReporter::new(reporter),
            transfer_ready,
            terminal,
        });
        if let Err(error) = self.connection.roundtrip() {
            self.fail_active();
            return Err(WaylandBridgeError::new(format!(
                "Wayland drag acknowledgement failed: {error}"
            )));
        }
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
        if !active.terminal.swap(true, Ordering::AcqRel) {
            active.reporter.finish_linux(outcome);
        }
    }

    fn fail_active(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        if !active.terminal.swap(true, Ordering::AcqRel) {
            active.reporter.finish(Outcome::Failed(SessionFailure {
                stage: FailureStage::Transfer,
                kind: FailureKind::NativeFailure,
                native_code: None,
                native_effect: None,
            }));
        }
    }

    fn send_payload(&mut self, mime: &str, mut pipe: WritePipe) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let Some(offer) = active.offers.iter().find(|offer| offer.mime_type() == mime) else {
            return;
        };
        if pipe
            .write_all(offer.data())
            .and_then(|()| pipe.flush())
            .is_ok()
        {
            active.reporter.bridge_payload_transferred();
            Self::sync_transfer_ready(active);
        }
    }
}

struct DragIcon {
    surface: WlSurface,
    _pool: SlotPool,
    _buffer: Buffer,
}

impl DragIcon {
    fn new(
        compositor: &WlCompositor,
        shm: &Shm,
        queue: &QueueHandle<BridgeState>,
        preview: &crate::DragPreview,
    ) -> Result<Self, WaylandBridgeError> {
        const STRIDE: i32 = crate::preview::WIDTH as i32 * 4;

        let surface = compositor.create_surface(queue, ());
        let mut pool = SlotPool::new(STRIDE as usize * crate::preview::HEIGHT, shm)
            .map_err(|error| WaylandBridgeError::new(format!("drag icon pool failed: {error}")))?;
        let (buffer, canvas) = pool
            .create_buffer(
                crate::preview::WIDTH as i32,
                crate::preview::HEIGHT as i32,
                STRIDE,
                wl_shm::Format::Argb8888,
            )
            .map_err(|error| WaylandBridgeError::new(format!("drag icon failed: {error}")))?;
        canvas.copy_from_slice(&crate::preview::render(preview));
        surface.attach(Some(buffer.wl_buffer()), 0, 0);
        surface.damage_buffer(
            0,
            0,
            crate::preview::WIDTH as i32,
            crate::preview::HEIGHT as i32,
        );
        surface.commit();
        Ok(Self {
            surface,
            _pool: pool,
            _buffer: buffer,
        })
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
            self.finish(LinuxOutcome::Exported);
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
