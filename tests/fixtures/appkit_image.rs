//! Two separately linked test images exercise process-global Objective-C registration.

#[cfg(not(target_os = "macos"))]
#[unsafe(no_mangle)]
pub extern "C" fn matari_test_rejected_start(
    _view: *mut std::ffi::c_void,
    _event: *mut std::ffi::c_void,
    _path: *const std::ffi::c_char,
) -> u32 {
    0
}

/// Attempt one public-API drag through the real rejecting AppKit fixture view.
///
/// # Safety
///
/// `view` and `event` must be the current main-thread mouseDown callback's live
/// NSView/NSEvent. `path` must be a readable NUL-terminated absolute file path.
#[cfg(target_os = "macos")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn matari_test_rejected_start(
    view: *mut std::ffi::c_void,
    event: *mut std::ffi::c_void,
    path: *const std::ffi::c_char,
) -> u32 {
    use matari_audio_drag_and_drop::*;
    use raw_window_handle::{DisplayHandle, HasDisplayHandle, HasWindowHandle, WindowHandle};
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;
    use std::ptr::NonNull;

    struct Origin(NonNull<std::ffi::c_void>);
    impl HasWindowHandle for Origin {
        fn window_handle(&self) -> Result<WindowHandle<'_>, raw_window_handle::HandleError> {
            // SAFETY: Real caller-owned view borrowed throughout this invocation.
            Ok(unsafe {
                WindowHandle::borrow_raw(raw_window_handle::AppKitWindowHandle::new(self.0).into())
            })
        }
    }
    impl HasDisplayHandle for Origin {
        fn display_handle(&self) -> Result<DisplayHandle<'_>, raw_window_handle::HandleError> {
            Ok(DisplayHandle::appkit())
        }
    }
    struct Adapter(Option<StartTicket>);
    impl ToolkitAdapter for Adapter {
        type Error = std::io::Error;
        fn outbound_route(&self) -> Option<SessionRoute> {
            Some(SessionRoute {
                protocol: NativeProtocol::AppKit,
                source: SourceContext::EmbeddedAppKit,
            })
        }
        fn schedule_outbound(
            &mut self,
            ticket: StartTicket,
        ) -> Result<(), RejectedStart<Self::Error>> {
            self.0 = Some(ticket);
            Ok(())
        }
        fn drive_inbound(
            &mut self,
            _: &mut dyn InboundHandler,
        ) -> Result<InboundDisposition, Self::Error> {
            Ok(InboundDisposition::Unhandled)
        }
    }
    let result = std::panic::catch_unwind(|| {
        let view = Origin(NonNull::new(view).expect("live view"));
        // SAFETY: Caller supplies a live NUL-terminated filesystem path.
        let path = unsafe { CStr::from_ptr(path) };
        let path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes()));
        let mut controller = Controller::new();
        let mut adapter = Adapter(None);
        let files = FileSet::try_from_paths([path]).unwrap();
        let session = controller.start_outbound(&mut adapter, files).unwrap();
        // SAFETY: Origin/event borrow is scoped to the caller's actual mouseDown.
        let origin = unsafe {
            DragOrigin::with_appkit_event(&view, NonNull::new(event).expect("live event"))
        }
        .unwrap();
        let result = adapter.0.take().unwrap().start(&mut NativeRuntime, origin);
        assert!(matches!(result, Err(ProtocolError::Runtime(_))));
        assert!(matches!(
            controller.update().events(),
            [SessionEvent::OutboundTerminal {
                session: actual,
                outcome: Outcome::Failed(SessionFailure {
                    stage: FailureStage::Start,
                    kind: FailureKind::NativeFailure,
                    ..
                }),
            }] if *actual == session
        ));
        assert!(controller.active_outbound().is_none());
        assert!(controller.update().events().is_empty());
    });
    u32::from(result.is_ok())
}
