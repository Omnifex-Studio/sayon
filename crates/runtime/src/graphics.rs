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
//!
//! Smooth shapes, and text in a font (see `font`), cover each pixel as
//! much as they cover its area instead, which blends the color that much:
//! a pixel's area is sampled on 16 rows, and across each row the coverage
//! is exact.

use crate::font_table::GLYPHS;
use crate::host::HostError;

/// The longest side of a canvas, and the most pixels one holds.
const LONGEST_SIDE: i64 = 16384;
pub const MOST_PIXELS: i64 = 1 << 26;

/// What a character the font lacks looks like: a box.
const MISSING: [u8; 8] = [0x00, 0x7E, 0x42, 0x42, 0x42, 0x42, 0x7E, 0x00];

/// The rows each row of pixels is sampled on for smooth shapes.
const SAMPLES: usize = 16;

/// An edge of an outline, from its top to its bottom, and whether it runs
/// down (1) or up (-1) the way the outline goes.
#[derive(Clone, Copy, Debug)]
struct Edge {
    top: f64,
    bottom: f64,
    /// Where it is at its top, and how far it goes across for each pixel
    /// down.
    x: f64,
    slope: f64,
    winding: i32,
}

/// The edges of closed outlines, such as a polygon's or a line of text's,
/// to fill smooth.
#[derive(Default)]
pub struct Outline {
    edges: Vec<Edge>,
}

impl Outline {
    /// Adds the edge from one point to another; a level one adds nothing.
    pub fn edge(&mut self, from: (f64, f64), to: (f64, f64)) {
        if from.1 == to.1 {
            return;
        }
        let (upper, lower, winding) = if from.1 < to.1 {
            (from, to, 1)
        } else {
            (to, from, -1)
        };
        self.edges.push(Edge {
            top: upper.1,
            bottom: lower.1,
            x: upper.0,
            slope: (lower.0 - upper.0) / (lower.1 - upper.1),
            winding,
        });
    }

    /// Adds a polygon's edges, from each point to the next and from the
    /// last to the first.
    pub fn polygon(&mut self, points: &[(f64, f64)]) {
        for (index, &point) in points.iter().enumerate() {
            self.edge(point, points[(index + 1) % points.len()]);
        }
    }
}

/// How much a smooth shape covers of each pixel of a row, from the spans
/// it covers on each of the row's samples.
struct Coverage {
    /// The first column.
    start: usize,
    /// The parts of pixels the spans cover, and where whole pixels start
    /// (1) and stop (-1) being covered, for each column, with room for one
    /// past the last.
    parts: Vec<f64>,
    whole: Vec<i32>,
}

impl Coverage {
    fn new(columns: std::ops::Range<usize>) -> Coverage {
        Coverage {
            start: columns.start,
            parts: vec![0.0; columns.len() + 1],
            whole: vec![0; columns.len() + 2],
        }
    }

    /// Adds the span from `from` to `to` across a sample.
    fn span(&mut self, from: f64, to: f64) {
        let end = (self.parts.len() - 1) as f64;
        let from = (from - self.start as f64).clamp(0.0, end);
        let to = (to - self.start as f64).clamp(0.0, end);
        if to <= from {
            return;
        }
        let (first, last) = (from as usize, to as usize);
        if first == last {
            self.parts[first] += to - from;
            return;
        }
        self.parts[first] += (first + 1) as f64 - from;
        self.whole[first + 1] += 1;
        self.whole[last] -= 1;
        self.parts[last] += to - last as f64;
    }
}

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

    /// Blends `color` over the pixels of `row` as much as `coverage` says
    /// they are covered, and empties it for the next row.
    fn blend_coverage(&mut self, row: usize, coverage: &mut Coverage, color: [u8; 4]) {
        let mut whole = 0;
        for index in 0..coverage.parts.len() - 1 {
            whole += coverage.whole[index];
            let covered = ((coverage.parts[index] + whole as f64) / SAMPLES as f64).min(1.0);
            if covered > 0.0 {
                let alpha = (color[3] as f64 * covered).round() as u8;
                self.blend(
                    coverage.start + index,
                    row,
                    [color[0], color[1], color[2], alpha],
                );
            }
        }
        coverage.parts.fill(0.0);
        coverage.whole.fill(0);
    }

    /// Fills a smooth shape within `top` to `bottom` and `left` to `right`,
    /// whose spans `spans` adds for the sample at each height.
    fn fill_smooth(
        &mut self,
        (top, bottom): (f64, f64),
        (left, right): (f64, f64),
        color: i64,
        mut spans: impl FnMut(f64, &mut Coverage),
    ) {
        let rows = clip(top.floor() as i64, bottom.ceil() as i64, self.height);
        let columns = clip(left.floor() as i64, right.ceil() as i64, self.width);
        if rows.is_empty() || columns.is_empty() {
            return;
        }
        let color = unpack(color);
        let mut coverage = Coverage::new(columns);
        for row in rows {
            for sample in 0..SAMPLES {
                let height = row as f64 + (sample as f64 + 0.5) / SAMPLES as f64;
                spans(height, &mut coverage);
            }
            self.blend_coverage(row, &mut coverage, color);
        }
    }

    /// Fills outlines smooth, by the nonzero rule, where they go around a
    /// pixel more times one way than the other, or else by the even-odd
    /// rule.
    pub fn fill_outline(&mut self, outline: &mut Outline, nonzero: bool, color: i64) {
        let edges = &mut outline.edges;
        if edges.is_empty() {
            return;
        }
        edges.sort_by(|a, b| a.top.total_cmp(&b.top));
        let top = edges[0].top;
        let bottom = edges
            .iter()
            .map(|e| e.bottom)
            .fold(f64::NEG_INFINITY, f64::max);
        let ends = |e: &Edge| [e.x, e.x + (e.bottom - e.top) * e.slope];
        let left = edges.iter().flat_map(ends).fold(f64::INFINITY, f64::min);
        let right = edges
            .iter()
            .flat_map(ends)
            .fold(f64::NEG_INFINITY, f64::max);
        let edges = &*edges;
        // The edges that may cross the samples still to come, and where
        // those that cross one do, in order across.
        let (mut next, mut active, mut crossings) = (0, Vec::new(), Vec::new());
        self.fill_smooth((top, bottom), (left, right), color, |height, coverage| {
            while next < edges.len() && edges[next].top <= height {
                active.push(next);
                next += 1;
            }
            active.retain(|&index| edges[index].bottom > height);
            crossings.clear();
            for &index in &active {
                let edge = edges[index];
                crossings.push((edge.x + (height - edge.top) * edge.slope, edge.winding));
            }
            crossings.sort_by(|a: &(f64, i32), b| a.0.total_cmp(&b.0));
            let inside = |winding: i32| {
                if nonzero {
                    winding != 0
                } else {
                    winding % 2 != 0
                }
            };
            let (mut winding, mut from) = (0, 0.0);
            for &(x, turn) in &crossings {
                let was = inside(winding);
                winding += turn;
                match (was, inside(winding)) {
                    (false, true) => from = x,
                    (true, false) => coverage.span(from, x),
                    _ => {}
                }
            }
        });
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

    /// A disc of `radius` around the center of the pixel `x`, `y`, smooth.
    pub fn fill_circle_smooth(&mut self, x: i64, y: i64, radius: i64, color: i64) {
        if radius <= 0 {
            return;
        }
        let (center_x, center_y, radius) = (x as f64 + 0.5, y as f64 + 0.5, radius as f64);
        self.fill_smooth(
            (center_y - radius, center_y + radius),
            (center_x - radius, center_x + radius),
            color,
            |height, coverage| {
                let across = radius * radius - (height - center_y) * (height - center_y);
                if across > 0.0 {
                    let half = across.sqrt();
                    coverage.span(center_x - half, center_x + half);
                }
            },
        );
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

    /// The polygon of `points`, smooth, by the even-odd rule.
    pub fn fill_polygon_smooth(&mut self, points: &[(f64, f64)], color: i64) {
        let mut outline = Outline::default();
        outline.polygon(points);
        self.fill_outline(&mut outline, false, color);
    }

    /// A line from the center of one pixel to the center of another,
    /// `thickness` pixels wide: one pixel of each row or column it crosses
    /// when it is 1, else the band around it. A smooth line is the band,
    /// at least a pixel wide.
    pub fn line(
        &mut self,
        from: (i64, i64),
        to: (i64, i64),
        thickness: i64,
        color: i64,
        smooth: bool,
    ) {
        if smooth {
            let band = band(from, to, thickness.max(1));
            self.fill_polygon_smooth(&band, color);
            return;
        }
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
        self.fill_polygon(&band(from, to, thickness), color);
    }

    /// Blends `color` over the pixels of a mask `width` pixels wide, its top
    /// left corner at `x`, `y`, as much as the mask's byte for each says.
    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    pub fn blend_mask(&mut self, mask: &[u8], width: usize, (x, y): (i64, i64), color: i64) {
        let color = unpack(color);
        let height = (mask.len() / width) as i64;
        for row in clip(y, y.saturating_add(height), self.height) {
            let from_row = (row as i64 - y) as usize;
            for column in clip(x, x.saturating_add(width as i64), self.width) {
                let covered = mask[from_row * width + (column as i64 - x) as usize] as u32;
                let alpha = (color[3] as u32 * covered + 127) / 255;
                self.blend(column, row, [color[0], color[1], color[2], alpha as u8]);
            }
        }
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

/// The band `thickness` wide around the line from the center of one pixel
/// to the center of another, cut square at the centers; for a point, a
/// square as wide.
fn band(from: (i64, i64), to: (i64, i64), thickness: i64) -> [(f64, f64); 4] {
    let start = (from.0 as f64 + 0.5, from.1 as f64 + 0.5);
    let end = (to.0 as f64 + 0.5, to.1 as f64 + 0.5);
    let (dx, dy) = (end.0 - start.0, end.1 - start.1);
    let length = (dx * dx + dy * dy).sqrt();
    let half = thickness as f64 / 2.0;
    let (nx, ny, ax, ay) = if length == 0.0 {
        (half, 0.0, 0.0, half)
    } else {
        (-dy / length * half, dx / length * half, 0.0, 0.0)
    };
    [
        (start.0 + nx - ax, start.1 + ny - ay),
        (end.0 + nx + ax, end.1 + ny + ay),
        (end.0 - nx + ax, end.1 - ny + ay),
        (start.0 - nx - ax, start.1 - ny - ay),
    ]
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
        canvas.line((-100, 3), (100, 3), 1, RED, false);
        canvas.fill_circle_smooth(i64::MAX, i64::MIN, 10, RED);
        canvas.fill_polygon_smooth(&[(-1e300, 0.0), (1e300, 0.0), (0.0, 1e300)], RED);
        assert_eq!(covered(&canvas), 64);
    }

    #[test]
    fn smooth_shapes_cover_pixels_by_their_area() {
        let mut canvas = Canvas::new(8, 8).unwrap();
        // A square half a pixel off the grid: whole pixels inside, halves
        // along its sides, and quarters at its corners.
        canvas.fill_polygon_smooth(&[(1.5, 1.5), (5.5, 1.5), (5.5, 5.5), (1.5, 5.5)], RED);
        assert_eq!(canvas.pixel(3, 3), RED);
        assert_eq!(canvas.pixel(1, 3), 0xFF00_0080);
        assert_eq!(canvas.pixel(5, 1), 0xFF00_0040);
        assert_eq!(canvas.pixel(0, 0), 0);
        // On the grid, a smooth polygon covers what the other does.
        let mut a = Canvas::new(20, 20).unwrap();
        let mut b = Canvas::new(20, 20).unwrap();
        a.fill_rectangle(3, 4, 10, 10, RED);
        b.fill_polygon_smooth(&[(3.0, 4.0), (13.0, 4.0), (13.0, 14.0), (3.0, 14.0)], RED);
        assert_eq!(a.pixels, b.pixels);
    }

    #[test]
    fn a_smooth_disc_covers_its_area() {
        let mut canvas = Canvas::new(40, 40).unwrap();
        canvas.fill_circle_smooth(20, 20, 10, RED);
        let area: f64 = canvas
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[3] as f64 / 255.0)
            .sum();
        assert!((area - std::f64::consts::PI * 100.0).abs() < 1.0, "{area}");
        assert_eq!(canvas.pixel(20, 20), RED);
        assert_eq!(canvas.pixel(20, 9), 0);
    }

    #[test]
    fn the_nonzero_rule_fills_what_goes_around_twice() {
        // Two squares the same way, one inside the other.
        let squares = || {
            let mut outline = Outline::default();
            outline.polygon(&[(0.0, 0.0), (6.0, 0.0), (6.0, 6.0), (0.0, 6.0)]);
            outline.polygon(&[(2.0, 2.0), (4.0, 2.0), (4.0, 4.0), (2.0, 4.0)]);
            outline
        };
        let mut nonzero = Canvas::new(6, 6).unwrap();
        nonzero.fill_outline(&mut squares(), true, RED);
        assert_eq!(nonzero.pixel(3, 3), RED);
        let mut even_odd = Canvas::new(6, 6).unwrap();
        even_odd.fill_outline(&mut squares(), false, RED);
        assert_eq!(even_odd.pixel(3, 3), 0);
        assert_eq!(even_odd.pixel(1, 1), RED);
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
        canvas.line((0, 0), (9, 3), 1, RED, false);
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
