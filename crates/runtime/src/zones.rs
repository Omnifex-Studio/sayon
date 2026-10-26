//! Time zones for `standard/time`: the offset from UTC each zone of the
//! IANA time zone database has at each moment.
//!
//! The database comes from the system: on Windows, through the ICU library
//! Windows ships (`icu.dll`); on Android, from the one file that holds every
//! zone's TZif data (Bionic's `tzdata`); elsewhere, from the TZif files
//! under `/usr/share/zoneinfo`. On any system, `TZDIR` names another
//! directory of TZif files to use instead.

use crate::host::HostError;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

fn unknown(zone: &str) -> HostError {
    (
        "TimeZoneError",
        format!("\"{zone}\" is not a time zone this system knows"),
    )
}

/// The offset from UTC, in seconds east, that `zone` has at the moment
/// `seconds` after 1970-01-01T00:00:00Z.
pub fn offset(zone: &str, seconds: i64) -> Result<i64, HostError> {
    if let Some(directory) = zone_directory() {
        return tzif_zone(&directory, zone).map(|rules| rules.offset(seconds));
    }
    platform::offset(zone, seconds)
}

/// Fails with `TimeZoneError` for a zone the system does not know.
pub fn check(zone: &str) -> Result<(), HostError> {
    offset(zone, 0).map(|_| ())
}

/// The name of the system's own time zone, or "UTC" when it has none.
pub fn local_zone() -> String {
    if zone_directory().is_some() || !cfg!(any(windows, target_os = "android")) {
        return unix_local_zone();
    }
    platform::local_zone().unwrap_or_else(|| "UTC".to_string())
}

/// The version of the time zone database, such as "2025b", or "unknown".
pub fn version() -> String {
    if let Some(directory) = zone_directory() {
        return tzif_version(&directory);
    }
    platform::version().unwrap_or_else(|| "unknown".to_string())
}

/// The directory of TZif files, unless the system's library is used.
fn zone_directory() -> Option<String> {
    if let Some(directory) = std::env::var_os("TZDIR") {
        return Some(directory.to_string_lossy().into_owned());
    }
    (!cfg!(any(windows, target_os = "android"))).then(|| "/usr/share/zoneinfo".to_string())
}

fn unix_local_zone() -> String {
    if let Ok(zone) = std::env::var("TZ") {
        let zone = zone.trim_start_matches(':');
        if valid_name(zone) {
            return zone.to_string();
        }
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let text = target.to_string_lossy();
        if let Some((_, zone)) = text.split_once("zoneinfo/")
            && valid_name(zone)
        {
            return zone.to_string();
        }
    }
    "UTC".to_string()
}

fn tzif_version(directory: &str) -> String {
    let path = std::path::Path::new(directory);
    if let Ok(text) = std::fs::read_to_string(path.join("tzdata.zi"))
        && let Some(version) = text
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("# version "))
    {
        return version.trim().to_string();
    }
    if let Ok(text) = std::fs::read_to_string(path.join("+VERSION")) {
        return text.trim().to_string();
    }
    "unknown".to_string()
}

/// A zone name names a file below the directory, and nothing else.
fn valid_name(zone: &str) -> bool {
    !zone.is_empty()
        && zone.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'))
        })
}

static ZONES: LazyLock<Mutex<HashMap<String, Arc<Rules>>>> = LazyLock::new(Mutex::default);

fn tzif_zone(directory: &str, zone: &str) -> Result<Arc<Rules>, HostError> {
    if !valid_name(zone) {
        return Err(unknown(zone));
    }
    let key = format!("{directory}\u{0}{zone}");
    let mut zones = ZONES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(rules) = zones.get(&key) {
        return Ok(rules.clone());
    }
    let path = std::path::Path::new(directory).join(zone);
    let bytes = std::fs::read(&path).map_err(|_| unknown(zone))?;
    let rules = Arc::new(Rules::parse(&bytes).ok_or_else(|| unknown(zone))?);
    zones.insert(key, rules.clone());
    Ok(rules)
}

// ----- TZif files ----------------------------------------------------------------------------

/// The rules of one zone, as a TZif file holds them: transitions to new
/// offsets, then a POSIX rule for the times after the last of them.
pub struct Rules {
    transitions: Vec<(i64, i64)>,
    /// The offset before the first transition.
    initial: i64,
    rule: Option<PosixRule>,
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let slice = self.bytes.get(self.at..self.at + count)?;
        self.at += count;
        Some(slice)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }
}

/// The counts of a TZif header.
struct Counts {
    utc: usize,
    standard: usize,
    leap: usize,
    times: usize,
    types: usize,
    characters: usize,
}

fn header(reader: &mut Reader) -> Option<(u8, Counts)> {
    if reader.take(4)? != b"TZif" {
        return None;
    }
    let version = reader.take(1)?[0];
    reader.take(15)?;
    let mut count = || reader.u32().map(|n| n as usize);
    Some((
        version,
        Counts {
            utc: count()?,
            standard: count()?,
            leap: count()?,
            times: count()?,
            types: count()?,
            characters: count()?,
        },
    ))
}

impl Rules {
    pub fn parse(bytes: &[u8]) -> Option<Rules> {
        let mut reader = Reader { bytes, at: 0 };
        let (version, mut counts) = header(&mut reader)?;
        let mut wide = false;
        if version >= b'2' {
            // The first block, with 32-bit times, is for older readers.
            let size = counts.times * 5
                + counts.types * 6
                + counts.characters
                + counts.leap * 8
                + counts.standard
                + counts.utc;
            reader.take(size)?;
            counts = header(&mut reader)?.1;
            wide = true;
        }
        let mut times = Vec::with_capacity(counts.times);
        for _ in 0..counts.times {
            times.push(if wide {
                reader.i64()?
            } else {
                i64::from(reader.i32()?)
            });
        }
        let indexes = reader.take(counts.times)?.to_vec();
        let mut offsets = Vec::with_capacity(counts.types);
        for _ in 0..counts.types {
            let offset = i64::from(reader.i32()?);
            reader.take(2)?;
            offsets.push(offset);
        }
        if offsets.is_empty() {
            return None;
        }
        let transitions = times
            .iter()
            .zip(&indexes)
            .map(|(&time, &index)| Some((time, *offsets.get(index as usize)?)))
            .collect::<Option<Vec<_>>>()?;
        let mut rule = None;
        if wide {
            let size = counts.characters + counts.leap * 12 + counts.standard + counts.utc;
            reader.take(size)?;
            let footer = &bytes[reader.at.min(bytes.len())..];
            if let Some(text) = std::str::from_utf8(footer)
                .ok()
                .and_then(|t| t.strip_prefix('\n'))
            {
                let text = text.trim_end_matches('\n');
                if !text.is_empty() {
                    rule = PosixRule::parse(text);
                }
            }
        }
        Some(Rules {
            transitions,
            initial: offsets[0],
            rule,
        })
    }

    pub fn offset(&self, seconds: i64) -> i64 {
        match self.transitions.last() {
            Some(&(last, _)) if seconds >= last && self.rule.is_some() => {
                self.rule.as_ref().expect("a rule").offset(seconds)
            }
            _ => {
                let after = self
                    .transitions
                    .partition_point(|&(time, _)| time <= seconds);
                match after {
                    0 => match &self.rule {
                        Some(rule) if self.transitions.is_empty() => rule.offset(seconds),
                        _ => self.initial,
                    },
                    n => self.transitions[n - 1].1,
                }
            }
        }
    }
}

// ----- POSIX TZ rules --------------------------------------------------------------------------

/// A rule such as `CET-1CEST,M3.5.0,M10.5.0/3`: standard time, and summer
/// time between two days of each year.
pub struct PosixRule {
    /// Seconds east of UTC.
    standard: i64,
    summer: Option<Summer>,
}

struct Summer {
    offset: i64,
    start: (Day, i64),
    end: (Day, i64),
}

#[derive(Clone, Copy)]
enum Day {
    /// `Jn`: day 1 to 365, never counting February 29.
    Julian(i64),
    /// `n`: day 0 to 365, counting February 29.
    Ordinal(i64),
    /// `Mm.w.d`: day `d` (0 is Sunday) of week `w` (5 is the last) of
    /// month `m`.
    Month(i64, i64, i64),
}

struct Text<'a> {
    rest: &'a str,
}

impl<'a> Text<'a> {
    fn peek(&self) -> Option<char> {
        self.rest.chars().next()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.rest = &self.rest[c.len_utf8()..];
            true
        } else {
            false
        }
    }

    fn name(&mut self) -> Option<()> {
        if self.eat('<') {
            let end = self.rest.find('>')?;
            self.rest = &self.rest[end + 1..];
            return Some(());
        }
        let length = self
            .rest
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .count();
        if length < 3 {
            return None;
        }
        self.rest = &self.rest[length..];
        Some(())
    }

    fn number(&mut self) -> Option<i64> {
        let length = self.rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if length == 0 {
            return None;
        }
        let value = self.rest[..length].parse().ok()?;
        self.rest = &self.rest[length..];
        Some(value)
    }

    /// `[+-]hh[:mm[:ss]]` as seconds.
    fn time(&mut self) -> Option<i64> {
        let sign = if self.eat('-') {
            -1
        } else {
            self.eat('+');
            1
        };
        let mut seconds = self.number()? * 3600;
        if self.eat(':') {
            seconds += self.number()? * 60;
            if self.eat(':') {
                seconds += self.number()?;
            }
        }
        Some(sign * seconds)
    }

    fn day(&mut self) -> Option<Day> {
        if self.eat('J') {
            return Some(Day::Julian(self.number()?));
        }
        if self.eat('M') {
            let month = self.number()?;
            self.eat('.').then_some(())?;
            let week = self.number()?;
            self.eat('.').then_some(())?;
            return Some(Day::Month(month, week, self.number()?));
        }
        Some(Day::Ordinal(self.number()?))
    }

    /// A day, and the local time of day the change happens: 02:00 unless
    /// given.
    fn change(&mut self) -> Option<(Day, i64)> {
        let day = self.day()?;
        let time = if self.eat('/') { self.time()? } else { 7200 };
        Some((day, time))
    }
}

impl PosixRule {
    pub fn parse(text: &str) -> Option<PosixRule> {
        let mut text = Text { rest: text };
        text.name()?;
        // POSIX counts west of UTC.
        let standard = -text.time()?;
        if text.rest.is_empty() {
            return Some(PosixRule {
                standard,
                summer: None,
            });
        }
        text.name()?;
        let offset = match text.peek() {
            Some(',') => standard + 3600,
            _ => -text.time()?,
        };
        text.eat(',').then_some(())?;
        let start = text.change()?;
        text.eat(',').then_some(())?;
        let end = text.change()?;
        text.rest.is_empty().then_some(())?;
        Some(PosixRule {
            standard,
            summer: Some(Summer { offset, start, end }),
        })
    }

    pub fn offset(&self, seconds: i64) -> i64 {
        let Some(summer) = &self.summer else {
            return self.standard;
        };
        let year = civil_from_days((seconds + self.standard).div_euclid(86_400)).0;
        // When summer time starts, in UTC: its local standard time, and
        // when it ends: its local summer time.
        let start = change_time(year, summer.start) - self.standard;
        let end = change_time(year, summer.end) - summer.offset;
        let in_summer = if start < end {
            seconds >= start && seconds < end
        } else {
            !(seconds >= end && seconds < start)
        };
        if in_summer {
            summer.offset
        } else {
            self.standard
        }
    }
}

/// The seconds from 1970 to a day of a year at a time of day, all local.
fn change_time(year: i64, (day, time): (Day, i64)) -> i64 {
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let first = days_from_civil(year, 1, 1);
    let days = match day {
        Day::Julian(n) => first + n - 1 + i64::from(leap && n >= 60),
        Day::Ordinal(n) => first + n,
        Day::Month(month, week, weekday) => {
            let start = days_from_civil(year, month, 1);
            // 1970-01-01 was a Thursday, weekday 4.
            let first_weekday = (start + 4).rem_euclid(7);
            let mut day = start + (weekday - first_weekday).rem_euclid(7) + (week - 1) * 7;
            let next_month = if month == 12 {
                days_from_civil(year + 1, 1, 1)
            } else {
                days_from_civil(year, month + 1, 1)
            };
            while day >= next_month {
                day -= 7;
            }
            day
        }
    };
    days * 86_400 + time
}

/// Days from 1970-01-01 to a date of the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = of_era * 365 + of_era / 4 - of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The year, month, and day of a count of days from 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

// ----- the system's library ------------------------------------------------------------------

#[cfg(windows)]
mod platform {
    use super::unknown;
    use crate::host::HostError;
    use std::ffi::{c_char, c_void};
    use std::sync::OnceLock;

    type Open = unsafe extern "C" fn(*const u16, i32, *const c_char, i32, *mut i32) -> *mut c_void;
    type SetMillis = unsafe extern "C" fn(*mut c_void, f64, *mut i32);
    type Get = unsafe extern "C" fn(*const c_void, i32, *mut i32) -> i32;
    type Close = unsafe extern "C" fn(*mut c_void);
    type Canonical = unsafe extern "C" fn(*const u16, i32, *mut u16, i32, *mut i8, *mut i32) -> i32;
    type Default = unsafe extern "C" fn(*mut u16, i32, *mut i32) -> i32;
    type Version = unsafe extern "C" fn(*mut i32) -> *const c_char;

    struct Icu {
        open: Open,
        set_millis: SetMillis,
        get: Get,
        close: Close,
        canonical: Canonical,
        default: Default,
        version: Version,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    fn icu() -> Option<&'static Icu> {
        static ICU: OnceLock<Option<Icu>> = OnceLock::new();
        ICU.get_or_init(|| {
            let name: Vec<u16> = "icu.dll".encode_utf16().chain(Some(0)).collect();
            // SAFETY: a NUL-terminated name; the symbols, when found, have
            // the types of ICU's C interface.
            unsafe {
                let module = LoadLibraryW(name.as_ptr());
                if module.is_null() {
                    return None;
                }
                let symbol = |name: &str| {
                    let name = format!("{name}\0");
                    let address = GetProcAddress(module, name.as_ptr());
                    (!address.is_null()).then_some(address)
                };
                Some(Icu {
                    open: std::mem::transmute::<*mut c_void, Open>(symbol("ucal_open")?),
                    set_millis: std::mem::transmute::<*mut c_void, SetMillis>(symbol(
                        "ucal_setMillis",
                    )?),
                    get: std::mem::transmute::<*mut c_void, Get>(symbol("ucal_get")?),
                    close: std::mem::transmute::<*mut c_void, Close>(symbol("ucal_close")?),
                    canonical: std::mem::transmute::<*mut c_void, Canonical>(symbol(
                        "ucal_getCanonicalTimeZoneID",
                    )?),
                    default: std::mem::transmute::<*mut c_void, Default>(symbol(
                        "ucal_getDefaultTimeZone",
                    )?),
                    version: std::mem::transmute::<*mut c_void, Version>(symbol(
                        "ucal_getTZDataVersion",
                    )?),
                })
            }
        })
        .as_ref()
    }

    fn missing() -> HostError {
        (
            "TimeZoneError",
            "this system has no time zone database (Windows's icu.dll)".to_string(),
        )
    }

    const GREGORIAN: i32 = 1;
    const ZONE_OFFSET: i32 = 15;
    const DST_OFFSET: i32 = 16;

    pub fn offset(zone: &str, seconds: i64) -> Result<i64, HostError> {
        let icu = icu().ok_or_else(missing)?;
        let id: Vec<u16> = zone.encode_utf16().collect();
        let mut status = 0;
        let mut canonical = [0u16; 128];
        let mut system = 0i8;
        // SAFETY: the buffers are as long as the counts given.
        unsafe {
            (icu.canonical)(
                id.as_ptr(),
                id.len() as i32,
                canonical.as_mut_ptr(),
                canonical.len() as i32,
                &mut system,
                &mut status,
            );
            if status > 0 || system == 0 {
                return Err(unknown(zone));
            }
            let calendar = (icu.open)(
                id.as_ptr(),
                id.len() as i32,
                c"en_US".as_ptr(),
                GREGORIAN,
                &mut status,
            );
            if status > 0 || calendar.is_null() {
                return Err(unknown(zone));
            }
            (icu.set_millis)(calendar, seconds as f64 * 1000.0, &mut status);
            let standard = (icu.get)(calendar, ZONE_OFFSET, &mut status);
            let summer = (icu.get)(calendar, DST_OFFSET, &mut status);
            (icu.close)(calendar);
            if status > 0 {
                return Err(unknown(zone));
            }
            Ok((i64::from(standard) + i64::from(summer)) / 1000)
        }
    }

    pub fn local_zone() -> Option<String> {
        let icu = icu()?;
        let mut buffer = [0u16; 128];
        let mut status = 0;
        // SAFETY: the buffer is as long as the count given.
        let length =
            unsafe { (icu.default)(buffer.as_mut_ptr(), buffer.len() as i32, &mut status) };
        if status > 0 || length <= 0 {
            return None;
        }
        String::from_utf16(&buffer[..length as usize]).ok()
    }

    pub fn version() -> Option<String> {
        let icu = icu()?;
        let mut status = 0;
        // SAFETY: ICU returns a static NUL-terminated string.
        unsafe {
            let text = (icu.version)(&mut status);
            if status > 0 || text.is_null() {
                return None;
            }
            Some(
                std::ffi::CStr::from_ptr(text)
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }
}

/// Android: the database is one file, an index of the zones' names before
/// their TZif data, as Bionic reads it. The newest is the time zone
/// module's (an APEX, since Android 10), then one an update left, then the
/// system's own. The phone's own zone is a system property.
#[cfg(target_os = "android")]
mod platform {
    use super::{Rules, ZONES, unknown, valid_name};
    use crate::host::HostError;
    use std::collections::HashMap;
    use std::sync::{Arc, OnceLock};

    /// The database: its version, its bytes, and each zone's data in them.
    struct Database {
        version: String,
        bytes: Vec<u8>,
        zones: HashMap<String, (usize, usize)>,
    }

    /// Each entry of the index: the zone's name in 40 bytes, then where its
    /// data starts, after the data's own start, and how long it is.
    const ENTRY: usize = 52;
    const NAME: usize = 40;

    fn files() -> Vec<String> {
        let place =
            |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());
        vec![
            format!(
                "{}/etc/tz/tzdata",
                place("ANDROID_TZDATA_ROOT", "/apex/com.android.tzdata")
            ),
            format!(
                "{}/misc/zoneinfo/current/tzdata",
                place("ANDROID_DATA", "/data")
            ),
            format!(
                "{}/usr/share/zoneinfo/tzdata",
                place("ANDROID_ROOT", "/system")
            ),
        ]
    }

    fn be32(bytes: &[u8], at: usize) -> Option<usize> {
        let word = bytes.get(at..at + 4)?;
        usize::try_from(i32::from_be_bytes([word[0], word[1], word[2], word[3]])).ok()
    }

    /// Reads a database: "tzdata" and its version, such as "2025b", then
    /// where the index and the data start.
    fn read(bytes: Vec<u8>) -> Option<Database> {
        if bytes.get(..6)? != b"tzdata" {
            return None;
        }
        let version = String::from_utf8_lossy(bytes.get(6..11)?).into_owned();
        let index = be32(&bytes, 12)?;
        let data = be32(&bytes, 16)?;
        let mut zones = HashMap::new();
        let mut at = index;
        while at + ENTRY <= data && at + ENTRY <= bytes.len() {
            let field = &bytes[at..at + NAME];
            let end = field.iter().position(|&b| b == 0).unwrap_or(NAME);
            let name = String::from_utf8_lossy(&field[..end]).into_owned();
            let start = data + be32(&bytes, at + NAME)?;
            let length = be32(&bytes, at + NAME + 4)?;
            if start + length <= bytes.len() {
                zones.insert(name, (start, length));
            }
            at += ENTRY;
        }
        Some(Database {
            version,
            bytes,
            zones,
        })
    }

    fn database() -> Option<&'static Database> {
        static DATABASE: OnceLock<Option<Database>> = OnceLock::new();
        DATABASE
            .get_or_init(|| {
                files()
                    .into_iter()
                    .find_map(|path| read(std::fs::read(path).ok()?))
            })
            .as_ref()
    }

    pub fn offset(zone: &str, seconds: i64) -> Result<i64, HostError> {
        if !valid_name(zone) {
            return Err(unknown(zone));
        }
        let key = format!("tzdata\u{0}{zone}");
        let mut zones = ZONES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rules) = zones.get(&key) {
            return Ok(rules.offset(seconds));
        }
        let database = database().ok_or_else(|| unknown(zone))?;
        let &(start, length) = database.zones.get(zone).ok_or_else(|| unknown(zone))?;
        let bytes = &database.bytes[start..start + length];
        let rules = Arc::new(Rules::parse(bytes).ok_or_else(|| unknown(zone))?);
        zones.insert(key, rules.clone());
        Ok(rules.offset(seconds))
    }

    unsafe extern "C" {
        fn __system_property_get(
            name: *const std::ffi::c_char,
            value: *mut std::ffi::c_char,
        ) -> i32;
    }

    /// `TZ` when it names a zone, or the phone's own.
    pub fn local_zone() -> Option<String> {
        if let Ok(zone) = std::env::var("TZ") {
            let zone = zone.trim_start_matches(':');
            if valid_name(zone) {
                return Some(zone.to_string());
            }
        }
        // A property's value is at most 91 bytes and a NUL.
        let mut value = [0 as std::ffi::c_char; 92];
        let name = c"persist.sys.timezone";
        // SAFETY: a NUL-terminated name, and room for the longest value.
        let length = unsafe { __system_property_get(name.as_ptr(), value.as_mut_ptr()) };
        if length <= 0 {
            return None;
        }
        // SAFETY: the property's value, NUL-terminated.
        let zone = unsafe { std::ffi::CStr::from_ptr(value.as_ptr()) };
        let zone = zone.to_string_lossy().into_owned();
        valid_name(&zone).then_some(zone)
    }

    pub fn version() -> Option<String> {
        database().map(|database| database.version.clone())
    }
}

#[cfg(not(any(windows, target_os = "android")))]
mod platform {
    use crate::host::HostError;

    pub fn offset(zone: &str, _seconds: i64) -> Result<i64, HostError> {
        Err(super::unknown(zone))
    }

    pub fn local_zone() -> Option<String> {
        None
    }

    pub fn version() -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_count_days() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn posix_rules_give_offsets() {
        let paris = PosixRule::parse("CET-1CEST,M3.5.0,M10.5.0/3").expect("rule");
        // 2026-03-29 is the last Sunday of March: 01:00 UTC is 03:00 CEST.
        let change = days_from_civil(2026, 3, 29) * 86_400 + 3600;
        assert_eq!(paris.offset(change - 1), 3600);
        assert_eq!(paris.offset(change), 7200);
        // 2026-10-25, the last Sunday of October, back at 01:00 UTC.
        let back = days_from_civil(2026, 10, 25) * 86_400 + 3600;
        assert_eq!(paris.offset(back - 1), 7200);
        assert_eq!(paris.offset(back), 3600);
        let sydney = PosixRule::parse("AEST-10AEDT,M10.1.0,M4.1.0/3").expect("rule");
        assert_eq!(sydney.offset(days_from_civil(2026, 1, 15) * 86_400), 39_600);
        assert_eq!(sydney.offset(days_from_civil(2026, 7, 15) * 86_400), 36_000);
        let fixed = PosixRule::parse("<+0530>-5:30").expect("rule");
        assert_eq!(fixed.offset(0), 19_800);
        assert_eq!(PosixRule::parse("UTC0").expect("rule").offset(0), 0);
        let new_york = PosixRule::parse("EST5EDT,M3.2.0,M11.1.0").expect("rule");
        assert_eq!(
            new_york.offset(days_from_civil(2026, 7, 4) * 86_400),
            -14_400
        );
        assert_eq!(
            new_york.offset(days_from_civil(2026, 12, 25) * 86_400),
            -18_000
        );
    }

    #[test]
    fn zone_names_stay_below_the_directory() {
        assert!(valid_name("Europe/Paris"));
        assert!(valid_name("Etc/GMT+5"));
        assert!(!valid_name("../etc/passwd"));
        assert!(!valid_name("/absolute"));
        assert!(!valid_name(""));
    }
}
