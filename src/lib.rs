#![doc = include_str!("../README.md")]

mod backend;
mod facade;
mod file_payload;
#[cfg(not(all(target_family = "unix", not(target_os = "macos"))))]
mod platform;
mod preview;
pub mod toolkit;
mod wayland;

pub use file_payload::FileDragOffer;
pub(crate) use file_payload::FileDragPayloadData;
pub use preview::{DragPreview, MidiPreviewNote};
pub use wayland::WaylandSourceReporter;

#[cfg(all(target_family = "unix", not(target_os = "macos")))]
pub use backend::{
    X11BridgeEvidence, X11BridgeReport, X11DropRouter, X11PointerEvent, X11Session,
    X11SessionError, X11SessionStatus, X11StartError, X11WaylandBridge, x11_bridge_report,
    x11_outbound_protocol, x11_wayland_bridge,
};

pub use facade::{
    Controller, DragOrigin, FailureKind, FailureStage, FileSet, FileSetError, Inbound,
    InboundDecision, InboundDisposition, InboundError, InboundHandler, InboundOffer, LinuxFailure,
    LinuxOutcome, LinuxRejector, NativeProtocol, NativeReporter, NativeRuntime, NativeRuntimePort,
    NativeStartError, OriginError, Outbound, Outcome, PreviewFailureStage, PreviewStatus,
    ProtocolError, RejectReason, RejectedStart, SessionEvent, SessionFailure, SessionId,
    SessionReporter, SessionRoute, SourceContext, StartError, StartTicket, ToolkitAdapter, Update,
    WaylandReporter, WaylandRuntimePort, WaylandStartError,
};
