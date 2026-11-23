//! Drawing the line, after zsh's zle_refresh.c.
//!
//! As in zsh, the display is kept as old and new video buffers, one
//! array of character cells per screen line; a refresh fills the new one
//! and sends the terminal only what differs, with the cheapest motions
//! the terminal description allows. Capabilities go through tcout() so a
//! `zle -T tc` function sees them; text attributes do not, as in zsh.
//!
//! Output to the terminal is buffered, as zsh's shout is, and written at
//! the points zsh flushes it. Output from widgets (print and so on) goes
//! straight to the terminal, so it can appear before display updates
//! queued earlier, exactly as with zsh.

#[path = "refresh_attr.rs"]
pub mod refresh_attr;
#[path = "refresh_term.rs"]
pub mod refresh_term;

use super::*;
use crate::zle::comp::result::{self, zterm_columns, zterm_lines};
use refresh_attr::*;
use refresh_term::*;
use std::cell::{Cell, RefCell};

pub use refresh_attr::set_memo_term_color;

thread_local! {
    // Set while zrefresh draws a completion list, which refreshes again.
    static INLIST: Cell<bool> = const { Cell::new(false) };
    static TTYBUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static TTYFD: Cell<i32> = const { Cell::new(1) };
}

/// Queues `s` for the terminal.
pub fn write_tty(s: &str) {
    write_bytes(s.as_bytes());
}

fn write_bytes(b: &[u8]) {
    TTYBUF.with(|t| t.borrow_mut().extend_from_slice(b));
}

/// Writes queued terminal output.
pub fn flush_tty() {
    let data = TTYBUF.with(|b| std::mem::take(&mut *b.borrow_mut()));
    if !data.is_empty() {
        let fd = TTYFD.with(|f| f.get());
        let _ = crate::sys::write_fd(fd, &data);
    }
}

pub fn set_tty_fd(fd: i32) {
    TTYFD.with(|f| f.set(fd));
}

/// The terminal's descriptor (zsh's SHTTY).
pub fn tty_fd() -> i32 {
    TTYFD.with(|f| f.get())
}

// ---- video cells ---------------------------------------------------------

// The second and later columns of a wide character.
const WEOF: u32 = u32::MAX;

/// One screen cell (zsh's REFRESH_ELEMENT). A character with combining
/// characters keeps the whole glyph in `mw`, where zsh indexes a
/// separate multiword buffer; `chr` is then its base character.
#[derive(Clone, PartialEq, Eq, Debug)]
struct RChar {
    chr: u32,
    atr: Zattr,
    mw: Option<Box<[char]>>,
}

const fn rc(chr: u32, atr: Zattr) -> RChar {
    RChar { chr, atr, mw: None }
}

const ZR_CR: RChar = rc('\r' as u32, TXT_ERROR);
const ZR_DT: RChar = rc('.' as u32, TXT_ERROR);
const ZR_NL: RChar = rc('\n' as u32, TXT_ERROR);
const ZR_SP: RChar = rc(' ' as u32, 0);
const ZR_ZR: RChar = rc(0, 0);

fn end_ellipsis() -> [RChar; 7] {
    [
        rc(' ' as u32, 0),
        rc('<' as u32, 0),
        ZR_DT,
        ZR_DT,
        ZR_DT,
        ZR_DT,
        rc(' ' as u32, 0),
    ]
}

fn mid_ellipsis1() -> [RChar; 6] {
    [
        rc(' ' as u32, 0),
        rc('<' as u32, 0),
        ZR_DT,
        ZR_DT,
        ZR_DT,
        ZR_DT,
    ]
}

fn mid_ellipsis2() -> [RChar; 2] {
    [rc('>' as u32, TXT_ERROR), rc(' ' as u32, 0)]
}

fn start_ellipsis() -> [RChar; 5] {
    [rc('>' as u32, 0), ZR_DT, ZR_DT, ZR_DT, ZR_DT]
}

fn zr_strlen(l: &[RChar]) -> usize {
    l.iter().position(|c| c.chr == 0).unwrap_or(l.len())
}

fn at(l: &[RChar], i: usize) -> RChar {
    l.get(i).cloned().unwrap_or(ZR_ZR)
}

// ZR_strncmp: whether the lines differ within `len` cells.
fn zr_strncmp(old: &[RChar], new: &[RChar], len: usize) -> bool {
    for k in 0..len {
        let (o, n) = (at(old, k), at(new, k));
        if o.chr == 0 || n.chr == 0 {
            return o != n;
        }
        if o != n {
            return true;
        }
    }
    false
}

/// zwcputc(): sets the cell's attributes, then writes its character.
fn zputc(c: &RChar) {
    treplaceattrs(c.atr);
    applytextattributes();
    let mut buf = [0u8; 4];
    if let Some(mw) = &c.mw {
        for ch in mw.iter() {
            write_bytes(ch.encode_utf8(&mut buf).as_bytes());
        }
    } else if c.chr != WEOF {
        if let Some(ch) = char::from_u32(c.chr) {
            write_bytes(ch.encode_utf8(&mut buf).as_bytes());
        }
    }
}

fn zwrite(cells: &[RChar]) {
    for c in cells {
        zputc(c);
    }
}

// ---- character classes -----------------------------------------------

fn iswcntrl(c: char) -> bool {
    let u = c as u32;
    u < 0x20 || (0x7f..=0x9f).contains(&u) || u == 0x2028 || u == 0x2029
}

fn iswprint(c: char) -> bool {
    !iswcntrl(c)
}

fn wcwidth(c: char) -> i32 {
    if iswcntrl(c) {
        -1
    } else {
        let mut b = [0u8; 4];
        crate::display_width(c.encode_utf8(&mut b)) as i32
    }
}

fn is_combining(c: char) -> bool {
    c != '\0' && wcwidth(c) == 0
}

fn is_basechar(c: char) -> bool {
    iswprint(c) && !c.is_whitespace() && wcwidth(c) > 0
}

// The `<xxxx>` form of a character with no printable form.
fn hexform(c: char) -> String {
    if (c as u32) > 0xffff {
        format!("<{:08x}>", c as u32)
    } else {
        format!("<{:04x}>", c as u32)
    }
}

// The second cell of `^X` for control character `c`.
fn ctrl_second(c: char) -> u32 {
    if (c as u32 & !0x80) > 31 {
        '?' as u32
    } else {
        c as u32 | '@' as u32
    }
}

// ---- prompts -------------------------------------------------------------

/// countprompt(): the width of the last line of a prompt and its height.
/// Escape sequences take no room, as text in %{...%} does in zsh.
fn countprompt(s: &str, overf: i32) -> (i32, i32) {
    let cols = zterm_columns();
    let (mut w, mut h, mut wcw) = (0i32, 1i32, 0i32);
    let overflow = |w: &mut i32, h: &mut i32, wcw: i32| {
        while *w > cols && overf >= 0 {
            *h += 1;
            if wcw != 0 {
                *w = wcw;
                break;
            }
            *w -= cols;
        }
    };
    for c in crate::strip_ansi(s).chars() {
        overflow(&mut w, &mut h, wcw);
        wcw = 0;
        if c == '\t' {
            w = (w | 7) + 1;
        } else if c == '\n' {
            w = 0;
            h += 1;
        } else if !iswcntrl(c) {
            wcw = wcwidth(c);
            w += wcw;
        }
    }
    overflow(&mut w, &mut h, wcw);
    if w == cols && overf == 0 {
        w = 0;
        h += 1;
    }
    (w, h)
}

fn rprompt_indent() -> i32 {
    match crate::shell::var_get_opt("ZLE_RPROMPT_INDENT") {
        Some(v) => zstrtol(v.as_bytes(), 0, 10).0 as i32,
        None => 1,
    }
}

// ---- highlighting --------------------------------------------------------

/// An element of zsh's region_highlights: the first four are the region,
/// the isearch match, the completion suffix and pasted text.
#[derive(Clone, Default)]
struct Rh {
    start: i64,
    end: i64,
    atr: Zattr,
    mask: Zattr,
    layer: i32,
    predisplay: bool,
}

const N_SPECIAL_HIGHLIGHTS: usize = 4;

#[derive(Default)]
struct Hl {
    default_attr: Zattr,
    default_mask: Zattr,
    special_attr: Zattr,
    special_mask: Zattr,
    ellipsis_attr: Zattr,
    special_layer: i32,
    rhs: Vec<Rh>,
}

impl Hl {
    /// The attributes of the character at `pos` (in the line with
    /// PREDISPLAY), as base and as shown when special, layer by layer.
    fn attrs_at(&self, pos: i64, prelen: i64, prompt_attr: Zattr) -> (Zattr, Zattr) {
        let mut base_attr = mixattrs(self.default_attr, self.default_mask, prompt_attr);
        let mut all_attr: Zattr = 0;
        let mut nextlayer = 0;
        loop {
            let layer = nextlayer;
            nextlayer = self.special_layer;
            for rh in &self.rhs {
                if rh.layer == layer {
                    let offset = if rh.predisplay { 0 } else { prelen };
                    if rh.start + offset <= pos && pos < rh.end + offset {
                        base_attr = mixattrs(rh.atr, rh.mask, base_attr);
                        if layer > self.special_layer {
                            all_attr = mixattrs(rh.atr, rh.mask, all_attr);
                        }
                    }
                } else if rh.layer > layer && (rh.layer < nextlayer || nextlayer <= layer) {
                    nextlayer = rh.layer;
                }
            }
            if self.special_layer == layer {
                all_attr = mixattrs(self.special_attr, self.special_mask, base_attr);
            }
            if nextlayer <= layer {
                break;
            }
        }
        (base_attr, all_attr)
    }
}

// ---- the display state ---------------------------------------------------

/// What zle_refresh.c keeps in statics: the video buffers, the cursor's
/// screen position and the prompt measurements.
#[derive(Default)]
pub struct RefreshState {
    /// Rows the last display took.
    pub nlnct: usize,
    /// Whether a completion list or message is shown below the line.
    pub listshown: bool,
    nbuf: Vec<Option<Vec<RChar>>>,
    obuf: Vec<Option<Vec<RChar>>>,
    more_start: bool,
    more_end: bool,
    olnct: i32,
    lpromptw: i32,
    rpromptw: i32,
    lpromptwof: i32,
    lprompth: i32,
    rprompth: i32,
    vcs: i32,
    vln: i32,
    vmaxln: i32,
    winw: i32,
    winh: i32,
    rwinh: i32,
    winpos: i32,
    winprompt: i32,
    cleareol: bool,
    clearf: bool,
    put_rpmpt: bool,
    oput_rpmpt: bool,
    numscrolls: i32,
    onumscrolls: i32,
    lpromptbuf: String,
    rpromptbuf: String,
    pmpt_attr: Zattr,
    rpmpt_attr: Zattr,
    prompt_attr: Zattr,
    hl: Hl,
}

// zrefresh's struct rparams.
struct Rparams {
    canscroll: i32,
    ln: usize,
    more_status: bool,
    nvcs: i32,
    nvln: i32,
    tosln: i32,
    s: usize,
    sen: usize,
}

impl RefreshState {
    fn pad(&self) -> RChar {
        rc(' ' as u32, self.prompt_attr)
    }

    fn nline(&mut self, ln: usize) -> &mut Vec<RChar> {
        let w = self.winw.max(0) as usize + 2;
        if self.nbuf.len() <= ln {
            self.nbuf.resize(ln + 1, None);
        }
        self.nbuf[ln].get_or_insert_with(|| vec![ZR_ZR; w])
    }

    fn set(&mut self, ln: usize, i: usize, c: RChar) {
        let l = self.nline(ln);
        if i < l.len() {
            l[i] = c;
        }
    }

    fn nchr(&self, ln: i32, i: usize) -> RChar {
        if ln < 0 {
            return ZR_ZR;
        }
        match self.nbuf.get(ln as usize) {
            Some(Some(l)) => at(l, i),
            _ => ZR_ZR,
        }
    }

    fn ochr(&self, ln: i32, i: usize) -> RChar {
        if ln < 0 {
            return ZR_ZR;
        }
        match self.obuf.get(ln as usize) {
            Some(Some(l)) => at(l, i),
            _ => ZR_ZR,
        }
    }

    fn has_n(&self, ln: usize) -> bool {
        matches!(self.nbuf.get(ln), Some(Some(_)))
    }

    fn has_o(&self, ln: usize) -> bool {
        matches!(self.obuf.get(ln), Some(Some(_)))
    }

    /// tcoutclear(): a clearing capability, with the attributes the
    /// cleared area should get.
    fn tcoutclear(&self, cap: usize) {
        treplaceattrs(if cap == TCCLEAREOL {
            self.prompt_attr
        } else {
            0
        });
        applytextattributes();
        tcout(cap);
    }

    fn scrollwindow(&mut self, tline: usize) {
        let winh = (self.winh.max(1) as usize).min(self.nbuf.len());
        if tline < winh {
            self.nbuf[tline..winh].rotate_left(1);
        }
        if tline == 0 {
            self.more_start = true;
        }
    }

    /// nextline(): on to the next line of the main area. True when the
    /// line loop should stop.
    fn nextline(&mut self, rp: &mut Rparams, wrapped: bool) -> bool {
        let w = self.winw as usize;
        self.set(rp.ln, w + 1, if wrapped { ZR_NL } else { ZR_ZR });
        self.set(rp.ln, rp.s, ZR_ZR);
        if rp.ln as i32 != self.winh - 1 {
            rp.ln += 1;
        } else {
            if rp.canscroll == 0 {
                if rp.nvln != -1
                    && rp.nvln != self.winh - 1
                    && (self.numscrolls != self.onumscrolls - 1 || rp.nvln <= self.winh / 2)
                {
                    return true;
                }
                self.numscrolls += 1;
                rp.canscroll = self.winh / 2;
            }
            rp.canscroll -= 1;
            self.scrollwindow(0);
            if rp.nvln != -1 {
                rp.nvln -= 1;
            }
        }
        self.nline(rp.ln);
        rp.s = 0;
        rp.sen = w;
        false
    }

    /// snextline(): on to the next line of the status area.
    fn snextline(&mut self, rp: &mut Rparams) {
        self.set(rp.ln, rp.s, ZR_ZR);
        if rp.ln as i32 != self.winh - 1 {
            rp.ln += 1;
        } else if rp.tosln > rp.ln as i32 {
            rp.tosln -= 1;
            if rp.nvln > 1 {
                self.scrollwindow(0);
                rp.nvln -= 1;
            } else {
                self.more_end = true;
            }
        } else if rp.tosln > 2 && rp.nvln > 1 {
            rp.tosln -= 1;
            if rp.tosln <= rp.nvln {
                self.scrollwindow(0);
                rp.nvln -= 1;
            } else {
                self.scrollwindow(rp.tosln as usize);
                self.more_end = true;
            }
        } else {
            rp.more_status = true;
            self.scrollwindow((rp.tosln + 1) as usize);
        }
        self.nline(rp.ln);
        rp.s = 0;
        rp.sen = self.winw as usize;
    }

    fn bufswap(&mut self) {
        std::mem::swap(&mut self.nbuf, &mut self.obuf);
    }

    /// The main display: fills the new buffer from `tmpline`, updates
    /// the screen line by line and moves to the cursor.
    fn draw(
        &mut self,
        tmpline: &[char],
        tmpcs: usize,
        prelen: usize,
        statusline: Option<&str>,
        transient: bool,
    ) {
        let winw = self.winw as usize;
        let tmpll = tmpline.len();
        let combining = crate::options::is_set("combiningchars");
        self.numscrolls = 0;
        self.nline(0);

        let mut rp = Rparams {
            canscroll: 0,
            ln: 0,
            more_status: false,
            nvcs: 0,
            nvln: -1,
            tosln: 0,
            s: self.lpromptw as usize,
            sen: winw,
        };
        let mut tmppos = 0usize;
        while tmppos < tmpll {
            let (base_attr, all_attr) =
                self.hl
                    .attrs_at(tmppos as i64, prelen as i64, self.prompt_attr);
            if tmppos == tmpcs {
                rp.nvln = rp.ln as i32;
                rp.nvcs = rp.s as i32;
            }
            let c = tmpline[tmppos];
            let width = wcwidth(c);
            if c == '\n' {
                if self.nextline(&mut rp, false) {
                    break;
                }
            } else if c == '\t' {
                let mut t0 = rp.s;
                if (t0 | 7) + 1 >= winw {
                    if self.nextline(&mut rp, true) {
                        break;
                    }
                } else {
                    loop {
                        self.set(rp.ln, rp.s, rc(' ' as u32, base_attr));
                        rp.s += 1;
                        t0 += 1;
                        if t0 & 7 == 0 {
                            break;
                        }
                    }
                }
            } else if iswprint(c) && width > 0 {
                if width as usize > rp.sen.saturating_sub(rp.s) {
                    // Too wide to fit: pad to the end of the line.
                    loop {
                        self.set(rp.ln, rp.s, rc(' ' as u32, all_attr));
                        rp.s += 1;
                        if rp.s >= rp.sen {
                            break;
                        }
                    }
                    if self.nextline(&mut rp, true) {
                        break;
                    }
                    if tmppos == tmpcs {
                        rp.nvln = rp.ln as i32;
                        rp.nvcs = rp.s as i32;
                    }
                }
                let mut ichars = 1;
                if combining && is_basechar(c) {
                    while tmppos + ichars < tmpll && is_combining(tmpline[tmppos + ichars]) {
                        ichars += 1;
                    }
                }
                if width as usize > rp.sen - rp.s {
                    self.set(rp.ln, rp.s, rc('?' as u32, all_attr));
                    rp.s += 1;
                } else {
                    let cell = if ichars > 1 {
                        RChar {
                            chr: c as u32,
                            atr: base_attr | TXT_MULTIWORD_MASK,
                            mw: Some(tmpline[tmppos..tmppos + ichars].into()),
                        }
                    } else {
                        rc(c as u32, base_attr)
                    };
                    self.set(rp.ln, rp.s, cell);
                    rp.s += 1;
                    for _ in 1..width {
                        self.set(rp.ln, rp.s, rc(WEOF, base_attr));
                        rp.s += 1;
                    }
                }
                tmppos += ichars - 1;
            } else if iswcntrl(c) && (c as u32) <= 0xff {
                self.set(rp.ln, rp.s, rc('^' as u32, all_attr));
                rp.s += 1;
                if rp.s == rp.sen && self.nextline(&mut rp, true) {
                    break;
                }
                self.set(rp.ln, rp.s, rc(ctrl_second(c), all_attr));
                rp.s += 1;
            } else {
                let mut stopped = false;
                for d in hexform(c).chars() {
                    self.set(rp.ln, rp.s, rc(d as u32, all_attr));
                    rp.s += 1;
                    if rp.s == rp.sen && self.nextline(&mut rp, true) {
                        stopped = true;
                        break;
                    }
                }
                if stopped {
                    break;
                }
            }
            if rp.s == rp.sen && self.nextline(&mut rp, true) {
                break;
            }
            tmppos += 1;
        }

        // On the next line for real: do it properly.
        if tmppos == tmpcs {
            rp.nvln = rp.ln as i32;
            rp.nvcs = rp.s as i32;
            if rp.nvcs == self.winw {
                self.nextline(&mut rp, true);
                self.set(rp.ln, rp.s, ZR_ZR);
                rp.nvcs = 0;
                rp.nvln += 1;
            }
        }
        if tmppos != tmpll {
            self.more_end = true;
        }

        if let Some(st) = statusline {
            let all_attr = self.hl.special_attr;
            rp.tosln = rp.ln as i32 + 1;
            self.set(rp.ln, winw + 1, ZR_ZR);
            self.snextline(&mut rp);
            let u: Vec<char> = st.chars().collect();
            let mut k = 0;
            while k < u.len() {
                let c = u[k];
                if iswprint(c) {
                    let width = wcwidth(c);
                    if width > 0 {
                        let mut ichars = 1;
                        if combining && is_basechar(c) {
                            while k + ichars < u.len() && is_combining(u[k + ichars]) {
                                ichars += 1;
                            }
                        }
                        if width as usize > rp.sen - rp.s {
                            loop {
                                self.set(rp.ln, rp.s, ZR_SP);
                                rp.s += 1;
                                if rp.s >= rp.sen {
                                    break;
                                }
                            }
                            self.set(rp.ln, winw + 1, ZR_NL);
                            self.snextline(&mut rp);
                        }
                        if width as usize > rp.sen - rp.s {
                            self.set(rp.ln, rp.s, rc('?' as u32, all_attr));
                            rp.s += 1;
                        } else {
                            let cell = if ichars > 1 {
                                RChar {
                                    chr: c as u32,
                                    atr: TXT_MULTIWORD_MASK,
                                    mw: Some(u[k..k + ichars].into()),
                                }
                            } else {
                                rc(c as u32, 0)
                            };
                            self.set(rp.ln, rp.s, cell);
                            rp.s += 1;
                            for _ in 1..width {
                                self.set(rp.ln, rp.s, rc(WEOF, 0));
                                rp.s += 1;
                            }
                        }
                        k += ichars - 1;
                    } else {
                        self.status_hex(&mut rp, c, all_attr);
                    }
                } else if iswcntrl(c) {
                    self.set(rp.ln, rp.s, rc('^' as u32, all_attr));
                    rp.s += 1;
                    if rp.s == rp.sen {
                        self.set(rp.ln, winw + 1, ZR_NL);
                        self.snextline(&mut rp);
                    }
                    self.set(rp.ln, rp.s, rc(ctrl_second(c), all_attr));
                    rp.s += 1;
                } else {
                    self.status_hex(&mut rp, c, all_attr);
                }
                if rp.s == rp.sen {
                    self.set(rp.ln, winw + 1, ZR_NL);
                    self.snextline(&mut rp);
                }
                k += 1;
            }
            if rp.s == rp.sen {
                self.snextline(&mut rp);
            }
        }
        self.set(rp.ln, rp.s, ZR_ZR);

        // `<....` at the end of the last line when there is more text.
        if self.more_end && winw >= 7 {
            if statusline.is_none() {
                rp.tosln = self.winh;
            }
            let (pad, prompt_attr, ellipsis_attr) =
                (self.pad(), self.prompt_attr, self.hl.ellipsis_attr);
            let line = self.nline((rp.tosln - 1).max(0) as usize);
            let mut sen = winw - 7;
            if let Some(s) = line[..sen].iter().position(|c| c.chr == 0) {
                for cell in &mut line[s..sen] {
                    *cell = pad.clone();
                }
                line[sen].chr = 0;
            }
            let mut extra = 0;
            while sen > 0 && line[sen].chr == WEOF {
                extra += 1;
                sen -= 1;
            }
            line[sen..sen + 7].clone_from_slice(&end_ellipsis());
            line[sen].atr = prompt_attr;
            line[sen + 1].atr = ellipsis_attr;
            for cell in &mut line[sen + 7..sen + 7 + extra] {
                *cell = ZR_DT;
            }
            line[winw] = ZR_ZR;
            line[winw + 1] = ZR_ZR;
        }

        // `<....>` at the end of the first status line when it is too big.
        if rp.more_status && winw >= 8 {
            let (prompt_attr, ellipsis_attr) = (self.prompt_attr, self.hl.ellipsis_attr);
            let line = self.nline(rp.tosln.max(0) as usize);
            let mut sen = winw - 8;
            if let Some(s) = line[..sen].iter().position(|c| c.chr == 0) {
                for cell in &mut line[s..sen] {
                    *cell = ZR_SP;
                }
            }
            let mut extra = 0;
            while sen > 0 && line[sen].chr == WEOF {
                extra += 1;
                sen -= 1;
            }
            line[sen..sen + 6].clone_from_slice(&mid_ellipsis1());
            line[sen + 1].atr = ellipsis_attr;
            sen += 6;
            for cell in &mut line[sen..sen + extra] {
                *cell = ZR_DT;
            }
            sen += extra;
            line[sen..sen + 2].clone_from_slice(&mid_ellipsis2());
            line[sen + 1].atr = prompt_attr;
            line[winw] = ZR_ZR;
            line[winw + 1] = ZR_ZR;
        }

        self.nlnct = rp.ln + 1;
        let nlnct = self.nlnct as i32;
        for iln in self.nlnct..(self.winh.max(0) as usize).min(self.nbuf.len()) {
            self.nbuf[iln] = None;
        }

        // Whether the right prompt exists and fits.
        let mut rprompt_off = 1;
        if !self.more_start {
            if transient {
                self.put_rpmpt = false;
            } else {
                self.put_rpmpt = self.rprompth == 1
                    && !self.rpromptbuf.is_empty()
                    && !self.rpromptbuf.contains('\t');
                if self.put_rpmpt {
                    rprompt_off = rprompt_indent().max(0);
                    let l0 = self.nbuf[0].as_deref().map_or(0, zr_strlen) as i32;
                    self.put_rpmpt = l0 + self.rpromptw < self.winw - rprompt_off;
                }
            }
        } else {
            // `>....` on the first line when there is text before it.
            let lw = (self.lpromptw as usize).min(winw);
            let (pad, prompt_attr, ellipsis_attr) =
                (self.pad(), self.prompt_attr, self.hl.ellipsis_attr);
            let line = self.nline(0);
            for cell in &mut line[..lw] {
                *cell = ZR_SP;
            }
            let t0 = (winw - lw).min(5);
            if t0 > 0 {
                line[lw..lw + t0].clone_from_slice(&start_ellipsis()[..t0]);
                line[lw].atr = ellipsis_attr;
            }
            for cell in &mut line[lw + t0..winw] {
                *cell = pad.clone();
            }
            if lw + t0 < winw {
                line[lw + t0].atr = prompt_attr;
            }
            line[winw] = ZR_ZR;
            line[winw + 1] = ZR_ZR;
        }

        for iln in 0..self.nlnct {
            let i = iln as i32;
            if i >= self.olnct {
                self.cleareol = true;
            }
            // When a line changed, see whether inserting or deleting a
            // line makes the update cheaper.
            if !self.clearf
                && iln > 0
                && i < self.olnct - 1
                && !(hasam() && self.vcs == self.winw)
                && self.has_n(iln)
                && self.has_o(iln)
                && zr_strncmp(
                    self.obuf[iln].as_deref().unwrap_or(&[]),
                    self.nbuf[iln].as_deref().unwrap_or(&[]),
                    16,
                )
            {
                if tccan(TCDELLINE)
                    && self.ochr(i + 1, 0).chr != 0
                    && !zr_strncmp(
                        self.obuf[iln + 1].as_deref().unwrap_or(&[]),
                        self.nbuf[iln].as_deref().unwrap_or(&[]),
                        16,
                    )
                {
                    self.moveto(i, 0);
                    tcout(TCDELLINE);
                    let olnct = self.olnct as usize;
                    if olnct < self.obuf.len() {
                        self.obuf[iln..=olnct].rotate_left(1);
                        self.obuf[olnct] = None;
                    }
                    self.olnct -= 1;
                } else if tccan(TCINSLINE)
                    && self.olnct < self.vmaxln
                    && self.has_n(iln + 1)
                    && self.has_o(iln)
                    && !zr_strncmp(
                        self.obuf[iln].as_deref().unwrap_or(&[]),
                        self.nbuf[iln + 1].as_deref().unwrap_or(&[]),
                        16,
                    )
                {
                    self.moveto(i, 0);
                    tcout(TCINSLINE);
                    let olnct = self.olnct as usize;
                    if olnct < self.obuf.len() {
                        self.obuf[olnct] = None;
                        self.obuf[iln..=olnct].rotate_right(1);
                    }
                    self.obuf[iln] = None;
                    self.olnct += 1;
                }
            }

            self.refreshline(iln);

            if self.put_rpmpt && iln == 0 && !self.oput_rpmpt {
                self.moveto(0, self.winw - rprompt_off - self.rpromptw);
                treplaceattrs(self.pmpt_attr);
                applytextattributes();
                write_tty(&self.rpromptbuf);
                if rprompt_off != 0 {
                    self.vcs = self.winw - rprompt_off;
                } else {
                    zputc(&ZR_CR);
                    self.vcs = 0;
                }
                set_current(self.rpmpt_attr);
            }
        }

        // Lines the old display had beyond the new one are cleared.
        if self.olnct > nlnct {
            self.cleareol = true;
            for iln in self.nlnct..self.olnct as usize {
                self.refreshline(iln);
            }
        }

        self.clearf = false;
        self.oput_rpmpt = self.put_rpmpt;
        self.moveto(rp.nvln, rp.nvcs);
    }

    // The `<xxxx>` form of a character in the status line.
    fn status_hex(&mut self, rp: &mut Rparams, c: char, all_attr: Zattr) {
        let winw = self.winw as usize;
        for d in hexform(c).chars() {
            self.set(rp.ln, rp.s, rc(d as u32, all_attr));
            rp.s += 1;
            if rp.s == rp.sen {
                self.set(rp.ln, winw + 1, ZR_NL);
                self.snextline(rp);
            }
        }
    }

    // After the cursor is placed: the new buffer becomes the old one.
    fn finish(&mut self) {
        self.bufswap();
        self.olnct = self.nlnct as i32;
        self.onumscrolls = self.numscrolls;
        if self.nlnct as i32 > self.vmaxln {
            self.vmaxln = self.nlnct as i32;
        }
    }

    // obuf[ln][i], when the old line is in use.
    fn ol(&self, ln: usize, use_ol: bool, i: usize) -> RChar {
        if use_ol {
            self.ochr(ln as i32, i)
        } else {
            ZR_ZR
        }
    }

    /// refreshline(): updates one screen line, using whatever the
    /// terminal offers.
    fn refreshline(&mut self, ln: usize) {
        let winw = self.winw;
        let w = winw.max(0) as usize;
        let pad = self.pad();
        let hasam = hasam();
        let nlnct = self.nlnct as i32;
        let have_nl = self.has_n(ln);
        let mut nl: Vec<RChar> = self.nbuf.get(ln).cloned().flatten().unwrap_or_default();
        let rnllen = zr_strlen(&nl) as i32;
        let mut nllen = rnllen;
        let use_ol = (ln as i32) < self.olnct && self.has_o(ln);
        let ollen = if use_ol {
            self.obuf[ln].as_deref().map_or(0, zr_strlen) as i32
        } else {
            0
        };

        // Clearing an old line: easy when the terminal can.
        if self.cleareol && nllen == 0 && !(hasam && (ln as i32) < nlnct - 1) && tccan(TCCLEAREOL) {
            self.moveto(ln as i32, 0);
            self.tcoutclear(TCCLEAREOL);
            return;
        }

        // 1: pad the new line to hold everything that has to be written.
        if self.cleareol
            || (nllen == 0 && (ln != 0 || !self.put_rpmpt))
            || (ln == 0 && self.put_rpmpt != self.oput_rpmpt)
        {
            let n = (nllen as usize).min(w);
            let mut p1 = vec![ZR_ZR; w + 2];
            p1[..n].clone_from_slice(&nl[..n]);
            for cell in &mut p1[n..w] {
                *cell = ZR_SP;
            }
            p1[w + 1] = if n < w { ZR_ZR } else { at(&nl, w + 1) };
            if ln != 0 && have_nl {
                // So the old buffer is right next time.
                self.nbuf[ln] = Some(p1.clone());
            }
            nl = p1;
            nllen = winw;
        } else if ollen > nllen {
            let mut p1: Vec<RChar> = nl[..nllen as usize].to_vec();
            p1.extend(std::iter::repeat_n(pad.clone(), (ollen - nllen) as usize));
            p1.push(ZR_ZR);
            nl = p1;
            nllen = ollen;
        }

        // 2: where clearing to the end of the line can start, if at all.
        let mut col_cleareol: i32;
        if hasam && (ln as i32) < nlnct - 1 && rnllen == winw {
            // Clearing would break cut and paste of the wrapped line.
            col_cleareol = -2;
        } else {
            col_cleareol = -1;
            if tccan(TCCLEAREOL) && (nllen == winw || self.put_rpmpt != self.oput_rpmpt) {
                let a = at(&nl, (nllen - 1).max(0) as usize).atr;
                let mut i = nllen;
                while i > 0
                    && at(&nl, (i - 1) as usize).chr == ' ' as u32
                    && at(&nl, (i - 1) as usize).atr == a
                {
                    i -= 1;
                }
                if nllen == winw && i < nllen {
                    col_cleareol = i;
                } else {
                    let a = self.ol(ln, use_ol, (ollen - 1).max(0) as usize).atr;
                    let mut j = ollen;
                    while j > 0 {
                        let o = self.ol(ln, use_ol, (j - 1) as usize);
                        if o.chr != ' ' as u32 || o.atr != a {
                            break;
                        }
                        j -= 1;
                    }
                    if j > i + tclen(TCCLEAREOL) {
                        col_cleareol = i;
                    }
                }
            }
        }

        // 2b: with automargins, wrap by writing the next line's first cell.
        let mut ni = 0usize;
        let mut oi = 0usize;
        let mut ccs = 0i32;
        let mut char_ins = 0i32;
        if hasam && self.vcs == winw {
            if self.has_n(self.vln.max(0) as usize)
                && self.nchr(self.vln, (self.vcs + 1) as usize).chr == '\n' as u32
            {
                self.vln += 1;
                self.vcs = 1;
                let c0 = self.nchr(self.vln, 0);
                let sp = ZR_SP;
                zputc(if c0.chr != 0 { &c0 } else { &sp });
                if ln as i32 == self.vln {
                    ni += 1;
                    if self.ol(ln, use_ol, oi).chr != 0 {
                        oi += 1;
                    }
                    ccs = 1;
                }
            } else {
                self.vln += 1;
                self.vcs = 0;
                zputc(&ZR_NL);
            }
        }
        let mut ins_last = false;

        // 2c: on the first line, start after the prompt.
        if ln == 0 && self.lpromptw != 0 {
            let i = self.lpromptw - ccs;
            let mut j = 0;
            while self.ol(ln, use_ol, oi + j).chr != 0 {
                j += 1;
            }
            ni += i.max(0) as usize;
            oi += (i.max(0) as usize).min(j);
            ccs = self.lpromptw;
        }

        // Realign to a real character.
        while at(&nl, ni).chr == WEOF {
            ni += 1;
            ccs += 1;
            self.vcs += 1;
            if self.ol(ln, use_ol, oi).chr != 0 {
                oi += 1;
            }
        }

        // 3: write the line out, with whatever tricks we can.
        loop {
            let n0 = at(&nl, ni);
            let o0 = self.ol(ln, use_ol, oi);
            if n0.chr != WEOF && o0.chr != WEOF {
                if n0.chr != 0 && o0.chr != 0 && self.ol(ln, use_ol, oi + 1) == at(&nl, ni + 1) {
                    // Skip the matching characters.
                    let ccs_was = ccs;
                    while at(&nl, ni).chr != 0 && self.ol(ln, use_ol, oi) == at(&nl, ni) {
                        ni += 1;
                        oi += 1;
                        ccs += 1;
                    }
                    while (at(&nl, ni).chr == WEOF || self.ol(ln, use_ol, oi).chr == WEOF)
                        && ccs > ccs_was
                    {
                        ni -= 1;
                        oi -= 1;
                        ccs -= 1;
                    }
                }

                if at(&nl, ni).chr == 0 {
                    if ccs == winw && hasam && char_ins > 0 && ins_last && self.vcs != winw {
                        ni -= 1;
                        self.moveto(ln as i32, winw - 1);
                        zputc(&at(&nl, ni));
                        self.vcs += 1;
                        return;
                    }
                    if char_ins <= 0 || ccs >= winw {
                        return;
                    }
                    if tccan(TCCLEAREOL) && char_ins >= tclen(TCCLEAREOL) && col_cleareol != -2 {
                        // Junk is left on the right.
                        col_cleareol = 0;
                    }
                }

                self.moveto(ln as i32, ccs);

                if col_cleareol >= 0 && ccs >= col_cleareol {
                    self.tcoutclear(TCCLEAREOL);
                    return;
                }

                // Rubbish left by insertions.
                if at(&nl, ni).chr == 0 {
                    let mut i = (winw - ccs).min(char_ins);
                    if tccan(TCDEL) && tcdelcost(i) <= i + 1 {
                        tcmultout(TCDEL, TCMULTDEL, i);
                    } else {
                        self.vcs += i;
                        while i > 0 {
                            zputc(&pad);
                            i -= 1;
                        }
                    }
                    return;
                }

                // Past the end of the old line: write what is left.
                if self.ol(ln, use_ol, oi).chr == 0 {
                    let mut i = if col_cleareol >= 0 {
                        col_cleareol
                    } else {
                        nllen
                    };
                    i -= self.vcs;
                    if i < 0 {
                        i = 0;
                    }
                    let end = (ni + i as usize).min(nl.len());
                    zwrite(&nl[ni.min(end)..end]);
                    self.vcs += i;
                    if col_cleareol >= 0 {
                        self.tcoutclear(TCCLEAREOL);
                    }
                    return;
                }

                // Inserting and deleting characters, unless there is a
                // right prompt in the way.
                if (ln != 0 || !self.put_rpmpt || !self.oput_rpmpt)
                    && self.ol(ln, use_ol, oi).chr != WEOF
                    && at(&nl, ni).chr != WEOF
                    && at(&nl, ni + 1).chr != 0
                    && self.ol(ln, use_ol, oi + 1).chr != 0
                    && self.ol(ln, use_ol, oi + 1) != at(&nl, ni + 1)
                {
                    if tccan(TCDEL) {
                        let mut i = 1usize;
                        while self.ol(ln, use_ol, oi + i).chr != 0 {
                            if tcdelcost(i as i32) < self.wpfxlen(ln, use_ol, oi + i, &nl, ni) {
                                // Some terminals fill cells freed at the
                                // end with the current attributes.
                                treplaceattrs(self.prompt_attr);
                                applytextattributes();
                                tcmultout(TCDEL, TCMULTDEL, i as i32);
                                oi += i;
                                char_ins -= i as i32;
                                while self.ol(ln, use_ol, oi).chr == WEOF {
                                    oi += 1;
                                    char_ins -= 1;
                                }
                                i = 0;
                                break;
                            }
                            i += 1;
                        }
                        if i == 0 {
                            continue;
                        }
                    }
                    // Characters pushed off the right are lost, but not on
                    // the last line, where that could scroll the screen.
                    if tccan(TCINS) && self.vln != zterm_lines() - 1 {
                        let mut i = 1usize;
                        while at(&nl, ni + i).chr != 0 {
                            if tcinscost(i as i32) < self.wpfxlen(ln, use_ol, oi, &nl, ni + i) {
                                tcmultout(TCINS, TCMULTINS, i as i32);
                                let end = (ni + i).min(nl.len());
                                zwrite(&nl[ni..end]);
                                ni += i;
                                while at(&nl, ni).chr == WEOF {
                                    ni += 1;
                                    i += 1;
                                }
                                char_ins += i as i32;
                                self.vcs += i as i32;
                                ccs = self.vcs;
                                // Truncate the old line where it was
                                // pushed off the right.
                                let room = (winw - ccs).max(0) as usize;
                                let mut k = 0usize;
                                while self.ol(ln, use_ol, oi + k).chr != 0 && k < room {
                                    k += 1;
                                }
                                while self.ol(ln, use_ol, oi + k).chr == WEOF {
                                    k += 1;
                                }
                                if k >= room {
                                    while k > 0 && self.ol(ln, use_ol, oi + k - 1).chr == WEOF {
                                        k -= 1;
                                    }
                                    if use_ol {
                                        if let Some(Some(l)) = self.obuf.get_mut(ln) {
                                            if let Some(cell) = l.get_mut(oi + k) {
                                                *cell = ZR_ZR;
                                            }
                                        }
                                    }
                                    ins_last = true;
                                }
                                i = 0;
                                break;
                            }
                            i += 1;
                        }
                        if i == 0 {
                            continue;
                        }
                    }
                }
            }
            // No tricks: write a character and try again.
            if at(&nl, ni).chr == 0 {
                break;
            }
            loop {
                let c = at(&nl, ni);
                treplaceattrs(c.atr);
                applytextattributes();
                zputc(&c);
                ni += 1;
                if self.ol(ln, use_ol, oi).chr != 0 {
                    oi += 1;
                }
                ccs += 1;
                self.vcs += 1;
                // Always overwrite the whole of a wide character.
                let (o, n) = (self.ol(ln, use_ol, oi), at(&nl, ni));
                if !((o.chr == WEOF && n.chr != 0) || (n.chr == WEOF && o.chr != 0)) {
                    break;
                }
            }
        }
    }

    // wpfxlen(): how many cells of the old line from `oi` match the new
    // one from `ni`.
    fn wpfxlen(&self, ln: usize, use_ol: bool, mut oi: usize, nl: &[RChar], mut ni: usize) -> i32 {
        let mut i = 0;
        loop {
            let o = self.ol(ln, use_ol, oi);
            if o.chr == 0 || o != at(nl, ni) {
                return i;
            }
            oi += 1;
            ni += 1;
            i += 1;
        }
    }

    /// moveto(): moves the cursor to line `ln` (from the prompt line),
    /// column `cl`.
    fn moveto(&mut self, ln: i32, cl: i32) {
        if self.vcs == self.winw {
            self.vln += 1;
            self.vcs = 0;
            if !hasam() {
                zputc(&ZR_CR);
                zputc(&ZR_NL);
            } else {
                let c0 = self.nchr(self.vln, 0);
                let rep = if self.vln < self.nlnct as i32 && c0.chr != 0 {
                    c0
                } else {
                    ZR_SP
                };
                zputc(&rep);
                zputc(&ZR_CR);
                if self.vln < self.olnct && self.ochr(self.vln, 0).chr != 0 {
                    if let Some(Some(l)) = self.obuf.get_mut(self.vln as usize) {
                        l[0] = rep;
                    }
                }
            }
        }

        if ln == self.vln && cl == self.vcs {
            return;
        }

        if ln < self.vln {
            tcmultout(TCUP, TCMULTUP, self.vln - ln);
            self.vln = ln;
        }
        // Down, with newlines where we might go off the screen.
        while ln > self.vln {
            if self.vln < self.vmaxln - 1 {
                if ln > self.vmaxln - 1 {
                    if tc_downcurs(self.vmaxln - 1 - self.vln) {
                        self.vcs = 0;
                    }
                    self.vln = self.vmaxln - 1;
                } else {
                    if tc_downcurs(ln - self.vln) {
                        self.vcs = 0;
                    }
                    self.vln = ln;
                    continue;
                }
            }
            zputc(&ZR_CR);
            self.vcs = 0;
            while ln > self.vln {
                zputc(&ZR_NL);
                self.vln += 1;
            }
        }

        if cl != self.vcs {
            self.singmoveto(cl);
        }
    }

    /// singmoveto(): moves along the current line.
    fn singmoveto(&mut self, pos: i32) {
        if pos == self.vcs {
            return;
        }
        if (!tccan(TCMULTLEFT) || pos == 0) && pos <= self.vcs / 2 {
            zputc(&ZR_CR);
            self.vcs = 0;
        }
        if pos < self.vcs {
            tcmultout(TCLEFT, TCMULTLEFT, self.vcs - pos);
        } else if pos > self.vcs {
            self.tc_rightcurs(pos - self.vcs);
        }
        self.vcs = pos;
    }

    /// tc_rightcurs(): moves `ct` columns right.
    fn tc_rightcurs(&mut self, mut ct: i32) {
        let cl = ct + self.vcs;
        let mut i = self.vcs;

        if tccan(TCMULTRIGHT) {
            tcoutarg(TCMULTRIGHT, ct);
            return;
        }
        if tccan(TCHORIZPOS) {
            tcoutarg(TCHORIZPOS, cl);
            return;
        }
        // Tabs, which do not overwrite (tabs are never expanded here).
        if tccan(TCNEXTTAB) && (self.vcs | 7) < cl {
            i = (self.vcs | 7) + 1;
            tcout(TCNEXTTAB);
            while i + 8 <= cl {
                tcout(TCNEXTTAB);
                i += 8;
            }
            ct = cl - i;
            if ct == 0 {
                return;
            }
        }
        // Otherwise write what is there; within the prompt, the prompt.
        if self.vln == 0 && i < self.lpromptw && termflags() & TERM_SHORT == 0 {
            if tccan(TCRIGHT) && tclen(TCRIGHT) * ct <= self.lpromptbuf.chars().count() as i32 {
                for _ in 0..(self.lpromptw - i) {
                    tcout(TCRIGHT);
                }
            } else {
                if i != 0 {
                    zputc(&ZR_CR);
                }
                tcmultout(TCUP, TCMULTUP, self.lprompth - 1);
                write_tty(&self.lpromptbuf);
                if self.lpromptwof == self.winw {
                    write_tty("\n");
                }
            }
            i = self.lpromptw;
            ct = cl - i;
        }
        if self.has_n(self.vln.max(0) as usize) {
            let mut t = 0usize;
            let mut j = 0;
            while self.nchr(self.vln, t).chr != 0 && j < i {
                j += 1;
                t += 1;
            }
            if j == i {
                while ct > 0 && self.nchr(self.vln, t).chr != 0 {
                    zputc(&self.nchr(self.vln, t));
                    ct -= 1;
                    t += 1;
                }
            }
        }
        while ct > 0 {
            zputc(&ZR_SP);
            ct -= 1;
        }
    }

    /// singlerefresh(): as much of the line as fits on one screen line,
    /// for SINGLE_LINE_ZLE and terminals that cannot do better.
    fn singlerefresh(&mut self, tmpline: &[char], tmpcs: usize, prelen: usize) {
        let owinpos = self.winpos;
        let owinprompt = self.winprompt;
        let combining = crate::options::is_set("combiningchars");
        let tmpll = tmpline.len();
        self.nlnct = 1;

        let lw = self.lpromptw.max(0) as usize;
        let mut vbuf: Vec<RChar> = vec![ZR_SP; lw];
        let mut nvcs = 0i32;
        let mut t0 = 0usize;
        while t0 < tmpll {
            let mut base_attr =
                mixattrs(self.hl.default_attr, self.hl.default_mask, self.prompt_attr);
            for rh in &self.hl.rhs {
                let offset = if rh.predisplay { 0 } else { prelen as i64 };
                if rh.start + offset <= t0 as i64 && (t0 as i64) < rh.end + offset {
                    if base_attr & (TXTFGCOLOUR | TXTBGCOLOUR) != 0 {
                        // Keep the colour already set.
                        base_attr |= rh.atr & !TXT_ATTR_COLOUR_MASK;
                    } else {
                        base_attr |= rh.atr;
                    }
                }
            }
            let all_attr = mixattrs(self.hl.special_attr, self.hl.special_mask, base_attr);
            if t0 == tmpcs {
                nvcs = vbuf.len() as i32;
            }
            let c = tmpline[t0];
            let width = wcwidth(c);
            if c == '\t' {
                vbuf.push(ZR_SP);
                while vbuf.len() & 7 != 0 {
                    vbuf.push(ZR_SP);
                }
            } else if c == '\n' {
                vbuf.push(rc('\\' as u32, all_attr));
                vbuf.push(rc('n' as u32, all_attr));
            } else if iswprint(c) && width > 0 {
                let mut ichars = 1;
                if combining && is_basechar(c) {
                    while t0 + ichars < tmpll && is_combining(tmpline[t0 + ichars]) {
                        ichars += 1;
                    }
                }
                vbuf.push(if ichars > 1 {
                    RChar {
                        chr: c as u32,
                        atr: base_attr | TXT_MULTIWORD_MASK,
                        mw: Some(tmpline[t0..t0 + ichars].into()),
                    }
                } else {
                    rc(c as u32, base_attr)
                });
                for _ in 1..width {
                    vbuf.push(rc(WEOF, base_attr));
                }
                t0 += ichars - 1;
            } else if iswcntrl(c) && (c as u32) <= 0xff {
                vbuf.push(rc('^' as u32, all_attr));
                vbuf.push(rc(ctrl_second(c), all_attr));
            } else {
                for d in hexform(c).chars() {
                    vbuf.push(rc(d as u32, all_attr));
                }
            }
            t0 += 1;
        }
        if t0 == tmpcs {
            nvcs = vbuf.len() as i32;
        }
        vbuf.push(ZR_ZR);

        // Which part of the line to show.
        let hasam = i32::from(hasam());
        let winw = self.winw;
        if self.winpos == -1 {
            self.winpos = 0;
        }
        if (self.winpos != 0 && nvcs < self.winpos + 1) || nvcs > self.winpos + winw - 2 {
            self.winpos = (nvcs - (winw - hasam) / 2).max(0);
        }
        let wp = self.winpos as usize;
        if wp != 0 && wp < vbuf.len() {
            // The line continues to the left.
            vbuf[wp] = rc('<' as u32, self.hl.ellipsis_attr);
        }
        if zr_strlen(&vbuf[wp.min(vbuf.len())..]) as i32 > winw - hasam {
            // The line continues to the right.
            let k = wp + (winw - hasam - 1).max(0) as usize;
            vbuf[k] = rc('>' as u32, self.hl.ellipsis_attr);
            vbuf[k + 1] = ZR_ZR;
        }
        let tail = &vbuf[wp.min(vbuf.len())..];
        let mut line0 = tail[..zr_strlen(tail)].to_vec();
        line0.push(ZR_ZR);
        let w = winw.max(0) as usize + 2;
        if line0.len() < w {
            line0.resize(w, ZR_ZR);
        }
        self.nline(0);
        self.nbuf[0] = Some(line0);
        nvcs -= self.winpos;

        self.winprompt = if self.winpos < self.lpromptw {
            // Skip the part of the buffer that is the prompt.
            self.lpromptw - self.winpos
        } else {
            0
        };
        if self.winpos != owinpos && self.winprompt != 0 {
            // Show what fits of the prompt: skip winpos characters,
            // writing escape sequences (zsh's %{...%}) whatever happens.
            self.singmoveto(0);
            let (clean, map) = crate::get_clean_and_mappings(&self.lpromptbuf);
            let src = self.lpromptbuf.clone();
            let mut skipchars = self.winpos;
            let mut prev = 0usize;
            for (k, ch) in clean.chars().enumerate() {
                let idx = map[k];
                write_tty(&src[prev..idx]);
                if skipchars == 0 {
                    let mut b = [0u8; 4];
                    write_bytes(ch.encode_utf8(&mut b).as_bytes());
                } else {
                    skipchars -= 1;
                }
                prev = idx + ch.len_utf8();
            }
            write_tty(&src[prev..]);
            self.vcs = self.winprompt;
        }

        // Display the visible part.
        let mut t0 = self.winprompt;
        let mut vp = self.winprompt.max(0) as usize;
        let mut rop = vp;
        loop {
            // Where the prompt used to be, anything may be on screen.
            if vp as i32 >= owinprompt {
                while self.nchr(0, vp).chr != 0 && self.ochr(0, rop) == self.nchr(0, vp) {
                    t0 += 1;
                    vp += 1;
                    rop += 1;
                }
            }
            if self.nchr(0, vp).chr == 0 && self.ochr(0, rop).chr == 0 {
                break;
            }
            self.singmoveto(t0);
            if self.ochr(0, rop).chr == 0 {
                let line = self.nbuf[0].clone().unwrap_or_default();
                let from = vp.min(line.len());
                let n = zr_strlen(&line[from..]);
                zwrite(&line[from..from + n]);
                self.vcs += n as i32;
                break;
            }
            if self.nchr(0, vp).chr == 0 {
                if tccan(TCCLEAREOL) {
                    self.tcoutclear(TCCLEAREOL);
                } else {
                    while self.ochr(0, rop).chr != 0 {
                        rop += 1;
                        zputc(&ZR_SP);
                        self.vcs += 1;
                    }
                }
                break;
            }
            zputc(&self.nchr(0, vp));
            self.vcs += 1;
            t0 += 1;
            vp += 1;
            rop += 1;
        }
        self.singmoveto(nvcs);
        self.bufswap();
    }
}

fn tcdelcost(x: i32) -> i32 {
    if tccan(TCMULTDEL) {
        tclen(TCMULTDEL)
    } else {
        x * tclen(TCDEL)
    }
}

fn tcinscost(x: i32) -> i32 {
    if tccan(TCMULTINS) {
        tclen(TCMULTINS)
    } else {
        x * tclen(TCINS)
    }
}

/// tc_downcurs(): true when it had to use newlines, leaving the cursor
/// in the first column.
fn tc_downcurs(ct: i32) -> bool {
    if ct != 0 && !tcmultout(TCDOWN, TCMULTDOWN, ct) {
        for _ in 0..ct {
            zputc(&ZR_NL);
        }
        zputc(&ZR_CR);
        return true;
    }
    false
}

// zsh's listshown; widgets may set the flag in RefreshState directly.
fn listshown(z: &Zle) -> i32 {
    let v = result::listshown();
    if v == 0 && z.refresh.listshown {
        1
    } else {
        v
    }
}

impl Zle {
    /// zle_set_highlight(): the special highlights from $zle_highlight,
    /// then the user's from $region_highlight.
    fn zle_set_highlight(&mut self) {
        let atrs = crate::shell::array_get("zle_highlight").unwrap_or_default();
        let mut hl = Hl {
            special_layer: 30,
            ..Hl::default()
        };
        let mut specials: Vec<Rh> = [20, 20, 10, 15]
            .iter()
            .map(|&layer| Rh {
                layer,
                ..Rh::default()
            })
            .collect();
        let mut set = [false; 6]; // special region isearch suffix paste ellipsis
        for a in &atrs {
            if a == "none" {
                hl.special_attr = 0;
                hl.default_attr = 0;
                hl.special_mask = 0;
                hl.default_mask = 0;
                set[..5].fill(true);
            } else if let Some(r) = a.strip_prefix("default:") {
                let m = match_highlight(r, false);
                hl.default_attr = m.atr;
                hl.default_mask = m.mask;
            } else if let Some(r) = a.strip_prefix("special:") {
                let m = match_highlight(r, true);
                hl.special_attr = m.atr;
                hl.special_mask = m.mask;
                if let Some(l) = m.layer {
                    hl.special_layer = l;
                }
                set[0] = true;
            } else if let Some((k, r)) = ["region:", "isearch:", "suffix:", "paste:"]
                .iter()
                .enumerate()
                .find_map(|(k, p)| a.strip_prefix(p).map(|r| (k, r)))
            {
                let m = match_highlight(r, true);
                specials[k].atr = m.atr;
                specials[k].mask = m.mask;
                if let Some(l) = m.layer {
                    specials[k].layer = l;
                }
                set[k + 1] = true;
            } else if let Some(r) = a.strip_prefix("ellipsis:") {
                hl.ellipsis_attr = match_highlight(r, false).atr;
                set[5] = true;
            }
        }
        if !set[0] {
            hl.special_attr = TXTSTANDOUT;
            hl.special_mask = TXTSTANDOUT;
        }
        for (k, def) in [TXTSTANDOUT, TXTUNDERLINE, TXTBOLDFACE, TXTSTANDOUT]
            .into_iter()
            .enumerate()
        {
            if !set[k + 1] {
                specials[k].atr = def;
                specials[k].mask = def;
            }
        }
        if !set[5] {
            hl.ellipsis_attr = TXTBGCOLOUR
                | (3u64 << TXT_ATTR_BG_COL_SHIFT)
                | TXTFGCOLOUR
                | (4u64 << TXT_ATTR_FG_COL_SHIFT);
        }

        // Where the special highlights apply now.
        let cs = self.cs as i64;
        let region = if self.region_active != 0 {
            let (mut s, mut e) = if self.cs <= self.mark {
                (self.cs, self.mark)
            } else {
                (self.mark, self.cs)
            };
            if self.region_active == 2 {
                e = self.findeol_at(e);
                s = self.findbol_at(s);
            } else if self.invicmdmode() {
                e += 1;
            }
            (s as i64, e as i64)
        } else {
            (-1, -1)
        };
        let isearch = if self.isearch.2 {
            (
                self.isearch.0.map_or(-1, |p| p as i64),
                self.isearch.1.map_or(-1, |p| p as i64),
            )
        } else {
            (-1, -1)
        };
        let sl = crate::zle::comp::tricky::suffixlen(self) as i64;
        let suffix = if sl != 0 { (cs - sl, cs) } else { (-1, -1) };
        let paste = if self.lastcmd & ZLE_YANK != 0 {
            (self.yankb as i64, self.yanke as i64)
        } else {
            (-1, -1)
        };
        for (rh, (s, e)) in specials.iter_mut().zip([region, isearch, suffix, paste]) {
            rh.start = s;
            rh.end = e;
        }
        debug_assert_eq!(specials.len(), N_SPECIAL_HIGHLIGHTS);
        hl.rhs = specials;
        for rh in &self.region_highlights {
            let m = match_highlight(&rh.attr, true);
            hl.rhs.push(Rh {
                start: rh.start,
                end: rh.end,
                atr: m.atr,
                mask: m.mask,
                layer: m.layer.unwrap_or(10),
                predisplay: rh.predisplay,
            });
        }
        self.refresh.hl = hl;
        allocate_colour_buffer();
    }

    /// resetvideo(): starts a new display from where the cursor is.
    fn resetvideo(&mut self) {
        let lines = zterm_lines();
        let r = &mut self.refresh;
        r.winw = zterm_columns();
        r.winh = if termflags() & TERM_SHORT != 0 {
            1
        } else if lines < 2 {
            24
        } else {
            lines
        };
        r.rwinh = lines;
        r.vln = 0;
        r.vmaxln = 0;
        r.winprompt = 0;
        r.winpos = -1;
        let w = r.winw.max(1) as usize + 2;
        let n = r.winh.max(1) as usize + 1;
        r.nbuf = vec![None; n];
        r.obuf = vec![None; n];
        let mut first = vec![ZR_ZR; w];
        first[0] = ZR_NL;
        r.nbuf[0] = Some(first.clone());
        r.obuf[0] = Some(first);

        (r.lpromptwof, r.lprompth) = countprompt(&r.lpromptbuf, 1);
        (r.rpromptw, r.rprompth) = countprompt(&r.rpromptbuf, 0);
        if r.lpromptwof < r.winw {
            r.lpromptw = r.lpromptwof;
        } else {
            r.lpromptw = 0;
            r.lprompth += 1;
        }
        if r.lpromptw > 0 {
            let lw = (r.lpromptw as usize).min(w - 1);
            for l in [&mut r.nbuf[0], &mut r.obuf[0]].into_iter().flatten() {
                for cell in &mut l[..lw] {
                    *cell = ZR_SP;
                }
                l[lw] = ZR_ZR;
            }
        }
        r.vcs = r.lpromptw;
        r.olnct = 0;
        r.nlnct = 0;
        if self.showinglist > 0 {
            self.showinglist = -2;
        }
        self.trashed = false;
    }

    /// Redraws the prompt and the line: zsh's zrefresh().
    pub fn zrefresh(&mut self) {
        // Drawing a list from the end of a refresh needs no refresh.
        if !zle_active() || INLIST.with(|i| i.get()) {
            return;
        }
        refresh_term::reload();

        // zsh's lpromptbuf and rpromptbuf, with the prompt marks.
        let lpromptbuf = format!("{}{}", termctl::lprompt_marker(self), self.lprompt);
        let rpromptbuf = format!("{}{}", termctl::rprompt_marker(self), self.rprompt);
        let pmpt_attr = sgr_attrs(&self.lprompt, 0);
        let rpmpt_attr = sgr_attrs(&self.rprompt, pmpt_attr);
        {
            let r = &mut self.refresh;
            r.lpromptbuf = lpromptbuf;
            r.rpromptbuf = rpromptbuf;
            r.pmpt_attr = pmpt_attr;
            r.rpmpt_attr = rpmpt_attr;
            r.prompt_attr = mixattrs(pmpt_attr, pmpt_attr & TXT_ATTR_ALL, rpmpt_attr);
        }

        let prelen = self.predisplay.len();
        let mut tmpline: Vec<char> =
            Vec::with_capacity(prelen + self.line.len() + self.postdisplay.len());
        tmpline.extend_from_slice(&self.predisplay);
        tmpline.extend_from_slice(&self.line);
        tmpline.extend_from_slice(&self.postdisplay);
        let tmpcs = (self.cs + prelen).min(tmpline.len());

        self.zle_set_highlight();

        if self.clearlist && listshown(self) > 0 {
            if tccan(TCCLEAREOD) {
                let r = &mut self.refresh;
                let (ovln, ovcs) = (r.vln, r.vcs);
                let idx = ovln.max(0) as usize;
                // Moving uses the new buffer; the screen is the old one.
                let saved = r.nbuf.get_mut(idx).map(|l| l.take());
                let old = r.obuf.get(idx).cloned().flatten();
                if let Some(slot) = r.nbuf.get_mut(idx) {
                    *slot = old;
                }
                r.moveto(r.nlnct as i32, 0);
                r.tcoutclear(TCCLEAREOD);
                r.moveto(ovln, ovcs);
                if let (Some(slot), Some(nb)) = (r.nbuf.get_mut(idx), saved) {
                    *slot = nb;
                }
            } else {
                result::invalidate_list(self);
                self.refresh.moveto(0, 0);
                result::set_clearflag(0);
                self.resetneeded = true;
            }
            result::set_listshown(self, 0);
            result::set_lastlistlen(0);
            if self.showinglist != -2 {
                self.showinglist = 0;
            }
        }
        self.clearlist = false;

        self.refresh.cleareol = false;
        self.refresh.more_start = false;
        self.refresh.more_end = false;
        let lines = zterm_lines();
        set_short(
            crate::options::is_set("singlelinezle")
                || lines < 3
                || termflags() & (TERM_NOUP | TERM_BAD | TERM_UNKNOWN) != 0,
        );
        if self.resetneeded {
            self.refresh.onumscrolls = 0;
            // Only attributes we set ourselves should be in force.
            tsetcap(TCALLATTRSOFF);
            set_current(0);
            set_unknown(0);
            self.resetvideo();
            self.resetneeded = false;
            self.refresh.oput_rpmpt = false;
            let clearflag = result::clearflag() != 0;
            if !clearflag {
                if tccan(TCCLEAREOD) {
                    self.refresh.tcoutclear(TCCLEAREOD);
                } else {
                    self.refresh.cleareol = true;
                }
                if listshown(self) > 0 {
                    result::set_listshown(self, 0);
                }
            }
            let r = &mut self.refresh;
            if termflags() & TERM_SHORT != 0 {
                r.vcs = 0;
            } else if !clearflag && !r.lpromptbuf.is_empty() {
                write_tty(&r.lpromptbuf);
                if r.lpromptwof == r.winw {
                    // Right with and without automargins.
                    write_tty("\n");
                }
                // The prompt carries its own escapes.
                set_current(r.pmpt_attr);
                set_unknown(0);
            }
            if clearflag {
                zputc(&ZR_CR);
                r.vcs = 0;
                let lw = r.lpromptw;
                r.moveto(0, lw);
            }
            flush_tty();
            r.clearf = clearflag;
        } else if self.refresh.winw != zterm_columns() || self.refresh.rwinh != lines {
            self.resetvideo();
        }

        let statusline = self.statusline.clone();
        if termflags() & TERM_SHORT != 0 {
            self.refresh.singlerefresh(&tmpline, tmpcs, prelen);
        } else {
            let transient = self.trashed && crate::options::is_set("transientrprompt");
            self.refresh
                .draw(&tmpline, tmpcs, prelen, statusline.as_deref(), transient);
            if self.trashed {
                termctl::on_trash(self);
            } else {
                termctl::cursor_form(self);
            }
            self.refresh.finish();
        }

        flush_tty();
        free_colour_buffer();

        // A new list, or one the line has grown over, is drawn now.
        let nlnct = self.refresh.nlnct as i32;
        if self.showinglist == -2 || (self.showinglist > 0 && self.showinglist < nlnct) {
            INLIST.with(|i| i.set(true));
            result::list_matches(self);
            INLIST.with(|i| i.set(false));
            if !self.errflag {
                self.zrefresh();
            }
        }
        if self.showinglist == -1 {
            self.showinglist = nlnct;
        }
    }

    /// Moves below the display, leaving it as it is, so other output can
    /// follow (zsh's trashzle).
    pub fn trash(&mut self) {
        if zle_active() && !self.trashed {
            // This refresh only puts the line right; listing here would
            // recurse.
            let sl = self.showinglist;
            self.showinglist = 0;
            self.trashed = true;
            self.zrefresh();
            self.showinglist = sl;
            let r = &mut self.refresh;
            treplaceattrs(r.prompt_attr);
            applytextattributes();
            let n = r.nlnct as i32;
            r.moveto(n, 0);
            if result::clearflag() != 0 && tccan(TCCLEAREOD) {
                tcout(TCCLEAREOD);
                result::set_clearflag(0);
                result::set_listshown(self, 0);
            }
            termctl::write_postedit();
            flush_tty();
            self.resetneeded = true;
        }
    }

    pub fn clearscreen(&mut self) {
        refresh_term::reload();
        self.refresh.tcoutclear(TCCLEARSCREEN);
        self.resetneeded = true;
        result::set_clearflag(0);
    }

    pub fn redisplay(&mut self) {
        refresh_term::reload();
        let r = &mut self.refresh;
        if !r.nbuf.is_empty() {
            r.moveto(0, 0);
        }
        // Extra care.
        zputc(&ZR_CR);
        tcmultout(TCUP, TCMULTUP, (r.lprompth - 1).max(0));
        self.resetneeded = true;
        result::set_clearflag(0);
    }
}

/// An SGR sequence for a zle_highlight style like `fg=red,bold`.
pub fn attr_to_sgr(attr: &str) -> String {
    let mut out = String::new();
    for part in attr.split(',') {
        let part = part.trim();
        let code = match part {
            "bold" => "1".to_string(),
            "faint" => "2".to_string(),
            "italic" => "3".to_string(),
            "underline" => "4".to_string(),
            "standout" | "reverse" => "7".to_string(),
            "none" | "" => continue,
            _ => {
                if let Some(c) = part.strip_prefix("fg=") {
                    colour(c, 30)
                } else if let Some(c) = part.strip_prefix("bg=") {
                    colour(c, 40)
                } else {
                    continue;
                }
            }
        };
        if !code.is_empty() {
            out.push_str(&format!("\x1b[{}m", code));
        }
    }
    out
}

fn colour(c: &str, base: u32) -> String {
    let names = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ];
    if let Some(i) = names.iter().position(|n| *n == c) {
        return (base + i as u32).to_string();
    }
    if c == "default" {
        return (base + 9).to_string();
    }
    if let Ok(n) = c.parse::<u32>() {
        return if n < 8 {
            (base + n).to_string()
        } else {
            format!("{};5;{}", base + 8, n)
        };
    }
    if let Some(hex) = c.strip_prefix('#') {
        if hex.len() == 6 {
            if let Ok(v) = u32::from_str_radix(hex, 16) {
                return format!("{};2;{};{};{}", base + 8, v >> 16, (v >> 8) & 255, v & 255);
            }
        }
    }
    String::new()
}

/// $region_highlight as zsh shows it: the style is kept in the form zsh
/// prints, with any layer other than the default.
pub fn get_region_highlight(z: &Zle) -> Vec<String> {
    z.region_highlights
        .iter()
        .map(|rh| {
            let mut s = format!(
                "{}{} {} {}",
                if rh.predisplay { "P" } else { "" },
                rh.start,
                rh.end,
                rh.attr
            );
            if let Some(m) = &rh.memo {
                s.push_str(&format!(" memo={}", m));
            }
            s
        })
        .collect()
}

fn parse_region_highlight_item(item: &str) -> RegionHighlight {
    let blank = |b: u8| b == b' ' || b == b'\t';
    let s = item.as_bytes();
    let predisplay = s.first() == Some(&b'P');
    let mut i = usize::from(predisplay);
    while i < s.len() && blank(s[i]) {
        i += 1;
    }
    let (v, e) = zstrtol(s, i, 10);
    let start = if e == i { -1 } else { v };
    i = e;
    while i < s.len() && blank(s[i]) {
        i += 1;
    }
    let (v, e) = zstrtol(s, i, 10);
    let end = if e == i { -1 } else { v };
    i = e;
    while i < s.len() && blank(s[i]) {
        i += 1;
    }
    let m = match_highlight(&item[i..], true);
    i += m.consumed;
    while i < s.len() && blank(s[i]) {
        i += 1;
    }
    // The memo ends at a comma, blank, or NUL, leaving room for more fields.
    let memo = item[i..].strip_prefix("memo=").map(|r| {
        r.chars()
            .take_while(|&c| c != '\0' && c != ',' && c != ' ' && c != '\t')
            .collect::<String>()
    });
    let mut attr = output_highlight(m.atr, m.mask);
    let layer = m.layer.unwrap_or(10);
    if layer != 10 {
        attr.push_str(&format!(",layer={}", layer));
    }
    RegionHighlight {
        start,
        end,
        attr,
        memo,
        predisplay,
    }
}

/// Normalizes `$region_highlight` entries as zsh's `set_region_highlight` does.
pub fn normalize_region_highlight(items: Vec<String>) -> Vec<String> {
    items
        .into_iter()
        .map(|item| {
            let rh = parse_region_highlight_item(&item);
            let mut s = format!(
                "{}{} {} {}",
                if rh.predisplay { "P" } else { "" },
                rh.start,
                rh.end,
                rh.attr
            );
            if let Some(m) = &rh.memo {
                s.push_str(&format!(" memo={}", m));
            }
            s
        })
        .collect()
}

/// Sets $region_highlight from its elements: `[P]start end style [memo=x]`.
pub fn set_region_highlight(z: &mut Zle, items: &[String]) {
    z.region_highlights.clear();
    for item in items {
        z.region_highlights.push(parse_region_highlight_item(item));
    }
}
