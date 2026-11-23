//! zsh/datetime, after Src/Modules/datetime.c: the strftime builtin and
//! the EPOCHSECONDS, EPOCHREALTIME and epochtime parameters, with zsh's
//! own ztrftime() from Src/utils.c for its format extensions.

use crate::params::Value;
use crate::shell::builtin_err;
use std::ffi::{CStr, CString, OsString};

extern "C" {
    fn tzset();
}

fn now() -> (i64, i64) {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_secs() as i64, i64::from(d.subsec_nanos()))
}

/// The values of EPOCHSECONDS (integer), EPOCHREALTIME (float, shown as
/// zsh shows a -F parameter: ten decimals) and epochtime (seconds and
/// nanoseconds). None for any other name.
pub fn epoch_param(name: &str) -> Option<Value> {
    if !matches!(name, "EPOCHSECONDS" | "EPOCHREALTIME" | "epochtime") {
        return None;
    }
    let (s, ns) = now();
    Some(match name {
        "EPOCHSECONDS" => Value::Scalar(s.to_string()),
        "EPOCHREALTIME" => Value::Scalar(format!("{:.10}", s as f64 + ns as f64 * 1e-9)),
        _ => Value::Array(vec![s.to_string(), ns.to_string()]),
    })
}

/// Broken-down local time for `secs`, per the process's TZ.
pub fn localtime(secs: i64) -> Option<libc::tm> {
    // The platform's time_t: 32 bits on some targets.
    let t = secs as _;
    // SAFETY: tm is plain old data, so all-zero is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers refer to live locals.
    let r = unsafe { libc::localtime_r(&t, &mut tm) };
    (!r.is_null()).then_some(tm)
}

// zsh's ztrftimebuf(): takes `decr` bytes from the space left, keeping
// one for the terminating NUL zsh's buffer needs.
fn fits(left: &mut i64, decr: i64) -> bool {
    if *left <= decr {
        return false;
    }
    *left -= decr;
    true
}

fn digit(v: libc::c_int) -> u8 {
    (i32::from(b'0') + v) as u8
}

enum Spec {
    Done,
    Overflow,
    // strftime() produced nothing for a spec other than %p/%P: zsh then
    // returns an empty result.
    Empty,
}

// Hands one conversion (`%` through its letter, flags included) to the
// C library, within the space left.
fn via_strftime(spec: &[u8], tm: &libc::tm, left: &mut i64, out: &mut Vec<u8>) -> Spec {
    let origchar = spec.last().copied().unwrap_or(0);
    let nul = spec.iter().position(|&b| b == 0).unwrap_or(spec.len());
    let cspec = CString::new(&spec[..nul]).unwrap_or_default();
    // Two bytes were already counted for this conversion.
    let size = (*left + 2).max(1) as usize;
    let mut buf = vec![0u8; size];
    // zsh tells a failed conversion from an empty one by whether
    // strftime() touched this byte.
    buf[0] = 1;
    // SAFETY: buf has `size` writable bytes, cspec is NUL-terminated and
    // tm is a valid broken-down time.
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), size, cspec.as_ptr(), tm) };
    if n == 0 && (buf[0] != 0 || (origchar != b'p' && origchar != b'P')) {
        return if buf[0] != 0 {
            Spec::Overflow
        } else {
            Spec::Empty
        };
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(size);
    out.extend_from_slice(&buf[..len]);
    *left -= len as i64 - 2;
    Spec::Done
}

/// zsh's ztrftime(): strftime() with zsh's extensions (`%.`/`%N.` for
/// fractions of a second, `%N`, `%f`, `%K`, `%L`, and `-` to strip
/// padding from `%d %m %M %S %y %e %H %k %l`), the rest handed to the C
/// library. `bufsize` is the output space zsh would allocate, NUL
/// included; None when the result does not fit (zsh's -1).
pub fn ztrftime(fmt: &[u8], tm: &libc::tm, nsec: i64, bufsize: usize) -> Option<Vec<u8>> {
    let mut left = bufsize as i64;
    let mut out = Vec::new();
    let at = |k: usize| fmt.get(k).copied().unwrap_or(0);
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            if !fits(&mut left, 1) {
                return None;
            }
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        let mut strip = false;
        let mut digs: i64 = 3;
        if at(i) == b'-' {
            strip = true;
            i += 1;
        }
        if at(i).is_ascii_digit() {
            // A count is only meaningful before `.`.
            let mut dend = i + 1;
            while at(dend).is_ascii_digit() {
                dend += 1;
            }
            if at(dend) == b'.' {
                digs = fmt[i..dend].iter().fold(0i64, |n, &d| {
                    n.saturating_mul(10).saturating_add(i64::from(d - b'0'))
                });
                i = dend;
            }
        }
        if !fits(&mut left, 2) {
            return None;
        }
        // zsh's `morefmt` loop: flags and widths zsh does not implement
        // itself send the whole conversion to strftime(), so `None` here.
        let c = loop {
            if !(i - start == 1 || (i - start == 2 && strip) || at(i) == b'.') {
                while at(i) != 0 && b"OE^#_-0123456789".contains(&at(i)) {
                    i += 1;
                }
                if at(i) != 0 {
                    i += 1;
                    break None;
                }
            }
            let c = at(i);
            i += 1;
            if !matches!(c, b'E' | b'O' | b'^' | b'#' | b'_' | b'-' | b'0'..=b'9') {
                break Some(c);
            }
        };
        match c {
            Some(b'.') => {
                let digs = if (0..=9).contains(&digs) { digs } else { 9 };
                if !fits(&mut left, digs) {
                    return None;
                }
                let mut fnsec = nsec;
                if digs < 9 {
                    let mut max: i64 = 100_000_000;
                    for _ in 0..8 - digs {
                        max /= 10;
                        fnsec /= 10;
                    }
                    max -= 1;
                    fnsec = (fnsec + 5) / 10;
                    fnsec = fnsec.min(max);
                }
                let s = format!("{:0w$}", fnsec, w = digs as usize);
                // zsh advances by `digs` only, so `%0.` leaves nothing.
                out.extend_from_slice(&s.as_bytes()[..digs as usize]);
            }
            // Premature end of the format.
            Some(0) => {
                out.push(b'%');
                i -= 1;
            }
            Some(c @ (b'e' | b'f')) => {
                let strip = strip || c == b'f';
                let d = tm.tm_mday;
                if d > 9 {
                    out.push(digit(d / 10));
                } else if !strip {
                    out.push(b' ');
                }
                out.push(digit(d % 10));
            }
            Some(c @ (b'H' | b'k' | b'K')) => {
                let strip = strip || c == b'K';
                let h = tm.tm_hour;
                if h > 9 {
                    out.push(digit(h / 10));
                } else if !strip {
                    out.push(if c == b'H' { b'0' } else { b' ' });
                }
                out.push(digit(h % 10));
            }
            Some(c @ (b'l' | b'L')) => {
                let strip = strip || c == b'L';
                let hr12 = match tm.tm_hour % 12 {
                    0 => 12,
                    h => h,
                };
                if hr12 > 9 {
                    out.push(b'1');
                } else if !strip {
                    out.push(b' ');
                }
                out.push(digit(hr12 % 10));
            }
            Some(b'd') => {
                if tm.tm_mday > 9 || !strip {
                    out.push(digit(tm.tm_mday / 10));
                }
                out.push(digit(tm.tm_mday % 10));
            }
            Some(b'm') => {
                if tm.tm_mon > 8 || !strip {
                    out.push(digit((tm.tm_mon + 1) / 10));
                }
                out.push(digit((tm.tm_mon + 1) % 10));
            }
            Some(b'M') => {
                if tm.tm_min > 9 || !strip {
                    out.push(digit(tm.tm_min / 10));
                }
                out.push(digit(tm.tm_min % 10));
            }
            Some(b'N') => {
                if !fits(&mut left, 9) {
                    return None;
                }
                out.extend_from_slice(format!("{:09}", nsec).as_bytes());
            }
            Some(b'S') => {
                if tm.tm_sec > 9 || !strip {
                    out.push(digit(tm.tm_sec / 10));
                }
                out.push(digit(tm.tm_sec % 10));
            }
            Some(b'y') => {
                if tm.tm_year > 9 || !strip {
                    out.push(digit((tm.tm_year / 10) % 10));
                }
                out.push(digit(tm.tm_year % 10));
            }
            _ => match via_strftime(&fmt[start..i], tm, &mut left, &mut out) {
                Spec::Done => {}
                Spec::Overflow => return None,
                Spec::Empty => return Some(Vec::new()),
            },
        }
    }
    Some(out)
}

/// `ztrftime` with the buffer sizes the strftime builtin tries: eight
/// bytes per format byte, doubled up to three times.
pub fn format_time(fmt: &[u8], tm: &libc::tm, nsec: i64) -> Option<Vec<u8>> {
    (0..4).find_map(|x| ztrftime(fmt, tm, nsec, (fmt.len() * 8) << x))
}

// While alive, the C library sees the shell's TZ and LC_TIME (by
// LC_ALL, LC_TIME, LANG precedence), as zsh's bin_strftime() makes TZ a
// local exported parameter and zsh keeps the locale in step with them.
struct TimeEnv {
    tz: Option<OsString>,
    locale: Option<CString>,
}

impl TimeEnv {
    fn enter() -> TimeEnv {
        let tz = std::env::var_os("TZ");
        match crate::shell::var_get_opt("TZ") {
            Some(v) if !v.contains('\0') => std::env::set_var("TZ", v),
            Some(_) => {}
            None => std::env::remove_var("TZ"),
        }
        // SAFETY: tzset() only rereads TZ; localtime_r() does not.
        unsafe { tzset() };
        TimeEnv {
            tz,
            locale: set_time_locale(),
        }
    }
}

impl Drop for TimeEnv {
    fn drop(&mut self) {
        match self.tz.take() {
            Some(v) => std::env::set_var("TZ", v),
            None => std::env::remove_var("TZ"),
        }
        // SAFETY: as in enter().
        unsafe { tzset() };
        if let Some(l) = &self.locale {
            // SAFETY: l is a NUL-terminated locale name from setlocale().
            unsafe { libc::setlocale(libc::LC_TIME, l.as_ptr()) };
        }
    }
}

// Switches LC_TIME to the shell's setting, returning the previous one.
fn set_time_locale() -> Option<CString> {
    let want = ["LC_ALL", "LC_TIME", "LANG"]
        .iter()
        .find_map(|n| crate::shell::var_get_opt(n).filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "C".to_string());
    let want = CString::new(want).ok()?;
    // SAFETY: a null locale only queries; the result is copied before the
    // next setlocale() call can overwrite it.
    let saved = unsafe {
        let cur = libc::setlocale(libc::LC_TIME, std::ptr::null());
        if cur.is_null() {
            return None;
        }
        CStr::from_ptr(cur).to_owned()
    };
    // SAFETY: want is NUL-terminated. An unknown locale leaves LC_TIME as
    // it was, as in zsh.
    unsafe { libc::setlocale(libc::LC_TIME, want.as_ptr()) };
    Some(saved)
}

fn warn(msg: &str) {
    crate::sys::err_line(&format!("{}{}", crate::shell::err_prefix(None), msg));
}

// C strtoul() in base 10: the value, where parsing stopped, and whether
// it overflowed (ERANGE).
fn strtoul(s: &str) -> (u64, usize, bool) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let neg = b.get(i) == Some(&b'-');
    if neg || b.get(i) == Some(&b'+') {
        i += 1;
    }
    let digits = i;
    let mut v: u64 = 0;
    let mut over = false;
    while i < b.len() && b[i].is_ascii_digit() {
        match v
            .checked_mul(10)
            .and_then(|v| v.checked_add(u64::from(b[i] - b'0')))
        {
            Some(n) => v = n,
            None => over = true,
        }
        i += 1;
    }
    if i == digits {
        return (0, 0, false);
    }
    if over {
        return (u64::MAX, i, true);
    }
    (if neg { v.wrapping_neg() } else { v }, i, false)
}

// zsh's zstrtol() in base 10, warning when the number was truncated.
fn zstrtol(s: &str) -> (i64, usize) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && matches!(b[i], b' ' | b'\t') {
        i += 1;
    }
    let neg = b.get(i) == Some(&b'-');
    if neg || b.get(i) == Some(&b'+') {
        i += 1;
    }
    let inp = i;
    let mut calc: u64 = 0;
    let mut trunc: Option<usize> = None;
    while i < b.len() && b[i].is_ascii_digit() {
        if trunc.is_none() {
            let newcalc = calc.wrapping_mul(10).wrapping_add(u64::from(b[i] - b'0'));
            if newcalc < calc {
                trunc = Some(i);
            } else {
                calc = newcalc;
            }
        }
        i += 1;
    }
    // Too long for a signed value, except the lowest negative number.
    if trunc.is_none() && (calc as i64) < 0 && (!neg || calc & !(1u64 << 63) != 0) {
        trunc = Some(i - 1);
        calc /= 10;
    }
    if let Some(t) = trunc {
        warn(&format!(
            "number truncated after {} digits: {}",
            t - inp,
            &s[inp..]
        ));
    }
    let v = calc as i64;
    (if neg { v.wrapping_neg() } else { v }, i)
}

/// The `strftime` builtin: `strftime [-n] [-s scalar] format [epochtime
/// [nanoseconds]]` and `strftime -r [-q] [-s scalar] format timestring`.
pub fn builtin_strftime(words: &[String]) -> i32 {
    let name = "strftime";
    let (mut nonl, mut quiet, mut reverse) = (false, false, false);
    let mut scalar: Option<String> = None;
    let mut i = 1;
    while let Some(w) = words.get(i) {
        if w == "-" || w == "--" {
            i += 1;
            break;
        }
        let b = w.as_bytes();
        if b.first() != Some(&b'-') || b[1].is_ascii_digit() {
            break;
        }
        for (k, c) in w[1..].char_indices() {
            match c {
                'n' => nonl = true,
                'q' => quiet = true,
                'r' => reverse = true,
                's' => {
                    let rest = &w[k + 2..];
                    if !rest.is_empty() {
                        scalar = Some(rest.to_string());
                    } else if let Some(a) = words.get(i + 1) {
                        scalar = Some(a.clone());
                        i += 1;
                    } else {
                        builtin_err(name, "argument expected: -s");
                        return 1;
                    }
                    break;
                }
                _ => {
                    builtin_err(name, &format!("bad option: -{}", c));
                    return 1;
                }
            }
        }
        i += 1;
    }
    let args = &words[i..];
    if args.is_empty() {
        builtin_err(name, "not enough arguments");
        return 1;
    }
    if args.len() > 3 {
        builtin_err(name, "too many arguments");
        return 1;
    }
    let _env = TimeEnv::enter();
    if let Some(s) = &scalar {
        if !crate::lexer::is_valid_name(s) {
            builtin_err(name, &format!("not an identifier: {}", s));
            return 1;
        }
    }
    if reverse {
        let Some(input) = args.get(1) else {
            builtin_err(name, "timestring expected");
            return 1;
        };
        return reverse_strftime(name, &args[0], input, scalar.as_deref(), quiet);
    }

    let (tm, nsec) = match args.get(1) {
        None => {
            let (secs, ns) = now();
            let Some(tm) = localtime(secs) else {
                builtin_err(name, &format!("{}: unable to convert to time", secs));
                return 1;
            };
            (tm, ns)
        }
        Some(a) => {
            let (secs, end, over) = strtoul(a);
            if over {
                builtin_err(name, &format!("{}: numerical result out of range", a));
                return 1;
            }
            if a.is_empty() || end != a.len() {
                builtin_err(name, &format!("{}: invalid decimal number", a));
                return 1;
            }
            let Some(tm) = localtime(secs as i64) else {
                builtin_err(name, &format!("{}: unable to convert to time", a));
                return 1;
            };
            let mut nsec = 0;
            if let Some(n) = args.get(2) {
                let (v, end) = zstrtol(n);
                if n.is_empty() || end != n.len() {
                    builtin_err(name, &format!("{}: invalid decimal number", n));
                    return 1;
                }
                if !(0..=999_999_999).contains(&v) {
                    builtin_err(name, &format!("{}: invalid nanosecond value", n));
                    return 1;
                }
                nsec = v;
            }
            (tm, nsec)
        }
    };

    let Some(out) = format_time(args[0].as_bytes(), &tm, nsec) else {
        builtin_err(name, &format!("bad/unsupported format: '{}'", args[0]));
        return 1;
    };
    match &scalar {
        Some(s) => {
            crate::shell::var_set(s, &String::from_utf8_lossy(&out), false);
        }
        None => {
            let mut out = out;
            if !nonl {
                out.push(b'\n');
            }
            let _ = crate::sys::write_fd(1, &out);
        }
    }
    0
}

// strftime -r: parse `input` with strptime() and give the epoch time.
fn reverse_strftime(name: &str, fmt: &str, input: &str, scalar: Option<&str>, quiet: bool) -> i32 {
    let (Ok(cfmt), Ok(cin)) = (CString::new(fmt), CString::new(input)) else {
        if !quiet {
            builtin_err(name, "format not matched");
        }
        return 1;
    };
    // SAFETY: tm is plain old data, so all-zero is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // mktime() then guesses DST for the current zone, and day 0 would be
    // the last day of the previous month.
    tm.tm_isdst = -1;
    tm.tm_mday = 1;
    // SAFETY: both strings are NUL-terminated and tm is a live local.
    let endp = unsafe { libc::strptime(cin.as_ptr(), cfmt.as_ptr(), &mut tm) };
    if endp.is_null() {
        if !quiet {
            builtin_err(name, "format not matched");
        }
        return 1;
    }
    // SAFETY: tm is a live local filled in by strptime().
    let t = unsafe { libc::mktime(&mut tm) } as i64;
    match scalar {
        Some(s) => {
            crate::shell::var_set(s, &t.to_string(), false);
        }
        None => {
            let _ = crate::sys::write_fd(1, format!("{}\n", t).as_bytes());
        }
    }
    // SAFETY: strptime() returned a pointer into cin, which is still alive.
    if unsafe { *endp } != 0 && !quiet {
        builtin_err(name, "warning: input string not completely matched");
    }
    0
}
