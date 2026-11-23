//! Completion listing enhancements, after zsh's Src/Zle/complist.c
//! (the zsh/complist module): coloured lists from $ZLS_COLORS,
//! scrolling lists ($LISTPROMPT) and menu selection (the menu-select
//! widget and the menuselect keymap).
//!
//! [`setup`] does what loading the module does: it adds menu-select,
//! hooks the lister into `list_matches` and menu selection into the
//! start of menu completion, and makes the menuselect and listscroll
//! keymaps.
//!
//! zsh keeps the logical table of the shown matches (mtab and mgtab) as
//! arrays of pointers, marking explanation lines and dummy matches by
//! setting the low bit. Here one table holds both: each cell is an
//! [`Mte`], the group and match index plus that mark.

use super::result::{
    accept_last, asklist, bld_all_str, calclist, clearflag, do_menucmp, do_single, hasbrpsfx,
    invalidate_list, listshown, set_clearflag, set_complistmatches_hook, set_lastlistlen,
    set_listshown, skipnolist, zterm_columns, zterm_lines,
};
use super::*;
use crate::pattern::Pattern;
use crate::zle::keymap::{open_keymap, with_keymaps, KeymapRef, Keymaps};
use crate::zle::refresh::{attr_to_sgr, flush_tty, write_tty};
use crate::zle::{Zle, CUT_RAW, WIDGET_NCOMP, ZLE_ISCOMP, ZLE_KEEPSUFFIX, ZLE_MENUCMP};
use std::cell::{Cell, RefCell};
use std::thread::LocalKey;

// ---- private state ------------------------------------------------------

thread_local! {
    // noselect: 1 if the lister said not to select, -1 if interactive
    // mode must reset the selection list. mselect: gnum of the selected
    // match. inselect: selection running. mcol, mline: the selected
    // cell. mcols, mlines: the table's size.
    static NOSELECT: Cell<i32> = const { Cell::new(0) };
    static MSELECT: Cell<i32> = const { Cell::new(-1) };
    static INSELECT: Cell<i32> = const { Cell::new(0) };
    static MCOL: Cell<i32> = const { Cell::new(0) };
    static MLINE: Cell<i32> = const { Cell::new(0) };
    static MCOLS: Cell<i32> = const { Cell::new(0) };
    static MLINES: Cell<i32> = const { Cell::new(0) };
    // selected: set once menu selection has shown its list. mlbeg, mlend:
    // the table lines on the screen (mlbeg -1 forces a redraw). mscroll:
    // the list scrolls ($LISTPROMPT). mrestlines: screen lines left.
    static SELECTED: Cell<i32> = const { Cell::new(0) };
    static MLBEG: Cell<i32> = const { Cell::new(-1) };
    static MLEND: Cell<i32> = const { Cell::new(9999999) };
    static MSCROLL: Cell<i32> = const { Cell::new(0) };
    static MRESTLINES: Cell<i32> = const { Cell::new(0) };
    static MNEW: Cell<i32> = const { Cell::new(0) };
    static MLASTCOLS: Cell<i32> = const { Cell::new(0) };
    static MLASTLINES: Cell<i32> = const { Cell::new(0) };
    static MHASSTAT: Cell<i32> = const { Cell::new(0) };
    static MFIRSTL: Cell<i32> = const { Cell::new(0) };
    static MLASTM: Cell<i32> = const { Cell::new(0) };
    // mlprinted: extra lines the last print took. molbeg, mocol, moline:
    // the last mlbeg, mcol and mline (molbeg -42: very invalid).
    static MLPRINTED: Cell<i32> = const { Cell::new(0) };
    static MOLBEG: Cell<i32> = const { Cell::new(-2) };
    static MOCOL: Cell<i32> = const { Cell::new(0) };
    static MOLINE: Cell<i32> = const { Cell::new(0) };
    static MSTATPRINTED: Cell<i32> = const { Cell::new(0) };
    static MTAB_BEEN_REALLOCATED: Cell<i32> = const { Cell::new(0) };
    // complistmatches' static onlnct.
    static ONLNCT: Cell<i32> = const { Cell::new(-1) };

    // $MENUPROMPT and $LISTPROMPT as in effect.
    static MSTATUS: RefCell<Option<String>> = const { RefCell::new(None) };
    static MLISTP: RefCell<Option<String>> = const { RefCell::new(None) };
    // The table of shown matches (mcols * mlines cells) and the selected
    // cell in it (zsh's mmtabp and mgtabp).
    static MTAB: RefCell<Vec<Mte>> = const { RefCell::new(Vec::new()) };
    static MMTABP: Cell<Option<usize>> = const { Cell::new(None) };

    static MCOLORS: RefCell<Option<std::rc::Rc<Listcols>>> = const { RefCell::new(None) };
    static LAST_CAP: RefCell<String> = const { RefCell::new(String::new()) };
    static LAST_GROUP: Cell<Option<usize>> = const { Cell::new(None) };
    static ISCOL: RefCell<IsCol> = const { RefCell::new(IsCol::new()) };
    static PRINTLIST: RefCell<PrintListState> = const { RefCell::new(PrintListState::new()) };
    // Text attributes asked for and those on the terminal.
    static TXTPEND: RefCell<Txt> = const { RefCell::new(Txt::new()) };
    static TXTCUR: RefCell<Txt> = const { RefCell::new(Txt::new()) };

    // domenuselect's statics: running (fdat) and the last search string.
    static FDAT: Cell<bool> = const { Cell::new(false) };
    static LASTSEARCH: RefCell<Option<String>> = const { RefCell::new(None) };

    static MSKEYMAP: RefCell<Option<KeymapRef>> = const { RefCell::new(None) };
    static LSKEYMAP: RefCell<Option<KeymapRef>> = const { RefCell::new(None) };
    static LOADED: Cell<bool> = const { Cell::new(false) };
}

type IntKey = LocalKey<Cell<i32>>;

fn get(k: &'static IntKey) -> i32 {
    k.with(Cell::get)
}

fn set(k: &'static IntKey, v: i32) {
    k.with(|c| c.set(v));
}

/// `--k`, returning the new value.
fn predec(k: &'static IntKey) -> i32 {
    let v = get(k) - 1;
    set(k, v);
    v
}

/// `x--`, returning the old value.
fn postdec(x: &mut i32) -> i32 {
    let t = *x;
    *x -= 1;
    t
}

/// A cell of the match table.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Mte {
    /// Group and match index; None is zsh's NULL.
    gm: Option<(usize, usize)>,
    /// zsh's MMARK bit: explanation lines and dummy matches.
    marked: bool,
}

impl Mte {
    /// zsh's `!*p`: the raw pointer is NULL.
    fn null(self) -> bool {
        self.gm.is_none() && !self.marked
    }

    /// zsh's `*p && !mmarked(*p)`.
    fn live(self) -> bool {
        self.gm.is_some() && !self.marked
    }
}

fn tab(i: isize) -> Mte {
    if i < 0 {
        return Mte::default();
    }
    MTAB.with(|t| t.borrow().get(i as usize).copied().unwrap_or_default())
}

fn tab_set(i: i32, v: Mte) {
    if i < 0 {
        return;
    }
    MTAB.with(|t| {
        if let Some(c) = t.borrow_mut().get_mut(i as usize) {
            *c = v;
        }
    });
}

fn tab_len() -> usize {
    MTAB.with(|t| t.borrow().len())
}

fn tab_match(z: &Zle, e: Mte) -> Option<CmatchRef> {
    let (g, m) = e.gm?;
    z.comp.amatches.get(g)?.matches.get(m).cloned()
}

fn gnum_of(m: &CmatchRef) -> i32 {
    m.borrow().gnum
}

fn cur_match(z: &Zle) -> Option<CmatchRef> {
    let g = z.comp.minfo.group?;
    let c = z.comp.minfo.cur?;
    z.comp.amatches.get(g)?.matches.get(c).cloned()
}

/// minfo.cur as a table cell, for comparing with one.
fn cur_entry(z: &Zle) -> Mte {
    Mte {
        gm: z.comp.minfo.group.zip(z.comp.minfo.cur),
        marked: false,
    }
}

fn gwidth(z: &Zle, g: usize, mc: i32) -> i32 {
    match z.comp.amatches.get(g) {
        Some(grp) if grp.widths.is_empty() => grp.width,
        Some(grp) => grp.widths.get(mc.max(0) as usize).copied().unwrap_or(0),
        None => 0,
    }
}

fn isset(name: &str) -> bool {
    crate::options::is_set(name)
}

fn getsparam(name: &str) -> Option<String> {
    crate::shell::var_get_opt(name)
}

/// zsh's zwcwidth: unprintable characters count one column.
fn zwcwidth(c: char) -> i32 {
    let u = c as u32;
    if u < 0x20 || (0x7f..0xa0).contains(&u) {
        1
    } else {
        crate::display_width(c.encode_utf8(&mut [0; 4])) as i32
    }
}

/// C's atoi: the leading decimal number, 0 if none.
fn atoi(s: &str) -> i32 {
    let t = s.trim_start();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let n: i64 = digits
        .chars()
        .take_while(char::is_ascii_digit)
        .fold(0i64, |a, d| {
            a.saturating_mul(10)
                .saturating_add(d.to_digit(10).unwrap_or(0) as i64)
        });
    (if neg { -n } else { n }) as i32
}

fn zbeep(z: &mut Zle) {
    crate::zle::utils::handlefeep(z, &[]);
}

// ---- terminal output ----------------------------------------------------

fn tcup(n: i32) {
    if n > 0 {
        write_tty(&format!("\x1b[{}A", n));
    }
}

fn tcright(n: i32) {
    if n > 0 {
        write_tty(&format!("\x1b[{}C", n));
    }
}

fn tc_downcurs(n: i32) {
    if n > 0 {
        write_tty(&format!("\x1b[{}B", n));
    }
}

fn cleareod() {
    write_tty("\x1b[J");
}

/// Clears to the end of the line while selecting.
fn cleareol() {
    if get(&MLBEG) >= 0 {
        if !LAST_CAP.with(|l| l.borrow().is_empty()) {
            // Keep the colour from bleeding into the cleared part.
            zcoff();
        }
        write_tty("\x1b[K");
    }
}

// ---- text attributes for explanation strings ----------------------------

#[derive(Clone, Debug, PartialEq)]
struct Txt {
    bold: bool,
    standout: bool,
    underline: bool,
    fg: Option<String>,
    bg: Option<String>,
}

impl Txt {
    const fn new() -> Self {
        Txt {
            bold: false,
            standout: false,
            underline: false,
            fg: None,
            bg: None,
        }
    }
}

fn tsetattrs(f: impl FnOnce(&mut Txt)) {
    TXTPEND.with(|t| f(&mut t.borrow_mut()));
}

fn treplaceattrs(t: Txt) {
    TXTPEND.with(|p| *p.borrow_mut() = t);
}

/// Puts the attributes asked for on the terminal.
fn applytextattributes() {
    let pend = TXTPEND.with(|t| t.borrow().clone());
    let cur = TXTCUR.with(|t| t.borrow().clone());
    if pend == cur {
        return;
    }
    let mut out = String::new();
    let off = (cur.bold && !pend.bold)
        || (cur.standout && !pend.standout)
        || (cur.underline && !pend.underline)
        || (cur.fg.is_some() && pend.fg.is_none())
        || (cur.bg.is_some() && pend.bg.is_none());
    let base = if off {
        out.push_str("\x1b[0m");
        Txt::new()
    } else {
        cur
    };
    if pend.bold && !base.bold {
        out.push_str("\x1b[1m");
    }
    if pend.standout && !base.standout {
        out.push_str("\x1b[7m");
    }
    if pend.underline && !base.underline {
        out.push_str("\x1b[4m");
    }
    if pend.fg != base.fg {
        if let Some(c) = &pend.fg {
            out.push_str(&attr_to_sgr(&format!("fg={}", c)));
        }
    }
    if pend.bg != base.bg {
        if let Some(c) = &pend.bg {
            out.push_str(&attr_to_sgr(&format!("bg={}", c)));
        }
    }
    write_tty(&out);
    TXTCUR.with(|t| *t.borrow_mut() = pend);
}

/// A highlight specification (`bold,fg=red`) as attributes.
fn parsehighlight(spec: &str) -> Txt {
    let mut t = Txt::new();
    for part in spec.split(',') {
        match part.trim() {
            "bold" => t.bold = true,
            "standout" => t.standout = true,
            "underline" => t.underline = true,
            p => {
                if let Some(c) = p.strip_prefix("fg=") {
                    t.fg = Some(c.to_string());
                } else if let Some(c) = p.strip_prefix("bg=") {
                    t.bg = Some(c.to_string());
                }
            }
        }
    }
    t
}

// ---- $ZLS_COLORS --------------------------------------------------------

// Indices of the terminal strings.
const COL_NO: usize = 0;
const COL_FI: usize = 1;
const COL_DI: usize = 2;
const COL_LN: usize = 3;
const COL_PI: usize = 4;
const COL_SO: usize = 5;
const COL_BD: usize = 6;
const COL_CD: usize = 7;
const COL_OR: usize = 8;
const COL_MI: usize = 9;
const COL_SU: usize = 10;
const COL_SG: usize = 11;
const COL_TW: usize = 12;
const COL_OW: usize = 13;
const COL_ST: usize = 14;
const COL_EX: usize = 15;
const COL_LC: usize = 16;
const COL_RC: usize = 17;
const COL_EC: usize = 18;
const COL_TC: usize = 19;
const COL_SP: usize = 20;
const COL_MA: usize = 21;
const COL_HI: usize = 22;
const COL_DU: usize = 23;
const COL_SA: usize = 24;
const NUM_COLS: usize = 25;

const COLNAMES: [&str; NUM_COLS] = [
    "no", "fi", "di", "ln", "pi", "so", "bd", "cd", "or", "mi", "su", "sg", "tw", "ow", "st", "ex",
    "lc", "rc", "ec", "tc", "sp", "ma", "hi", "du", "sa",
];

const DEFCOLS: [Option<&str>; NUM_COLS] = [
    Some("0"),
    Some("0"),
    Some("1;31"),
    Some("1;36"),
    Some("33"),
    Some("1;35"),
    Some("1;33"),
    Some("1;33"),
    None,
    None,
    Some("37;41"),
    Some("30;43"),
    Some("30;42"),
    Some("34;42"),
    Some("37;44"),
    Some("1;32"),
    Some("\x1b["),
    Some("m"),
    None,
    Some("0"),
    Some("0"),
    Some("7"),
    None,
    None,
    Some("0"),
];

/// Maximum number of in-string colours.
const MAX_POS: usize = 11;
const NOPOS: i32 = 0xfffffff;

/// ln=target: colour symlinks by what they point to.
const LC_FOLLOW_SYMLINKS: i32 = 1;

/// A terminal string for a file type, maybe for some groups only.
#[derive(Clone)]
struct Filecol {
    prog: Option<std::rc::Rc<Pattern>>,
    col: Option<String>,
}

/// Terminal strings for matches a pattern matches.
struct Patcol {
    prog: Option<std::rc::Rc<Pattern>>,
    pat: Pattern,
    cols: Vec<String>,
}

/// A terminal string for a filename extension.
struct Extcol {
    prog: Option<std::rc::Rc<Pattern>>,
    ext: String,
    col: String,
}

struct Listcols {
    files: Vec<Vec<Filecol>>,
    pats: Vec<Patcol>,
    exts: Vec<Extcol>,
    flags: i32,
}

fn mcolors() -> std::rc::Rc<Listcols> {
    MCOLORS.with(|m| {
        m.borrow().clone().unwrap_or_else(|| {
            std::rc::Rc::new(Listcols {
                files: vec![Vec::new(); NUM_COLS],
                pats: Vec::new(),
                exts: Vec::new(),
                flags: 0,
            })
        })
    })
}

fn filecol(col: Option<&str>) -> Vec<Filecol> {
    vec![Filecol {
        prog: None,
        col: col.map(str::to_string),
    }]
}

fn is_oct(c: Option<&char>) -> bool {
    c.is_some_and(|c| ('0'..='7').contains(c))
}

/// Parses the value of a definition (after the `=`), from `i`. Returns
/// the value and the index of the character ending it.
fn getcolval(s: &[char], start: usize, multi: bool) -> (String, usize) {
    let mut out = String::new();
    let mut i = start;
    while i < s.len() && s[i] != ':' && (!multi || s[i] != '=') {
        if s[i] == '\\' && i + 1 < s.len() {
            i += 1;
            match s[i] {
                'a' => out.push('\x07'),
                'n' => out.push('\n'),
                'b' => out.push('\x08'),
                't' => out.push('\t'),
                'v' => out.push('\x0b'),
                'f' => out.push('\x0c'),
                'r' => out.push('\r'),
                'e' => out.push('\x1b'),
                '_' => out.push(' '),
                '?' => out.push('\x7f'),
                c if ('0'..='7').contains(&c) => {
                    // zsh starts from the digit's character code and
                    // steps past the character after a short escape;
                    // kept so the same strings come out.
                    let mut v = c as u32;
                    i += 1;
                    if is_oct(s.get(i)) {
                        v = v * 8 + s[i] as u32;
                        i += 1;
                        if is_oct(s.get(i)) {
                            v = v * 8 + s[i] as u32;
                        }
                    }
                    out.push(char::from((v & 0xff) as u8));
                }
                c => out.push(c),
            }
        } else if s[i] == '^' {
            match s.get(i + 1) {
                // zsh masks the `^` itself here, not the next character.
                Some(&n) if ('@'..='_').contains(&n) || n.is_ascii_lowercase() => {
                    out.push(char::from(b'^' & !0x60))
                }
                Some('?') => out.push('\x7f'),
                Some(&n) => {
                    out.push('^');
                    out.push(n);
                }
                None => out.push('^'),
            }
            i += 1;
        } else {
            out.push(s[i]);
        }
        i += 1;
    }
    (out, i.min(s.len()))
}

/// Parses one definition from `i`; returns the index after it.
fn getcoldef(lc: &mut Listcols, s: &[char], start: usize) -> usize {
    let mut i = start;
    let mut gprog: Option<std::rc::Rc<Pattern>> = None;

    if s.get(i) == Some(&'(') {
        let mut p = i + 1;
        let mut l = 0;
        while p < s.len() && (s[p] != ')' || l != 0) {
            if s[p] == '\\' && p + 1 < s.len() {
                p += 1;
            } else if s[p] == '(' {
                l += 1;
            } else if s[p] == ')' {
                l -= 1;
            }
            p += 1;
        }
        if p < s.len() && s[p] == ')' {
            let pat: String = s[i..=p].iter().collect();
            gprog = Some(std::rc::Rc::new(Pattern::new(&pat)));
            i = p + 1;
        }
    }
    match s.get(i) {
        Some('*') => {
            // An extension.
            i += 1;
            let n = i;
            while i < s.len() && s[i] != '=' {
                i += 1;
            }
            if i >= s.len() {
                return i;
            }
            let ext: String = s[n..i].iter().collect();
            i += 1;
            let (col, p) = getcolval(s, i, false);
            lc.exts.push(Extcol {
                prog: gprog,
                ext,
                col,
            });
            if p < s.len() {
                p + 1
            } else {
                p
            }
        }
        Some('=') => {
            // A pattern.
            i += 1;
            let ps = i;
            let mut nesting = 0;
            while i < s.len() && (nesting != 0 || s[i] != '=') {
                let c = s[i];
                i += 1;
                match c {
                    '\\' => {
                        if i < s.len() {
                            i += 1;
                        }
                    }
                    '(' => nesting += 1,
                    ')' => nesting -= 1,
                    _ => {}
                }
            }
            if i >= s.len() {
                return i;
            }
            let pat: String = s[ps..i].iter().collect();
            i += 1;
            let mut cols = Vec::new();
            loop {
                let (v, t) = getcolval(s, i, true);
                if cols.len() < MAX_POS {
                    cols.push(v);
                }
                i = t;
                if s.get(i) != Some(&'=') {
                    break;
                }
                i += 1;
            }
            lc.pats.push(Patcol {
                prog: gprog,
                pat: Pattern::new(&pat),
                cols,
            });
            if i < s.len() {
                i + 1
            } else {
                i
            }
        }
        _ => {
            // A file type.
            let n = i;
            while i < s.len() && s[i] != '=' {
                i += 1;
            }
            if i >= s.len() {
                return i;
            }
            let name: String = s[n..i].iter().collect();
            i += 1;
            let idx = COLNAMES.iter().position(|c| *c == name);
            let rest: String = s[i..].iter().take(7).collect();
            if idx == Some(COL_LN)
                && rest.starts_with("target")
                && matches!(rest.chars().nth(6), None | Some(':'))
            {
                lc.flags |= LC_FOLLOW_SYMLINKS;
                i + 6
            } else {
                let (col, p) = getcolval(s, i, false);
                if let Some(idx) = idx {
                    lc.files[idx].push(Filecol {
                        prog: if idx == COL_EC || idx == COL_LC || idx == COL_RC {
                            None
                        } else {
                            gprog
                        },
                        col: Some(col),
                    });
                }
                if p < s.len() {
                    p + 1
                } else {
                    p
                }
            }
        }
    }
}

/// Sets up the colours from $ZLS_COLORS (or $ZLS_COLOURS).
fn getcols() {
    let mut lc = Listcols {
        files: vec![Vec::new(); NUM_COLS],
        pats: Vec::new(),
        exts: Vec::new(),
        flags: 0,
    };
    let Some(s) = getsparam("ZLS_COLORS").or_else(|| getsparam("ZLS_COLOURS")) else {
        for f in lc.files.iter_mut() {
            *f = filecol(Some(""));
        }
        // The terminal's standout mode marks the selected match.
        lc.files[COL_MA] = filecol(Some("\x1b[7m"));
        lc.files[COL_EC] = filecol(Some("\x1b[27m"));
        MCOLORS.with(|m| *m.borrow_mut() = Some(std::rc::Rc::new(lc)));
        return;
    };
    let s: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < s.len() {
        if s[i] == ':' {
            i += 1;
        } else {
            i = getcoldef(&mut lc, &s, i);
        }
    }
    for (i, f) in lc.files.iter_mut().enumerate() {
        if f.first().is_none_or(|c| c.col.is_none()) {
            *f = filecol(DEFCOLS[i]);
        }
    }
    if lc.files[COL_OR].first().is_none_or(|c| c.col.is_none()) {
        lc.files[COL_OR] = lc.files[COL_LN].clone();
    }
    if lc.files[COL_MI].first().is_none_or(|c| c.col.is_none()) {
        lc.files[COL_MI] = lc.files[COL_FI].clone();
    }
    MCOLORS.with(|m| *m.borrow_mut() = Some(std::rc::Rc::new(lc)));
}

fn colstr(lc: &Listcols, i: usize) -> String {
    lc.files
        .get(i)
        .and_then(|f| f.first())
        .and_then(|f| f.col.clone())
        .unwrap_or_default()
}

fn hascol(lc: &Listcols, i: usize) -> bool {
    lc.files
        .get(i)
        .and_then(|f| f.first())
        .is_some_and(|f| f.col.is_some())
}

/// Switches to colour `cap`, between lc and rc, unless it is on.
fn zlrputs(cap: &str) {
    let need = LAST_CAP.with(|l| {
        let l = l.borrow();
        l.is_empty() || *l != cap
    });
    if need {
        let lc = mcolors();
        write_tty(&format!(
            "{}{}{}",
            colstr(&lc, COL_LC),
            cap,
            colstr(&lc, COL_RC)
        ));
        LAST_CAP.with(|l| *l.borrow_mut() = cap.to_string());
    }
}

/// Switches to colour `colour` for matches of `group`.
fn zcputs(group: Option<&str>, colour: usize) {
    let lc = mcolors();
    if let Some(fcs) = lc.files.get(colour) {
        for fc in fcs {
            if let Some(col) = &fc.col {
                let ok = match (&fc.prog, group) {
                    (Some(p), Some(g)) => p.matches(g),
                    _ => true,
                };
                if ok {
                    zlrputs(col);
                    return;
                }
            }
        }
    }
    zlrputs("0");
}

/// Turns off colouring.
fn zcoff() {
    let lc = mcolors();
    if hascol(&lc, COL_EC) {
        write_tty(&colstr(&lc, COL_EC));
        LAST_CAP.with(|l| l.borrow_mut().clear());
    } else {
        zcputs(None, COL_NO);
    }
}

// ---- in-string colours --------------------------------------------------

struct IsCol {
    nrefs: usize,
    begpos: [i32; MAX_POS],
    endpos: [i32; MAX_POS],
    // End positions, sorted.
    sendpos: [i32; MAX_POS],
    curisbeg: usize,
    curissend: usize,
    curiscols: [Option<String>; MAX_POS],
    curiscol: usize,
    patcols: Vec<String>,
    patidx: usize,
}

impl IsCol {
    const fn new() -> Self {
        IsCol {
            nrefs: 0,
            begpos: [0; MAX_POS],
            endpos: [0; MAX_POS],
            sendpos: [0; MAX_POS],
            curisbeg: 0,
            curissend: 0,
            curiscols: [
                None, None, None, None, None, None, None, None, None, None, None,
            ],
            curiscol: 0,
            patcols: Vec::new(),
            patidx: 0,
        }
    }
}

/// Matches `s` against `pat` as zsh's pattryrefs: on a match, the
/// number of (#b) groups and where each begins and ends (inclusive),
/// -1 for a group that did not match.
fn pattryrefs(pat: &Pattern, s: &str) -> Option<(usize, [i32; MAX_POS], [i32; MAX_POS])> {
    let chars: Vec<char> = s.chars().collect();
    if !pat.matches_chars(&chars) {
        return None;
    }
    // The pattern code reports groups through $match and friends; read
    // them from there and put the user's values back.
    const NAMES: [&str; 6] = ["match", "mbegin", "mend", "MATCH", "MBEGIN", "MEND"];
    let saved: Vec<Option<crate::params::Param>> =
        crate::shell::with_shell(|sh| NAMES.iter().map(|n| sh.params.remove(*n)).collect());
    crate::shell::without_assign_warnings(|| pat.record(&chars, 0, chars.len()));
    let mb = crate::shell::array_get("mbegin").unwrap_or_default();
    let me = crate::shell::array_get("mend").unwrap_or_default();
    crate::shell::with_shell(|sh| {
        for (n, p) in NAMES.iter().zip(saved) {
            sh.params.remove(*n);
            if let Some(p) = p {
                sh.params.insert(n.to_string(), p);
            }
        }
    });
    let mut beg = [NOPOS; MAX_POS];
    let mut end = [NOPOS; MAX_POS];
    for (i, (b, e)) in mb.iter().zip(me.iter()).take(MAX_POS - 1).enumerate() {
        let b: i32 = b.parse().unwrap_or(-1);
        let e: i32 = e.parse().unwrap_or(-1);
        if b < 0 {
            beg[i] = -1;
            end[i] = -1;
        } else {
            beg[i] = b - 1;
            end[i] = e - 1;
        }
    }
    Some((mb.len(), beg, end))
}

/// Starts in-string colouring with the first colour of the pattern.
fn initiscol() {
    ISCOL.with(|ic| {
        let mut guard = ic.borrow_mut();
        let ic = &mut *guard;
        let first = ic.patcols.first().cloned().unwrap_or_default();
        zlrputs(&first);
        ic.curiscol = 0;
        ic.curiscols[0] = Some(first);
        ic.patidx = 1;
        ic.curisbeg = 0;
        ic.curissend = 0;
        let n = ic.nrefs.min(MAX_POS);
        ic.sendpos.fill(NOPOS);
        ic.begpos[n..].fill(NOPOS);
        ic.endpos[n..].fill(NOPOS);
    });
}

/// Switches colours at character `pos` of an in-string coloured match.
fn doiscol(pos: i32) {
    ISCOL.with(|ic| {
        let mut guard = ic.borrow_mut();
        let ic = &mut *guard;
        while ic.curissend < MAX_POS && pos > ic.sendpos[ic.curissend] {
            ic.curissend += 1;
            if ic.curiscol > 0 {
                zcputs(None, COL_NO);
                ic.curiscol -= 1;
                let c = ic.curiscols[ic.curiscol].clone().unwrap_or_default();
                zlrputs(&c);
            }
        }
        while ic.curisbeg < MAX_POS {
            let b = ic.begpos[ic.curisbeg];
            let e = ic.endpos[ic.curisbeg];
            let fi = e < b || b == -1;
            let more = (fi || pos == b) && ic.patidx < ic.patcols.len();
            if !more {
                break;
            }
            if !fi {
                let mut i = ic.curissend;
                while i < MAX_POS - 1 && ic.sendpos[i] <= e {
                    i += 1;
                }
                let mut j = MAX_POS - 1;
                while j > i {
                    ic.sendpos[j] = ic.sendpos[j - 1];
                    j -= 1;
                }
                ic.sendpos[i] = e;
                zcputs(None, COL_NO);
                let c = ic.patcols[ic.patidx].clone();
                zlrputs(&c);
                if ic.curiscol + 1 < MAX_POS {
                    ic.curiscol += 1;
                    let k = ic.curiscol;
                    ic.curiscols[k] = Some(c);
                }
            }
            ic.patidx += 1;
            ic.curisbeg += 1;
        }
    });
}

/// printfmt() without escapes but with in-string colours.
fn clprintfmt(z: &mut Zle, p: &str, ml: i32) -> i32 {
    let cols = zterm_columns();
    let mlend = get(&MLEND);
    let mut ml = ml;
    let mut cc = 0;

    initiscol();

    for (i, c) in p.chars().enumerate() {
        doiscol(i as i32);
        cc += 1;
        if c == '\n' {
            cleareol();
            cc = 0;
        }
        if ml == mlend - 1 && cc % cols == cols - 1 {
            return 0;
        }
        write_tty(c.encode_utf8(&mut [0; 4]));
        if cc % cols == 0 {
            ml += 1;
        }
        if get(&MSCROLL) != 0 && cc % cols == 0 && predec(&MRESTLINES) == 0 {
            let ask = asklistscroll(z, ml);
            if ask != 0 {
                return ask;
            }
        }
    }
    cleareol();
    0
}

/// nicezputs() with in-string colours and scrolling.
fn clnicezputs(z: &mut Zle, do_colors: bool, s: &str, ml: i32) -> i32 {
    let cols = zterm_columns();
    let mlend = get(&MLEND);
    let mut ml = ml;
    let oml = ml;
    let mut i = 0;
    let mut col = 0;
    let ums = super::tricky::untokenize(&s.chars().collect::<Vec<_>>());

    if do_colors {
        initiscol();
    }
    for c in ums {
        let rep = crate::zle::utils::nicechar(c);
        let asis = rep.chars().eq(std::iter::once(c));
        if do_colors {
            doiscol(i);
            i += 1;
        }
        // A printable character goes out whole; a made-visible one
        // character of its representation at a time.
        let units: Vec<(String, i32)> = if asis {
            vec![(rep, zwcwidth(c))]
        } else {
            rep.chars().map(|x| (x.to_string(), 1)).collect()
        };
        for (u, w) in units {
            if ml == mlend - 1 && col == cols - 1 {
                set(&MLPRINTED, ml - oml);
                return 0;
            }
            write_tty(&u);
            col += w;
            if col > cols {
                ml += 1;
                if get(&MSCROLL) != 0 && predec(&MRESTLINES) == 0 {
                    let ask = asklistscroll(z, ml);
                    if ask != 0 {
                        set(&MLPRINTED, ml - oml);
                        return ask;
                    }
                }
                col -= cols;
                if do_colors {
                    write_tty(" \x08");
                }
            }
        }
    }
    set(&MLPRINTED, ml - oml);
    0
}

/// The colour for a match from the patterns; 1 if it has in-string
/// colours (initiscol() starts them).
fn putmatchcol(group: Option<&str>, n: &str) -> i32 {
    let lc = mcolors();
    for pc in &lc.pats {
        let gok = match (&pc.prog, group) {
            (Some(p), Some(g)) => p.matches(g),
            _ => true,
        };
        if !gok {
            continue;
        }
        if let Some((nrefs, beg, end)) = pattryrefs(&pc.pat, n) {
            ISCOL.with(|ic| {
                let mut ic = ic.borrow_mut();
                ic.nrefs = nrefs;
                ic.begpos = beg;
                ic.endpos = end;
            });
            if pc.cols.len() > 1 {
                ISCOL.with(|ic| ic.borrow_mut().patcols = pc.cols.clone());
                return 1;
            }
            zlrputs(&pc.cols[0]);
            return 0;
        }
    }
    zcputs(group, COL_NO);
    0
}

/// The colour for a file with mode `m`; `special` is a colour to use
/// instead of the mode's (or -1).
fn putfilecol(group: Option<&str>, filename: &str, m: u32, special: i32) -> i32 {
    let lc = mcolors();
    for pc in &lc.pats {
        let gok = match (&pc.prog, group) {
            (Some(p), Some(g)) => p.matches(g),
            _ => true,
        };
        if !gok {
            continue;
        }
        if let Some((nrefs, beg, end)) = pattryrefs(&pc.pat, filename) {
            ISCOL.with(|ic| {
                let mut ic = ic.borrow_mut();
                ic.nrefs = nrefs;
                ic.begpos = beg;
                ic.endpos = end;
            });
            if pc.cols.len() > 1 {
                ISCOL.with(|ic| ic.borrow_mut().patcols = pc.cols.clone());
                return 1;
            }
            zlrputs(&pc.cols[0]);
            return 0;
        }
    }

    let m = m as libc::mode_t;
    let fmt = m & libc::S_IFMT;
    let colour: Option<usize> = if special != -1 {
        Some(special as usize)
    } else if fmt == libc::S_IFDIR {
        Some(if m & libc::S_IWOTH != 0 {
            if m & libc::S_ISVTX != 0 {
                COL_TW
            } else {
                COL_OW
            }
        } else if m & libc::S_ISVTX != 0 {
            COL_ST
        } else {
            COL_DI
        })
    } else if fmt == libc::S_IFLNK {
        Some(COL_LN)
    } else if fmt == libc::S_IFIFO {
        Some(COL_PI)
    } else if fmt == libc::S_IFSOCK {
        Some(COL_SO)
    } else if fmt == libc::S_IFBLK {
        Some(COL_BD)
    } else if fmt == libc::S_IFCHR {
        Some(COL_CD)
    } else if m & libc::S_ISUID != 0 {
        Some(COL_SU)
    } else if m & libc::S_ISGID != 0 {
        Some(COL_SG)
    } else if fmt == libc::S_IFREG && m & (libc::S_IXUSR | libc::S_IXGRP | libc::S_IXOTH) != 0 {
        Some(COL_EX)
    } else {
        None
    };
    if let Some(c) = colour {
        zcputs(group, c);
        return 0;
    }

    for ec in &lc.exts {
        let gok = match (&ec.prog, group) {
            (Some(p), Some(g)) => p.matches(g),
            _ => true,
        };
        if filename.ends_with(&ec.ext) && gok {
            zlrputs(&ec.col);
            return 0;
        }
    }

    // A suffix alias.
    let f: Vec<char> = filename.chars().collect();
    if f.len() > 2 {
        let mut suf = f.len() - 1;
        while suf > 1 {
            if f[suf - 1] == '.' {
                let sx: String = f[suf..].iter().collect();
                let hit = crate::shell::with_shell(|sh| {
                    sh.assoc(crate::types::SALIASES)
                        .is_some_and(|m| m.contains_key(&sx))
                });
                if hit {
                    zcputs(group, COL_SA);
                    return 0;
                }
                break;
            }
            suf -= 1;
        }
    }
    zcputs(group, COL_FI);
    0
}

// ---- printing the list --------------------------------------------------

const COMPLETE_WIDGETS: &[&str] = &[
    "complete-word",
    "expand-or-complete",
    "expand-or-complete-prefix",
    "menu-complete",
    "menu-expand-or-complete",
    "menu-select",
];

/// Asks whether to scroll on ($LISTPROMPT); nonzero to stop listing.
fn asklistscroll(z: &mut Zle, ml: i32) -> i32 {
    let mut ret = 0;

    compprintfmt(z, None, 1, 1, true, ml, None);

    flush_tty();
    menuselect_bindings();
    let ls = LSKEYMAP.with(|k| k.borrow().clone());
    z.selectlocalmap(ls);
    match z.getkeycmd().as_deref() {
        None | Some("send-break") => ret = 1,
        Some(
            "accept-line"
            | "down-history"
            | "down-line-or-history"
            | "down-line-or-search"
            | "vi-down-line-or-history",
        ) => set(&MRESTLINES, 1),
        Some(c) if COMPLETE_WIDGETS.contains(&c) => set(&MRESTLINES, zterm_lines() - 1),
        Some("accept-search") => ret = 1,
        Some(_) => {
            z.ungetkeycmd();
            ret = 1;
        }
    }
    z.selectlocalmap(None);
    let cols = zterm_columns();
    write_tty("\r");
    write_tty(&" ".repeat((cols - 1).max(0) as usize));
    write_tty("\r");
    ret
}

fn compprintnl(z: &mut Zle, ml: i32) -> i32 {
    cleareol();
    write_tty("\n");
    if get(&MSCROLL) != 0 && predec(&MRESTLINES) == 0 {
        let ask = asklistscroll(z, ml);
        if ask != 0 {
            return ask;
        }
    }
    0
}

/// Prints an explanation string (or, for None, the status line of a
/// scrolling or selection list). Returns the lines it took.
fn compprintfmt(
    z: &mut Zle,
    fmt: Option<&str>,
    n: i32,
    dopr: i32,
    doesc: bool,
    ml: i32,
    stop: Option<&mut i32>,
) -> i32 {
    let cols = zterm_columns();
    let mlend = get(&MLEND);
    let mut dopr = dopr;
    let mut ml = ml;
    let mut l = 0;
    let mut cc = 0;
    let stat = fmt.is_none();
    let fmt: String = match fmt {
        Some(f) => f.to_string(),
        None if get(&MLBEG) >= 0 => match MSTATUS.with(|m| m.borrow().clone()) {
            None => {
                set(&MLPRINTED, 0);
                return 0;
            }
            Some(s) => {
                cc = -1;
                s
            }
        },
        None => MLISTP.with(|m| m.borrow().clone()).unwrap_or_default(),
    };
    let p: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < p.len() {
        let c = p[i];
        let width = zwcwidth(c);

        if doesc && c == '%' {
            i += 1;
            if i >= p.len() {
                break;
            }
            let mut arg = 0;
            while i < p.len() && p[i].is_ascii_digit() {
                arg = arg * 10 + p[i].to_digit(10).unwrap_or(0) as i32;
                i += 1;
            }
            let Some(&cchar) = p.get(i) else {
                break;
            };
            i += 1;
            let mut m = false;
            let mut nc = String::new();
            let nlist = z.comp.listdat.nlist;
            let nlines = z.comp.listdat.nlines;
            match cchar {
                '%' => {
                    if dopr == 1 {
                        applytextattributes();
                        write_tty("%");
                    }
                    cc += 1;
                }
                'n' => {
                    if !stat {
                        let s = n.to_string();
                        if dopr == 1 {
                            applytextattributes();
                            write_tty(&s);
                        }
                        cc += s.len() as i32;
                    }
                }
                'B' | 'b' | 'S' | 's' | 'U' | 'u' => {
                    if dopr != 0 {
                        tsetattrs(|t| match cchar {
                            'B' => t.bold = true,
                            'b' => t.bold = false,
                            'S' => t.standout = true,
                            's' => t.standout = false,
                            'U' => t.underline = true,
                            _ => t.underline = false,
                        });
                    }
                }
                'F' | 'K' => {
                    let colour = if p.get(i) == Some(&'{') {
                        i += 1;
                        let st = i;
                        while i < p.len() && p[i] != '}' {
                            i += 1;
                        }
                        let c: String = p[st..i].iter().collect();
                        if i < p.len() {
                            i += 1;
                        }
                        c
                    } else {
                        arg.to_string()
                    };
                    let key = if cchar == 'F' { "fg" } else { "bg" };
                    let ok = !attr_to_sgr(&format!("{}={}", key, colour)).is_empty();
                    if ok && dopr != 0 {
                        tsetattrs(|t| {
                            if cchar == 'F' {
                                t.fg = Some(colour);
                            } else {
                                t.bg = Some(colour);
                            }
                        });
                    }
                }
                'f' => {
                    if dopr != 0 {
                        tsetattrs(|t| t.fg = None);
                    }
                }
                'k' => {
                    if dopr != 0 {
                        tsetattrs(|t| t.bg = None);
                    }
                }
                'H' => {
                    if p.get(i) == Some(&'{') {
                        let st = i + 1;
                        let mut e = st;
                        while e < p.len() && p[e] != '}' {
                            e += 1;
                        }
                        let spec: String = p[st.min(e)..e].iter().collect();
                        i = (e + 1).min(p.len());
                        if dopr != 0 {
                            treplaceattrs(parsehighlight(&spec));
                        }
                    } else {
                        treplaceattrs(Txt::new());
                    }
                }
                '{' => {
                    if arg != 0 {
                        cc += arg;
                    }
                    if dopr != 0 {
                        applytextattributes();
                    }
                    while i < p.len() && !(p[i] == '%' && p.get(i + 1) == Some(&'}')) {
                        if dopr != 0 {
                            write_tty(p[i].encode_utf8(&mut [0; 4]));
                        }
                        i += 1;
                    }
                    if i < p.len() {
                        i += 2;
                    }
                }
                'm' | 'M' => {
                    if stat {
                        let v = format!(
                            "{}/{}",
                            if n != 0 { get(&MLASTM) } else { get(&MSELECT) },
                            nlist
                        );
                        nc = if cchar == 'm' { v } else { format!("{:<9}", v) };
                        m = true;
                    }
                }
                'l' | 'L' => {
                    if stat {
                        let v = format!("{}/{}", ml + 1, nlines);
                        nc = if cchar == 'l' { v } else { format!("{:<9}", v) };
                        m = true;
                    }
                }
                'p' | 'P' if stat => {
                    let mfirstl = get(&MFIRSTL);
                    let pct = if n != 0 {
                        mfirstl != 0
                    } else {
                        get(&MLBEG) > 0 || ml != mfirstl
                    };
                    nc = if ml == nlines - 1 {
                        "Bottom".to_string()
                    } else if pct {
                        let v = ((ml + 1) * 100) / nlines.max(1);
                        if cchar == 'p' {
                            format!("{}%", v)
                        } else {
                            format!("{:2}%   ", v)
                        }
                    } else if cchar == 'p' {
                        "Top".to_string()
                    } else {
                        "Top   ".to_string()
                    };
                    m = true;
                }
                _ => {}
            }
            if m && dopr != 0 {
                let mut ln = nc.len() as i32;
                if ln + cc > cols - 2 {
                    ln -= ln + cc - (cols - 2);
                    nc.truncate(ln.max(0) as usize);
                }
                applytextattributes();
                write_tty(&nc);
                cc += ln;
            }
        } else {
            cc += width;

            if (cc >= cols - 2 || c == '\n') && stat {
                dopr = 2;
            }
            if c == '\n' {
                if dopr == 1 {
                    cleareol();
                }
                l += 1 + ((cc - 1) / cols);
                cc = 0;
            }
            if dopr == 1 {
                applytextattributes();
                if ml == mlend - 1 && (cc % cols) == cols - 1 {
                    dopr = 0;
                    i += 1;
                    continue;
                }
                write_tty(c.encode_utf8(&mut [0; 4]));
                i += 1;
                let beg = cc % cols == 0;
                if beg && !stat {
                    ml += 1;
                    write_tty(" \x08");
                }
                if get(&MSCROLL) != 0
                    && beg
                    && predec(&MRESTLINES) == 0
                    && asklistscroll(z, ml) != 0
                {
                    if let Some(s) = stop {
                        *s = 1;
                    }
                    if stat && n != 0 {
                        set(&MFIRSTL, -1);
                    }
                    let r = l + if cc != 0 { (cc - 1) / cols } else { 0 };
                    set(&MLPRINTED, r);
                    return r;
                }
            } else {
                i += 1;
            }
        }
    }
    if dopr != 0 {
        treplaceattrs(Txt::new());
        applytextattributes();
        if cc % cols == 0 {
            write_tty(" \x08");
        }
        cleareol();
    }
    if stat && n != 0 {
        set(&MFIRSTL, -1);
    }
    // Not subtracting 1 from cc here is right, unlike printfmt.
    let r = l + (cc / cols);
    set(&MLPRINTED, r);
    r
}

/// zputs() with scrolling.
fn compzputs(z: &mut Zle, s: &str, ml: i32) -> i32 {
    let cols = zterm_columns();
    let mut ml = ml;
    let mut col = 0;
    for c in s.chars() {
        if super::tricky::itok(c) {
            continue;
        }
        write_tty(c.encode_utf8(&mut [0; 4]));
        if c == '\n' {
            cleareol();
        }
        if get(&MSCROLL) != 0 {
            col += 1;
            if col == cols || c == '\n' {
                ml += 1;
                if predec(&MRESTLINES) == 0 {
                    let ask = asklistscroll(z, ml);
                    if ask != 0 {
                        return ask;
                    }
                }
                col = 0;
            }
        }
    }
    0
}

/// compprintlist()'s statics: where drawing a partly shown list can
/// start again.
#[derive(Clone)]
struct PrintListState {
    lasttype: i32,
    lastbeg: i32,
    lastml: i32,
    lastinvcount: i32,
    lastn: i32,
    lastnl: i32,
    lastnlnct: i32,
    lastg: Option<usize>,
    lastp: usize,
    lastexpl: usize,
}

impl PrintListState {
    const fn new() -> Self {
        PrintListState {
            lasttype: 0,
            lastbeg: 0,
            lastml: 0,
            lastinvcount: -1,
            lastn: 0,
            lastnl: 0,
            lastnlnct: -1,
            lastg: None,
            lastp: 0,
            lastexpl: 0,
        }
    }
}

/// Prints the list; returns the number of matches printed.
fn compprintlist(z: &mut Zle, showall: bool) -> i32 {
    let zlines = zterm_lines();
    let nlnct = z.refresh.nlnct as i32;
    let mlbeg = get(&MLBEG);
    let mlend = get(&MLEND);
    let mnew = get(&MNEW);
    let mselect = get(&MSELECT);
    let mcols = get(&MCOLS);
    let mhasstat = get(&MHASSTAT);
    let nlines = z.comp.listdat.nlines;
    let onlyexpl = z.comp.listdat.onlyexpl;
    let dolist = |x: i32| x >= mlbeg && x < mlend;
    let dolistcl = |x: i32| x >= mlbeg && x < mlend + 1;
    let dolistnl = |x: i32| x >= mlbeg && x < mlend - 1;

    let mut pl = PRINTLIST.with(|p| p.borrow().clone());
    let mut pnl = false;
    let mut ml = 0;
    let mut printed = 0;
    let mut stop = 0;
    let mut asked = true;
    let mut lastused = false;

    set(&MFIRSTL, -1);
    if mnew != 0 || pl.lastinvcount != z.comp.invcount || pl.lastbeg != mlbeg || mlbeg < 0 {
        pl.lasttype = 0;
        pl.lastg = None;
        pl.lastexpl = 0;
        pl.lastml = 0;
        pl.lastnlnct = -1;
    }
    let mut cl = (if nlines > zlines - nlnct - mhasstat {
        zlines - nlnct - mhasstat
    } else {
        nlines
    }) - i32::from(pl.lastnlnct > nlnct);
    pl.lastnlnct = nlnct;
    set(&MRESTLINES, zlines - 1);
    pl.lastinvcount = z.comp.invcount;

    if cl < 2 {
        cl = -1;
        cleareod();
    }

    'end: {
        let mut g = match pl.lastg {
            Some(lg) if pl.lasttype != 0 => lg,
            _ => 0,
        };
        while g < z.comp.amatches.len() && !z.errflag {
            let nexpl = z.comp.amatches[g].expls.len();
            if nexpl > 0 {
                let mut e = 0;
                if !lastused && pl.lasttype == 1 {
                    e = pl.lastexpl;
                    ml = pl.lastml;
                    lastused = true;
                }
                while e < nexpl && !z.errflag {
                    let ex = z.comp.amatches[g].expls[e].clone();
                    if (ex.count != 0 || ex.always)
                        && (onlyexpl == 0 || onlyexpl & (if ex.always { 2 } else { 1 }) != 0)
                    {
                        if pnl {
                            if dolistnl(ml) && compprintnl(z, ml) != 0 {
                                break 'end;
                            }
                            ml += 1;
                            if dolistcl(ml) && cl >= 0 {
                                cl -= 1;
                                if cl <= 1 {
                                    cl = -1;
                                    cleareod();
                                }
                            }
                        }
                        if mlbeg < 0 && get(&MFIRSTL) < 0 {
                            set(&MFIRSTL, ml);
                        }
                        let s = ex.str.clone().unwrap_or_default();
                        compprintfmt(
                            z,
                            Some(s.as_str()),
                            if ex.always { -1 } else { ex.count },
                            i32::from(dolist(ml)),
                            true,
                            ml,
                            Some(&mut stop),
                        );
                        if mselect >= 0 {
                            let mm = mcols * ml;
                            for i in 0..mcols {
                                tab_set(
                                    mm + i,
                                    Mte {
                                        gm: None,
                                        marked: true,
                                    },
                                );
                            }
                        }
                        if stop != 0 {
                            break 'end;
                        }
                        if pl.lasttype == 0 && ml >= mlbeg {
                            pl.lasttype = 1;
                            pl.lastg = Some(g);
                            pl.lastbeg = mlbeg;
                            pl.lastml = ml;
                            pl.lastexpl = e;
                            pl.lastp = 0;
                            lastused = true;
                        }
                        let mlp = get(&MLPRINTED);
                        ml += mlp;
                        if dolistcl(ml) && cl >= 0 {
                            cl -= mlp;
                            if cl <= 1 {
                                cl = -1;
                                cleareod();
                            }
                        }
                        pnl = true;
                    }
                    e += 1;
                    if mnew == 0 && ml > mlend {
                        break 'end;
                    }
                }
            }
            let grp = z.comp.amatches[g].clone();
            if onlyexpl == 0 && mlbeg < 0 && !grp.ylist.is_empty() {
                if pnl {
                    if dolistnl(ml) && compprintnl(z, ml) != 0 {
                        break 'end;
                    }
                    pnl = false;
                    ml += 1;
                    if cl >= 0 {
                        cl -= 1;
                        if cl <= 1 {
                            cl = -1;
                            cleareod();
                        }
                    }
                }
                if mlbeg < 0 && get(&MFIRSTL) < 0 {
                    set(&MFIRSTL, ml);
                }
                if grp.flags & CGF_LINES != 0 {
                    let mut pp = 0;
                    while pp < grp.ylist.len() {
                        if compzputs(z, &grp.ylist[pp], ml) != 0 {
                            break 'end;
                        }
                        pp += 1;
                        if pp < grp.ylist.len() && compprintnl(z, ml) != 0 {
                            break 'end;
                        }
                    }
                } else {
                    let mut n = grp.lcount;
                    let mut nl = grp.lins;
                    let nc = grp.lins;
                    let mut pp: i32 = 0;
                    while n != 0 && postdec(&mut nl) != 0 && !z.errflag {
                        let mut i = grp.cols;
                        let mut mc = 0;
                        let mut pq = pp;
                        while n != 0 && postdec(&mut i) != 0 && !z.errflag {
                            if pq >= grp.lcount {
                                break;
                            }
                            let s = grp
                                .ylist
                                .get(pq.max(0) as usize)
                                .cloned()
                                .unwrap_or_default();
                            if compzputs(z, &s, get(&MSCROLL)) != 0 {
                                break 'end;
                            }
                            if i != 0 {
                                let w = if grp.widths.is_empty() {
                                    grp.width
                                } else {
                                    grp.widths.get(mc as usize).copied().unwrap_or(0)
                                };
                                let a = w - s.len() as i32;
                                if a > 0 {
                                    write_tty(&" ".repeat(a as usize));
                                }
                            }
                            pq += if grp.flags & CGF_ROWS != 0 { 1 } else { nc };
                            mc += 1;
                            n -= 1;
                        }
                        if n != 0 {
                            if compprintnl(z, ml) != 0 {
                                break 'end;
                            }
                            ml += 1;
                            if cl >= 0 {
                                cl -= 1;
                                if cl <= 1 {
                                    cl = -1;
                                    cleareod();
                                }
                            }
                        }
                        pp += if grp.flags & CGF_ROWS != 0 {
                            grp.cols
                        } else {
                            1
                        };
                    }
                }
            } else if onlyexpl == 0 && (grp.lcount != 0 || (showall && grp.mcount != 0)) {
                let len = grp.matches.len();
                let mut n = grp.dcount;
                let mut nl = grp.lins;
                let nc = grp.lins;
                let mut p;

                if grp.flags & CGF_HASDL != 0 && (lastused || pl.lasttype == 0 || pl.lasttype == 2)
                {
                    if !lastused && pl.lasttype == 2 {
                        p = pl.lastp;
                        ml = pl.lastml;
                        n = pl.lastn;
                        nl = pl.lastnl;
                        lastused = true;
                        pnl = false;
                    } else {
                        p = 0;
                    }
                    while p < len {
                        let show = {
                            let m = grp.matches[p].borrow();
                            m.disp.is_some()
                                && m.flags & CMF_DISPLINE != 0
                                && (showall || m.flags & (CMF_HIDE | CMF_NOLIST) == 0)
                        };
                        if show {
                            if pnl {
                                if dolistnl(ml) && compprintnl(z, ml) != 0 {
                                    break 'end;
                                }
                                ml += 1;
                                if dolistcl(ml) && cl >= 0 {
                                    cl -= 1;
                                    if cl <= 1 {
                                        cl = -1;
                                        cleareod();
                                    }
                                }
                            }
                            if pl.lasttype == 0 && ml >= mlbeg {
                                pl.lasttype = 2;
                                pl.lastg = Some(g);
                                pl.lastbeg = mlbeg;
                                pl.lastml = ml;
                                pl.lastp = p;
                                pl.lastn = n;
                                pl.lastnl = nl;
                                lastused = true;
                            }
                            if get(&MFIRSTL) < 0 {
                                set(&MFIRSTL, ml);
                            }
                            if dolist(ml) {
                                printed += 1;
                            }
                            if clprintm(z, g, Some(p), 0, ml, true, 0) != 0 {
                                break 'end;
                            }
                            let mlp = get(&MLPRINTED);
                            ml += mlp;
                            if dolistcl(ml) {
                                cl -= mlp;
                                if cl <= 1 {
                                    cl = -1;
                                    cleareod();
                                }
                            }
                            pnl = true;
                        }
                        if mnew == 0 && ml > mlend {
                            break 'end;
                        }
                        p += 1;
                    }
                }
                if n != 0 && pnl {
                    if dolistnl(ml) && compprintnl(z, ml) != 0 {
                        break 'end;
                    }
                    pnl = false;
                    ml += 1;
                    if dolistcl(ml) && cl >= 0 {
                        cl -= 1;
                        if cl <= 1 {
                            cl = -1;
                            cleareod();
                        }
                    }
                }
                if !lastused && pl.lasttype == 3 {
                    p = pl.lastp;
                    n = pl.lastn;
                    nl = pl.lastnl;
                    ml = pl.lastml;
                    lastused = true;
                } else {
                    p = skipnolist(&z.comp.amatches[g], 0, showall);
                }
                while n != 0 && postdec(&mut nl) != 0 && !z.errflag {
                    if pl.lasttype == 0 && ml >= mlbeg {
                        pl.lasttype = 3;
                        pl.lastg = Some(g);
                        pl.lastbeg = mlbeg;
                        pl.lastml = ml;
                        pl.lastp = p;
                        pl.lastn = n;
                        pl.lastnl = nl + 1;
                        lastused = true;
                    }
                    let mut i = grp.cols;
                    let mut mc = 0;
                    let mut q = p;
                    while n != 0 && postdec(&mut i) != 0 && !z.errflag {
                        let wid = gwidth(z, g, mc);
                        if q >= len {
                            if clprintm(z, g, None, mc, ml, i == 0, wid) != 0 {
                                break 'end;
                            }
                            break;
                        }
                        if clprintm(z, g, Some(q), mc, ml, i == 0, wid) != 0 {
                            break 'end;
                        }
                        if dolist(ml) {
                            printed += 1;
                        }
                        let mlp = get(&MLPRINTED);
                        ml += mlp;
                        if dolistcl(ml) {
                            cl -= mlp;
                            if cl < 1 {
                                cl = -1;
                                cleareod();
                            }
                        }
                        if get(&MFIRSTL) < 0 {
                            set(&MFIRSTL, ml);
                        }
                        n -= 1;
                        if n != 0 {
                            let mut j = if grp.flags & CGF_ROWS != 0 { 1 } else { nc };
                            while j != 0 && q < len {
                                q = skipnolist(&z.comp.amatches[g], q + 1, showall);
                                j -= 1;
                            }
                        }
                        mc += 1;
                    }
                    while postdec(&mut i) > 0 {
                        let wid = gwidth(z, g, mc);
                        if clprintm(z, g, None, mc, ml, i == 0, wid) != 0 {
                            break 'end;
                        }
                        mc += 1;
                    }
                    if n != 0 {
                        if dolistnl(ml) && compprintnl(z, ml) != 0 {
                            break 'end;
                        }
                        ml += 1;
                        if dolistcl(ml) && cl >= 0 {
                            cl -= 1;
                            if cl <= 1 {
                                cl = -1;
                                cleareod();
                            }
                        }
                        if nl != 0 {
                            let mut j = if grp.flags & CGF_ROWS != 0 {
                                grp.cols
                            } else {
                                1
                            };
                            while j != 0 && p < len {
                                p = skipnolist(&z.comp.amatches[g], p + 1, showall);
                                j -= 1;
                            }
                        }
                    }
                    if mnew == 0 && ml > mlend {
                        break 'end;
                    }
                }
            }
            if grp.lcount != 0 || (showall && grp.mcount != 0) {
                pnl = true;
            }
            g += 1;
        }
        asked = false;
    }
    PRINTLIST.with(|p| *p.borrow_mut() = pl);

    set(&MSTATPRINTED, 0);
    set_lastlistlen(0);
    let nlnct = z.refresh.nlnct as i32;
    if nlnct <= 1 {
        set(&MSCROLL, 0);
    }
    if clearflag() != 0 {
        // Back up to the prompt, if always_last_prompt is set and the
        // list fits.
        if mlbeg >= 0 {
            let mut nl = nlines + nlnct;
            if nl >= zlines {
                if mhasstat != 0 {
                    write_tty("\n");
                    let mline = get(&MLINE);
                    compprintfmt(z, None, 0, 1, true, mline, None);
                    set(&MSTATPRINTED, 1);
                }
                nl = zlines - 1;
            } else {
                nl -= 1;
            }
            tcup(nl);
            z.showinglist = -1;
            set_lastlistlen(nlines);
        } else {
            let nl = nlines + nlnct - 1;
            if nl < zlines {
                cleareol();
                tcup(nl);
                z.showinglist = -1;
                set_lastlistlen(nlines);
            } else {
                set_clearflag(0);
                if !asked {
                    set(&MRESTLINES, i32::from(ml + nlnct > zlines));
                    compprintnl(z, ml);
                }
            }
        }
    } else if !asked {
        set(&MRESTLINES, i32::from(ml + nlnct > zlines));
        compprintnl(z, ml);
    }
    let ls = if clearflag() != 0 { 1 } else { -1 };
    set_listshown(z, ls);
    set(&MNEW, 0);

    printed
}

/// Prints match `mp` of group `g` (None: an empty cell) in column `mc`
/// of list line `ml`, recording it in the match table when selecting.
fn clprintm(
    z: &mut Zle,
    g: usize,
    mp: Option<usize>,
    mc: i32,
    ml: i32,
    lastc: bool,
    width: i32,
) -> i32 {
    let cols = zterm_columns();
    let mlbeg = get(&MLBEG);
    let mlend = get(&MLEND);
    let dolist = ml >= mlbeg && ml < mlend;
    let mselect = get(&MSELECT);
    let mcols = get(&MCOLS);
    let mut subcols = 0;
    let mut stop = 0;
    let mut ret = 0;

    if LAST_GROUP.with(Cell::get) != Some(g) {
        LAST_CAP.with(|l| l.borrow_mut().clear());
    }
    LAST_GROUP.with(|l| l.set(Some(g)));

    let Some(grp) = z.comp.amatches.get(g) else {
        return 0;
    };
    let name = grp.name.clone();
    let gflags = grp.flags;
    let name = name.as_deref();

    let Some(mi) = mp else {
        if dolist {
            zcputs(name, COL_SP);
            let len = width - 2;
            if len > 0 {
                write_tty(&" ".repeat(len as usize));
            }
            zcoff();
        }
        set(&MLPRINTED, 0);
        return 0;
    };
    let Some(mr) = grp.matches.get(mi).cloned() else {
        return 0;
    };
    let mx = if grp.widths.is_empty() {
        mc * grp.width
    } else {
        grp.widths.iter().take(mc.max(0) as usize).sum()
    };

    let (all, empty) = {
        let m = mr.borrow();
        (
            m.flags & CMF_ALL != 0,
            m.disp.as_deref().is_none_or(str::is_empty),
        )
    };
    if all && empty {
        bld_all_str(z, &mr);
    }
    let m = mr.borrow().clone();
    let lc = mcolors();
    let cell = Mte {
        gm: Some((g, mi)),
        marked: m.flags & CMF_DUMMY != 0,
    };

    set(&MLASTM, m.gnum);
    if let (Some(disp), true) = (&m.disp, m.flags & CMF_DISPLINE != 0) {
        if mselect >= 0 {
            let mm = mcols * ml;
            for i in 0..mcols {
                tab_set(mm + i, cell);
            }
        }
        if !dolist {
            let r = super::tricky::printfmt(z, disp, 0, false, false);
            set(&MLPRINTED, r);
            return 0;
        }
        if m.gnum == mselect {
            let mm = mcols * ml;
            set(&MLINE, ml);
            set(&MCOL, 0);
            MMTABP.with(|t| t.set(Some(mm.max(0) as usize)));
            zcputs(name, COL_MA);
        } else if m.flags & CMF_NOLIST != 0 && hascol(&lc, COL_HI) {
            zcputs(name, COL_HI);
        } else if mselect >= 0 && m.flags & (CMF_MULT | CMF_FMULT) != 0 && hascol(&lc, COL_DU) {
            zcputs(name, COL_DU);
        } else {
            subcols = putmatchcol(name, disp);
        }
        if subcols != 0 {
            ret = clprintfmt(z, disp, ml);
        } else {
            compprintfmt(z, Some(disp.as_str()), 0, 1, false, ml, Some(&mut stop));
            if stop != 0 {
                ret = 1;
            }
        }
        zcoff();
    } else {
        if mselect >= 0 {
            let mm = mcols * ml;
            let k = if width != 0 { width } else { mcols };
            for i in 0..k {
                tab_set(mx + mm + i, cell);
            }
        }
        let shown = m.disp.clone().or_else(|| m.str.clone()).unwrap_or_default();
        if !dolist {
            let nc = super::tricky::nicewidth(&shown);
            set(&MLPRINTED, if nc != 0 { (nc - 1) / cols } else { 0 });
            return 0;
        }
        let follow = lc.flags & LC_FOLLOW_SYMLINKS != 0;
        if m.gnum == mselect {
            let mm = mcols * ml;
            set(&MCOL, mx);
            set(&MLINE, ml);
            MMTABP.with(|t| t.set(Some((mx + mm).max(0) as usize)));
            zcputs(name, COL_MA);
        } else if m.flags & CMF_NOLIST != 0 {
            zcputs(name, COL_HI);
        } else if mselect >= 0 && m.flags & (CMF_MULT | CMF_FMULT) != 0 {
            zcputs(name, COL_DU);
        } else if m.mode != 0 {
            // A symlink is orphaned when lstat worked and stat did not.
            let orphan = if m.fmode == 0 { COL_OR as i32 } else { -1 };
            let s = m.str.clone().unwrap_or_default();
            subcols = putfilecol(name, &s, if follow { m.fmode } else { m.mode }, orphan);
        } else {
            subcols = putmatchcol(name, &shown);
        }

        ret = clnicezputs(z, subcols != 0, &shown, ml);
        if ret != 0 {
            zcoff();
            return 1;
        }
        let mut len = super::tricky::nicewidth(&shown);
        set(&MLPRINTED, if len != 0 { (len - 1) / cols } else { 0 });

        let modec = if follow { m.fmodec } else { m.modec };
        if gflags & CGF_FILES != 0 && modec != '\0' {
            if m.gnum != mselect {
                zcoff();
                zcputs(name, COL_TC);
            }
            write_tty(modec.encode_utf8(&mut [0; 4]));
            len += 1;
        }
        len = width - len - 2;
        if len > 0 {
            if m.gnum != mselect {
                zcoff();
                zcputs(name, COL_SP);
            }
            write_tty(&" ".repeat(len as usize));
        }
        zcoff();
        if !lastc {
            zcputs(name, COL_SP);
            write_tty("  ");
            zcoff();
        }
    }
    ret
}

/// For singledraw(): the column where the match at (`c`, `l`) starts,
/// its column number in the list and whether it is last on the line.
fn singlecalc(c: i32, l: i32) -> (i32, i32, bool) {
    let cols = zterm_columns();
    let base = (l * cols) as isize;
    let mp = tab(base + c as isize);
    let mut c2 = c;
    let mut n = 0;
    let mut first = true;
    let mut op = Mte::default();
    let mut j = c;
    while j >= 0 {
        let p = tab(base + j as isize);
        if p == mp {
            c2 = j;
        }
        if !first && p != op {
            n += 1;
        }
        op = p;
        first = false;
        j -= 1;
    }
    let mut lc = true;
    for k in c2..cols {
        let p = tab(base + k as isize);
        if !p.null() && mp != p {
            lc = false;
        }
    }
    (n, c2, lc)
}

/// Redraws just the old and the new selected match.
fn singledraw(z: &mut Zle) {
    let zlines = zterm_lines();
    let cols = zterm_columns();
    let nlnct = z.refresh.nlnct as i32;
    let (mcol, mline, mlbeg) = (get(&MCOL), get(&MLINE), get(&MLBEG));
    let (mocol, moline, molbeg) = (get(&MOCOL), get(&MOLINE), get(&MOLBEG));

    let t1 = mline - mlbeg;
    let t2 = moline - molbeg;
    let ((mc1, ml1, md1), (mc2, ml2, md2)) = if t2 < t1 {
        ((mocol, moline, t2), (mcol, mline, t1))
    } else {
        ((mcol, mline, t1), (mocol, moline, t2))
    };
    let (mcc1, mc1, lc1) = singlecalc(mc1, ml1);
    let (mcc2, mc2, lc2) = singlecalc(mc2, ml2);

    for (i, (mc, ml, md, mcc, lc)) in [(mc1, ml1, md1, mcc1, lc1), (mc2, ml2, md2, mcc2, lc2)]
        .into_iter()
        .enumerate()
    {
        if i == 0 {
            tc_downcurs(md);
        } else if md2 != md1 {
            tc_downcurs(md2 - md1);
        }
        tcright(mc);
        let e = tab((ml * cols + mc) as isize);
        if let Some((g, m)) = e.gm {
            let w = gwidth(z, g, mcc);
            clprintm(z, g, Some(m), mcc, ml, lc, w);
            tcup(get(&MLPRINTED));
        }
        write_tty("\r");
    }

    if get(&MSTATPRINTED) != 0 {
        let i = zlines - md2 - nlnct;
        tc_downcurs(i - 1);
        compprintfmt(z, None, 0, 1, true, mline, None);
        tcup(zlines - 1);
    } else {
        tcup(md2 + nlnct);
    }
    z.showinglist = -1;
    set_listshown(z, 1);
}

/// The lister (the comp_list_matches hook).
fn complistmatches(z: &mut Zle) -> i32 {
    if get(&NOSELECT) > 0 {
        set(&NOSELECT, 0);
    }
    if (z.comp.minfo.asked == 2 && get(&MSELECT) < 0)
        || z.refresh.nlnct as i32 >= zterm_lines()
        || z.errflag
    {
        z.showinglist = 0;
        set(&NOSELECT, 1);
        return 1;
    }
    let extendedglob = isset("extendedglob");
    crate::options::set("extendedglob", true);
    let r = complistmatches_body(z);
    crate::options::set("extendedglob", extendedglob);
    r
}

fn complistmatches_body(z: &mut Zle) -> i32 {
    getcols();

    let mselect = get(&MSELECT);
    let zcols = zterm_columns();
    let zlines = zterm_lines();
    let calc = calclist(z, mselect >= 0);
    let mnew = (calc != 0 || get(&MLASTCOLS) != zcols || get(&MLASTLINES) != z.comp.listdat.nlines)
        && mselect >= 0;
    set(&MNEW, i32::from(mnew));
    let nlines = z.comp.listdat.nlines;

    let lastprompt = z
        .comp
        .complastprompt
        .as_deref()
        .is_some_and(|s| !s.is_empty());
    if nlines == 0 || (mselect >= 0 && !(isset("zle") && lastprompt)) {
        z.showinglist = 0;
        set_listshown(z, 0);
        set(&NOSELECT, 1);
        return 1;
    }
    if get(&INSELECT) != 0 || get(&MLBEG) >= 0 {
        set_clearflag(0);
    }

    set(&MSCROLL, 0);
    let mut mlistp = None;
    let enter = mselect >= 0 || get(&MLBEG) >= 0 || {
        mlistp = getsparam("LISTPROMPT");
        mlistp.is_some()
    };
    if enter {
        if mlistp.as_deref() == Some("") {
            mlistp = Some("%SAt %p: Hit TAB for more, or the character to insert%s".to_string());
        }
        MLISTP.with(|m| *m.borrow_mut() = mlistp.clone());
        z.trash();
        // complist does its own attributes, so wipe any zle residue.
        treplaceattrs(Txt::new());
        applytextattributes();
        z.showinglist = 0;
        set_listshown(z, 0);

        set_lastlistlen(0);

        let nlnct = z.refresh.nlnct as i32;
        if mlistp.is_some() {
            set_clearflag(i32::from(isset("zle") && z.comp.dolastprompt != 0));
            set(&MSCROLL, 1);
        } else {
            set_clearflag(1);
            z.comp.minfo.asked = i32::from(nlines + nlnct <= zlines);
        }
    } else {
        MLISTP.with(|m| *m.borrow_mut() = None);
        if asklist(z) != 0 {
            set(&NOSELECT, 1);
            return 1;
        }
    }
    let nlnct = z.refresh.nlnct as i32;
    if get(&MLBEG) >= 0 {
        set(&MLEND, get(&MLBEG) + zlines - nlnct - get(&MHASSTAT));
        while get(&MLINE) >= get(&MLEND) {
            set(&MLBEG, get(&MLBEG) + 1);
            set(&MLEND, get(&MLEND) + 1);
        }
    } else {
        set(&MLEND, 9999999);
    }

    if mnew {
        set(&MTAB_BEEN_REALLOCATED, 1);
        let i = (zcols * nlines).max(0) as usize;
        MTAB.with(|t| *t.borrow_mut() = vec![Mte::default(); i]);
        set(&MLASTCOLS, zcols);
        set(&MCOLS, zcols);
        set(&MLASTLINES, nlines);
        set(&MLINES, nlines);
        MMTABP.with(|t| t.set(None));
    }
    LAST_CAP.with(|l| l.borrow_mut().clear());

    let mlbeg = get(&MLBEG);
    if !mnew && get(&INSELECT) != 0 && get(&ONLNCT) == nlnct && mlbeg >= 0 && mlbeg == get(&MOLBEG)
    {
        if get(&NOSELECT) == 0 {
            singledraw(z);
        }
    } else if compprintlist(z, mselect >= 0) == 0 || clearflag() == 0 {
        set(&NOSELECT, 1);
    }

    set(&ONLNCT, z.refresh.nlnct as i32);
    set(&MOLBEG, get(&MLBEG));
    set(&MOCOL, get(&MCOL));
    set(&MOLINE, get(&MLINE));

    let ns = get(&NOSELECT);
    if ns < 0 {
        0
    } else {
        ns
    }
}

// ---- menu selection -----------------------------------------------------

/// Moves the selection in its line to the column nearest `wish` that
/// holds a match; true if the line has none.
fn adjust_mcol(wish: i32, tabp: &mut isize) -> bool {
    let mcol = get(&MCOL);
    let mcols = get(&MCOLS);
    let row = *tabp - mcol as isize;
    let live = |i: i32| tab(row + i as isize).live();

    let mut p = wish;
    while p >= 0 && !live(p) {
        p -= 1;
    }
    let mut n = wish;
    while n < mcols && !live(n) {
        n += 1;
    }
    if n == mcols {
        n = -1;
    }
    let c = if p < 0 {
        if n < 0 {
            return true;
        }
        n
    } else if n < 0 || (mcol - p) < (n - mcol) {
        p
    } else {
        n
    };
    *tabp = row + c as isize;
    set(&MCOL, c);
    false
}

/// amatches, pmatches, lastmatches and lmatches, saved together.
type SavedLists = (Vec<Cmgroup>, Vec<Cmgroup>, Vec<Cmgroup>, Vec<Cmgroup>);

/// What to restore when undoing in menu selection.
struct Menustack {
    line: Vec<char>,
    cs: usize,
    mline: i32,
    mlbeg: i32,
    nolist: bool,
    acc: i32,
    info: Menuinfo,
    /// The lists of the completion before accept-and-infer-next-history;
    /// None for accept-and-hold.
    lists: Option<SavedLists>,
    brbeg: Vec<Brinfo>,
    brend: Vec<Brinfo>,
    nbrbeg: i32,
    nbrend: i32,
    nmatches: i32,
    origline: Option<String>,
    origcs: i32,
    origll: i32,
    /// The interactive status line and the menu mode.
    status: String,
    mode: i32,
}

#[derive(Clone)]
struct Menusearch {
    str: String,
    line: i32,
    col: i32,
    back: bool,
    state: i32,
    ptr: isize,
}

struct Msearch {
    stack: Vec<Menusearch>,
    str: String,
    state: i32,
}

const MS_OK: i32 = 0;
const MS_FAILED: i32 = 1;
const MS_WRAPPED: i32 = 2;

const MAX_STATUS: i32 = 128;

// Menu modes.
const MM_INTER: i32 = 1;
const MM_FSEARCH: i32 = 2;
const MM_BSEARCH: i32 = 3;

/// Puts `s` in the line with the cursor at `cs`.
fn setzline(z: &mut Zle, s: &[char], cs: i32) {
    z.cs = 0;
    let ll = z.ll();
    z.foredel(ll, CUT_RAW);
    z.insert_chars(s);
    z.cs = (cs.max(0) as usize).min(z.ll());
}

/// Sets the interactive status line from the word typed so far. With
/// `sline`, first puts that line (cursor `scs`) back, returning the old
/// line, cursor, length and the word length.
fn setmstatus(
    z: &mut Zle,
    status: &mut String,
    sline: Option<(&[char], i32)>,
) -> Option<(Vec<char>, i32, i32, i32)> {
    let mut ret = None;

    let (p, s) = if let Some((sl, scs)) = sline {
        let wb = super::tricky::wb();
        let lastend = z.comp.lastend;
        let cs = z.cs as i32;
        ret = Some((z.line.clone(), cs, z.ll() as i32, lastend - wb));
        let sub = |a: i32, b: i32| -> String {
            let n = z.line.len() as i32;
            let (a, b) = (a.clamp(0, n) as usize, b.clamp(0, n) as usize);
            if a < b {
                z.line[a..b].iter().collect()
            } else {
                String::new()
            }
        };
        let p = sub(wb, cs);
        let s = if lastend < cs {
            String::new()
        } else {
            sub(cs, lastend)
        };
        setzline(z, sl, scs);
        (p, s)
    } else {
        (
            z.comp.complastprefix.clone().unwrap_or_default(),
            z.comp.complastsuffix.clone().unwrap_or_default(),
        )
    };
    let pc: Vec<char> = p.chars().collect();
    let sc: Vec<char> = s.chars().collect();
    let pl = pc.len() as i32;
    let sl = sc.len() as i32;
    let max = zterm_columns().min(MAX_STATUS) - 14;

    if max > 12 {
        let h = (max - 2) >> 1;

        let mut st = String::from("interactive: ");
        if pl > h - 3 {
            st.push_str("...");
            st.extend(&pc[(pl - h + 3).clamp(0, pl) as usize..]);
        } else {
            st.push_str(&p);
        }
        st.push_str("[]");
        if sl > h - 3 {
            st.extend(sc.iter().take((h - 3).max(0) as usize));
            st.push_str("...");
        } else {
            st.push_str(&s);
        }
        *status = st;
    }
    ret
}

fn msearchpush(ms: &mut Msearch, p: isize, back: bool) {
    ms.stack.push(Menusearch {
        str: ms.str.clone(),
        line: get(&MLINE),
        col: get(&MCOL),
        back,
        state: ms.state,
        ptr: p,
    });
}

fn msearchpop(ms: &mut Msearch, backp: &mut bool) -> Option<isize> {
    let s = if ms.stack.len() > 1 {
        ms.stack.pop()?
    } else {
        ms.stack.last()?.clone()
    };
    ms.str = s.str;
    set(&MLINE, s.line);
    set(&MCOL, s.col);
    ms.state = s.state;
    *backp = s.back;
    Some(s.ptr)
}

/// Searches the table for a match containing the search string (with
/// `ins` added), from the selected one on; `rep` skips the selected one.
fn msearch(
    z: &Zle,
    ms: &mut Msearch,
    ptr: isize,
    ins: Option<&str>,
    back: bool,
    rep: bool,
    wrapp: &mut i32,
) -> Option<isize> {
    let mcols = get(&MCOLS);
    let (mcol, mline) = (get(&MCOL), get(&MLINE));
    let nlines = z.comp.listdat.nlines;
    let mut x = mcol;
    let mut y = mline;
    let mut wrap = false;
    let owrap = ms.state & MS_WRAPPED;
    let mut rep = rep;
    let mut l = Mte::default();

    msearchpush(ms, ptr, back);

    if let Some(ins) = ins {
        ms.str.push_str(ins);
    }
    let (mut ex, mut ey) = if back { (mcols - 1, -1) } else { (0, nlines) };
    let mut p = (mline * mcols + mcol) as isize;
    if rep {
        l = tab(p);
    }
    loop {
        let e = tab(p);
        if !rep && e.gm.is_some() && e != l {
            l = e;
            if let Some(m) = tab_match(z, e) {
                let m = m.borrow();
                let t = m.disp.as_deref().or(m.str.as_deref()).unwrap_or("");
                if t.contains(ms.str.as_str()) {
                    set(&MCOL, x);
                    set(&MLINE, y);
                    return Some(p);
                }
            }
        }
        rep = false;

        if back {
            p -= 1;
            x -= 1;
            if x < 0 {
                x = mcols - 1;
                y -= 1;
            }
        } else {
            p += 1;
            x += 1;
            if x == mcols {
                x = 0;
                y += 1;
            }
        }
        if x == ex && y == ey {
            if back {
                x = mcols - 1;
                y = nlines - 1;
                p = (y * mcols + x) as isize;
            } else {
                x = 0;
                y = 0;
                p = 0;
            }
            ex = mcol;
            ey = mline;

            if wrap || (x == ex && y == ey) {
                ms.state = MS_FAILED | owrap;
                break;
            }
            ms.state |= MS_WRAPPED;
            wrap = true;
            *wrapp = 1;
        }
    }
    None
}

enum Mv {
    Down,
    Up,
    Right,
    Left,
    Top,
    Bottom,
}

/// The menu_start hook.
fn menu_start(z: &mut Zle, num: i32, _nmesg: i32) -> i32 {
    domenuselect(z, Some(num))
}

/// Menu selection. `dat` is the number of matches when called as the
/// menu_start hook, None from the menu-select widget.
fn domenuselect(z: &mut Zle, dat: Option<i32>) -> i32 {
    if z.comp.hasoldlist == 0 {
        return 2;
    }
    let mut ms = Msearch {
        stack: Vec::new(),
        str: String::new(),
        state: MS_OK,
    };
    let mut status = String::new();

    if FDAT.with(Cell::get)
        || dat.is_some_and(|num| getsparam("MENUSELECT").is_none_or(|s| num < atoi(&s)))
    {
        return 0;
    }
    let zlines = zterm_lines();
    let pl = z.refresh.nlnct as i32;
    let mut step = 1;
    if let Some(s) = getsparam("MENUSCROLL") {
        step = crate::arith::eval(&s).unwrap_or(0) as i32;
        match step.cmp(&0) {
            std::cmp::Ordering::Equal => step = (zlines - pl) >> 1,
            std::cmp::Ordering::Less => step += zlines - pl,
            std::cmp::Ordering::Greater => {}
        }
        if step <= 0 {
            step = 1;
        }
    }
    let mut mode = 0;
    if let Some(s) = getsparam("MENUMODE") {
        if s == "interactive" {
            // Interactive mode shows the line as typed, the match so far
            // in the status line.
            mode = MM_INTER;
            let orig: Vec<char> = z
                .comp
                .origline
                .clone()
                .unwrap_or_default()
                .chars()
                .collect();
            let ocs = z.comp.origcs;
            setzline(z, &orig, ocs);
            setmstatus(z, &mut status, None);
        } else if s.starts_with("search") {
            mode = if s.contains("back") {
                MM_BSEARCH
            } else {
                MM_FSEARCH
            };
        }
    }
    let mstatus = getsparam("MENUPROMPT").map(|s| {
        if s.is_empty() {
            "%SScrolling active: current selection at %p%s".to_string()
        } else {
            s
        }
    });
    set(
        &MHASSTAT,
        i32::from(mstatus.as_deref().is_some_and(|s| !s.is_empty())),
    );
    MSTATUS.with(|m| *m.borrow_mut() = mstatus);
    FDAT.with(|f| f.set(true));
    menuselect_bindings();
    let mk = MSKEYMAP.with(|k| k.borrow().clone());
    z.selectlocalmap(mk);
    set(&NOSELECT, 1);
    let skip_limit = z.comp.nmatches.max(1) as usize;
    let saved_mult = z.zmod.mult;
    for _ in 0..skip_limit {
        let Some(mr) = cur_match(z) else {
            break;
        };
        let m = mr.borrow().clone();
        let skip = (z.comp.menuacc != 0 && {
            let (pre, post) = (z.comp.minfo.prebr.clone(), z.comp.minfo.postbr.clone());
            !hasbrpsfx(z, &m, pre.as_deref(), post.as_deref())
        }) || m.flags & CMF_DUMMY != 0
            || (m.flags & (CMF_NOLIST | CMF_MULT) != 0
                && m.str.as_deref().is_none_or(str::is_empty));
        if !skip {
            break;
        }
        z.zmod.mult = 1;
        do_menucmp(z, 0);
    }
    z.zmod.mult = saved_mult;

    set(&MSELECT, cur_match(z).as_ref().map_or(-1, gnum_of));
    set(&MLINE, 0);
    set(&MLINES, 999999);
    set(&MLBEG, 0);
    set(&MOLBEG, -42);
    set(&MTAB_BEEN_REALLOCATED, 0);

    let oe = z.comp.onlyexpl;
    let mut i = 0;
    let mut acc = 0;
    let mut wishcol = 0;
    let mut setwish = false;
    let mut wasnext = false;
    let mut lbeg = 0;
    let mut wrap = 0;
    let mut broken = 0;
    let mut first = true;
    let mut nolist = false;
    let mut modeline: Vec<char> = Vec::new();
    let mut modecs = 0;
    let mut modell = 0;
    let mut modelen = 0;
    let mut cmd: Option<String> = None;
    let mut do_last_key = false;
    let mut u: Vec<Menustack> = Vec::new();
    // The selected cell; with use_cur, minfo.cur itself (zsh points p
    // at minfo.cur after an undo).
    let mut p: isize = 0;
    let mut use_cur = false;
    let mut goto_getk = false;

    'sel: loop {
        if !goto_getk {
            let mcols = get(&MCOLS);
            if get(&MLINE) < 0 || get(&MTAB_BEEN_REALLOCATED) != 0 {
                let mlines = get(&MLINES);
                let rows = if mcols > 0 {
                    (tab_len() / mcols as usize) as i32
                } else {
                    0
                };
                let msel = get(&MSELECT);
                'rows: for y in 0..mlines.min(rows) {
                    for x in 0..mcols {
                        let e = tab((y * mcols + x) as isize);
                        if e.live() && tab_match(z, e).as_ref().is_some_and(|m| gnum_of(m) == msel)
                        {
                            set(&MCOL, x);
                            set(&MLINE, y);
                            break 'rows;
                        }
                    }
                }
            }
            set(&MTAB_BEEN_REALLOCATED, 0);
            let mut mlbeg = get(&MLBEG);
            let mline = get(&MLINE);
            let mlines = get(&MLINES);
            while mline < mlbeg {
                mlbeg -= step;
                if mlbeg < 0 {
                    mlbeg = 0;
                    if mline < 0 {
                        break;
                    }
                }
            }
            if mlbeg != 0 && lbeg != mlbeg {
                let mut q = ((mlbeg - 1) * mcols) as isize;
                while mlbeg != 0 {
                    if (0..mcols).any(|c| tab(q + c as isize).live()) {
                        break;
                    }
                    q -= mcols as isize;
                    mlbeg -= 1;
                }
            }
            let space = zlines - pl - get(&MHASSTAT);
            if space > 0 {
                while mline >= mlbeg + space {
                    mlbeg += step;
                    if mlbeg + space > mlines {
                        mlbeg = mlines - space;
                    }
                }
            }
            if lbeg != mlbeg {
                let mut q = (mlbeg * mcols) as isize;
                while mlbeg < mlines {
                    if (0..mcols).any(|c| !tab(q + c as isize).null()) {
                        break;
                    }
                    q += mcols as isize;
                    mlbeg += 1;
                }
            }
            set(&MLBEG, mlbeg);
            lbeg = mlbeg;
            z.comp.onlyexpl = 0;
            z.showinglist = -2;
            if first && listshown() == 0 && isset("listbeep") {
                zbeep(z);
            }
            if first {
                // The line as it was, to put back in interactive mode
                // after menu completion inserts a match.
                modeline = z.line.clone();
                modecs = z.cs as i32;
                modell = z.ll() as i32;
                modelen = z.comp.minfo.len;
            }
            first = false;
            if mode == MM_INTER {
                z.statusline = Some(status.clone());
            } else if mode != 0 {
                let mut st = format!(
                    "{}{}isearch{}: ",
                    if ms.state & MS_FAILED != 0 {
                        "failed "
                    } else {
                        ""
                    },
                    if ms.state & MS_WRAPPED != 0 {
                        "wrapped "
                    } else {
                        ""
                    },
                    if mode == MM_FSEARCH { "" } else { " backward" }
                );
                let room = (MAX_STATUS - st.len() as i32 - 1).max(0) as usize;
                st.extend(ms.str.chars().take(room));
                status = st;
                z.statusline = Some(status.clone());
            } else {
                z.statusline = None;
            }
            if get(&NOSELECT) < 0 {
                z.showinglist = 0;
                z.clearlist = false;
                set_clearflag(1);
            }
            z.zrefresh();
            z.statusline = None;
            set(&INSELECT, 1);
            set(&SELECTED, 1);
            if get(&NOSELECT) != 0 {
                if get(&NOSELECT) < 0 {
                    // No selection until after this keystroke.
                    set(&NOSELECT, 0);
                    goto_getk = true;
                } else {
                    broken = 1;
                    break 'sel;
                }
            }
            if !goto_getk {
                if i == 0 {
                    // zsh's loop leaves 0 only when the first cell holds
                    // something; an empty table does not stop it.
                    let mut k = mcols * get(&MLINES);
                    loop {
                        if k == 0 {
                            k = -1;
                            break;
                        }
                        k -= 1;
                        if !tab(k as isize).null() {
                            break;
                        }
                    }
                    if k == 0 {
                        break 'sel;
                    }
                    i = 1;
                }
                let Some(pp) = MMTABP.with(Cell::get) else {
                    // The selected match is not shown: find its line.
                    continue 'sel;
                };
                p = pp as isize;
                use_cur = false;
                let e = tab(p);
                if let Some((g, m)) = e.gm {
                    z.comp.minfo.cur = Some(m);
                    z.comp.minfo.group = Some(g);
                }
                let ce = cur_entry(z);
                let mut mcol = get(&MCOL);
                if setwish {
                    wishcol = mcol;
                } else {
                    match mcol.cmp(&wishcol) {
                        std::cmp::Ordering::Greater => {
                            while mcol > 0 && tab(p - 1) == ce {
                                mcol -= 1;
                                p -= 1;
                            }
                        }
                        std::cmp::Ordering::Less => {
                            while mcol < mcols - 1 && tab(p + 1) == ce {
                                mcol += 1;
                                p += 1;
                            }
                        }
                        std::cmp::Ordering::Equal => {}
                    }
                }
                set(&MCOL, mcol);
                setwish = false;
                wasnext = false;
            }
        }
        goto_getk = false;

        // getk:
        if !do_last_key {
            z.zmod.mult = 1;
            cmd = z.getkeycmd();
            // Do not pass an interrupt on: it spoils the redraw.
            z.errflag = false;
            if get(&MTAB_BEEN_REALLOCATED) != 0 {
                do_last_key = true;
                continue 'sel;
            }
        }
        do_last_key = false;

        let was_inter = mode == MM_INTER;
        let c = cmd.clone().unwrap_or_default();
        let c = c.as_str();
        let mcols = get(&MCOLS);
        let mlines = get(&MLINES);
        let mut mv: Option<Mv> = None;

        if cmd.is_none() || c == "send-break" {
            zbeep(z);
            set(&MOLBEG, -1);
            break 'sel;
        } else if nolist
            && c != "undo"
            && (mode == 0
                || (c != "backward-delete-char" && c != "self-insert" && c != "self-insert-unmeta"))
        {
            z.ungetkeycmd();
            break 'sel;
        } else if c == "accept-line" || c == "accept-search" {
            if mode == MM_FSEARCH || mode == MM_BSEARCH {
                mode = 0;
                continue 'sel;
            }
            acc = 1;
            break 'sel;
        } else if c == "vi-insert" {
            if mode == MM_INTER {
                mode = 0;
            } else {
                // Into interactive mode, as when it starts that way.
                mode = MM_INTER;
                let orig: Vec<char> = z
                    .comp
                    .origline
                    .clone()
                    .unwrap_or_default()
                    .chars()
                    .collect();
                let ocs = z.comp.origcs;
                setzline(z, &orig, ocs);
                setmstatus(z, &mut status, None);
                continue 'sel;
            }
        } else if c == "accept-and-infer-next-history"
            || (mode == MM_INTER && (c == "self-insert" || c == "self-insert-unmeta"))
        {
            let lists = (
                std::mem::take(&mut z.comp.amatches),
                std::mem::take(&mut z.comp.pmatches),
                std::mem::take(&mut z.comp.lastmatches),
                z.comp.lastlmatches.clone(),
            );
            u.push(Menustack {
                line: z.line.clone(),
                cs: z.cs,
                mline: get(&MLINE),
                mlbeg: get(&MLBEG),
                nolist,
                acc: z.comp.menuacc,
                info: z.comp.minfo.clone(),
                lists: Some(lists),
                brbeg: z.comp.brbeg.clone(),
                brend: z.comp.brend.clone(),
                nbrbeg: z.comp.nbrbeg,
                nbrend: z.comp.nbrend,
                nmatches: z.comp.nmatches,
                origline: z.comp.origline.clone(),
                origcs: z.comp.origcs,
                origll: z.comp.origll,
                status: status.clone(),
                mode,
            });
            z.comp.menucmp = 0;
            z.comp.menuacc = 0;
            z.comp.hasoldlist = 0;
            z.comp.minfo.cur = None;
            super::tricky::fixsuffix(z);
            z.handleundo();
            z.comp.validlist = 0;
            invalidate_list(z);
            z.comp.iforcemenu = 1;
            z.comp.comprecursive = 1;
            let mut saved: Option<(Vec<char>, i32)> = None;
            if c != "accept-and-infer-next-history" {
                // Put the line back as typed, add the character, and
                // remember the result: the line shows only what the
                // user typed.
                let orig: Vec<char> = z
                    .comp
                    .origline
                    .clone()
                    .unwrap_or_default()
                    .chars()
                    .collect();
                let ocs = z.comp.origcs;
                setzline(z, &orig, ocs);
                if c == "self-insert" {
                    crate::zle::widgets::misc::selfinsert(z, &[]);
                } else {
                    crate::zle::widgets::misc::selfinsertunmeta(z, &[]);
                }
                z.comp.minfo.len += 1;
                z.comp.minfo.end += 1;

                saved = Some((z.line.clone(), z.cs as i32));
                z.comp.iforcemenu = -1;
            } else {
                mode = 0;
            }
            super::tricky::menucomplete(z, &[]);
            z.comp.iforcemenu = 0;

            if let Some((sline, scs)) = saved {
                if let Some((ml, mcs, mll, mlen)) =
                    setmstatus(z, &mut status, Some((sline.as_slice(), scs)))
                {
                    modeline = ml;
                    modecs = mcs;
                    modell = mll;
                    modelen = mlen;
                }
                // A quoted word's end is lost by now; put it back.
                super::tricky::set_we(scs);
            }

            if z.comp.nmatches < 1 || cur_match(z).is_none() {
                nolist = true;
                z.statusline = if mode == MM_INTER {
                    Some(status.clone())
                } else {
                    None
                };
                if z.comp.nmessages != 0 {
                    z.showinglist = -2;
                    z.zrefresh();
                    set(&NOSELECT, -1);
                } else {
                    z.trash();
                    cleareod();
                    write_tty("no matches\r");
                    flush_tty();
                    tcup(z.refresh.nlnct as i32);
                    z.showinglist = 0;
                    z.clearlist = false;
                    set_clearflag(1);
                    z.zrefresh();
                    z.showinglist = 0;
                    z.clearlist = false;
                }
                z.statusline = None;

                goto_getk = true;
                continue 'sel;
            }
            z.clearlist = true;
            set_listshown(z, 1);
            set(&MSELECT, cur_match(z).as_ref().map_or(-1, gnum_of));
            setwish = true;
            wasnext = true;
            set(&MLINE, 0);
            set(&MOLBEG, -42);
            continue 'sel;
        } else if c == "accept-and-hold" || c == "accept-and-menu-complete" {
            if mode == MM_INTER {
                let cur = z.comp.minfo.cur;
                let m = cur_match(z);
                z.comp.minfo.cur = None;
                if let Some(m) = m {
                    do_single(z, &m);
                }
                z.comp.minfo.cur = cur;
            }
            mode = 0;
            u.push(Menustack {
                line: z.line.clone(),
                cs: z.cs,
                mline: get(&MLINE),
                mlbeg: get(&MLBEG),
                nolist,
                acc: z.comp.menuacc,
                info: z.comp.minfo.clone(),
                lists: None,
                brbeg: z.comp.brbeg.clone(),
                brend: z.comp.brend.clone(),
                nbrbeg: z.comp.nbrbeg,
                nbrend: z.comp.nbrend,
                nmatches: z.comp.nmatches,
                origline: z.comp.origline.clone(),
                origcs: z.comp.origcs,
                origll: z.comp.origll,
                status: status.clone(),
                mode,
            });
            accept_last(z);
            z.handleundo();
            z.comp.comprecursive = 1;
            do_menucmp(z, 0);
            set(&MSELECT, cur_match(z).as_ref().map_or(-1, gnum_of));

            let ce = cur_entry(z);
            p -= get(&MCOL) as isize;
            let ol = get(&MLINE);
            let mut mline = ol;
            loop {
                let mut mcol = 0;
                while mcol < mcols {
                    if tab(p) == ce {
                        break;
                    }
                    mcol += 1;
                    p += 1;
                }
                set(&MCOL, mcol);
                if mcol != mcols {
                    break;
                }
                mline += 1;
                if mline == mlines {
                    mline = 0;
                    p -= (mlines * mcols) as isize;
                }
                if mline == ol {
                    break;
                }
            }
            set(&MLINE, mline);
            if tab(p) != ce {
                set(&NOSELECT, 1);
                z.clearlist = true;
                set_listshown(z, 1);
                z.comp.onlyexpl = 0;
                z.zrefresh();
                break 'sel;
            }
            setwish = true;
            continue 'sel;
        } else if c == "undo" || (mode == MM_INTER && c == "backward-delete-char") {
            let Some(s) = u.pop() else {
                break 'sel;
            };
            z.handleundo();
            setzline(z, &s.line, s.cs as i32);
            z.comp.menuacc = s.acc;
            z.comp.minfo = s.info;
            use_cur = true;
            set(&MLINE, s.mline);
            set(&MLBEG, s.mlbeg);
            if let Some((am, pm, lm, llm)) = s.lists {
                z.comp.amatches = am;
                z.comp.pmatches = pm;
                z.comp.lastmatches = lm;
                z.comp.lastlmatches = llm;
                z.comp.nmatches = s.nmatches;
                z.comp.hasoldlist = 1;
                z.comp.validlist = 1;
            }
            z.comp.brbeg = s.brbeg;
            z.comp.brend = s.brend;
            z.comp.nbrbeg = s.nbrbeg;
            z.comp.nbrend = s.nbrend;
            z.comp.origline = s.origline;
            z.comp.origcs = s.origcs;
            z.comp.origll = s.origll;
            status = s.status;
            mode = s.mode;
            nolist = s.nolist;

            z.clearlist = true;
            setwish = true;
            z.comp.listdat.valid = 0;
            set(&MOLBEG, -42);

            if nolist {
                z.statusline = if mode == MM_INTER {
                    Some(status.clone())
                } else {
                    None
                };
                z.zrefresh();
                z.statusline = None;
                goto_getk = true;
                continue 'sel;
            }
            if mode != 0 {
                continue 'sel;
            }
        } else if c == "redisplay" {
            crate::zle::widgets::core::redisplay(z, &[]);
            set(&MOLBEG, -42);
            continue 'sel;
        } else if c == "clear-screen" {
            crate::zle::widgets::core::clearscreen(z, &[]);
            set(&MOLBEG, -42);
            continue 'sel;
        } else if [
            "down-history",
            "down-line-or-history",
            "down-line-or-search",
            "vi-down-line-or-history",
        ]
        .contains(&c)
        {
            mode = 0;
            wrap = 0;
            mv = Some(Mv::Down);
        } else if [
            "up-history",
            "up-line-or-history",
            "up-line-or-search",
            "vi-up-line-or-history",
        ]
        .contains(&c)
        {
            mode = 0;
            wrap = 0;
            mv = Some(Mv::Up);
        } else if [
            "emacs-forward-word",
            "vi-forward-word",
            "vi-forward-word-end",
            "forward-word",
        ]
        .contains(&c)
        {
            let mut k = zlines - pl - 1;
            let oi = k;
            let mut ll = 0;
            let mut lp: Option<isize> = None;

            mode = 0;
            let mut mline = get(&MLINE);
            if mline == mlines - 1 {
                mv = Some(Mv::Top);
            } else {
                while k > 0 {
                    if mline == mlines - 1 {
                        if k != oi && lp.is_some() {
                            break;
                        }
                        mv = Some(Mv::Top);
                        break;
                    }
                    mline += 1;
                    p += mcols as isize;
                    if adjust_mcol(wishcol, &mut p) {
                        continue;
                    }
                    if tab(p).live() {
                        k -= 1;
                        lp = Some(p);
                        ll = mline;
                    }
                }
                if mv.is_none() {
                    p = lp.unwrap_or(p);
                    mline = ll;
                }
            }
            set(&MLINE, mline);
        } else if ["emacs-backward-word", "vi-backward-word", "backward-word"].contains(&c) {
            let mut k = zlines - pl - 1;
            let oi = k;
            let mut ll = 0;
            let mut lp: Option<isize> = None;

            mode = 0;
            let mut mline = get(&MLINE);
            if mline == 0 {
                mv = Some(Mv::Bottom);
            } else {
                while k > 0 {
                    if mline == 0 {
                        if k != oi && lp.is_some() {
                            break;
                        }
                        mv = Some(Mv::Bottom);
                        break;
                    }
                    mline -= 1;
                    p -= mcols as isize;
                    if adjust_mcol(wishcol, &mut p) {
                        continue;
                    }
                    // zsh's test here is `*p || !mmarked(*p)`.
                    let e = tab(p);
                    if !(e.null() && e.marked) {
                        k -= 1;
                        lp = Some(p);
                        ll = mline;
                    }
                }
                if mv.is_none() {
                    p = lp.unwrap_or(p);
                    mline = ll;
                }
            }
            set(&MLINE, mline);
        } else if c == "beginning-of-history" {
            mode = 0;
            mv = Some(Mv::Top);
        } else if c == "end-of-history" {
            mode = 0;
            mv = Some(Mv::Bottom);
        } else if c == "forward-char" || c == "vi-forward-char" {
            mode = 0;
            wrap = 0;
            mv = Some(Mv::Right);
        } else if c == "backward-char" || c == "vi-backward-char" {
            mode = 0;
            wrap = 0;
            mv = Some(Mv::Left);
        } else if [
            "beginning-of-buffer-or-history",
            "beginning-of-line",
            "beginning-of-line-hist",
            "vi-beginning-of-line",
        ]
        .contains(&c)
        {
            mode = 0;
            p -= get(&MCOL) as isize;
            let mut mcol = 0;
            while !tab(p).live() && mcol < mcols {
                mcol += 1;
                p += 1;
            }
            set(&MCOL, mcol);
            wishcol = 0;
        } else if [
            "end-of-buffer-or-history",
            "end-of-line",
            "end-of-line-hist",
            "vi-end-of-line",
        ]
        .contains(&c)
        {
            mode = 0;
            p += (mcols - get(&MCOL) - 1) as isize;
            let mut mcol = mcols - 1;
            while !tab(p).live() && mcol > 0 {
                mcol -= 1;
                p -= 1;
            }
            set(&MCOL, mcol);
            wishcol = mcols - 1;
        } else if c == "vi-forward-blank-word"
            || c == "vi-forward-blank-word-end"
            || c == "vi-backward-blank-word"
        {
            let fwd = c != "vi-backward-blank-word";
            let g = tab(p).gm.map(|x| x.0);
            let ol = get(&MLINE);

            mode = 0;
            loop {
                let mline = get(&MLINE);
                if fwd {
                    if mline == mlines - 1 {
                        p -= (mline * mcols) as isize;
                        set(&MLINE, 0);
                    } else {
                        set(&MLINE, mline + 1);
                        p += mcols as isize;
                    }
                } else if mline == 0 {
                    set(&MLINE, mlines - 1);
                    p += ((mlines - 1) * mcols) as isize;
                } else {
                    set(&MLINE, mline - 1);
                    p -= mcols as isize;
                }
                adjust_mcol(wishcol, &mut p);
                let e = tab(p);
                let same = e.marked || e.gm.is_none() || e.gm.map(|x| x.0) == g;
                let again = ol != get(&MLINE) && same;
                if !again {
                    break;
                }
            }
        } else if COMPLETE_WIDGETS.contains(&c) {
            if mode == MM_INTER {
                // Menu completion put the match in the line; interactive
                // mode shows only what was typed.
                z.comp.origline = Some(modeline.iter().collect());
                z.comp.origcs = modecs;
                z.comp.origll = modell;
                let n = (modell.max(0) as usize).min(modeline.len());
                let ml = modeline[..n].to_vec();
                setzline(z, &ml, modecs);
                z.comp.minfo.len = modelen;
                super::tricky::set_we(super::tricky::wb() + modelen);
            } else {
                mode = 0;
                z.comp.comprecursive = 1;
                do_menucmp(z, 0);
                set(&MSELECT, cur_match(z).as_ref().map_or(-1, gnum_of));
                setwish = true;
                set(&MLINE, -1);
            }
            continue 'sel;
        } else if c == "reverse-menu-complete" {
            mode = 0;
            z.comp.comprecursive = 1;
            z.zmod.mult = -z.zmod.mult;
            do_menucmp(z, 0);
            set(&MSELECT, cur_match(z).as_ref().map_or(-1, gnum_of));
            setwish = true;
            set(&MLINE, -1);
            continue 'sel;
        } else if c == "history-incremental-search-forward"
            || c == "history-incremental-search-backward"
            || ((mode == MM_FSEARCH || mode == MM_BSEARCH)
                && (c == "self-insert" || c == "self-insert-unmeta" || c == "bracketed-paste"))
        {
            let op = p;
            let was = mode == MM_FSEARCH || mode == MM_BSEARCH;
            let ins = c == "self-insert" || c == "self-insert-unmeta" || c == "bracketed-paste";
            let back = c == "history-incremental-search-backward";

            loop {
                let mut toins: Option<String> = None;
                if was {
                    p += (wishcol - get(&MCOL)) as isize;
                    set(&MCOL, wishcol);
                }
                if !ins {
                    if was {
                        let last = LASTSEARCH.with(|l| l.borrow().clone());
                        if ms.str.is_empty() && back == (mode == MM_BSEARCH) {
                            if let Some(ls) = last {
                                ms.str = ls;
                                mode = 0;
                            }
                        }
                    } else {
                        ms.str.clear();
                        ms.stack.clear();
                        ms.state = MS_OK;
                    }
                } else {
                    if c == "self-insert-unmeta" {
                        crate::zle::widgets::misc::fixunmeta(z);
                    }
                    if c == "bracketed-paste" {
                        toins = Some(crate::zle::widgets::misc::bracketedstring(z));
                    } else if z.lastchar_wide_valid {
                        toins = Some(z.lastchar_wide.map(String::from).unwrap_or_default());
                    } else {
                        toins = Some(char::from(z.lastchar as u8).to_string());
                    }
                }
                let mut wrap = 0;
                let np = msearch(
                    z,
                    &mut ms,
                    p,
                    toins.as_deref(),
                    if ins { mode == MM_BSEARCH } else { back },
                    was && !ins,
                    &mut wrap,
                );

                if !ins {
                    mode = if back { MM_BSEARCH } else { MM_FSEARCH };
                }
                if !ms.str.is_empty() {
                    LASTSEARCH.with(|l| *l.borrow_mut() = Some(ms.str.clone()));
                }
                if let Some(np) = np {
                    wishcol = get(&MCOL);
                    p = np;
                }
                adjust_mcol(wishcol, &mut p);

                let again = (back || c == "history-incremental-search-forward")
                    && np.is_some()
                    && wrap == 0
                    && was
                    && tab(p).gm == tab(op).gm;
                if !again {
                    break;
                }
            }
        } else if (mode == MM_FSEARCH || mode == MM_BSEARCH) && c == "backward-delete-char" {
            let mut back = true;
            let np = msearchpop(&mut ms, &mut back);

            mode = if back { MM_BSEARCH } else { MM_FSEARCH };
            wishcol = get(&MCOL);
            if let Some(np) = np {
                p = np;
                adjust_mcol(wishcol, &mut p);
            }
        } else if c == "undefined-key" {
            mode = 0;
            continue 'sel;
        } else {
            z.ungetkeycmd();
            if crate::zle::thingy::widget_flags(c) & WIDGET_NCOMP != 0 {
                acc = 0;
                broken = 2;
            } else {
                acc = 1;
            }
            break 'sel;
        }

        // The moves that run on into each other (zsh's gotos).
        while let Some(m) = mv.take() {
            match m {
                Mv::Down => {
                    let omline = get(&MLINE);
                    let op = p;
                    loop {
                        let mline = get(&MLINE);
                        if mline == mlines - 1 {
                            if wrap & 2 != 0 {
                                set(&MLINE, omline);
                                p = op;
                                break;
                            }
                            p -= (mline * mcols) as isize;
                            set(&MLINE, 0);
                            wrap |= 1;
                        } else {
                            set(&MLINE, mline + 1);
                            p += mcols as isize;
                        }
                        adjust_mcol(wishcol, &mut p);
                        if tab(p).live() {
                            break;
                        }
                    }
                    if wrap == 1 {
                        mv = Some(Mv::Right);
                    }
                }
                Mv::Up => {
                    let omline = get(&MLINE);
                    let op = p;
                    loop {
                        let mline = get(&MLINE);
                        if mline == 0 {
                            if wrap & 2 != 0 {
                                set(&MLINE, omline);
                                p = op;
                                break;
                            }
                            set(&MLINE, mlines - 1);
                            p += ((mlines - 1) * mcols) as isize;
                            wrap |= 1;
                        } else {
                            set(&MLINE, mline - 1);
                            p -= mcols as isize;
                        }
                        adjust_mcol(wishcol, &mut p);
                        if tab(p).live() {
                            break;
                        }
                    }
                    if wrap == 1 {
                        if get(&MCOL) == wishcol {
                            mv = Some(Mv::Left);
                        } else {
                            wishcol = get(&MCOL);
                        }
                    }
                }
                Mv::Top => {
                    let mut mline = get(&MLINE);
                    let mut ll = mline;
                    let mut lp = p;
                    while mline != 0 {
                        mline -= 1;
                        p -= mcols as isize;
                        if adjust_mcol(wishcol, &mut p) {
                            continue;
                        }
                        if tab(p).live() {
                            lp = p;
                            ll = mline;
                        }
                    }
                    set(&MLINE, ll);
                    p = lp;
                }
                Mv::Bottom => {
                    let mut mline = get(&MLINE);
                    let mut ll = mline;
                    let mut lp = p;
                    while mline < mlines - 1 {
                        mline += 1;
                        p += mcols as isize;
                        if adjust_mcol(wishcol, &mut p) {
                            continue;
                        }
                        if tab(p).live() {
                            lp = p;
                            ll = mline;
                        }
                    }
                    set(&MLINE, ll);
                    p = lp;
                }
                Mv::Right => {
                    let omcol = get(&MCOL);
                    let op = p;
                    loop {
                        let mcol = get(&MCOL);
                        if mcol == mcols - 1 {
                            if wrap & 1 != 0 {
                                p = op;
                                set(&MCOL, omcol);
                                break;
                            }
                            p -= mcol as isize;
                            set(&MCOL, 0);
                            wrap |= 2;
                        } else {
                            set(&MCOL, mcol + 1);
                            p += 1;
                        }
                        let e = tab(p);
                        let again = !e.live() || (get(&MCOL) != omcol && e == tab(op));
                        if !again {
                            break;
                        }
                    }
                    wishcol = get(&MCOL);
                    if wrap == 2 {
                        mv = Some(Mv::Down);
                    }
                }
                Mv::Left => {
                    let omcol = get(&MCOL);
                    let op = p;
                    loop {
                        let mcol = get(&MCOL);
                        if mcol == 0 {
                            if wrap & 1 != 0 {
                                p = op;
                                set(&MCOL, omcol);
                                break;
                            }
                            set(&MCOL, mcols - 1);
                            p += (mcols - 1) as isize;
                            wrap |= 2;
                        } else {
                            set(&MCOL, mcol - 1);
                            p -= 1;
                        }
                        let e = tab(p);
                        let again = !e.live() || (get(&MCOL) != omcol && e == tab(op));
                        if !again {
                            break;
                        }
                    }
                    wishcol = get(&MCOL);
                    if wrap == 2 {
                        p += (mcols - 1 - get(&MCOL)) as isize;
                        wishcol = mcols - 1;
                        set(&MCOL, mcols - 1);
                        adjust_mcol(wishcol, &mut p);
                        mv = Some(Mv::Up);
                    }
                }
            }
        }

        let target = if use_cur {
            cur_match(z)
        } else {
            tab_match(z, tab(p))
        };
        if was_inter {
            z.comp.minfo.cur = None;
        }
        if let Some(m) = target {
            do_single(z, &m);
            set(&MSELECT, m.borrow().gnum);
        }
    }
    z.selectlocalmap(None);
    set(&MSELECT, -1);
    set(&MLASTCOLS, -1);
    set(&MLASTLINES, -1);
    MSTATUS.with(|m| *m.borrow_mut() = None);
    set(&INSELECT, 0);
    set(&MHASSTAT, 0);
    if nolist {
        z.clearlist = true;
        set_listshown(z, 1);
    }
    if acc != 0 && z.comp.validlist != 0 {
        if let Some(m) = cur_match(z) {
            z.comp.menucmp = 0;
            z.comp.lastambig = 0;
            z.comp.hasoldlist = 0;
            if mode == MM_INTER {
                z.comp.minfo.cur = None;
            }
            do_single(z, &m);
        }
    }
    if wasnext || broken != 0 {
        z.comp.menucmp = 1;
        z.showinglist = if z.comp.validlist != 0 && !nolist {
            -2
        } else {
            0
        };
        z.comp.minfo.asked = 0;
        if get(&NOSELECT) == 0 {
            let nos = get(&NOSELECT);
            z.zrefresh();
            set(&NOSELECT, nos);
        }
    }
    if get(&NOSELECT) == 0 && (dat.is_none() || acc != 0) {
        // Forcing a full redraw here keeps the list from being garbled
        // (zsh-workers/24756).
        set(&MLBEG, -1);
        z.showinglist = if z.comp.validlist != 0 && !nolist {
            -2
        } else {
            0
        };
        z.comp.onlyexpl = oe;
        if acc != 0 && listshown() != 0 {
            // Clear the list without redrawing it first.
            z.clearlist = true;
            set_listshown(z, 1);
            z.showinglist = 1;
        } else if z.comp.smatches == 0 {
            z.clearlist = true;
            set_listshown(z, 1);
        }
        z.zrefresh();
    }
    set(&MLBEG, -1);
    FDAT.with(|f| f.set(false));

    if broken == 2 {
        3
    } else if dat.is_some() && broken == 0 {
        if acc != 0 {
            1
        } else {
            2
        }
    } else {
        i32::from(get(&NOSELECT) == 0) ^ acc
    }
}

/// The menu-select widget.
fn menuselect(z: &mut Zle, args: &[String]) -> i32 {
    let mut d = false;

    if z.comp.minfo.cur.is_none() {
        set(&SELECTED, 0);
        super::tricky::menucomplete(z, args);
        if (z.comp.minfo.cur.is_some() && z.comp.minfo.asked == 2) || get(&SELECTED) != 0 {
            return 0;
        }
        d = true;
    }
    if z.comp.minfo.cur.is_some() && (z.comp.minfo.asked == 2 || domenuselect(z, None) != 0) && !d {
        super::tricky::menucomplete(z, args);
    }
    0
}

/// Makes the menuselect and listscroll keymaps unless they exist (the
/// user may have deleted them).
fn menuselect_bindings() {
    let ms = open_keymap("menuselect").unwrap_or_else(|| {
        let km = Keymaps::new_keymap(None);
        with_keymaps(|t| t.link(km.clone(), "menuselect", true));
        {
            let mut k = km.borrow_mut();
            k.bind_widget(b"\t", "complete-word");
            k.bind_widget(b"\n", "accept-line");
            k.bind_widget(b"\r", "accept-line");
            k.bind_widget(b"\x1b[A", "up-line-or-history");
            k.bind_widget(b"\x1b[B", "down-line-or-history");
            k.bind_widget(b"\x1b[C", "forward-char");
            k.bind_widget(b"\x1b[D", "backward-char");
            k.bind_widget(b"\x1bOA", "up-line-or-history");
            k.bind_widget(b"\x1bOB", "down-line-or-history");
            k.bind_widget(b"\x1bOC", "forward-char");
            k.bind_widget(b"\x1bOD", "backward-char");
        }
        km
    });
    MSKEYMAP.with(|k| *k.borrow_mut() = Some(ms));
    let ls = open_keymap("listscroll").unwrap_or_else(|| {
        let km = Keymaps::new_keymap(None);
        with_keymaps(|t| t.link(km.clone(), "listscroll", true));
        {
            let mut k = km.borrow_mut();
            k.bind_widget(b"\t", "complete-word");
            k.bind_widget(b" ", "complete-word");
            k.bind_widget(b"\n", "accept-line");
            k.bind_widget(b"\r", "accept-line");
            k.bind_widget(b"\x1b[B", "down-line-or-history");
            k.bind_widget(b"\x1bOB", "down-line-or-history");
        }
        km
    });
    LSKEYMAP.with(|k| *k.borrow_mut() = Some(ls));
}

/// Loads zsh/complist: the menu-select widget, the listing and menu
/// start hooks, and the menuselect and listscroll keymaps.
pub fn setup() {
    MTAB.with(|t| t.borrow_mut().clear());
    set(&MSELECT, -1);
    set(&INSELECT, 0);

    crate::zle::thingy::add_internal(
        "menu-select",
        menuselect,
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_ISCOMP,
    );
    set_complistmatches_hook(Some(complistmatches));
    super::core::set_menu_start_hook(Some(menu_start));
    menuselect_bindings();
    LOADED.with(|l| l.set(true));
}

/// Whether zsh/complist is loaded.
pub fn is_loaded() -> bool {
    LOADED.with(Cell::get)
}
