//! X11 preview windows with explicit native pixel layout and cursor hotspot.

use x11rb::connection::Connection;
use x11rb::protocol::render::{self, PictType};
use x11rb::protocol::shape::{self, SK, SO};
use x11rb::protocol::xproto::{
    Colormap, ColormapAlloc, ConfigureWindowAux, ConnectionExt, CreateGCAux, CreateWindowAux,
    EventMask, Gcontext, ImageFormat, ImageOrder, Rectangle, Screen, StackMode, VisualClass,
    Visualtype, Window, WindowClass,
};

use super::{X11SessionError, atom, x11_error};

#[derive(Clone, Copy)]
struct PixelLayout {
    depth: u8,
    bpp: u8,
    pad: u8,
    order: ImageOrder,
    masks: [u32; 4],
}

impl PixelLayout {
    fn validate(self) -> Result<Self, X11SessionError> {
        if !matches!(self.bpp, 16 | 24 | 32)
            || !matches!(self.pad, 8 | 16 | 32)
            || !matches!(self.order, ImageOrder::LSB_FIRST | ImageOrder::MSB_FIRST)
            || self.depth > self.bpp
            || self.masks[..3].contains(&0)
        {
            return Err(X11SessionError::new("unsupported X11 preview pixel layout"));
        }
        let mut occupied = 0;
        for mask in self.masks {
            if mask == 0 {
                continue;
            }
            let normalized = mask >> mask.trailing_zeros();
            if normalized & normalized.wrapping_add(1) != 0
                || occupied & mask != 0
                || (self.bpp < 32 && mask >> self.bpp != 0)
            {
                return Err(X11SessionError::new(
                    "unsupported X11 preview channel masks",
                ));
            }
            occupied |= mask;
        }
        Ok(self)
    }

    fn stride(self, width: usize) -> usize {
        (width * usize::from(self.bpp)).div_ceil(usize::from(self.pad)) * usize::from(self.pad / 8)
    }

    fn pack(self, width: usize, height: usize, bgra: &[u8]) -> Vec<u8> {
        let stride = self.stride(width);
        let bytes = usize::from(self.bpp / 8);
        let mut packed = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let pixel = &bgra[(y * width + x) * 4..][..4];
                let mut channels = [pixel[2], pixel[1], pixel[0], pixel[3]];
                if self.masks[3] == 0 && pixel[3] != 0 {
                    // Binary Shape transparency is the non-composited fallback.
                    // Restore straight color so partial alpha cannot darken it.
                    for channel in &mut channels[..3] {
                        *channel = ((u32::from(*channel) * 255 + u32::from(pixel[3]) / 2)
                            / u32::from(pixel[3]))
                        .min(255) as u8;
                    }
                }
                let value =
                    channels
                        .into_iter()
                        .zip(self.masks)
                        .fold(0, |value, (channel, mask)| {
                            if mask == 0 {
                                return value;
                            }
                            let shift = mask.trailing_zeros();
                            value
                                | ((((u64::from(channel) * u64::from(mask >> shift) + 127) / 255)
                                    as u32)
                                    << shift)
                        });
                let encoded = if self.order == ImageOrder::LSB_FIRST {
                    value.to_le_bytes()
                } else {
                    value.to_be_bytes()
                };
                let encoded = if self.order == ImageOrder::LSB_FIRST {
                    &encoded[..bytes]
                } else {
                    &encoded[4 - bytes..]
                };
                packed[y * stride + x * bytes..][..bytes].copy_from_slice(encoded);
            }
        }
        packed
    }
}

pub(super) struct PreviewWindow {
    pub(super) window: Window,
    gc: Gcontext,
    colormap: Option<Colormap>,
    layout: PixelLayout,
    width: u16,
    height: u16,
    hotspot: [i32; 2],
    pixels: Vec<u8>,
}

impl PreviewWindow {
    pub(super) fn new<C: Connection>(
        conn: &C,
        root: Window,
        x: i16,
        y: i16,
        preview: &crate::DragPreview,
    ) -> Result<Self, X11SessionError> {
        let screen_index = conn
            .setup()
            .roots
            .iter()
            .position(|screen| screen.root == root)
            .ok_or_else(|| X11SessionError::new("X11 drag root has no matching screen"))?;
        let screen = &conn.setup().roots[screen_index];
        let shape_version = shape::query_version(conn)
            .map_err(x11_error)?
            .reply()
            .map_err(x11_error)?;
        if (shape_version.major_version, shape_version.minor_version) < (1, 1) {
            return Err(X11SessionError::new(
                "X11 preview requires Shape input regions",
            ));
        }
        let canvas = crate::preview::render_scaled(preview, preview.backing_scale() as f32)
            .map_err(x11_error)?;
        if canvas.width > i16::MAX as usize || canvas.height > i16::MAX as usize {
            return Err(X11SessionError::new(
                "X11 preview exceeds signed drawing coordinates",
            ));
        }
        let (visual, depth, alpha) =
            alpha_visual(conn, screen_index, screen).unwrap_or_else(|| {
                let visual = screen
                    .allowed_depths
                    .iter()
                    .flat_map(|depth| &depth.visuals)
                    .find(|visual| visual.visual_id == screen.root_visual)
                    .cloned();
                (visual, screen.root_depth, 0)
            });
        let visual = visual.ok_or_else(|| X11SessionError::new("X11 preview visual is absent"))?;
        if visual.class != VisualClass::TRUE_COLOR {
            return Err(X11SessionError::new(
                "X11 preview requires a TrueColor visual",
            ));
        }
        let format = conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|format| format.depth == depth)
            .ok_or_else(|| X11SessionError::new("X11 preview pixmap format is absent"))?;
        let layout = PixelLayout {
            depth,
            bpp: format.bits_per_pixel,
            pad: format.scanline_pad,
            order: conn.setup().image_byte_order,
            masks: [visual.red_mask, visual.green_mask, visual.blue_mask, alpha],
        }
        .validate()?;
        let pixels = layout.pack(canvas.width, canvas.height, &canvas.pixels);
        let colormap = if visual.visual_id != screen.root_visual {
            let id = conn.generate_id().map_err(x11_error)?;
            conn.create_colormap(ColormapAlloc::NONE, id, root, visual.visual_id)
                .map_err(x11_error)?
                .check()
                .map_err(x11_error)?;
            Some(id)
        } else {
            None
        };
        let window = match conn.generate_id() {
            Ok(id) => id,
            Err(error) => {
                if let Some(id) = colormap {
                    let _ = conn.free_colormap(id);
                }
                return Err(x11_error(error));
            }
        };
        let creation = conn
            .create_window(
                depth,
                window,
                root,
                0,
                0,
                canvas.width as u16,
                canvas.height as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                visual.visual_id,
                &CreateWindowAux::new()
                    .override_redirect(1)
                    .background_pixel(0)
                    .border_pixel(0)
                    .colormap(colormap.unwrap_or(screen.default_colormap))
                    .event_mask(EventMask::EXPOSURE),
            )
            .map_err(x11_error)
            .and_then(|cookie| cookie.check().map_err(x11_error));
        if let Err(error) = creation {
            if let Some(id) = colormap {
                let _ = conn.free_colormap(id);
            }
            return Err(error);
        }
        let gc = match conn.generate_id() {
            Ok(id) => id,
            Err(error) => {
                let _ = conn.destroy_window(window);
                if let Some(id) = colormap {
                    let _ = conn.free_colormap(id);
                }
                return Err(x11_error(error));
            }
        };
        let result = Self {
            window,
            gc,
            colormap,
            layout,
            width: canvas.width as u16,
            height: canvas.height as u16,
            hotspot: canvas.hotspot,
            pixels,
        };
        let attach = (|| {
            conn.create_gc(gc, window, &CreateGCAux::new())
                .map_err(x11_error)?
                .check()
                .map_err(x11_error)?;
            shape::rectangles(
                conn,
                SO::SET,
                SK::INPUT,
                x11rb::protocol::xproto::ClipOrdering::UNSORTED,
                window,
                0,
                0,
                &[],
            )
            .map_err(x11_error)?
            .check()
            .map_err(x11_error)?;
            if alpha == 0 {
                result.set_binary_shape(conn, &canvas.pixels)?;
            }
            result.move_to(conn, x, y)?;
            conn.map_window(window)
                .map_err(x11_error)?
                .check()
                .map_err(x11_error)?;
            // Window drawing is clipped to its mapped visible region. An
            // upload before MapWindow is discarded and the map paints zero.
            result.draw(conn)?;
            Ok(())
        })();
        if let Err(error) = attach {
            result.destroy(conn);
            return Err(error);
        }
        Ok(result)
    }

    fn set_binary_shape<C: Connection>(
        &self,
        conn: &C,
        pixels: &[u8],
    ) -> Result<(), X11SessionError> {
        let mut rectangles = Vec::new();
        for y in 0..usize::from(self.height) {
            let mut x = 0;
            while x < usize::from(self.width) {
                if pixels[(y * usize::from(self.width) + x) * 4 + 3] < 128 {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < usize::from(self.width)
                    && pixels[(y * usize::from(self.width) + x) * 4 + 3] >= 128
                {
                    x += 1;
                }
                rectangles.push(Rectangle {
                    x: start as i16,
                    y: y as i16,
                    width: (x - start) as u16,
                    height: 1,
                });
            }
        }
        // Empty SET clears all bounding input; subsequent chunks only union.
        shape::rectangles(
            conn,
            SO::SET,
            SK::BOUNDING,
            x11rb::protocol::xproto::ClipOrdering::UNSORTED,
            self.window,
            0,
            0,
            &[],
        )
        .map_err(x11_error)?
        .check()
        .map_err(x11_error)?;
        let chunk = (conn.maximum_request_bytes().saturating_sub(32) / 8).clamp(1, 4096);
        for rectangles in rectangles.chunks(chunk) {
            shape::rectangles(
                conn,
                SO::UNION,
                SK::BOUNDING,
                x11rb::protocol::xproto::ClipOrdering::UNSORTED,
                self.window,
                0,
                0,
                rectangles,
            )
            .map_err(x11_error)?
            .check()
            .map_err(x11_error)?;
        }
        Ok(())
    }

    pub(super) fn move_to<C: Connection>(
        &self,
        conn: &C,
        x: i16,
        y: i16,
    ) -> Result<(), X11SessionError> {
        let coordinate = |position: i16, hotspot: i32| {
            (i64::from(position) - i64::from(hotspot))
                .clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i32
        };
        conn.configure_window(
            self.window,
            &ConfigureWindowAux::new()
                .x(coordinate(x, self.hotspot[0]))
                .y(coordinate(y, self.hotspot[1]))
                .stack_mode(StackMode::ABOVE),
        )
        .map_err(x11_error)?
        .check()
        .map_err(x11_error)?;
        conn.flush().map_err(x11_error)
    }

    pub(super) fn draw<C: Connection>(&self, conn: &C) -> Result<(), X11SessionError> {
        let stride = self.layout.stride(usize::from(self.width));
        let rows = (conn.maximum_request_bytes().saturating_sub(32) / stride)
            .min(usize::from(self.height));
        if rows == 0 {
            return Err(X11SessionError::new(
                "X11 preview row exceeds the maximum request size",
            ));
        }
        for y in (0..usize::from(self.height)).step_by(rows) {
            let count = rows.min(usize::from(self.height) - y);
            conn.put_image(
                ImageFormat::Z_PIXMAP,
                self.window,
                self.gc,
                self.width,
                count as u16,
                0,
                y as i16,
                0,
                self.layout.depth,
                &self.pixels[y * stride..(y + count) * stride],
            )
            .map_err(x11_error)?
            .check()
            .map_err(x11_error)?;
        }
        conn.flush().map_err(x11_error)
    }

    pub(super) fn destroy<C: Connection>(self, conn: &C) {
        let _ = conn.free_gc(self.gc);
        let _ = conn.destroy_window(self.window);
        if let Some(id) = self.colormap {
            let _ = conn.free_colormap(id);
        }
    }
}

fn alpha_visual<C: Connection>(
    conn: &C,
    screen_index: usize,
    screen: &Screen,
) -> Option<(Option<Visualtype>, u8, u32)> {
    let owner = atom(conn, format!("_NET_WM_CM_S{screen_index}").as_bytes()).ok()?;
    if conn.get_selection_owner(owner).ok()?.reply().ok()?.owner == x11rb::NONE {
        return None;
    }
    render::query_version(conn, 0, 11).ok()?.reply().ok()?;
    let formats = render::query_pict_formats(conn).ok()?.reply().ok()?;
    let screen_formats = formats.screens.get(screen_index)?;
    for depth in &screen_formats.depths {
        if depth.depth != 32 {
            continue;
        }
        for visual in &depth.visuals {
            let Some(format) = formats.formats.iter().find(|format| {
                format.id == visual.format
                    && format.type_ == PictType::DIRECT
                    && format.direct.alpha_mask != 0
            }) else {
                continue;
            };
            let Some(xvisual) = screen
                .allowed_depths
                .iter()
                .flat_map(|depth| &depth.visuals)
                .find(|candidate| {
                    candidate.visual_id == visual.visual
                        && candidate.class == VisualClass::TRUE_COLOR
                })
            else {
                continue;
            };
            let masks = [
                format.direct.red_mask,
                format.direct.green_mask,
                format.direct.blue_mask,
            ]
            .into_iter()
            .zip([
                format.direct.red_shift,
                format.direct.green_shift,
                format.direct.blue_shift,
            ])
            .map(|(mask, shift)| u32::from(mask).checked_shl(u32::from(shift)))
            .collect::<Option<Vec<_>>>()?;
            if masks != [xvisual.red_mask, xvisual.green_mask, xvisual.blue_mask] {
                continue;
            }
            let alpha = u32::from(format.direct.alpha_mask)
                .checked_shl(u32::from(format.direct.alpha_shift))?;
            return Some((Some(*xvisual), 32, alpha));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_masks_endianness_alpha_and_scanline_padding_are_respected() {
        let rgba = [0x20, 0x40, 0x60, 0x80];
        let layout = PixelLayout {
            depth: 32,
            bpp: 32,
            pad: 32,
            order: ImageOrder::LSB_FIRST,
            masks: [0xff0000, 0xff00, 0xff, 0xff000000],
        }
        .validate()
        .unwrap();
        assert_eq!(layout.pack(1, 1, &rgba), rgba);
        assert_eq!(
            PixelLayout {
                order: ImageOrder::MSB_FIRST,
                ..layout
            }
            .pack(1, 1, &rgba),
            [0x80, 0x60, 0x40, 0x20]
        );
        let rgb565 = PixelLayout {
            depth: 16,
            bpp: 16,
            pad: 32,
            order: ImageOrder::LSB_FIRST,
            masks: [0xf800, 0x7e0, 0x1f, 0],
        }
        .validate()
        .unwrap();
        assert_eq!(rgb565.pack(1, 1, &[0, 0, 255, 255]), [0, 0xf8, 0, 0]);
        assert_eq!(
            PixelLayout {
                order: ImageOrder::MSB_FIRST,
                ..rgb565
            }
            .pack(1, 1, &[0, 0, 255, 255]),
            [0xf8, 0, 0, 0]
        );
        let rgb24 = PixelLayout {
            depth: 24,
            bpp: 24,
            pad: 32,
            masks: [0xff0000, 0xff00, 0xff, 0],
            ..layout
        };
        assert_eq!(rgb24.pack(1, 1, &[1, 2, 3, 255]), [1, 2, 3, 0]);
        assert_eq!(rgb24.pack(1, 1, &[32, 64, 96, 128]), [64, 128, 191, 0]);
    }

    #[test]
    fn unsupported_or_overlapping_layouts_are_refused() {
        let layout = PixelLayout {
            depth: 24,
            bpp: 32,
            pad: 32,
            order: ImageOrder::LSB_FIRST,
            masks: [0xff0000, 0xff00, 0xff, 0],
        };
        assert!(PixelLayout { bpp: 8, ..layout }.validate().is_err());
        assert!(
            PixelLayout {
                masks: [0xff, 0xff, 0xff, 0],
                ..layout
            }
            .validate()
            .is_err()
        );
        assert!(
            PixelLayout {
                masks: [0xfa0000, 0xff00, 0xff, 0],
                ..layout
            }
            .validate()
            .is_err()
        );
    }
}
