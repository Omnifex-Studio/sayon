//! PNG images for `standard/graphics`: decoding every kind of PNG into a
//! canvas's red, green, blue, and alpha bytes, and encoding a canvas as one.
//! The runtime has its own DEFLATE, as zlib has it (RFC 1950 and 1951), so
//! that images need no library of the system's.

use crate::host::HostError;

fn fail(message: impl Into<String>) -> HostError {
    ("ImageError", message.into())
}

// ----- checksums -----------------------------------------------------------------------------

/// The CRC-32 a PNG chunk is checked with.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// The Adler-32 a zlib stream ends with.
fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in bytes.chunks(5552) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

// ----- inflating ------------------------------------------------------------------------------

/// The bits of a DEFLATE stream, the lowest of each byte first.
struct Bits<'a> {
    data: &'a [u8],
    position: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Result<u32, HostError> {
        let byte = *self
            .data
            .get(self.position / 8)
            .ok_or_else(|| fail("the compressed data ends early"))?;
        let bit = (byte >> (self.position % 8)) & 1;
        self.position += 1;
        Ok(bit as u32)
    }

    fn bits(&mut self, count: u32) -> Result<u32, HostError> {
        let mut value = 0;
        for index in 0..count {
            value |= self.bit()? << index;
        }
        Ok(value)
    }
}

/// A canonical Huffman code: how many codes each length has, and the
/// symbols in the order of their codes.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Huffman {
        let mut counts = [0u16; 16];
        for &length in lengths {
            counts[length as usize] += 1;
        }
        counts[0] = 0;
        let mut offsets = [0u16; 16];
        for length in 1..16 {
            offsets[length] = offsets[length - 1] + counts[length - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length != 0 {
                symbols[offsets[length as usize] as usize] = symbol as u16;
                offsets[length as usize] += 1;
            }
        }
        Huffman { counts, symbols }
    }

    /// Reads one symbol, a bit at a time, as zlib's `puff` does.
    fn decode(&self, bits: &mut Bits) -> Result<u16, HostError> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for length in 1..16 {
            code |= bits.bit()? as i32;
            let count = self.counts[length] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(fail("the compressed data has a code that means nothing"))
    }
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// The lengths of the fixed Huffman codes of literals and lengths.
fn fixed_lengths() -> Vec<u8> {
    let mut lengths = vec![8u8; 288];
    lengths[144..256].fill(9);
    lengths[256..280].fill(7);
    lengths
}

/// Decompresses a raw DEFLATE stream, stopping past `limit` bytes.
fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, HostError> {
    let mut bits = Bits { data, position: 0 };
    let mut out = Vec::new();
    loop {
        let last = bits.bit()?;
        match bits.bits(2)? {
            0 => {
                // Stored: from the next byte, its length and the length's
                // complement.
                bits.position = bits.position.div_ceil(8) * 8;
                let length = bits.bits(16)?;
                let complement = bits.bits(16)?;
                if length != !complement & 0xFFFF {
                    return Err(fail("a stored block of the compressed data is damaged"));
                }
                let start = bits.position / 8;
                let block = data
                    .get(start..start + length as usize)
                    .ok_or_else(|| fail("the compressed data ends early"))?;
                out.extend_from_slice(block);
                bits.position += length as usize * 8;
            }
            kind @ (1 | 2) => {
                let (literals, distances) = if kind == 1 {
                    (Huffman::new(&fixed_lengths()), Huffman::new(&[5; 30]))
                } else {
                    dynamic_codes(&mut bits)?
                };
                loop {
                    let symbol = literals.decode(&mut bits)?;
                    match symbol {
                        0..=255 => out.push(symbol as u8),
                        256 => break,
                        257..=285 => {
                            let index = (symbol - 257) as usize;
                            let length = LENGTH_BASE[index] as usize
                                + bits.bits(LENGTH_EXTRA[index] as u32)? as usize;
                            let code = distances.decode(&mut bits)? as usize;
                            if code >= 30 {
                                return Err(fail(
                                    "the compressed data has a distance that means nothing",
                                ));
                            }
                            let distance = DISTANCE_BASE[code] as usize
                                + bits.bits(DISTANCE_EXTRA[code] as u32)? as usize;
                            if distance > out.len() {
                                return Err(fail(
                                    "the compressed data refers to bytes before its start",
                                ));
                            }
                            let start = out.len() - distance;
                            for offset in 0..length {
                                out.push(out[start + offset]);
                            }
                        }
                        _ => {
                            return Err(fail(
                                "the compressed data has a length that means nothing",
                            ));
                        }
                    }
                    if out.len() > limit {
                        return Err(fail("the image holds more data than its size needs"));
                    }
                }
            }
            _ => return Err(fail("the compressed data has a block of no known kind")),
        }
        if out.len() > limit {
            return Err(fail("the image holds more data than its size needs"));
        }
        if last == 1 {
            return Ok(out);
        }
    }
}

/// The codes a dynamic block gives at its start.
fn dynamic_codes(bits: &mut Bits) -> Result<(Huffman, Huffman), HostError> {
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let literal_count = bits.bits(5)? as usize + 257;
    let distance_count = bits.bits(5)? as usize + 1;
    let code_count = bits.bits(4)? as usize + 4;
    let mut code_lengths = [0u8; 19];
    for &place in ORDER.iter().take(code_count) {
        code_lengths[place] = bits.bits(3)? as u8;
    }
    let codes = Huffman::new(&code_lengths);
    let mut lengths = Vec::with_capacity(literal_count + distance_count);
    while lengths.len() < literal_count + distance_count {
        let symbol = codes.decode(bits)?;
        let (value, repeat) = match symbol {
            0..=15 => (symbol as u8, 1),
            16 => {
                let previous = *lengths
                    .last()
                    .ok_or_else(|| fail("the compressed data repeats a length before any"))?;
                (previous, 3 + bits.bits(2)? as usize)
            }
            17 => (0, 3 + bits.bits(3)? as usize),
            _ => (0, 11 + bits.bits(7)? as usize),
        };
        lengths.extend(std::iter::repeat_n(value, repeat));
    }
    if lengths.len() > literal_count + distance_count {
        return Err(fail("the compressed data's code lengths run over"));
    }
    Ok((
        Huffman::new(&lengths[..literal_count]),
        Huffman::new(&lengths[literal_count..]),
    ))
}

/// Decompresses a zlib stream, checking what it ends with.
fn unzlib(data: &[u8], limit: usize) -> Result<Vec<u8>, HostError> {
    if data.len() < 6
        || data[0] & 0x0F != 8
        || !u16::from_be_bytes([data[0], data[1]]).is_multiple_of(31)
    {
        return Err(fail(
            "the image's data is not compressed as PNG compresses it",
        ));
    }
    if data[1] & 0x20 != 0 {
        return Err(fail("the image's data needs a preset dictionary"));
    }
    let out = inflate(&data[2..], limit)?;
    let stored = u32::from_be_bytes(data[data.len() - 4..].try_into().expect("four bytes"));
    if adler32(&out) != stored {
        return Err(fail("the image's data is damaged (its checksum is wrong)"));
    }
    Ok(out)
}

// ----- deflating ------------------------------------------------------------------------------

/// Writes bits, the lowest first, as DEFLATE wants them.
struct Writer {
    out: Vec<u8>,
    bits: u64,
    count: u32,
}

impl Writer {
    fn put(&mut self, value: u32, count: u32) {
        self.bits |= (value as u64) << self.count;
        self.count += count;
        while self.count >= 8 {
            self.out.push(self.bits as u8);
            self.bits >>= 8;
            self.count -= 8;
        }
    }

    /// A Huffman code, which goes the highest bit first.
    fn code(&mut self, code: u32, length: u32) {
        let reversed = code.reverse_bits() >> (32 - length);
        self.put(reversed, length);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            self.out.push(self.bits as u8);
        }
        self.out
    }
}

/// The fixed code of a literal or length symbol, and its length.
fn fixed_code(symbol: u32) -> (u32, u32) {
    match symbol {
        0..=143 => (0x30 + symbol, 8),
        144..=255 => (0x190 + symbol - 144, 9),
        256..=279 => (symbol - 256, 7),
        _ => (0xC0 + symbol - 280, 8),
    }
}

/// Compresses with the fixed codes, finding repeats of three bytes or more
/// in the last 32 KiB greedily, through a table of where each three bytes
/// were last seen and, for each place in the window, the place before with
/// the same three.
fn deflate(data: &[u8]) -> Vec<u8> {
    const WINDOW: usize = 32 * 1024;
    const CHAIN: usize = 32;
    let mut writer = Writer {
        out: Vec::new(),
        bits: 0,
        count: 0,
    };
    // One block, the last, of fixed codes.
    writer.put(1, 1);
    writer.put(1, 2);
    let hash = |at: usize| {
        ((data[at] as usize) << 10 ^ (data[at + 1] as usize) << 5 ^ data[at + 2] as usize) & 0x7FFF
    };
    let mut head = vec![usize::MAX; 0x8000];
    let mut previous = vec![usize::MAX; WINDOW];
    let mut at = 0;
    let insert = |at: usize, head: &mut Vec<usize>, previous: &mut Vec<usize>| {
        if at + 2 < data.len() {
            let key = hash(at);
            previous[at % WINDOW] = head[key];
            head[key] = at;
        }
    };
    while at < data.len() {
        let mut best = (0usize, 0usize);
        if at + 2 < data.len() {
            let mut candidate = head[hash(at)];
            let mut tries = 0;
            while candidate != usize::MAX && at - candidate < WINDOW && tries < CHAIN {
                let most = (data.len() - at).min(258);
                let length = (0..most)
                    .take_while(|&i| data[candidate + i] == data[at + i])
                    .count();
                if length > best.0 {
                    best = (length, at - candidate);
                    if length == most {
                        break;
                    }
                }
                candidate = previous[candidate % WINDOW];
                tries += 1;
            }
        }
        if best.0 >= 3 {
            let (length, distance) = best;
            let index = LENGTH_BASE
                .iter()
                .rposition(|&base| base as usize <= length)
                .expect("a length");
            let (code, bits) = fixed_code(257 + index as u32);
            writer.code(code, bits);
            writer.put(
                (length - LENGTH_BASE[index] as usize) as u32,
                LENGTH_EXTRA[index] as u32,
            );
            let place = DISTANCE_BASE
                .iter()
                .rposition(|&base| base as usize <= distance)
                .expect("a distance");
            writer.code(place as u32, 5);
            writer.put(
                (distance - DISTANCE_BASE[place] as usize) as u32,
                DISTANCE_EXTRA[place] as u32,
            );
            for offset in 0..length {
                insert(at + offset, &mut head, &mut previous);
            }
            at += length;
        } else {
            let (code, bits) = fixed_code(data[at] as u32);
            writer.code(code, bits);
            insert(at, &mut head, &mut previous);
            at += 1;
        }
    }
    let (code, bits) = fixed_code(256);
    writer.code(code, bits);
    writer.finish()
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    out.extend(deflate(data));
    out.extend(adler32(data).to_be_bytes());
    out
}

// ----- PNG ------------------------------------------------------------------------------------

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'];

/// An image: its size, and its red, green, blue, and alpha bytes.
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

/// Undoes a row's filter, given the row above.
fn unfilter(kind: u8, row: &mut [u8], above: &[u8], step: usize) -> Result<(), HostError> {
    let paeth = |a: i16, b: i16, c: i16| {
        let p = a + b - c;
        let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
        if pa <= pb && pa <= pc {
            a
        } else if pb <= pc {
            b
        } else {
            c
        }
    };
    for index in 0..row.len() {
        let left = if index >= step { row[index - step] } else { 0 };
        let up = above[index];
        let corner = if index >= step {
            above[index - step]
        } else {
            0
        };
        let add = match kind {
            0 => 0,
            1 => left,
            2 => up,
            3 => ((left as u16 + up as u16) / 2) as u8,
            4 => paeth(left as i16, up as i16, corner as i16) as u8,
            _ => {
                return Err(fail(format!(
                    "a row of the image has the filter {kind}, which PNG has not"
                )));
            }
        };
        row[index] = row[index].wrapping_add(add);
    }
    Ok(())
}

/// Decodes a PNG image of at most `most` pixels.
pub fn decode(bytes: &[u8], most: usize) -> Result<Image, HostError> {
    if bytes.get(..8) != Some(&SIGNATURE) {
        return Err(fail("the data is not a PNG image"));
    }
    let mut at = 8;
    let mut header = None;
    let mut palette: Vec<[u8; 4]> = Vec::new();
    let mut transparent: Option<Vec<u8>> = None;
    let mut compressed = Vec::new();
    loop {
        let length = bytes
            .get(at..at + 4)
            .map(|b| u32::from_be_bytes(b.try_into().expect("four bytes")) as usize)
            .ok_or_else(|| fail("the PNG image ends early"))?;
        let chunk = bytes
            .get(at + 4..at + 8 + length)
            .ok_or_else(|| fail("the PNG image ends early"))?;
        let stored = bytes
            .get(at + 8 + length..at + 12 + length)
            .map(|b| u32::from_be_bytes(b.try_into().expect("four bytes")))
            .ok_or_else(|| fail("the PNG image ends early"))?;
        if crc32(chunk) != stored {
            return Err(fail(
                "a chunk of the PNG image is damaged (its checksum is wrong)",
            ));
        }
        let (kind, data) = chunk.split_at(4);
        match kind {
            b"IHDR" if data.len() == 13 => header = Some(data.to_vec()),
            b"PLTE" => {
                palette = data
                    .chunks(3)
                    .filter(|c| c.len() == 3)
                    .map(|c| [c[0], c[1], c[2], 255])
                    .collect()
            }
            b"tRNS" => transparent = Some(data.to_vec()),
            b"IDAT" => compressed.extend_from_slice(data),
            b"IEND" => break,
            // Chunks that must be understood have an upper-case first letter.
            _ if kind[0].is_ascii_uppercase() && kind != b"IHDR" => {
                return Err(fail(format!(
                    "the PNG image has a {} chunk, which Sayon does not read",
                    String::from_utf8_lossy(kind)
                )));
            }
            _ => {}
        }
        at += 12 + length;
    }
    let header = header.ok_or_else(|| fail("the PNG image has no header"))?;
    let width = u32::from_be_bytes(header[0..4].try_into().expect("four bytes")) as usize;
    let height = u32::from_be_bytes(header[4..8].try_into().expect("four bytes")) as usize;
    let (depth, color, interlace) = (header[8], header[9], header[12]);
    if width == 0
        || height == 0
        || width.saturating_mul(height) > most
        || width > 16384
        || height > 16384
    {
        return Err(fail(format!(
            "an image of {width} by {height} pixels is larger than Sayon draws"
        )));
    }
    let channels = match (color, depth) {
        (0, 1 | 2 | 4 | 8 | 16) => 1,
        (2, 8 | 16) => 3,
        (3, 1 | 2 | 4 | 8) => 1,
        (4, 8 | 16) => 2,
        (6, 8 | 16) => 4,
        _ => {
            return Err(fail(format!(
                "the PNG image's colors (type {color}, {depth} bits) are not a kind PNG has"
            )));
        }
    };
    if header[10] != 0 || header[11] != 0 || interlace > 1 {
        return Err(fail(
            "the PNG image is compressed or filtered in no way PNG has",
        ));
    }
    if color == 3 {
        if palette.is_empty() {
            return Err(fail("the PNG image has no palette for its colors"));
        }
        if let Some(alphas) = &transparent {
            for (entry, &alpha) in palette.iter_mut().zip(alphas) {
                entry[3] = alpha;
            }
        }
    }
    let bits = channels * depth as usize;
    let step = bits.div_ceil(8);
    // The passes of Adam7: where each starts, and how far apart its pixels are.
    let passes: &[(usize, usize, usize, usize)] = if interlace == 1 {
        &[
            (0, 0, 8, 8),
            (4, 0, 8, 8),
            (0, 4, 4, 8),
            (2, 0, 4, 4),
            (0, 2, 2, 4),
            (1, 0, 2, 2),
            (0, 1, 1, 2),
        ]
    } else {
        &[(0, 0, 1, 1)]
    };
    let mut needed = 0;
    for &(x0, y0, dx, dy) in passes {
        let (columns, rows) = (
            width.saturating_sub(x0).div_ceil(dx),
            height.saturating_sub(y0).div_ceil(dy),
        );
        if columns > 0 && rows > 0 {
            needed += rows * (1 + (columns * bits).div_ceil(8));
        }
    }
    let data = unzlib(&compressed, needed)?;
    if data.len() < needed {
        return Err(fail("the PNG image's data ends early"));
    }
    let mut pixels = vec![0u8; width * height * 4];
    let maximum = (1u32 << depth.min(8)) - 1;
    let mut offset = 0;
    for &(x0, y0, dx, dy) in passes {
        let (columns, rows) = (
            width.saturating_sub(x0).div_ceil(dx),
            height.saturating_sub(y0).div_ceil(dy),
        );
        if columns == 0 || rows == 0 {
            continue;
        }
        let stride = (columns * bits).div_ceil(8);
        let mut above = vec![0u8; stride];
        for row in 0..rows {
            let kind = data[offset];
            let mut line = data[offset + 1..offset + 1 + stride].to_vec();
            offset += 1 + stride;
            unfilter(kind, &mut line, &above, step)?;
            for column in 0..columns {
                // The samples of the pixel, each as read at its depth.
                let sample = |channel: usize| -> u16 {
                    match depth {
                        16 => u16::from_be_bytes([
                            line[(column * channels + channel) * 2],
                            line[(column * channels + channel) * 2 + 1],
                        ]),
                        8 => line[column * channels + channel] as u16,
                        _ => {
                            let bit = (column * channels + channel) * depth as usize;
                            ((line[bit / 8] >> (8 - depth as usize - bit % 8)) as u32 & maximum)
                                as u16
                        }
                    }
                };
                let to8 = |value: u16| -> u8 {
                    match depth {
                        16 => (value >> 8) as u8,
                        8 => value as u8,
                        _ => (value as u32 * 255 / maximum) as u8,
                    }
                };
                let rgba = match color {
                    0 => {
                        let gray = sample(0);
                        let alpha = match &transparent {
                            Some(t) if t.len() >= 2 && gray == u16::from_be_bytes([t[0], t[1]]) => {
                                0
                            }
                            _ => 255,
                        };
                        [to8(gray), to8(gray), to8(gray), alpha]
                    }
                    2 => {
                        let (r, g, b) = (sample(0), sample(1), sample(2));
                        let alpha = match &transparent {
                            Some(t)
                                if t.len() >= 6
                                    && [r, g, b]
                                        == [0, 2, 4]
                                            .map(|i| u16::from_be_bytes([t[i], t[i + 1]])) =>
                            {
                                0
                            }
                            _ => 255,
                        };
                        [to8(r), to8(g), to8(b), alpha]
                    }
                    3 => *palette.get(sample(0) as usize).ok_or_else(|| {
                        fail("a pixel of the PNG image has a color its palette lacks")
                    })?,
                    4 => {
                        let gray = to8(sample(0));
                        [gray, gray, gray, to8(sample(1))]
                    }
                    _ => [
                        to8(sample(0)),
                        to8(sample(1)),
                        to8(sample(2)),
                        to8(sample(3)),
                    ],
                };
                let (x, y) = (x0 + column * dx, y0 + row * dy);
                pixels[(y * width + x) * 4..(y * width + x) * 4 + 4].copy_from_slice(&rgba);
            }
            above = line;
        }
    }
    Ok(Image {
        width,
        height,
        pixels,
    })
}

/// Encodes red, green, blue, and alpha bytes as a PNG image, each row with
/// the filter that leaves the smallest differences.
pub fn encode(width: usize, height: usize, pixels: &[u8]) -> Vec<u8> {
    let stride = width * 4;
    let mut raw = Vec::with_capacity((stride + 1) * height);
    let zero = vec![0u8; stride];
    for row in 0..height {
        let line = &pixels[row * stride..(row + 1) * stride];
        let above = if row == 0 {
            &zero[..]
        } else {
            &pixels[(row - 1) * stride..row * stride]
        };
        let mut best: Option<(u64, u8, Vec<u8>)> = None;
        for kind in 0..5u8 {
            let filtered: Vec<u8> = (0..stride)
                .map(|index| {
                    let left = if index >= 4 { line[index - 4] } else { 0 };
                    let up = above[index];
                    let corner = if index >= 4 { above[index - 4] } else { 0 };
                    let predicted = match kind {
                        0 => 0,
                        1 => left,
                        2 => up,
                        3 => ((left as u16 + up as u16) / 2) as u8,
                        _ => {
                            let (a, b, c) = (left as i16, up as i16, corner as i16);
                            let p = a + b - c;
                            let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                            (if pa <= pb && pa <= pc {
                                a
                            } else if pb <= pc {
                                b
                            } else {
                                c
                            }) as u8
                        }
                    };
                    line[index].wrapping_sub(predicted)
                })
                .collect();
            let cost: u64 = filtered
                .iter()
                .map(|&b| (b as i8).unsigned_abs() as u64)
                .sum();
            if best.as_ref().is_none_or(|(least, _, _)| cost < *least) {
                best = Some((cost, kind, filtered));
            }
        }
        let (_, kind, filtered) = best.expect("a filter");
        raw.push(kind);
        raw.extend(filtered);
    }
    let mut out = SIGNATURE.to_vec();
    let mut chunk = |kind: &[u8], data: &[u8]| {
        out.extend((data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend(crc32(&body).to_be_bytes());
    };
    let mut header = Vec::new();
    header.extend((width as u32).to_be_bytes());
    header.extend((height as u32).to_be_bytes());
    header.extend([8, 6, 0, 0, 0]);
    chunk(b"IHDR", &header);
    chunk(b"IDAT", &zlib(&raw));
    chunk(b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_match_their_check_values() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn deflated_data_inflates_again() {
        let mut data = Vec::new();
        for index in 0..20_000u32 {
            data.push((index * 7 % 251) as u8);
            if index % 3 == 0 {
                data.extend_from_slice(b"repeated text, repeated text");
            }
        }
        let packed = zlib(&data);
        assert!(packed.len() < data.len() / 2);
        assert_eq!(unzlib(&packed, data.len()).unwrap(), data);
    }

    #[test]
    fn encoded_images_decode_again() {
        let (width, height) = (17, 9);
        let pixels: Vec<u8> = (0..width * height * 4)
            .map(|i| (i * 37 % 256) as u8)
            .collect();
        let png = encode(width, height, &pixels);
        let image = decode(&png, 1 << 26).unwrap();
        assert_eq!((image.width, image.height), (width, height));
        assert_eq!(image.pixels, pixels);
    }

    #[test]
    fn damaged_images_fail() {
        let png = encode(2, 2, &[9; 16]);
        let mut damaged = png.clone();
        damaged[40] ^= 1;
        assert!(decode(&damaged, 100).is_err());
        assert!(decode(&png[..20], 100).is_err());
        assert!(decode(b"not a png", 100).is_err());
        assert!(decode(&png, 3).is_err());
    }
}
