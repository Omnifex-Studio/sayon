//! Drawing for `standard/graphics`. A canvas is pixels the runtime holds,
//! in a stream entry, so that drawing is the same code, and gives the same
//! pixels, in the interpreter and in compiled programs on every target: it
//! uses integers and IEEE arithmetic only, with no platform's renderer. A
//! window only shows a canvas (see `window`).
//!
//! Pixels are 8-bit red, green, blue, and alpha, not premultiplied, in rows
//! from the top. A color crosses from the library packed in an integer as
//! `0xRRGGBBAA`. Drawing blends the source over what is there. Rectangles,
//! circles, and polygons cover the pixels whose centers they hold; a
//! polygon's points are pixel corners, so a square of 10 covers 10 by 10
//! pixels as a rectangle of 10 does, and a line runs between the centers
//! of the pixels it names.

use crate::font_table::GLYPHS;
use crate::host::HostError;

/// The longest side of a canvas, and the most pixels one holds.
const LONGEST_SIDE: i64 = 16384;
const MOST_PIXELS: i64 = 1 << 26;

/// What a character the font lacks looks like: a box.
const MISSING: [u8; 8] = [0x00, 0x7E, 0x42, 0x42, 0x42, 0x42, 0x7E, 0x00];

pub struct Canvas {
    pub width: usize,
    pub height: usize,
    /// Red, green, blue, and alpha for each pixel, row by row.
    pub pixels: Vec<u8>,
}

fn unpack(color: i64) -> [u8; 4] {
    [
        (color >> 24) as u8,
        (color >> 16) as u8,
        (color >> 8) as u8,
        color as u8,
    ]
}

/// A range of whole pixels, `start..end`, clipped to `0..limit`.
fn clip(start: i64, end: i64, limit: usize) -> std::ops::Range<usize> {
    let limit = limit as i64;
    let start = start.clamp(0, limit);
    let end = end.clamp(start, limit);
    start as usize..end as usize
}

impl Canvas {
    /// A canvas of transparent pixels.
    pub fn new(width: i64, height: i64) -> Result<Canvas, HostError> {
        if !(1..=LONGEST_SIDE).contains(&width)
            || !(1..=LONGEST_SIDE).contains(&height)
            || width * height > MOST_PIXELS
        {
            return Err((
                "ValueError",
                format!(
                    "a canvas of {width} by {height} pixels is not possible; each side is 1 to \
                     {LONGEST_SIDE} pixels, and {MOST_PIXELS} pixels at most in all"
                ),
            ));
        }
        Ok(Canvas {
            width: width as usize,
            height: height as usize,
            pixels: vec![0; (width * height * 4) as usize],
        })
    }

    /// A canvas of the given pixels, four bytes each, row by row.
    pub fn from_bytes(width: i64, height: i64, bytes: &[u8]) -> Result<Canvas, HostError> {
        let mut canvas = Canvas::new(width, height)?;
        if bytes.len() != canvas.pixels.len() {
            return Err((
                "ValueError",
                format!(
                    "an image of {width} by {height} pixels takes {} bytes, not {}",
                    canvas.pixels.len(),
                    bytes.len()
                ),
            ));
        }
        canvas.pixels.copy_from_slice(bytes);
        Ok(canvas)
    }

    /// The color of a pixel, packed; transparent outside the canvas.
    pub fn pixel(&self, x: i64, y: i64) -> i64 {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            return 0;
        }
        let at = (y as usize * self.width + x as usize) * 4;
        let p = &self.pixels[at..at + 4];
        ((p[0] as i64) << 24) | ((p[1] as i64) << 16) | ((p[2] as i64) << 8) | p[3] as i64
    }

    /// Blends `color` over the pixel at `x`, `y`, which is on the canvas.
    fn blend(&mut self, x: usize, y: usize, color: [u8; 4]) {
        let at = (y * self.width + x) * 4;
        let alpha = color[3] as u32;
        if alpha == 255 {
            self.pixels[at..at + 4].copy_from_slice(&color);
            return;
        }
        if alpha == 0 {
            return;
        }
        let pixel = &mut self.pixels[at..at + 4];
        // What shows of the pixel below, and how opaque the result is.
        let below = (pixel[3] as u32 * (255 - alpha) + 127) / 255;
        let total = alpha + below;
        for channel in 0..3 {
            let mixed = color[channel] as u32 * alpha + pixel[channel] as u32 * below;
            pixel[channel] = ((mixed + total / 2) / total) as u8;
        }
        pixel[3] = total as u8;
    }

    /// Makes every pixel `color`, without blending.
    pub fn clear(&mut self, color: i64) {
        let color = unpack(color);
        for pixel in self.pixels.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&color);
        }
    }

    pub fn fill_rectangle(&mut self, x: i64, y: i64, width: i64, height: i64, color: i64) {
        let color = unpack(color);
        for row in clip(y, y.saturating_add(height), self.height) {
            for column in clip(x, x.saturating_add(width), self.width) {
                self.blend(column, row, color);
            }
        }
    }

    /// Covers the pixels whose centers lie within `radius` of the center of
    /// the pixel `x`, `y`.
    pub fn fill_circle(&mut self, x: i64, y: i64, radius: i64, color: i64) {
        if radius <= 0 {
            return;
        }
        let color = unpack(color);
        let limit = 4 * (radius as i128) * (radius as i128);
        let rows = clip(
            y.saturating_sub(radius),
            y.saturating_add(radius + 1),
            self.height,
        );
        let columns = clip(
            x.saturating_sub(radius),
            x.saturating_add(radius + 1),
            self.width,
        );
        for row in rows {
            let dy = 2 * (row as i128 - y as i128);
            for column in columns.clone() {
                let dx = 2 * (column as i128 - x as i128);
                if dx * dx + dy * dy <= limit {
                    self.blend(column, row, color);
                }
            }
        }
    }

    /// Covers the pixels whose centers lie inside the polygon of `points`,
    /// by the even-odd rule.
    pub fn fill_polygon(&mut self, points: &[(f64, f64)], color: i64) {
        if points.len() < 3 {
            return;
        }
        let color = unpack(color);
        let top = points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let bottom = points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        let rows = clip(top.floor() as i64, bottom.ceil() as i64 + 1, self.height);
        let mut crossings: Vec<f64> = Vec::new();
        for row in rows {
            let center = row as f64 + 0.5;
            crossings.clear();
            for (index, &(x0, y0)) in points.iter().enumerate() {
                let (x1, y1) = points[(index + 1) % points.len()];
                if (y0 <= center && center < y1) || (y1 <= center && center < y0) {
                    crossings.push(x0 + (center - y0) * (x1 - x0) / (y1 - y0));
                }
            }
            crossings.sort_by(f64::total_cmp);
            for pair in crossings.as_chunks::<2>().0 {
                // The pixels whose centers lie from the one crossing to the
                // other.
                let first = (pair[0] - 0.5).ceil() as i64;
                let last = (pair[1] - 0.5).ceil() as i64;
                for column in clip(first, last, self.width) {
                    self.blend(column, row, color);
                }
            }
        }
    }

    /// A line from the center of one pixel to the center of another,
    /// `thickness` pixels wide: one pixel of each row or column it crosses
    /// when it is 1, else the band around it.
    pub fn line(&mut self, from: (i64, i64), to: (i64, i64), thickness: i64, color: i64) {
        if thickness <= 1 {
            let packed = unpack(color);
            let (mut x, mut y) = from;
            let (dx, dy) = ((to.0 - x).abs(), -(to.1 - y).abs());
            let (step_x, step_y) = ((to.0 - x).signum(), (to.1 - y).signum());
            let mut error = dx + dy;
            loop {
                if x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height {
                    self.blend(x as usize, y as usize, packed);
                }
                if (x, y) == to {
                    return;
                }
                let doubled = 2 * error;
                if doubled >= dy {
                    error += dy;
                    x += step_x;
                }
                if doubled <= dx {
                    error += dx;
                    y += step_y;
                }
            }
        }
        let start = (from.0 as f64 + 0.5, from.1 as f64 + 0.5);
        let end = (to.0 as f64 + 0.5, to.1 as f64 + 0.5);
        let (dx, dy) = (end.0 - start.0, end.1 - start.1);
        let length = (dx * dx + dy * dy).sqrt();
        let half = thickness as f64 / 2.0;
        // A point is a square as wide as the line.
        let (nx, ny, ax, ay) = if length == 0.0 {
            (half, 0.0, 0.0, half)
        } else {
            (-dy / length * half, dx / length * half, 0.0, 0.0)
        };
        let band = [
            (start.0 + nx - ax, start.1 + ny - ay),
            (end.0 + nx + ax, end.1 + ny + ay),
            (end.0 - nx + ax, end.1 - ny + ay),
            (start.0 - nx - ax, start.1 - ny - ay),
        ];
        self.fill_polygon(&band, color);
    }

    /// Draws `source` with its top left corner at `x`, `y`, blending it
    /// over what is there.
    pub fn draw(&mut self, source: &Canvas, x: i64, y: i64) {
        for row in clip(y, y.saturating_add(source.height as i64), self.height) {
            let from_row = (row as i64 - y) as usize;
            for column in clip(x, x.saturating_add(source.width as i64), self.width) {
                let from = (from_row * source.width + (column as i64 - x) as usize) * 4;
                let color = [
                    source.pixels[from],
                    source.pixels[from + 1],
                    source.pixels[from + 2],
                    source.pixels[from + 3],
                ];
                self.blend(column, row, color);
            }
        }
    }

    /// Draws text in the built-in font, each character in a square cell of
    /// `size` pixels, its top left corner at `x`, `y`; a line break starts
    /// a line below.
    pub fn text(&mut self, text: &str, x: i64, y: i64, size: i64, color: i64) {
        if size <= 0 {
            return;
        }
        let packed = unpack(color);
        let (mut left, mut top) = (x, y);
        for character in text.chars() {
            if character == '\n' {
                left = x;
                top = top.saturating_add(size);
                continue;
            }
            let rows = glyph(character);
            for row in clip(top, top.saturating_add(size), self.height) {
                let bits = rows[((row as i64 - top) * 8 / size) as usize];
                if bits == 0 {
                    continue;
                }
                for column in clip(left, left.saturating_add(size), self.width) {
                    let bit = (column as i64 - left) * 8 / size;
                    if bits >> bit & 1 == 1 {
                        self.blend(column, row, packed);
                    }
                }
            }
            left = left.saturating_add(size);
        }
    }
}

/// The rows of a character of the built-in font: nothing for a space, and
/// a box for one it lacks.
fn glyph(character: char) -> [u8; 8] {
    if character.is_whitespace() {
        return [0; 8];
    }
    match GLYPHS.binary_search_by_key(&(character as u32), |(code, _)| *code) {
        Ok(index) => GLYPHS[index].1,
        Err(_) => MISSING,
    }
}

/// How wide and high text is in the built-in font at `size`: its longest
/// line's characters, and its lines.
pub fn text_size(text: &str, size: i64) -> (i64, i64) {
    let size = size.max(0);
    let lines: Vec<i64> = text
        .split('\n')
        .map(|line| line.chars().count() as i64)
        .collect();
    let longest = lines.iter().copied().max().unwrap_or(0);
    (
        longest.saturating_mul(size),
        (lines.len() as i64).saturating_mul(size),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: i64 = 0xFF00_00FF;

    fn covered(canvas: &Canvas) -> usize {
        canvas
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] != 0)
            .count()
    }

    #[test]
    fn a_square_polygon_covers_what_its_rectangle_does() {
        let mut a = Canvas::new(20, 20).unwrap();
        let mut b = Canvas::new(20, 20).unwrap();
        a.fill_rectangle(3, 4, 10, 10, RED);
        b.fill_polygon(&[(3.0, 4.0), (13.0, 4.0), (13.0, 14.0), (3.0, 14.0)], RED);
        assert_eq!(a.pixels, b.pixels);
        assert_eq!(covered(&a), 100);
    }

    #[test]
    fn shapes_are_clipped_to_the_canvas() {
        let mut canvas = Canvas::new(8, 8).unwrap();
        canvas.fill_rectangle(-5, -5, 100, 100, RED);
        canvas.fill_circle(i64::MAX, i64::MIN, 10, RED);
        canvas.line((-100, 3), (100, 3), 1, RED);
        assert_eq!(covered(&canvas), 64);
    }

    #[test]
    fn half_transparent_colors_blend() {
        let mut canvas = Canvas::new(1, 1).unwrap();
        canvas.clear(0x0000_FFFF);
        canvas.fill_rectangle(0, 0, 1, 1, 0xFF00_0080);
        assert_eq!(canvas.pixel(0, 0), 0x8000_7FFF);
    }

    #[test]
    fn a_line_has_a_pixel_for_each_step() {
        let mut canvas = Canvas::new(10, 10).unwrap();
        canvas.line((0, 0), (9, 3), 1, RED);
        assert_eq!(covered(&canvas), 10);
        assert_eq!(canvas.pixel(9, 3), RED);
    }

    #[test]
    fn text_is_drawn_a_cell_per_character() {
        let mut canvas = Canvas::new(32, 16).unwrap();
        canvas.text("A b", 0, 0, 8, RED);
        // The top of A's left side, in its own cell, and nothing in the space.
        assert_eq!(canvas.pixel(2, 1), RED);
        assert!((8..16).all(|x| (0..8).all(|y| canvas.pixel(x, y) == 0)));
        assert_eq!(text_size("A b\nlonger", 8), (48, 16));
    }

    #[test]
    fn impossible_canvases_fail() {
        assert!(Canvas::new(0, 10).is_err());
        assert!(Canvas::new(16385, 1).is_err());
        assert!(Canvas::from_bytes(2, 2, &[0; 15]).is_err());
    }
}
