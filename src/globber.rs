//! Pathname expansion with zsh patterns: `*`, `?`, `[...]`, `(a|b)`,
//! `<n-m>`, `**/` recursion, and glob qualifiers such as `(N)`, `(.)`,
//! `(/)` and `(m-1)`.
//!
//! Patterns arrive with quoted characters backslash-escaped.

use crate::pattern::Pattern;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

/// Parsed glob qualifiers.
#[derive(Default, Debug)]
pub struct Quals {
    pub null: bool,
    pub dots: bool,
    // OR-separated groups (`,`) of AND-ed file-type/permission tests.
    clauses: Vec<Vec<Test>>,
    sort: Option<(char, bool)>,
    sort_code: Option<(String, bool)>,
    // `:h`, `:t`, ... applied to each match.
    modifiers: String,
    // `(e:code:)` / `(+func)`: shell code each match must pass, with
    // $REPLY set to it (and able to replace it via REPLY or reply).
    code: Vec<String>,
    // `(P:word:)`: words put before each match.
    prepend: Vec<String>,
    // `([n,m])`: which of the sorted matches to keep.
    range: Option<(i64, i64)>,
    // `(Yn)`: stop after n matches.
    limit: Option<usize>,
}

#[derive(Debug)]
enum ModeCheck {
    // All bits in mask are set (`+`).
    AllSet(u32),
    // No bits in mask are set (`-`).
    NoneSet(u32),
    // Masked bits equal expected (`=`).
    Exact(u32, u32),
}

#[derive(Debug)]
enum Test {
    Regular,
    Dir,
    Link,
    Exec,
    Socket,
    Fifo,
    Perm(u32),
    Mode(Vec<ModeCheck>),
    Uid(u32),
    Gid(u32),
    // Modified (`m`), accessed (`a`) or changed (`c`) less (`-`) / more
    // (`+`) / exactly than `n` units ago.
    Time(char, char, i64, u64),
    // `L`: size under (`-`) / over (`+`) / exactly `n` units of 2^shift bytes.
    Size(char, u64, u32),
    // `l`: link count.
    Nlink(char, u64),
    // `%`, `%b`, `%c`: a device file.
    Device(char),
    // `-`: the test looks at what a symbolic link points to.
    Follow(Box<Test>),
    // `F`: a directory with something in it.
    Full,
    Negate(Box<Test>),
}

/// Splits a trailing qualifier group `(...)` off `pattern`. A final group
/// counts as qualifiers when it has no `|`, as with zsh's BARE_GLOB_QUAL.
pub fn split_qualifiers(pattern: &str) -> (&str, Option<Quals>) {
    let items = unescaped_items(pattern);
    match split_qualifiers_items(&items) {
        Ok((split, Some(q))) => {
            let byte_open = byte_offset_for_items(pattern, split);
            (&pattern[..byte_open], Some(q))
        }
        _ => (pattern, None),
    }
}

fn unescaped_items(s: &str) -> Vec<(char, bool)> {
    let mut out = Vec::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push((n, true));
                continue;
            }
        }
        out.push((c, false));
    }
    out
}

fn byte_offset_for_items(s: &str, item_idx: usize) -> usize {
    let mut chars = s.char_indices();
    let mut count = 0;
    while count < item_idx {
        let Some((_, c)) = chars.next() else {
            return s.len();
        };
        if c == '\\' {
            let _ = chars.next();
        }
        count += 1;
    }
    chars.next().map(|(idx, _)| idx).unwrap_or(s.len())
}

/// Splits trailing qualifiers off expanded `(char, quoted)` pieces, preserving
/// quoted vs unquoted delimiter semantics for `(e...)` and `(#q...)`.
pub fn split_qualifiers_items(items: &[(char, bool)]) -> Result<(usize, Option<Quals>), String> {
    if items.last() != Some(&(')', false)) || crate::builtins::is_disabled('p', "(") {
        return Ok((items.len(), None));
    }
    let extglob = crate::options::is_set("extendedglob");
    let mut bare_disallowed = !crate::options::is_set("bareglobqual");
    let bar_enabled = !crate::builtins::is_disabled('p', "|");
    let tilde_enabled = extglob && !crate::builtins::is_disabled('p', "~");
    let mut depth = 1usize;
    let mut open = None;
    for k in (0..items.len() - 1).rev() {
        match items[k] {
            (')', false) => {
                depth += 1;
                bare_disallowed = true;
            }
            ('|', false) if bar_enabled => bare_disallowed = true,
            ('~', false) if tilde_enabled => bare_disallowed = true,
            ('(', false) => {
                depth -= 1;
                if depth == 0 {
                    open = Some(k);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(open) = open else {
        return Ok((items.len(), None));
    };
    let inner = &items[open + 1..items.len() - 1];
    let body = if extglob
        && !crate::builtins::is_disabled('p', "#")
        && matches!(inner.first(), Some(&('#', false)))
    {
        if inner.get(1) == Some(&('q', false)) {
            &inner[2..]
        } else {
            return Ok((items.len(), None));
        }
    } else if bare_disallowed {
        return Ok((items.len(), None));
    } else {
        inner
    };
    Ok((open, Some(parse_quals_items(body)?)))
}

// A qualifier argument: text between a delimiter and its partner
// (`(`, `[`, `{`, `<` pair up; anything else closes itself).
// Unquoted delimiters only close on unquoted matching delimiters (matching
// zsh's tokenized `ctok` in `get_strarg`); quoted delimiters close on the
// matching character regardless of quoting.
fn delimited_items(chars: &[(char, bool)], i: &mut usize) -> Result<String, String> {
    let &(open, open_quoted) = chars
        .get(*i)
        .ok_or_else(|| "missing delimiter".to_string())?;
    let close = match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        c => c,
    };
    let mut out = String::new();
    let mut k = *i + 1;
    while k < chars.len() {
        let (c, quoted) = chars[k];
        if c == close && (open_quoted || !quoted) {
            *i = k + 1;
            return Ok(out);
        }
        out.push(c);
        k += 1;
    }
    Err("missing end of string".to_string())
}

fn parse_mode_spec(spec: &str) -> Result<Vec<ModeCheck>, String> {
    let mut checks = Vec::new();
    for part in spec.split(',') {
        if part.is_empty() {
            continue;
        }
        let bytes = part.as_bytes();
        // Octal form: `[+-]?[0-7]+`
        let (sign, rest) = match bytes[0] {
            b'+' => (Some('+'), &part[1..]),
            b'-' => (Some('-'), &part[1..]),
            b'=' => (Some('='), &part[1..]),
            _ => (None, part),
        };
        if !rest.is_empty() && rest.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
            let bits = u32::from_str_radix(rest, 8).map_err(|_| "invalid mode".to_string())?;
            checks.push(match sign {
                Some('+') => ModeCheck::AllSet(bits),
                Some('-') => ModeCheck::NoneSet(bits),
                _ => ModeCheck::Exact(0o7777, bits),
            });
            continue;
        }
        // Symbolic form: `[ugoa]*([+-=][rwxst]*)+`
        let mut idx = 0;
        let mut who_rwx = 0u32;
        let mut who_spec = 0u32;
        while idx < bytes.len() {
            match bytes[idx] {
                b'u' => {
                    who_rwx |= 0o700;
                    who_spec |= 0o4000;
                }
                b'g' => {
                    who_rwx |= 0o070;
                    who_spec |= 0o2000;
                }
                b'o' => {
                    who_rwx |= 0o007;
                    who_spec |= 0o1000;
                }
                b'a' => {
                    who_rwx |= 0o777;
                    who_spec |= 0o7000;
                }
                _ => break,
            }
            idx += 1;
        }
        if who_rwx == 0 && who_spec == 0 {
            who_rwx = 0o777;
            who_spec = 0o7000;
        }
        if idx >= bytes.len() {
            return Err("invalid mode".to_string());
        }
        while idx < bytes.len() {
            let op = bytes[idx];
            if !matches!(op, b'+' | b'-' | b'=') {
                return Err("invalid mode".to_string());
            }
            idx += 1;
            let mut bits = 0u32;
            while idx < bytes.len() && !matches!(bytes[idx], b'+' | b'-' | b'=') {
                match bytes[idx] {
                    b'r' => bits |= who_rwx & 0o444,
                    b'w' => bits |= who_rwx & 0o222,
                    b'x' => bits |= who_rwx & 0o111,
                    b's' => bits |= who_spec & 0o6000,
                    b't' => bits |= 0o1000,
                    _ => return Err("invalid mode".to_string()),
                }
                idx += 1;
            }
            checks.push(match op {
                b'+' => ModeCheck::AllSet(bits),
                b'-' => ModeCheck::NoneSet(bits),
                _ => ModeCheck::Exact(who_rwx, bits),
            });
        }
    }
    Ok(checks)
}

fn parse_quals_items(chars: &[(char, bool)]) -> Result<Quals, String> {
    let mut q = Quals {
        clauses: vec![Vec::new()],
        ..Default::default()
    };
    let mut i = 0;
    let mut negate = false;
    let mut follow = false;
    while i < chars.len() {
        let (c, _) = chars[i];
        i += 1;
        let test = match c {
            'N' => {
                q.null = true;
                continue;
            }
            // Modifiers end the qualifiers.
            ':' => {
                q.modifiers = chars[i - 1..].iter().map(|&(ch, _)| ch).collect();
                break;
            }
            'D' => {
                q.dots = true;
                continue;
            }
            '^' => {
                negate = !negate;
                continue;
            }
            ',' => {
                q.clauses.push(Vec::new());
                negate = false;
                follow = false;
                continue;
            }
            'e' => {
                q.code.push(delimited_items(chars, &mut i)?);
                continue;
            }
            '+' => {
                let start = i;
                while chars
                    .get(i)
                    .is_some_and(|&(c, _)| c.is_alphanumeric() || c == '_' || c == '-')
                {
                    i += 1;
                }
                q.code
                    .push(chars[start..i].iter().map(|&(c, _)| c).collect());
                continue;
            }
            'P' => {
                q.prepend.push(delimited_items(chars, &mut i)?);
                continue;
            }
            'Y' => {
                let start = i;
                while chars.get(i).is_some_and(|&(c, _)| c.is_ascii_digit()) {
                    i += 1;
                }
                // ^Y turns the limit off again.
                q.limit = if negate {
                    None
                } else {
                    Some(
                        chars[start..i]
                            .iter()
                            .map(|&(c, _)| c)
                            .collect::<String>()
                            .parse()
                            .map_err(|_| "invalid count".to_string())?,
                    )
                };
                continue;
            }
            '[' => {
                i -= 1;
                let spec = delimited_items(chars, &mut i)?;
                let eval = |t: &str| crate::arith::eval(t.trim()).ok();
                q.range = Some(match spec.split_once(',') {
                    Some((a, b)) => (
                        eval(a).ok_or_else(|| "invalid subscript".to_string())?,
                        eval(b).ok_or_else(|| "invalid subscript".to_string())?,
                    ),
                    None => {
                        let n = eval(&spec).ok_or_else(|| "invalid subscript".to_string())?;
                        (n, n)
                    }
                });
                continue;
            }
            '.' => Test::Regular,
            '/' => Test::Dir,
            '@' => Test::Link,
            '*' => Test::Exec,
            '=' => Test::Socket,
            'p' => Test::Fifo,
            'F' => Test::Full,
            'r' => Test::Perm(0o400),
            'w' => Test::Perm(0o200),
            'x' => Test::Perm(0o100),
            'R' => Test::Perm(0o004),
            'W' => Test::Perm(0o002),
            'X' => Test::Perm(0o001),
            's' => Test::Perm(0o4000),
            'S' => Test::Perm(0o2000),
            't' => Test::Perm(0o1000),
            '%' => match chars.get(i) {
                Some(&(k @ ('b' | 'c'), _)) => {
                    i += 1;
                    Test::Device(k)
                }
                _ => Test::Device('%'),
            },
            'L' | 'l' => {
                let shift = match chars.get(i) {
                    Some(&(u, _)) if c == 'L' && "pPkKmMgGtT".contains(u) => {
                        i += 1;
                        match u.to_ascii_lowercase() {
                            'p' => 9,
                            'k' => 10,
                            'm' => 20,
                            'g' => 30,
                            _ => 40,
                        }
                    }
                    _ => 0,
                };
                let sign = match chars.get(i) {
                    Some(&(s @ ('+' | '-'), _)) => {
                        i += 1;
                        s
                    }
                    _ => '=',
                };
                let start = i;
                while chars.get(i).is_some_and(|&(c, _)| c.is_ascii_digit()) {
                    i += 1;
                }
                let n: u64 = chars[start..i]
                    .iter()
                    .map(|&(c, _)| c)
                    .collect::<String>()
                    .parse()
                    .map_err(|_| "number expected".to_string())?;
                if c == 'L' {
                    Test::Size(sign, n, shift)
                } else {
                    Test::Nlink(sign, n)
                }
            }
            'm' | 'a' | 'c' => {
                let unit = match chars.get(i) {
                    Some(&(u @ ('M' | 'w' | 'h' | 'm' | 's'), _)) => {
                        i += 1;
                        u
                    }
                    _ => 'd',
                };
                let sign = match chars.get(i) {
                    Some(&(s @ ('+' | '-'), _)) => {
                        i += 1;
                        s
                    }
                    _ => '=',
                };
                let start = i;
                while chars.get(i).is_some_and(|&(c, _)| c.is_ascii_digit()) {
                    i += 1;
                }
                let n: i64 = chars[start..i]
                    .iter()
                    .map(|&(c, _)| c)
                    .collect::<String>()
                    .parse()
                    .map_err(|_| "invalid time".to_string())?;
                let secs = match unit {
                    'M' => 30 * 86400,
                    'w' => 7 * 86400,
                    'h' => 3600,
                    'm' => 60,
                    's' => 1,
                    _ => 86400,
                };
                Test::Time(c, sign, n, secs)
            }
            'o' | 'O' => {
                let &(key, _) = chars
                    .get(i)
                    .ok_or_else(|| "unknown sort specifier".to_string())?;
                i += 1;
                let rev = (c == 'O') != negate;
                match key {
                    'e' => {
                        q.sort_code = Some((delimited_items(chars, &mut i)?, rev));
                    }
                    '+' => {
                        let start = i;
                        while chars
                            .get(i)
                            .is_some_and(|&(c, _)| c.is_alphanumeric() || c == '_' || c == '-')
                        {
                            i += 1;
                        }
                        if start == i {
                            return Err("missing identifier after `+'".to_string());
                        }
                        q.sort_code =
                            Some((chars[start..i].iter().map(|&(c, _)| c).collect(), rev));
                    }
                    _ => q.sort = Some((key, rev)),
                }
                continue;
            }
            'f' => {
                let spec = match chars.get(i) {
                    Some(&(ch, _)) if ch.is_ascii_digit() || ch == '+' || ch == '-' => {
                        let start = i;
                        if ch == '+' || ch == '-' {
                            i += 1;
                        }
                        while chars.get(i).is_some_and(|&(d, _)| ('0'..='7').contains(&d)) {
                            i += 1;
                        }
                        chars[start..i]
                            .iter()
                            .map(|&(ch, _)| ch)
                            .collect::<String>()
                    }
                    _ => delimited_items(chars, &mut i)?,
                };
                Test::Mode(parse_mode_spec(&spec)?)
            }
            'u' => {
                let uid = if chars.get(i).is_some_and(|&(d, _)| d.is_ascii_digit()) {
                    let start = i;
                    while chars.get(i).is_some_and(|&(d, _)| d.is_ascii_digit()) {
                        i += 1;
                    }
                    chars[start..i]
                        .iter()
                        .map(|&(ch, _)| ch)
                        .collect::<String>()
                        .parse::<u32>()
                        .map_err(|_| "invalid uid".to_string())?
                } else {
                    let name = delimited_items(chars, &mut i)?;
                    if let Ok(n) = name.parse::<u32>() {
                        n
                    } else {
                        let cstr =
                            std::ffi::CString::new(name).map_err(|_| "unknown user".to_string())?;
                        let pw = unsafe { libc::getpwnam(cstr.as_ptr()) };
                        if pw.is_null() {
                            return Err("unknown user".to_string());
                        }
                        unsafe { (*pw).pw_uid }
                    }
                };
                Test::Uid(uid)
            }
            'g' => {
                let gid = if chars.get(i).is_some_and(|&(d, _)| d.is_ascii_digit()) {
                    let start = i;
                    while chars.get(i).is_some_and(|&(d, _)| d.is_ascii_digit()) {
                        i += 1;
                    }
                    chars[start..i]
                        .iter()
                        .map(|&(ch, _)| ch)
                        .collect::<String>()
                        .parse::<u32>()
                        .map_err(|_| "invalid gid".to_string())?
                } else {
                    let name = delimited_items(chars, &mut i)?;
                    if let Ok(n) = name.parse::<u32>() {
                        n
                    } else {
                        let cstr = std::ffi::CString::new(name)
                            .map_err(|_| "unknown group".to_string())?;
                        let gr = unsafe { libc::getgrnam(cstr.as_ptr()) };
                        if gr.is_null() {
                            return Err("unknown group".to_string());
                        }
                        unsafe { (*gr).gr_gid }
                    }
                };
                Test::Gid(gid)
            }
            'U' => Test::Uid(unsafe { libc::geteuid() }),
            'G' => Test::Gid(unsafe { libc::getegid() }),
            '-' => {
                follow = !follow;
                continue;
            }
            // Accepted without effect.
            'T' => continue,
            '0'..='9' | ']' => continue,
            other => return Err(format!("unknown file attribute: {}", other)),
        };
        let test = if follow {
            Test::Follow(Box::new(test))
        } else {
            test
        };
        let test = if negate {
            Test::Negate(Box::new(test))
        } else {
            test
        };
        q.clauses.last_mut().unwrap().push(test);
    }
    Ok(q)
}

#[cfg(test)]
fn parse_quals(s: &str) -> Option<Quals> {
    parse_quals_items(&unescaped_items(s)).ok()
}

// `(e:code:)`: runs code with $REPLY set to the match; a zero status
// keeps it, replaced by $reply (if set) or a changed $REPLY.
fn run_code(code: &str, path: &str) -> Option<Vec<String>> {
    crate::shell::var_set("REPLY", path, false);
    crate::shell::with_shell(|s| {
        s.remove_param("reply");
    });
    let st =
        crate::shell::in_eval_context("globqual", || crate::exec::run_string_named(code, None));
    if st != 0 {
        return None;
    }
    let reply = crate::shell::with_shell(|s| s.array("reply").cloned());
    Some(match reply {
        Some(items) => items,
        None => vec![crate::shell::var_get("REPLY")],
    })
}

fn passes(path: &Path, test: &Test) -> bool {
    passes_as(path, test, false)
}

// As zsh does, a test sees the link itself unless `-` came before it;
// then it sees the target, or the link again when that is missing.
fn passes_as(path: &Path, test: &Test, follow: bool) -> bool {
    match test {
        Test::Negate(t) => !passes_as(path, t, follow),
        Test::Follow(t) => passes_as(path, t, true),
        t => {
            let stat = if follow {
                fs::metadata(path).or_else(|_| fs::symlink_metadata(path))
            } else {
                fs::symlink_metadata(path)
            };
            let Ok(m) = stat else {
                return false;
            };
            let cmp = |sign: char, have: u64, want: u64| match sign {
                '-' => have < want,
                '+' => have > want,
                _ => have == want,
            };
            let fmt = (m.mode() as libc::mode_t) & libc::S_IFMT;
            match t {
                Test::Link => m.file_type().is_symlink(),
                Test::Size(sign, n, shift) => {
                    let units = (m.len() + (1u64 << shift) - 1) >> shift;
                    cmp(*sign, units, *n)
                }
                Test::Nlink(sign, n) => cmp(*sign, m.nlink(), *n),
                Test::Device(kind) => match kind {
                    'b' => fmt == libc::S_IFBLK,
                    'c' => fmt == libc::S_IFCHR,
                    _ => fmt == libc::S_IFBLK || fmt == libc::S_IFCHR,
                },
                Test::Regular => m.is_file(),
                Test::Dir => m.is_dir(),
                Test::Exec => m.is_file() && m.permissions().mode() & 0o111 != 0,
                Test::Socket => (m.mode() as libc::mode_t) & libc::S_IFMT == libc::S_IFSOCK,
                Test::Fifo => (m.mode() as libc::mode_t) & libc::S_IFMT == libc::S_IFIFO,
                Test::Perm(bits) => m.permissions().mode() & bits != 0,
                Test::Mode(checks) => {
                    let mode = m.permissions().mode() & 0o7777;
                    checks.iter().all(|c| match *c {
                        ModeCheck::AllSet(bits) => (mode & bits) == bits,
                        ModeCheck::NoneSet(bits) => (mode & bits) == 0,
                        ModeCheck::Exact(mask, bits) => (mode & mask) == bits,
                    })
                }
                Test::Uid(uid) => m.uid() == *uid,
                Test::Gid(gid) => m.gid() == *gid,
                Test::Full => {
                    m.is_dir() && fs::read_dir(path).is_ok_and(|mut d| d.next().is_some())
                }
                Test::Time(which, sign, n, secs) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let stamp = match which {
                        'a' => m.atime(),
                        'c' => m.ctime(),
                        _ => m.mtime(),
                    };
                    let age = (now - stamp).max(0) / *secs as i64;
                    match sign {
                        '-' => age < *n,
                        '+' => age > *n,
                        _ => age == *n,
                    }
                }
                Test::Negate(_) | Test::Follow(_) => unreachable!(),
            }
        }
    }
}

// Splits on `/` that are not escaped.
fn components(pattern: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                out.last_mut().unwrap().push('\\');
                if let Some(n) = chars.next() {
                    out.last_mut().unwrap().push(n);
                }
            }
            '/' => out.push(String::new()),
            c => out.last_mut().unwrap().push(c),
        }
    }
    out
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else if dir.ends_with('/') {
        format!("{}{}", dir, name)
    } else {
        format!("{}/{}", dir, name)
    }
}

fn list_dir(dir: &str) -> Vec<String> {
    let path = if dir.is_empty() { "." } else { dir };
    let mut names: Vec<String> = match fs::read_dir(path) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

// Every directory under `dir`, `dir` itself first, not following links.
fn walk_dirs(dir: &str, dots: bool, out: &mut Vec<String>) {
    out.push(dir.to_string());
    for name in list_dir(dir) {
        if name.starts_with('.') && !dots {
            continue;
        }
        let p = join(dir, &name);
        if fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
            walk_dirs(&p, dots, out);
        }
    }
}

fn sort_matches(out: &mut Vec<String>, q: &Quals) {
    if let Some((code, rev)) = &q.sort_code {
        let saved = crate::shell::var_get_opt("REPLY");
        let mut keyed: Vec<(String, String)> = out
            .iter()
            .map(|p| {
                crate::shell::var_set("REPLY", p, false);
                crate::shell::in_eval_context("globsort", || {
                    crate::exec::run_string_named(code, None);
                });
                (crate::shell::var_get("REPLY"), p.clone())
            })
            .collect();
        match saved {
            Some(v) => {
                crate::shell::var_set("REPLY", &v, false);
            }
            None => crate::shell::with_shell(|s| {
                s.remove_param("REPLY");
            }),
        }
        keyed.sort_by(|a, b| {
            crate::shell::collate(&a.0, &b.0).then_with(|| crate::shell::collate(&a.1, &b.1))
        });
        if *rev {
            keyed.reverse();
        }
        *out = keyed.into_iter().map(|(_, p)| p).collect();
        return;
    }
    match q.sort {
        Some((key, rev)) => {
            match key {
                'm' => out.sort_by_key(|p| {
                    std::cmp::Reverse(fs::metadata(p).map(|m| m.mtime()).unwrap_or(0))
                }),
                'L' => out.sort_by_key(|p| fs::metadata(p).map(|m| m.len()).unwrap_or(0)),
                _ => out.sort_by(|a, b| crate::shell::collate(a, b)),
            }
            if rev {
                out.reverse();
            }
        }
        None => sort_names(out),
    }
}

// The default order: by name, numbers by value with NUMERIC_GLOB_SORT.
fn sort_names(out: &mut Vec<String>) {
    if crate::options::is_set("numericglobsort") {
        out.sort_by(|a, b| crate::expand::numeric_order(a, b));
    } else {
        out.sort_by(|a, b| crate::shell::collate(a, b));
    }
    out.dedup();
}

/// Expands `pattern`. Returns the sorted matches; empty if none.
pub fn glob(pattern: &str, quals: Option<&Quals>) -> Vec<String> {
    // `a~b`: a's matches, less the paths b matches.
    if let Some((main, excl)) = crate::pattern::glob_exclusion(pattern) {
        let ex = Pattern::new(&excl);
        let mut out = glob(&main, quals);
        out.retain(|p| !ex.matches(p));
        return out;
    }
    let dots = quals.is_some_and(|q| q.dots);
    let comps = components(pattern);
    let trailing_slash =
        pattern != "/" && comps.len() > 1 && comps.last().is_some_and(String::is_empty);
    let mut current = vec![(String::new(), false)];
    let absolute = pattern.starts_with('/');
    let last = comps.len() - 1;
    for (k, comp) in comps.iter().enumerate() {
        if k == 0 && absolute {
            current = vec![("/".to_string(), false)];
            continue;
        }
        if comp.is_empty() {
            continue;
        }
        let mut next = Vec::new();
        if (comp == "**" || comp == "***") && k < last {
            for (dir, _) in &current {
                let mut found = Vec::new();
                walk_dirs(dir, dots, &mut found);
                next.extend(found.into_iter().map(|p| (p, true)));
            }
            current = next;
            continue;
        }
        if !crate::pattern::has_meta(comp) {
            let lit = crate::pattern::unescape(comp);
            for (dir, _) in &current {
                next.push((join(dir, &lit), false));
            }
            current = next;
            continue;
        }
        let pat = Pattern::new(comp);
        let explicit_dot = comp.starts_with('.') || comp.starts_with("\\.");
        for (dir, _) in &current {
            for name in list_dir(dir) {
                if name.starts_with('.') && !(dots || explicit_dot) {
                    continue;
                }
                if name == "." || name == ".." {
                    continue;
                }
                if pat.matches(&name) {
                    let p = join(dir, &name);
                    if k == last || fs::metadata(&p).is_ok_and(|m| m.is_dir()) {
                        next.push((p, true));
                    }
                }
            }
        }
        current = next;
    }
    let mut out: Vec<String> = current
        .into_iter()
        .filter(|(p, from_readdir)| {
            !p.is_empty()
                && (*from_readdir || fs::symlink_metadata(p).is_ok())
                && (!trailing_slash || fs::metadata(p).is_ok_and(|m| m.is_dir()))
        })
        .map(|(p, _)| p)
        .collect();
    let append_slash = |items: &mut Vec<String>| {
        if trailing_slash {
            for p in items {
                if !p.ends_with('/') {
                    p.push('/');
                }
            }
        }
    };
    let Some(q) = quals else {
        append_slash(&mut out);
        sort_names(&mut out);
        return out;
    };
    if !q.clauses.is_empty() {
        out.retain(|p| {
            q.clauses
                .iter()
                .any(|clause| clause.iter().all(|t| passes(Path::new(p), t)))
        });
    }
    append_slash(&mut out);
    sort_matches(&mut out, q);
    if !q.code.is_empty() {
        let saved = (
            crate::shell::var_get_opt("REPLY"),
            crate::shell::with_shell(|s| s.array("reply").cloned()),
        );
        let mut kept = Vec::new();
        for p in &out {
            let mut cur = vec![p.clone()];
            for code in &q.code {
                let mut next = Vec::new();
                for c in &cur {
                    if let Some(r) = run_code(code, c) {
                        next.extend(r);
                    }
                }
                cur = next;
            }
            kept.extend(cur);
        }
        match saved.0 {
            Some(v) => {
                crate::shell::var_set("REPLY", &v, false);
            }
            None => crate::shell::with_shell(|s| {
                s.remove_param("REPLY");
            }),
        }
        match saved.1 {
            Some(a) => {
                crate::shell::array_set("reply", a);
            }
            None => crate::shell::with_shell(|s| {
                s.remove_param("reply");
            }),
        }
        out = kept;
    }
    if let Some(n) = q.limit {
        out.truncate(n);
    }
    if let Some((a, b)) = q.range {
        let len = out.len() as i64;
        let idx = |n: i64| if n < 0 { len + n } else { n - 1 };
        let (lo, hi) = (idx(a).max(0), idx(b).min(len - 1));
        out = if lo <= hi && lo < len {
            out[lo as usize..=hi as usize].to_vec()
        } else {
            Vec::new()
        };
    }
    if !q.modifiers.is_empty() {
        out = out
            .iter()
            .map(|p| crate::expand::modify_word(p, &qualifier_text(&q.modifiers)))
            .collect();
        // Sorting by name sorts the modified names, as zsh does.
        // (Names that become the same all stay.)
        if q.range.is_none() && q.sort.is_none_or(|(k, _)| k == 'n') {
            if crate::options::is_set("numericglobsort") {
                out.sort_by(|a, b| crate::expand::numeric_order(a, b));
            } else {
                out.sort_by(|a, b| crate::shell::collate(a, b));
            }
            if q.sort.is_some_and(|(_, rev)| rev) {
                out.reverse();
            }
        }
    }
    if !q.prepend.is_empty() {
        out = out
            .into_iter()
            .flat_map(|p| {
                q.prepend
                    .iter()
                    .cloned()
                    .chain(std::iter::once(p))
                    .collect::<Vec<_>>()
            })
            .collect();
    }
    out
}

// A qualifier's modifiers as zsh sees them: quoting in the word has made
// expansion characters literal (`\$`), but a replacement's `${match[1]}`
// is expanded per match.
fn qualifier_text(mods: &str) -> String {
    let mut out = String::new();
    let mut cs = mods.chars().peekable();
    while let Some(c) = cs.next() {
        if c == '\\' && cs.peek().is_some_and(|n| "$`{}[]".contains(*n)) {
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualifiers_split_only_without_alternation() {
        let _lock = crate::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::shell::shell_init();
        crate::shell::with_shell(|s| crate::options::init(s, crate::options::Emulation::Zsh));
        assert!(split_qualifiers("*(N)").1.is_some_and(|q| q.null));
        assert!(split_qualifiers("*.(rs|toml)").1.is_none());
        assert_eq!(split_qualifiers("*(Nm-1)").0, "*");
        // As in zsh, a word that is all qualifiers has an empty pattern.
        let (base, q) = split_qualifiers("(N)");
        assert!(base.is_empty() && q.is_some_and(|q| q.null));
    }

    #[test]
    fn globbing_with_groups_ranges_and_qualifiers() {
        let _lock = crate::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::shell::shell_init();
        crate::shell::with_shell(|s| crate::options::init(s, crate::options::Emulation::Zsh));
        let dir = tempfile::TempDir::new().unwrap();
        let d = dir.path().to_str().unwrap();
        for f in ["a.rs", "b.toml", "c.txt", "f1", "f2", "f10", ".hidden"] {
            fs::write(dir.path().join(f), "").unwrap();
        }
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/x.rs"), "").unwrap();
        let names = |v: Vec<String>| -> Vec<String> {
            v.into_iter()
                .map(|p| p[d.len() + 1..].to_string())
                .collect()
        };
        assert_eq!(
            names(glob(&format!("{}/*.(rs|toml)", d), None)),
            ["a.rs", "b.toml"]
        );
        assert_eq!(names(glob(&format!("{}/f<1-2>", d), None)), ["f1", "f2"]);
        assert_eq!(
            names(glob(&format!("{}/**/*.rs", d), None)),
            ["a.rs", "sub/x.rs"]
        );
        let q = parse_quals("/").unwrap();
        assert_eq!(names(glob(&format!("{}/*", d), Some(&q))), ["sub"]);
        let q = parse_quals("D.").unwrap();
        assert!(names(glob(&format!("{}/*", d), Some(&q)))
            .iter()
            .any(|n| n == ".hidden"));
        assert!(glob(&format!("{}/nope*", d), None).is_empty());
    }
}
