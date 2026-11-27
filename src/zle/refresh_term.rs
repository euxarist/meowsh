//! Terminal capabilities for the display code: zsh's tcstr/tclen table
//! (init.c's init_term) with tcout(), tcoutarg(), tcmultout() and
//! tsetcap(). meowsh has no curses, so the strings are fixed xterm and
//! vt100 descriptions, kept as terminfo has them (padding included, since
//! zsh compares their lengths when choosing motions). Only the colour
//! capabilities are read from the terminal's terminfo entry.

use super::write_bytes;
use crate::zle::termctl;
use std::cell::{Cell, RefCell};

pub(super) const TERM_BAD: u32 = 0x01;
pub(super) const TERM_UNKNOWN: u32 = 0x02;
pub(super) const TERM_NOUP: u32 = 0x04;
pub(super) const TERM_SHORT: u32 = 0x08;

pub(super) const TCCLEARSCREEN: usize = 0;
pub(super) const TCLEFT: usize = 1;
pub(super) const TCMULTLEFT: usize = 2;
pub(super) const TCRIGHT: usize = 3;
pub(super) const TCMULTRIGHT: usize = 4;
pub(super) const TCUP: usize = 5;
pub(super) const TCMULTUP: usize = 6;
pub(super) const TCDOWN: usize = 7;
pub(super) const TCMULTDOWN: usize = 8;
pub(super) const TCDEL: usize = 9;
pub(super) const TCMULTDEL: usize = 10;
pub(super) const TCINS: usize = 11;
pub(super) const TCMULTINS: usize = 12;
pub(super) const TCCLEAREOD: usize = 13;
pub(super) const TCCLEAREOL: usize = 14;
pub(super) const TCINSLINE: usize = 15;
pub(super) const TCDELLINE: usize = 16;
pub(super) const TCNEXTTAB: usize = 17;
pub(super) const TCBOLDFACEBEG: usize = 18;
pub(super) const TCFAINTBEG: usize = 19;
pub(super) const TCSTANDOUTBEG: usize = 20;
pub(super) const TCUNDERLINEBEG: usize = 21;
pub(super) const TCITALICSBEG: usize = 22;
pub(super) const TCALLATTRSOFF: usize = 23;
pub(super) const TCSTANDOUTEND: usize = 24;
pub(super) const TCUNDERLINEEND: usize = 25;
pub(super) const TCITALICSEND: usize = 26;
pub(super) const TCHORIZPOS: usize = 27;
const TCSAVECURSOR: usize = 32;
const TCRESTRCURSOR: usize = 33;
const TCBACKSPACE: usize = 34;
pub(super) const TCFGCOLOUR: usize = 35;
pub(super) const TCBGCOLOUR: usize = 36;
const TC_COUNT: usize = 39;

// zsh's tccapnams: what `zle -T tc` functions are passed.
const NAMES: [&str; TC_COUNT] = [
    "cl", "le", "LE", "nd", "RI", "up", "UP", "do", "DO", "dc", "DC", "ic", "IC", "cd", "ce", "al",
    "dl", "ta", "md", "mh", "so", "us", "ZH", "me", "se", "ue", "ZR", "ch", "ku", "kd", "kl", "kr",
    "sc", "rc", "bc", "AF", "AB", "vi", "ve",
];

// ncurses' xterm (xterm-new) entry.
const XTERM: &[(&str, &str)] = &[
    ("cl", "\x1b[H\x1b[2J"),
    ("le", "\x08"),
    ("LE", "\x1b[%p1%dD"),
    ("nd", "\x1b[C"),
    ("RI", "\x1b[%p1%dC"),
    ("up", "\x1b[A"),
    ("UP", "\x1b[%p1%dA"),
    ("do", "\n"),
    ("DO", "\x1b[%p1%dB"),
    ("dc", "\x1b[P"),
    ("DC", "\x1b[%p1%dP"),
    ("IC", "\x1b[%p1%d@"),
    ("cd", "\x1b[J"),
    ("ce", "\x1b[K"),
    ("al", "\x1b[L"),
    ("dl", "\x1b[M"),
    ("ta", "\t"),
    ("md", "\x1b[1m"),
    ("mh", "\x1b[2m"),
    ("so", "\x1b[7m"),
    ("us", "\x1b[4m"),
    ("ZH", "\x1b[3m"),
    ("me", "\x1b(B\x1b[m"),
    ("se", "\x1b[27m"),
    ("ue", "\x1b[24m"),
    ("ZR", "\x1b[23m"),
    ("ch", "\x1b[%i%p1%dG"),
    ("ku", "\x1bOA"),
    ("kd", "\x1bOB"),
    ("kl", "\x1bOD"),
    ("kr", "\x1bOC"),
    ("sc", "\x1b7"),
    ("rc", "\x1b8"),
    ("AF", "\x1b[3%p1%dm"),
    ("AB", "\x1b[4%p1%dm"),
    ("vi", "\x1b[?25l"),
    ("ve", "\x1b[?12l\x1b[?25h"),
];

// xterm-256color differs from xterm in its colours.
const XTERM_256: &[(&str, &str)] = &[
    (
        "AF",
        "\x1b[%?%p1%{8}%<%t3%p1%d%e%p1%{16}%<%t9%p1%{8}%-%d%e38;5;%p1%d%;m",
    ),
    (
        "AB",
        "\x1b[%?%p1%{8}%<%t4%p1%d%e%p1%{16}%<%t10%p1%{8}%-%d%e48;5;%p1%d%;m",
    ),
];

// ncurses' vt100 entry.
const VT100: &[(&str, &str)] = &[
    ("cl", "\x1b[H\x1b[J$<50>"),
    ("le", "\x08"),
    ("LE", "\x1b[%p1%dD"),
    ("nd", "\x1b[C$<2>"),
    ("RI", "\x1b[%p1%dC"),
    ("up", "\x1b[A$<2>"),
    ("UP", "\x1b[%p1%dA"),
    ("do", "\n"),
    ("DO", "\x1b[%p1%dB"),
    ("cd", "\x1b[J$<50>"),
    ("ce", "\x1b[K$<3>"),
    ("ta", "\t"),
    ("md", "\x1b[1m$<2>"),
    ("so", "\x1b[7m$<2>"),
    ("us", "\x1b[4m$<2>"),
    ("me", "\x1b[m\x0f$<2>"),
    ("se", "\x1b[m$<2>"),
    ("ue", "\x1b[m$<2>"),
    ("ku", "\x1bOA"),
    ("kd", "\x1bOB"),
    ("kl", "\x1bOD"),
    ("kr", "\x1bOC"),
    ("sc", "\x1b7"),
    ("rc", "\x1b8"),
];

// ncurses' dumb entry has no motions at all.
const DUMB: &[(&str, &str)] = &[("do", "\n")];

struct Term {
    name: String,
    flags: u32,
    caps: Vec<Option<String>>,
    am: bool,
    colours: i32,
}

thread_local! {
    static TERM: RefCell<Option<Term>> = const { RefCell::new(None) };
    static SHORT: Cell<bool> = const { Cell::new(false) };
}

fn build(name: &str) -> Term {
    let mut t = Term {
        name: name.to_string(),
        flags: 0,
        caps: vec![None; TC_COUNT],
        am: false,
        colours: 0,
    };
    if name.is_empty() {
        t.flags = TERM_UNKNOWN;
        return t;
    }
    let (tables, am, colours): (Vec<&[(&str, &str)]>, bool, i32) = if name == "dumb" {
        (vec![DUMB], true, -1)
    } else if name.starts_with("vt1") {
        (vec![VT100], true, -1)
    } else if name.contains("256color") {
        (vec![XTERM_256, XTERM], true, 256)
    } else if name.contains("88color") || name == "rxvt-unicode" {
        (vec![XTERM_256, XTERM], true, 88)
    } else {
        (vec![XTERM], true, 8)
    };
    t.am = am;
    t.colours = colours;
    for (i, cap) in NAMES.iter().enumerate() {
        t.caps[i] = tables
            .iter()
            .find_map(|tb| tb.iter().find(|(n, _)| n == cap))
            .map(|(_, v)| v.to_string());
    }
    // zsh takes colours from terminfo; the tables only guess them from
    // the name, which leaves terminals like foot at 8 colours.
    if let Some(ti) = read_terminfo(name) {
        t.colours = ti.colours;
        t.caps[TCFGCOLOUR] = ti.setaf;
        t.caps[TCBGCOLOUR] = ti.setab;
    }
    // init_term's fixups.
    if t.caps[TCUP].is_none() {
        t.flags |= TERM_NOUP;
    }
    if t.caps[TCBACKSPACE].is_none() {
        t.caps[TCBACKSPACE] = Some("\x08".into());
    }
    if t.caps[TCLEFT].is_none() {
        t.caps[TCLEFT] = t.caps[TCBACKSPACE].clone();
    }
    if t.caps[TCSAVECURSOR].is_some() && t.caps[TCRESTRCURSOR].is_none() {
        t.caps[TCSAVECURSOR] = None;
    }
    if t.caps[TCDOWN]
        .as_deref()
        .is_some_and(|s| s.starts_with('\n'))
    {
        t.caps[TCDOWN] = None;
    }
    if t.caps[TCCLEARSCREEN].is_none() {
        t.caps[TCCLEARSCREEN] = Some("\x0c".into());
    }
    for (cap, def) in [
        (TCITALICSBEG, "\x1b[3m"),
        (TCITALICSEND, "\x1b[23m"),
        (TCFAINTBEG, "\x1b[2m"),
    ] {
        if t.caps[cap].is_none() {
            t.caps[cap] = Some(def.into());
        }
    }
    t
}

/// The colour capabilities of a compiled terminfo entry.
struct TermInfoColours {
    colours: i32,
    setaf: Option<String>,
    setab: Option<String>,
}

/// Finds `name` in the terminfo database as ncurses searches it.
fn terminfo_path(name: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let first = name.chars().next()?;
    if name.contains('/') {
        return None;
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::var_os("TERMINFO") {
        dirs.push(d.into());
    }
    if let Some(h) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(h).join(".terminfo"));
    }
    let system = ["/etc/terminfo", "/lib/terminfo", "/usr/share/terminfo"];
    match std::env::var("TERMINFO_DIRS") {
        Ok(list) => {
            for d in list.split(':') {
                if d.is_empty() {
                    dirs.extend(system.iter().map(PathBuf::from));
                } else {
                    dirs.push(d.into());
                }
            }
        }
        Err(_) => dirs.extend(system.iter().map(PathBuf::from)),
    }
    dirs.push("/usr/lib/terminfo".into());
    dirs.into_iter().find_map(|d| {
        [first.to_string(), format!("{:02x}", first as u32)]
            .into_iter()
            .map(|sub| d.join(sub).join(name))
            .find(|p| p.is_file())
    })
}

/// Reads `colors`, `setaf` and `setab` from a compiled terminfo entry
/// (term(5): the legacy format and the 32-bit-number one).
fn read_terminfo(name: &str) -> Option<TermInfoColours> {
    const COLORS: usize = 13;
    const SETAF: usize = 359;
    const SETAB: usize = 360;
    let data = std::fs::read(terminfo_path(name)?).ok()?;
    let short = |i: usize| -> Option<i32> {
        let b = data.get(i..i + 2)?;
        Some(i16::from_le_bytes([b[0], b[1]]) as i32)
    };
    let num_size = match short(0)? {
        0o432 => 2,
        0o1036 => 4,
        _ => return None,
    };
    let [names, bools, nums, strs] = [1, 2, 3, 4].map(|k| short(k * 2).unwrap_or(-1));
    if [names, bools, nums, strs].iter().any(|&n| n < 0) {
        return None;
    }
    let (names, bools, nums, strs) = (names as usize, bools as usize, nums as usize, strs as usize);
    let mut off = 12 + names + bools;
    off += off % 2;
    let nums_at = off;
    let strs_at = nums_at + nums * num_size;
    let table_at = strs_at + strs * 2;
    let colours = if COLORS < nums {
        let at = nums_at + COLORS * num_size;
        let b = data.get(at..at + num_size)?;
        if num_size == 2 {
            i16::from_le_bytes([b[0], b[1]]) as i32
        } else {
            i32::from_le_bytes([b[0], b[1], b[2], b[3]])
        }
    } else {
        -1
    };
    let string = |idx: usize| -> Option<String> {
        if idx >= strs {
            return None;
        }
        let o = short(strs_at + idx * 2)?;
        if o < 0 {
            return None;
        }
        let s = data.get(table_at + o as usize..)?;
        let end = s.iter().position(|&c| c == 0)?;
        Some(String::from_utf8_lossy(&s[..end]).into_owned())
    };
    Some(TermInfoColours {
        colours,
        setaf: string(SETAF),
        setab: string(SETAB),
    })
}

/// Rereads the description when $TERM has changed (zsh reinitialises
/// the terminal when TERM is assigned).
pub(super) fn reload() {
    let name = crate::shell::var_get_opt("TERM").unwrap_or_default();
    TERM.with(|t| {
        let mut t = t.borrow_mut();
        if t.as_ref().is_none_or(|t| t.name != name) {
            *t = Some(build(&name));
        }
    });
}

fn with<R>(f: impl FnOnce(&Term) -> R) -> R {
    reload();
    TERM.with(|t| f(t.borrow().as_ref().expect("terminal description")))
}

fn raw(cap: usize) -> Option<String> {
    with(|t| t.caps[cap].clone())
}

pub(super) fn tclen(cap: usize) -> i32 {
    with(|t| t.caps[cap].as_ref().map_or(0, |s| s.len() as i32))
}

pub(super) fn tccan(cap: usize) -> bool {
    tclen(cap) != 0
}

pub(super) fn hasam() -> bool {
    with(|t| t.am)
}

pub(super) fn tccolours() -> i32 {
    with(|t| t.colours)
}

pub(super) fn termflags() -> u32 {
    with(|t| t.flags)
        | (if SHORT.with(|s| s.get()) {
            TERM_SHORT
        } else {
            0
        })
}

pub(super) fn set_short(on: bool) {
    SHORT.with(|s| s.set(on));
}

/// What tputs sends for `s`: padding is dropped, as for a terminal with
/// xon/xoff flow control.
fn tputs(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("$<") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        match after.find('>') {
            Some(e)
                if after[..e]
                    .chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, '.' | '*' | '/')) =>
            {
                rest = &after[e + 1..];
            }
            _ => {
                out.push_str("$<");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// tgoto(cap, arg, arg) for terminfo strings: a small tparm.
fn tparm(fmt: &str, arg: i32) -> String {
    let b = fmt.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut stack: Vec<i32> = Vec::new();
    let mut params = [arg, arg];
    let mut i = 0;
    // Skips a conditional branch: to the matching %e (when `to_else`) or %;.
    let skip = |i: &mut usize, to_else: bool| {
        let mut level = 0;
        while *i + 1 < b.len() {
            if b[*i] != b'%' {
                *i += 1;
                continue;
            }
            let c = b[*i + 1];
            *i += 2;
            match c {
                b'?' => level += 1,
                b';' if level == 0 => return,
                b';' => level -= 1,
                b'e' if level == 0 && to_else => return,
                _ => {}
            }
        }
        *i = b.len();
    };
    while i < b.len() {
        if b[i] != b'%' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        let Some(&c) = b.get(i + 1) else {
            break;
        };
        i += 2;
        match c {
            b'%' => out.push(b'%'),
            b'p' => {
                let n = b.get(i).map_or(0, |d| d.wrapping_sub(b'1') as usize);
                i += 1;
                stack.push(params.get(n).copied().unwrap_or(0));
            }
            b'd' => out.extend(stack.pop().unwrap_or(0).to_string().bytes()),
            b'c' => out.push(stack.pop().unwrap_or(0) as u8),
            b'i' => {
                params[0] += 1;
                params[1] += 1;
            }
            b'\'' => {
                stack.push(b.get(i).copied().unwrap_or(0) as i32);
                i += 2;
            }
            b'{' => {
                let mut n = 0i32;
                while i < b.len() && b[i].is_ascii_digit() {
                    n = n * 10 + (b[i] - b'0') as i32;
                    i += 1;
                }
                i += 1;
                stack.push(n);
            }
            b'<' | b'>' | b'=' | b'+' | b'-' | b'*' | b'/' | b'm' | b'&' | b'|' | b'^' | b'A'
            | b'O' => {
                let y = stack.pop().unwrap_or(0);
                let x = stack.pop().unwrap_or(0);
                stack.push(match c {
                    b'<' => (x < y) as i32,
                    b'>' => (x > y) as i32,
                    b'=' => (x == y) as i32,
                    b'+' => x.wrapping_add(y),
                    b'-' => x.wrapping_sub(y),
                    b'*' => x.wrapping_mul(y),
                    b'/' => x.checked_div(y).unwrap_or(0),
                    b'm' => x.checked_rem(y).unwrap_or(0),
                    b'&' => x & y,
                    b'|' => x | y,
                    b'^' => x ^ y,
                    b'A' => (x != 0 && y != 0) as i32,
                    _ => (x != 0 || y != 0) as i32,
                });
            }
            b'!' => {
                let x = stack.pop().unwrap_or(0);
                stack.push((x == 0) as i32);
            }
            b'~' => {
                let x = stack.pop().unwrap_or(0);
                stack.push(!x);
            }
            b't' => {
                if stack.pop().unwrap_or(0) == 0 {
                    skip(&mut i, true);
                }
            }
            b'e' => skip(&mut i, false),
            _ => {}
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// tputs(tgoto(tcstr[cap], arg, arg)): the sequence for a capability
/// with a parameter, bypassing `zle -T tc` (as zsh's colour code does).
pub(super) fn tgoto_str(cap: usize, arg: i32) -> String {
    raw(cap).map(|s| tputs(&tparm(&s, arg))).unwrap_or_default()
}

/// zsh's tcout(): through the `zle -T tc` function when there is one.
pub(super) fn tcout(cap: usize) {
    let out = match termctl::tcout(NAMES[cap], None) {
        Some(s) => s,
        None => raw(cap).map(|s| tputs(&s)).unwrap_or_default(),
    };
    write_bytes(out.as_bytes());
}

/// zsh's tcoutarg().
pub(super) fn tcoutarg(cap: usize, arg: i32) {
    let out = match termctl::tcout(NAMES[cap], Some(arg)) {
        Some(s) => s,
        None => tgoto_str(cap, arg),
    };
    write_bytes(out.as_bytes());
}

/// zsh's tcmultout(): `ct` repeats of `cap`, or `multcap` once when that
/// is no longer.
pub(super) fn tcmultout(cap: usize, multcap: usize, ct: i32) -> bool {
    if tccan(multcap) && (!tccan(cap) || tclen(multcap) <= tclen(cap) * ct) {
        tcoutarg(multcap, ct);
        true
    } else if tccan(cap) {
        for _ in 0..ct {
            tcout(cap);
        }
        true
    } else {
        false
    }
}

/// zsh's tsetcap(cap, 0): straight to the terminal, never through
/// `zle -T tc`.
pub(super) fn tsetcap(cap: usize) {
    if tccan(cap) && termflags() & (TERM_NOUP | TERM_BAD | TERM_UNKNOWN) == 0 {
        if let Some(s) = raw(cap) {
            write_bytes(tputs(&s).as_bytes());
        }
    }
}

const TERMINFO_MAP: &[(&str, &str)] = &[
    ("clear", "cl"),
    ("cub1", "le"),
    ("cub", "LE"),
    ("cuf1", "nd"),
    ("cuf", "RI"),
    ("cuu1", "up"),
    ("cuu", "UP"),
    ("cud1", "do"),
    ("cud", "DO"),
    ("dch1", "dc"),
    ("dch", "DC"),
    ("ich1", "ic"),
    ("ich", "IC"),
    ("ed", "cd"),
    ("el", "ce"),
    ("il1", "al"),
    ("dl1", "dl"),
    ("ht", "ta"),
    ("bold", "md"),
    ("dim", "mh"),
    ("smso", "so"),
    ("smul", "us"),
    ("sitm", "ZH"),
    ("sgr0", "me"),
    ("rmso", "se"),
    ("rmul", "ue"),
    ("ritm", "ZR"),
    ("hpa", "ch"),
    ("kcuu1", "ku"),
    ("kcud1", "kd"),
    ("kcub1", "kl"),
    ("kcuf1", "kr"),
    ("sc", "sc"),
    ("rc", "rc"),
    ("setaf", "AF"),
    ("setab", "AB"),
    ("civis", "vi"),
    ("cnorm", "ve"),
];

pub fn termcap_lookup(key: &str) -> Option<String> {
    with(|t| match key {
        "Co" => (t.colours >= 0).then(|| t.colours.to_string()),
        "li" => Some(
            crate::shell::var_get_opt("LINES")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "24".to_string()),
        ),
        "co" => Some(
            crate::shell::var_get_opt("COLUMNS")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "80".to_string()),
        ),
        "am" => t.am.then(|| "yes".to_string()),
        _ => NAMES
            .iter()
            .position(|&n| n == key)
            .and_then(|i| t.caps[i].clone()),
    })
}

pub fn terminfo_lookup(key: &str) -> Option<String> {
    match key {
        "colors" => with(|t| (t.colours >= 0).then(|| t.colours.to_string())),
        "lines" => termcap_lookup("li"),
        "cols" => termcap_lookup("co"),
        "am" => with(|t| t.am.then(|| "yes".to_string())),
        _ => TERMINFO_MAP
            .iter()
            .find(|(ti, _)| *ti == key)
            .and_then(|(_, tc)| termcap_lookup(tc)),
    }
}

pub fn termcap_entries() -> Vec<(String, String)> {
    with(|t| {
        let mut out = Vec::new();
        if t.colours >= 0 {
            out.push(("Co".to_string(), t.colours.to_string()));
        }
        if t.am {
            out.push(("am".to_string(), "yes".to_string()));
        }
        for (i, &name) in NAMES.iter().enumerate() {
            if let Some(ref val) = t.caps[i] {
                out.push((name.to_string(), val.clone()));
            }
        }
        out
    })
}

pub fn terminfo_entries() -> Vec<(String, String)> {
    with(|t| {
        let mut out = Vec::new();
        if t.colours >= 0 {
            out.push(("colors".to_string(), t.colours.to_string()));
        }
        if t.am {
            out.push(("am".to_string(), "yes".to_string()));
        }
        for &(ti, tc) in TERMINFO_MAP {
            if let Some(i) = NAMES.iter().position(|&n| n == tc) {
                if let Some(ref val) = t.caps[i] {
                    out.push((ti.to_string(), val.clone()));
                }
            }
        }
        out
    })
}

fn run_echo_cap(builtin: &str, words: &[String], is_terminfo: bool) -> i32 {
    let Some(cap) = words.get(1) else {
        crate::shell::builtin_err(builtin, "not enough arguments");
        return 1;
    };
    let is_num = if is_terminfo {
        matches!(cap.as_str(), "colors" | "lines" | "cols")
    } else {
        matches!(cap.as_str(), "Co" | "li" | "co")
    };
    let val = if is_terminfo {
        terminfo_lookup(cap)
    } else {
        termcap_lookup(cap)
    };
    let Some(val) = val else {
        crate::shell::builtin_err(builtin, &format!("no such capability: {}", cap));
        return 1;
    };
    if is_num {
        crate::sys::out(&format!("{}\n", val));
        return 0;
    }
    let a1 = words
        .get(2)
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0);
    let formatted = if words.len() > 2 {
        tparm(&val, a1)
    } else {
        tputs(&val)
    };
    crate::sys::out(&formatted);
    0
}

pub fn builtin_echoti(words: &[String]) -> i32 {
    run_echo_cap("echoti", words, true)
}

pub fn builtin_echotc(words: &[String]) -> i32 {
    run_echo_cap("echotc", words, false)
}
