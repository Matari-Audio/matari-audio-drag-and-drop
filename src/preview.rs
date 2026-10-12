/// One normalized MIDI note in a drag preview.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MidiPreviewNote {
    /// Note start, where `0.0` is the left edge and `1.0` is the right edge.
    pub start: f32,
    /// Note end in the same normalized time range.
    pub end: f32,
    /// Pitch, where `0.0` is the bottom and `1.0` is the top.
    pub pitch: f32,
}

/// Invalid drag bitmap data or logical geometry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PreviewImageError {
    /// Dimensions are zero, overflow, or exceed the bounded image budget.
    Dimensions,
    /// The pixel count does not match the bitmap dimensions exactly.
    ByteLength,
    /// A color component exceeds its alpha in premultiplied BGRA.
    Alpha,
    /// Logical extent, backing scale, or hotspot is not finite and valid.
    Geometry,
}

impl std::fmt::Display for PreviewImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Dimensions => "drag bitmap dimensions are invalid or too large",
            Self::ByteLength => "drag bitmap byte length does not match its dimensions",
            Self::Alpha => "drag bitmap colors must be premultiplied by alpha",
            Self::Geometry => "drag preview geometry is invalid",
        })
    }
}

impl std::error::Error for PreviewImageError {}

/// Owned top-down, tightly packed premultiplied BGRA8 bitmap in sRGB.
///
/// Bitmap dimensions describe backing pixels. Logical size and the cursor
/// hotspot describe toolkit points, independent of plugin position or DPI.
#[derive(Clone, Debug, PartialEq)]
pub struct DragImage {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
    logical_size: [f64; 2],
    hotspot: [f64; 2],
}

impl DragImage {
    /// Validate a bitmap, initially at one backing pixel per logical point.
    /// The initial hotspot is the image center.
    ///
    /// # Errors
    /// Returns an error for zero/oversized dimensions, mismatched byte length,
    /// or non-premultiplied color components. Images are bounded to 16M pixels.
    pub fn new(width: usize, height: usize, pixels: Vec<u8>) -> Result<Self, PreviewImageError> {
        validate_pixels(width, height, &pixels)?;
        Ok(Self {
            width,
            height,
            pixels,
            logical_size: [width as f64, height as f64],
            hotspot: [width as f64 * 0.5, height as f64 * 0.5],
        })
    }

    /// Assign logical extent and cursor-relative hotspot, measured from the
    /// top-left image corner. A hotspot outside the image is permitted.
    ///
    /// # Errors
    /// Logical dimensions must be finite and positive; hotspot coordinates
    /// must be finite. Both are bounded to native signed-coordinate limits.
    pub fn with_geometry(
        mut self,
        logical_size: [f64; 2],
        hotspot: [f64; 2],
    ) -> Result<Self, PreviewImageError> {
        validate_geometry(logical_size, hotspot)?;
        self.logical_size = logical_size;
        self.hotspot = hotspot;
        Ok(self)
    }

    /// Bitmap dimensions in backing pixels.
    #[must_use]
    pub const fn bitmap_size(&self) -> [usize; 2] {
        [self.width, self.height]
    }

    /// Validated premultiplied BGRA8 bytes in row order.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Logical extent in toolkit points.
    #[must_use]
    pub const fn logical_size(&self) -> [f64; 2] {
        self.logical_size
    }

    /// Cursor position measured from the top-left logical image corner.
    #[must_use]
    pub const fn hotspot(&self) -> [f64; 2] {
        self.hotspot
    }
}

fn pixel_bytes(width: usize, height: usize) -> Result<usize, PreviewImageError> {
    if width == 0 || height == 0 || width > u16::MAX as usize || height > u16::MAX as usize {
        return Err(PreviewImageError::Dimensions);
    }
    let count = width
        .checked_mul(height)
        .ok_or(PreviewImageError::Dimensions)?;
    if count > 16 * 1024 * 1024 {
        return Err(PreviewImageError::Dimensions);
    }
    count.checked_mul(4).ok_or(PreviewImageError::Dimensions)
}

fn validate_pixels(width: usize, height: usize, pixels: &[u8]) -> Result<(), PreviewImageError> {
    if pixels.len() != pixel_bytes(width, height)? {
        return Err(PreviewImageError::ByteLength);
    }
    if pixels
        .as_chunks::<4>()
        .0
        .iter()
        .any(|pixel| pixel[..3].iter().any(|color| *color > pixel[3]))
    {
        return Err(PreviewImageError::Alpha);
    }
    Ok(())
}

fn validate_geometry(size: [f64; 2], hotspot: [f64; 2]) -> Result<(), PreviewImageError> {
    if size
        .iter()
        .any(|v| !v.is_finite() || *v <= 0.0 || *v > f64::from(u16::MAX))
        || hotspot
            .iter()
            .any(|v| !v.is_finite() || v.abs() > f64::from(i32::MAX))
    {
        return Err(PreviewImageError::Geometry);
    }
    Ok(())
}

/// Optional source-side image data for an outbound file drag.
#[derive(Clone, Debug, PartialEq)]
pub enum DragPreview {
    /// Min/max waveform buckets in normalized audio amplitude.
    Waveform {
        /// Ordered `(minimum, maximum)` sample buckets.
        buckets: Vec<(f32, f32)>,
    },
    /// Column-major normalized spectral energy.
    Spectral {
        /// Number of time columns.
        columns: usize,
        /// Number of frequency rows.
        rows: usize,
        /// Energy at `column * rows + row`, clamped to `0.0..=1.0` when drawn.
        energy: Vec<f32>,
    },
    /// The whole thumbnail, drawn by the caller: premultiplied BGRA,
    /// [`PREVIEW_WIDTH`] by [`PREVIEW_HEIGHT`], row by row.
    Image {
        /// `PREVIEW_WIDTH * PREVIEW_HEIGHT * 4` bytes.
        pixels: Vec<u8>,
    },
    /// Validated bitmap with explicit logical extent and cursor hotspot.
    Bitmap(DragImage),
    /// Normalized piano-roll notes.
    Midi {
        /// Notes to draw in the source preview.
        notes: Vec<MidiPreviewNote>,
    },
}

impl DragPreview {
    /// Validate legacy bitmap input before allocating native resources.
    ///
    /// # Errors
    /// Legacy images must contain exactly the documented number of
    /// premultiplied BGRA bytes. Validated [`DragImage`] values remain valid.
    pub fn validate(&self) -> Result<(), PreviewImageError> {
        match self {
            Self::Image { pixels } => validate_pixels(WIDTH, HEIGHT, pixels),
            _ => Ok(()),
        }
    }

    /// Logical image extent in toolkit points.
    #[must_use]
    pub fn logical_size(&self) -> [f64; 2] {
        match self {
            Self::Bitmap(image) => image.logical_size(),
            _ => [WIDTH as f64, HEIGHT as f64],
        }
    }

    /// Cursor hotspot from the image's logical top-left corner.
    #[must_use]
    pub fn hotspot(&self) -> [f64; 2] {
        match self {
            Self::Bitmap(image) => image.hotspot(),
            _ => [WIDTH as f64 * 0.5, HEIGHT as f64 * 0.5],
        }
    }

    /// Backing scale that preserves the caller's bitmap detail.
    #[must_use]
    pub fn backing_scale(&self) -> f64 {
        match self {
            Self::Bitmap(image) => (image.width as f64 / image.logical_size[0])
                .max(image.height as f64 / image.logical_size[1]),
            _ => 1.0,
        }
    }
}

pub(crate) const WIDTH: usize = 224;
pub(crate) const HEIGHT: usize = 90;
/// The width of a drag thumbnail at a backing scale of one, in pixels.
pub const PREVIEW_WIDTH: usize = WIDTH;
/// The height of a drag thumbnail at a backing scale of one, in pixels.
pub const PREVIEW_HEIGHT: usize = HEIGHT;

/// Premultiplied BGRA pixels for one drag thumbnail.
pub(crate) struct Canvas {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) pixels: Vec<u8>,
    #[cfg_attr(
        all(target_os = "macos", not(test)),
        expect(
            dead_code,
            reason = "AppKit places an NSImage using logical point geometry"
        )
    )]
    pub(crate) hotspot: [i32; 2],
    scale: f32,
}

impl Canvas {
    fn px(&self, logical: usize) -> usize {
        (logical as f32 * self.scale).round() as usize
    }
}

/// Preserve original bitmap backing pixels for point-sized native images.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn render_original(preview: &DragPreview) -> Result<Canvas, PreviewImageError> {
    preview.validate()?;
    if let DragPreview::Bitmap(image) = preview {
        return Ok(Canvas {
            width: image.width,
            height: image.height,
            pixels: image.pixels.clone(),
            hotspot: [
                (image.hotspot[0] * image.width as f64 / image.logical_size[0]).round() as i32,
                (image.hotspot[1] * image.height as f64 / image.logical_size[1]).round() as i32,
            ],
            scale: preview.backing_scale() as f32,
        });
    }
    render_scaled(preview, 1.0)
}

/// AppKit's default bitmap layout uses premultiplied RGBA.
#[cfg(test)]
pub(crate) fn render_rgba(preview: &DragPreview) -> Result<Canvas, PreviewImageError> {
    let mut canvas = render_original(preview)?;
    for pixel in canvas.pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(canvas)
}

/// Render at `scale` times the logical size, for DPI-aware backends.
pub(crate) fn render_scaled(
    preview: &DragPreview,
    scale: f32,
) -> Result<Canvas, PreviewImageError> {
    preview.validate()?;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(PreviewImageError::Geometry);
    }
    let size = preview.logical_size();
    let width = (size[0] * f64::from(scale)).round() as usize;
    let height = (size[1] * f64::from(scale)).round() as usize;
    let bytes = pixel_bytes(width, height)?;
    let hotspot = preview.hotspot();
    let mut canvas = Canvas {
        width,
        height,
        pixels: vec![0_u8; bytes],
        hotspot: [
            (hotspot[0] * f64::from(scale)).round() as i32,
            (hotspot[1] * f64::from(scale)).round() as i32,
        ],
        scale,
    };
    let bitmap = match preview {
        DragPreview::Image { pixels } => Some((WIDTH, HEIGHT, pixels.as_slice())),
        DragPreview::Bitmap(image) => Some((image.width, image.height, image.pixels.as_slice())),
        _ => None,
    };
    if let Some((bitmap_width, bitmap_height, pixels)) = bitmap {
        // Validation makes every sampled pixel available. Preserve the caller's
        // explicit bitmap/point relationship rather than guessing it from DPI.
        for y in 0..height {
            for x in 0..width {
                let (sx, sy) = (
                    (x * bitmap_width / width).min(bitmap_width - 1),
                    (y * bitmap_height / height).min(bitmap_height - 1),
                );
                let from = (sy * bitmap_width + sx) * 4;
                canvas.pixels[(y * width + x) * 4..][..4].copy_from_slice(&pixels[from..from + 4]);
            }
        }
        return Ok(canvas);
    }
    let radius = canvas.px(12);
    let (sx, sy) = (canvas.px(3), canvas.px(4));
    fill_rounded_rect(
        &mut canvas,
        sx,
        sy,
        width - sx,
        height - sy,
        radius,
        [0, 0, 0, 64],
    );
    fill_rounded_rect(
        &mut canvas,
        0,
        0,
        width,
        height,
        radius,
        [22, 141, 204, 255],
    );
    let border = canvas.px(1).max(1);
    fill_rounded_rect(
        &mut canvas,
        border,
        border,
        width.saturating_sub(2 * border),
        height.saturating_sub(2 * border),
        radius.saturating_sub(border),
        [25, 25, 25, 255],
    );

    match preview {
        DragPreview::Waveform { buckets } => waveform(&mut canvas, buckets),
        DragPreview::Spectral {
            columns,
            rows,
            energy,
        } => spectral(&mut canvas, *columns, *rows, energy),
        DragPreview::Midi { notes } => midi(&mut canvas, notes),
        DragPreview::Image { .. } | DragPreview::Bitmap(_) => {}
    }
    Ok(canvas)
}

fn waveform(canvas: &mut Canvas, buckets: &[(f32, f32)]) {
    let left = canvas.px(15);
    let right = canvas.width - canvas.px(16);
    let top = canvas.px(15);
    let bottom = canvas.height - canvas.px(17);
    let bar = canvas.px(2).max(1);
    let center = (top + bottom) / 2;
    horizontal(canvas, left, right, center, [22, 141, 204, 128]);
    let last = buckets.len().saturating_sub(1).max(1);
    for (index, &(minimum, maximum)) in buckets.iter().enumerate() {
        let x = left + index * (right - left) / last;
        let amplitude = (bottom - top) as f32 * 0.42;
        let a = (center as f32 - maximum.clamp(-1.0, 1.0) * amplitude)
            .round()
            .clamp(top as f32, bottom as f32) as usize;
        let b = (center as f32 - minimum.clamp(-1.0, 1.0) * amplitude)
            .round()
            .clamp(top as f32, bottom as f32) as usize;
        fill_rect(
            canvas,
            x,
            a.min(b),
            bar,
            a.max(b).saturating_sub(a.min(b)).max(1),
            [0, 170, 255, 255],
        );
    }
}

fn spectral(canvas: &mut Canvas, columns: usize, rows: usize, energy: &[f32]) {
    if columns == 0 || rows == 0 {
        return;
    }
    let left = canvas.px(14);
    let top = canvas.px(14);
    let draw_width = canvas.width - 2 * left;
    let draw_height = canvas.height - 2 * top;
    for y in 0..draw_height {
        for x in 0..draw_width {
            let column = x as f32 * columns.saturating_sub(1) as f32
                / draw_width.saturating_sub(1).max(1) as f32;
            let row = (draw_height - 1 - y) as f32 * rows.saturating_sub(1) as f32
                / draw_height.saturating_sub(1).max(1) as f32;
            let value = sample_spectral(energy, columns, rows, column, row);
            set_pixel(canvas, left + x, top + y, spectral_color(value));
        }
    }
}

fn sample_spectral(energy: &[f32], columns: usize, rows: usize, column: f32, row: f32) -> f32 {
    let x0 = column.floor() as usize;
    let y0 = row.floor() as usize;
    let x1 = (x0 + 1).min(columns - 1);
    let y1 = (y0 + 1).min(rows - 1);
    let tx = column - x0 as f32;
    let ty = row - y0 as f32;
    let at = |x: usize, y: usize| {
        energy
            .get(x.saturating_mul(rows).saturating_add(y))
            .copied()
            .unwrap_or_default()
            .clamp(0.0, 1.0)
    };
    let top = at(x0, y0) + (at(x1, y0) - at(x0, y0)) * tx;
    let bottom = at(x0, y1) + (at(x1, y1) - at(x0, y1)) * tx;
    top + (bottom - top) * ty
}

fn midi(canvas: &mut Canvas, notes: &[MidiPreviewNote]) {
    let left = canvas.px(14) as f32;
    let top = canvas.px(14) as f32;
    let draw_width = canvas.width as f32 - 2.0 * left;
    let draw_height = canvas.height as f32 - 2.0 * top;
    if notes.is_empty() {
        horizontal(
            canvas,
            left as usize,
            (left + draw_width) as usize,
            (top + draw_height * 0.5) as usize,
            [55, 72, 92, 255],
        );
        return;
    }
    let scale = canvas.scale;
    for note in notes.iter().take(96) {
        let start = note.start.clamp(0.0, 1.0);
        let end = note.end.max(start + 0.01).clamp(0.0, 1.0);
        let height = (draw_height / 18.0).clamp(3.0 * scale, 7.0 * scale);
        fill_rect(
            canvas,
            (left + start * draw_width).round() as usize,
            (top + (1.0 - note.pitch.clamp(0.0, 1.0)) * (draw_height - height)).round() as usize,
            ((end - start) * draw_width).round().max(2.0 * scale) as usize,
            height.round() as usize,
            [0, 170, 255, 255],
        );
    }
}

fn spectral_color(value: f32) -> [u8; 4] {
    let value = value.clamp(0.0, 1.0).powf(0.72);
    let (from, to, amount) = if value < 0.28 {
        ([22.0, 20.0, 29.0], [90.0, 32.0, 118.0], value / 0.28)
    } else if value < 0.68 {
        (
            [90.0, 32.0, 118.0],
            [0.0, 184.0, 255.0],
            (value - 0.28) / 0.40,
        )
    } else {
        (
            [0.0, 184.0, 255.0],
            [255.0, 150.0, 52.0],
            (value - 0.68) / 0.32,
        )
    };
    [
        (from[0] + (to[0] - from[0]) * amount).round() as u8,
        (from[1] + (to[1] - from[1]) * amount).round() as u8,
        (from[2] + (to[2] - from[2]) * amount).round() as u8,
        255,
    ]
}

fn fill_rect(canvas: &mut Canvas, x: usize, y: usize, width: usize, height: usize, color: [u8; 4]) {
    for row in y..y.saturating_add(height).min(canvas.height) {
        for column in x..x.saturating_add(width).min(canvas.width) {
            set_pixel(canvas, column, row, color);
        }
    }
}

fn fill_rounded_rect(
    canvas: &mut Canvas,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    radius: usize,
    color: [u8; 4],
) {
    let right = x.saturating_add(width).min(canvas.width);
    let bottom = y.saturating_add(height).min(canvas.height);
    let radius = radius.min(width / 2).min(height / 2);
    for row in y..bottom {
        for column in x..right {
            let dx = if column < x + radius {
                x + radius - column
            } else {
                column.saturating_sub(right.saturating_sub(radius + 1))
            };
            let dy = if row < y + radius {
                y + radius - row
            } else {
                row.saturating_sub(bottom.saturating_sub(radius + 1))
            };
            if dx == 0 || dy == 0 || dx * dx + dy * dy <= radius * radius {
                set_pixel(canvas, column, row, color);
            }
        }
    }
}

fn horizontal(canvas: &mut Canvas, left: usize, right: usize, y: usize, color: [u8; 4]) {
    for x in left..=right.min(canvas.width - 1) {
        set_pixel(canvas, x, y, color);
    }
}

fn set_pixel(canvas: &mut Canvas, x: usize, y: usize, rgba: [u8; 4]) {
    if x >= canvas.width || y >= canvas.height {
        return;
    }
    let offset = (y * canvas.width + x) * 4;
    let alpha = u16::from(rgba[3]);
    canvas.pixels[offset..offset + 4].copy_from_slice(&[
        (u16::from(rgba[2]) * alpha / 255) as u8,
        (u16::from(rgba[1]) * alpha / 255) as u8,
        (u16::from(rgba[0]) * alpha / 255) as u8,
        rgba[3],
    ]);
}

#[cfg(test)]
mod tests {
    #[test]
    fn appkit_pixels_preserve_alpha_and_put_red_first() {
        let mut pixels = vec![0; super::WIDTH * super::HEIGHT * 4];
        pixels[..4].copy_from_slice(&[8, 16, 32, 64]);
        let rgba = super::render_rgba(&super::DragPreview::Image { pixels }).unwrap();
        assert_eq!(&rgba.pixels[..4], &[32, 16, 8, 64]);
        assert!(rgba.pixels[4..].iter().all(|byte| *byte == 0));
    }

    use super::*;

    #[test]
    fn an_image_is_the_whole_thumbnail() {
        let mut pixels = vec![0_u8; WIDTH * HEIGHT * 4];
        pixels[..4].copy_from_slice(&[1, 2, 3, 255]);
        let preview = DragPreview::Image {
            pixels: pixels.clone(),
        };
        assert_eq!(render_scaled(&preview, 1.0).unwrap().pixels, pixels);
        let big = render_scaled(&preview, 2.0).unwrap();
        assert_eq!(&big.pixels[..8], &[1, 2, 3, 255, 1, 2, 3, 255]);
    }

    #[test]
    fn scaled_render_matches_logical_size_times_scale() {
        let preview = DragPreview::Waveform {
            buckets: vec![(-0.5, 0.5); 32],
        };
        let canvas = render_scaled(&preview, 1.5).unwrap();
        assert_eq!((canvas.width, canvas.height), (336, 135));
        assert_eq!(canvas.pixels.len(), 336 * 135 * 4);
        assert_eq!(
            render_scaled(&preview, 1.0).unwrap().pixels.len(),
            WIDTH * HEIGHT * 4
        );
        // Corner stays transparent, border stays opaque at every scale.
        assert_eq!(canvas.pixels[3], 0);
        let mid_left = (canvas.height / 2 * canvas.width) * 4 + 3;
        assert_eq!(canvas.pixels[mid_left], 255);
    }

    #[test]
    fn bitmap_contract_rejects_malformed_and_unbounded_images() {
        assert_eq!(
            DragImage::new(0, 1, Vec::new()).unwrap_err(),
            PreviewImageError::Dimensions
        );
        assert_eq!(
            DragImage::new(usize::MAX, 2, Vec::new()).unwrap_err(),
            PreviewImageError::Dimensions
        );
        assert_eq!(
            DragImage::new(1, 1, vec![0; 3]).unwrap_err(),
            PreviewImageError::ByteLength
        );
        assert_eq!(
            DragImage::new(1, 1, vec![1, 0, 0, 0]).unwrap_err(),
            PreviewImageError::Alpha
        );
        let old = DragPreview::Image { pixels: vec![0; 3] };
        assert!(render_scaled(&old, 1.0).is_err());
        let image = DragImage::new(1, 1, vec![0; 4]).unwrap();
        for size in [[f64::NAN, 1.0], [0.0, 1.0], [1.0, f64::INFINITY]] {
            assert!(image.clone().with_geometry(size, [0.0, 0.0]).is_err());
        }
        assert!(image.with_geometry([1.0, 1.0], [f64::NAN, 0.0]).is_err());
    }

    #[test]
    fn backing_pixels_logical_extent_and_hotspot_convert_once() {
        let pixels = vec![3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255];
        let image = DragImage::new(2, 2, pixels.clone())
            .unwrap()
            .with_geometry([1.0, 1.0], [0.25, 0.5])
            .unwrap();
        let preview = DragPreview::Bitmap(image);
        assert_eq!(preview.backing_scale(), 2.0);
        let native = render_original(&preview).unwrap();
        assert_eq!((native.width, native.height), (2, 2));
        assert_eq!(native.pixels, pixels);
        let two = render_scaled(&preview, 2.0).unwrap();
        assert_eq!(two.pixels, pixels);
        assert_eq!(two.hotspot, [1, 1]);
        let one = render_scaled(&preview, 1.0).unwrap();
        assert_eq!((one.width, one.height), (1, 1));
        assert_eq!(one.pixels, [3, 2, 1, 255]);
        assert!(render_scaled(&preview, f32::INFINITY).is_err());
        assert!(render_scaled(&preview, 1.0e10).is_err());
    }
}
