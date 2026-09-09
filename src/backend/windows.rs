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
    SelectObject,
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
    DPI_AWARENESS_CONTEXT, GetDpiForWindow, GetWindowDpiAwarenessContext,
    SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Shell::{
    CFSTR_FILENAMEW, CFSTR_PREFERREDDROPEFFECT, DROPFILES, SHCreateStdEnumFmtEtc,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GetCursorPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SetWindowPos, ULW_ALPHA, USER_DEFAULT_SCREEN_DPI, UpdateLayeredWindow,
    WS_DISABLED, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
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
    let drop_source: IDropSource = FileDropSource { drag_image }.into();
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

/// Source-side drag thumbnail: a layered top-level window that follows the
/// cursor at the same offset as the in-app chip.
///
/// The Shell `IDragSourceHelper` only draws when the drop target calls
/// `IDropTargetHelper` (Explorer does; DAWs mostly do not), and the legacy
/// `ImageList_DragMove` path paints a fixed-size, ~75% alpha image straight on
/// the screen DC, which is what users saw as "translucent, wrong size, jumps
/// to the corner". A layered window owned by us is composited by DWM, keeps
/// full alpha, and is sized at the plugin window's DPI.
struct SourceDragImage {
    hwnd: HWND,
    _bitmap: DragBitmapGuard,
    context: DPI_AWARENESS_CONTEXT,
    offset: POINT,
}

/// In-app chip offset from the cursor, in logical pixels
/// (`src/export/drag.rs` `CHIP_OFFSET` on the BUFFR side).
const CHIP_OFFSET: (f32, f32) = (20.0, 22.0);

impl SourceDragImage {
    fn new(preview: &DragPreview, owner: HWND) -> std::result::Result<Self, PreviewAttachError> {
        // SAFETY: `owner` is the live plugin window this drag started from.
        let context = unsafe { GetWindowDpiAwarenessContext(owner) };
        let _dpi = ThreadDpiContext::enter(context);
        // SAFETY: `owner` is a live window; a zero result means "unaware".
        let dpi = unsafe { GetDpiForWindow(owner) };
        let scale = if dpi == 0 {
            1.0
        } else {
            dpi as f32 / USER_DEFAULT_SCREEN_DPI as f32
        };
        let canvas = crate::preview::render_scaled(preview, scale);
        let bitmap = DragBitmapGuard(
            create_drag_bitmap(&canvas.pixels, canvas.width, canvas.height)
                .map_err(|_| PreviewAttachError::new(PreviewFailureStage::Bitmap, None))?,
        );
        let offset = POINT {
            x: (CHIP_OFFSET.0 * scale).round() as i32,
            y: (CHIP_OFFSET.1 * scale).round() as i32,
        };
        let mut cursor = POINT::default();
        // SAFETY: `cursor` is a writable `POINT`.
        let _ = unsafe { GetCursorPos(&mut cursor) };
        // SAFETY: "STATIC" is a system window class, all pointer arguments are
        // null, and the window is destroyed in `Drop`.
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
                cursor.x + offset.x,
                cursor.y + offset.y,
                canvas.width as i32,
                canvas.height as i32,
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
            _bitmap: bitmap,
            context,
            offset,
        };
        // SAFETY: `hwnd` is the layered window created above and the DIB is
        // alive for the whole call; the memory DC is released before return.
        unsafe {
            let source = CreateCompatibleDC(None);
            let previous = SelectObject(source, image._bitmap.0.into());
            let size = SIZE {
                cx: canvas.width as i32,
                cy: canvas.height as i32,
            };
            let origin = POINT::default();
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let result = UpdateLayeredWindow(
                hwnd,
                None,
                None,
                Some(&size),
                Some(source),
                Some(&origin),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            SelectObject(source, previous);
            let _ = DeleteDC(source);
            result.map_err(|error| {
                PreviewAttachError::new(PreviewFailureStage::Attach, Some(error.code().0))
            })?;
        }
        image.move_to_cursor();
        Ok(image)
    }

    fn move_to_cursor(&self) {
        let _dpi = ThreadDpiContext::enter(self.context);
        let mut cursor = POINT::default();
        // SAFETY: `cursor` is a writable `POINT` and `hwnd` is alive until `Drop`.
        unsafe {
            if GetCursorPos(&mut cursor).is_ok() {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    cursor.x + self.offset.x,
                    cursor.y + self.offset.y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
        }
    }
}

impl Drop for SourceDragImage {
    fn drop(&mut self) {
        // SAFETY: This struct uniquely owns the window it created.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Runs cursor and window calls in the plugin window's DPI awareness context
/// so `GetCursorPos`, `GetDpiForWindow` and `SetWindowPos` agree on one
/// coordinate space, whatever the host process default is.
struct ThreadDpiContext(DPI_AWARENESS_CONTEXT);

impl ThreadDpiContext {
    fn enter(context: DPI_AWARENESS_CONTEXT) -> Self {
        // SAFETY: Thread-local state change, restored in `Drop`.
        Self(unsafe { SetThreadDpiAwarenessContext(context) })
    }
}

impl Drop for ThreadDpiContext {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: Restores the context saved in `enter` on the same thread.
            unsafe {
                SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
}

struct PreviewAttachError {
    stage: PreviewFailureStage,
    native_code: Option<i32>,
}

impl PreviewAttachError {
    fn new(stage: PreviewFailureStage, native_code: Option<i32>) -> Self {
        Self { stage, native_code }
    }
}

fn create_drag_bitmap(
    rgba: &[u8],
    width: usize,
    height: usize,
) -> std::result::Result<HBITMAP, String> {
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
    drag_image: Option<SourceDragImage>,
}

#[allow(non_snake_case)]
impl IDropSource_Impl for FileDropSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if let Some(image) = &self.drag_image {
            image.move_to_cursor();
        }
        if fescapepressed.as_bool() {
            DRAGDROP_S_CANCEL
        } else if (grfkeystate.0 & MK_LBUTTON.0) == 0 {
            DRAGDROP_S_DROP
        } else {
            HRESULT(0)
        }
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
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
