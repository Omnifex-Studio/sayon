//! The little of the ZIP format an Android package needs: reading the
//! entries of a package `aapt2` wrote, and writing them again with more,
//! each stored entry's data aligned as Android maps it from the package:
//! 4 bytes for most, a page (16 KiB, the larger of the pages Android uses)
//! for native libraries. That is what `zipalign -P 16` does.

/// An entry, as the package holds it: compressed or stored.
pub struct Entry {
    pub name: String,
    /// 0 for stored, 8 for deflated.
    pub method: u16,
    pub crc: u32,
    pub size: u32,
    /// The entry's data as the package holds it.
    pub data: Vec<u8>,
}

impl Entry {
    /// An entry stored as it is.
    pub fn stored(name: &str, data: Vec<u8>) -> Entry {
        Entry {
            name: name.to_string(),
            method: 0,
            crc: crc32(&data),
            size: data.len() as u32,
            data,
        }
    }
}

const LOCAL: u32 = 0x0403_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const END: u32 = 0x0605_4b50;

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, String> {
    bytes
        .get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| "the package ends early".to_string())
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, String> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| "the package ends early".to_string())
}

/// The entries of a ZIP file, in the order of its central directory, which
/// tells their sizes even where a local header leaves them to a descriptor.
pub fn read(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    let end = (0..bytes.len().saturating_sub(21))
        .rev()
        .find(|&at| u32_at(bytes, at) == Ok(END))
        .ok_or("the package has no ZIP directory")?;
    let count = u16_at(bytes, end + 10)? as usize;
    let mut at = u32_at(bytes, end + 16)? as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        if u32_at(bytes, at)? != CENTRAL {
            return Err("the package's ZIP directory is damaged".to_string());
        }
        let method = u16_at(bytes, at + 10)?;
        let crc = u32_at(bytes, at + 16)?;
        let compressed = u32_at(bytes, at + 20)? as usize;
        let size = u32_at(bytes, at + 24)?;
        let name_length = u16_at(bytes, at + 28)? as usize;
        let extra_length = u16_at(bytes, at + 30)? as usize;
        let comment_length = u16_at(bytes, at + 32)? as usize;
        let local = u32_at(bytes, at + 42)? as usize;
        let name = bytes
            .get(at + 46..at + 46 + name_length)
            .ok_or("the package ends early")?;
        if u32_at(bytes, local)? != LOCAL {
            return Err("an entry of the package is damaged".to_string());
        }
        let start =
            local + 30 + u16_at(bytes, local + 26)? as usize + u16_at(bytes, local + 28)? as usize;
        let data = bytes
            .get(start..start + compressed)
            .ok_or("the package ends early")?
            .to_vec();
        entries.push(Entry {
            name: String::from_utf8_lossy(name).into_owned(),
            method,
            crc,
            size,
            data,
        });
        at += 46 + name_length + extra_length + comment_length;
    }
    Ok(entries)
}

/// The ID of the extra field that pads an entry to its alignment, as
/// Android's `zipalign` writes it: the alignment, then zeros.
const ALIGNMENT_FIELD: u16 = 0xD935;

/// A ZIP file of `entries`, each stored one's data aligned to `alignment`
/// of its name.
pub fn write(entries: &[Entry], alignment: impl Fn(&str) -> usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut directory = Vec::new();
    for entry in entries {
        let offset = out.len();
        let header = 30 + entry.name.len();
        let mut extra = Vec::new();
        if entry.method == 0 {
            let align = alignment(&entry.name);
            let mut padding = (align - (offset + header) % align) % align;
            // The field needs room for its header and the alignment.
            if padding != 0 && padding < 6 {
                padding += align;
            }
            if padding != 0 {
                extra.extend_from_slice(&ALIGNMENT_FIELD.to_le_bytes());
                extra.extend_from_slice(&((padding - 4) as u16).to_le_bytes());
                extra.extend_from_slice(&(align as u16).to_le_bytes());
                extra.resize(padding, 0);
            }
        }
        // Version 2.0, no flags, and a fixed time (1981-01-01), so that
        // the same program makes the same package.
        let fields = |out: &mut Vec<u8>| {
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&entry.method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0x0221u16.to_le_bytes());
            out.extend_from_slice(&entry.crc.to_le_bytes());
            out.extend_from_slice(&(entry.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&entry.size.to_le_bytes());
            out.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
        };
        out.extend_from_slice(&LOCAL.to_le_bytes());
        fields(&mut out);
        out.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        out.extend_from_slice(entry.name.as_bytes());
        out.extend_from_slice(&extra);
        out.extend_from_slice(&entry.data);

        directory.extend_from_slice(&CENTRAL.to_le_bytes());
        directory.extend_from_slice(&20u16.to_le_bytes());
        fields(&mut directory);
        // No extra field, no comment, disk 0, no attributes.
        directory.extend_from_slice(&[0; 12]);
        directory.extend_from_slice(&(offset as u32).to_le_bytes());
        directory.extend_from_slice(entry.name.as_bytes());
    }
    let start = out.len();
    out.extend_from_slice(&directory);
    out.extend_from_slice(&END.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(directory.len() as u32).to_le_bytes());
    out.extend_from_slice(&(start as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// The CRC-32 a ZIP file checks each entry with.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (index, slot) in table.iter_mut().enumerate() {
        let mut value = index as u32;
        for _ in 0..8 {
            value = if value & 1 != 0 {
                0xEDB8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
        }
        *slot = value;
    }
    let mut crc = !0u32;
    for &byte in bytes {
        crc = table[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn entries_survive_writing_and_align() {
        let entries = vec![
            Entry::stored("a.txt", b"hello".to_vec()),
            Entry::stored("lib/x86_64/libmain.so", vec![7; 100]),
        ];
        let bytes = write(
            &entries,
            |name| if name.ends_with(".so") { 16384 } else { 4 },
        );
        let read_back = read(&bytes).unwrap();
        assert_eq!(read_back.len(), 2);
        assert_eq!(read_back[1].name, "lib/x86_64/libmain.so");
        assert_eq!(read_back[1].data, vec![7; 100]);
        assert_eq!(read_back[1].crc, crc32(&[7; 100]));
        let at = bytes.windows(100).position(|w| w == [7; 100]).unwrap();
        assert_eq!(at % 16384, 0);
    }
}
