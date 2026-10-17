//! Reading the zip archive embedded in the installer: the central
//! directory, DEFLATE decompression (RFC 1951), and CRC-32 checks.
//!
//! Only what the release archives use is supported: stored and deflated
//! entries, no encryption, no ZIP64. Entry names are checked so that none
//! can escape the installation directory.

/// A file from the archive. Directories are implied by file paths.
#[derive(Debug, PartialEq)]
pub struct Entry {
    /// Path inside the archive, with `/` separators.
    pub path: String,
    pub data: Vec<u8>,
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, String> {
    bytes
        .get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| "the archive is truncated".to_string())
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, String> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| "the archive is truncated".to_string())
}

/// Reads every file of a zip archive, verifying each one's checksum.
pub fn read_zip(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    const END_SIGNATURE: u32 = 0x0605_4b50;
    const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
    const LOCAL_SIGNATURE: u32 = 0x0403_4b50;

    // The end record is at least 22 bytes and may be followed by a comment.
    let search_start = bytes.len().saturating_sub(22 + 0xFFFF);
    let end = (search_start..=bytes.len().saturating_sub(22))
        .rev()
        .find(|&at| u32_at(bytes, at) == Ok(END_SIGNATURE))
        .ok_or("not a zip archive")?;
    let count = u16_at(bytes, end + 10)? as usize;
    let mut at = u32_at(bytes, end + 16)? as usize;

    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        if u32_at(bytes, at)? != CENTRAL_SIGNATURE {
            return Err("the archive's directory is damaged".into());
        }
        let flags = u16_at(bytes, at + 8)?;
        let method = u16_at(bytes, at + 10)?;
        let crc = u32_at(bytes, at + 16)?;
        let compressed = u32_at(bytes, at + 20)? as usize;
        let size = u32_at(bytes, at + 24)? as usize;
        let name_length = u16_at(bytes, at + 28)? as usize;
        let extra_length = u16_at(bytes, at + 30)? as usize;
        let comment_length = u16_at(bytes, at + 32)? as usize;
        let local = u32_at(bytes, at + 42)? as usize;
        let raw_name = bytes
            .get(at + 46..at + 46 + name_length)
            .ok_or("the archive is truncated")?;
        at += 46 + name_length + extra_length + comment_length;

        let name = std::str::from_utf8(raw_name)
            .map_err(|_| "an archive entry has a name that is not UTF-8".to_string())?;
        if flags & 1 != 0 {
            return Err(format!("`{name}` is encrypted"));
        }
        let path = safe_path(name)?;
        if path.ends_with('/') {
            continue;
        }

        if u32_at(bytes, local)? != LOCAL_SIGNATURE {
            return Err(format!("the archive entry for `{path}` is damaged"));
        }
        let start =
            local + 30 + u16_at(bytes, local + 26)? as usize + u16_at(bytes, local + 28)? as usize;
        let stored = bytes
            .get(start..start + compressed)
            .ok_or("the archive is truncated")?;
        let data = match method {
            0 => stored.to_vec(),
            8 => inflate(stored, size)?,
            other => {
                return Err(format!(
                    "`{path}` uses unsupported compression method {other}"
                ));
            }
        };
        if data.len() != size || crc32(&data) != crc {
            return Err(format!(
                "`{path}` is corrupted (its checksum does not match)"
            ));
        }
        entries.push(Entry { path, data });
    }
    Ok(entries)
}

/// Normalizes an entry name and rejects any that could leave the
/// destination: absolute paths, drive letters, and `..` components.
fn safe_path(name: &str) -> Result<String, String> {
    let path = name.replace('\\', "/");
    let unsafe_name = path.starts_with('/')
        || path.contains(':')
        || path.is_empty()
        || path
            .trim_end_matches('/')
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..");
    if unsafe_name {
        return Err(format!("the archive contains an unsafe path `{name}`"));
    }
    Ok(path)
}

// ----- DEFLATE ------------------------------------------------------------------

struct Bits<'a> {
    input: &'a [u8],
    position: usize,
    buffer: u32,
    count: u32,
}

impl<'a> Bits<'a> {
    fn new(input: &'a [u8]) -> Bits<'a> {
        Bits {
            input,
            position: 0,
            buffer: 0,
            count: 0,
        }
    }

    /// The next `need` bits, least significant first.
    fn take(&mut self, need: u32) -> Result<u32, String> {
        while self.count < need {
            let byte = *self
                .input
                .get(self.position)
                .ok_or("compressed data ends early")?;
            self.position += 1;
            self.buffer |= (byte as u32) << self.count;
            self.count += 8;
        }
        let value = self.buffer & ((1u64 << need) - 1) as u32;
        self.buffer >>= need;
        self.count -= need;
        Ok(value)
    }

    /// Drops the bits left in the current byte.
    fn align(&mut self) {
        self.buffer = 0;
        self.count = 0;
    }
}

/// A canonical Huffman code: how many codes have each length, and the
/// symbols in code order.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Huffman, String> {
        let mut counts = [0u16; 16];
        for &length in lengths {
            counts[length as usize] += 1;
        }
        counts[0] = 0;
        // Reject over-subscribed codes; incomplete ones are allowed (RFC 1951
        // permits a single distance code).
        let mut left: i32 = 1;
        for &count in &counts[1..] {
            left = left * 2 - count as i32;
            if left < 0 {
                return Err("compressed data has an invalid code".into());
            }
        }
        let mut offsets = [0u16; 16];
        for length in 1..15 {
            offsets[length + 1] = offsets[length] + counts[length];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length != 0 {
                symbols[offsets[length as usize] as usize] = symbol as u16;
                offsets[length as usize] += 1;
            }
        }
        Ok(Huffman { counts, symbols })
    }

    fn decode(&self, bits: &mut Bits) -> Result<u16, String> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for length in 1..16 {
            code |= bits.take(1)? as i32;
            let count = self.counts[length] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("compressed data has an invalid code".into())
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

/// Decompresses a raw DEFLATE stream whose output is `size` bytes.
pub fn inflate(input: &[u8], size: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(size);
    let mut bits = Bits::new(input);
    loop {
        let last = bits.take(1)? == 1;
        match bits.take(2)? {
            0 => {
                bits.align();
                let at = bits.position;
                let length = u16_at(input, at)?;
                let check = u16_at(input, at + 2)?;
                if length != !check {
                    return Err("compressed data has a damaged block".into());
                }
                let data = input
                    .get(at + 4..at + 4 + length as usize)
                    .ok_or("compressed data ends early")?;
                out.extend_from_slice(data);
                bits.position = at + 4 + length as usize;
            }
            1 => {
                let mut lengths = [0u8; 288];
                lengths[..144].fill(8);
                lengths[144..256].fill(9);
                lengths[256..280].fill(7);
                lengths[280..].fill(8);
                let literals = Huffman::new(&lengths)?;
                let distances = Huffman::new(&[5u8; 30])?;
                codes(&mut bits, &mut out, &literals, &distances)?;
            }
            2 => {
                let (literals, distances) = dynamic_codes(&mut bits)?;
                codes(&mut bits, &mut out, &literals, &distances)?;
            }
            _ => return Err("compressed data has an invalid block type".into()),
        }
        if out.len() > size {
            return Err("compressed data is larger than recorded".into());
        }
        if last {
            return Ok(out);
        }
    }
}

fn dynamic_codes(bits: &mut Bits) -> Result<(Huffman, Huffman), String> {
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let literal_count = bits.take(5)? as usize + 257;
    let distance_count = bits.take(5)? as usize + 1;
    let code_count = bits.take(4)? as usize + 4;
    if literal_count > 286 || distance_count > 30 {
        return Err("compressed data has too many codes".into());
    }
    let mut code_lengths = [0u8; 19];
    for &position in &ORDER[..code_count] {
        code_lengths[position] = bits.take(3)? as u8;
    }
    let lengths_code = Huffman::new(&code_lengths)?;
    let mut lengths = vec![0u8; literal_count + distance_count];
    let mut index = 0;
    while index < lengths.len() {
        let symbol = lengths_code.decode(bits)?;
        let (value, repeat) = match symbol {
            0..=15 => (symbol as u8, 1),
            16 => {
                let previous = *lengths[..index]
                    .last()
                    .ok_or("compressed data repeats a missing length")?;
                (previous, 3 + bits.take(2)? as usize)
            }
            17 => (0, 3 + bits.take(3)? as usize),
            _ => (0, 11 + bits.take(7)? as usize),
        };
        if index + repeat > lengths.len() {
            return Err("compressed data has too many lengths".into());
        }
        lengths[index..index + repeat].fill(value);
        index += repeat;
    }
    if lengths[256] == 0 {
        return Err("compressed data has no end-of-block code".into());
    }
    Ok((
        Huffman::new(&lengths[..literal_count])?,
        Huffman::new(&lengths[literal_count..])?,
    ))
}

fn codes(
    bits: &mut Bits,
    out: &mut Vec<u8>,
    literals: &Huffman,
    distances: &Huffman,
) -> Result<(), String> {
    loop {
        let symbol = literals.decode(bits)?;
        match symbol {
            0..=255 => out.push(symbol as u8),
            256 => return Ok(()),
            _ => {
                let index = (symbol - 257) as usize;
                if index >= LENGTH_BASE.len() {
                    return Err("compressed data has an invalid length".into());
                }
                let length =
                    LENGTH_BASE[index] as usize + bits.take(LENGTH_EXTRA[index] as u32)? as usize;
                let code = distances.decode(bits)? as usize;
                if code >= DISTANCE_BASE.len() {
                    return Err("compressed data has an invalid distance".into());
                }
                let distance =
                    DISTANCE_BASE[code] as usize + bits.take(DISTANCE_EXTRA[code] as u32)? as usize;
                if distance > out.len() {
                    return Err("compressed data refers before its start".into());
                }
                let from = out.len() - distance;
                // Copies may overlap the bytes they produce.
                for offset in 0..length {
                    let byte = out[from + offset];
                    out.push(byte);
                }
            }
        }
    }
}

// ----- CRC-32 -----------------------------------------------------------------------

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                0xEDB8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// The CRC-32 used by zip archives.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc = CRC_TABLE[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a zip archive of stored (uncompressed) entries.
    fn stored_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in files {
            let offset = out.len() as u32;
            let crc = crc32(data);
            let header = |signature: u32, central_record: bool| {
                let mut h = Vec::new();
                h.extend_from_slice(&signature.to_le_bytes());
                if central_record {
                    h.extend_from_slice(&20u16.to_le_bytes());
                }
                h.extend_from_slice(&20u16.to_le_bytes());
                h.extend_from_slice(&0u16.to_le_bytes()); // flags
                h.extend_from_slice(&0u16.to_le_bytes()); // stored
                h.extend_from_slice(&0u32.to_le_bytes()); // time and date
                h.extend_from_slice(&crc.to_le_bytes());
                h.extend_from_slice(&(data.len() as u32).to_le_bytes());
                h.extend_from_slice(&(data.len() as u32).to_le_bytes());
                h.extend_from_slice(&(name.len() as u16).to_le_bytes());
                h.extend_from_slice(&0u16.to_le_bytes()); // extra
                if central_record {
                    h.extend_from_slice(&0u16.to_le_bytes()); // comment
                    h.extend_from_slice(&0u16.to_le_bytes()); // disk
                    h.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
                    h.extend_from_slice(&0u32.to_le_bytes()); // external attributes
                    h.extend_from_slice(&offset.to_le_bytes());
                }
                h.extend_from_slice(name.as_bytes());
                h
            };
            out.extend(header(0x0403_4b50, false));
            out.extend_from_slice(data);
            central.extend(header(0x0201_4b50, true));
        }
        let directory_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&directory_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn checksums_match_the_standard() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn stored_entries_are_read() {
        let zip = stored_zip(&[
            ("top/", b""),
            ("top/a.txt", b"hello"),
            ("top\\b\\c.txt", b""),
        ]);
        let entries = read_zip(&zip).unwrap();
        assert_eq!(
            entries,
            vec![
                Entry {
                    path: "top/a.txt".into(),
                    data: b"hello".to_vec()
                },
                Entry {
                    path: "top/b/c.txt".into(),
                    data: Vec::new()
                },
            ]
        );
    }

    #[test]
    fn unsafe_names_and_damage_are_rejected() {
        for name in [
            "../evil.txt",
            "/abs.txt",
            "C:/drive.txt",
            "a/../../b",
            "a//b",
        ] {
            assert!(read_zip(&stored_zip(&[(name, b"x")])).is_err(), "{name}");
        }
        let mut zip = stored_zip(&[("a.txt", b"hello")]);
        zip[30 + 5] = b'j'; // change the stored data, not the checksum
        assert!(read_zip(&zip).unwrap_err().contains("corrupted"));
        assert!(read_zip(b"not an archive").is_err());
    }

    #[test]
    fn archives_from_compress_archive_are_read() {
        // Made by PowerShell's Compress-Archive, as the release script does.
        let entries = read_zip(include_bytes!("../tests/sample.zip")).unwrap();
        let find = |path: &str| {
            entries
                .iter()
                .find(|e| e.path == path)
                .map(|e| e.data.clone())
                .unwrap_or_else(|| panic!("{path} missing"))
        };
        let numbers: String = (0..3000).map(|i| format!("line {i}\n")).collect();
        assert_eq!(find("sample/numbers.txt"), numbers.into_bytes());
        assert_eq!(find("sample/empty.txt"), Vec::<u8>::new());
        let mut x: u64 = 1;
        let random: Vec<u8> = (0..1000)
            .map(|_| {
                x = (x * 1_103_515_245 + 12_345) % 2_147_483_648;
                ((x >> 16) & 0xFF) as u8
            })
            .collect();
        assert_eq!(find("sample/nested/random.bin"), random);
    }

    #[test]
    fn fixed_and_stored_blocks_inflate() {
        // "hello, hello" with a back-reference, fixed Huffman codes.
        let fixed = [
            0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0xd7, 0x51, 0xc8, 0x00, 0x51, 0x00,
        ];
        assert_eq!(inflate(&fixed, 12).unwrap(), b"hello, hello");
        // One final stored block.
        let stored = [0x01, 0x03, 0x00, 0xfc, 0xff, b'a', b'b', b'c'];
        assert_eq!(inflate(&stored, 3).unwrap(), b"abc");
        assert!(inflate(&[0xff], 1).is_err());
    }
}
