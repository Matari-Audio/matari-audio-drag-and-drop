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
    /// Normalized piano-roll notes.
    Midi {
        /// Notes to draw in the source preview.
        notes: Vec<MidiPreviewNote>,
    },
}

pub(crate) const WIDTH: usize = 224;
pub(crate) const HEIGHT: usize = 90;

/// Premultiplied BGRA pixels for one drag thumbnail.
pub(crate) struct Canvas {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) pixels: Vec<u8>,
    scale: f32,
}

impl Canvas {
    fn px(&self, logical: usize) -> usize {
        (logical as f32 * self.scale).round() as usize
    }
}

/// Render at the logical `WIDTH`x`HEIGHT`.
#[cfg(not(target_os = "windows"))]
pub(crate) fn render(preview: &DragPreview) -> Vec<u8> {
    render_scaled(preview, 1.0).pixels
}

/// Render at `scale` times the logical size, for DPI-aware backends.
pub(crate) fn render_scaled(preview: &DragPreview, scale: f32) -> Canvas {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let width = (WIDTH as f32 * scale).round() as usize;
    let height = (HEIGHT as f32 * scale).round() as usize;
    let mut canvas = Canvas {
        width,
        height,
        pixels: vec![0_u8; width * height * 4],
        scale,
    };
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
        width - 2 * border,
        height - 2 * border,
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
    }
    canvas
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
    use super::*;

    #[test]
    fn scaled_render_matches_logical_size_times_scale() {
        let preview = DragPreview::Waveform {
            buckets: vec![(-0.5, 0.5); 32],
        };
        let canvas = render_scaled(&preview, 1.5);
        assert_eq!((canvas.width, canvas.height), (336, 135));
        assert_eq!(canvas.pixels.len(), 336 * 135 * 4);
        assert_eq!(
            render_scaled(&preview, 1.0).pixels.len(),
            WIDTH * HEIGHT * 4
        );
        // Corner stays transparent, border stays opaque at every scale.
        assert_eq!(canvas.pixels[3], 0);
        let mid_left = (canvas.height / 2 * canvas.width) * 4 + 3;
        assert_eq!(canvas.pixels[mid_left], 255);
    }
}
