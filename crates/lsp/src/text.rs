//! Where things are, as the protocol says it: files by `file:` URI, and
//! places by line and character, the characters counted in UTF-16 code
//! units, as the protocol does unless both sides agree otherwise.

use std::path::{Path, PathBuf};

/// The byte offset of a line and character in `text`. A character past the
/// end of its line is the line's end, and a line past the last is the text's
/// end.
pub fn offset(text: &str, line: u32, character: u32) -> usize {
    let mut start = 0;
    for _ in 0..line {
        match text[start..].find('\n') {
            Some(newline) => start += newline + 1,
            None => return text.len(),
        }
    }
    let end = text[start..].find('\n').map_or(text.len(), |n| start + n);
    let line_text = text[start..end].trim_end_matches('\r');
    let mut units = 0;
    for (index, c) in line_text.char_indices() {
        if units >= character {
            return start + index;
        }
        units += c.len_utf16() as u32;
    }
    start + line_text.len()
}

/// The line and character of a byte offset in `text`.
pub fn position(text: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(text.len());
    let before = &text[..floor_boundary(text, offset)];
    let line = before.matches('\n').count() as u32;
    let line_start = before.rfind('\n').map_or(0, |n| n + 1);
    let character = before[line_start..].encode_utf16().count() as u32;
    (line, character)
}

fn floor_boundary(text: &str, mut offset: usize) -> usize {
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// The `file:` URI of a path, written as editors write it: forward slashes,
/// and every byte but letters, digits, and `-._~/` escaped.
pub fn uri_of(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    // Windows' long paths, as canonicalizing gives them.
    let text = text
        .strip_prefix("//?/UNC/")
        .map(|rest| format!("//{rest}"))
        .or_else(|| text.strip_prefix("//?/").map(str::to_string))
        .unwrap_or(text);
    let mut uri = String::from("file://");
    if !text.starts_with('/') {
        uri.push('/');
    }
    for (index, byte) in text.bytes().enumerate() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(byte as char)
            }
            // A drive's colon stays, as `file:///c:/`.
            b':' if index == 1 => uri.push(':'),
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

/// The path a `file:` URI names.
pub fn path_of(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let mut bytes = Vec::new();
    let raw = rest.as_bytes();
    let mut at = 0;
    while at < raw.len() {
        if raw[at] == b'%'
            && let Some(byte) = rest
                .get(at + 1..at + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            bytes.push(byte);
            at += 3;
            continue;
        }
        bytes.push(raw[at]);
        at += 1;
    }
    let mut text = String::from_utf8(bytes).ok()?;
    // `/c:/Users/...` is a Windows path; `//server/share` a network one.
    let drive = text.as_bytes();
    if drive.len() >= 3 && drive[0] == b'/' && drive[1].is_ascii_alphabetic() && drive[2] == b':' {
        text.remove(0);
    } else if !text.starts_with('/') {
        text = format!("//{text}");
    }
    if cfg!(windows) {
        text = text.replace('/', "\\");
    }
    Some(PathBuf::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_count_utf16_units() {
        let text = "first\nsé😀x\r\nlast";
        assert_eq!(offset(text, 0, 2), 2);
        // "s", "é", the emoji's two units: "x" is the fifth.
        assert_eq!(offset(text, 1, 4), 6 + 1 + 2 + 4);
        assert_eq!(position(text, 6 + 1 + 2 + 4), (1, 4));
        assert_eq!(offset(text, 1, 99), 6 + 1 + 2 + 4 + 1);
        assert_eq!(offset(text, 9, 0), text.len());
        assert_eq!(position(text, text.len()), (2, 4));
    }

    #[test]
    fn paths_become_uris_and_back() {
        if cfg!(windows) {
            let path = Path::new(r"C:\Users\A B\sayon\main.sayon");
            assert_eq!(uri_of(path), "file:///C:/Users/A%20B/sayon/main.sayon");
            assert_eq!(
                path_of("file:///c%3A/Users/A%20B/sayon/main.sayon").unwrap(),
                PathBuf::from(r"c:\Users\A B\sayon\main.sayon")
            );
            assert_eq!(
                uri_of(Path::new(r"\\?\C:\work\x.sayon")),
                "file:///C:/work/x.sayon"
            );
        } else {
            let path = Path::new("/home/a b/é.sayon");
            assert_eq!(uri_of(path), "file:///home/a%20b/%C3%A9.sayon");
            assert_eq!(path_of(&uri_of(path)).unwrap(), path);
        }
    }
}
