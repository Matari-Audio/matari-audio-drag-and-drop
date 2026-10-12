#![cfg(target_os = "linux")]

#[path = "support/selection_race.rs"]
mod selection_race;

use matari_audio_drag_and_drop::*;
use raw_window_handle::{DisplayHandle, HasDisplayHandle, HasWindowHandle, WindowHandle};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::{Event, xproto::*};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const ROUTE: SessionRoute = SessionRoute {
    protocol: NativeProtocol::Xdnd,
    source: SourceContext::DetachedX11,
};

struct Window(u32);
impl HasWindowHandle for Window {
    fn window_handle(&self) -> Result<WindowHandle<'_>, raw_window_handle::HandleError> {
        // SAFETY: the fixture owns this live X window until the session ends.
        Ok(unsafe {
            WindowHandle::borrow_raw(raw_window_handle::XlibWindowHandle::new(self.0.into()).into())
        })
    }
}
impl HasDisplayHandle for Window {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, raw_window_handle::HandleError> {
        // SAFETY: Xlib permits an omitted display pointer; start_x11 uses our connection.
        Ok(unsafe {
            DisplayHandle::borrow_raw(raw_window_handle::XlibDisplayHandle::new(None, 0).into())
        })
    }
}

#[derive(Default)]
struct Adapter(Option<StartTicket>);
impl ToolkitAdapter for Adapter {
    type Error = std::io::Error;
    fn outbound_route(&self) -> Option<SessionRoute> {
        Some(ROUTE)
    }
    fn schedule_outbound(&mut self, ticket: StartTicket) -> Result<(), RejectedStart<Self::Error>> {
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

struct Drag {
    conn: RustConnection,
    source: u32,
    editor: u32,
    target: u32,
    root: u32,
    time: u32,
    session: Option<X11Session>,
    controller: Controller,
}

impl Drag {
    fn new(version: u32) -> Self {
        Self::with_proxy(version, None)
    }

    fn with_proxy(version: u32, proxy_version: Option<u32>) -> Self {
        Self::with_preview(version, proxy_version, None)
    }

    fn with_preview(
        version: u32,
        proxy_version: Option<u32>,
        preview: Option<DragPreview>,
    ) -> Self {
        Self::with_preview_compositor(version, proxy_version, preview, false)
    }

    fn with_preview_compositor(
        version: u32,
        proxy_version: Option<u32>,
        preview: Option<DragPreview>,
        compositor: bool,
    ) -> Self {
        let (conn, screen) = x11rb::connect(None).expect("run with xvfb-run");
        let root = conn.setup().roots[screen].root;
        let source = conn.generate_id().unwrap();
        let target = conn.generate_id().unwrap();
        for (window, x) in [(source, 0), (target, 200)] {
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                x,
                0,
                100,
                100,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .unwrap()
            .check()
            .unwrap();
            conn.map_window(window).unwrap().check().unwrap();
        }
        let aware = conn
            .intern_atom(false, b"XdndAware")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        conn.change_property32(PropMode::REPLACE, target, aware, AtomEnum::ATOM, &[version])
            .unwrap()
            .check()
            .unwrap();
        if let Some(version) = proxy_version {
            let proxy = conn.generate_id().unwrap();
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                proxy,
                root,
                -100,
                -100,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap()
            .check()
            .unwrap();
            let property = conn
                .intern_atom(false, b"XdndProxy")
                .unwrap()
                .reply()
                .unwrap()
                .atom;
            for window in [target, proxy] {
                conn.change_property32(
                    PropMode::REPLACE,
                    window,
                    property,
                    AtomEnum::WINDOW,
                    &[proxy],
                )
                .unwrap()
                .check()
                .unwrap();
            }
            conn.change_property32(PropMode::REPLACE, proxy, aware, AtomEnum::ATOM, &[version])
                .unwrap()
                .check()
                .unwrap();
        }
        let time = loop {
            if let Event::PropertyNotify(event) = conn.wait_for_event().unwrap() {
                break event.time;
            }
        };
        if compositor {
            let atom = conn
                .intern_atom(false, format!("_NET_WM_CM_S{screen}").as_bytes())
                .unwrap()
                .reply()
                .unwrap()
                .atom;
            conn.set_selection_owner(source, atom, time)
                .unwrap()
                .check()
                .unwrap();
        }
        conn.warp_pointer(0u32, root, 0, 0, 0, 0, 250, 50)
            .unwrap()
            .check()
            .unwrap();
        let mut controller = Controller::new();
        let mut adapter = Adapter::default();
        let mut files = FileSet::try_from_paths([std::env::current_exe().unwrap()]).unwrap();
        if let Some(preview) = preview {
            files = files.with_preview(preview);
        }
        controller.start_outbound(&mut adapter, files).unwrap();
        let window = Window(source);
        let session = adapter
            .0
            .take()
            .unwrap()
            .start_x11(
                &conn,
                DragOrigin::from_window(&window).unwrap(),
                X11PointerEvent {
                    root,
                    root_x: 250,
                    root_y: 50,
                    time,
                },
            )
            .unwrap();
        let editor = source;
        let selection = conn
            .intern_atom(false, b"XdndSelection")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let source = conn
            .get_selection_owner(selection)
            .unwrap()
            .reply()
            .unwrap()
            .owner;
        Self {
            conn,
            source,
            editor,
            target,
            root,
            time,
            session: Some(session),
            controller,
        }
    }

    fn atom(&self, name: &[u8]) -> u32 {
        self.conn
            .intern_atom(false, name)
            .unwrap()
            .reply()
            .unwrap()
            .atom
    }

    fn messages(&self) -> Vec<ClientMessageEvent> {
        // A server roundtrip makes all earlier SendEvent requests observable.
        self.conn.get_input_focus().unwrap().reply().unwrap();
        let mut messages = Vec::new();
        while let Some(event) = self.conn.poll_for_event().unwrap() {
            if let Event::ClientMessage(event) = event {
                messages.push(event);
            }
        }
        messages
    }

    fn status(&mut self, format: u8, window: u32, action: u32) {
        self.status_from(self.target, format, window, action);
    }

    fn status_from(&mut self, target: u32, format: u8, window: u32, action: u32) {
        let event = ClientMessageEvent::new(
            format,
            window,
            self.atom(b"XdndStatus"),
            [target, 1, 0, 0, action],
        );
        self.session
            .as_mut()
            .unwrap()
            .handle_event(&self.conn, &Event::ClientMessage(event))
            .unwrap();
    }

    fn release(&mut self) {
        self.release_at(250, 50);
    }

    fn release_at(&mut self, x: i16, y: i16) {
        let event = ButtonReleaseEvent {
            detail: 1,
            root: self.root,
            event: self.editor,
            root_x: x,
            root_y: y,
            time: self.time,
            ..Default::default()
        };
        self.session
            .as_mut()
            .unwrap()
            .handle_event(&self.conn, &Event::ButtonRelease(event))
            .unwrap();
    }

    fn motion(&mut self, x: i16, y: i16) {
        self.conn
            .warp_pointer(0u32, self.root, 0, 0, 0, 0, x, y)
            .unwrap()
            .check()
            .unwrap();
        let event = x11rb::protocol::xproto::MotionNotifyEvent {
            root: self.root,
            event: self.editor,
            root_x: x,
            root_y: y,
            time: self.time,
            ..Default::default()
        };
        self.session
            .as_mut()
            .unwrap()
            .handle_event(&self.conn, &Event::MotionNotify(event))
            .unwrap();
        let owner = self
            .conn
            .get_selection_owner(self.atom(b"XdndSelection"))
            .unwrap()
            .reply()
            .unwrap()
            .owner;
        if owner != x11rb::NONE {
            self.source = owner;
        }
    }

    fn finish(&mut self, flags: u32, action: u32) -> LinuxOutcome {
        let event = ClientMessageEvent::new(
            32,
            self.source,
            self.atom(b"XdndFinished"),
            [self.target, flags, action, 0, 0],
        );
        self.session
            .as_mut()
            .unwrap()
            .handle_event(&self.conn, &Event::ClientMessage(event))
            .unwrap();
        self.terminal_outcome()
    }

    fn terminal_outcome(&mut self) -> LinuxOutcome {
        self.controller
            .update()
            .events()
            .iter()
            .find_map(|event| match event {
                SessionEvent::OutboundTerminal {
                    outcome: Outcome::Linux(outcome),
                    ..
                } => Some(*outcome),
                _ => None,
            })
            .expect("native finish must report a terminal outcome")
    }

    fn preview_window(&self) -> u32 {
        self.conn
            .query_tree(self.root)
            .unwrap()
            .reply()
            .unwrap()
            .children
            .into_iter()
            .find(|window| {
                *window != self.editor
                    && *window != self.target
                    && self
                        .conn
                        .get_window_attributes(*window)
                        .unwrap()
                        .reply()
                        .unwrap()
                        .class
                        == WindowClass::INPUT_OUTPUT
            })
            .expect("preview window")
    }

    fn restart(&mut self) {
        self.session.take().unwrap().cancel(&self.conn).unwrap();
        self.controller.update();
        let clock_atom = self.atom(b"_DND_TEST_CLOCK");
        self.conn
            .change_property32(
                PropMode::REPLACE,
                self.editor,
                clock_atom,
                AtomEnum::CARDINAL,
                &[1],
            )
            .unwrap()
            .check()
            .unwrap();
        self.time = loop {
            if let Event::PropertyNotify(event) = self.conn.wait_for_event().unwrap()
                && event.window == self.editor
                && event.atom == clock_atom
            {
                break event.time;
            }
        };
        let mut adapter = Adapter::default();
        self.controller
            .start_outbound(
                &mut adapter,
                FileSet::try_from_paths([std::env::current_exe().unwrap()]).unwrap(),
            )
            .unwrap();
        let window = Window(self.editor);
        self.session = Some(
            adapter
                .0
                .take()
                .unwrap()
                .start_x11(
                    &self.conn,
                    DragOrigin::from_window(&window).unwrap(),
                    X11PointerEvent {
                        root: self.root,
                        root_x: 250,
                        root_y: 50,
                        time: self.time,
                    },
                )
                .unwrap(),
        );
        self.source = self
            .conn
            .get_selection_owner(self.atom(b"XdndSelection"))
            .unwrap()
            .reply()
            .unwrap()
            .owner;
    }
}

impl Drop for Drag {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let _ = session.cancel(&self.conn);
        }
    }
}

#[test]
#[ignore = "requires an isolated X11 display: xvfb-run -a cargo test --test xdnd -- --ignored --test-threads=1"]
fn negotiates_the_targets_xdnd_version() {
    for version in [3, 4, 5, 8] {
        let drag = Drag::new(version);
        let enter = drag
            .messages()
            .into_iter()
            .find(|m| m.type_ == drag.atom(b"XdndEnter"))
            .unwrap();
        assert_eq!(enter.data.as_data32()[1] >> 24, version.min(5));
    }
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn proxy_awareness_supplies_the_negotiated_version() {
    for logical_version in [0, 5] {
        let drag = Drag::with_proxy(logical_version, Some(3));
        let enter = drag
            .messages()
            .into_iter()
            .find(|m| m.type_ == drag.atom(b"XdndEnter"))
            .unwrap();
        assert_eq!(enter.window, drag.target);
        assert_eq!(enter.data.as_data32()[1] >> 24, 3);
    }
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn old_targets_finish_without_version_five_success_fields() {
    for version in [3, 4] {
        let mut drag = Drag::new(version);
        drag.status(32, drag.source, drag.atom(b"XdndActionCopy"));
        drag.release();
        assert_eq!(drag.finish(0, 0), LinuxOutcome::Exported);
    }
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn version_five_finish_still_requires_success_and_copy() {
    for (flags, action, expected) in [
        (1, b"XdndActionCopy".as_slice(), LinuxOutcome::Exported),
        (
            0,
            b"XdndActionCopy".as_slice(),
            LinuxOutcome::Rejected(LinuxRejector::Target),
        ),
        (
            1,
            b"XdndActionMove".as_slice(),
            LinuxOutcome::Rejected(LinuxRejector::Target),
        ),
    ] {
        let mut drag = Drag::new(5);
        drag.status(32, drag.source, drag.atom(b"XdndActionCopy"));
        drag.release();
        let action = drag.atom(action);
        assert_eq!(drag.finish(flags, action), expected);
    }
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn copy_only_source_refuses_a_move_status() {
    let mut drag = Drag::new(5);
    drag.messages();
    drag.status(32, drag.source, drag.atom(b"XdndActionMove"));
    drag.release();
    assert!(
        !drag
            .messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndDrop"))
    );
    assert!(
        drag.controller
            .update()
            .events()
            .iter()
            .any(|event| matches!(
                event,
                SessionEvent::OutboundTerminal {
                    outcome: Outcome::Linux(LinuxOutcome::Rejected(LinuxRejector::Target)),
                    ..
                }
            ))
    );
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn unrelated_window_and_wrong_format_statuses_cannot_authorize_a_drop() {
    for (format, wrong_window) in [(32, true), (8, false), (16, false)] {
        let mut drag = Drag::new(5);
        drag.messages();
        drag.status(
            format,
            if wrong_window {
                drag.target
            } else {
                drag.source
            },
            drag.atom(b"XdndActionCopy"),
        );
        drag.release();
        assert!(
            !drag
                .messages()
                .iter()
                .any(|m| m.type_ == drag.atom(b"XdndDrop"))
        );
    }
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn release_waits_for_the_latest_position_status() {
    let mut drag = Drag::new(5);
    drag.messages();
    let copy = drag.atom(b"XdndActionCopy");
    drag.status(32, drag.source, copy);
    drag.motion(260, 50);
    drag.release_at(260, 50);
    let messages = drag.messages();
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.type_ == drag.atom(b"XdndPosition"))
            .count(),
        1
    );
    assert!(!messages.iter().any(|m| m.type_ == drag.atom(b"XdndDrop")));
    drag.status(32, drag.source, drag.atom(b"XdndActionMove"));
    assert!(
        !drag
            .messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndDrop"))
    );
    assert!(
        drag.controller
            .update()
            .events()
            .iter()
            .any(|event| matches!(
                event,
                SessionEvent::OutboundTerminal {
                    outcome: Outcome::Linux(LinuxOutcome::Rejected(LinuxRejector::Target)),
                    ..
                }
            ))
    );
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn pending_motion_coalesces_to_the_release_position() {
    let mut drag = Drag::new(5);
    drag.messages();
    drag.motion(260, 50);
    drag.motion(270, 50);
    drag.release_at(270, 50);
    assert!(drag.messages().is_empty());
    let copy = drag.atom(b"XdndActionCopy");
    drag.status(32, drag.source, copy);
    let messages = drag.messages();
    let positions: Vec<_> = messages
        .iter()
        .filter(|m| m.type_ == drag.atom(b"XdndPosition"))
        .collect();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].data.as_data32()[2], (270 << 16) | 50);
    assert!(!messages.iter().any(|m| m.type_ == drag.atom(b"XdndDrop")));
    drag.status(32, drag.source, copy);
    assert!(
        drag.messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndDrop"))
    );
    assert_eq!(drag.finish(1, copy), LinuxOutcome::Exported);
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn pending_motion_back_to_the_outstanding_position_needs_no_extra_status() {
    let mut drag = Drag::new(5);
    drag.messages();
    drag.motion(260, 50);
    drag.motion(250, 50);
    drag.release();
    assert!(drag.messages().is_empty());
    let copy = drag.atom(b"XdndActionCopy");
    drag.status(32, drag.source, copy);
    let messages = drag.messages();
    assert!(messages.iter().any(|m| m.type_ == drag.atom(b"XdndDrop")));
    assert!(
        !messages
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndPosition"))
    );
    assert_eq!(drag.finish(1, copy), LinuxOutcome::Exported);
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn departed_target_status_cannot_discharge_the_new_targets_wait() {
    let mut drag = Drag::new(5);
    drag.messages();
    let second = drag.conn.generate_id().unwrap();
    drag.conn
        .create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            second,
            drag.root,
            400,
            0,
            100,
            100,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new(),
        )
        .unwrap()
        .check()
        .unwrap();
    drag.conn.map_window(second).unwrap().check().unwrap();
    drag.conn
        .change_property32(
            PropMode::REPLACE,
            second,
            drag.atom(b"XdndAware"),
            AtomEnum::ATOM,
            &[5],
        )
        .unwrap()
        .check()
        .unwrap();
    drag.motion(450, 50);
    let transition = drag.messages();
    assert!(
        transition
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndLeave"))
    );
    assert!(
        transition
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndPosition") && m.window == second)
    );
    let copy = drag.atom(b"XdndActionCopy");
    drag.status(32, drag.source, copy);
    drag.release_at(450, 50);
    assert!(drag.messages().is_empty());
    drag.status_from(second, 32, drag.source, copy);
    assert!(
        drag.messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndDrop") && m.window == second)
    );
    drag.target = second;
    assert_eq!(drag.finish(1, copy), LinuxOutcome::Exported);
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn cancel_retires_a_released_drag_waiting_for_status() {
    let mut drag = Drag::new(5);
    drag.messages();
    drag.motion(270, 50);
    drag.release_at(270, 50);
    drag.session.take().unwrap().cancel(&drag.conn).unwrap();
    let messages = drag.messages();
    assert!(messages.iter().any(|m| m.type_ == drag.atom(b"XdndLeave")));
    assert!(!messages.iter().any(|m| m.type_ == drag.atom(b"XdndDrop")));
    assert_eq!(
        drag.conn
            .get_selection_owner(drag.atom(b"XdndSelection"))
            .unwrap()
            .reply()
            .unwrap()
            .owner,
        x11rb::NONE
    );
    assert!(
        drag.controller
            .update()
            .events()
            .iter()
            .any(|event| matches!(
                event,
                SessionEvent::OutboundTerminal {
                    outcome: Outcome::Linux(LinuxOutcome::Cancelled),
                    ..
                }
            ))
    );
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn dead_proxy_falls_back_to_the_logical_target() {
    let mut drag = Drag::new(5);
    drag.messages();
    let dead = drag.conn.generate_id().unwrap();
    // An allocated but never-created XID is a deterministic stale proxy.
    drag.conn
        .change_property32(
            PropMode::REPLACE,
            drag.target,
            drag.atom(b"XdndProxy"),
            AtomEnum::WINDOW,
            &[dead],
        )
        .unwrap()
        .check()
        .unwrap();
    drag.motion(250, 50);
    assert!(drag.messages().is_empty());
    let copy = drag.atom(b"XdndActionCopy");
    drag.status(32, drag.source, copy);
    drag.release();
    assert!(
        drag.messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndDrop"))
    );
    assert_eq!(drag.finish(1, copy), LinuxOutcome::Exported);
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn multi_window_proxy_property_is_ignored() {
    let mut drag = Drag::with_proxy(5, Some(3));
    drag.messages();
    let proxy_atom = drag.atom(b"XdndProxy");
    let proxy = drag
        .conn
        .get_property(false, drag.target, proxy_atom, AtomEnum::WINDOW, 0, 1)
        .unwrap()
        .reply()
        .unwrap()
        .value32()
        .unwrap()
        .next()
        .unwrap();
    drag.conn
        .change_property32(
            PropMode::REPLACE,
            drag.target,
            proxy_atom,
            AtomEnum::WINDOW,
            &[proxy, proxy],
        )
        .unwrap()
        .check()
        .unwrap();
    drag.motion(250, 50);
    let messages = drag.messages();
    let enter = messages
        .iter()
        .find(|m| m.type_ == drag.atom(b"XdndEnter"))
        .unwrap();
    assert_eq!(enter.data.as_data32()[1] >> 24, 5);
    let copy = drag.atom(b"XdndActionCopy");
    drag.status(32, drag.source, copy);
    drag.release();
    assert_eq!(drag.finish(1, copy), LinuxOutcome::Exported);
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn release_uses_its_coordinates_even_without_a_motion_callback() {
    let mut drag = Drag::new(5);
    drag.messages();
    let copy = drag.atom(b"XdndActionCopy");
    drag.status(32, drag.source, copy);
    drag.conn
        .warp_pointer(0u32, drag.root, 0, 0, 0, 0, 280, 50)
        .unwrap()
        .check()
        .unwrap();
    drag.release_at(280, 50);
    let messages = drag.messages();
    assert!(messages.iter().any(
        |m| m.type_ == drag.atom(b"XdndPosition") && m.data.as_data32()[2] == (280 << 16) | 50
    ));
    assert!(!messages.iter().any(|m| m.type_ == drag.atom(b"XdndDrop")));
    drag.status(32, drag.source, copy);
    assert!(
        drag.messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndDrop"))
    );
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn preview_cannot_intercept_the_pointer_or_become_a_drop_target() {
    let drag = Drag::with_preview(
        5,
        None,
        Some(DragPreview::Image {
            pixels: vec![255; PREVIEW_WIDTH * PREVIEW_HEIGHT * 4],
        }),
    );
    let preview = drag.preview_window();
    let shape = x11rb::protocol::shape::get_rectangles(
        &drag.conn,
        preview,
        x11rb::protocol::shape::SK::INPUT,
    )
    .unwrap()
    .reply()
    .unwrap();
    assert!(shape.rectangles.is_empty());
    // Place the preview directly over the target and pointer.
    drag.conn
        .configure_window(preview, &ConfigureWindowAux::new().x(200).y(0))
        .unwrap()
        .check()
        .unwrap();
    let pointer = drag.conn.query_pointer(drag.root).unwrap().reply().unwrap();
    assert_eq!(pointer.child, drag.target);
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn retired_status_and_selection_clear_cannot_authorize_a_second_visit() {
    let mut drag = Drag::new(5);
    drag.messages();
    let first = drag.source;
    drag.motion(350, 50);
    drag.motion(250, 50);
    let current = drag.source;
    assert_ne!(first, current);
    assert!(
        drag.conn.get_geometry(first).unwrap().reply().is_ok(),
        "retired source survives until teardown"
    );
    drag.messages();
    let clear = SelectionClearEvent {
        owner: first,
        selection: drag.atom(b"XdndSelection"),
        time: drag.time,
        ..Default::default()
    };
    assert_eq!(
        drag.session
            .as_mut()
            .unwrap()
            .handle_event(&drag.conn, &Event::SelectionClear(clear))
            .unwrap(),
        X11SessionStatus::Active
    );
    let copy = drag.atom(b"XdndActionCopy");
    drag.status(32, first, copy);
    drag.release();
    assert!(
        !drag
            .messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndDrop"))
    );
    drag.status(32, current, copy);
    assert!(
        drag.messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndDrop"))
    );
    assert_eq!(drag.finish(1, copy), LinuxOutcome::Exported);
    assert!(drag.conn.get_geometry(first).unwrap().reply().is_err());
    assert!(drag.conn.get_geometry(current).unwrap().reply().is_err());
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn a_previous_sessions_reply_cannot_authorize_the_next_drag() {
    let mut drag = Drag::new(5);
    drag.messages();
    let previous = drag.source;
    drag.restart();
    assert_ne!(drag.source, previous);
    drag.messages();
    let copy = drag.atom(b"XdndActionCopy");
    drag.status(32, previous, copy);
    drag.release();
    assert!(
        !drag
            .messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndDrop"))
    );
    drag.status(32, drag.source, copy);
    assert!(
        drag.messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndDrop"))
    );
    assert_eq!(drag.finish(1, copy), LinuxOutcome::Exported);
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn missing_status_expires_without_a_drop_and_releases_native_state() {
    let mut drag = Drag::new(5);
    drag.messages();
    drag.release();
    let now = Instant::now();
    assert_eq!(
        drag.session
            .as_mut()
            .unwrap()
            .poll_at(&drag.conn, now + Duration::from_secs(1))
            .unwrap(),
        X11SessionStatus::Active
    );
    assert_eq!(
        drag.session
            .as_mut()
            .unwrap()
            .poll_at(&drag.conn, now + Duration::from_secs(3))
            .unwrap(),
        X11SessionStatus::Finished
    );
    let messages = drag.messages();
    assert!(messages.iter().any(|m| m.type_ == drag.atom(b"XdndLeave")));
    assert!(!messages.iter().any(|m| m.type_ == drag.atom(b"XdndDrop")));
    assert_eq!(drag.terminal_outcome(), LinuxOutcome::Cancelled);
    assert_eq!(
        drag.conn
            .get_selection_owner(drag.atom(b"XdndSelection"))
            .unwrap()
            .reply()
            .unwrap()
            .owner,
        x11rb::NONE
    );
    assert!(
        drag.conn
            .get_geometry(drag.source)
            .unwrap()
            .reply()
            .is_err()
    );
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn missing_finished_expires_as_indeterminate() {
    let mut drag = Drag::new(5);
    drag.messages();
    let copy = drag.atom(b"XdndActionCopy");
    drag.status(32, drag.source, copy);
    drag.release();
    assert!(
        drag.messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndDrop"))
    );
    let now = Instant::now();
    assert_eq!(
        drag.session
            .as_mut()
            .unwrap()
            .poll_at(&drag.conn, now + Duration::from_secs(19))
            .unwrap(),
        X11SessionStatus::Active
    );
    assert_eq!(
        drag.session
            .as_mut()
            .unwrap()
            .poll_at(&drag.conn, now + Duration::from_secs(21))
            .unwrap(),
        X11SessionStatus::Finished
    );
    assert_eq!(drag.terminal_outcome(), LinuxOutcome::Indeterminate);
    assert!(
        !drag
            .messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndLeave"))
    );
    assert_eq!(
        drag.conn
            .get_selection_owner(drag.atom(b"XdndSelection"))
            .unwrap()
            .reply()
            .unwrap()
            .owner,
        x11rb::NONE
    );
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn losing_selection_does_not_clear_the_new_owners_data() {
    let mut drag = Drag::new(5);
    drag.conn
        .set_selection_owner(
            drag.editor,
            drag.atom(b"XdndSelection"),
            x11rb::CURRENT_TIME,
        )
        .unwrap()
        .check()
        .unwrap();
    let clear = SelectionClearEvent {
        owner: drag.source,
        selection: drag.atom(b"XdndSelection"),
        time: drag.time,
        ..Default::default()
    };
    assert_eq!(
        drag.session
            .as_mut()
            .unwrap()
            .handle_event(&drag.conn, &Event::SelectionClear(clear))
            .unwrap(),
        X11SessionStatus::Finished
    );
    assert_eq!(
        drag.conn
            .get_selection_owner(drag.atom(b"XdndSelection"))
            .unwrap()
            .reply()
            .unwrap()
            .owner,
        drag.editor
    );
    assert_eq!(drag.terminal_outcome(), LinuxOutcome::Cancelled);
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn bitmap_geometry_uses_backing_pixels_and_declared_cursor_hotspot() {
    let bitmap = DragImage::new(10, 6, vec![255; 10 * 6 * 4])
        .unwrap()
        .with_geometry([5.0, 3.0], [1.0, 2.0])
        .unwrap();
    let mut drag = Drag::with_preview(5, None, Some(DragPreview::Bitmap(bitmap)));
    let preview = drag.preview_window();
    let geometry = drag.conn.get_geometry(preview).unwrap().reply().unwrap();
    assert_eq!(
        (geometry.x, geometry.y, geometry.width, geometry.height),
        (248, 46, 10, 6)
    );
    drag.motion(260, 60);
    let geometry = drag.conn.get_geometry(preview).unwrap().reply().unwrap();
    assert_eq!((geometry.x, geometry.y), (258, 56));
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn root_visual_fallback_removes_transparent_pixels_and_uploads_straight_color() {
    let bitmap = DragImage::new(2, 1, vec![0, 0, 0, 0, 0, 0, 128, 128]).unwrap();
    let drag = Drag::with_preview(5, None, Some(DragPreview::Bitmap(bitmap)));
    let preview = drag.preview_window();
    let shape = x11rb::protocol::shape::get_rectangles(
        &drag.conn,
        preview,
        x11rb::protocol::shape::SK::BOUNDING,
    )
    .unwrap()
    .reply()
    .unwrap();
    assert_eq!(shape.rectangles.len(), 1);
    let rectangle = shape.rectangles[0];
    assert_eq!(
        (rectangle.x, rectangle.y, rectangle.width, rectangle.height),
        (1, 0, 1, 1)
    );
    let attrs = drag
        .conn
        .get_window_attributes(preview)
        .unwrap()
        .reply()
        .unwrap();
    let visual = drag
        .conn
        .setup()
        .roots
        .iter()
        .flat_map(|screen| &screen.allowed_depths)
        .flat_map(|depth| &depth.visuals)
        .find(|visual| visual.visual_id == attrs.visual)
        .unwrap();
    let image = drag
        .conn
        .get_image(ImageFormat::Z_PIXMAP, preview, 1, 0, 1, 1, u32::MAX)
        .unwrap()
        .reply()
        .unwrap();
    let mut bytes = [0; 4];
    let n = usize::from(
        drag.conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|format| format.depth == image.depth)
            .unwrap()
            .bits_per_pixel
            / 8,
    );
    if drag.conn.setup().image_byte_order == ImageOrder::LSB_FIRST {
        bytes[..n].copy_from_slice(&image.data[..n]);
    } else {
        bytes[4 - n..].copy_from_slice(&image.data[..n]);
    }
    let value = if drag.conn.setup().image_byte_order == ImageOrder::LSB_FIRST {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    };
    assert_eq!(
        value & (visual.red_mask | visual.green_mask | visual.blue_mask),
        visual.red_mask
    );
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn compositor_selection_enables_verified_argb_preview_visual() {
    let bitmap = DragImage::new(1, 1, vec![32, 64, 96, 128]).unwrap();
    let drag = Drag::with_preview_compositor(5, None, Some(DragPreview::Bitmap(bitmap)), true);
    let preview = drag.preview_window();
    let geometry = drag.conn.get_geometry(preview).unwrap().reply().unwrap();
    assert_eq!(geometry.depth, 32);
    let image = drag
        .conn
        .get_image(ImageFormat::Z_PIXMAP, preview, 0, 0, 1, 1, u32::MAX)
        .unwrap()
        .reply()
        .unwrap();
    let expected = if drag.conn.setup().image_byte_order == ImageOrder::LSB_FIRST {
        [32, 64, 96, 128]
    } else {
        [128, 96, 64, 32]
    };
    assert_eq!(image.data, expected);
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn stationary_scroll_positions_remain_serialized_until_release() {
    let mut drag = Drag::new(5);
    drag.messages();
    let copy = drag.atom(b"XdndActionCopy");
    drag.status(32, drag.source, copy);
    let now = Instant::now();
    drag.session
        .as_mut()
        .unwrap()
        .poll_at(&drag.conn, now + Duration::from_millis(120))
        .unwrap();
    let positions = drag.messages();
    assert_eq!(
        positions
            .iter()
            .filter(|m| m.type_ == drag.atom(b"XdndPosition"))
            .count(),
        1
    );
    assert_eq!(positions[0].data.as_data32()[2], (250 << 16) | 50);
    drag.session
        .as_mut()
        .unwrap()
        .poll_at(&drag.conn, now + Duration::from_millis(250))
        .unwrap();
    assert!(drag.messages().is_empty());
    drag.release();
    assert!(drag.messages().is_empty());
    drag.status(32, drag.source, copy);
    assert!(
        drag.messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndDrop"))
    );
    assert_eq!(drag.finish(1, copy), LinuxOutcome::Exported);
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn a_nonempty_no_more_positions_rectangle_suppresses_stationary_ticks() {
    let mut drag = Drag::new(5);
    drag.messages();
    let copy = drag.atom(b"XdndActionCopy");
    let status = ClientMessageEvent::new(
        32,
        drag.source,
        drag.atom(b"XdndStatus"),
        [drag.target, 1, 200 << 16, (100 << 16) | 100, copy],
    );
    drag.session
        .as_mut()
        .unwrap()
        .handle_event(&drag.conn, &Event::ClientMessage(status))
        .unwrap();
    drag.session
        .as_mut()
        .unwrap()
        .poll_at(&drag.conn, Instant::now() + Duration::from_secs(1))
        .unwrap();
    assert!(drag.messages().is_empty());
    drag.release();
    assert!(
        drag.messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndDrop"))
    );
    assert_eq!(drag.finish(1, copy), LinuxOutcome::Exported);
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn target_lookup_uses_release_snapshot_when_live_pointer_has_already_left() {
    let mut drag = Drag::new(5);
    drag.messages();
    let copy = drag.atom(b"XdndActionCopy");
    drag.status(32, drag.source, copy);
    drag.conn
        .warp_pointer(0u32, drag.root, 0, 0, 0, 0, 350, 50)
        .unwrap()
        .check()
        .unwrap();
    drag.release_at(280, 50);
    let messages = drag.messages();
    assert!(messages.iter().any(
        |m| m.type_ == drag.atom(b"XdndPosition") && m.data.as_data32()[2] == (280 << 16) | 50
    ));
    assert!(
        !messages
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndDrop") || m.type_ == drag.atom(b"XdndLeave"))
    );
    drag.status(32, drag.source, copy);
    assert!(
        drag.messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndDrop"))
    );
    assert_eq!(drag.finish(1, copy), LinuxOutcome::Exported);
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn teardown_after_drop_does_not_send_leave_or_claim_cancellation() {
    let mut drag = Drag::new(5);
    drag.messages();
    let copy = drag.atom(b"XdndActionCopy");
    drag.status(32, drag.source, copy);
    drag.release();
    assert!(
        drag.messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndDrop"))
    );
    drag.session.take().unwrap().cancel(&drag.conn).unwrap();
    assert!(
        !drag
            .messages()
            .iter()
            .any(|m| m.type_ == drag.atom(b"XdndLeave"))
    );
    assert_eq!(drag.terminal_outcome(), LinuxOutcome::Indeterminate);
    assert_eq!(
        drag.conn
            .get_selection_owner(drag.atom(b"XdndSelection"))
            .unwrap()
            .reply()
            .unwrap()
            .owner,
        x11rb::NONE
    );
}

#[test]
#[ignore = "requires an isolated X11 display"]
fn terminal_cleanup_cannot_clear_a_peer_claim_at_the_ownership_boundary() {
    // Exercise actual session teardown through a real X11 connection. The
    // interposer lets a second client take ownership after an old GetOwner
    // reply, or immediately before DestroyWindow when there is no owner query.
    for mode in 0..4 {
        let mut drag = Drag::new(5);
        let copy = drag.atom(b"XdndActionCopy");
        if mode >= 2 {
            drag.status(32, drag.source, copy);
            drag.release();
        } else if mode == 1 {
            drag.release();
        }
        let (peer, _) = x11rb::connect(None).unwrap();
        let peer_window = peer.generate_id().unwrap();
        peer.create_window(
            0,
            peer_window,
            drag.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )
        .unwrap()
        .check()
        .unwrap();
        let selection = drag.atom(b"XdndSelection");
        let race = selection_race::SelectionRace::new(
            &drag.conn,
            &peer,
            drag.source,
            peer_window,
            selection,
        );
        let expected = match mode {
            0 => {
                drag.session.take().unwrap().cancel(&race).unwrap();
                LinuxOutcome::Cancelled
            }
            1 => {
                assert_eq!(
                    drag.session
                        .as_mut()
                        .unwrap()
                        .poll_at(&race, Instant::now() + Duration::from_secs(3))
                        .unwrap(),
                    X11SessionStatus::Finished
                );
                LinuxOutcome::Cancelled
            }
            2 => {
                let finished = ClientMessageEvent::new(
                    32,
                    drag.source,
                    drag.atom(b"XdndFinished"),
                    [drag.target, 1, copy, 0, 0],
                );
                assert_eq!(
                    drag.session
                        .as_mut()
                        .unwrap()
                        .handle_event(&race, &Event::ClientMessage(finished))
                        .unwrap(),
                    X11SessionStatus::Finished
                );
                LinuxOutcome::Exported
            }
            3 => {
                assert_eq!(
                    drag.session
                        .as_mut()
                        .unwrap()
                        .poll_at(&race, Instant::now() + Duration::from_secs(21))
                        .unwrap(),
                    X11SessionStatus::Finished
                );
                LinuxOutcome::Indeterminate
            }
            _ => unreachable!(),
        };
        assert!(
            race.took_over(),
            "the peer actually raced cleanup, mode {mode}"
        );
        assert_eq!(
            peer.get_selection_owner(selection)
                .unwrap()
                .reply()
                .unwrap()
                .owner,
            peer_window,
            "cleanup erased the peer's selection, mode {mode}"
        );
        assert!(
            !race.sent_explicit_clear.get(),
            "cleanup sent a non-atomic clear, mode {mode}"
        );
        assert!(
            drag.conn
                .get_geometry(drag.source)
                .unwrap()
                .reply()
                .is_err()
        );
        assert_eq!(drag.terminal_outcome(), expected);
    }
}
