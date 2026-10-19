//! Text encodings other than UTF-8, which Sayon's strings use, for
//! `strings.encode`, `strings.decode`, and files opened with an encoding.

use crate::host::HostError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    /// UTF-16 in the byte order a byte-order mark gives, or little-endian
    /// without one; written little-endian, after a byte-order mark.
    Utf16,
    Utf16Le,
    Utf16Be,
    /// ISO 8859-1: each byte is the character of the same code.
    Latin1,
    Ascii,
    /// Latin-1 with letters and signs in 0x80 to 0x9F instead of controls.
    Windows1252,
}

impl Encoding {
    /// An encoding by its name, in any letter case.
    pub fn by_name(name: &str) -> Result<Encoding, HostError> {
        Ok(match name.to_ascii_lowercase().as_str() {
            "utf-8" | "utf8" => Encoding::Utf8,
            "utf-16" | "utf16" => Encoding::Utf16,
            "utf-16le" | "utf-16-le" => Encoding::Utf16Le,
            "utf-16be" | "utf-16-be" => Encoding::Utf16Be,
            "latin-1" | "latin1" | "iso-8859-1" => Encoding::Latin1,
            "ascii" | "us-ascii" => Encoding::Ascii,
            "windows-1252" | "cp1252" => Encoding::Windows1252,
            _ => {
                return Err((
                    "ValueError",
                    format!(
                        "\"{name}\" is not an encoding; use \"utf-8\", \"utf-16\", \"utf-16le\", \"utf-16be\", \"latin-1\", \"ascii\", or \"windows-1252\""
                    ),
                ));
            }
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf16 => "UTF-16",
            Encoding::Utf16Le => "UTF-16LE",
            Encoding::Utf16Be => "UTF-16BE",
            Encoding::Latin1 => "Latin-1",
            Encoding::Ascii => "ASCII",
            Encoding::Windows1252 => "Windows-1252",
        }
    }
}

/// The characters Windows-1252 has in 0x80 to 0x9F, or 0 for the five
/// codes it leaves undefined.
const WINDOWS_1252: [u32; 32] = [
    0x20AC, 0, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039,
    0x0152, 0, 0x017D, 0, 0, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC,
    0x2122, 0x0161, 0x203A, 0x0153, 0, 0x017E, 0x0178,
];

fn unencodable(character: char, encoding: Encoding) -> HostError {
    let mut shown = String::new();
    crate::text::quote(&character.to_string(), &mut shown);
    (
        "EncodingError",
        format!("{shown} has no {} form", encoding.name()),
    )
}

/// Text in an encoding.
pub fn encode(text: &str, encoding: Encoding) -> Result<Vec<u8>, HostError> {
    let mut bytes = Vec::with_capacity(text.len());
    match encoding {
        Encoding::Utf8 => bytes.extend_from_slice(text.as_bytes()),
        Encoding::Utf16 | Encoding::Utf16Le | Encoding::Utf16Be => {
            if encoding == Encoding::Utf16 {
                bytes.extend_from_slice(&[0xFF, 0xFE]);
            }
            for unit in text.encode_utf16() {
                let pair = if encoding == Encoding::Utf16Be {
                    unit.to_be_bytes()
                } else {
                    unit.to_le_bytes()
                };
                bytes.extend_from_slice(&pair);
            }
        }
        Encoding::Latin1 | Encoding::Ascii => {
            let limit = if encoding == Encoding::Ascii {
                0x7F
            } else {
                0xFF
            };
            for character in text.chars() {
                if character as u32 > limit {
                    return Err(unencodable(character, encoding));
                }
                bytes.push(character as u8);
            }
        }
        Encoding::Windows1252 => {
            for character in text.chars() {
                let code = character as u32;
                let byte = if code < 0x80 || (0xA0..=0xFF).contains(&code) {
                    Some(code as u8)
                } else {
                    WINDOWS_1252
                        .iter()
                        .position(|&c| c == code && c != 0)
                        .map(|index| 0x80 + index as u8)
                };
                bytes.push(byte.ok_or_else(|| unencodable(character, encoding))?);
            }
        }
    }
    Ok(bytes)
}

fn invalid(encoding: Encoding, position: usize) -> HostError {
    (
        "EncodingError",
        format!(
            "the bytes are not valid {} from byte {position}",
            encoding.name()
        ),
    )
}

/// Bytes of an encoding as text. Bytes that are not valid are an error,
/// or with `lossy` the replacement character.
pub fn decode(bytes: &[u8], encoding: Encoding, lossy: bool) -> Result<String, HostError> {
    let mut decoder = Decoder::new(encoding, lossy);
    let mut text = decoder.feed(bytes)?;
    text.push_str(&decoder.finish()?);
    Ok(text)
}

/// Decodes bytes that come a part at a time, keeping a character cut in
/// two by a part until the next one.
pub struct Decoder {
    encoding: Encoding,
    lossy: bool,
    /// Bytes of a character not yet complete.
    pending: Vec<u8>,
    /// For UTF-16: the byte order, once the first bytes have shown it.
    big_endian: Option<bool>,
    /// Bytes decoded so far, for the position in messages.
    consumed: usize,
}

impl Decoder {
    pub fn new(encoding: Encoding, lossy: bool) -> Decoder {
        let big_endian = match encoding {
            Encoding::Utf16Le => Some(false),
            Encoding::Utf16Be => Some(true),
            _ => None,
        };
        Decoder {
            encoding,
            lossy,
            pending: Vec::new(),
            big_endian,
            consumed: 0,
        }
    }

    /// The text of the next part of the bytes.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<String, HostError> {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(bytes);
        let mut text = String::with_capacity(data.len());
        let used = match self.encoding {
            Encoding::Utf8 => self.utf8(&data, &mut text)?,
            Encoding::Utf16 | Encoding::Utf16Le | Encoding::Utf16Be => {
                self.utf16(&data, &mut text)?
            }
            Encoding::Latin1 => {
                text.extend(data.iter().map(|&b| b as char));
                data.len()
            }
            Encoding::Ascii | Encoding::Windows1252 => self.single(&data, &mut text)?,
        };
        self.consumed += used;
        self.pending = data[used..].to_vec();
        Ok(text)
    }

    /// The end of the bytes: a character still incomplete is an error, or
    /// with `lossy` the replacement character.
    pub fn finish(&mut self) -> Result<String, HostError> {
        if self.pending.is_empty() {
            return Ok(String::new());
        }
        if self.lossy {
            self.pending.clear();
            return Ok('\u{FFFD}'.to_string());
        }
        Err(invalid(self.encoding, self.consumed))
    }

    fn utf8(&mut self, data: &[u8], text: &mut String) -> Result<usize, HostError> {
        let complete = crate::text::char_boundary(data, data.len());
        match std::str::from_utf8(&data[..complete]) {
            Ok(valid) => text.push_str(valid),
            Err(error) if self.lossy => {
                let _ = error;
                text.push_str(&String::from_utf8_lossy(&data[..complete]));
            }
            Err(error) => {
                return Err(invalid(self.encoding, self.consumed + error.valid_up_to()));
            }
        }
        Ok(complete)
    }

    fn single(&mut self, data: &[u8], text: &mut String) -> Result<usize, HostError> {
        for (index, &byte) in data.iter().enumerate() {
            let code = match (self.encoding, byte) {
                (_, 0x00..=0x7F) => Some(byte as u32),
                (Encoding::Windows1252, 0x80..=0x9F) => {
                    Some(WINDOWS_1252[(byte - 0x80) as usize]).filter(|&c| c != 0)
                }
                (Encoding::Windows1252, _) => Some(byte as u32),
                _ => None,
            };
            match code.and_then(char::from_u32) {
                Some(character) => text.push(character),
                None if self.lossy => text.push('\u{FFFD}'),
                None => return Err(invalid(self.encoding, self.consumed + index)),
            }
        }
        Ok(data.len())
    }

    fn utf16(&mut self, data: &[u8], text: &mut String) -> Result<usize, HostError> {
        let mut start = 0;
        if self.big_endian.is_none() {
            if data.len() < 2 {
                return Ok(0);
            }
            // A byte-order mark says the order, and is not text.
            self.big_endian = Some(match (data[0], data[1]) {
                (0xFE, 0xFF) => {
                    start = 2;
                    true
                }
                (0xFF, 0xFE) => {
                    start = 2;
                    false
                }
                _ => false,
            });
        }
        let big = self.big_endian == Some(true);
        let unit = |at: usize| {
            let pair = [data[at], data[at + 1]];
            if big {
                u16::from_be_bytes(pair)
            } else {
                u16::from_le_bytes(pair)
            }
        };
        let mut at = start;
        while at + 1 < data.len() {
            let first = unit(at);
            if (0xD800..0xDC00).contains(&first) {
                if at + 3 >= data.len() {
                    break;
                }
                let second = unit(at + 2);
                if (0xDC00..0xE000).contains(&second) {
                    let code = 0x10000 + ((first as u32 - 0xD800) << 10) + (second as u32 - 0xDC00);
                    text.push(char::from_u32(code).expect("a pair of surrogates"));
                    at += 4;
                    continue;
                }
            }
            match char::from_u32(first as u32) {
                Some(character) => text.push(character),
                None if self.lossy => text.push('\u{FFFD}'),
                None => return Err(invalid(self.encoding, self.consumed + at)),
            }
            at += 2;
        }
        Ok(at)
    }
}

/// Reads a file in an encoding as UTF-8, for the stream table's readers.
pub struct DecodeReader<R> {
    inner: R,
    decoder: Decoder,
    /// Decoded bytes not yet read.
    out: Vec<u8>,
    ended: bool,
}

impl<R: std::io::Read> DecodeReader<R> {
    pub fn new(inner: R, encoding: Encoding) -> DecodeReader<R> {
        DecodeReader {
            inner,
            decoder: Decoder::new(encoding, false),
            out: Vec::new(),
            ended: false,
        }
    }
}

impl<R: std::io::Read> std::io::Read for DecodeReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let failed =
            |(_, message): HostError| std::io::Error::new(std::io::ErrorKind::InvalidData, message);
        while self.out.is_empty() && !self.ended {
            let mut raw = [0u8; 16 * 1024];
            let count = self.inner.read(&mut raw)?;
            let text = if count == 0 {
                self.ended = true;
                self.decoder.finish().map_err(failed)?
            } else {
                self.decoder.feed(&raw[..count]).map_err(failed)?
            };
            self.out.extend_from_slice(text.as_bytes());
        }
        let count = buffer.len().min(self.out.len());
        buffer[..count].copy_from_slice(&self.out[..count]);
        self.out.drain(..count);
        Ok(count)
    }
}
