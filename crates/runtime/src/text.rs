//! Text rules shared by the interpreter and compiled programs, so both
//! execution modes print and parse identically.

use std::fmt::Write as _;

pub fn format_f64(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        };
    }
    let magnitude = value.abs();
    if magnitude != 0.0 && !(1e-5..1e16).contains(&magnitude) {
        return format!("{value:e}");
    }
    if value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

pub fn format_f32(value: f32) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        };
    }
    let magnitude = value.abs();
    if magnitude != 0.0 && !(1e-5..1e16).contains(&magnitude) {
        return format!("{value:e}");
    }
    if value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

/// Writes `text` as a Sayon string literal, as strings appear when displayed
/// inside a collection.
pub fn quote(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            '{' => out.push_str("{{"),
            '}' => out.push_str("}}"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{{{:X}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A number parsed by `tonumber`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Number {
    Integer(i64),
    Float(f64),
}

/// Parses `tonumber` input. The whole text must be a decimal integer or a
/// decimal/exponent float; there is no trimming and no locale.
pub fn parse_number(text: &str) -> Result<Number, (&'static str, String)> {
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        return match text.parse::<i64>() {
            Ok(value) => Ok(Number::Integer(value)),
            Err(_) => Err((
                "ConversionError",
                format!("\"{text}\" is outside the integer range"),
            )),
        };
    }
    let valid_float = !digits.is_empty()
        && digits
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
        && digits
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_digit() || b == b'.');
    match text.parse::<f64>() {
        Ok(value) if valid_float && value.is_finite() => Ok(Number::Float(value)),
        Ok(_) if valid_float => Err((
            "ConversionError",
            format!("\"{text}\" is outside the float range"),
        )),
        _ => Err(("ParseError", format!("\"{text}\" is not a number"))),
    }
}

/// Number of values `range(start, end, step)` yields, computed without overflow.
/// The message for view bounds outside an array or view of `length` elements.
pub fn view_bounds_message(start: i128, end: i128, length: usize) -> String {
    if start > end {
        format!("a view cannot start at {start} and end before it, at {end}")
    } else {
        format!("the view from {start} to {end} is outside an array of length {length}")
    }
}

/// The message for a string position outside a string of `length`
/// characters.
pub fn position_error(position: i128, length: usize) -> String {
    if length == 0 {
        format!("position {position} is out of bounds: the string is empty")
    } else {
        format!("position {position} is out of bounds for a string of length {length}")
    }
}

pub fn range_length(start: i64, end: i64, step: i64) -> i64 {
    let (start, end, step) = (start as i128, end as i128, step as i128);
    let count = if step > 0 {
        if end <= start {
            0
        } else {
            (end - start + step - 1) / step
        }
    } else if end >= start {
        0
    } else {
        (start - end + (-step) - 1) / (-step)
    };
    count as i64
}

/// Canonical names of the integer types, indexed by the code generator's
/// integer type codes.
pub const INTEGER_TYPE_NAMES: [&str; 8] = [
    "integer8",
    "integer16",
    "integer32",
    "integer",
    "unsigned8",
    "unsigned16",
    "unsigned32",
    "unsigned",
];

/// The inclusive range of an integer type code, as i128.
pub fn integer_type_range(code: u32) -> (i128, i128) {
    match code {
        0 => (i8::MIN as i128, i8::MAX as i128),
        1 => (i16::MIN as i128, i16::MAX as i128),
        2 => (i32::MIN as i128, i32::MAX as i128),
        3 => (i64::MIN as i128, i64::MAX as i128),
        4 => (0, u8::MAX as i128),
        5 => (0, u16::MAX as i128),
        6 => (0, u32::MAX as i128),
        _ => (0, u64::MAX as i128),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_display_like_literals() {
        assert_eq!(format_f64(3.0), "3.0");
        assert_eq!(format_f64(2.75), "2.75");
        assert_eq!(format_f64(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(format_f64(1e20), "1e20");
        assert_eq!(format_f64(-0.5), "-0.5");
        assert_eq!(format_f64(f64::NAN), "nan");
        assert_eq!(format_f32(0.1), "0.1");
    }

    #[test]
    fn range_lengths() {
        assert_eq!(range_length(0, 10, 1), 10);
        assert_eq!(range_length(0, 10, 3), 4);
        assert_eq!(range_length(10, 0, -3), 4);
        assert_eq!(range_length(5, 5, 1), 0);
        assert_eq!(range_length(5, 0, 1), 0);
        assert_eq!(range_length(i64::MIN, i64::MAX, i64::MAX), 3);
    }

    #[test]
    fn number_parsing() {
        assert_eq!(parse_number("42"), Ok(Number::Integer(42)));
        assert_eq!(parse_number("-7"), Ok(Number::Integer(-7)));
        assert_eq!(parse_number("2.5"), Ok(Number::Float(2.5)));
        assert_eq!(parse_number("1e3"), Ok(Number::Float(1000.0)));
        assert!(parse_number(" 42").is_err());
        assert!(parse_number("12abc").is_err());
        assert!(parse_number("").is_err());
        assert!(parse_number("inf").is_err());
        assert!(matches!(
            parse_number("99999999999999999999"),
            Err(("ConversionError", _))
        ));
    }

    #[test]
    fn quoting_escapes_specials() {
        let mut out = String::new();
        quote("a\"b\n{c}", &mut out);
        assert_eq!(out, "\"a\\\"b\\n{{c}}\"");
    }
}
