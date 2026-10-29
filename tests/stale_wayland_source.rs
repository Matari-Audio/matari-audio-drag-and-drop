//! A Wayland source whose grab ended without a terminal event must stop
//! blocking the controller, and a late event must not rewrite its outcome.

use matari_audio_drag_and_drop::{
    Controller, FileSet, InboundDisposition, InboundHandler, LinuxOutcome, NativeProtocol, Outcome,
    RejectedStart, SessionEvent, SessionRoute, SourceContext, StartError, StartTicket,
    ToolkitAdapter, WaylandReporter, WaylandRuntimePort, WaylandSourceReporter,
};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

type Slot = Rc<RefCell<Option<WaylandReporter>>>;

fn route() -> SessionRoute {
    SessionRoute {
        protocol: NativeProtocol::WaylandDataDevice,
        source: SourceContext::Wayland,
    }
}

fn files() -> FileSet {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    FileSet::try_from_paths([path]).unwrap()
}

/// Keeps the native reporter, as a toolkit's Wayland runtime would.
struct Runtime(Slot);

impl WaylandRuntimePort for Runtime {
    type Error = std::io::Error;

    fn start_drag(
        self,
        _files: FileSet,
        reporter: WaylandReporter,
    ) -> Result<SessionRoute, Self::Error> {
        *self.0.borrow_mut() = Some(reporter);
        Ok(route())
    }
}

struct Adapter(Slot);

impl ToolkitAdapter for Adapter {
    type Error = std::io::Error;

    fn outbound_route(&self) -> Option<SessionRoute> {
        Some(route())
    }

    fn schedule_outbound(&mut self, ticket: StartTicket) -> Result<(), RejectedStart<Self::Error>> {
        ticket
            .start_wayland(Runtime(Rc::clone(&self.0)))
            .expect("start the Wayland source");
        Ok(())
    }

    fn drive_inbound(
        &mut self,
        _handler: &mut dyn InboundHandler,
    ) -> Result<InboundDisposition, Self::Error> {
        Ok(InboundDisposition::Unhandled)
    }
}

fn terminal_outcome(events: &[SessionEvent]) -> Option<Outcome> {
    events.iter().find_map(|event| match event {
        SessionEvent::OutboundTerminal { outcome, .. } => Some(outcome.clone()),
        _ => None,
    })
}

#[test]
fn a_retired_source_releases_the_controller_once() {
    let slot: Slot = Rc::default();
    let mut adapter = Adapter(Rc::clone(&slot));
    let mut controller = Controller::new();
    controller.start_outbound(&mut adapter, files()).unwrap();
    let late = slot.borrow().clone().unwrap();

    assert!(matches!(
        controller.start_outbound(&mut adapter, files()),
        Err(StartError::Busy { .. })
    ));

    let source = WaylandSourceReporter::new(slot.borrow_mut().take().unwrap());
    source.retire();
    let events: Vec<_> = controller.update().into_events().collect();
    assert_eq!(
        terminal_outcome(&events),
        Some(Outcome::Linux(LinuxOutcome::Cancelled))
    );

    late.finish_linux(LinuxOutcome::Exported);
    assert!(controller.update().into_events().next().is_none());
    controller.start_outbound(&mut adapter, files()).unwrap();
}

#[test]
fn a_dropped_source_retires_as_indeterminate() {
    let slot: Slot = Rc::default();
    let mut adapter = Adapter(Rc::clone(&slot));
    let mut controller = Controller::new();
    controller.start_outbound(&mut adapter, files()).unwrap();

    let mut source = WaylandSourceReporter::new(slot.borrow_mut().take().unwrap());
    source.drop_performed();
    source.retire();
    let events: Vec<_> = controller.update().into_events().collect();
    assert_eq!(
        terminal_outcome(&events),
        Some(Outcome::Linux(LinuxOutcome::Indeterminate))
    );
}
