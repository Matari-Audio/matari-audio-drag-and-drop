use std::mem::{ManuallyDrop, size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::Mutex;

use raw_window_handle::RawWindowHandle;
use windows::Win32::Foundation::{
    COLORREF, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC,
    E_NOTIMPL, GlobalFree, HANDLE, HGLOBAL, HWND, OLE_E_ADVISENOTSUPPORTED, POINT,
    RPC_E_CHANGED_MODE, SIZE,
};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC,
    HGDIOBJ, SelectObject,
};
use windows::Win32::System::Com::{
    DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl,
    IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GHND, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    CF_HDROP, CLIPBOARD_FORMAT, DROPEFFECT, DROPEFFECT_COPY, DoDragDrop, IDropSource,
    IDropSource_Impl, OleDuplicateData, OleInitialize, OleUninitialize,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE, GetDpiForWindow,
    SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Shell::{
    CFSTR_FILENAMEW, CFSTR_PREFERREDDROPEFFECT, DROPFILES, SHCreateStdEnumFmtEtc,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GetPhysicalCursorPos, HWND_TOPMOST, IsWindow, SWP_NOACTIVATE,
    SWP_NOSIZE, SWP_SHOWWINDOW, SetWindowPos, ULW_ALPHA, UPDATE_LAYERED_WINDOW_FLAGS,
    USER_DEFAULT_SCREEN_DPI, UpdateLayeredWindow, WS_DISABLED, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::implement;
use windows_core::{BOOL, HRESULT, IUnknown, PCWSTR, Ref, Result};

use super::ExternalDragPayload;
use super::{DragWindow, ExternalDragError};
use crate::{
    DragPreview, FailureKind, FailureStage, Outcome, PreviewFailureStage, PreviewStatus,
    SessionFailure, SessionReporter,
};

pub(super) fn start_external_file_drag(
    window: DragWindow<'_>,
    payload: ExternalDragPayload,
    reporter: Option<SessionReporter>,
) -> std::result::Result<(), ExternalDragError> {
    let ExternalDragPayload { paths, preview } = payload;

    if paths.is_empty() {
        return Err(ExternalDragError::EmptyPayload);
    }
    validate_paths(&paths)?;

    let hwnd = match window.window().as_raw() {
        RawWindowHandle::Win32(handle) => HWND(handle.hwnd.get() as *mut _),
        other => {
            return Err(ExternalDragError::UnsupportedBackend {
                backend: window.backend_kind(),
                window: format!("{other:?}"),
            });
        }
    };

    let _ole = OleDragApartment::initialize()?;
    let data_object: IDataObject = FileDataObject::new(paths)?.into();
    let drag_image =
        preview
            .as_ref()
            .and_then(|preview| match SourceDragImage::new(preview, hwnd) {
                Ok(image) => {
                    if let Some(reporter) = &reporter {
                        reporter.preview(PreviewStatus::Attached);
                    }
                    Some(image)
                }
                Err(error) => {
                    if let Some(reporter) = &reporter {
                        reporter.preview(PreviewStatus::Unavailable {
                            stage: error.stage,
                            native_code: error.native_code,
                        });
                    }
                    None
                }
            });
    let drop_source: IDropSource = FileDropSource {
        drag_image,
        owner: hwnd,
    }
    .into();
    let mut effect = DROPEFFECT(0);
    // SAFETY: OLE is initialized on this GUI thread, both COM interfaces remain
    // alive for the call, and `effect` is a writable `DROPEFFECT`.
    unsafe {
        let result = DoDragDrop(
            &data_object,
            &drop_source,
            DROPEFFECT_COPY,
            &mut effect as *mut DROPEFFECT,
        );
        result.ok().map_err(|err| {
            ExternalDragError::StartFailed(format!(
                "Windows OLE DoDragDrop failed for {hwnd:?}: {err}"
            ))
        })?;
        // OLE has returned and released its borrowed interfaces. Destroy our
        // native preview before any terminal wake can reveal the source ghost.
        drop(drop_source);
        drop(data_object);
        if let Some(reporter) = &reporter {
            let outcome = if result == DRAGDROP_S_DROP && effect.0 & DROPEFFECT_COPY.0 != 0 {
                Outcome::Copied
            } else if result == DRAGDROP_S_CANCEL {
                Outcome::Cancelled
            } else {
                Outcome::Failed(SessionFailure {
                    stage: FailureStage::Transfer,
                    kind: FailureKind::NativeRejected,
                    native_code: Some(result.0),
                    native_effect: Some(effect.0),
                })
            };
            reporter.finish(outcome);
        }
    }
    Ok(())
}

struct DragBitmapGuard(HBITMAP);

impl Drop for DragBitmapGuard {
    fn drop(&mut self) {
        // SAFETY: This guard uniquely owns the bitmap created by
        // `create_drag_bitmap`, and OLE has finished using it before drop.
        unsafe {
            let _ = DeleteObject(self.0.into());
        }
    }
}

/// One independently per-monitor-aware layered window. The system STATIC
/// class requires no callback into plugin code. Position APIs always run in a
/// scoped physical desktop context, regardless of the host/editor awareness.
struct SourceDragImage {
    hwnd: HWND,
    preview: DragPreview,
    state: Mutex<DragImageState>,
}

struct DragImageState {
    bitmap: Option<DragBitmapGuard>,
    dpi: u32,
    hotspot: [i32; 2],
}

impl SourceDragImage {
    fn new(preview: &DragPreview, _owner: HWND) -> std::result::Result<Self, PreviewAttachError> {
        preview
            .validate()
            .map_err(|_| PreviewAttachError::new(PreviewFailureStage::Bitmap, None))?;
        let _dpi = ThreadDpiContext::physical()?;
        let cursor = physical_cursor()?;
        // A hidden one-pixel top-level on the cursor monitor lets Windows
        // supply actual effective DPI even in an unaware/system-aware host.
        // SAFETY: System STATIC class, no image-local WNDPROC, and scoped
        // per-monitor awareness; the owned window is destroyed on all failures.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                windows_core::w!("STATIC"),
                PCWSTR::null(),
                WS_POPUP | WS_DISABLED,
                cursor.x,
                cursor.y,
                1,
                1,
                None,
                None,
                None,
                None,
            )
        }
        .map_err(|error| {
            PreviewAttachError::new(PreviewFailureStage::Helper, Some(error.code().0))
        })?;
        let image = Self {
            hwnd,
            preview: preview.clone(),
            state: Mutex::new(DragImageState {
                bitmap: None,
                dpi: 0,
                hotspot: [0, 0],
            }),
        };
        // SAFETY: Query our own live, per-monitor-aware window.
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi == 0 {
            return Err(PreviewAttachError::new(PreviewFailureStage::Helper, None));
        }
        {
            let mut state = image
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            image.rasterize(dpi, cursor, &mut state)?;
        }
        // Upload/position happened while hidden. This is the sole show;
        // ordinary OLE movement never changes visibility or activation.
        // SAFETY: Prepared live layered window, physical desktop coordinates.
        unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOSIZE
                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                    | SWP_NOACTIVATE
                    | SWP_SHOWWINDOW,
            )
        }
        .map_err(|error| {
            PreviewAttachError::new(PreviewFailureStage::Attach, Some(error.code().0))
        })?;
        Ok(image)
    }

    fn rasterize(
        &self,
        dpi: u32,
        cursor: POINT,
        state: &mut DragImageState,
    ) -> std::result::Result<(), PreviewAttachError> {
        let canvas = crate::preview::render_scaled(
            &self.preview,
            dpi as f32 / USER_DEFAULT_SCREEN_DPI as f32,
        )
        .map_err(|_| PreviewAttachError::new(PreviewFailureStage::Bitmap, None))?;
        let bitmap = DragBitmapGuard(
            create_drag_bitmap(&canvas.pixels, canvas.width, canvas.height)
                .map_err(|_| PreviewAttachError::new(PreviewFailureStage::Bitmap, None))?,
        );
        let dc = BitmapDc::new(bitmap.0)?;
        let size = SIZE {
            cx: i32::try_from(canvas.width)
                .map_err(|_| PreviewAttachError::new(PreviewFailureStage::Bitmap, None))?,
            cy: i32::try_from(canvas.height)
                .map_err(|_| PreviewAttachError::new(PreviewFailureStage::Bitmap, None))?,
        };
        let position = image_position(cursor, canvas.hotspot);
        let origin = POINT::default();
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // Atomic content/size/position update: retain the previous bitmap and
        // geometry if allocating or uploading the replacement fails.
        // SAFETY: Live owned window and DIB, valid selected memory DC; all
        // coordinates/dimensions use the scoped physical desktop context.
        unsafe {
            UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&position),
                Some(&size),
                Some(dc.dc),
                Some(&origin),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
        }
        .map_err(|error| {
            PreviewAttachError::new(PreviewFailureStage::Attach, Some(error.code().0))
        })?;
        // Deselect before replacing/dropping either bitmap.
        drop(dc);
        state.bitmap = Some(bitmap);
        state.dpi = dpi;
        state.hotspot = canvas.hotspot;
        Ok(())
    }

    fn move_to_cursor(&self) {
        let Ok(_dpi) = ThreadDpiContext::physical() else {
            return;
        };
        let Ok(cursor) = physical_cursor() else {
            return;
        };
        // A native API can cause reentrant callbacks; the last good image is
        // already visible, so skip that nested update rather than deadlock.
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        let position = image_position(cursor, state.hotspot);
        // SAFETY: Position-only layered update retains the uploaded content,
        // dimensions and visibility; source DC/size/blend are all NULL.
        if unsafe {
            UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&position),
                None,
                None,
                None,
                COLORREF(0),
                None,
                UPDATE_LAYERED_WINDOW_FLAGS(0),
            )
        }
        .is_err()
        {
            return;
        }
        // GetDpiForWindow follows the preview's actual monitor, not the source
        // HWND's frozen/virtualized DPI. Cosmetic failure leaves old content
        // and anchor intact, and the next callback retries the transition.
        // SAFETY: Our per-monitor-aware window remains live for the drag.
        let dpi = unsafe { GetDpiForWindow(self.hwnd) };
        if dpi != 0 && dpi != state.dpi {
            let _ = self.rasterize(dpi, cursor, &mut state);
        }
    }
}

impl Drop for SourceDragImage {
    fn drop(&mut self) {
        // SAFETY: Unique ownership; system WNDPROC has no plugin lifetime.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

fn physical_cursor() -> std::result::Result<POINT, PreviewAttachError> {
    let mut cursor = POINT::default();
    // SAFETY: Writable POINT, no input injection or pointer modification.
    unsafe { GetPhysicalCursorPos(&mut cursor) }.map_err(|error| {
        PreviewAttachError::new(PreviewFailureStage::Helper, Some(error.code().0))
    })?;
    Ok(cursor)
}

fn image_position(cursor: POINT, hotspot: [i32; 2]) -> POINT {
    POINT {
        x: cursor.x.saturating_sub(hotspot[0]),
        y: cursor.y.saturating_sub(hotspot[1]),
    }
}

struct BitmapDc {
    dc: HDC,
    previous: HGDIOBJ,
}

impl BitmapDc {
    fn new(bitmap: HBITMAP) -> std::result::Result<Self, PreviewAttachError> {
        // SAFETY: Null creates a screen-compatible memory DC owned here.
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.0.is_null() {
            return Err(PreviewAttachError::last(PreviewFailureStage::Helper));
        }
        // SAFETY: Live compatible DC and DIB, deselected in Drop.
        let previous = unsafe { SelectObject(dc, bitmap.into()) };
        if previous.0.is_null() || previous.0 as isize == -1 {
            let error = PreviewAttachError::last(PreviewFailureStage::Helper);
            // SAFETY: Our DC; no valid bitmap selection to undo.
            unsafe {
                let _ = DeleteDC(dc);
            }
            return Err(error);
        }
        Ok(Self { dc, previous })
    }
}

impl Drop for BitmapDc {
    fn drop(&mut self) {
        // SAFETY: Restore the live previous object before deleting our DC.
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteDC(self.dc);
        }
    }
}

/// Scoped thread awareness; never changes the process/host default.
struct ThreadDpiContext(DPI_AWARENESS_CONTEXT);

impl ThreadDpiContext {
    fn physical() -> std::result::Result<Self, PreviewAttachError> {
        let context = Self::enter(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
        if context.0.0.is_null() {
            Err(PreviewAttachError::last(PreviewFailureStage::Helper))
        } else {
            Ok(context)
        }
    }

    fn enter(context: DPI_AWARENESS_CONTEXT) -> Self {
        // SAFETY: Thread-local state change, restored in `Drop`.
        Self(unsafe { SetThreadDpiAwarenessContext(context) })
    }
}

impl Drop for ThreadDpiContext {
    fn drop(&mut self) {
        // `-1` is the valid UNAWARE context; only NULL means the switch failed.
        if !self.0.0.is_null() {
            // SAFETY: Restores the context saved in `enter` on the same thread.
            unsafe {
                SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
}

#[cfg(test)]
mod dpi_tests {
    use super::*;
    use windows::Win32::UI::HiDpi::{
        AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
        DPI_AWARENESS_CONTEXT_UNAWARE, GetThreadDpiAwarenessContext,
    };

    #[test]
    fn dpi_guard_restores_unaware_pseudohandle() {
        let original = ThreadDpiContext::enter(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
        assert!(!original.0.0.is_null());

        drop(ThreadDpiContext(DPI_AWARENESS_CONTEXT_UNAWARE));

        // SAFETY: Both values are valid contexts; the guard changes only this thread.
        assert!(unsafe {
            AreDpiAwarenessContextsEqual(
                GetThreadDpiAwarenessContext(),
                DPI_AWARENESS_CONTEXT_UNAWARE,
            )
            .as_bool()
        });
    }

    #[test]
    fn physical_preview_preserves_unaware_host_and_last_good_raster() {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsWindowVisible};

        let original = ThreadDpiContext::enter(DPI_AWARENESS_CONTEXT_UNAWARE);
        assert!(!original.0.0.is_null());
        let preview = DragPreview::Bitmap(
            crate::DragImage::new(4, 3, vec![255; 4 * 3 * 4])
                .unwrap()
                .with_geometry([40.0, 30.0], [7.0, 11.0])
                .unwrap(),
        );
        let image = SourceDragImage::new(&preview, HWND::default()).expect("native preview attach");
        assert!(
            // SAFETY: Valid context/window; no cursor movement or input injection.
            unsafe {
                AreDpiAwarenessContextsEqual(
                    GetThreadDpiAwarenessContext(),
                    DPI_AWARENESS_CONTEXT_UNAWARE,
                )
            }
            .as_bool()
        );
        // SAFETY: Live owned preview HWND.
        assert!(unsafe { IsWindowVisible(image.hwnd) }.as_bool());
        {
            let _physical = ThreadDpiContext::physical().unwrap();
            let mut state = image.state.lock().unwrap();
            // Exercise a real native reupload to a negative physical desktop
            // point at 200% scale. This doesn't claim a physical two-monitor test.
            image
                .rasterize(192, POINT { x: -120, y: -80 }, &mut state)
                .unwrap();
            let mut rect = RECT::default();
            // SAFETY: Live HWND and writable native RECT in physical context.
            unsafe { GetWindowRect(image.hwnd, &mut rect) }.unwrap();
            assert_eq!(
                (
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top
                ),
                (-134, -102, 80, 60)
            );
            assert_eq!(state.hotspot, [14, 22]);
            let bitmap = state.bitmap.as_ref().unwrap().0;
            // Fail the replacement upload after constructing its raster/DC.
            // SAFETY: We own the HWND; no drag/modal callbacks exist in this test.
            unsafe { DestroyWindow(image.hwnd) }.unwrap();
            assert!(image.rasterize(144, POINT::default(), &mut state).is_err());
            assert_eq!(state.dpi, 192);
            assert_eq!(state.bitmap.as_ref().unwrap().0, bitmap);
            assert_eq!(state.hotspot, [14, 22]);
        }
        assert!(
            // SAFETY: Scoped preview API calls must restore the original unaware host.
            unsafe {
                AreDpiAwarenessContextsEqual(
                    GetThreadDpiAwarenessContext(),
                    DPI_AWARENESS_CONTEXT_UNAWARE,
                )
            }
            .as_bool()
        );
    }

    #[test]
    fn lost_source_window_cancels_the_native_loop() {
        let source: IDropSource = FileDropSource {
            owner: HWND::default(),
            drag_image: None,
        }
        .into();
        assert_eq!(
            // SAFETY: Live source COM interface, no input injection or target use.
            unsafe { source.QueryContinueDrag(false, MK_LBUTTON) },
            DRAGDROP_S_CANCEL
        );
    }

    #[test]
    fn failed_dpi_switch_preserves_current_context() {
        let original = ThreadDpiContext::enter(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
        assert!(!original.0.0.is_null());

        let failed = ThreadDpiContext::enter(DPI_AWARENESS_CONTEXT::default());
        assert!(failed.0.0.is_null());
        drop(failed);

        // SAFETY: The current context and predefined comparison context are valid.
        assert!(unsafe {
            AreDpiAwarenessContextsEqual(
                GetThreadDpiAwarenessContext(),
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
            )
            .as_bool()
        });
    }
}

#[derive(Debug)]
struct PreviewAttachError {
    stage: PreviewFailureStage,
    native_code: Option<i32>,
}

impl PreviewAttachError {
    fn last(stage: PreviewFailureStage) -> Self {
        // SAFETY: Read immediately after the failed Win32 operation.
        Self::new(
            stage,
            Some(unsafe { windows::Win32::Foundation::GetLastError() }.0 as i32),
        )
    }

    fn new(stage: PreviewFailureStage, native_code: Option<i32>) -> Self {
        Self { stage, native_code }
    }
}

fn create_drag_bitmap(
    rgba: &[u8],
    width: usize,
    height: usize,
) -> std::result::Result<HBITMAP, String> {
    let bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4));
    if width == 0
        || height == 0
        || width > i32::MAX as usize
        || height > i32::MAX as usize
        || bytes != Some(rgba.len())
    {
        return Err("invalid drag bitmap dimensions or pixel length".into());
    }
    // SAFETY: Zero is a valid initial state for `BITMAPINFO`; the required
    // header fields are initialized below before the GDI call.
    let mut info: BITMAPINFO = unsafe { zeroed() };
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        biHeight: height as i32,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    let mut bits = ptr::null_mut();
    // SAFETY: `info` is initialized for a 32-bit DIB and `bits` is a valid out
    // pointer. The returned bitmap is owned by `DragBitmapGuard`.
    let bitmap = unsafe {
        CreateDIBSection(
            Some(HDC::default()),
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        )
        .map_err(|error| format!("CreateDIBSection failed: {error}"))?
    };
    if bits.is_null() {
        // SAFETY: GDI returned the live bitmap above, but no writable storage.
        let _ = unsafe { DeleteObject(bitmap.into()) };
        return Err("CreateDIBSection returned null bits".into());
    }

    let stride = width * 4;
    // SAFETY: A 32-bit DIB exposes exactly `stride * height` writable bytes.
    let destination = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), stride * height) };
    for y in 0..height {
        let source_row = &rgba[y * stride..(y + 1) * stride];
        let destination_row = &mut destination[(height - 1 - y) * stride..(height - y) * stride];
        destination_row.copy_from_slice(source_row);
    }
    Ok(bitmap)
}

struct OleDragApartment;

impl OleDragApartment {
    fn initialize() -> std::result::Result<Self, String> {
        // SAFETY: This initializes OLE for the current GUI thread. Successful
        // initialization is paired with `OleUninitialize` in `Drop`.
        match unsafe { OleInitialize(None) } {
            Ok(()) => Ok(Self),
            Err(err) if err.code() == RPC_E_CHANGED_MODE => {
                Err("Windows OLE drag unavailable: plugin UI thread is already initialized as a multithreaded COM apartment".to_string())
            }
            Err(err) => Err(format!("Windows OLE initialize failed: {err}")),
        }
    }
}

impl Drop for OleDragApartment {
    fn drop(&mut self) {
        // SAFETY: `OleDragApartment` is created only after `OleInitialize`
        // succeeds and remains on the synchronous drag's GUI thread.
        unsafe { OleUninitialize() };
    }
}

fn validate_paths(paths: &[PathBuf]) -> std::result::Result<(), String> {
    for path in paths {
        let metadata = std::fs::metadata(path)
            .map_err(|err| format!("drag file is not readable: {}: {err}", path.display()))?;
        if !metadata.is_file() {
            return Err(format!("drag path is not a file: {}", path.display()));
        }
        if metadata.len() == 0 {
            return Err(format!("drag file is empty: {}", path.display()));
        }
    }
    Ok(())
}

#[implement(IDataObject)]
struct FileDataObject {
    paths: Vec<PathBuf>,
    formats: ShellDragFormats,
    stored: Mutex<Vec<StoredMedium>>,
}

impl FileDataObject {
    fn new(paths: Vec<PathBuf>) -> std::result::Result<Self, String> {
        Ok(Self {
            paths,
            formats: ShellDragFormats::new()?,
            stored: Mutex::new(Vec::new()),
        })
    }

    fn format(clipboard_format: u16) -> FORMATETC {
        FORMATETC {
            cfFormat: clipboard_format,
            ptd: ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        }
    }

    /// Match a format supplied by an `IDataObject` COM call.
    ///
    /// # Safety
    ///
    /// `pformatetc` must be null or point to a readable `FORMATETC` for the
    /// duration of this call.
    unsafe fn requested_format(&self, pformatetc: *const FORMATETC) -> Option<ShellDragFormat> {
        // SAFETY: The caller upholds the pointer contract documented above;
        // `as_ref` additionally handles a null pointer.
        let format = unsafe { pformatetc.as_ref() }?;

        if format.dwAspect != DVASPECT_CONTENT.0
            || format.lindex != -1
            || (format.tymed & TYMED_HGLOBAL.0 as u32) == 0
        {
            return None;
        }

        self.formats
            .formats()
            .into_iter()
            .find(|candidate| candidate.clipboard_format() == format.cfFormat)
    }

    fn hdrop_medium(&self) -> Result<STGMEDIUM> {
        Ok(build_hdrop(&self.paths)?.into_medium())
    }

    fn preferred_drop_effect_medium(&self) -> Result<STGMEDIUM> {
        Ok(build_u32_hglobal(DROPEFFECT_COPY.0)?.into_medium())
    }

    fn filenamew_medium(&self) -> Result<STGMEDIUM> {
        let Some(path) = self.paths.first() else {
            return Err(DV_E_FORMATETC.into());
        };
        let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        Ok(build_wide_string_hglobal(&wide)?.into_medium())
    }

    fn stored_medium(&self, format: &FORMATETC) -> Result<STGMEDIUM> {
        let stored = self
            .stored
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stored
            .iter()
            .find(|stored| stored.matches(format))
            .map(StoredMedium::duplicate_medium)
            .transpose()?
            .ok_or_else(|| DV_E_FORMATETC.into())
    }
}

#[allow(non_snake_case)]
impl IDataObject_Impl for FileDataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> Result<STGMEDIUM> {
        // SAFETY: COM requires `pformatetcin` to be null or a readable
        // `FORMATETC` for the duration of `GetData`.
        match unsafe { self.requested_format(pformatetcin) } {
            Some(ShellDragFormat::Hdrop) => self.hdrop_medium(),
            Some(ShellDragFormat::PreferredDropEffect(_)) => self.preferred_drop_effect_medium(),
            Some(ShellDragFormat::FileNameW(_)) => self.filenamew_medium(),
            // SAFETY: Same COM contract as above; `as_ref` handles null.
            None => unsafe { pformatetcin.as_ref() }
                .ok_or_else(|| windows_core::Error::from(DV_E_FORMATETC))
                .and_then(|format| self.stored_medium(format)),
        }
    }

    fn GetDataHere(&self, _pformatetc: *const FORMATETC, _pmedium: *mut STGMEDIUM) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        // SAFETY: COM requires `pformatetc` to be null or a readable
        // `FORMATETC` for the duration of `QueryGetData`.
        let stored = unsafe { pformatetc.as_ref() }.is_some_and(|format| {
            self.stored
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .any(|stored| stored.matches(format))
        });
        // SAFETY: Same COM contract as the block above.
        if unsafe { self.requested_format(pformatetc) }.is_some() || stored {
            HRESULT(0)
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _pformatectin: *const FORMATETC,
        _pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        E_NOTIMPL
    }

    fn SetData(
        &self,
        pformatetc: *const FORMATETC,
        pmedium: *const STGMEDIUM,
        frelease: BOOL,
    ) -> Result<()> {
        // SAFETY: COM requires both pointers to remain readable for this call.
        let (Some(format), Some(medium)) =
            (unsafe { pformatetc.as_ref() }, unsafe { pmedium.as_ref() })
        else {
            return Err(DV_E_FORMATETC.into());
        };
        let stored = if frelease.as_bool() {
            // SAFETY: `frelease` transfers ownership of the entire medium,
            // including any custom `pUnkForRelease`, to this data object.
            StoredMedium::take(format, unsafe { ptr::read(pmedium) })
        } else {
            StoredMedium::duplicate(format, medium)?
        };
        let mut formats = self
            .stored
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = formats.iter_mut().find(|item| item.same_format(format)) {
            *existing = stored;
        } else {
            formats.push(stored);
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> Result<IEnumFORMATETC> {
        if dwdirection == DATADIR_GET.0 as u32 {
            let formats = self
                .formats
                .formats()
                .into_iter()
                .map(|format| FileDataObject::format(format.clipboard_format()))
                .chain(
                    self.stored
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .iter()
                        .map(StoredMedium::format),
                )
                .collect::<Vec<_>>();
            // SAFETY: `formats` is initialized and valid for the call;
            // `SHCreateStdEnumFmtEtc` copies the entries into the enumerator.
            unsafe { SHCreateStdEnumFmtEtc(&formats) }
        } else {
            Err(E_NOTIMPL.into())
        }
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Ref<'_, IAdviseSink>,
    ) -> Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _dwconnection: u32) -> Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

struct StoredMedium {
    format: FORMATETC,
    medium: STGMEDIUM,
}

impl StoredMedium {
    fn take(format: &FORMATETC, medium: STGMEDIUM) -> Self {
        Self {
            format: FORMATETC {
                ptd: ptr::null_mut(),
                ..*format
            },
            medium,
        }
    }

    fn duplicate(format: &FORMATETC, source: &STGMEDIUM) -> Result<Self> {
        if source.tymed != TYMED_HGLOBAL.0 as u32 {
            return Err(DV_E_FORMATETC.into());
        }
        // SAFETY: `source.tymed` is TYMED_HGLOBAL, so this union field is live.
        let source_handle = unsafe { source.u.hGlobal };
        // SAFETY: OLE duplicates the live HGLOBAL into an independently owned
        // allocation suitable for the same clipboard format.
        let duplicate = unsafe {
            OleDuplicateData(
                HANDLE(source_handle.0),
                CLIPBOARD_FORMAT(format.cfFormat),
                GHND,
            )
        };
        if duplicate.0.is_null() {
            return Err(windows_core::Error::from_thread());
        }
        Ok(Self::take(
            format,
            STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 {
                    hGlobal: HGLOBAL(duplicate.0),
                },
                pUnkForRelease: ManuallyDrop::new(None::<IUnknown>),
            },
        ))
    }

    fn same_format(&self, format: &FORMATETC) -> bool {
        self.format.cfFormat == format.cfFormat
            && self.format.dwAspect == format.dwAspect
            && self.format.lindex == format.lindex
    }

    fn matches(&self, format: &FORMATETC) -> bool {
        (format.tymed & self.medium.tymed) != 0 && self.same_format(format)
    }

    fn format(&self) -> FORMATETC {
        FORMATETC {
            ptd: ptr::null_mut(),
            tymed: self.medium.tymed,
            ..self.format
        }
    }

    fn duplicate_medium(&self) -> Result<STGMEDIUM> {
        Self::duplicate(&self.format, &self.medium).map(|copy| {
            // SAFETY: `copy` is forgotten right after the bitwise move, so the
            // medium is released exactly once, by the caller.
            let medium = unsafe { ptr::read(&copy.medium) };
            std::mem::forget(copy);
            medium
        })
    }
}

impl Drop for StoredMedium {
    fn drop(&mut self) {
        // SAFETY: This value owns the complete medium and its release policy.
        unsafe { windows::Win32::System::Ole::ReleaseStgMedium(&mut self.medium) };
    }
}

#[derive(Clone, Copy)]
struct ShellDragFormats {
    preferred_drop_effect: u16,
    filenamew: u16,
}

#[derive(Clone, Copy)]
enum ShellDragFormat {
    Hdrop,
    PreferredDropEffect(u16),
    FileNameW(u16),
}

impl ShellDragFormats {
    fn new() -> std::result::Result<Self, String> {
        let preferred_drop_effect = registered_clipboard_format(CFSTR_PREFERREDDROPEFFECT)
            .ok_or_else(|| {
                "Windows could not register Preferred DropEffect clipboard format".to_string()
            })?;
        let filenamew = registered_clipboard_format(CFSTR_FILENAMEW)
            .ok_or_else(|| "Windows could not register FileNameW clipboard format".to_string())?;
        Ok(Self {
            preferred_drop_effect,
            filenamew,
        })
    }

    fn formats(self) -> Vec<ShellDragFormat> {
        vec![
            ShellDragFormat::Hdrop,
            ShellDragFormat::PreferredDropEffect(self.preferred_drop_effect),
            ShellDragFormat::FileNameW(self.filenamew),
        ]
    }
}

impl ShellDragFormat {
    fn clipboard_format(self) -> u16 {
        match self {
            ShellDragFormat::Hdrop => CF_HDROP.0,
            ShellDragFormat::PreferredDropEffect(format) | ShellDragFormat::FileNameW(format) => {
                format
            }
        }
    }
}

fn registered_clipboard_format(name: PCWSTR) -> Option<u16> {
    // SAFETY: The caller supplies one of the static, nul-terminated
    // `CFSTR_*` strings exported by the Windows bindings.
    let value = unsafe { RegisterClipboardFormatW(name) };
    u16::try_from(value).ok().filter(|value| *value != 0)
}

#[implement(IDropSource)]
struct FileDropSource {
    owner: HWND,
    drag_image: Option<SourceDragImage>,
}

#[allow(non_snake_case)]
impl IDropSource_Impl for FileDropSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if let Some(image) = &self.drag_image {
            image.move_to_cursor();
        }
        // SAFETY: IsWindow accepts stale HWND values; never dereferences plugin state.
        if fescapepressed.as_bool() || !unsafe { IsWindow(Some(self.owner)) }.as_bool() {
            DRAGDROP_S_CANCEL
        } else if (grfkeystate.0 & MK_LBUTTON.0) == 0 {
            DRAGDROP_S_DROP
        } else {
            HRESULT(0)
        }
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        if let Some(image) = &self.drag_image {
            image.move_to_cursor();
        }
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

struct OwnedHGlobal(HGLOBAL);

impl OwnedHGlobal {
    fn allocate(bytes: usize) -> Result<Self> {
        // SAFETY: `GHND` requests a movable, zero-initialized allocation and
        // `bytes` is the exact size subsequently written by the builder.
        unsafe { GlobalAlloc(GHND, bytes).map(Self) }
    }

    fn lock(&self) -> Result<ptr::NonNull<u8>> {
        // SAFETY: `self.0` is a live allocation exclusively owned by this
        // value. It remains alive until the matching `unlock`.
        let data = unsafe { GlobalLock(self.0) }.cast::<u8>();
        ptr::NonNull::new(data).ok_or_else(windows_core::Error::from_thread)
    }

    fn unlock(&self) {
        // SAFETY: Each builder calls this exactly once after a successful
        // `lock`, before the handle is freed or transferred.
        let _ = unsafe { GlobalUnlock(self.0) };
    }

    fn into_medium(self) -> STGMEDIUM {
        let hglobal = self.0;
        std::mem::forget(self);
        STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: hglobal },
            pUnkForRelease: ManuallyDrop::new(None::<IUnknown>),
        }
    }
}

impl Drop for OwnedHGlobal {
    fn drop(&mut self) {
        // SAFETY: While this owner exists, it holds the only responsibility
        // for a live, unlocked `HGLOBAL`. `into_medium` forgets the owner when
        // COM assumes that responsibility through `STGMEDIUM`.
        let _ = unsafe { GlobalFree(Some(self.0)) };
    }
}

fn build_hdrop(paths: &[PathBuf]) -> Result<OwnedHGlobal> {
    let mut encoded_paths = Vec::with_capacity(paths.len());
    let mut wide_len = 1usize;

    for path in paths {
        let mut encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
        encoded.push(0);
        wide_len += encoded.len();
        encoded_paths.push(encoded);
    }

    let header_size = size_of::<DROPFILES>();
    let bytes = header_size + wide_len * size_of::<u16>();
    let hglobal = OwnedHGlobal::allocate(bytes)?;
    let data = hglobal.lock()?.as_ptr();

    // SAFETY: `data` points to the `bytes`-sized allocation above. The header,
    // each encoded path, and the final terminator exactly fill that allocation
    // without overlap.
    unsafe {
        (data as *mut DROPFILES).write(DROPFILES {
            pFiles: header_size as u32,
            pt: POINT { x: 0, y: 0 },
            fNC: BOOL(0),
            fWide: BOOL(1),
        });

        let mut cursor = data.add(header_size) as *mut u16;
        for encoded in &encoded_paths {
            ptr::copy_nonoverlapping(encoded.as_ptr(), cursor, encoded.len());
            cursor = cursor.add(encoded.len());
        }
        cursor.write(0);
    }
    hglobal.unlock();

    Ok(hglobal)
}

fn build_u32_hglobal(value: u32) -> Result<OwnedHGlobal> {
    let hglobal = OwnedHGlobal::allocate(size_of::<u32>())?;
    let data = hglobal.lock()?.cast::<u32>().as_ptr();

    // SAFETY: `data` points to an aligned allocation of exactly one `u32`.
    unsafe {
        data.write(value);
    }
    hglobal.unlock();

    Ok(hglobal)
}

fn build_wide_string_hglobal(wide: &[u16]) -> Result<OwnedHGlobal> {
    let bytes = (wide.len() + 1) * size_of::<u16>();
    let hglobal = OwnedHGlobal::allocate(bytes)?;
    let data = hglobal.lock()?.cast::<u16>().as_ptr();

    // SAFETY: `data` points to `wide.len() + 1` writable `u16` elements. The
    // source is disjoint, and the final element is reserved for the terminator.
    unsafe {
        ptr::copy_nonoverlapping(wide.as_ptr(), data, wide.len());
        data.add(wide.len()).write(0);
    }
    hglobal.unlock();

    Ok(hglobal)
}
