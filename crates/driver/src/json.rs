//! JSON, as the language server protocol's messages and the package
//! registry's answers are written: values, parsed from text and written back.

use std::fmt::Write;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    /// Members in the order they were written.
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn object(members: Vec<(&str, Json)>) -> Json {
        Json::Object(
            members
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect(),
        )
    }

    pub fn string(text: impl Into<String>) -> Json {
        Json::String(text.into())
    }

    pub fn number(value: impl Into<f64>) -> Json {
        Json::Number(value.into())
    }

    /// The member `key` of an object; `Null` for anything else.
    pub fn get(&self, key: &str) -> &Json {
        match self {
            Json::Object(members) => members
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value)
                .unwrap_or(&Json::Null),
            _ => &Json::Null,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_u32(&self) -> Option<u32> {
        match self {
            Json::Number(value) if *value >= 0.0 && *value <= u32::MAX as f64 => {
                Some(*value as u32)
            }
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> &[Json] {
        match self {
            Json::Array(items) => items,
            _ => &[],
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    /// Parses a whole text as one value.
    pub fn parse(text: &str) -> Result<Json, String> {
        let mut parser = Parser {
            bytes: text.as_bytes(),
            at: 0,
        };
        let value = parser.value(0)?;
        parser.space();
        if parser.at != parser.bytes.len() {
            return Err(format!("unexpected text at byte {}", parser.at));
        }
        Ok(value)
    }
}

impl std::fmt::Display for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Json::Null => f.write_str("null"),
            Json::Bool(value) => write!(f, "{value}"),
            Json::Number(value) => {
                if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 {
                    write!(f, "{}", *value as i64)
                } else if value.is_finite() {
                    write!(f, "{value}")
                } else {
                    f.write_str("null")
                }
            }
            Json::String(text) => write_string(f, text),
            Json::Array(items) => {
                f.write_char('[')?;
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        f.write_char(',')?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_char(']')
            }
            Json::Object(members) => {
                f.write_char('{')?;
                for (index, (key, value)) in members.iter().enumerate() {
                    if index > 0 {
                        f.write_char(',')?;
                    }
                    write_string(f, key)?;
                    write!(f, ":{value}")?;
                }
                f.write_char('}')
            }
        }
    }
}

fn write_string(f: &mut std::fmt::Formatter<'_>, text: &str) -> std::fmt::Result {
    f.write_char('"')?;
    for c in text.chars() {
        match c {
            '"' => f.write_str("\\\"")?,
            '\\' => f.write_str("\\\\")?,
            '\n' => f.write_str("\\n")?,
            '\r' => f.write_str("\\r")?,
            '\t' => f.write_str("\\t")?,
            c if (c as u32) < 0x20 => write!(f, "\\u{:04x}", c as u32)?,
            c => f.write_char(c)?,
        }
    }
    f.write_char('"')
}

/// How deep arrays and objects may nest.
const DEEPEST: usize = 128;

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn space(&mut self) {
        while self
            .bytes
            .get(self.at)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.at += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        self.space();
        if self.bytes.get(self.at) == Some(&byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("expected `{}` at byte {}", byte as char, self.at))
        }
    }

    fn word(&mut self, word: &str, value: Json) -> Result<Json, String> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(format!("unexpected text at byte {}", self.at))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > DEEPEST {
            return Err("values nest too deeply".to_string());
        }
        self.space();
        match self.bytes.get(self.at) {
            None => Err("the text ends early".to_string()),
            Some(b'n') => self.word("null", Json::Null),
            Some(b't') => self.word("true", Json::Bool(true)),
            Some(b'f') => self.word("false", Json::Bool(false)),
            Some(b'"') => self.string().map(Json::String),
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                self.space();
                if self.bytes.get(self.at) == Some(&b']') {
                    self.at += 1;
                    return Ok(Json::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.space();
                    match self.bytes.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b']') => {
                            self.at += 1;
                            return Ok(Json::Array(items));
                        }
                        _ => return Err(format!("expected `,` or `]` at byte {}", self.at)),
                    }
                }
            }
            Some(b'{') => {
                self.at += 1;
                let mut members = Vec::new();
                self.space();
                if self.bytes.get(self.at) == Some(&b'}') {
                    self.at += 1;
                    return Ok(Json::Object(members));
                }
                loop {
                    self.space();
                    if self.bytes.get(self.at) != Some(&b'"') {
                        return Err(format!("expected a member's name at byte {}", self.at));
                    }
                    let key = self.string()?;
                    self.expect(b':')?;
                    let value = self.value(depth + 1)?;
                    members.push((key, value));
                    self.space();
                    match self.bytes.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b'}') => {
                            self.at += 1;
                            return Ok(Json::Object(members));
                        }
                        _ => return Err(format!("expected `,` or `}}` at byte {}", self.at)),
                    }
                }
            }
            Some(_) => self.number(),
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.at;
        while self
            .bytes
            .get(self.at)
            .is_some_and(|b| matches!(b, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'))
        {
            self.at += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.at])
            .ok()
            .and_then(|text| text.parse::<f64>().ok())
            .map(Json::Number)
            .ok_or_else(|| format!("unexpected text at byte {start}"))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .and_then(|d| std::str::from_utf8(d).ok())
            .and_then(|d| u32::from_str_radix(d, 16).ok())
            .ok_or_else(|| format!("expected four hexadecimal digits at byte {}", self.at))?;
        self.at += 4;
        Ok(digits)
    }

    fn string(&mut self) -> Result<String, String> {
        self.at += 1;
        let mut out = String::new();
        loop {
            let start = self.at;
            while self
                .bytes
                .get(self.at)
                .is_some_and(|b| *b != b'"' && *b != b'\\')
            {
                self.at += 1;
            }
            out.push_str(
                std::str::from_utf8(&self.bytes[start..self.at])
                    .map_err(|_| "a string is not UTF-8".to_string())?,
            );
            match self.bytes.get(self.at) {
                None => return Err("a string does not end".to_string()),
                Some(b'"') => {
                    self.at += 1;
                    return Ok(out);
                }
                Some(_) => {
                    self.at += 1;
                    let escape = *self
                        .bytes
                        .get(self.at)
                        .ok_or_else(|| "a string does not end".to_string())?;
                    self.at += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let first = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&first)
                                && self.bytes[self.at..].starts_with(b"\\u")
                            {
                                self.at += 2;
                                let second = self.hex4()?;
                                0x10000
                                    + ((first - 0xD800) << 10)
                                    + (second.wrapping_sub(0xDC00) & 0x3FF)
                            } else {
                                first
                            };
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                        }
                        _ => return Err(format!("unknown escape at byte {}", self.at - 1)),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_parse_and_print_again() {
        let text = r#"{"a":[1,2.5,-3e2,true,false,null],"b":"line\nbreak \"quoted\" é 😀","c":{}}"#;
        let value = Json::parse(text).unwrap();
        assert_eq!(value.get("a").as_array().len(), 6);
        assert_eq!(value.get("b").as_str(), Some("line\nbreak \"quoted\" é 😀"));
        assert_eq!(Json::parse(&value.to_string()).unwrap(), value);
        assert_eq!(Json::number(-300.0).to_string(), "-300");
        assert!(Json::parse("{\"a\":}").is_err());
        assert!(Json::parse("[1,2").is_err());
        assert!(Json::parse(&"[".repeat(1000)).is_err());
    }
}
