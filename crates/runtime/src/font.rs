//! Fonts for `standard/graphics`. A TrueType font is read from its bytes,
//! and the runtime draws its glyphs' outlines smooth on a canvas (see
//! `graphics`), so that text in it is the same on every system. The
//! system's own fonts are found in its font directories by the names
//! inside them; in a web page they are the browser's, which the browser
//! draws, through the glue's host functions.
//!
//! A font's tables, of the OpenType specification: `head` for its units
//! per em and the size of `loca`'s offsets, `maxp` for its number of
//! glyphs, `cmap` for each character's glyph (its formats 4 and 12),
//! `hhea` and `hmtx` for its ascender, descender, line gap, and advances,
//! `loca` and `glyf` for the outlines, and `name` for its names. Of a
//! collection (`.ttc`), the first font is read. Kerning, hinting, and the
//! glyphs of PostScript outlines (CFF, in most `.otf` files) are not read
//! yet.

use crate::graphics::{Canvas, Outline};
use crate::host::HostError;

fn fail(message: impl Into<String>) -> HostError {
    ("FontError", message.into())
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    data.get(at..at.checked_add(2)?)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn i16_at(data: &[u8], at: usize) -> Option<i16> {
    u16_at(data, at).map(|value| value as i16)
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at.checked_add(4)?)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// A number of the 2.14 format: two bits whole, fourteen a fraction.
fn f2dot14(data: &[u8], at: usize) -> Option<f64> {
    i16_at(data, at).map(|value| value as f64 / 16384.0)
}

/// A font, and what it is called.
pub struct Font {
    pub name: String,
    face: Face,
}

enum Face {
    TrueType(Box<TrueType>),
    /// One of the browser's, by its number in the page's glue.
    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    Browser(i32),
}

impl Font {
    /// Reads a TrueType font, or the first of a collection.
    pub fn from_bytes(data: Vec<u8>) -> Result<Font, HostError> {
        let font = TrueType::read(data)?;
        let names = font.names();
        Ok(Font {
            name: names.full(),
            face: Face::TrueType(Box::new(font)),
        })
    }

    /// The system's font named `name`, by its family's name, such as
    /// "Arial", or its full name, such as "Arial Bold"; `sans-serif`,
    /// `serif`, `monospace`, or nothing for the system's usual one of each.
    pub fn system(name: &str) -> Result<Font, HostError> {
        #[cfg(all(target_family = "wasm", target_os = "unknown"))]
        {
            web::open(name)
        }
        #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
        {
            system::find(name)
        }
    }

    /// How far above the baseline the font reaches, how far below it, and
    /// the gap between one line's bottom and the next's top, in pixels at
    /// `size` pixels per em.
    fn metrics(&self, size: f64) -> (f64, f64, f64) {
        match &self.face {
            Face::TrueType(font) => {
                let scale = size / font.units;
                (
                    font.ascender * scale,
                    font.descender * scale,
                    font.gap * scale,
                )
            }
            #[cfg(all(target_family = "wasm", target_os = "unknown"))]
            Face::Browser(number) => web::metrics(*number, size),
        }
    }

    /// How far a line of text advances, in pixels.
    fn advance(&self, line: &str, size: f64) -> f64 {
        match &self.face {
            Face::TrueType(font) => {
                let scale = size / font.units;
                line.chars()
                    .map(|character| font.advance(font.glyph(character)) * scale)
                    .sum()
            }
            #[cfg(all(target_family = "wasm", target_os = "unknown"))]
            Face::Browser(number) => web::advance(*number, line, size),
        }
    }

    /// The font's ascent (0), descent (1), or the height of a line (2), in
    /// whole pixels at `size`.
    pub fn metric(&self, which: i64, size: i64) -> i64 {
        let (ascent, descent, gap) = self.metrics(size.max(0) as f64);
        match which {
            0 => ascent.round() as i64,
            1 => descent.round() as i64,
            _ => (ascent + descent + gap).round() as i64,
        }
    }

    /// How wide text is at `size`, its longest line, in whole pixels.
    pub fn width(&self, text: &str, size: i64) -> i64 {
        let size = size.max(0) as f64;
        text.split('\n')
            .map(|line| self.advance(line, size))
            .fold(0.0, f64::max)
            .ceil() as i64
    }

    /// How high text is at `size`, from its first line's top to its last
    /// line's bottom, in whole pixels.
    pub fn height(&self, text: &str, size: i64) -> i64 {
        let (ascent, descent, gap) = self.metrics(size.max(0) as f64);
        let lines = text.split('\n').count() as f64;
        ((lines - 1.0) * (ascent + descent + gap) + ascent + descent).ceil() as i64
    }

    /// Draws text, its first line's top left corner at `x`, `y`, each line
    /// below the last by the font's line height.
    pub fn draw(&self, canvas: &mut Canvas, text: &str, (x, y): (i64, i64), size: i64, color: i64) {
        if size <= 0 {
            return;
        }
        let size = size as f64;
        let (ascent, descent, gap) = self.metrics(size);
        for (index, line) in text.split('\n').enumerate() {
            let baseline = y as f64 + ascent + index as f64 * (ascent + descent + gap);
            match &self.face {
                Face::TrueType(font) => {
                    let scale = size / font.units;
                    let mut outline = Outline::default();
                    let mut pen = x as f64;
                    for character in line.chars() {
                        let glyph = font.glyph(character);
                        font.outline(glyph, (pen, baseline), scale, &mut outline);
                        pen += font.advance(glyph) * scale;
                    }
                    canvas.fill_outline(&mut outline, true, color);
                }
                #[cfg(all(target_family = "wasm", target_os = "unknown"))]
                Face::Browser(number) => web::draw(
                    *number,
                    canvas,
                    line,
                    (x, baseline.round() as i64),
                    size,
                    color,
                ),
            }
        }
    }
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl Drop for Font {
    fn drop(&mut self) {
        if let Face::Browser(number) = self.face {
            web::close(number);
        }
    }
}

/// What a font calls itself, from its `name` table.
#[derive(Clone, Debug, Default)]
struct Names {
    /// Its family, such as "Arial", by the name of its typographic family
    /// where it has one, and by its older name too.
    families: Vec<String>,
    /// Its style in the family, such as "Regular" or "Bold".
    style: String,
    /// Its family and style, such as "Arial Bold".
    full: String,
}

impl Names {
    fn full(&self) -> String {
        if !self.full.is_empty() {
            return self.full.clone();
        }
        let family = self.families.first().cloned().unwrap_or_default();
        format!("{family} {}", self.style).trim().to_string()
    }

    /// Reads them from the `name` table.
    fn read(table: &[u8]) -> Names {
        let count = u16_at(table, 2).unwrap_or(0) as usize;
        let strings = u16_at(table, 4).unwrap_or(0) as usize;
        // The best string of each name so far: lower is better.
        let mut found: [(u32, String); 5] = Default::default();
        for index in 0..count {
            let record = 6 + 12 * index;
            let (Some(platform), Some(language), Some(id), Some(length), Some(offset)) = (
                u16_at(table, record),
                u16_at(table, record + 4),
                u16_at(table, record + 6),
                u16_at(table, record + 8),
                u16_at(table, record + 10),
            ) else {
                break;
            };
            let slot = match id {
                1 => 0,
                2 => 1,
                4 => 2,
                16 => 3,
                17 => 4,
                _ => continue,
            };
            // Windows' English names first, then its others, then Unicode's
            // and the Macintosh's.
            let rank = match (platform, language) {
                (3, 0x409) => 1,
                (3, _) => 2,
                (0, _) => 3,
                (1, 0) => 4,
                _ => continue,
            };
            let start = strings + offset as usize;
            let Some(bytes) = table.get(start..start + length as usize) else {
                continue;
            };
            let text = if platform == 1 {
                bytes.iter().map(|&b| b as char).collect()
            } else {
                let units: Vec<u16> = bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_be_bytes(*pair))
                    .collect();
                String::from_utf16_lossy(&units)
            };
            if found[slot].0 == 0 || rank < found[slot].0 {
                found[slot] = (rank, text);
            }
        }
        let [family, style, full, typographic, typographic_style] = found.map(|(_, text)| text);
        let families = [typographic, family]
            .into_iter()
            .filter(|name| !name.is_empty())
            .collect();
        let style = if typographic_style.is_empty() {
            style
        } else {
            typographic_style
        };
        Names {
            families,
            style,
            full,
        }
    }
}

/// Where the tables of the first font in `data` are, by their tags.
fn directory(data: &[u8]) -> Result<Vec<([u8; 4], usize, usize)>, HostError> {
    let not_font = || fail("these bytes are not a TrueType font");
    let mut start = 0;
    if data.get(0..4) == Some(b"ttcf") {
        start = u32_at(data, 12).ok_or_else(not_font)? as usize;
    }
    match data.get(start..start + 4).ok_or_else(not_font)? {
        [0, 1, 0, 0] | b"true" => {}
        b"OTTO" => {
            return Err(fail(
                "the font's glyphs are PostScript outlines (CFF), which Sayon does not read yet",
            ));
        }
        _ => return Err(not_font()),
    }
    let count = u16_at(data, start + 4).ok_or_else(not_font)? as usize;
    let mut tables = Vec::with_capacity(count);
    for index in 0..count {
        let record = start + 12 + 16 * index;
        let tag = data.get(record..record + 4).ok_or_else(not_font)?;
        let offset = u32_at(data, record + 8).ok_or_else(not_font)? as usize;
        let length = u32_at(data, record + 12).ok_or_else(not_font)? as usize;
        if offset
            .checked_add(length)
            .is_none_or(|end| end > data.len())
        {
            return Err(fail("the font's tables run past its end"));
        }
        tables.push((tag.try_into().expect("four bytes"), offset, length));
    }
    Ok(tables)
}

/// A TrueType font's bytes, and where in them what it draws with is.
struct TrueType {
    data: Vec<u8>,
    /// The units of its outlines in an em.
    units: f64,
    /// In its units: how far above the baseline it reaches, how far below,
    /// and the gap between lines.
    ascender: f64,
    descender: f64,
    gap: f64,
    glyphs: u16,
    /// Whether `loca` holds 32-bit offsets, not 16-bit halves.
    long_offsets: bool,
    loca: usize,
    glyf: (usize, usize),
    hmtx: usize,
    /// The glyphs with an advance of their own; the rest take the last's.
    advances: u16,
    /// The subtable of `cmap` read, its format, and whether it is a
    /// symbol font's, whose characters are at 0xF000 and on.
    cmap: usize,
    format: u16,
    symbol: bool,
    name: (usize, usize),
}

impl TrueType {
    fn read(data: Vec<u8>) -> Result<TrueType, HostError> {
        let tables = directory(&data)?;
        let table = |tag: &[u8; 4]| {
            tables
                .iter()
                .find(|(found, _, _)| found == tag)
                .map(|&(_, offset, length)| (offset, length))
                .ok_or_else(|| {
                    fail(format!(
                        "the font has no {} table",
                        String::from_utf8_lossy(tag)
                    ))
                })
        };
        let damaged = || fail("the font is damaged");
        let (head, _) = table(b"head")?;
        let (maxp, _) = table(b"maxp")?;
        let (hhea, _) = table(b"hhea")?;
        let (hmtx, _) = table(b"hmtx")?;
        let (loca, _) = table(b"loca")?;
        let glyf = table(b"glyf")?;
        let (cmap, _) = table(b"cmap")?;
        let name = table(b"name").unwrap_or((0, 0));
        let units = u16_at(&data, head + 18).ok_or_else(damaged)?;
        if units == 0 {
            return Err(damaged());
        }
        let long_offsets = i16_at(&data, head + 50).ok_or_else(damaged)? != 0;
        let glyphs = u16_at(&data, maxp + 4).ok_or_else(damaged)?;
        let ascender = i16_at(&data, hhea + 4).ok_or_else(damaged)?;
        let descender = i16_at(&data, hhea + 6).ok_or_else(damaged)?;
        let gap = i16_at(&data, hhea + 8).ok_or_else(damaged)?;
        let advances = u16_at(&data, hhea + 34).ok_or_else(damaged)?;
        if advances == 0 || u16_at(&data, hmtx + 4 * (advances as usize - 1)).is_none() {
            return Err(damaged());
        }
        // The subtable that maps the most: Unicode's full range, then its
        // basic plane, then a symbol font's.
        let mut best: Option<(u32, usize, u16, bool)> = None;
        for index in 0..u16_at(&data, cmap + 2).ok_or_else(damaged)? as usize {
            let record = cmap + 4 + 8 * index;
            let (Some(platform), Some(encoding), Some(offset)) = (
                u16_at(&data, record),
                u16_at(&data, record + 2),
                u32_at(&data, record + 4),
            ) else {
                return Err(damaged());
            };
            let subtable = cmap + offset as usize;
            let Some(format) = u16_at(&data, subtable) else {
                continue;
            };
            let rank = match (platform, encoding, format) {
                (3, 10, 12) | (0, _, 12) => 1,
                (3, 1, 4) | (0, _, 4) => 2,
                (3, 0, 4) => 3,
                _ => continue,
            };
            if best.is_none_or(|(least, ..)| rank < least) {
                best = Some((rank, subtable, format, rank == 3));
            }
        }
        let (_, cmap, format, symbol) =
            best.ok_or_else(|| fail("the font maps no Unicode characters to its glyphs"))?;
        Ok(TrueType {
            data,
            units: units as f64,
            ascender: ascender as f64,
            descender: -(descender as f64),
            gap: gap as f64,
            glyphs,
            long_offsets,
            loca,
            glyf,
            hmtx,
            advances,
            cmap,
            format,
            symbol,
            name,
        })
    }

    fn names(&self) -> Names {
        let (offset, length) = self.name;
        Names::read(&self.data[offset..offset + length])
    }

    /// The glyph of a character, or 0, the glyph of the ones it lacks.
    fn glyph(&self, character: char) -> u16 {
        let code = character as u32;
        match self.lookup(code) {
            0 if self.symbol && code < 0x100 => self.lookup(code + 0xF000),
            glyph => glyph,
        }
    }

    fn lookup(&self, code: u32) -> u16 {
        let found = if self.format == 4 {
            self.lookup_segments(code)
        } else {
            self.lookup_groups(code)
        };
        found.filter(|&glyph| glyph < self.glyphs).unwrap_or(0)
    }

    /// Format 4: segments of the basic plane, each mapped by a delta, or
    /// through an array of glyphs.
    fn lookup_segments(&self, code: u32) -> Option<u16> {
        let (data, at) = (&self.data[..], self.cmap);
        if code > 0xFFFF {
            return None;
        }
        let code = code as u16;
        let doubled = u16_at(data, at + 6)? as usize;
        let ends = at + 14;
        let starts = ends + doubled + 2;
        let deltas = starts + doubled;
        let ranges = deltas + doubled;
        // The first segment that ends at the character or after.
        let (mut low, mut high) = (0, doubled / 2);
        while low < high {
            let middle = (low + high) / 2;
            if u16_at(data, ends + 2 * middle)? < code {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        if low == doubled / 2 || u16_at(data, starts + 2 * low)? > code {
            return None;
        }
        let delta = u16_at(data, deltas + 2 * low)?;
        let range = u16_at(data, ranges + 2 * low)? as usize;
        if range == 0 {
            return Some(code.wrapping_add(delta));
        }
        let start = u16_at(data, starts + 2 * low)?;
        let glyph = u16_at(data, ranges + 2 * low + range + 2 * (code - start) as usize)?;
        (glyph != 0).then(|| glyph.wrapping_add(delta))
    }

    /// Format 12: groups of characters, each to consecutive glyphs.
    fn lookup_groups(&self, code: u32) -> Option<u16> {
        let (data, at) = (&self.data[..], self.cmap);
        let (mut low, mut high) = (0, u32_at(data, at + 12)? as usize);
        while low < high {
            let middle = (low + high) / 2;
            let group = at + 16 + 12 * middle;
            let (start, end) = (u32_at(data, group)?, u32_at(data, group + 4)?);
            if end < code {
                low = middle + 1;
            } else if start > code {
                high = middle;
            } else {
                let glyph = u32_at(data, group + 8)? + (code - start);
                return u16::try_from(glyph).ok();
            }
        }
        None
    }

    /// How far a glyph advances the pen, in the font's units.
    fn advance(&self, glyph: u16) -> f64 {
        let index = glyph.min(self.advances - 1) as usize;
        u16_at(&self.data, self.hmtx + 4 * index).unwrap_or(0) as f64
    }

    /// Where a glyph's outline is in `glyf`.
    fn place(&self, glyph: u16) -> Option<(usize, usize)> {
        let glyph = glyph as usize;
        let (start, end) = if self.long_offsets {
            (
                u32_at(&self.data, self.loca + 4 * glyph)? as usize,
                u32_at(&self.data, self.loca + 4 * glyph + 4)? as usize,
            )
        } else {
            (
                u16_at(&self.data, self.loca + 2 * glyph)? as usize * 2,
                u16_at(&self.data, self.loca + 2 * glyph + 2)? as usize * 2,
            )
        };
        (start <= end && end <= self.glyf.1).then_some((self.glyf.0 + start, self.glyf.0 + end))
    }

    /// Adds a glyph's contours, each its points in the font's units and
    /// whether each is on the curve, or off it, the control point of a
    /// quadratic curve. A glyph made of others takes them each moved, or
    /// transformed, as it says, to eight levels deep.
    fn contours(&self, glyph: u16, depth: u32, out: &mut Vec<Vec<(f64, f64, bool)>>) -> Option<()> {
        if depth > 8 {
            return None;
        }
        let (start, end) = self.place(glyph)?;
        if start == end {
            return Some(());
        }
        let data = &self.data[start..end];
        let count = i16_at(data, 0)?;
        if count >= 0 {
            let count = count as usize;
            let mut ends = Vec::with_capacity(count);
            for index in 0..count {
                ends.push(u16_at(data, 10 + 2 * index)? as usize);
            }
            let points = ends.last().map_or(0, |&last| last + 1);
            let mut at = 12 + 2 * count + u16_at(data, 10 + 2 * count)? as usize;
            let mut flags = Vec::with_capacity(points);
            while flags.len() < points {
                let flag = *data.get(at)?;
                at += 1;
                flags.push(flag);
                if flag & 8 != 0 {
                    let repeat = *data.get(at)?;
                    at += 1;
                    flags.extend(std::iter::repeat_n(flag, repeat as usize));
                }
            }
            flags.truncate(points);
            // Each coordinate is a change from the last: a byte with a sign
            // in the flags, or the same, or a 16-bit change.
            let mut coordinates = |short: u8, same: u8| -> Option<Vec<f64>> {
                let mut value = 0i32;
                let mut values = Vec::with_capacity(points);
                for &flag in &flags {
                    if flag & short != 0 {
                        let change = *data.get(at)? as i32;
                        at += 1;
                        value += if flag & same != 0 { change } else { -change };
                    } else if flag & same == 0 {
                        value += i16_at(data, at)? as i32;
                        at += 2;
                    }
                    values.push(value as f64);
                }
                Some(values)
            };
            let xs = coordinates(2, 16)?;
            let ys = coordinates(4, 32)?;
            let mut first = 0;
            for end in ends {
                if end < first || end >= points {
                    return None;
                }
                out.push(
                    (first..=end)
                        .map(|index| (xs[index], ys[index], flags[index] & 1 != 0))
                        .collect(),
                );
                first = end + 1;
            }
            return Some(());
        }
        let mut at = 10;
        loop {
            let flags = u16_at(data, at)?;
            let component = u16_at(data, at + 2)?;
            at += 4;
            // Its offset; given as two points to match instead, which is
            // rare, it is not moved.
            let (dx, dy) = if flags & 1 != 0 {
                at += 4;
                (i16_at(data, at - 4)? as f64, i16_at(data, at - 2)? as f64)
            } else {
                at += 2;
                (
                    *data.get(at - 2)? as i8 as f64,
                    *data.get(at - 1)? as i8 as f64,
                )
            };
            let (dx, dy) = if flags & 2 != 0 { (dx, dy) } else { (0.0, 0.0) };
            let (mut a, mut b, mut c, mut d) = (1.0, 0.0, 0.0, 1.0);
            if flags & 8 != 0 {
                a = f2dot14(data, at)?;
                d = a;
                at += 2;
            } else if flags & 0x40 != 0 {
                a = f2dot14(data, at)?;
                d = f2dot14(data, at + 2)?;
                at += 4;
            } else if flags & 0x80 != 0 {
                a = f2dot14(data, at)?;
                b = f2dot14(data, at + 2)?;
                c = f2dot14(data, at + 4)?;
                d = f2dot14(data, at + 6)?;
                at += 8;
            }
            let mut parts = Vec::new();
            self.contours(component, depth + 1, &mut parts)?;
            for contour in parts {
                out.push(
                    contour
                        .into_iter()
                        .map(|(x, y, on)| (a * x + c * y + dx, b * x + d * y + dy, on))
                        .collect(),
                );
            }
            if flags & 0x20 == 0 {
                return Some(());
            }
        }
    }

    /// Adds a glyph's outline to `outline`, its origin at `origin` and its
    /// units `scale` pixels each, as lines close enough to its curves.
    fn outline(&self, glyph: u16, origin: (f64, f64), scale: f64, outline: &mut Outline) {
        let mut contours = Vec::new();
        if self.contours(glyph, 0, &mut contours).is_none() {
            return;
        }
        let place = |(x, y, on): (f64, f64, bool)| (origin.0 + x * scale, origin.1 - y * scale, on);
        let middle =
            |a: (f64, f64, bool), b: (f64, f64, bool)| ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0, true);
        for contour in contours {
            if contour.len() < 2 {
                continue;
            }
            let mut points: Vec<(f64, f64, bool)> = contour.into_iter().map(place).collect();
            // Start on the curve: at its first point there, or else between
            // the last point and the first.
            match points.iter().position(|point| point.2) {
                Some(first) => points.rotate_left(first),
                None => points.insert(0, middle(points[points.len() - 1], points[0])),
            }
            points.push(points[0]);
            let mut current = points[0];
            let mut control = None;
            for &point in &points[1..] {
                match (point.2, control) {
                    (true, None) => {
                        outline.edge((current.0, current.1), (point.0, point.1));
                        current = point;
                    }
                    (true, Some(off)) => {
                        curve(outline, current, off, point);
                        current = point;
                        control = None;
                    }
                    (false, None) => control = Some(point),
                    // Between two points off the curve is one on it.
                    (false, Some(off)) => {
                        let between = middle(off, point);
                        curve(outline, current, off, between);
                        current = between;
                        control = Some(point);
                    }
                }
            }
        }
    }
}

/// Adds a quadratic curve as lines, enough that none strays more than a
/// tenth of a pixel from it.
fn curve(
    outline: &mut Outline,
    from: (f64, f64, bool),
    control: (f64, f64, bool),
    to: (f64, f64, bool),
) {
    // A square root, not `hypot`, which each system's C library rounds its
    // own way.
    let (across, down) = (
        from.0 - 2.0 * control.0 + to.0,
        from.1 - 2.0 * control.1 + to.1,
    );
    let bend = (across * across + down * down).sqrt();
    let steps = ((bend / 0.4).sqrt().ceil() as usize).clamp(1, 64);
    let mut last = (from.0, from.1);
    for step in 1..=steps {
        let t = step as f64 / steps as f64;
        let (a, b, c) = ((1.0 - t) * (1.0 - t), 2.0 * (1.0 - t) * t, t * t);
        let point = (
            a * from.0 + b * control.0 + c * to.0,
            a * from.1 + b * control.1 + c * to.1,
        );
        outline.edge(last, point);
        last = point;
    }
}

/// Finding the system's fonts: the files of its font directories, read
/// once for their names.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod system {
    use super::{Font, Names, fail, u16_at, u32_at};
    use crate::host::HostError;
    use std::io::{Read, Seek, SeekFrom};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// The fonts found, and their names, once looked for.
    static FOUND: Mutex<Option<Vec<(PathBuf, Names)>>> = Mutex::new(None);

    /// The usual fonts of each kind, the first the system has.
    #[cfg(windows)]
    const USUAL: [&[&str]; 3] = [
        &["Segoe UI", "Arial", "Tahoma", "Verdana"],
        &["Times New Roman", "Georgia", "Cambria"],
        &["Consolas", "Courier New", "Lucida Console"],
    ];
    #[cfg(target_os = "android")]
    const USUAL: [&[&str]; 3] = [
        &["Roboto", "Noto Sans", "Droid Sans"],
        &["Noto Serif", "Droid Serif"],
        &["Droid Sans Mono", "Noto Sans Mono", "Cutive Mono"],
    ];
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    const USUAL: [&[&str]; 3] = [
        &["Helvetica Neue", "Helvetica", "Arial"],
        &["Times New Roman", "Times", "Georgia"],
        &["Menlo", "Courier New", "Courier"],
    ];
    #[cfg(not(any(windows, target_os = "android", target_os = "macos", target_os = "ios")))]
    const USUAL: [&[&str]; 3] = [
        &[
            "DejaVu Sans",
            "Noto Sans",
            "Liberation Sans",
            "Ubuntu",
            "Cantarell",
            "FreeSans",
        ],
        &[
            "DejaVu Serif",
            "Noto Serif",
            "Liberation Serif",
            "FreeSerif",
        ],
        &[
            "DejaVu Sans Mono",
            "Noto Sans Mono",
            "Liberation Mono",
            "Ubuntu Mono",
            "FreeMono",
        ],
    ];

    /// The directories the system keeps fonts in.
    fn directories() -> Vec<PathBuf> {
        let variable = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let mut found = Vec::new();
        if cfg!(windows) {
            let system = variable("WINDIR")
                .or_else(|| variable("SystemRoot"))
                .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
            found.push(system.join("Fonts"));
            if let Some(local) = variable("LOCALAPPDATA") {
                found.push(local.join(r"Microsoft\Windows\Fonts"));
            }
        } else if cfg!(target_os = "android") {
            found.extend(["/system/fonts", "/product/fonts"].map(PathBuf::from));
        } else if cfg!(any(target_os = "macos", target_os = "ios")) {
            found.extend(["/System/Library/Fonts", "/Library/Fonts"].map(PathBuf::from));
            if let Some(home) = variable("HOME") {
                found.push(home.join("Library/Fonts"));
            }
        } else {
            let home = variable("HOME");
            match variable("XDG_DATA_HOME") {
                Some(data) => found.push(data.join("fonts")),
                None => found.extend(home.iter().map(|home| home.join(".local/share/fonts"))),
            }
            found.extend(home.iter().map(|home| home.join(".fonts")));
            let shared = std::env::var("XDG_DATA_DIRS")
                .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
            found.extend(
                shared
                    .split(':')
                    .filter(|dir| !dir.is_empty())
                    .map(|dir| Path::new(dir).join("fonts")),
            );
        }
        found
    }

    /// Adds the font files in a directory and those within, in order of
    /// their names, to `found`.
    fn walk(dir: &Path, depth: u32, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                if depth < 4 {
                    walk(&path, depth + 1, found);
                }
            } else if path.extension().is_some_and(|extension| {
                ["ttf", "ttc", "otf"]
                    .iter()
                    .any(|known| extension.eq_ignore_ascii_case(known))
            }) {
                found.push(path);
            }
        }
    }

    /// A font file's names, reading only its directory and `name` table;
    /// nothing for one not of TrueType outlines.
    fn names_of(path: &Path) -> Option<Names> {
        let mut file = std::fs::File::open(path).ok()?;
        let mut read = |at: u64, length: usize| -> Option<Vec<u8>> {
            let mut bytes = vec![0; length];
            file.seek(SeekFrom::Start(at)).ok()?;
            file.read_exact(&mut bytes).ok()?;
            Some(bytes)
        };
        let head = read(0, 16)?;
        let start = if &head[0..4] == b"ttcf" {
            u32_at(&head, 12)? as u64
        } else {
            0
        };
        let header = read(start, 12)?;
        if !matches!(&header[0..4], [0, 1, 0, 0] | b"true") {
            return None;
        }
        let count = u16_at(&header, 4)? as usize;
        let records = read(start + 12, 16 * count)?;
        for record in records.as_chunks::<16>().0 {
            if &record[0..4] == b"name" {
                let offset = u32_at(record, 8)? as u64;
                let length = (u32_at(record, 12)? as usize).min(1 << 20);
                return Some(Names::read(&read(offset, length)?));
            }
        }
        None
    }

    /// How well a font's names match `wanted`, lower being better: its
    /// full name, then its family in its regular style, then in another.
    fn fit(names: &Names, wanted: &str) -> Option<u32> {
        let same = |name: &str| name.trim().eq_ignore_ascii_case(wanted.trim());
        if same(&names.full) {
            return Some(0);
        }
        if names.families.iter().any(|family| same(family)) {
            return Some(if regular(names) { 1 } else { 2 });
        }
        None
    }

    /// Whether a font is of its family's regular style.
    fn regular(names: &Names) -> bool {
        ["Regular", "Normal", "Book", "Roman", "Plain"]
            .iter()
            .any(|style| names.style.eq_ignore_ascii_case(style))
    }

    pub fn find(name: &str) -> Result<Font, HostError> {
        let mut found = FOUND.lock().unwrap_or_else(|e| e.into_inner());
        let fonts = found.get_or_insert_with(|| {
            let mut paths = Vec::new();
            for dir in directories() {
                walk(&dir, 0, &mut paths);
            }
            paths
                .into_iter()
                .filter_map(|path| names_of(&path).map(|names| (path, names)))
                .collect()
        });
        // A kind of font is the system's usual one of the kind, or else its
        // usual sans-serif one, or else any in its regular style.
        let kind = match name.trim().to_ascii_lowercase().as_str() {
            "" | "sans-serif" => Some(0),
            "serif" => Some(1),
            "monospace" => Some(2),
            _ => None,
        };
        let wanted: Vec<&str> = match kind {
            Some(kind) => USUAL[kind].iter().chain(USUAL[0]).copied().collect(),
            None => vec![name],
        };
        let chosen = wanted
            .iter()
            .find_map(|wanted| {
                fonts
                    .iter()
                    .filter_map(|(path, names)| fit(names, wanted).map(|fit| (fit, path)))
                    .min_by_key(|(fit, _)| *fit)
                    .map(|(_, path)| path)
            })
            .or_else(|| {
                kind?;
                let regular = fonts.iter().find(|(_, names)| regular(names));
                regular.or(fonts.first()).map(|(path, _)| path)
            });
        if let Some(path) = chosen {
            let bytes = std::fs::read(path).map_err(|e| {
                fail(format!(
                    "could not read the font \"{}\": {e}",
                    path.display()
                ))
            })?;
            return Font::from_bytes(bytes);
        }
        Err(fail(if fonts.is_empty() {
            "the system has no fonts of TrueType outlines where Sayon looks".to_string()
        } else {
            format!("the system has no font named \"{name}\"")
        }))
    }
}

/// The browser's fonts, which the page's glue measures and draws with a
/// canvas of its own.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod web {
    use super::{Face, Font, fail};
    use crate::graphics::Canvas;
    use crate::host::HostError;

    #[link(wasm_import_module = "sayon")]
    unsafe extern "C" {
        /// The number of the browser's font named so, 0 when the browser
        /// lacks it, or -1 where there is no canvas to draw text with.
        fn host_font_open(name: *const u8, length: usize) -> i32;
        /// How wide a line of text is (0), or the font's ascent (1) or
        /// descent (2), at `size` pixels.
        fn host_font_measure(
            font: i32,
            text: *const u8,
            length: usize,
            size: f64,
            which: i32,
        ) -> f64;
        /// Draws a line of text in white on a canvas of its own and gives
        /// its width; `host_font_drawn` gives its height (0) and where on it
        /// the text starts (1) and its baseline is (2), and
        /// `host_font_take` copies how opaque each of its pixels is.
        fn host_font_draw(font: i32, text: *const u8, length: usize, size: f64) -> i32;
        fn host_font_drawn(which: i32) -> i32;
        fn host_font_take(into: *mut u8);
        fn host_font_close(font: i32);
    }

    pub fn open(name: &str) -> Result<Font, HostError> {
        let name = if name.trim().is_empty() {
            "sans-serif"
        } else {
            name.trim()
        };
        match unsafe { host_font_open(name.as_ptr(), name.len()) } {
            -1 => Err(fail(
                "there is no canvas to draw text with here, so no fonts",
            )),
            0 => Err(fail(format!("the browser has no font named \"{name}\""))),
            number => Ok(Font {
                name: name.to_string(),
                face: Face::Browser(number),
            }),
        }
    }

    fn measure(font: i32, text: &str, size: f64, which: i32) -> f64 {
        unsafe { host_font_measure(font, text.as_ptr(), text.len(), size, which) }
    }

    pub fn metrics(font: i32, size: f64) -> (f64, f64, f64) {
        (measure(font, "", size, 1), measure(font, "", size, 2), 0.0)
    }

    pub fn advance(font: i32, line: &str, size: f64) -> f64 {
        measure(font, line, size, 0)
    }

    pub fn draw(
        font: i32,
        canvas: &mut Canvas,
        line: &str,
        (x, baseline): (i64, i64),
        size: f64,
        color: i64,
    ) {
        if line.is_empty() {
            return;
        }
        let width = unsafe { host_font_draw(font, line.as_ptr(), line.len(), size) };
        let [height, left, top] = [0, 1, 2].map(|which| unsafe { host_font_drawn(which) });
        if width <= 0 || height <= 0 {
            return;
        }
        let mut mask = vec![0u8; width as usize * height as usize];
        unsafe { host_font_take(mask.as_mut_ptr()) };
        canvas.blend_mask(
            &mask,
            width as usize,
            (x - left as i64, baseline - top as i64),
            color,
        );
    }

    pub fn close(font: i32) {
        unsafe { host_font_close(font) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes of 16-bit words, as a font holds them.
    fn words(values: &[i32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|&v| (v as u16).to_be_bytes())
            .collect()
    }

    /// A font of two glyphs, the one for missing characters and a square
    /// for "A", made here as a font maker would.
    fn square_font() -> Vec<u8> {
        let mut head = vec![0; 54];
        head[18..20].copy_from_slice(&1000u16.to_be_bytes());
        let maxp = words(&[0, 0, 2]);
        let mut hhea = words(&[0, 0, 800, -200, 0]);
        hhea.resize(34, 0);
        hhea.extend(words(&[2]));
        let hmtx = words(&[500, 0, 1000, 0]);
        // The square: one contour of four points on the curve, from 0 to
        // 500 across and up, each a change from the last.
        let mut glyf = words(&[1, 0, 0, 500, 500, 3, 0]);
        glyf.extend([1, 1, 1, 1]);
        glyf.extend(words(&[0, 500, 0, -500, 0, 0, 500, 0]));
        let loca = words(&[0, 0, glyf.len() as i32 / 2]);
        // Format 4, after the header and the one record: "A" to glyph 1,
        // and the segment that ends the table.
        let cmap = [
            words(&[0, 1, 3, 1, 0, 12]),
            words(&[4, 32, 0, 4, 0, 0, 0]),
            words(&[0x41, 0xFFFF, 0, 0x41, 0xFFFF, 1 - 0x41, 1, 0, 0]),
        ]
        .concat();
        let name_text = words(&"Square".encode_utf16().map(i32::from).collect::<Vec<_>>());
        let mut name = words(&[0, 1, 18, 3, 1, 0x409, 4, name_text.len() as i32, 0]);
        name.extend(name_text);
        let tables: [(&[u8; 4], Vec<u8>); 8] = [
            (b"cmap", cmap),
            (b"glyf", glyf),
            (b"head", head),
            (b"hhea", hhea),
            (b"hmtx", hmtx),
            (b"loca", loca),
            (b"maxp", maxp),
            (b"name", name),
        ];
        let mut font = [vec![0, 1, 0, 0], words(&[tables.len() as i32, 0, 0, 0])].concat();
        let mut body = Vec::new();
        for (tag, data) in &tables {
            let offset = 12 + 16 * tables.len() + body.len();
            font.extend_from_slice(*tag);
            font.extend([0; 4]);
            font.extend((offset as u32).to_be_bytes());
            font.extend((data.len() as u32).to_be_bytes());
            body.extend_from_slice(data);
            body.resize(body.len().next_multiple_of(4), 0);
        }
        font.extend(body);
        font
    }

    #[test]
    fn a_font_draws_its_glyphs() {
        let font = Font::from_bytes(square_font()).unwrap();
        assert_eq!(font.name, "Square");
        // At 20 pixels an em: an ascent of 16, a descent of 4, and the
        // square 10 pixels a side, sitting on the baseline.
        assert_eq!(
            (font.metric(0, 20), font.metric(1, 20), font.metric(2, 20)),
            (16, 4, 20)
        );
        assert_eq!(font.width("AA\nA", 20), 40);
        assert_eq!(font.height("A\nA", 20), 40);
        let mut canvas = Canvas::new(40, 40).unwrap();
        font.draw(&mut canvas, "AA", (0, 0), 20, 0xFF00_00FF);
        assert_eq!(canvas.pixel(5, 10), 0xFF00_00FF);
        assert_eq!(canvas.pixel(25, 10), 0xFF00_00FF);
        assert_eq!(canvas.pixel(15, 10), 0);
        assert_eq!(canvas.pixel(5, 17), 0);
    }

    #[test]
    fn what_is_not_a_font_fails() {
        assert!(Font::from_bytes(b"not a font at all".to_vec()).is_err());
        let mut font = square_font();
        font[0..4].copy_from_slice(b"OTTO");
        let error = Font::from_bytes(font).err().unwrap();
        assert!(error.1.contains("CFF"), "{error:?}");
        let mut font = square_font();
        font.truncate(200);
        assert!(Font::from_bytes(font).is_err());
    }
}
