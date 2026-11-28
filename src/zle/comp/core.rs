//! The completion core, after zsh's Src/Zle/compcore.c: running the
//! completion widget's function, collecting matches (compadd's engine),
//! grouping and sorting them, and the quoting helpers around that.
//!
//! The word being completed arrives tokenized as zsh's lexer leaves it,
//! with the token chars of tricky.rs (U+0084 .. U+00A1, zsh's bytes), so
//! `$`, quotes and glob characters typed unquoted can be told from quoted
//! ones.

use super::complete::{
    comp_setunset, make_comp_params, pull_comp_params, push_comp_params, CP_EXACT, CP_EXACTSTR,
    CP_INSERT, CP_LASTPROMPT, CP_LIST, CP_OLDINS, CP_OLDLIST, CP_PARAMETER, CP_PATMATCH, CP_QUOTE,
    CP_QUOTING, CP_REDIRECT, CP_VARED,
};
use super::tricky::{
    addedx, dupstrspace, inull, inwhat, itok, remnulargs, set_addedx, set_wb, set_we, wb, we,
    ztoken, Lextok, ZleLexer, BANG, BAR, BNULL, BNULLKEEP, DASH, DNULL, EQUALS, HAT, INANG,
    INBRACE, INBRACK, INPAR, INPARMATH, IN_COND, IN_ENV, IN_MATH, IN_PAR, LEXFLAGS_ZLE, OUTANG,
    OUTBRACE, OUTBRACK, OUTPAR, OUTPARMATH, POUND, QSTRING, QTICK, QT_BACKSLASH,
    QT_BACKSLASH_PATTERN, QT_BACKSLASH_SHOWNULL, QT_DOLLARS, QT_DOUBLE, QT_NONE, QT_SINGLE,
    QT_SINGLE_OPTIONAL, QUEST, SNULL, STAR, STRING, TICK, TILDE,
};
use super::*;
use crate::pattern::Pattern;
use crate::zle::{Zle, CUT_RAW};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

// ---- tokens -------------------------------------------------------------

// tricky::untokenize as a string.
fn untok(s: &[char]) -> String {
    super::tricky::untokenize(s).into_iter().collect()
}

// The token standing for `c`, among those tokenize makes.
fn token_for(c: char) -> Option<char> {
    (0..29u32)
        .filter_map(|k| char::from_u32(POUND as u32 + k))
        .find(|&t| ztoken(t) == Some(c))
}

/// Makes the glob characters of `s` active, as zsh's tokenize.
pub fn tokenize(s: &[char]) -> Vec<char> {
    let shglob = crate::options::is_set("shglob");
    let mut s = s.to_vec();
    let mut bslash = false;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        match c {
            BNULL | BNULLKEEP | '\\' => {
                if bslash {
                    s[i - 1] = BNULL;
                } else {
                    bslash = true;
                    i += 1;
                    continue;
                }
            }
            '<' if !shglob => {
                if bslash {
                    s[i - 1] = BNULL;
                } else {
                    let t = i;
                    let mut k = i + 1;
                    while k < s.len() && s[k].is_ascii_digit() {
                        k += 1;
                    }
                    if k < s.len() && (s[k] == '-' || s[k] == DASH) {
                        k += 1;
                        while k < s.len() && s[k].is_ascii_digit() {
                            k += 1;
                        }
                        if k < s.len() && s[k] == '>' {
                            s[t] = INANG;
                            s[k] = OUTANG;
                            i = k;
                        }
                    }
                }
            }
            '(' | '|' | ')' if shglob => {}
            '(' | '|' | ')' | '>' | '^' | '#' | '~' | '[' | ']' | '*' | '?' | '=' | '-' | '!' => {
                if let Some(t) = token_for(c) {
                    if bslash {
                        s[i - 1] = BNULL;
                    } else {
                        s[i] = t;
                    }
                }
            }
            _ => {}
        }
        bslash = false;
        i += 1;
    }
    s
}

/// Whether a tokenized string has active pattern characters.
pub fn haswilds(s: &[char]) -> bool {
    if s.len() == 1 && (s[0] == INBRACK || s[0] == OUTBRACK) {
        return false;
    }
    let mut s = s.to_vec();
    if s.len() > 1 && s[0] == '%' && s[1] == QUEST {
        s[1] = '?';
    }
    let shglob = crate::options::is_set("shglob");
    let kshglob = crate::options::is_set("kshglob");
    let extglob = crate::options::is_set("extendedglob");
    for (k, &c) in s.iter().enumerate() {
        match c {
            INPAR => {
                if !shglob
                    || (k > 0
                        && kshglob
                        && matches!(s[k - 1], QUEST | STAR | '+' | BANG | '!' | '@'))
                {
                    return true;
                }
            }
            BAR | STAR | INBRACK | INANG | QUEST => return true,
            POUND | HAT if extglob => return true,
            _ => {}
        }
    }
    false
}

/// The pattern (as crate::pattern reads it) of a tokenized string:
/// tokens are active, other characters literal.
pub fn pat_from_tokens(s: &[char]) -> String {
    let mut out = String::new();
    for &c in s {
        if inull(c) {
            continue;
        }
        if let Some(t) = ztoken(c) {
            out.push(t);
        } else {
            out.push_str(&crate::pattern::escape(&c.to_string()));
        }
    }
    out
}

// ---- quoting ------------------------------------------------------------

// fromcomp bits.
pub const FC_LINE: i32 = 1;
pub const FC_INWORD: i32 = 2;

const SPECCHARS: &str = "#$^*()=|{}[]`<>?~;&\n\t \\'\"";
const PATCHARS: &str = "#^*()|[]<>?~\\";

/// A menu_start hook (zsh's MENUSTARTHOOK, which zsh/complist sets):
/// given the match and message counts, nonzero to stop menu completion.
pub type MenuStartHook = fn(&mut Zle, i32, i32) -> i32;

thread_local! {
    // Bounds of a $... word around the parameter name (parwb, parwe,
    // paroffs); parwb < 0 when not in one.
    static PARPOS: Cell<(i32, i32, i32)> = const { Cell::new((-1, 0, 0)) };
    static OLDMENUCMP: Cell<i32> = const { Cell::new(0) };
    // permmatches' static fi.
    static PERM_FI: Cell<bool> = const { Cell::new(false) };
    // Whether compstate[pattern_match] was assigned (zsh compares the
    // pointer with the one it set).
    static PATMATCH_SET: Cell<bool> = const { Cell::new(false) };
    // makebangspecial(0) in effect.
    static BANG_OFF: Cell<bool> = const { Cell::new(false) };
    // Where curexpl sits in the current group's explanations, when it
    // was added there; zsh shares the pointer, so counts go to both.
    static CUREXPL_AT: Cell<Option<usize>> = const { Cell::new(None) };
    static MENU_START: Cell<Option<MenuStartHook>> = const { Cell::new(None) };
}

/// Sets the menu_start hook (zsh/complist adds its menu selection here).
pub fn set_menu_start_hook(f: Option<MenuStartHook>) {
    MENU_START.with(|h| h.set(f));
}

/// Records an assignment to compstate[pattern_match].
pub fn note_patmatch_set() {
    PATMATCH_SET.with(|p| p.set(true));
}

pub(super) fn st(o: &Option<String>) -> &str {
    o.as_deref().unwrap_or("")
}

pub(super) fn clen(s: &str) -> usize {
    s.chars().count()
}

pub(super) fn csub(s: &str, a: usize, b: usize) -> String {
    s.chars().skip(a).take(b.saturating_sub(a)).collect()
}

fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

fn opt(name: &str) -> bool {
    crate::options::is_set(name)
}

fn bangchar() -> char {
    crate::shell::var_get_opt("histchars")
        .and_then(|h| h.chars().next())
        .unwrap_or('!')
}

// Whether the history character counts as special for quoting.
fn bang_special() -> bool {
    !BANG_OFF.with(|b| b.get()) && opt("banghist") && opt("interactive")
}

/// zsh's makebangspecial: 0 stops `!` being quoted, 1 restores it.
pub fn makebangspecial(yesno: bool) {
    BANG_OFF.with(|b| b.set(!yesno));
}

fn addunprintable(v: &mut Vec<char>, c: char, next: Option<char>) {
    let mut buf = [0u8; 4];
    for &b in c.encode_utf8(&mut buf).as_bytes() {
        match b {
            0 => {
                v.extend(['\\', '0']);
                if next.is_some_and(|n| ('0'..='7').contains(&n)) {
                    v.extend(['0', '0']);
                }
            }
            7 => v.extend(['\\', 'a']),
            8 => v.extend(['\\', 'b']),
            12 => v.extend(['\\', 'f']),
            b'\n' => v.extend(['\\', 'n']),
            b'\r' => v.extend(['\\', 'r']),
            b'\t' => v.extend(['\\', 't']),
            11 => v.extend(['\\', 'v']),
            _ => {
                v.push('\\');
                v.push((b'0' + ((b >> 6) & 7)) as char);
                v.push((b'0' + ((b >> 3) & 7)) as char);
                v.push((b'0' + (b & 7)) as char);
            }
        }
    }
}

fn isprint(c: char) -> bool {
    !c.is_control()
}

/// Quotes `s` for quoting style `instring` (a QT_* value), as zsh's
/// quotestring. Tokens pass through unquoted.
pub fn quotestring(s: &str, instring: i32) -> String {
    let u = chars(s);
    let len = u.len();
    let mut v: Vec<char> = Vec::with_capacity(len * 2);
    let mut instring = instring;
    let mut shownull = false;
    let mut quotesub = 0;
    let mut quotestart = 0usize;
    match instring {
        QT_BACKSLASH_SHOWNULL => {
            shownull = true;
            instring = QT_BACKSLASH;
        }
        QT_SINGLE_OPTIONAL => {
            quotesub = 1;
            shownull = true;
        }
        _ => {}
    }
    let bang = bang_special();
    let magiceq = opt("magicequalsubst");
    let extglob = opt("extendedglob");
    let rcquotes = opt("rcquotes");
    let banghist = opt("banghist");
    let hist = bangchar();
    let ispecial = |c: char| SPECCHARS.contains(c) || (bang && c == hist);
    let mut i = 0;
    if instring == QT_DOLLARS {
        if i < len && inull(u[i]) {
            i += 1;
        }
        while i < len {
            let c = u[i];
            if isprint(c) && !itok(c) {
                if c == '\\' || c == '\'' || (banghist && c == hist) {
                    v.push('\\');
                }
                v.push(c);
            } else {
                addunprintable(&mut v, c, u.get(i + 1).copied());
            }
            i += 1;
        }
    } else if instring == QT_BACKSLASH_PATTERN {
        for &c in &u {
            if PATCHARS.contains(c) {
                v.push('\\');
            }
            v.push(c);
        }
    } else {
        if shownull && u.is_empty() {
            v.extend(['\'', '\'']);
        }
        while i < len {
            let c = u[i];
            let mut dobackslash = false;
            if c == TICK || c == QTICK {
                v.push(c);
                i += 1;
                while i < len && u[i] != c {
                    v.push(u[i]);
                    i += 1;
                }
                v.push(c);
                if i < len {
                    i += 1;
                }
                continue;
            } else if (c == QSTRING || c == '$')
                && u.get(i + 1) == Some(&'\'')
                && instring == QT_DOUBLE
            {
                // $'...' needs no quoting inside double quotes.
                v.push(c);
                v.push('\'');
                i += 2;
                continue;
            } else if (c == STRING || c == QSTRING)
                && matches!(u.get(i + 1), Some(&INPAR) | Some(&INBRACK) | Some(&INBRACE))
            {
                let close = match u[i + 1] {
                    INPAR => OUTPAR,
                    INBRACE => OUTBRACE,
                    _ => OUTBRACK,
                };
                let beg = c;
                let mut level = 0;
                v.push(u[i]);
                v.push(u[i + 1]);
                i += 2;
                while i < len && (u[i] != close || level != 0) {
                    if u[i] == beg {
                        level += 1;
                    } else if u[i] == close {
                        level -= 1;
                    }
                    v.push(u[i]);
                    i += 1;
                }
                if i < len {
                    v.push(u[i]);
                    i += 1;
                }
                continue;
            } else if ispecial(c)
                && ((c != '=' && c != '~')
                    || i == 0
                    || (magiceq && (u[i - 1] == '=' || u[i - 1] == ':'))
                    || (c == '~' && extglob))
                && (instring == QT_BACKSLASH
                    || instring == QT_SINGLE_OPTIONAL
                    || (banghist && c == hist && instring != QT_SINGLE)
                    || (instring == QT_DOUBLE && matches!(c, '$' | '`' | '"' | '\\'))
                    || (instring == QT_SINGLE && c == '\''))
            {
                if instring == QT_SINGLE_OPTIONAL {
                    if quotesub == 1 {
                        if c == '\'' {
                            v.push('\\');
                        } else {
                            v.insert(quotestart, '\'');
                            quotesub = 2;
                        }
                        v.push(c);
                        i += 1;
                        quotestart = v.len();
                    } else if c == '\'' {
                        if !rcquotes {
                            v.extend(['\'', '\\', '\'']);
                            quotesub = 1;
                            quotestart = v.len();
                        } else {
                            v.extend(['\'', '\'']);
                        }
                        i += 1;
                    } else {
                        v.push(c);
                        i += 1;
                    }
                    continue;
                } else if c == '\n' || (instring == QT_SINGLE && c == '\'') {
                    if c == '\n' {
                        v.extend(['$', '\'', '\\', 'n', '\'']);
                    } else if !rcquotes {
                        v.extend(['\'', '\\', '\'', '\'']);
                    } else {
                        v.extend(['\'', '\'']);
                    }
                    i += 1;
                    continue;
                } else {
                    dobackslash = true;
                }
            }
            if itok(c) || instring != QT_BACKSLASH {
                if dobackslash {
                    v.push('\\');
                }
                if c == INPARMATH {
                    let mut inmath = 1;
                    v.push(c);
                    i += 1;
                    while i < len {
                        let uc = u[i];
                        v.push(uc);
                        i += 1;
                        if uc == OUTPARMATH {
                            inmath -= 1;
                            if inmath == 0 {
                                break;
                            }
                        } else if uc == INPARMATH {
                            inmath += 1;
                        }
                    }
                } else {
                    v.push(c);
                    i += 1;
                }
                continue;
            }
            if isprint(c) {
                if dobackslash {
                    v.push('\\');
                }
                v.push(c);
            } else {
                v.extend(['$', '\'']);
                addunprintable(&mut v, c, u.get(i + 1).copied());
                v.push('\'');
            }
            i += 1;
        }
    }
    if quotesub == 2 {
        v.push('\'');
    }
    v.into_iter().collect()
}

/// Quotes `s` for every level of $compstate[all_quotes]; with `ign` not
/// the innermost one.
pub fn multiquote(z: &Zle, s: &str, ign: bool) -> String {
    let qs: Vec<char> = st(&z.comp.compqstack).chars().collect();
    let mut s = s.to_string();
    if !qs.is_empty() && (!ign || qs.len() > 1) {
        for &q in &qs[usize::from(ign)..] {
            s = quotestring(&s, q as i32);
        }
    }
    s
}

/// multiquote, leaving a leading `~` unquoted.
pub fn tildequote(z: &Zle, s: &str, ign: bool) -> String {
    match s.strip_prefix('~') {
        Some(rest) => {
            let q = multiquote(z, &format!("x{}", rest), ign);
            format!("~{}", q.chars().skip(1).collect::<String>())
        }
        None => multiquote(z, s, ign),
    }
}

pub fn comp_quoting_string(stype: i32) -> &'static str {
    match stype {
        QT_SINGLE => "'",
        QT_DOUBLE => "\"",
        QT_DOLLARS => "$'",
        _ => "\\",
    }
}

/// Copies `s` without its backslashes (keeping what they quote).
pub fn rembslash(s: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == '\\' {
            i += 1;
            if i < s.len() {
                out.push(s[i]);
                i += 1;
            }
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

/// Removes one of every pair of quoted single quotes; returns how many
/// characters went.
pub fn remsquote(s: &mut Vec<char>) -> i32 {
    let qa = if opt("rcquotes") { 1 } else { 3 };
    let mut ret = 0;
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let hit = if qa == 1 {
            s[i] == '\'' && s.get(i + 1) == Some(&'\'')
        } else {
            s[i] == '\''
                && s.get(i + 1) == Some(&'\\')
                && s.get(i + 2) == Some(&'\'')
                && s.get(i + 3) == Some(&'\'')
        };
        if hit {
            ret += qa;
            out.push('\'');
            i += qa as usize + 1;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    *s = out;
    ret
}

/// tokenize, also making `$`, `{` and `}` active unless quoted.
pub fn ctokenize(p: &[char]) -> Vec<char> {
    let mut p = tokenize(p);
    let mut bslash = false;
    for k in 0..p.len() {
        if p[k] == '\\' {
            bslash = true;
        } else {
            if matches!(p[k], '$' | '{' | '}') {
                if bslash {
                    p[k - 1] = BNULL;
                } else {
                    p[k] = match p[k] {
                        '$' => STRING,
                        '{' => INBRACE,
                        _ => OUTBRACE,
                    };
                }
            }
            bslash = false;
        }
    }
    p
}

/// The whole word: IPREFIX, PREFIX and SUFFIX. Without `untok` the
/// prefix and suffix are tokenized for use as a pattern.
pub fn comp_str(z: &Zle, ipl: Option<&mut i32>, pl: Option<&mut i32>, untok: bool) -> String {
    let mut p = chars(st(&z.comp.compprefix));
    let mut s = chars(st(&z.comp.compsuffix));
    let ip = chars(st(&z.comp.compiprefix));
    if !untok {
        p = remnulargs(&ctokenize(&p));
        s = remnulargs(&ctokenize(&s));
    }
    if let Some(ipl) = ipl {
        *ipl = ip.len() as i32;
    }
    if let Some(pl) = pl {
        *pl = p.len() as i32;
    }
    ip.into_iter().chain(p).chain(s).collect()
}

// ---- parameter names ----------------------------------------------------

fn skipparens(inpar: char, outpar: char, s: &[char], i: &mut usize) -> i32 {
    if s.get(*i) != Some(&inpar) {
        return -1;
    }
    let mut level = 1;
    loop {
        *i += 1;
        if *i >= s.len() || level == 0 {
            break;
        }
        if s[*i] == inpar {
            level += 1;
        } else if s[*i] == outpar {
            level -= 1;
        }
    }
    level
}

fn iident(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

// zsh's itype_end(s, INAMESPC, 0).
fn itype_end_namespc(s: &[char], ptr: usize) -> usize {
    let posix = opt("posixidentifiers");
    let ident_end = |mut k: usize| {
        while k < s.len() && !itok(s[k]) && iident(s[k]) && (!posix || s[k].is_ascii()) {
            k += 1;
        }
        k
    };
    let mut ptr = ptr;
    if !posix {
        let dot = usize::from(s.get(ptr) == Some(&'.'));
        let t = ident_end(ptr + dot);
        if t > ptr + dot {
            if s.get(t) == Some(&'.') {
                ptr = t + 1;
            } else {
                return t;
            }
        }
    }
    ident_end(ptr)
}

/// Checks whether the cursor is in a parameter name of tokenized word
/// `s`; returns the name. With `set`, also the prefix and suffix around
/// it; with `test`, only whether it is one.
pub fn check_param(z: &mut Zle, s: &[char], set: bool, test: bool) -> Option<String> {
    z.comp.parpre = None;
    if !test {
        z.comp.ispar = 0;
        z.comp.parq = 0;
        z.comp.eparq = 0;
    }
    let offs = (z.comp.offs.max(0) as usize).min(s.len());
    let mut found = false;
    let mut qstring = false;
    let mut p = offs;
    loop {
        if p < s.len()
            && (s[p] == STRING || s[p] == QSTRING)
            && p < offs
            && !(s[p] == STRING && s.get(p + 1) == Some(&SNULL))
            && !(s[p] == QSTRING && s.get(p + 1) == Some(&'\''))
        {
            found = true;
            qstring = s[p] == QSTRING;
            break;
        }
        if p == 0 {
            break;
        }
        p -= 1;
    }
    let isstr = |c: Option<&char>| matches!(c, Some(&STRING) | Some(&QSTRING));
    if found {
        while p > 0 && isstr(s.get(p - 1)) {
            p -= 1;
        }
        while isstr(s.get(p + 1)) && isstr(s.get(p + 2)) {
            p += 2;
        }
    }
    if !found || matches!(s.get(p + 1), Some(&INPAR) | Some(&INBRACK) | Some(&SNULL)) {
        return None;
    }
    // A parameter expression, not $(...), $[...] or $'...'.
    let mut b = p + 1;
    let mut br = 1;
    let mut nest = false;
    if s.get(b) == Some(&INBRACE) {
        let mut tb = b;
        // Past the closing brace: not in the name.
        if skipparens(INBRACE, OUTBRACE, s, &mut tb) == 0 && tb <= offs {
            return None;
        }
        b += 1;
        br += 1;
        let r = if qstring {
            skipparens('(', ')', s, &mut b)
        } else {
            skipparens(INPAR, OUTPAR, s, &mut b)
        };
        if r > 0 || b > offs {
            // Still in the (...) flags.
            z.comp.ispar = 2;
            return None;
        }
        let mut tb = p as isize - 1;
        while tb > 0 && s[tb as usize] != OUTBRACE && s[tb as usize] != INBRACE {
            tb -= 1;
        }
        if tb > 0 && s[tb as usize] == INBRACE && s[tb as usize - 1] == STRING {
            nest = true;
        }
    }
    while b < s.len() && matches!(s[b], '^' | HAT | '=' | EQUALS | '~' | TILDE) {
        b += 1;
    }
    if matches!(s.get(b), Some(&'#') | Some(&POUND) | Some(&'+')) {
        b += 1;
    }
    let mut e = b;
    let q = if test { DNULL } else { '"' };
    while s.get(e) == Some(&q) {
        e += 1;
        z.comp.parq += 1;
    }
    if !test {
        b = e;
    }
    match s.get(e) {
        Some(&(QUEST | STAR | STRING | QSTRING | '?' | '*' | '$' | '-' | '!' | '@')) => e += 1,
        Some(c) if c.is_ascii_digit() => {
            while s.get(e).is_some_and(|c| c.is_ascii_digit()) {
                e += 1;
            }
        }
        _ => {
            let mut ie = itype_end_namespc(s, e);
            if ie != e {
                let patmatch = !st(&z.comp.comppatmatch).is_empty();
                loop {
                    e = ie;
                    ie = if patmatch && matches!(s.get(e), Some(&STAR) | Some(&QUEST)) {
                        e + 1
                    } else {
                        itype_end_namespc(s, e)
                    };
                    if ie == e {
                        break;
                    }
                }
            } else if s.get(e) == Some(&'.') {
                e += 1;
            }
        }
    }
    if offs <= e && offs >= b {
        let mut pp = e;
        while s.get(pp) == Some(&q) {
            pp += 1;
            z.comp.parq -= 1;
            z.comp.eparq += 1;
        }
        if test {
            return Some(s[b..].iter().collect());
        }
        if set {
            if br >= 2 {
                z.comp.mflags |= CMF_PARBR as i32;
                if nest {
                    z.comp.mflags |= CMF_PARNEST as i32;
                }
            }
            z.comp.isuf = Some(untok(&s[e..]));
            let head: String = s[..b].iter().collect();
            z.comp.ripre = Some(format!("{}{}", st(&z.comp.ripre), head));
            z.comp.ipre = Some(untok(&chars(&format!("{}{}", st(&z.comp.ipre), head))));
        }
        if z.comp.compfunc.is_some() {
            z.comp.parflags = if br >= 2 {
                (CMF_PARBR | if nest { CMF_PARNEST } else { 0 }) as i32
            } else {
                0
            };
            z.comp.parpre = Some(untok(&s[..b]));
        }
        z.comp.offs -= b as i32;
        set_wb(z.cs as i32 - z.comp.offs);
        set_we(wb() + (e - b) as i32);
        z.comp.ispar = if br >= 2 { 2 } else { 1 };
        return Some(s[b..e].iter().collect());
    } else if offs > e && s.get(e) == Some(&':') {
        // Probably in modifiers.
        z.comp.ispar = if br >= 2 { 2 } else { 1 };
    }
    None
}

// ---- compset -q ---------------------------------------------------------

/// compset -q: splits the current word into words as if it were a
/// command line, and completes the word of those under the cursor.
pub fn set_comp_sep(z: &mut Zle) -> i32 {
    let mut lip = 0;
    let mut lp = 0;
    let full = comp_str(z, Some(&mut lip), Some(&mut lp), true);
    let tail: Vec<char> = chars(&full).into_iter().skip(lip.max(0) as usize).collect();
    let s: Vec<char> = untok(&tail).chars().collect();
    let owb = wb();
    let owe = we();
    let noffs = (lp.max(0) as usize).min(s.len());
    let ois = z.comp.instring;
    let oib = z.comp.inbackt;

    let tl = s.len() as i32 + 2;
    let mut tmp: Vec<char> = Vec::with_capacity(s.len() + 2);
    tmp.push(' ');
    tmp.extend_from_slice(&s[..noffs]);
    tmp.push('x');
    tmp.extend_from_slice(&s[noffs..]);
    let mut scs = 1 + noffs as i32;
    let mut zcs = scs;

    let qs: Vec<char> = st(&z.comp.compqstack).chars().collect();
    let qttype = qs.first().map(|&c| c as i32).unwrap_or(QT_NONE);
    let (mut dq, mut sq, mut dolq, mut css, mut qa) = (0i32, 0i32, 0i32, 0i32, 0i32);
    let mut remq = false;
    match qttype {
        QT_BACKSLASH => {
            remq = true;
            tmp = rembslash(&tmp);
        }
        QT_SINGLE => {
            qa = if opt("rcquotes") { 1 } else { 3 };
            sq = remsquote(&mut tmp);
        }
        QT_DOUBLE => {
            let mut j = 0i32;
            let mut p = 0usize;
            while p < tmp.len() {
                if tmp[p] == '\\' && matches!(tmp.get(p + 1), Some('\\' | '"')) {
                    dq += 1;
                    tmp.remove(p);
                    if tmp[p] == '"' {
                        zcs -= 1;
                    } else if j > zcs {
                        zcs += 1;
                        css += 1;
                    }
                    if p >= tmp.len() {
                        break;
                    }
                }
                p += 1;
                j += 1;
            }
        }
        QT_DOLLARS => {
            let j = zcs;
            let cut = (zcs.max(0) as usize).min(tmp.len());
            let head: String = tmp[..cut].iter().collect();
            let all: String = tmp.iter().collect();
            zcs = clen(&crate::expand::decode_escapes(&head)) as i32;
            tmp = chars(&crate::expand::decode_escapes(&all));
            dolq = tl - tmp.len() as i32;
            css += zcs - j;
        }
        _ => {}
    }
    let odq = dq;

    let mut parts: Vec<Vec<char>> = Vec::new();
    let mut got = false;
    let mut cur: i32 = -1;
    let mut i = 0;
    let (mut swb, mut swe, mut soffs, mut sqq, mut lsq) = (0i32, 0i32, 0i32, 0i32, 0i32);
    let mut ns: Vec<char> = Vec::new();
    let oaddedx = addedx();
    set_addedx(1);
    // The lexer's line length leaves out the `x`.
    let mut lx = ZleLexer::new(&dupstrspace(&tmp), LEXFLAGS_ZLE, zcs, tl - 1);
    lx.noaliases = true;
    loop {
        lx.ctxtlex();
        if lx.tok == Lextok::Lexerr {
            let Some(t) = lx.tokstr.as_mut() else {
                break;
            };
            // An unterminated string: an odd number of quotes.
            if t.iter().filter(|&&c| c == SNULL || c == DNULL).count() & 1 != 0 {
                lx.tok = Lextok::String;
                if t.last() == Some(&' ') {
                    t.pop();
                }
            }
        }
        if lx.tok == Lextok::Endinput {
            break;
        }
        let mut idx = None;
        if let Some(tokstr) = lx.tokstr.as_ref().filter(|t| !t.is_empty()) {
            let mut k = 0;
            while dq != 0 && k < tokstr.len() {
                if tokstr[k] == BNULL {
                    dq -= 1;
                    if tokstr.get(k + 1) == Some(&'\\') {
                        dq -= 1;
                    }
                }
                k += 1;
            }
            if qttype == QT_SINGLE {
                lsq = 0;
                for &c in tokstr {
                    if sq != 0 && c == SNULL {
                        sq -= qa;
                    }
                    if c == '\'' {
                        sq -= qa;
                        lsq += qa;
                    }
                }
            } else {
                lsq = 0;
            }
            parts.push(tokstr.clone());
            idx = Some(parts.len() - 1);
        }
        if !got && lx.lexflags == 0 {
            got = true;
            if let Some(idx) = idx {
                cur = idx as i32;
                swb = wb() - dq - sq - dolq;
                swe = we() - dq - sq - dolq;
                sqq = lsq;
                soffs = zcs - swb - css;
                let w = &mut parts[idx];
                if soffs >= 0 && (soffs as usize) < w.len() {
                    w.remove(soffs as usize);
                }
                ns = w.clone();
            }
        }
        i += 1;
        if lx.tok == Lextok::Endinput || lx.tok == Lextok::Lexerr {
            break;
        }
    }
    set_addedx(oaddedx);
    set_wb(owb);
    set_we(owe);
    if cur < 0 || i < 1 {
        return 1;
    }
    let ooffs = z.comp.offs;
    z.comp.offs = soffs;
    if check_param(z, &ns, false, true).is_some() {
        for c in ns.iter_mut() {
            if *c == DNULL {
                *c = '"';
            } else if *c == SNULL {
                *c = '\'';
            }
        }
    }
    z.comp.offs = ooffs;

    let mut ts: Vec<char> = untok(&ns).chars().collect();
    let n0 = ns.first().copied();
    if matches!(n0, Some(SNULL) | Some(DNULL))
        || (matches!(n0, Some(STRING) | Some(QSTRING)) && ns.get(1) == Some(&SNULL))
    {
        let mut tsptr = 0;
        let mut nsptr = 0;
        z.comp.instring = match n0 {
            Some(SNULL) => QT_SINGLE,
            Some(DNULL) => QT_DOUBLE,
            _ => {
                nsptr += 1;
                tsptr += 1;
                swb += 1;
                QT_DOLLARS
            }
        };
        z.comp.inbackt = 0;
        swb += 1;
        let tail = &ns[nsptr..];
        if tail.len() > 1 && tail.last() == tail.first() {
            swe -= 1;
        }
        tsptr += 1;
        let tsptr = tsptr.min(ts.len());
        let head: String = ts[..tsptr].iter().collect();
        z.comp.autoq = Some(if qs.len() > 1 {
            String::new()
        } else {
            multiquote(z, &head, true)
        });
        ts = ts.split_off(tsptr);
    } else {
        z.comp.instring = QT_NONE;
        z.comp.autoq = None;
    }

    // Find where the word starts and the cursor is once the quotes go.
    let mut p = 0usize;
    let mut i = swb;
    while p < ns.len() {
        let c = ns[p];
        if inull(c) {
            if i < scs && c == BNULL {
                if p + 1 < ns.len() && remq {
                    swb -= 2;
                }
                if odq != 0 {
                    swb -= 1;
                    if ns.get(p + 1) == Some(&'\\') {
                        swb -= 1;
                    }
                }
            }
            if p + 1 < ns.len() || c != BNULL {
                if c == BNULL {
                    if scs == i + 1 {
                        scs += 1;
                        soffs += 1;
                    }
                } else {
                    let past = scs > i;
                    i -= 1;
                    if past {
                        scs -= 1;
                    }
                }
            } else if scs == swe {
                scs -= 1;
            }
            ns.remove(p);
            i += 1;
            continue;
        }
        p += 1;
        i += 1;
    }
    let mut ns: Vec<char> = ts;

    if z.comp.instring != 0 && qs.contains(&(QT_BACKSLASH as u8 as char)) {
        let nsstr: String = ns.iter().collect();
        let rl = ns.len() as i32;
        let ql = clen(&multiquote(z, &nsstr, qs.len() > 1)) as i32;
        if ql > rl {
            swb -= ql - rl;
        }
    }
    let clampi = |v: i32, len: usize| (v.max(0) as usize).min(len);
    let qi = clampi(swb - 1 - sqq + dq, s.len());
    let mut qp: Vec<char> = if qttype == QT_SINGLE {
        s[..qi].to_vec()
    } else {
        rembslash(&s[..qi])
    };
    if swe < swb {
        swe = swb;
    }
    swe -= 1;
    let sl = s.len() as i32;
    if swe > sl {
        swe = sl;
        let keep = (swe - swb + 1).max(0) as usize;
        if ns.len() > keep {
            ns.truncate(keep);
        }
    }
    let qsi = clampi(swe, s.len());
    let mut qsv: Vec<char> = if qttype == QT_SINGLE {
        s[qsi..].to_vec()
    } else {
        rembslash(&s[qsi..])
    };
    let soffs = clampi(soffs, ns.len());
    if qttype == QT_SINGLE {
        remsquote(&mut qp);
        remsquote(&mut qsv);
    }

    let (mut set, mut unset) = (CP_QUOTE | CP_QUOTING, 0);
    let head = if z.comp.instring == QT_NONE {
        QT_BACKSLASH
    } else {
        z.comp.instring
    };
    z.comp.compqstack = Some(format!("{}{}", head as u8 as char, st(&z.comp.compqstack)));
    let (q, qn) = match z.comp.instring {
        QT_DOUBLE => ("\"", "double"),
        QT_SINGLE => ("'", "single"),
        QT_DOLLARS => ("$'", "dollars"),
        _ => {
            unset = set;
            set = 0;
            ("", "")
        }
    };
    z.comp.compquote = Some(q.into());
    z.comp.compquoting = Some(qn.into());
    comp_setunset(z, 0, 0, set, unset);

    if !opt("completeinword") {
        z.comp.compprefix = Some(untok(&ns));
        z.comp.compsuffix = Some(String::new());
    } else {
        z.comp.compprefix = Some(untok(&ns[..soffs]));
        z.comp.compsuffix = Some(untok(&ns[soffs..]));
    }
    let pc = chars(st(&z.comp.compprefix));
    let n = pc.len();
    if n > 1 && pc[n - 1] == '\\' && pc[n - 2] != '\\' {
        z.comp.compprefix = Some(pc[..n - 1].iter().collect());
    }
    let qps: String = qp.iter().collect();
    let qss: String = qsv.iter().collect();
    z.comp.compqiprefix = Some(format!(
        "{}{}{}",
        st(&z.comp.compqiprefix),
        st(&z.comp.compiprefix),
        multiquote(z, &qps, true)
    ));
    z.comp.compqisuffix = Some(format!(
        "{}{}{}",
        multiquote(z, &qss, true),
        st(&z.comp.compisuffix),
        st(&z.comp.compqisuffix)
    ));
    z.comp.compiprefix = Some(String::new());
    z.comp.compisuffix = Some(String::new());
    z.comp.compwords = parts.iter().map(|w| untok(w)).collect();
    let nw = z.comp.compwords.len() as i64;
    z.comp.compcurrent = (cur as i64 + 1).min(nw);
    z.comp.instring = ois;
    z.comp.inbackt = oib;
    0
}

/// Sets array parameter `name` to the strings collected.
pub fn set_list_array(name: &str, l: Vec<String>) {
    crate::shell::array_set(name, l);
}

/// The words of `(...)` list `nam`, or of the parameter it names.
pub fn get_user_var(nam: Option<&str>) -> Option<Vec<String>> {
    let nam = nam?;
    if nam.starts_with('(') {
        let p = chars(nam);
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut notempty = false;
        let mut brk = false;
        let mut i = 1;
        while i < p.len() {
            let c = p[i];
            if c == '\\' && i + 1 < p.len() {
                cur.push(p[i + 1]);
                notempty = true;
                i += 1;
            } else if c == ',' || c == ' ' || c == '\t' || c == ')' {
                if c == ')' {
                    brk = true;
                }
                if notempty {
                    let w = cur.strip_prefix('\n').unwrap_or(&cur).to_string();
                    out.push(w);
                }
                cur.clear();
                notempty = false;
            } else {
                notempty = true;
                cur.push(c);
            }
            if brk {
                break;
            }
            i += 1;
        }
        if !brk || out.is_empty() {
            return None;
        }
        Some(out)
    } else if let Some(a) = crate::shell::array_get(nam) {
        Some(a)
    } else if let Some(v) = crate::shell::assoc_values(nam) {
        Some(v)
    } else if crate::shell::var_exists(nam) {
        Some(vec![crate::shell::var_get(nam)])
    } else {
        None
    }
}

// The values (or with `keys` the keys) of the parameter expression
// `name`, as compadd -a and -k take them.
fn get_data_arr(name: &str, keys: bool) -> Option<Vec<String>> {
    let word = format!("\"${{(@{}){}}}\"", if keys { "k" } else { "" }, name);
    crate::expand::expand_words(&[word]).ok()
}

// ---- matches ------------------------------------------------------------

fn cur_group(z: &mut Zle) -> Option<&mut Cmgroup> {
    let g = z.comp.mgroup?;
    z.comp.amatches.get_mut(g)
}

// One more match for the current explanation.
fn count_expl(z: &mut Zle, alt: bool) {
    let Some(e) = z.comp.curexpl.as_mut() else {
        return;
    };
    if alt {
        e.fcount += 1;
    } else {
        e.count += 1;
    }
    if let Some(k) = CUREXPL_AT.with(|c| c.get()) {
        if let Some(le) = cur_group(z).and_then(|g| g.lexpls.get_mut(k)) {
            if alt {
                le.fcount += 1;
            } else {
                le.count += 1;
            }
        }
    }
}

// The display strings of compadd -d, with the position of the one in
// use; it starts before the first, as zsh's pointer does.
type Disp = Option<(Vec<String>, isize)>;

// Steps to the next display string.
fn next_disp(disp: &mut Disp) {
    if let Some((d, k)) = disp.as_mut() {
        *k += 1;
        if *k as usize >= d.len() {
            *disp = None;
        }
    }
}

fn cur_disp(disp: &Disp) -> Option<String> {
    disp.as_ref().map(|(d, k)| d[*k as usize].clone())
}

// Adds a match with no matching (the <all> and dummy matches).
fn addmatch(z: &mut Zle, s: &str, flags: u32, disp: &mut Disp, line: bool) {
    let mut cm = Cmatch {
        str: Some(s.to_string()),
        flags: flags | listflags(z),
        ..Default::default()
    };
    next_disp(disp);
    if let Some(d) = cur_disp(disp) {
        cm.disp = Some(d);
    } else if line {
        cm.disp = Some(String::new());
        cm.flags |= CMF_DISPLINE;
    }
    z.comp.mnum += 1;
    if let Some(ai) = z.comp.ainfo.as_mut() {
        ai.count += 1;
    }
    count_expl(z, false);
    let cm = Rc::new(RefCell::new(cm));
    if let Some(g) = cur_group(z) {
        g.lmatches.push(cm);
        g.new = 1;
    }
    z.comp.newmatches = 1;
}

fn listflags(z: &Zle) -> u32 {
    let l = st(&z.comp.complist);
    (if l.contains("packed") { CMF_PACKED } else { 0 })
        | (if l.contains("rows") { CMF_ROWS } else { 0 })
}

fn strpfx(s: &str, t: &str) -> bool {
    t.starts_with(s)
}

fn strsfx(s: &str, t: &str) -> bool {
    t.ends_with(s)
}

fn pfxlen(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

fn append_cline(l: &mut Cline, tail: Cline) {
    let mut cur = l;
    while let Some(n) = cur {
        cur = &mut n.next;
    }
    *cur = tail;
}

fn last_cline(l: &mut Cline) -> Option<&mut ClineNode> {
    let mut cur = l.as_deref_mut()?;
    while cur.next.is_some() {
        cur = cur.next.as_deref_mut()?;
    }
    Some(cur)
}

// Removes the last node of a list.
fn pop_cline(l: &mut Cline) -> Cline {
    if l.as_ref().is_some_and(|n| n.next.is_none()) {
        return l.take();
    }
    let mut cur = l.as_deref_mut()?;
    while cur.next.as_ref().is_some_and(|n| n.next.is_some()) {
        cur = cur.next.as_deref_mut()?;
    }
    cur.next.take()
}

// Puts prefix parts `p` before `line`, moving the prefix of the last
// part into the line's first part when that part is otherwise empty.
fn prepend_parts(mut p: Cline, mut line: Cline, mask: u32) -> Cline {
    let line_flags = line.as_ref().map(|l| l.flags).unwrap_or(0);
    let merge = last_cline(&mut p).is_some_and(|lp| {
        lp.prefix.is_some()
            && line_flags & mask == 0
            && lp.llen == 0
            && lp.wlen == 0
            && lp.olen == 0
    });
    if merge {
        let mut lpre = last_cline(&mut p).and_then(|lp| lp.prefix.take());
        if let Some(ln) = line.as_mut() {
            append_cline(&mut lpre, ln.prefix.take());
            ln.prefix = lpre;
        }
        pop_cline(&mut p);
    }
    append_cline(&mut p, line);
    p
}

fn bld(z: &mut Zle, s: &str) -> Cline {
    let l = clen(s) as i32;
    super::matching::bld_parts(z, s, l, l)
}

/// Adds a match with all its data; `alt` puts it among the ones fignore
/// hid. Returns it, or None when it does not fit the braces in the line.
#[allow(clippy::too_many_arguments)]
pub fn add_match_data(
    z: &mut Zle,
    alt: bool,
    mstr: &str,
    orig: &str,
    mut line: Cline,
    ipre: Option<&str>,
    ripre: Option<&str>,
    isuf: Option<&str>,
    pre: Option<&str>,
    prpre: Option<&str>,
    ppre: Option<&str>,
    pline: &mut Cline,
    psuf: Option<&str>,
    sline: &mut Cline,
    suf: Option<&str>,
    flags: u32,
    exact: i32,
) -> Option<CmatchRef> {
    use super::matching::{cline_matched, join_clines};
    let qipre = z.comp.qipre.clone().unwrap_or_default();
    let qisuf = z.comp.qisuf.clone().unwrap_or_default();
    let nonempty = |s: Option<&str>| s.filter(|s| !s.is_empty()).map(str::to_string);

    cline_matched(&mut line);
    if pline.is_some() {
        cline_matched(pline);
    }
    if sline.is_some() {
        cline_matched(sline);
    }
    let mut sline_l = sline.clone();

    // Path and ignored suffixes go after the match's own parts.
    let psl = if sline_l.is_none() {
        psuf.map(clen).unwrap_or(0)
    } else {
        0
    };
    let isl = isuf.map(clen).unwrap_or(0);
    let qisl = clen(&qisuf);
    if psl + isl + qisl > 0 {
        if psl > 0 {
            let mut s = bld(z, psuf.unwrap_or(""));
            if let Some(sl) = sline_l.take() {
                let mut c = Some(sl);
                append_cline(&mut c, s);
                s = c;
            }
            let mergeable = last_cline(&mut line).is_some_and(|p| {
                p.flags & (CLF_SUF | CLF_MID) == 0 && p.llen == 0 && p.wlen == 0 && p.olen == 0
            });
            if mergeable && s.is_some() {
                if let (Some(mut p), Some(sn)) = (pop_cline(&mut line), s.as_mut()) {
                    if p.prefix.is_some() {
                        let mut pre = p.prefix.take();
                        append_cline(&mut pre, sn.prefix.take());
                        sn.prefix = pre;
                    }
                    sn.flags |= (p.flags & CLF_MATCHED) | CLF_MID;
                }
            }
            append_cline(&mut line, s);
        }
        if isl > 0 {
            let s = bld(z, isuf.unwrap_or(""));
            if let Some(sl) = sline_l.take() {
                append_cline(&mut line, Some(sl));
            }
            append_cline(&mut line, s);
        }
        if qisl > 0 {
            let mut q = bld(z, &qisuf);
            if let Some(qn) = q.as_mut() {
                qn.flags |= CLF_SUF;
                qn.suffix = qn.prefix.take();
            }
            if let Some(sl) = sline_l.take() {
                append_cline(&mut line, Some(sl));
            }
            append_cline(&mut line, q);
        }
    } else if let Some(sl) = sline_l.take() {
        append_cline(&mut line, Some(sl));
    }

    // The prefix is inserted more eagerly than the -S suffix.
    let qipl = clen(&qipre);
    let ipl = ipre.map(clen).unwrap_or(0);
    let pl = pre.map(clen).unwrap_or(0);
    let ppl = if pline.is_none() {
        ppre.map(clen).unwrap_or(0)
    } else {
        0
    };
    let palen = qipl + ipl + pl + ppl;
    if pl > 0 {
        if ppl > 0 || pline.is_some() {
            let p = if pline.is_some() {
                pline.clone()
            } else {
                bld(z, ppre.unwrap_or(""))
            };
            line = prepend_parts(p, line, CLF_SUF | CLF_MID);
        }
        for part in [pre.unwrap_or(""), ipre.unwrap_or(""), qipre.as_str()] {
            if !part.is_empty() {
                let mut p = bld(z, part);
                append_cline(&mut p, line);
                line = p;
            }
        }
    } else if palen > 0 || pline.is_some() {
        let mut p = if palen > 0 {
            let apre = format!(
                "{}{}{}{}",
                qipre,
                ipre.unwrap_or(""),
                pre.unwrap_or(""),
                if ppl > 0 { ppre.unwrap_or("") } else { "" }
            );
            bld(z, &apre)
        } else {
            None
        };
        if pline.is_some() {
            append_cline(&mut p, pline.clone());
        }
        line = prepend_parts(p, line, CLF_SUF);
    }

    let stl = clen(mstr);
    let isuf_v = if !qisuf.is_empty() {
        Some(match nonempty(isuf) {
            Some(i) => format!("{}{}", i, qisuf),
            None => qisuf.clone(),
        })
    } else {
        nonempty(isuf)
    };
    let ipre_v = if !qipre.is_empty() {
        Some(match nonempty(ipre) {
            Some(i) => format!("{}{}", qipre, i),
            None => qipre.clone(),
        })
    } else {
        nonempty(ipre)
    };
    let mut cm = Cmatch {
        str: Some(mstr.to_string()),
        orig: Some(orig.to_string()),
        ppre: nonempty(ppre),
        psuf: nonempty(psuf),
        prpre: if flags & CMF_FILE != 0 {
            nonempty(prpre)
        } else {
            None
        },
        ipre: ipre_v,
        ripre: nonempty(ripre),
        isuf: isuf_v,
        pre: pre.map(str::to_string),
        suf: suf.map(str::to_string),
        flags: flags | listflags(z),
        ..Default::default()
    };
    if flags & CMF_FILE != 0 && !orig.is_empty() && !orig.ends_with('/') {
        let pb = format!("{}{}", cm.prpre.as_deref().unwrap_or("./"), orig);
        if let Some(m) = ztat(&pb, true) {
            cm.mode = m;
            cm.modec = file_type(m);
            if cm.modec == ' ' {
                cm.modec = '\0';
            }
        }
        if let Some(m) = ztat(&pb, false) {
            cm.fmode = m;
            cm.fmodec = file_type(m);
            if cm.fmodec == ' ' {
                cm.fmodec = '\0';
            }
        }
    }
    let qs: Vec<char> = st(&z.comp.compqstack).chars().collect();
    let bs = QT_BACKSLASH as u8 as char;
    if (qs.first() == Some(&bs) && qs.len() > 1)
        || (z.comp.autoq.is_some() && !qs.is_empty() && qs.get(1) == Some(&bs))
    {
        cm.flags |= CMF_NOSPACE;
    }
    if z.comp.nbrbeg != 0 {
        cm.brpl = z.comp.brbeg.iter().map(|b| b.curpos).collect();
    }
    if z.comp.nbrend != 0 {
        cm.brsl = z.comp.brend.iter().map(|b| b.curpos).collect();
    }
    cm.qipl = qipl as i32;
    cm.qisl = qisl as i32;
    cm.autoq = match &z.comp.autoq {
        Some(a) => Some(a.clone()),
        None if z.comp.inbackt != 0 => Some("`".into()),
        None => None,
    };
    if z.comp.lastprebr.is_some() || z.comp.lastpostbr.is_some() {
        let (lpre, lpost) = (z.comp.lastprebr.clone(), z.comp.lastpostbr.clone());
        if !super::result::hasbrpsfx(z, &cm, lpre.as_deref(), lpost.as_deref()) {
            return None;
        }
    }

    let mut ai = if alt {
        z.comp.fainfo.take()
    } else {
        z.comp.ainfo.take()
    }
    .unwrap_or_default();
    ai.line = join_clines(z, ai.line.take(), line);
    z.comp.mnum += 1;
    ai.count += 1;

    let cmr = Rc::new(RefCell::new(cm));
    if let Some(g) = cur_group(z) {
        if alt {
            g.lfmatches.push(cmr.clone());
        } else {
            g.lmatches.push(cmr.clone());
        }
        g.new = 1;
    }
    z.comp.newmatches = 1;
    if alt {
        z.comp.compignored += 1;
    }
    if st(&z.comp.complastprompt).is_empty() {
        z.comp.dolastprompt = 0;
    }
    count_expl(z, alt);
    if ai.firstm.is_none() {
        ai.firstm = Some(cmr.clone());
    }
    let (lpl, lsl) = {
        let c = cmr.borrow();
        (
            c.ppre.as_deref().map(clen).unwrap_or(0),
            c.psuf.as_deref().map(clen).unwrap_or(0),
        )
    };
    let ml = (stl + lpl + lsl) as i32;
    if ml < z.comp.minmlen {
        z.comp.minmlen = ml;
    }
    if ml > z.comp.maxmlen {
        z.comp.maxmlen = ml;
    }
    if exact != 0 {
        if ai.exact == 0 {
            ai.exact = z.comp.useexact;
            if z.comp.incompfunc != 0 && st(&z.comp.compexactstr).is_empty() {
                // The exact string is shown in $compstate.
                let c = cmr.borrow();
                z.comp.compexactstr = Some(format!(
                    "{}{}{}",
                    c.ppre.as_deref().unwrap_or(""),
                    mstr,
                    c.psuf.as_deref().unwrap_or("")
                ));
                drop(c);
                comp_setunset(z, 0, 0, CP_EXACTSTR, 0);
            }
            ai.exactm = Some(cmr.clone());
        } else if z.comp.useexact != 0
            && ai
                .exactm
                .as_ref()
                .is_none_or(|e| !matcheq(&cmr.borrow(), &e.borrow()))
        {
            ai.exact = 2;
            ai.exactm = None;
            if z.comp.incompfunc != 0 {
                comp_setunset(z, 0, 0, 0, CP_EXACTSTR);
            }
        }
    }
    if alt {
        z.comp.fainfo = Some(ai);
    } else {
        z.comp.ainfo = Some(ai);
    }
    Some(cmr)
}

fn ztat(nam: &str, ls: bool) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let get = |p: &str| {
        if ls {
            std::fs::symlink_metadata(p)
        } else {
            std::fs::metadata(p)
        }
    };
    let unquoted: String = rembslash(&chars(nam)).into_iter().collect();
    get(nam).or_else(|_| get(&unquoted)).ok().map(|m| m.mode())
}

/// The character `ls -F` shows for a file mode.
pub fn file_type(mode: u32) -> char {
    let mode = mode as libc::mode_t;
    match mode & libc::S_IFMT {
        libc::S_IFBLK => '#',
        libc::S_IFCHR => '%',
        libc::S_IFDIR => '/',
        libc::S_IFIFO => '|',
        libc::S_IFLNK => '@',
        libc::S_IFREG => {
            if mode & 0o111 != 0 {
                '*'
            } else {
                ' '
            }
        }
        libc::S_IFSOCK => '=',
        _ => '?',
    }
}

/// compadd: adds the matches `argv` described by `dat`. Returns 1 when
/// none were added.
pub fn addmatches(z: &mut Zle, dat: &mut Cadata, argv: &[String]) -> i32 {
    use super::matching::{
        add_bmatchers, bld_parts, comp_match, match_str, revert_cline, update_bmatchers,
    };
    let nm = z.comp.mnum;
    let ohp = z.comp.haspattern;
    let ois = z.comp.instring;
    let oib = z.comp.inbackt;
    let oaq = z.comp.autoq.clone();
    let oppre = dat.ppre.clone();
    let oqp = z.comp.qipre.clone();
    let oqs = z.comp.qisuf.clone();
    let oms = z.comp.mstack.clone();

    if dat.dummies >= 0 {
        dat.aflags = (dat.aflags | CAF_NOSORT | CAF_UNIQCON) & !CAF_UNIQALL;
    }
    let af = dat.aflags;
    let gflags = (if af & CAF_NOSORT != 0 { CGF_NOSORT } else { 0 })
        | (if af & CAF_MATSORT != 0 {
            CGF_MATSORT
        } else {
            0
        })
        | (if af & CAF_NUMSORT != 0 {
            CGF_NUMSORT
        } else {
            0
        })
        | (if af & CAF_REVSORT != 0 {
            CGF_REVSORT
        } else {
            0
        })
        | (if af & CAF_UNIQALL != 0 {
            CGF_UNIQALL
        } else {
            0
        })
        | (if af & CAF_UNIQCON != 0 {
            CGF_UNIQCON
        } else {
            0
        });
    endcmgroup(z, None);
    match dat.group.clone() {
        Some(g) => begcmgroup(z, Some(&g), gflags),
        None => begcmgroup(z, Some("default"), 0),
    }
    CUREXPL_AT.with(|c| c.set(None));
    if dat.mesg.is_some() || dat.exp.is_some() {
        z.comp.curexpl = Some(Cexpl {
            always: dat.mesg.is_some(),
            str: dat.mesg.clone().or_else(|| dat.exp.clone()),
            count: 0,
            fcount: 0,
        });
        if dat.mesg.is_some() && dat.dpar.is_empty() && dat.opar.is_none() && dat.apar.is_none() {
            addexpl(z, true);
        }
    } else {
        z.comp.curexpl = None;
    }

    if argv.is_empty() && dat.dummies == 0 && dat.aflags & CAF_ALL == 0 {
        return 1;
    }

    let quoted = dat.aflags & CAF_QUOTE != 0;
    for b in z.comp.brbeg.iter_mut().chain(z.comp.brend.iter_mut()) {
        b.curpos = if quoted { b.pos } else { b.qpos };
    }
    if dat.flags & CMF_ISPAR != 0 {
        dat.flags |= z.comp.parflags as u32;
    }
    match st(&z.comp.compquote).chars().next() {
        Some('`') => {
            z.comp.instring = QT_NONE;
            z.comp.inbackt = 0;
            z.comp.autoq = Some(String::new());
        }
        Some(qc) => {
            match qc {
                '\'' => z.comp.instring = QT_SINGLE,
                '"' => z.comp.instring = QT_DOUBLE,
                '$' => z.comp.instring = QT_DOLLARS,
                _ => {}
            }
            z.comp.inbackt = 0;
            let cq = st(&z.comp.compquote).to_string();
            let cq = if qc == '$' { &cq[1..] } else { &cq[..] };
            z.comp.autoq = Some(multiquote(z, cq, true));
        }
        None => {
            z.comp.instring = QT_NONE;
            z.comp.inbackt = 0;
            z.comp.autoq = None;
        }
    }
    z.comp.qipre = Some(st(&z.comp.compqiprefix).to_string());
    z.comp.qisuf = Some(st(&z.comp.compqisuffix).to_string());
    z.comp.useexact = i32::from(st(&z.comp.compexact) == "accept");

    let doadd = dat.apar.is_none() && dat.opar.is_none() && dat.dpar.is_empty();
    if doadd {
        if dat.aflags & CAF_MATCH != 0 {
            z.comp.hasmatched = 1;
        } else {
            z.comp.hasunmatched = 1;
        }
    }
    let mut aparl: Vec<String> = Vec::new();
    let mut oparl: Vec<String> = Vec::new();
    // -D arrays with something in them, each with the element next.
    let mut dparr: Vec<Option<(Vec<String>, usize)>> = Vec::new();
    let mut dparl: Vec<Vec<String>> = Vec::new();
    if !dat.dpar.is_empty() {
        let mut darr = 0;
        while darr < dat.dpar.len() {
            match crate::shell::array_get(&dat.dpar[darr]).filter(|a| !a.is_empty()) {
                Some(a) => {
                    dparr.push(Some((a, 0)));
                    dparl.push(Vec::new());
                    darr += 1;
                }
                None => {
                    // Swap in the last -D argument.
                    let last = dat.dpar.pop().unwrap_or_default();
                    if darr < dat.dpar.len() {
                        dat.dpar[darr] = last;
                    }
                }
            }
        }
    }
    if let Some(m) = dat.match_.clone() {
        z.comp.mstack.insert(
            0,
            Cmlist {
                matcher: Some(m.clone()),
                str: String::new(),
            },
        );
        add_bmatchers(z, &m);
        z.comp.matchers.push(m);
    }
    if z.comp.mnum != 0 && (!z.comp.mstack.is_empty() || !z.comp.bmatchers.is_empty()) {
        update_bmatchers(z);
    }

    // The suffixes to ignore (fignore).
    let mut aign: Vec<String> = Vec::new();
    let mut pign: Vec<Pattern> = Vec::new();
    if let Some(a) = dat.ign.clone().and_then(|i| get_user_var(Some(&i))) {
        for tmp in a {
            let t = remnulargs(&tokenize(&chars(&tmp)));
            let q = |k: usize| t.get(k).copied();
            if ((q(0) == Some(QUEST) && q(1) == Some(STAR))
                || (q(1) == Some(QUEST) && q(0) == Some(STAR)))
                && t.len() > 2
                && !haswilds(&t[2..])
            {
                aign.push(untok(&t[2..]));
            } else {
                pign.push(Pattern::new(&pat_from_tokens(&t)));
            }
        }
    }
    let mut disp: Disp = dat
        .disp
        .clone()
        .and_then(|d| get_user_var(Some(&d)))
        .map(|d| (d, -1));

    let matching = dat.aflags & CAF_MATCH != 0;
    let mut lipre: Option<String> = None;
    let mut lisuf: Option<String> = None;
    let mut lpre = String::new();
    let mut lsuf = String::new();
    let mut llpl = 0usize;
    let mut llsl = 0usize;
    if matching {
        lipre = Some(st(&z.comp.compiprefix).to_string());
        lisuf = Some(st(&z.comp.compisuffix).to_string());
        lpre = st(&z.comp.compprefix).to_string();
        lsuf = st(&z.comp.compsuffix).to_string();
        llpl = clen(&lpre);
        llsl = clen(&lsuf);
        if (llpl + clen(st(&z.comp.qipre)) + clen(st(&lipre))) as i32 != z.comp.origlpre
            || (llsl + clen(st(&z.comp.qisuf)) + clen(st(&lisuf))) as i32 != z.comp.origlsuf
        {
            z.comp.lenchanged = 1;
        }
        // An existing -P prefix.
        if let Some(pre) = dat.pre.as_deref().filter(|p| !p.is_empty()) {
            let k = pfxlen(pre, &lpre);
            if k == clen(pre) || k == clen(&lpre) {
                llpl -= k;
                lpre = lpre.chars().skip(k).collect();
            }
        }
    }
    if let Some(i) = dat.ipre.take() {
        dat.ipre = Some(match &lipre {
            Some(l) => format!("{}{}", l, i),
            None => i,
        });
    } else if lipre.is_some() {
        dat.ipre = lipre.clone();
    }
    if let Some(i) = dat.isuf.take() {
        dat.isuf = Some(match &lisuf {
            Some(l) => format!("{}{}", l, i),
            None => i,
        });
    } else if lisuf.is_some() {
        dat.isuf = lisuf.clone();
    }
    let lpl = match dat.ppre.take() {
        Some(p) => {
            let q = if dat.flags & CMF_FILE != 0 {
                tildequote(z, &p, quoted)
            } else {
                multiquote(z, &p, quoted)
            };
            let n = clen(&q);
            dat.ppre = Some(q);
            n
        }
        None => 0,
    };
    let lsl = match dat.psuf.take() {
        Some(p) => {
            let q = multiquote(z, &p, quoted);
            let n = clen(&q);
            dat.psuf = Some(q);
            n
        }
        None => 0,
    };

    let mut args: Vec<String> = argv.to_vec();
    let mut cp: Option<Pattern> = None;
    let mut bpl: Option<usize> = if z.comp.brbeg.is_empty() {
        None
    } else {
        Some(0)
    };
    let mut bsl: Option<usize> = if z.comp.brend.is_empty() {
        None
    } else {
        Some(0)
    };
    let (mut bcp, mut bcs, mut bpadd, mut bsadd) = (0i32, 0i32, 0i32, 0i32);
    let mut pline: Cline = None;
    let mut sline: Cline = None;
    if matching {
        let mut globflag = String::new();
        if !st(&z.comp.comppatmatch).is_empty() && dat.ppre.is_some() && lpre.starts_with("(#") {
            let lc = chars(&lpre);
            if let Some(k) = lc.iter().skip(2).position(|&c| c == ')') {
                let gfl = k + 3;
                globflag = lc[..gfl].iter().collect();
                lpre = lc[gfl..].iter().collect();
                llpl = llpl.saturating_sub(gfl);
            }
        }
        if let Some(s) = dat.ppre.clone() {
            let ml = match_str(z, &lpre, &s, &mut bpl, 0, None, 0, 0, 1);
            if ml >= 0 {
                if let Some(subs) = z.comp.matchsubs.take() {
                    let tmp = ClineNode {
                        prefix: Some(subs),
                        ..Default::default()
                    };
                    append_cline(&mut z.comp.matchparts, Some(Box::new(tmp)));
                }
                pline = z.comp.matchparts.take();
                lpre = lpre.chars().skip(ml as usize).collect();
                llpl = llpl.saturating_sub(ml as usize);
                bcp = ml;
                bpadd = clen(&s) as i32 - ml;
            } else {
                if llpl <= lpl && strpfx(&lpre, &s) {
                    lpre.clear();
                } else if llpl > lpl && strpfx(&s, &lpre) {
                    lpre = lpre.chars().skip(lpl).collect();
                } else {
                    args.clear();
                }
                bcp = lpl as i32;
            }
        }
        if let Some(s) = dat.psuf.clone() {
            let ml = match_str(z, &lsuf, &s, &mut bsl, 0, None, 1, 0, 1);
            if ml >= 0 {
                if let Some(subs) = z.comp.matchsubs.take() {
                    let tmp = ClineNode {
                        flags: CLF_SUF,
                        suffix: Some(subs),
                        ..Default::default()
                    };
                    append_cline(&mut z.comp.matchparts, Some(Box::new(tmp)));
                }
                sline = revert_cline(z.comp.matchparts.take());
                lsuf = csub(&lsuf, 0, llsl.saturating_sub(ml as usize));
                bcs = ml;
                bsadd = clen(&s) as i32 - ml;
            } else {
                if llsl <= lsl && strsfx(&lsuf, &s) {
                    lsuf.clear();
                } else if llsl > lsl && strsfx(&s, &lsuf) {
                    lsuf = csub(&lsuf, 0, llsl - lsl);
                } else {
                    args.clear();
                }
                bcs = lsl as i32;
            }
        }
        let pm = st(&z.comp.comppatmatch).to_string();
        if !pm.is_empty() {
            let is = pm.starts_with('*');
            let gfl = clen(&globflag);
            // zsh writes the 'x' at the old prefix length; when the
            // prefix got shorter the rest is cut off.
            let whole = clen(&lpre) == llpl;
            let tmp = if whole {
                format!("{}{}{}{}", globflag, lpre, if is { "x" } else { "" }, lsuf)
            } else {
                format!("{}{}", globflag, lpre)
            };
            let mut t = tokenize(&chars(&tmp));
            if haswilds(&t) {
                if is && whole {
                    if let Some(c) = t.get_mut(llpl + gfl) {
                        *c = STAR;
                    }
                }
                let t = remnulargs(&t);
                cp = Some(Pattern::new(&pat_from_tokens(&t)));
                z.comp.haspattern = 1;
            }
        }
    } else if !st(&z.comp.comppatmatch).is_empty() {
        // Callers that matched themselves pass the prefix and suffix
        // with the pattern, so ambiguity is still seen.
        let tmp = format!("{}{}", st(&z.comp.compprefix), st(&z.comp.compsuffix));
        let t = remnulargs(&tokenize(&chars(&tmp)));
        if haswilds(&t) {
            z.comp.haspattern = 1;
        }
    }
    if !args.is_empty() {
        if dat.prpre.is_none() {
            if let Some(op) = &oppre {
                dat.prpre = Some(crate::expand::expand_string(op).unwrap_or_else(|_| op.clone()));
            }
        }
        if dat.remf.is_some() {
            dat.rems = None;
        }
        if matching {
            lpre = if !quoted && dat.ppre.is_none() && dat.flags & CMF_FILE != 0 {
                tildequote(z, &lpre, true)
            } else {
                multiquote(z, &lpre, true)
            };
            lsuf = multiquote(z, &lsuf, true);
        }
    }

    // Walk through the matches given.
    let obpl = bpl;
    let obsl = bsl;
    let arrays_mode = dat.aflags & CAF_ARRAYS != 0;
    let keys = dat.aflags & CAF_KEYS != 0;
    let mut arrays: Vec<String> = Vec::new();
    let mut ai = 0;
    let mut cur: Vec<String> = if arrays_mode {
        arrays = std::mem::take(&mut args);
        next_array(&arrays, &mut ai, keys)
    } else {
        args
    };
    let qu = if !quoted {
        if dat.ppre.is_some() || dat.flags & CMF_FILE == 0 {
            1
        } else {
            2
        }
    } else {
        0
    };
    let mut ci = 0;
    while ci < cur.len() {
        let s = cur[ci].clone();
        'one: {
            bpl = obpl;
            bsl = obsl;
            next_disp(&mut disp);
            if !aign.is_empty() || !pign.is_empty() {
                let ibuf = format!(
                    "{}{}{}",
                    dat.ppre.as_deref().unwrap_or(""),
                    s,
                    dat.psuf.as_deref().unwrap_or("")
                );
                let il = clen(&ibuf);
                let mut addit = true;
                for pt in &aign {
                    if !addit {
                        break;
                    }
                    addit = !(clen(pt) < il && ibuf.ends_with(pt.as_str()));
                }
                if addit {
                    for pt in &pign {
                        if !addit {
                            break;
                        }
                        addit = !pt.matches(&ibuf);
                    }
                }
                if !addit {
                    z.comp.compignored += 1;
                    advance_dpar(&mut dparr);
                    break 'one;
                }
            }
            let mut lc: Cline = None;
            let mut isexact = 0;
            let ms = if !matching {
                let ms = if quoted {
                    s.clone()
                } else {
                    multiquote(z, &s, false)
                };
                let l = clen(&ms) as i32;
                lc = bld_parts(z, &ms, l, -1);
                ms
            } else {
                match comp_match(
                    z,
                    &lpre,
                    &lsuf,
                    &s,
                    cp.as_ref(),
                    &mut lc,
                    qu,
                    &mut bpl,
                    bcp,
                    &mut bsl,
                    bcs,
                    &mut isexact,
                ) {
                    Some(m) => m,
                    None => {
                        advance_dpar(&mut dparr);
                        break 'one;
                    }
                }
            };
            if doadd {
                if let Some(k) = obpl {
                    for b in z.comp.brbeg.iter_mut().skip(k) {
                        b.curpos += bpadd;
                    }
                }
                if let Some(k) = obsl {
                    for b in z.comp.brend.iter_mut().skip(k) {
                        b.curpos += bsadd;
                    }
                }
                if let Some(cm) = add_match_data(
                    z,
                    false,
                    &ms,
                    &s,
                    lc,
                    dat.ipre.as_deref(),
                    None,
                    dat.isuf.as_deref(),
                    dat.pre.as_deref(),
                    dat.prpre.as_deref(),
                    dat.ppre.as_deref(),
                    &mut pline,
                    dat.psuf.as_deref(),
                    &mut sline,
                    dat.suf.as_deref(),
                    dat.flags,
                    isexact,
                ) {
                    let mut c = cm.borrow_mut();
                    c.rems = dat.rems.clone();
                    c.remf = dat.remf.clone();
                    if let Some(d) = cur_disp(&disp) {
                        c.disp = Some(d);
                    }
                }
            } else {
                if dat.apar.is_some() {
                    aparl.push(ms);
                }
                if dat.opar.is_some() {
                    oparl.push(s.clone());
                }
                for (d, l) in dparr.iter_mut().zip(dparl.iter_mut()) {
                    if let Some((a, k)) = d {
                        l.push(a[*k].clone());
                        *k += 1;
                        if *k >= a.len() {
                            *d = None;
                        }
                    }
                }
            }
        }
        if arrays_mode && ci + 1 >= cur.len() {
            cur = next_array(&arrays, &mut ai, keys);
            ci = 0;
            continue;
        }
        ci += 1;
    }
    if let Some(a) = &dat.apar {
        set_list_array(a, aparl);
    }
    if let Some(o) = &dat.opar {
        set_list_array(o, oparl);
    }
    for (name, l) in dat.dpar.iter().zip(dparl) {
        set_list_array(name, l);
    }
    if dat.exp.is_some() {
        addexpl(z, false);
    }
    if z.comp.hasallmatch == 0 && dat.aflags & CAF_ALL != 0 {
        addmatch(z, "<all>", dat.flags | CMF_ALL, &mut disp, true);
        z.comp.hasallmatch = 1;
    }
    while dat.dummies > 0 {
        dat.dummies -= 1;
        addmatch(z, "", dat.flags | CMF_DUMMY, &mut disp, false);
    }

    z.comp.mstack = oms;
    z.comp.instring = ois;
    z.comp.inbackt = oib;
    z.comp.autoq = oaq;
    z.comp.qipre = oqp;
    z.comp.qisuf = oqs;
    if z.comp.mnum == nm {
        z.comp.haspattern = ohp;
    }
    i32::from(z.comp.mnum == nm)
}

// Steps each -D array past the element of a match not added.
fn advance_dpar(dparr: &mut [Option<(Vec<String>, usize)>]) {
    for d in dparr.iter_mut() {
        if let Some((a, k)) = d {
            *k += 1;
            if *k >= a.len() {
                *d = None;
            }
        }
    }
}

// The next array with elements for compadd -a / -k, from arrays[*ai].
fn next_array(arrays: &[String], ai: &mut usize, keys: bool) -> Vec<String> {
    while *ai < arrays.len() {
        let name = &arrays[*ai];
        *ai += 1;
        if let Some(a) = get_data_arr(name, keys).filter(|a| !a.is_empty()) {
            return a;
        }
    }
    Vec::new()
}

/// Starts a new group of matches, or goes back to one of that name.
pub fn begcmgroup(z: &mut Zle, n: Option<&str>, flags: u32) {
    CUREXPL_AT.with(|c| c.set(None));
    if let Some(n) = n {
        let mask = CGF_NOSORT | CGF_UNIQALL | CGF_UNIQCON | CGF_MATSORT | CGF_NUMSORT | CGF_REVSORT;
        if let Some(k) = z
            .comp
            .amatches
            .iter()
            .position(|p| p.name.as_deref() == Some(n) && flags == p.flags & mask)
        {
            z.comp.mgroup = Some(k);
            return;
        }
    }
    let g = Cmgroup {
        name: n.map(str::to_string),
        flags,
        ..Default::default()
    };
    z.comp.amatches.insert(0, g);
    z.comp.mgroup = Some(0);
}

/// Ends the current group for now.
pub fn endcmgroup(z: &mut Zle, ylist: Option<Vec<String>>) {
    if let Some(g) = cur_group(z) {
        g.ylist = ylist.unwrap_or_default();
    }
}

/// Adds the current explanation to the group, joining duplicates.
pub fn addexpl(z: &mut Zle, always: bool) {
    let Some(cur) = z.comp.curexpl.clone() else {
        return;
    };
    let mgroup = z.comp.mgroup;
    let Some(g) = mgroup.and_then(|k| z.comp.amatches.get_mut(k)) else {
        return;
    };
    if let Some(e) = g.lexpls.iter_mut().find(|e| e.str == cur.str) {
        e.count += cur.count;
        e.fcount += cur.fcount;
        if always {
            e.always = true;
            g.new = 1;
            z.comp.nmessages += 1;
            z.comp.newmatches = 1;
        }
        return;
    }
    g.lexpls.push(cur);
    CUREXPL_AT.with(|c| c.set(Some(g.lexpls.len() - 1)));
    z.comp.newmatches = 1;
    if always {
        g.new = 1;
        z.comp.nmessages += 1;
    }
}

// strcoll(3), as zsh sorts with it.
fn strcoll(a: &[u8], b: &[u8]) -> i32 {
    // The locale may have changed since the last comparison.
    crate::shell::sync_lc_collate();
    match (std::ffi::CString::new(a), std::ffi::CString::new(b)) {
        (Ok(x), Ok(y)) => {
            // SAFETY: both are valid NUL-terminated strings.
            let r = unsafe { libc::strcoll(x.as_ptr(), y.as_ptr()) };
            r.signum()
        }
        _ => a.cmp(b) as i32,
    }
}

/// Compares strings as zsh's sort does: `nobslash` ignores backslashes,
/// `numeric` (1, or -1 for signed) compares digit runs by value.
pub fn zstrcmp(a: &str, b: &str, nobslash: bool, numeric: i32) -> i32 {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let at = |v: &[u8], k: usize| v.get(k).copied().unwrap_or(0);
    let dig = |c: u8| c.is_ascii_digit();
    let (mut i, mut j) = (0usize, 0usize);
    if nobslash {
        while at(a, i) != 0 && at(b, j) != 0 {
            if at(a, i) == b'\\' {
                i += 1;
            }
            if at(b, j) == b'\\' {
                j += 1;
            }
            if at(a, i) != at(b, j) || at(a, i) == 0 {
                break;
            }
            i += 1;
            j += 1;
        }
    }
    let mut cmp = strcoll(&a[i.min(a.len())..], &b[j.min(b.len())..]);
    if numeric != 0 {
        let mut mul = 0;
        while at(a, i) == at(b, j) && at(a, i) != 0 {
            i += 1;
            j += 1;
        }
        if numeric < 0 {
            if at(a, i) == b'-' && dig(at(a, i + 1)) && dig(at(b, j)) {
                cmp = -1;
                mul = 1;
            } else if at(b, j) == b'-' && dig(at(b, j + 1)) && dig(at(a, i)) {
                cmp = 1;
                mul = 1;
            }
        }
        if mul == 0 && (dig(at(a, i)) || dig(at(b, j))) {
            while i > 0 && j > 0 && dig(a[i - 1]) {
                i -= 1;
                j -= 1;
            }
            mul = if numeric < 0 && i > 0 && a[i - 1] == b'-' {
                -1
            } else {
                1
            };
            if dig(at(a, i)) && dig(at(b, j)) {
                while at(a, i) == b'0' {
                    i += 1;
                }
                while at(b, j) == b'0' {
                    j += 1;
                }
                while dig(at(a, i)) && at(a, i) == at(b, j) {
                    i += 1;
                    j += 1;
                }
                if dig(at(a, i)) || dig(at(b, j)) {
                    cmp = mul * (at(a, i) as i32 - at(b, j) as i32);
                    while dig(at(a, i)) && dig(at(b, j)) {
                        i += 1;
                        j += 1;
                    }
                    if dig(at(a, i)) && !dig(at(b, j)) {
                        return mul;
                    }
                    if dig(at(b, j)) && !dig(at(a, i)) {
                        return -mul;
                    }
                }
            }
        }
    }
    cmp
}

// zsh's matchcmp, `order` being the group's flags.
fn matchcmp(a: &Cmatch, b: &Cmatch, order: u32, numglob: bool) -> std::cmp::Ordering {
    let cmp = i32::from(b.disp.is_some()) - i32::from(a.disp.is_some());
    let sortdir = if order & CGF_REVSORT != 0 { -1 } else { 1 };
    let (x, y) = if order & CGF_MATSORT != 0 || (cmp == 0 && a.disp.is_none()) {
        (st(&a.str), st(&b.str))
    } else {
        if cmp != 0 {
            return cmp.cmp(&0);
        }
        let c = (b.flags & CMF_DISPLINE) as i64 - (a.flags & CMF_DISPLINE) as i64;
        if c != 0 {
            return c.cmp(&0);
        }
        (st(&a.disp), st(&b.disp))
    };
    let numeric = i32::from(numglob || order & CGF_NUMSORT != 0);
    (sortdir * zstrcmp(x, y, true, numeric)).cmp(&0)
}

/// Whether two matches would put the same text in the line.
pub fn matcheq(a: &Cmatch, b: &Cmatch) -> bool {
    a.ipre == b.ipre
        && a.pre == b.pre
        && a.ppre == b.ppre
        && a.psuf == b.psuf
        && a.suf == b.suf
        && a.str == b.str
}

// Drops runs of equal matches, keeping the first of each, and marks
// the matches showing the same string as the last of a run (CMF_MULT,
// and CMF_FMULT on that one).
fn dedup_consecutive(rp: &[CmatchRef]) -> Vec<CmatchRef> {
    let mut out = Vec::new();
    let mut k = 0;
    while k < rp.len() {
        out.push(rp[k].clone());
        let mut bp = k;
        while bp + 1 < rp.len() && matcheq(&rp[k].borrow(), &rp[bp + 1].borrow()) {
            bp += 1;
        }
        let ap = bp;
        let mut dup = false;
        while bp + 1 < rp.len() && {
            let (x, y) = (rp[ap].borrow(), rp[bp + 1].borrow());
            x.disp.is_none() && y.disp.is_none() && x.str == y.str
        } {
            rp[bp + 1].borrow_mut().flags |= CMF_MULT;
            dup = true;
            bp += 1;
        }
        if dup {
            rp[ap].borrow_mut().flags |= CMF_FMULT;
        }
        k = ap + 1;
    }
    out
}

/// Sorts and uniquifies a group's matches as its flags say. Returns them
/// with the number of matches, of unlisted ones and of one-per-line
/// display strings.
pub fn makearray(l: &[CmatchRef], flags: u32) -> (Vec<CmatchRef>, i32, i32, i32) {
    let mut rp: Vec<CmatchRef> = l.to_vec();
    let numglob = opt("numericglobsort");
    let (mut nl, mut ll) = (0, 0);
    if !rp.is_empty() {
        let cmp = |a: &CmatchRef, b: &CmatchRef| matchcmp(&a.borrow(), &b.borrow(), flags, numglob);
        if flags & CGF_NOSORT == 0 {
            rp.sort_by(cmp);
            // Sorted, -1 is a no-op; only -2 keeps duplicates.
            if flags & CGF_UNIQCON == 0 {
                rp = dedup_consecutive(&rp);
            }
        } else if flags & CGF_UNIQALL == 0 && flags & CGF_UNIQCON == 0 {
            // Remove all duplicates: find them in a sorted copy.
            let mut sp = rp.clone();
            sp.sort_by(cmp);
            let mut del = false;
            let mut asp = 1;
            while asp < sp.len() {
                let ap = asp - 1;
                let mut bp = asp;
                if matcheq(&sp[ap].borrow(), &sp[bp].borrow()) {
                    sp[bp].borrow_mut().flags = CMF_DELETE;
                    del = true;
                } else if sp[ap].borrow().disp.is_none() {
                    let mut dup = false;
                    while bp < sp.len() && {
                        let (x, y) = (sp[ap].borrow(), sp[bp].borrow());
                        y.disp.is_none() && x.str == y.str
                    } {
                        sp[bp].borrow_mut().flags |= CMF_MULT;
                        dup = true;
                        asp += 1;
                        bp = asp;
                    }
                    if dup {
                        sp[ap].borrow_mut().flags |= CMF_FMULT;
                        asp -= 1;
                    }
                }
                asp += 1;
            }
            if del {
                rp.retain(|m| m.borrow().flags & CMF_DELETE == 0);
            }
        } else if flags & CGF_UNIQCON == 0 {
            // -1: only consecutive duplicates.
            rp = dedup_consecutive(&rp);
        }
        for m in &rp {
            let m = m.borrow();
            if m.disp.is_some() && m.flags & CMF_DISPLINE != 0 {
                ll += 1;
            }
            if m.flags & (CMF_NOLIST | CMF_MULT) != 0 {
                nl += 1;
            }
        }
    }
    let n = rp.len() as i32;
    (rp, n, nl, ll)
}

fn dupmatch(m: &CmatchRef) -> CmatchRef {
    Rc::new(RefCell::new(m.borrow().clone()))
}

/// Makes the permanent copy of the groups (zsh's pmatches). With `last`
/// this is the final list. Returns whether only fignore'd matches are
/// there.
pub fn permmatches(z: &mut Zle, last: bool) -> bool {
    let ofi = PERM_FI.with(|f| f.get());
    let c = &mut z.comp;
    if !c.pmatches.is_empty() && c.newmatches == 0 {
        if last && ofi {
            c.ainfo = c.fainfo.clone();
        }
        return ofi;
    }
    c.newmatches = 0;
    let mut fi = false;
    c.pmatches.clear();
    c.nmatches = 0;
    c.smatches = 0;
    c.diffmatches = 0;
    if c.ainfo.as_ref().is_none_or(|a| a.count == 0) {
        if last {
            c.ainfo = c.fainfo.clone();
        }
        fi = true;
    }
    let mut gn = 1;
    let mut pm: Vec<Cmgroup> = Vec::new();
    for g in c.amatches.iter_mut() {
        let mlist = if fi { &g.lfmatches } else { &g.lmatches };
        let (m, nn, nl, ll) = makearray(mlist, g.flags);
        g.matches = m;
        g.mcount = nn;
        g.lcount = (nn - nl).max(0);
        g.llcount = ll;
        if !g.ylist.is_empty() {
            g.lcount = g.ylist.len() as i32;
            c.smatches = 2;
        }
        g.expls = g.lexpls.clone();
        g.ecount = g.expls.len() as i32;
        g.ccount = 0;
        c.nmatches += g.mcount;
        c.smatches += g.lcount;
        if g.mcount > 1 {
            c.diffmatches = 1;
        }
        let n = Cmgroup {
            num: gn,
            flags: g.flags,
            mcount: g.mcount,
            matches: g.matches.iter().map(dupmatch).collect(),
            name: g.name.clone(),
            lcount: g.lcount,
            llcount: g.llcount,
            ylist: g.ylist.clone(),
            ecount: g.ecount,
            expls: g
                .expls
                .iter()
                .map(|o| Cexpl {
                    count: if fi { o.fcount } else { o.count },
                    always: o.always,
                    fcount: 0,
                    str: o.str.clone(),
                })
                .collect(),
            ..Default::default()
        };
        gn += 1;
        pm.insert(0, n);
        g.new = 0;
    }
    let mut mn = 1;
    let mut first: Option<CmatchRef> = None;
    for g in pm.iter_mut() {
        g.nbrbeg = c.nbrbeg;
        g.nbrend = c.nbrend;
        for (rn, m) in g.matches.iter().enumerate() {
            let mut m = m.borrow_mut();
            m.rnum = rn as i32 + 1;
            m.gnum = mn;
            mn += 1;
        }
        if c.diffmatches == 0 {
            if let Some(m0) = g.matches.first() {
                match &first {
                    Some(p) => {
                        if !matcheq(&m0.borrow(), &p.borrow()) {
                            c.diffmatches = 1;
                        }
                    }
                    None => first = Some(m0.clone()),
                }
            }
        }
    }
    c.pmatches = pm;
    c.hasperm = 1;
    c.permmnum = mn - 1;
    c.permgnum = gn - 1;
    c.listdat.valid = 0;
    PERM_FI.with(|f| f.set(fi));
    fi
}

/// Drops a list of groups; with `cm` the menu loses its current match.
pub fn freematches(z: &mut Zle, g: Vec<Cmgroup>, cm: bool) {
    drop(g);
    if cm {
        z.comp.minfo.cur = None;
    }
}

// ---- running the completion function ------------------------------------

/// Puts the line back as it was before completion.
fn restore_origline(z: &mut Zle) {
    z.cs = 0;
    let ll = z.ll();
    z.foredel(ll, CUT_RAW);
    let orig: Vec<char> = st(&z.comp.origline).chars().collect();
    z.insert_chars(&orig);
    z.cs = (z.comp.origcs.max(0) as usize).min(z.ll());
}

/// The completion entry point: completes tokenized word `s`.
pub fn do_completion(z: &mut Zle, s: &str, lst: i32, incmd: i32) -> i32 {
    use super::result::{do_allmatches, do_ambig_menu, do_ambiguous, do_single};
    let s = chars(s);
    let osl = z.showinglist;
    let mut ret = 0;
    z.comp.ainfo = None;
    z.comp.fainfo = None;
    z.comp.matchers.clear();
    let top = if z.comp.instring == QT_NONE {
        QT_BACKSLASH
    } else {
        z.comp.instring
    };
    z.comp.compqstack = Some((top as u8 as char).to_string());
    z.comp.useline = if z.comp.wouldinstab != 0 {
        -1
    } else {
        i32::from(lst != COMP_LIST_COMPLETE)
    };
    z.comp.useexact = i32::from(opt("recexact"));
    z.comp.compexactstr = Some(String::new());
    z.comp.uselist = if z.comp.useline != 0 {
        if opt("autolist") && !opt("bashautolist") {
            if opt("listambiguous") {
                3
            } else {
                2
            }
        } else {
            0
        }
    } else {
        1
    };
    z.comp.comppatmatch = Some(if z.comp.useglob != 0 { "*" } else { "" }.into());
    PATMATCH_SET.with(|p| p.set(false));
    z.comp.comppatinsert = Some("menu".into());
    z.comp.forcelist = 0;
    z.comp.haspattern = 0;
    z.comp.complistmax = crate::shell::var_get("LISTMAX").trim().parse().unwrap_or(0);
    z.comp.complastprompt = Some(if opt("alwayslastprompt") { "yes" } else { "" }.into());
    z.comp.dolastprompt = 1;
    z.comp.complist = Some(
        match (opt("listrowsfirst"), opt("listpacked")) {
            (true, true) => "packed rows",
            (true, false) => "rows",
            (false, true) => "packed",
            (false, false) => "",
        }
        .into(),
    );
    z.comp.startauto = i32::from(opt("automenu"));
    z.comp.movetoend = if z.cs as i32 == we() || opt("alwaystoend") {
        2
    } else {
        1
    };
    z.showinglist = 0;
    z.comp.hasmatched = 0;
    z.comp.hasunmatched = 0;
    z.comp.minmlen = 1000000;
    z.comp.maxmlen = -1;
    z.comp.compignored = 0;
    z.comp.nmessages = 0;
    z.comp.hasallmatch = 0;

    if makecomplist(z, &s, incmd, lst) != 0 {
        // Error: feep.
        restore_origline(z);
        z.clearlist = true;
        ret = 1;
        z.comp.minfo.cur = None;
        if z.comp.useline < 0 {
            ret = crate::zle::widgets::misc::selfinsert(z, &[]);
        }
    } else {
        z.comp.lastprebr = None;
        z.comp.lastpostbr = None;
        if !st(&z.comp.comppatmatch).is_empty() && PATMATCH_SET.with(|p| p.get()) {
            z.comp.haspattern = 1;
        }
        let c = &z.comp;
        if c.iforcemenu != 0 {
            if c.nmatches != 0 {
                do_ambig_menu(z);
            }
            ret = i32::from(z.comp.nmatches == 0);
        } else if c.useline < 0 {
            ret = crate::zle::widgets::misc::selfinsert(z, &[]);
        } else if c.useline == 0 && c.uselist != 0 {
            // Only the list is wanted.
            restore_origline(z);
            z.showinglist = -2;
        } else if c.useline == 2 && c.nmatches > 1 {
            do_allmatches(z, 1);
            z.comp.minfo.cur = None;
            if z.comp.forcelist != 0 {
                z.showinglist = -2;
            } else {
                z.invalidatelist();
            }
        } else if c.useline != 0 {
            if c.nmatches > 1 && c.diffmatches != 0 {
                ret = do_ambiguous(z);
                if z.showinglist == 0
                    && z.comp.uselist != 0
                    && z.refresh.listshown
                    && (z.comp.usemenu == 2 || z.comp.oldlist != 0)
                {
                    z.showinglist = osl;
                }
            } else if c.nmatches == 1 || (c.nmatches > 1 && c.diffmatches == 0) {
                let m = c
                    .amatches
                    .iter()
                    .find(|g| g.mcount != 0)
                    .and_then(|g| g.matches.first().cloned());
                z.comp.minfo.cur = None;
                z.comp.minfo.asked = 0;
                if let Some(m) = m {
                    do_single(z, &m);
                }
                if z.comp.forcelist != 0 {
                    if z.comp.uselist != 0 {
                        z.showinglist = -2;
                    } else {
                        z.clearlist = true;
                    }
                } else {
                    z.invalidatelist();
                }
            } else if c.nmessages != 0 && c.forcelist != 0 {
                if c.uselist != 0 {
                    z.showinglist = -2;
                } else {
                    z.clearlist = true;
                }
            }
        } else {
            z.invalidatelist();
            z.comp.lastambig = i32::from(opt("bashautolist"));
            if z.comp.forcelist != 0 {
                z.clearlist = true;
            }
            restore_origline(z);
        }
        // The explanation strings, if needed.
        let c = &z.comp;
        if z.showinglist == 0
            && c.validlist != 0
            && c.usemenu != 2
            && c.uselist != 0
            && (c.nmatches != 1 || c.diffmatches != 0)
            && c.useline >= 0
            && c.useline != 2
            && (c.oldlist == 0 || !z.refresh.listshown)
        {
            z.comp.onlyexpl = 3;
            z.showinglist = -2;
        }
    }
    z.comp.matchers.clear();
    if z.cs > z.ll() {
        z.cs = z.ll();
    }
    ret
}

/// Run before completion: continues menu completion, or sets up the
/// cursor and automenu. Nonzero when completion is done with.
pub fn before_complete(z: &mut Zle, lst: &mut i32) -> i32 {
    OLDMENUCMP.with(|o| o.set(z.comp.menucmp));
    if z.comp.showagain != 0 && z.comp.validlist != 0 {
        z.showinglist = -2;
    }
    z.comp.showagain = 0;
    if z.comp.minfo.cur.is_some() && z.comp.menucmp != 0 && *lst != COMP_LIST_EXPAND {
        super::result::do_menucmp(z, *lst);
        return 1;
    }
    if z.comp.minfo.cur.is_some()
        && z.comp.menucmp != 0
        && z.comp.validlist != 0
        && *lst == COMP_LIST_COMPLETE
    {
        z.showinglist = -2;
        z.comp.onlyexpl = 0;
        z.comp.listdat.valid = 0;
        return 1;
    }
    // Back to after the string the last completion inserted.
    if z.comp.fromcomp & FC_INWORD != 0 {
        z.cs = (z.comp.lastend.max(0) as usize).min(z.ll());
    }
    if z.comp.startauto != 0
        && z.comp.lastambig != 0
        && (!opt("bashautolist") || z.comp.lastambig == 2)
    {
        z.comp.usemenu = 2;
    }
    0
}

/// Run after completion: starts menu selection when menu completion
/// began (the menu_start hook).
pub fn after_complete(z: &mut Zle, dat: &mut [i32; 2]) -> i32 {
    if z.comp.menucmp != 0 && OLDMENUCMP.with(|o| o.get()) == 0 {
        let ret = match MENU_START.with(|h| h.get()) {
            Some(f) => {
                let (n, m) = (z.comp.nmatches, z.comp.nmessages);
                f(z, n, m)
            }
            None => 0,
        };
        if ret != 0 {
            dat[1] = 0;
            z.comp.menucmp = 0;
            z.comp.menuacc = 0;
            z.comp.minfo.cur = None;
            if ret >= 2 {
                z.fixsuffix();
                restore_origline(z);
                if ret == 2 {
                    z.clearlist = true;
                    z.invalidatelist();
                }
            }
        }
    }
    0
}

// The match the menu is on, in the list of the last completion.
fn minfo_cur_match(z: &Zle) -> Option<CmatchRef> {
    let g = z.comp.minfo.group?;
    let c = z.comp.minfo.cur?;
    z.comp.lastmatches.get(g)?.matches.get(c).cloned()
}

/// Calls completion widget function `fname` for tokenized word `s`, with
/// the special completion parameters set up.
fn callcompfunc(z: &mut Zle, s: &[char], fname: &str) {
    let lv = crate::exec::last_status();
    let func =
        crate::shell::with_shell(|sh| sh.functions.get(fname).map(|f| (f.body.clone(), f.line)));
    let Some((body, line)) = func else {
        return;
    };
    let icf = z.comp.incompfunc;
    let osets = (z.comp.comp_set_reals, z.comp.comp_set_keys);
    let mut aadd = 0usize;
    let mut usea = true;
    let rset = CP_ALLREALS;
    let mut kset = CP_ALLKEYS
        & !(CP_PARAMETER
            | CP_REDIRECT
            | CP_QUOTE
            | CP_QUOTING
            | CP_EXACTSTR
            | CP_OLDLIST
            | CP_OLDINS
            | (if z.comp.useglob != 0 { 0 } else { CP_PATMATCH }));
    match z.pending_vared.clone() {
        Some(v) => {
            z.comp.compvared = Some(v);
            kset |= CP_VARED;
        }
        None => z.comp.compvared = Some(String::new()),
    }
    if st(&z.comp.complastprompt).is_empty() {
        kset &= !CP_LASTPROMPT;
    }
    let mut parameter = String::new();
    let mut redirect = String::new();
    let context;
    let c = &mut z.comp;
    if c.ispar != 0 {
        context = if c.ispar == 2 {
            "brace_parameter"
        } else {
            "parameter"
        };
    } else if c.linwhat == IN_PAR {
        context = "assign_parameter";
    } else if c.linwhat == IN_MATH {
        if c.insubscr != 0 {
            context = "subscript";
            if let Some(v) = &c.varname {
                parameter = v.clone();
                kset |= CP_PARAMETER;
            }
        } else {
            context = "math";
        }
        usea = false;
    } else if c.lincmd != 0 {
        if c.insubscr != 0 {
            context = "subscript";
            kset |= CP_PARAMETER;
        } else {
            context = "command";
        }
    } else if c.linredir != 0 {
        context = "redirect";
        if let Some(r) = &c.rdstr {
            redirect = r.clone();
        }
        kset |= CP_REDIRECT;
    } else {
        match c.linwhat {
            IN_ENV => {
                context = if c.linarr != 0 {
                    "array_value"
                } else {
                    "value"
                };
                parameter = c.varname.clone().unwrap_or_default();
                kset |= CP_PARAMETER;
                if c.clwpos == 0 {
                    c.clwpos = 1;
                    c.clwnum = 2;
                    c.clwords.resize(2, String::new());
                    c.clwords[1] = s.iter().collect();
                }
                aadd = 1;
            }
            IN_COND => context = "condition",
            _ => {
                if c.cmdstr.is_some() {
                    context = "command";
                } else {
                    context = "value";
                    kset |= CP_PARAMETER;
                    if let Some(w) = c.clwords.first() {
                        parameter = w.clone();
                    }
                    aadd = 1;
                }
            }
        }
    }
    c.compcontext = Some(context.into());
    let nw = (c.clwnum.max(0) as usize).min(c.clwords.len());
    c.compwords = if usea && (aadd == 0 || nw > 0) {
        c.clwords[..nw]
            .iter()
            .skip(aadd)
            .map(|w| untok(&chars(w)))
            .collect()
    } else {
        Vec::new()
    };
    c.compredirs = c.rdstrs.clone();
    c.compparameter = Some(untok(&chars(&parameter)));
    c.compredirect = Some(redirect);
    if c.instring > QT_BACKSLASH {
        let (q, n) = match c.instring {
            QT_SINGLE => ("'", "single"),
            QT_DOUBLE => ("\"", "double"),
            _ => ("$'", "dollars"),
        };
        c.compquote = Some(q.into());
        c.compquoting = Some(n.into());
        kset |= CP_QUOTE | CP_QUOTING;
    } else if c.inbackt != 0 {
        c.compquote = Some("`".into());
        c.compquoting = Some("backtick".into());
        kset |= CP_QUOTE | CP_QUOTING;
    } else {
        c.compquote = Some(String::new());
        c.compquoting = Some(String::new());
    }
    let math = c.linwhat == IN_MATH;
    makebangspecial(false);
    let quote = |z: &Zle, part: &[char]| {
        let t: String = part.iter().collect();
        let t = if math { t } else { multiquote(z, &t, false) };
        untok(&chars(&t))
    };
    if !opt("completeinword") {
        z.comp.compprefix = Some(quote(&*z, s));
        z.comp.compsuffix = Some(String::new());
    } else {
        let offs = (z.comp.offs.max(0) as usize).min(s.len());
        z.comp.compprefix = Some(quote(&*z, &s[..offs]));
        z.comp.compsuffix = Some(quote(&*z, &s[offs..]));
    }
    makebangspecial(true);
    let c = &mut z.comp;
    c.complastprefix = c.compprefix.clone();
    c.complastsuffix = c.compsuffix.clone();
    let (parwb, parwe, paroffs) = PARPOS.with(|p| p.get());
    if parwb < 0 {
        c.compiprefix = Some(String::new());
        c.compisuffix = Some(String::new());
    } else {
        let ll = z.line.len();
        let cl = |v: i32| (v.max(0) as usize).min(ll);
        let (pb, b, e, pe) = (cl(parwb), cl(wb()), cl(we()), cl(parwe));
        z.comp.compiprefix = Some(z.line[pb..b.max(pb)].iter().collect());
        z.comp.compisuffix = Some(z.line[e..pe.max(e)].iter().collect());
        set_wb(parwb);
        set_we(parwe);
        z.comp.offs = paroffs;
    }
    let c = &mut z.comp;
    c.compqiprefix = Some(c.qipre.clone().unwrap_or_default());
    c.compqisuffix = Some(c.qisuf.clone().unwrap_or_default());
    c.origlpre =
        (clen(st(&c.compqiprefix)) + clen(st(&c.compiprefix)) + clen(st(&c.compprefix))) as i32;
    c.origlsuf =
        (clen(st(&c.compqisuffix)) + clen(st(&c.compisuffix)) + clen(st(&c.compsuffix))) as i32;
    c.lenchanged = 0;
    c.compcurrent = if usea {
        (c.clwpos + 1 - aadd as i32) as i64
    } else {
        0
    };
    let mut list = match c.uselist {
        1 => "list".to_string(),
        2 => "autolist".to_string(),
        3 => "ambiguous".to_string(),
        _ => {
            kset &= !CP_LIST;
            String::new()
        }
    };
    if opt("listpacked") {
        list.push_str(" packed");
    }
    if opt("listrowsfirst") {
        list.push_str(" rows");
    }
    let c = &mut z.comp;
    c.complist = Some(list);
    let mut insert = if c.useline != 0 {
        match c.usemenu {
            0 => {
                if opt("automenu") {
                    "automenu-unambiguous"
                } else {
                    "unambiguous"
                }
            }
            1 => "menu",
            2 => "automenu",
            _ => "",
        }
        .to_string()
    } else {
        kset &= !CP_INSERT;
        String::new()
    };
    if c.useline < 0 {
        insert = format!("tab {}", insert);
    }
    c.compinsert = Some(insert);
    if c.useexact != 0 {
        c.compexact = Some("accept".into());
    } else {
        c.compexact = Some(String::new());
        kset &= !CP_EXACT;
    }
    c.comptoend = Some(if c.movetoend == 1 { "single" } else { "match" }.into());
    if c.hasoldlist != 0 && c.lastpermmnum != 0 {
        c.compoldlist = Some(if z.refresh.listshown { "shown" } else { "yes" }.into());
        kset |= CP_OLDLIST;
        match minfo_cur_match(z) {
            Some(m) => {
                z.comp.compoldins = Some(m.borrow().gnum.to_string());
                kset |= CP_OLDINS;
            }
            None => z.comp.compoldins = Some(String::new()),
        }
    } else {
        c.compoldlist = Some(String::new());
        c.compoldins = Some(String::new());
    }

    z.comp.incompfunc = 1;
    crate::shell::push_scope();
    z.comp.comp_set_reals = 0;
    z.comp.comp_set_keys = 0;
    comp_setunset(z, rset, !rset & CP_ALLREALS, kset, !kset & CP_ALLKEYS);
    make_comp_params(z);
    crate::zle::params::make_params(z);
    let mut words = vec![fname.to_string()];
    if z.comp.cfargs.is_empty() {
        words.extend(crate::shell::with_shell(|sh| sh.pos_params.clone()));
    } else {
        words.extend(z.comp.cfargs.iter().cloned());
    }
    let oxt = opt("xtrace");
    crate::options::set("xtrace", false);
    z.widget_depth += 1;
    let ret = z.shell(|| {
        crate::shell::in_eval_context("shfunc", || crate::exec::call_function(&body, &words, line))
    });
    z.widget_depth -= 1;
    crate::options::set("xtrace", oxt);
    z.comp.cfret = ret;
    pull_comp_params(z);
    crate::shell::pop_scope();
    crate::zle::params::end_scope();
    if z.widget_depth > 0 {
        crate::zle::params::push(z);
    }
    if matches!(crate::exec::flow(), crate::types::Flow::Return(_)) {
        crate::exec::set_flow(crate::types::Flow::None);
    }
    z.lastcmd = 0;
    z.comp.incompfunc = icf;
    z.comp.startauto = 0;
    z.comp.comp_set_reals = osets.0;
    z.comp.comp_set_keys = osets.1;
    if icf == 1 {
        push_comp_params(z);
    }

    let c = &mut z.comp;
    let list = c.complist.clone();
    c.uselist = match list.as_deref() {
        None => 0,
        Some(l) if l.starts_with("list") => 1,
        Some(l) if l.starts_with("auto") => 2,
        Some(l) if l.starts_with("ambig") => 3,
        _ => 0,
    };
    c.forcelist = i32::from(list.as_deref().is_some_and(|l| l.contains("force")));
    c.onlyexpl = list
        .as_deref()
        .map(|l| i32::from(l.contains("expl")) | if l.contains("messages") { 2 } else { 0 })
        .unwrap_or(0);
    match c.compinsert.clone() {
        None => c.useline = 0,
        Some(ins) => {
            if ins.contains("tab") {
                c.useline = -1;
            } else if ins == "unambig" || ins == "unambiguous" || ins == "automenu-unambiguous" {
                c.useline = 1;
                c.usemenu = 0;
            } else if ins == "all" {
                c.useline = 2;
                c.usemenu = 0;
            } else if ins.starts_with(|ch: char| ch.is_ascii_digit()) {
                c.useline = 1;
                c.usemenu = 3;
                c.insmnum = atoi(&ins);
                c.insspace = i32::from(ins.ends_with(' '));
            } else {
                if ins.starts_with("menu") {
                    c.useline = 1;
                    c.usemenu = 1;
                } else if ins.starts_with("auto") {
                    c.useline = 1;
                    c.usemenu = 2;
                } else {
                    c.useline = 0;
                    c.usemenu = 0;
                    // With compstate[insert] emptied no unambiguous prefix
                    // is inserted, so the next tab may start the menu.
                    c.lastambig = i32::from(opt("automenu"));
                    c.startauto = c.lastambig;
                }
                if c.useline != 0 {
                    if let Some(k) = ins.find(':') {
                        c.insmnum = atoi(&ins[k + 1..]);
                    }
                }
            }
        }
    }
    let ins = st(&c.compinsert).to_string();
    c.startauto = i32::from(
        c.startauto != 0
            || ins == "automenu-unambiguous"
            || (c.bashlistfirst != 0 && opt("automenu") && ins.is_empty()),
    );
    c.useexact = i32::from(st(&c.compexact) == "accept");
    c.movetoend = match st(&c.comptoend) {
        "" => 0,
        "single" => 1,
        "always" => 3,
        _ => 2,
    };
    c.oldlist = i32::from(c.hasoldlist != 0 && st(&c.compoldlist) == "keep");
    c.oldins = i32::from(c.hasoldlist != 0 && c.minfo.cur.is_some() && st(&c.compoldins) == "keep");
    crate::shell::with_shell(|sh| sh.last_status = lv);
}

/// C's atoi: the leading (signed) number, 0 if none.
pub(super) fn atoi(s: &str) -> i32 {
    let t = s.trim_start();
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: i64 = digits.parse().unwrap_or(0);
    let n = if neg { -n } else { n };
    n.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// Makes the list of matches by calling the completion function.
/// Nonzero when there are none.
fn makecomplist(z: &mut Zle, s: &[char], _incmd: i32, lst: i32) -> i32 {
    let (owb, owe, ooffs) = (wb(), we(), z.comp.offs);
    let mut s: Vec<char> = s.to_vec();
    match z.comp.compfunc.clone() {
        Some(_) => {
            if let Some(p) = check_param(z, &s, false, false) {
                s = chars(&p);
                PARPOS.with(|pp| pp.set((owb, owe, ooffs)));
            } else {
                PARPOS.with(|pp| pp.set((-1, 0, 0)));
            }
        }
        None => PARPOS.with(|pp| pp.set((-1, 0, 0))),
    }
    z.comp.linwhat = inwhat();
    let Some(cf) = z.comp.compfunc.clone() else {
        // Without a completion function zsh asks compctl, which is not
        // here: nothing is added.
        return lst;
    };
    let onm = z.comp.nmatches;
    let odm = z.comp.diffmatches;
    // The function runs with standard input closed, as in zsh.
    let osi = if z.fd != 0 {
        // SAFETY: plain descriptor calls; 0 is restored below.
        let fd = unsafe { libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 10) };
        if fd >= 0 {
            unsafe { libc::close(0) };
        }
        fd
    } else {
        -1
    };
    let redup = |fd: i32| {
        if fd >= 0 {
            // SAFETY: fd is the copy of 0 made above.
            unsafe {
                libc::dup2(fd, 0);
                libc::close(fd);
            }
        }
    };
    let c = &mut z.comp;
    c.bmatchers.clear();
    c.mstack.clear();
    c.ainfo = Some(Aminfo::default());
    c.fainfo = Some(Aminfo::default());
    if c.validlist == 0 {
        c.lastambig = 0;
    }
    c.amatches.clear();
    c.mgroup = None;
    c.mnum = 0;
    c.unambig_mnum = -1;
    c.isuf = None;
    c.insmnum = z.zmod.mult as i32;
    c.oldlist = 0;
    c.oldins = 0;
    begcmgroup(z, Some("default"), 0);
    let c = &mut z.comp;
    c.menucmp = 0;
    c.menuacc = 0;
    c.newmatches = 0;
    c.onlyexpl = 0;

    callcompfunc(z, &s, &cf);
    endcmgroup(z, None);

    if z.comp.oldlist != 0 {
        let c = &mut z.comp;
        c.nmatches = onm;
        c.diffmatches = odm;
        c.validlist = 1;
        c.amatches = c.lastmatches.clone();
        c.lmatches = c.lastlmatches.clone();
        if !c.pmatches.is_empty() {
            c.pmatches.clear();
            c.hasperm = 0;
        }
        redup(osi);
        return 0;
    }
    if !z.comp.lastmatches.is_empty() {
        let lm = std::mem::take(&mut z.comp.lastmatches);
        freematches(z, lm, true);
    }
    permmatches(z, true);
    let c = &mut z.comp;
    c.amatches = c.pmatches.clone();
    c.lastpermmnum = c.permmnum;
    c.lastpermgnum = c.permgnum;
    c.lastmatches = std::mem::take(&mut c.pmatches);
    c.lastlmatches = c.lmatches.clone();
    c.hasperm = 0;
    c.hasoldlist = 1;
    if (c.nmatches != 0 || c.nmessages != 0) && !z.errflag {
        z.comp.validlist = 1;
        redup(osi);
        return 0;
    }
    redup(osi);
    1
}
