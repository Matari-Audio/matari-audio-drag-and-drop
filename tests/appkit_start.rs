// Run on a logged-in Mac: cargo test --test appkit_start
// A custom harness keeps AppKit on the process's real main thread.
#[cfg(not(target_os = "macos"))]
fn main() {
    println!("AppKit start regression requires macOS");
}

#[cfg(target_os = "macos")]
fn main() {
    appkit::run();
}

#[cfg(target_os = "macos")]
mod appkit {
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeSet;
    use std::ffi::{CStr, CString, c_char, c_void};
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::ptr::NonNull;

    use matari_audio_drag_and_drop::*;
    use objc2::rc::{Retained, Weak, autoreleasepool};
    use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, ProtocolObject};
    use objc2::{
        ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
    };
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSBitmapImageRep, NSDraggingItem, NSDraggingSession,
        NSDraggingSource, NSEvent, NSEventModifierFlags, NSEventType, NSImage, NSView, NSWindow,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSArray, NSObjectProtocol, NSPoint, NSRect, NSSize, NSURL};
    use raw_window_handle::{DisplayHandle, HasDisplayHandle, HasWindowHandle, WindowHandle};

    #[derive(Clone, Copy)]
    struct Fixture {
        handle: *mut c_void,
        attempt: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char) -> u32,
    }

    struct Observation {
        class: &'static AnyClass,
        callback: *const c_void,
        image: usize,
    }

    fn image_at(address: *const c_void) -> usize {
        let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
        // SAFETY: Real method address, writable dladdr output.
        assert_ne!(unsafe { libc::dladdr(address, info.as_mut_ptr()) }, 0);
        // SAFETY: Initialized by successful dladdr.
        unsafe { info.assume_init() }.dli_fbase as usize
    }

    fn load_fixtures() -> Vec<Fixture> {
        let Some(paths) = std::env::var_os("MATARI_APPKIT_IMAGES") else {
            return Vec::new();
        };
        let paths = std::env::split_paths(&paths).collect::<Vec<_>>();
        assert_eq!(
            paths.len(),
            2,
            "supply two independently linked fixture images"
        );
        paths
            .into_iter()
            .map(|path| {
                let path = CString::new(path.as_os_str().as_bytes()).unwrap();
                // SAFETY: Valid library pathname; test owns loader reference until cleanup.
                let handle =
                    unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
                assert!(!handle.is_null(), "cannot load fixture image");
                // SAFETY: Exact exported C ABI function declared by the fixture source.
                let symbol = unsafe { libc::dlsym(handle, c"matari_test_rejected_start".as_ptr()) };
                assert!(!symbol.is_null(), "fixture export missing");
                // SAFETY: Symbol is the fixture's function with this exact C signature.
                let attempt = unsafe {
                    std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char) -> u32,
                    >(symbol)
                };
                Fixture { handle, attempt }
            })
            .collect()
    }

    struct Ivars {
        path: PathBuf,
        controller: RefCell<Controller>,
        sources: RefCell<Vec<Weak<ProtocolObject<dyn NSDraggingSource>>>>,
        fixtures: Vec<Fixture>,
        observations: RefCell<Vec<Observation>>,
        attempts: Cell<usize>,
        flipped: Cell<bool>,
    }

    define_class!(
        // SAFETY: NSView subclass, initialized ivars, and main-thread methods.
        #[unsafe(super = NSView)]
        #[thread_kind = MainThreadOnly]
        #[ivars = Ivars]
        struct RejectingView;

        // SAFETY: Inherited NSObject implementation.
        unsafe impl NSObjectProtocol for RejectingView {}

        impl RejectingView {
            #[unsafe(method(isFlipped))]
            fn is_flipped(&self) -> bool { self.ivars().flipped.get() }

            #[unsafe(method(beginDraggingSessionWithItems:event:source:))]
            fn reject_start(
                &self,
                _items: &NSArray<NSDraggingItem>,
                _event: &NSEvent,
                source: &ProtocolObject<dyn NSDraggingSource>,
            ) -> *mut NSDraggingSession {
                self.ivars().attempts.set(self.ivars().attempts.get() + 1);
                self.ivars().sources.borrow_mut().push(Weak::new(source));
                let item = _items.firstObject().expect("file item");
                // SAFETY: Backend constructed this item with the exact file NSURL.
                let url: Retained<NSURL> = unsafe { Retained::cast_unchecked(item.item()) };
                // SAFETY: NSURL owns a valid NUL-terminated filesystem representation.
                let advertised = unsafe { CStr::from_ptr(url.fileSystemRepresentation().as_ptr()) };
                assert_eq!(advertised.to_bytes(), self.ivars().path.as_os_str().as_bytes(), "preserve exported symlink basename and URL bytes");
                let is_main = (self.ivars().attempts.get() - 1).is_multiple_of(1 + self.ivars().fixtures.len());
                let location = self.convertPoint_fromView(_event.locationInWindow(), None);
                if is_main {
                    let frame = item.draggingFrame();
                    assert_eq!(frame.size, NSSize::new(40.0, 30.0));
                    assert_eq!(frame.origin.x + 7.0, location.x);
                    if self.ivars().flipped.get() {
                        assert_eq!(frame.origin.y + 11.0, location.y);
                    } else {
                        assert_eq!(frame.origin.y + frame.size.height - 11.0, location.y);
                    }
                    let components = item.imageComponents().expect("requested image components");
                    let contents = components.firstObject().expect("requested image component").contents().expect("NSImage");
                    // SAFETY: Backend supplied exactly an NSImage as the item contents.
                    let image: Retained<NSImage> = unsafe { Retained::cast_unchecked(contents) };
                    assert_eq!(image.size(), NSSize::new(40.0, 30.0));
                    let representation = image.representations().firstObject().expect("bitmap representation");
                    assert_eq!((representation.pixelsWide(), representation.pixelsHigh()), (8, 6));
                    assert_eq!(representation.size(), image.size());
                    // SAFETY: Backend creates an NSBitmapImageRep with this verified
                    // geometry and stores its exact checked 4-byte raster.
                    let bitmap: Retained<NSBitmapImageRep> = unsafe { Retained::cast_unchecked(representation) };
                    // SAFETY: Verified real bitmap with a non-null data pointer.
                    assert!(!bitmap.bitmapData().is_null());
                    // SAFETY: Verified 8x6 RGBA bitmap with live non-null storage.
                    let pixels = unsafe { std::slice::from_raw_parts(bitmap.bitmapData(), 8 * 6 * 4) };
                    assert_eq!(&pixels[..4], &[200, 30, 20, 255]);
                    assert_eq!(&pixels[pixels.len() - 4..], &[20, 30, 200, 255]);
                } else {
                    assert_eq!(item.draggingFrame().size, NSSize::new(32.0, 32.0));
                    assert!(item.imageComponents().is_some(), "visible file-icon fallback");
                }
                // SAFETY: ProtocolObject is an Objective-C object; class identity
                // and runtime IMP can be inspected without dereferencing Rust state.
                let object = unsafe { &*std::ptr::from_ref(source).cast::<AnyObject>() };
                let class = object.class();
                assert!(class.name().to_bytes().starts_with(b"MatariExternalDragSource_"));
                let callback = class.instance_method(sel!(draggingSession:endedAtPoint:operation:))
                    .expect("source callback").implementation() as *const () as *const c_void;
                self.ivars().observations.borrow_mut().push(Observation {
                    class, callback, image: image_at(callback),
                });
                std::ptr::null_mut()
            }

            #[unsafe(method(mouseDown:))]
            fn mouse_down(&self, event: &NSEvent) {
                let mut controller = self.ivars().controller.borrow_mut();
                let mut adapter = Adapter(None);
                let files = FileSet::try_from_paths([self.ivars().path.clone()])
                    .unwrap()
                    .with_preview({
                        let mut pixels = [20, 30, 200, 255].repeat(8 * 6);
                        let len = pixels.len();
                        pixels[len - 4..].copy_from_slice(&[200, 30, 20, 255]);
                        DragPreview::Bitmap(DragImage::new(8, 6, pixels).unwrap()
                            .with_geometry([40.0, 30.0], [7.0, 11.0]).unwrap())
                    });
                let session = controller.start_outbound(&mut adapter, files).unwrap();
                // SAFETY: This fixture owns the real view and event; start is
                // synchronous inside this view's main-thread mouseDown callback.
                let origin = unsafe {
                    DragOrigin::with_appkit_event(self, NonNull::from(event).cast())
                }
                .unwrap();
                let error = adapter.0.take().unwrap().start(&mut NativeRuntime, origin);
                assert!(matches!(error, Err(ProtocolError::Runtime(_))));
                let update = controller.update();
                // No Started/Attached event may escape a rejected native start.
                assert!(matches!(
                    update.events(),
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
                for fixture in &self.ivars().fixtures {
                    let path = CString::new(self.ivars().path.as_os_str().as_bytes()).unwrap();
                    // SAFETY: Live actual view/event in this mouseDown callback,
                    // fixture loader references remain live through all calls.
                    assert_eq!(unsafe { (fixture.attempt)(
                        std::ptr::from_ref::<NSView>(self).cast_mut().cast(),
                        std::ptr::from_ref(event).cast_mut().cast(), path.as_ptr(),
                    ) }, 1, "independent image native rejection");
                }
            }
        }
    );

    impl HasWindowHandle for RejectingView {
        fn window_handle(&self) -> Result<WindowHandle<'_>, raw_window_handle::HandleError> {
            let view: &NSView = self;
            // SAFETY: The fixture owns this actual NSView through native start.
            Ok(unsafe {
                WindowHandle::borrow_raw(
                    raw_window_handle::AppKitWindowHandle::new(NonNull::from(view).cast()).into(),
                )
            })
        }
    }

    impl HasDisplayHandle for RejectingView {
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

    struct TempFile(PathBuf);
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    pub fn run() {
        let mtm = MainThreadMarker::new().expect("AppKit requires the real main thread");
        let path = std::env::temp_dir().join(format!(
            "matari-appkit-rejection-{}.wav",
            std::process::id()
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let path = TempFile(path);
        file.write_all(b"fixture").unwrap();
        drop(file);
        let alias = TempFile(path.0.with_extension("export alias.wav"));
        std::os::unix::fs::symlink(&path.0, &alias.0).unwrap();
        autoreleasepool(|_| {
            let _app = NSApplication::sharedApplication(mtm);
            // Occupy the old process-global name before any backend start. A
            // successful test now demonstrates that we do not share that class.
            let _poison = ClassBuilder::new(
                c"MatariExternalDragSource",
                objc2_foundation::NSObject::class(),
            )
            .expect("standalone fixture fixed-name class")
            .register();
            let fixtures = load_fixtures();
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(300.0, 200.0));
            let allocated = mtm.alloc::<RejectingView>().set_ivars(Ivars {
                path: alias.0.clone(),
                controller: RefCell::new(Controller::new()),
                sources: RefCell::new(Vec::new()),
                fixtures,
                observations: RefCell::new(Vec::new()),
                attempts: Cell::new(0),
                flipped: Cell::new(false),
            });
            // SAFETY: Correct superclass initializer on an initialized allocation.
            let view: Retained<RejectingView> =
                unsafe { msg_send![super(allocated), initWithFrame: frame] };
            // SAFETY: Window and all following AppKit accesses stay on main.
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(mtm),
                    frame,
                    NSWindowStyleMask::Borderless,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            let content = NSView::initWithFrame(NSView::alloc(mtm), frame);
            window.setContentView(Some(&content));
            content.addSubview(&view);
            view.setFrameOrigin(NSPoint::new(25.0, 30.0));
            for event_number in 1_isize..=2 {
                view.ivars().flipped.set(event_number == 2);
                if event_number == 2 {
                    // Empty regular files remain valid payloads on every backend.
                    std::fs::write(&path.0, []).unwrap();
                }
                autoreleasepool(|_| {
                    // SAFETY: Exact NSEvent mouse-event factory selector/types;
                    // its obsolete graphics-context parameter must be nil.
                    let event: Option<Retained<NSEvent>> = unsafe {
                        msg_send![NSEvent::class(),
                            mouseEventWithType: NSEventType::LeftMouseDown,
                            location: NSPoint::new(20.0, 20.0),
                            modifierFlags: NSEventModifierFlags::empty(),
                            timestamp: 0.0_f64,
                            windowNumber: window.windowNumber(),
                            context: std::ptr::null::<AnyObject>(),
                            eventNumber: event_number,
                            clickCount: 1_isize,
                            pressure: 1.0_f32
                        ]
                    };
                    view.mouseDown(&event.expect("native NSEvent fixture"));
                });
                let sources = view.ivars().sources.borrow();
                assert!(!sources.is_empty(), "start reached AppKit");
                assert!(
                    sources.iter().all(|source| source.load().is_none()),
                    "every rejected source deallocated"
                );
            }
            let count = 1 + view.ivars().fixtures.len();
            assert_eq!(view.ivars().attempts.get(), 2 * count);
            let observations = view.ivars().observations.borrow();
            let classes = observations
                .iter()
                .map(|o| std::ptr::from_ref(o.class) as usize)
                .collect::<BTreeSet<_>>();
            let images = observations
                .iter()
                .map(|o| o.image)
                .collect::<BTreeSet<_>>();
            assert_eq!(
                classes.len(),
                count,
                "one distinct registered class per linked image"
            );
            assert_eq!(
                images.len(),
                count,
                "IMPs belong to independent linked images"
            );
            for fixture in &view.ivars().fixtures {
                // SAFETY: Release only this test's loader reference. The backend
                // has established process-lifetime residence before registration.
                assert_eq!(unsafe { libc::dlclose(fixture.handle) }, 0);
            }
            for observation in &*observations {
                assert_eq!(
                    image_at(observation.callback),
                    observation.image,
                    "callback stays mapped after caller unload"
                );
                // SAFETY: Resident registered NSObject subclass. Creating and
                // releasing a zero-state instance exercises its future dealloc
                // dispatch after all caller-owned image handles were closed.
                let object: Retained<AnyObject> = unsafe { msg_send![observation.class, new] };
                drop(object);
            }
            window.setContentView(None);
        });
        if std::env::var_os("MATARI_APPKIT_IMAGES").is_none() {
            println!(
                "AppKit independent-library residency case skipped: MATARI_APPKIT_IMAGES is unset"
            );
        }
        println!(
            "AppKit nil start: error, terminal failure, source cleanup, retry and fixed-name isolation passed"
        );
    }
}
