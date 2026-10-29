//! Event-driven lifecycle state for native Wayland data sources.

use crate::{LinuxOutcome, Outcome, WaylandReporter};

/// Reports one native Wayland source lifecycle without guessing completion.
///
/// Standard sources become replaceable after both a performed drop and a
/// delivered payload. The serial-less Hyprland bridge may explicitly use full
/// payload delivery because its XWayland bridge omits the drop callback.
/// Replaceability is never terminal: the native source remains alive until
/// Wayland sends `dnd_finished` or `cancelled`.
pub struct WaylandSourceReporter {
    reporter: WaylandReporter,
    data_requested: bool,
    drop_performed: bool,
    transfer_ready: bool,
}

impl WaylandSourceReporter {
    /// Wrap the reporter supplied to [`crate::WaylandRuntimePort::start_drag`].
    #[must_use]
    pub const fn new(reporter: WaylandReporter) -> Self {
        Self {
            reporter,
            data_requested: false,
            drop_performed: false,
            transfer_ready: false,
        }
    }

    /// Report that the destination received one requested MIME payload.
    pub fn data_requested(&mut self) {
        self.reporter.data_requested();
        self.data_requested = true;
        self.report_transfer_ready();
    }

    /// Report the compositor's `dnd_drop_performed` event.
    pub fn drop_performed(&mut self) {
        self.reporter.drop_performed();
        self.drop_performed = true;
        self.report_transfer_ready();
    }

    /// Whether protocol evidence permits a newer gesture to replace this one.
    ///
    /// The native source must remain alive after this becomes true.
    #[must_use]
    pub const fn is_transfer_ready(&self) -> bool {
        self.transfer_ready
    }

    /// Mark a fully written payload as replaceable for the serial-less
    /// Hyprland bridge, which may omit `dnd_drop_performed`.
    ///
    /// This is non-terminal and deliberately does not report a performed
    /// drop. The native source must remain alive for late MIME requests and
    /// authoritative `dnd_finished` or `cancelled` events.
    #[cfg(target_os = "linux")]
    pub(crate) fn bridge_payload_transferred(&mut self) {
        self.reporter.data_requested();
        self.data_requested = true;
        self.mark_transfer_ready();
    }

    /// Finish the source from an authoritative Linux terminal event.
    pub fn finish_linux(&self, outcome: LinuxOutcome) {
        self.reporter.finish_linux(outcome);
    }

    /// Finish a source whose native grab ended without a terminal event.
    ///
    /// Call only after native evidence that the compositor released the drag,
    /// such as a new pointer press on the origin surface. A performed drop
    /// that never finished is indeterminate; otherwise the drag was cancelled.
    pub fn retire(&self) {
        self.finish_linux(if self.drop_performed {
            LinuxOutcome::Indeterminate
        } else {
            LinuxOutcome::Cancelled
        });
    }

    /// Finish the source from an authoritative native terminal event.
    pub fn finish(&self, outcome: Outcome) {
        self.reporter.finish(outcome);
    }

    fn report_transfer_ready(&mut self) {
        if !self.transfer_ready && self.data_requested && self.drop_performed {
            self.mark_transfer_ready();
        }
    }

    fn mark_transfer_ready(&mut self) {
        if !self.transfer_ready {
            self.transfer_ready = true;
            self.reporter.transfer_ready();
        }
    }
}
