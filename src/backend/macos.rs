use std::ffi::{CString, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, ClassBuilder, Ivar, ProtocolObject, Sel};
use objc2::{AnyThread, ClassType, MainThreadMarker, msg_send, sel};
use objc2_app_kit::{
    NSBitmapImageRep, NSDeviceRGBColorSpace, NSDragOperation, NSDraggingContext, NSDraggingItem,
    NSDraggingSession, NSDraggingSource, NSEvent, NSImage, NSImageRep, NSView, NSWorkspace,
};
use objc2_foundation::{NSArray, NSObject, NSPoint, NSRect, NSSize, NSString, NSURL};
use raw_window_handle::RawWindowHandle;

use super::ExternalDragPayload;
use super::{DragWindow, ExternalDragError};
use crate::{DragPreview, Outcome, PreviewFailureStage, PreviewStatus, SessionReporter};

struct DragSourceState {
    reporter: Mutex<Option<SessionReporter>>,
    owns_self: AtomicBool,
}

struct SourceClass {
    class: &'static AnyClass,
    state: &'static Ivar,
}

// Each linked image has its own cell, native class and Rust state layout.
static SOURCE_CLASS: OnceLock<Result<SourceClass, String>> = OnceLock::new();

fn source_class(_mtm: MainThreadMarker) -> Result<&'static SourceClass, ExternalDragError> {
    SOURCE_CLASS
        .get_or_init(|| {
            pin_callback_image()?;
            let identity = ptr::from_ref(&SOURCE_CLASS) as usize;
            let name = CString::new(format!("MatariExternalDragSource_{identity:x}"))
                .map_err(|error| error.to_string())?;
            let mut builder = ClassBuilder::new(&name, NSObject::class()).ok_or_else(|| {
                "cannot allocate an isolated AppKit drag source class".to_string()
            })?;
            builder.add_ivar::<*mut c_void>(c"_matariState");
            let protocol = AnyProtocol::get(c"NSDraggingSource")
                .ok_or_else(|| "AppKit NSDraggingSource protocol is unavailable".to_string())?;
            builder.add_protocol(protocol);
            // SAFETY: Exact NSDraggingSource selectors/encodings; the state ivar
            // is initialized before any source is passed to AppKit. The image
            // was pinned before publishing any image-local IMP or deallocator.
            unsafe {
                builder.add_method(
                    sel!(draggingSession:sourceOperationMaskForDraggingContext:),
                    source_mask
                        as extern "C-unwind" fn(
                            *mut AnyObject,
                            Sel,
                            *mut NSDraggingSession,
                            NSDraggingContext,
                        ) -> NSDragOperation,
                );
                builder.add_method(
                    sel!(draggingSession:endedAtPoint:operation:),
                    source_ended
                        as extern "C-unwind" fn(
                            *mut AnyObject,
                            Sel,
                            *mut NSDraggingSession,
                            NSPoint,
                            NSDragOperation,
                        ),
                );
                builder.add_method(
                    sel!(dealloc),
                    source_dealloc as extern "C-unwind" fn(*mut AnyObject, Sel),
                );
            }
            let class = builder.register();
            let state = class
                .instance_variable(c"_matariState")
                .ok_or_else(|| "AppKit source state ivar is unavailable".to_string())?;
            Ok(SourceClass { class, state })
        })
        .as_ref()
        .map_err(|error| ExternalDragError::StartFailed(error.clone()))
}

// The runtime retains registered classes indefinitely, including their IMPs.
// A process-lifetime image pin is therefore intentional, not a per-session
// leak. It also prevents address reuse for this image's class identity.
fn pin_callback_image() -> Result<(), String> {
    unsafe extern "C" {
        fn _dyld_get_image_header(index: u32) -> *const c_void;
    }
    let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
    // SAFETY: A real callback address and a writable native Dl_info.
    if unsafe {
        libc::dladdr(
            source_ended as *const () as *const c_void,
            info.as_mut_ptr(),
        )
    } == 0
    {
        return Err("cannot locate the AppKit drag callback image".into());
    }
    // SAFETY: dladdr initialized all fields on success.
    let info = unsafe { info.assume_init() };
    // SAFETY: dyld's first image is the process's permanently resident executable.
    if info.dli_fbase == unsafe { _dyld_get_image_header(0) }.cast_mut() {
        return Ok(());
    }
    if info.dli_fname.is_null() {
        return Err("AppKit drag callback image has no loader path".into());
    }
    // Darwin dlfcn.h: RTLD_NODELETE = 0x80. NOLOAD prohibits loading another
    // image if this exact loader path is no longer present. Deliberately never
    // dlclose this reference: future runtime dispatch/dealloc must stay valid.
    const RTLD_NODELETE: libc::c_int = 0x80;
    // SAFETY: dladdr's loader-owned path is valid while executing this image.
    let handle = unsafe {
        libc::dlopen(
            info.dli_fname,
            libc::RTLD_NOW | libc::RTLD_LOCAL | libc::RTLD_NOLOAD | RTLD_NODELETE,
        )
    };
    if handle.is_null() {
        return Err("cannot keep the AppKit drag callback image resident".into());
    }
    Ok(())
}

fn source_state(source: &AnyObject) -> Option<&DragSourceState> {
    let class = SOURCE_CLASS.get()?.as_ref().ok()?;
    // SAFETY: Only our registered class dispatches these methods; its ivar is
    // a zero-initialized pointer, set to one owned Box after initialization.
    let state = unsafe { *class.state.load::<*mut c_void>(source) };
    // SAFETY: Non-null state points to the live Box until this source deallocates.
    unsafe { state.cast::<DragSourceState>().as_ref() }
}

extern "C-unwind" fn source_mask(
    _source: *mut AnyObject,
    _selector: Sel,
    _session: *mut NSDraggingSession,
    _context: NSDraggingContext,
) -> NSDragOperation {
    NSDragOperation::Copy
}

extern "C-unwind" fn source_ended(
    source: *mut AnyObject,
    _selector: Sel,
    _session: *mut NSDraggingSession,
    _screen_point: NSPoint,
    operation: NSDragOperation,
) {
    // SAFETY: Objective-C dispatch supplies a live object for this method.
    let Some(source) = (unsafe { source.as_ref() }) else {
        return;
    };
    let Some(state) = source_state(source) else {
        return;
    };
    let reporter = state
        .reporter
        .lock()
        .map(|mut reporter| reporter.take())
        .unwrap_or(None);
    let owns_self = state.owns_self.swap(false, Ordering::AcqRel);
    if let Some(reporter) = reporter {
        // A caller-provided wake closure must never unwind through AppKit.
        // Terminal delivery is queued before wake; release ownership afterward
        // even if that application callback panics.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            reporter.finish(if operation.contains(NSDragOperation::Copy) {
                Outcome::Copied
            } else {
                Outcome::Cancelled
            });
        }));
    }
    if owns_self {
        // SAFETY: Exactly one ownership was transferred after accepted start.
        // No source/state access follows its release; dealloc may run here.
        drop(unsafe { Retained::from_raw(ptr::from_ref(source).cast_mut()) });
    }
}

extern "C-unwind" fn source_dealloc(source: *mut AnyObject, _selector: Sel) {
    // SAFETY: Runtime's dealloc dispatch owns this still-live object.
    let Some(source) = (unsafe { source.as_ref() }) else {
        return;
    };
    if let Some(Ok(class)) = SOURCE_CLASS.get() {
        // SAFETY: Exclusive deallocation of our source; clear before dropping
        // the state to make nested destruction unable to consume it twice.
        let pointer = unsafe { class.state.load_ptr::<*mut c_void>(source) };
        // SAFETY: This is the initialized owned state pointer or allocation's zero.
        let state = unsafe { pointer.replace(ptr::null_mut()) };
        if !state.is_null() {
            // SAFETY: Exactly this object owns the Box written by new_source.
            let state = unsafe { Box::from_raw(state.cast::<DragSourceState>()) };
            // Do not let a caller-owned reporter/wake capture's destructor
            // unwind through the Objective-C deallocation boundary.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(state)));
        }
    }
    // SAFETY: This NSObject subclass has finished releasing its Rust state.
    unsafe {
        let _: () = msg_send![super(source, NSObject::class()), dealloc];
    }
}

fn new_source(
    mtm: MainThreadMarker,
    reporter: Option<SessionReporter>,
) -> Result<Retained<AnyObject>, ExternalDragError> {
    let class = source_class(mtm)?;
    // SAFETY: NSObject allocation/initializer on our registered main-thread class.
    let source: Option<Retained<AnyObject>> = unsafe { msg_send![class.class, new] };
    let source = source.ok_or_else(|| {
        ExternalDragError::StartFailed("cannot allocate AppKit drag source".into())
    })?;
    let state = Box::new(DragSourceState {
        reporter: Mutex::new(reporter),
        owns_self: AtomicBool::new(false),
    });
    // SAFETY: Fresh object, no published references; matching pointer ivar type.
    unsafe {
        *class.state.load_ptr::<*mut c_void>(&source) = Box::into_raw(state).cast();
    }
    Ok(source)
}

pub(super) fn start_external_file_drag(
    window: DragWindow<'_>,
    payload: ExternalDragPayload,
    appkit_event: Option<std::ptr::NonNull<std::ffi::c_void>>,
    reporter: Option<SessionReporter>,
) -> Result<(), ExternalDragError> {
    let ExternalDragPayload { paths, preview } = payload;

    if paths.is_empty() {
        return Err(ExternalDragError::EmptyPayload);
    }
    validate_paths(&paths)?;

    let ns_view = match window.window().as_raw() {
        RawWindowHandle::AppKit(handle) => handle.ns_view.as_ptr().cast(),
        other => {
            return Err(ExternalDragError::UnsupportedBackend {
                backend: window.backend_kind(),
                window: format!("{other:?}"),
            });
        }
    };

    let mtm = MainThreadMarker::new().ok_or_else(|| {
        "macOS external file drag must start on the AppKit main thread".to_string()
    })?;
    let event = appkit_event
        // SAFETY: `DragOrigin::with_appkit_event` requires this to be a live
        // `NSEvent` from the same main-thread gesture; `mtm` proves this call
        // is executing on that main thread.
        .map(|event| unsafe { &*event.cast::<NSEvent>().as_ptr() })
        .ok_or_else(|| {
            "macOS external file drag needs the exact initiating AppKit NSEvent".to_string()
        })?;

    // SAFETY: `RawWindowHandle::AppKit` guarantees `ns_view` identifies the
    // live `NSView` borrowed through `window`; `mtm` proves main-thread access.
    let view = unsafe { &*ns_view };
    start_drag_from_view(view, event, &paths, preview.as_ref(), reporter, mtm)
}

fn start_drag_from_view(
    view: &NSView,
    event: &NSEvent,
    paths: &[PathBuf],
    preview: Option<&DragPreview>,
    reporter: Option<SessionReporter>,
    mtm: MainThreadMarker,
) -> Result<(), ExternalDragError> {
    let location = view.convertPoint_fromView(event.locationInWindow(), None);
    let image = preview.and_then(|preview| match ns_image_from_preview(preview, view) {
        Ok(image) => Some(image),
        Err(stage) => {
            if let Some(reporter) = &reporter {
                reporter.preview(PreviewStatus::Unavailable {
                    stage,
                    native_code: None,
                });
            }
            None
        }
    });
    let items = dragging_items(paths, location, image.as_ref(), preview, view.isFlipped())?;
    let item_refs = items.iter().map(|item| &**item).collect::<Vec<_>>();
    let item_array = NSArray::from_slice(&item_refs);
    let source = new_source(mtm, reporter.clone())?;
    // SAFETY: source_class registers NSDraggingSource and its required methods.
    let source_ref = unsafe {
        &*ptr::from_ref::<AnyObject>(&source).cast::<ProtocolObject<dyn NSDraggingSource>>()
    };

    // SAFETY: This is the binding's exact selector and argument types, but
    // AppKit documents a nil result when it cannot begin a dragging session.
    let session: Option<Retained<NSDraggingSession>> = unsafe {
        msg_send![view, beginDraggingSessionWithItems: &*item_array, event: event, source: source_ref]
    };
    let _session = session.ok_or_else(|| {
        ExternalDragError::StartFailed("AppKit could not begin the dragging session".to_string())
    })?;
    source_state(&source)
        .expect("initialized source state")
        .owns_self
        .store(true, Ordering::Release);
    let _self_owned_until_terminal = Retained::into_raw(source);
    if image.is_some()
        && let Some(reporter) = &reporter
    {
        reporter.preview(PreviewStatus::Attached);
    }
    Ok(())
}

#[allow(deprecated)] // Generic icon fallback for non-Unicode paths on older supported macOS.
fn dragging_items(
    paths: &[PathBuf],
    location: NSPoint,
    image: Option<&Retained<NSImage>>,
    preview: Option<&DragPreview>,
    flipped: bool,
) -> Result<Vec<Retained<NSDraggingItem>>, ExternalDragError> {
    paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            // Preserve the validated filename and symlink identity; do not resolve
            // it or replace non-UTF-8 filesystem bytes with another pathname.
            let bytes = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
                ExternalDragError::StartFailed("drag path contains a NUL byte".into())
            })?;
            // SAFETY: Exact Foundation initializer/ownership with nullable
            // result; bytes is the live NUL-terminated filesystem pathname.
            let file_url: Option<Retained<NSURL>> = unsafe {
                msg_send![NSURL::alloc(),
                    initFileURLWithFileSystemRepresentation: ptr::NonNull::new(bytes.as_ptr().cast_mut()).expect("CString pointer"),
                    isDirectory: false,
                    relativeToURL: Option::<&NSURL>::None
                ]
            };
            let file_url = file_url.ok_or_else(|| {
                ExternalDragError::StartFailed(format!("AppKit cannot represent drag path: {}", path.display()))
            })?;
            let writer: &ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting> =
                ProtocolObject::from_ref(&*file_url);
            let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);
            let fallback;
            let (contents, size, hotspot) = if let (Some(image), Some(preview)) = (image, preview) {
                (&**image, preview.logical_size(), preview.hotspot())
            } else {
                let workspace = NSWorkspace::sharedWorkspace();
                let native = if let Some(path) = file_url.path() {
                    workspace.iconForFile(&path)
                } else {
                    workspace.iconForFileType(&NSString::from_str(""))
                };
                // SAFETY: NSImage implements NSCopying; keep an independent size.
                fallback = unsafe { msg_send![&*native, copy] };
                let fallback: &Retained<NSImage> = &fallback;
                fallback.setSize(NSSize::new(32.0, 32.0));
                (&**fallback, [32.0, 32.0], [16.0, 16.0])
            };
            let frame = drag_frame(location, size, hotspot, flipped, index);
            // SAFETY: Item and image are real initialized AppKit objects. The item
            // retains/copies its contents while these local owners remain live.
            unsafe {
                item.setDraggingFrame_contents(frame, Some(contents));
            }
            Ok(item)
        })
        .collect()
}

fn drag_frame(
    location: NSPoint,
    size: [f64; 2],
    hotspot: [f64; 2],
    flipped: bool,
    index: usize,
) -> NSRect {
    let cascade = index as f64 * 4.0;
    let origin = NSPoint::new(
        location.x - hotspot[0] + cascade,
        if flipped {
            location.y - hotspot[1] + cascade
        } else {
            location.y + hotspot[1] - size[1] - cascade
        },
    );
    NSRect::new(origin, NSSize::new(size[0], size[1]))
}

fn ns_image_from_preview(
    preview: &DragPreview,
    view: &NSView,
) -> Result<Retained<NSImage>, PreviewFailureStage> {
    preview
        .validate()
        .map_err(|_| PreviewFailureStage::Bitmap)?;
    let scale = view
        .window()
        .map_or(1.0, |window| window.backingScaleFactor()) as f32;
    let mut canvas = if matches!(preview, DragPreview::Bitmap(_)) {
        crate::preview::render_original(preview)
    } else {
        crate::preview::render_scaled(preview, scale)
    }
    .map_err(|_| PreviewFailureStage::Bitmap)?;
    // The common canvas is premultiplied BGRA; AppKit's bitmap is RGBA.
    for pixel in canvas.pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let [width, height] = [canvas.width, canvas.height];
    let [point_width, point_height] = preview.logical_size();
    let size = NSSize::new(point_width, point_height);
    let image = NSImage::initWithSize(NSImage::alloc(), size);
    // SAFETY: Exact raster dimensions and checked 8-bit premultiplied layout.
    let Some(bitmap) = (unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(), ptr::null_mut(), width as isize, height as isize,
            8, 4, true, false, NSDeviceRGBColorSpace, (width * 4) as isize, 32,
        )
    }) else {
        return Err(PreviewFailureStage::Bitmap);
    };
    let destination = bitmap.bitmapData();
    if destination.is_null() {
        return Err(PreviewFailureStage::Bitmap);
    }
    // SAFETY: AppKit allocated width * height * 4 bytes; Canvas validates length.
    unsafe {
        ptr::copy_nonoverlapping(canvas.pixels.as_ptr(), destination, canvas.pixels.len());
    }
    // Tag backing pixels separately from the logical point extent.
    bitmap.setSize(size);
    let representation: &NSImageRep = bitmap.as_ref();
    image.addRepresentation(representation);
    Ok(image)
}

fn validate_paths(paths: &[PathBuf]) -> Result<(), String> {
    for path in paths {
        let metadata = std::fs::metadata(path)
            .map_err(|err| format!("cannot inspect drag file: {}: {err}", path.display()))?;
        if !metadata.is_file() {
            return Err(format!("drag path is not a file: {}", path.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotspot_preserves_top_left_geometry_in_both_view_orientations() {
        let location = NSPoint::new(120.0, 70.0);
        let flipped = drag_frame(location, [80.0, 30.0], [7.0, 11.0], true, 0);
        let unflipped = drag_frame(location, [80.0, 30.0], [7.0, 11.0], false, 0);
        assert_eq!(flipped.origin, NSPoint::new(113.0, 59.0));
        assert_eq!(unflipped.origin, NSPoint::new(113.0, 51.0));
        assert_eq!(flipped.size, unflipped.size);
        // The hotspot's top-left y is native frame origin + height in an
        // unflipped view and native origin in a flipped view.
        assert_eq!(flipped.origin.y + 11.0, location.y);
        assert_eq!(unflipped.origin.y + 30.0 - 11.0, location.y);
    }
}
