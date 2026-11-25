//! The Z line editor, after zsh's Src/Zle.
//!
//! State lives in one [`Zle`] value owned by [`zleread`]. Widgets get it
//! as `&mut Zle`. Shell code a widget runs (a user widget, a hook) gets
//! back to it through [`with_active`], which only works inside
//! [`Zle::shell`]; that keeps a single `&mut` path to the state.

pub mod comp;
pub mod keymap;
pub mod params;
pub mod refresh;
pub mod termctl;
pub mod thingy;
pub mod utils;
pub mod widgets;

use keymap::{Bind, KeymapRef};
use std::cell::Cell;

pub type WidgetFn = fn(&mut Zle, &[String]) -> i32;

// Widget flags, as zle.h.
pub const WIDGET_INT: u32 = 1 << 0;
pub const WIDGET_NCOMP: u32 = 1 << 1;
pub const ZLE_MENUCMP: u32 = 1 << 2;
pub const ZLE_YANKAFTER: u32 = 1 << 3;
pub const ZLE_YANKBEFORE: u32 = 1 << 4;
pub const ZLE_YANK: u32 = ZLE_YANKAFTER | ZLE_YANKBEFORE;
pub const ZLE_LINEMOVE: u32 = 1 << 5;
pub const ZLE_VIOPER: u32 = 1 << 6;
pub const ZLE_LASTCOL: u32 = 1 << 7;
pub const ZLE_KILL: u32 = 1 << 8;
pub const ZLE_KEEPSUFFIX: u32 = 1 << 9;
pub const ZLE_NOTCOMMAND: u32 = 1 << 10;
pub const ZLE_ISCOMP: u32 = 1 << 11;
pub const WIDGET_INUSE: u32 = 1 << 12;
pub const WIDGET_FREE: u32 = 1 << 13;
pub const ZLE_NOLAST: u32 = 1 << 14;

// Modifier flags.
pub const MOD_MULT: u32 = 1 << 0;
pub const MOD_TMULT: u32 = 1 << 1;
pub const MOD_VIBUF: u32 = 1 << 2;
pub const MOD_VIAPP: u32 = 1 << 3;
pub const MOD_NEG: u32 = 1 << 4;
pub const MOD_NULL: u32 = 1 << 5;
pub const MOD_CHAR: u32 = 1 << 6;
pub const MOD_LINE: u32 = 1 << 7;
pub const MOD_PRI: u32 = 1 << 8;
pub const MOD_CLIP: u32 = 1 << 9;
pub const MOD_OSSEL: u32 = MOD_PRI | MOD_CLIP;

// cut() flags.
pub const CUT_FRONT: u32 = 1 << 0;
pub const CUT_REPLACE: u32 = 1 << 1;
pub const CUT_RAW: u32 = 1 << 2;
pub const CUT_YANK: u32 = 1 << 3;

pub const CUTBUFFER_LINE: u32 = 1;
pub const KRINGCTDEF: usize = 8;

// zleread flags.
pub const ZLRF_HISTORY: u32 = 1 << 0;
pub const ZLRF_NOSETTY: u32 = 1 << 1;
pub const ZLRF_IGNOREEOF: u32 = 1 << 2;

// zleread contexts.
pub const ZLCON_LINE_START: i32 = 0;
pub const ZLCON_LINE_CONT: i32 = 1;
pub const ZLCON_SELECT: i32 = 2;
pub const ZLCON_VARED: i32 = 3;

// setline flags.
pub const ZSL_COPY: u32 = 1;
pub const ZSL_TOEND: u32 = 2;

#[derive(Clone, Copy, Debug, Default)]
pub struct Modifier {
    pub flags: u32,
    pub mult: i64,
    pub tmult: i64,
    pub vibuf: usize,
    pub base: u32,
}

impl Modifier {
    pub fn new() -> Self {
        Modifier {
            flags: 0,
            mult: 1,
            tmult: 1,
            vibuf: 0,
            base: 10,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CutBuffer {
    pub buf: Vec<char>,
    pub flags: u32,
    /// Whether the buffer holds anything (zsh's non-NULL buf).
    pub set: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ViChange {
    pub zmod: Modifier,
    pub buf: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Change {
    pub flags: u32,
    pub hist: usize,
    pub off: usize,
    pub del: Vec<char>,
    pub ins: Vec<char>,
    pub old_cs: usize,
    pub new_cs: usize,
    pub changeno: i64,
}

pub const CH_NEXT: u32 = 1;
pub const CH_PREV: u32 = 2;

#[derive(Clone, Debug, Default)]
pub struct RegionHighlight {
    pub start: i64,
    pub end: i64,
    pub attr: String,
    pub memo: Option<String>,
    pub predisplay: bool,
}

pub struct Zle {
    // The line and cursor.
    pub line: Vec<char>,
    pub cs: usize,
    pub mark: usize,
    pub region_active: i32,
    pub insmode: bool,
    pub done: bool,
    pub eofsent: bool,
    pub errflag: bool,

    // Input.
    pub fd: i32,
    pub unget: Vec<u8>,
    pub keybuf: Vec<u8>,
    pub lastchar: i32,
    pub lastchar_wide: Option<char>,
    pub lastchar_wide_valid: bool,
    pub keytimeout: i64,

    // Keymaps and commands.
    pub curkeymapname: String,
    pub curkeymap: KeymapRef,
    pub localkeymap: Option<KeymapRef>,
    pub bindk: Option<String>,
    pub lbindk: Option<String>,
    pub lastcmd: u32,
    pub zmod: Modifier,
    pub prefixflag: bool,
    pub lastnamed: Option<String>,

    // vi state.
    pub vichgflag: i32,
    pub viinrepeat: i32,
    pub curvichg: ViChange,
    pub lastvichg: ViChange,
    pub vichgrepeat: bool,
    pub virangeflag: i32,
    pub vilinerange: bool,
    pub viinsbegin: usize,
    pub vistartchange: i64,
    pub vimarkcs: [Option<usize>; 27],
    pub vimarkline: [usize; 27],
    pub vfinddir: i32,
    pub vfindchar: Option<char>,
    pub tailadd: i32,
    pub visrchstr: Option<String>,
    pub visrchsense: i32,

    // Cut buffers.
    pub cutbuf: CutBuffer,
    pub kring: Vec<CutBuffer>,
    pub kringnum: usize,
    pub kringsize: usize,
    pub vibuf: Vec<CutBuffer>,
    pub yankb: usize,
    pub yanke: usize,
    pub yankcs: usize,
    pub kct: Option<usize>,

    // Undo.
    pub changes: Vec<Change>,
    pub ucur: usize,
    pub nextchanges: Vec<Change>,
    pub undo_changeno: i64,
    pub undo_limitno: i64,
    pub lastline: Vec<char>,
    pub lastcs: usize,

    // History.
    pub histline: usize,
    pub curhist: usize,
    pub lastcol: i32,

    // Display.
    pub lprompt: String,
    pub rprompt: String,
    pub statusline: Option<String>,
    pub predisplay: Vec<char>,
    pub postdisplay: Vec<char>,
    pub region_highlights: Vec<RegionHighlight>,
    pub trashed: bool,
    pub resetneeded: bool,
    pub showinglist: i32,
    pub clearlist: bool,
    pub refresh: refresh::RefreshState,

    // The read.
    pub zlereadflags: u32,
    pub zlecontext: i32,
    pub recursive: i32,
    pub pending_vared: Option<String>,
    pub isfirstln: bool,
    pub saved_termios: Option<libc::termios>,
    pub suffix: utils::Suffix,
    pub lastsearch: Option<String>,
    pub lastabortedsearch: Option<String>,
    pub isearch: (Option<usize>, Option<usize>, bool),
    pub widget_depth: i32,
    pub comp: comp::CompState,
}

thread_local! {
    static ACTIVE: Cell<*mut Zle> = const { Cell::new(std::ptr::null_mut()) };
    /// Set while zleread runs, even outside widget code.
    static ZLE_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// Whether the line editor is reading a line.
pub fn zle_active() -> bool {
    ZLE_ACTIVE.with(|a| a.get())
}

struct ActiveGuard(*mut Zle);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE.with(|a| a.set(self.0));
    }
}

/// Runs `f` on the line editor's state, from shell code a widget runs.
/// None when no widget is running.
pub fn with_active<R>(f: impl FnOnce(&mut Zle) -> R) -> Option<R> {
    let p = ACTIVE.with(|a| a.replace(std::ptr::null_mut()));
    if p.is_null() {
        return None;
    }
    let _guard = ActiveGuard(p);
    // SAFETY: `p` came from `Zle::shell`'s `&mut Zle`, and `ACTIVE` is
    // cleared while `f` executes so no re-entrant call can alias `&mut *p`
    // unless it explicitly reborrows via `Zle::shell`.
    Some(f(unsafe { &mut *p }))
}

impl Zle {
    pub fn new(fd: i32) -> Zle {
        let main = keymap::open_keymap("main")
            .or_else(|| keymap::open_keymap(".safe"))
            .unwrap();
        Zle {
            line: Vec::new(),
            cs: 0,
            mark: 0,
            region_active: 0,
            insmode: true,
            done: false,
            eofsent: false,
            errflag: false,
            fd,
            unget: Vec::new(),
            keybuf: Vec::new(),
            lastchar: 0,
            lastchar_wide: None,
            lastchar_wide_valid: false,
            keytimeout: 40,
            curkeymapname: "main".into(),
            curkeymap: main,
            localkeymap: None,
            bindk: None,
            lbindk: None,
            lastcmd: 0,
            zmod: Modifier::new(),
            prefixflag: false,
            lastnamed: None,
            vichgflag: 0,
            viinrepeat: 0,
            curvichg: ViChange::default(),
            lastvichg: ViChange::default(),
            vichgrepeat: false,
            virangeflag: 0,
            vilinerange: false,
            viinsbegin: 0,
            vistartchange: -1,
            vimarkcs: [None; 27],
            vimarkline: [0; 27],
            vfinddir: 0,
            vfindchar: None,
            tailadd: 0,
            visrchstr: None,
            visrchsense: 0,
            cutbuf: CutBuffer::default(),
            kring: Vec::new(),
            kringnum: 0,
            kringsize: 0,
            vibuf: vec![CutBuffer::default(); 37],
            yankb: 0,
            yanke: 0,
            yankcs: 0,
            kct: None,
            changes: Vec::new(),
            ucur: 0,
            nextchanges: Vec::new(),
            undo_changeno: 0,
            undo_limitno: 0,
            lastline: Vec::new(),
            lastcs: 0,
            histline: 0,
            curhist: 0,
            lastcol: -1,
            lprompt: String::new(),
            rprompt: String::new(),
            statusline: None,
            predisplay: Vec::new(),
            postdisplay: Vec::new(),
            region_highlights: Vec::new(),
            trashed: false,
            resetneeded: true,
            showinglist: 0,
            clearlist: false,
            refresh: refresh::RefreshState::default(),
            zlereadflags: 0,
            zlecontext: 0,
            recursive: 0,
            pending_vared: None,
            isfirstln: true,
            saved_termios: None,
            suffix: utils::Suffix::default(),
            lastsearch: None,
            lastabortedsearch: None,
            isearch: (None, None, false),
            widget_depth: 0,
            comp: comp::CompState::default(),
        }
    }

    /// Runs shell code that may call back into the line editor.
    pub fn shell<R>(&mut self, f: impl FnOnce() -> R) -> R {
        let _guard = ActiveGuard(ACTIVE.with(|a| a.replace(self as *mut Zle)));
        f()
    }

    pub fn ll(&self) -> usize {
        self.line.len()
    }

    pub fn zmult(&self) -> i64 {
        self.zmod.mult
    }

    pub fn invicmdmode(&self) -> bool {
        self.curkeymapname == "vicmd"
    }

    // ---- input ----------------------------------------------------------

    pub fn ungetbyte(&mut self, b: u8) {
        self.unget.push(b);
    }

    /// Pushes `s` back so it is read next, in order.
    pub fn ungetbytes(&mut self, s: &[u8]) {
        for &b in s.iter().rev() {
            self.unget.push(b);
        }
    }

    /// Reads a byte. With `keytmout`, gives up after $KEYTIMEOUT
    /// hundredths of a second and returns Err(true); end of input is
    /// Err(false).
    pub fn getbyte(&mut self, keytmout: bool) -> Result<u8, bool> {
        self.lastchar_wide_valid = false;
        let b = if let Some(b) = self.unget.pop() {
            b
        } else {
            // The terminal swaps CR and NL (INLCR, ICRNL) so typeahead
            // read before the switch to raw mode looks right; swap back.
            match self.raw_getbyte(keytmout)? {
                b'\r' => b'\n',
                b'\n' => b'\r',
                b => b,
            }
        };
        if self.vichgflag != 0 && !self.vichgrepeat {
            self.curvichg.buf.push(b);
        }
        self.lastchar = b as i32;
        Ok(b)
    }

    fn raw_getbyte(&mut self, keytmout: bool) -> Result<u8, bool> {
        let timeout_ms: i32 = if keytmout {
            (self.keytimeout.max(0) * 10) as i32
        } else {
            -1
        };
        let start = std::time::Instant::now();
        loop {
            let wait = if timeout_ms < 0 {
                -1
            } else {
                let left = timeout_ms as i64 - start.elapsed().as_millis() as i64;
                if left <= 0 {
                    return Err(true);
                }
                left as i32
            };
            let watches: Vec<(i32, String, bool)> = thingy::watch_fds()
                .into_iter()
                .filter(|(fd, _, _)| *fd != self.fd)
                .collect();
            let mut pfds = Vec::with_capacity(1 + watches.len());
            pfds.push(libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            });
            for (fd, _, _) in &watches {
                pfds.push(libc::pollfd {
                    fd: *fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            let r = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, wait) };
            if r < 0 {
                let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if e == libc::EINTR {
                    if self.handle_signals() {
                        return Err(false);
                    }
                    continue;
                }
                return Err(false);
            }
            if r == 0 {
                return Err(true);
            }
            let mut handled_watch = false;
            for ((fd, func, is_widget), pfd) in watches.iter().zip(pfds[1..].iter()) {
                if pfd.revents == 0 {
                    continue;
                }
                handled_watch = true;
                let mut args = vec![fd.to_string()];
                if pfd.revents & libc::POLLNVAL != 0 {
                    thingy::remove_watch_fd(*fd);
                    args.push("nval".to_string());
                } else if pfd.revents & libc::POLLERR != 0 {
                    args.push("err".to_string());
                } else if pfd.revents & libc::POLLHUP != 0 && pfd.revents & libc::POLLIN == 0 {
                    args.push("hup".to_string());
                }
                if *is_widget {
                    self.execzlefunc(func, &args, true, false);
                } else {
                    self.call_user_widget(func, &args);
                }
            }
            if handled_watch && pfds[0].revents == 0 {
                continue;
            }
            let mut b = [0u8; 1];
            let n = unsafe { libc::read(self.fd, b.as_mut_ptr() as *mut libc::c_void, 1) };
            if n == 1 {
                return Ok(b[0]);
            }
            if n < 0 {
                let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if e == libc::EINTR || e == libc::EAGAIN {
                    if self.handle_signals() {
                        return Err(false);
                    }
                    continue;
                }
            }
            return Err(false);
        }
    }

    // Runs traps for signals that interrupted a read. True when the read
    // should stop: an untrapped interrupt, or a trap asking to exit.
    fn handle_signals(&mut self) -> bool {
        if !crate::trap::has_pending() {
            return false;
        }
        self.shell(crate::trap::run_pending);
        match crate::exec::flow() {
            crate::types::Flow::Interrupt => {
                crate::exec::set_flow(crate::types::Flow::None);
                self.errflag = true;
                true
            }
            crate::types::Flow::Exit(_) => true,
            _ => false,
        }
    }

    /// Reads a whole (possibly multibyte) character.
    pub fn getfullchar(&mut self, keytmout: bool) -> Option<char> {
        let b = self.getbyte(keytmout).ok()?;
        self.getrestchar(b)
    }

    /// Completes the character starting with byte `b`.
    pub fn getrestchar(&mut self, b: u8) -> Option<char> {
        self.lastchar_wide_valid = true;
        let need = utf8_len(b);
        let mut bytes = vec![b];
        while bytes.len() < need {
            match self.getbyte(true) {
                Ok(n) if n & 0xc0 == 0x80 => bytes.push(n),
                Ok(n) => {
                    self.ungetbyte(n);
                    self.lastchar_wide = Some('?');
                    return Some('?');
                }
                Err(_) => {
                    self.lastchar_wide = Some('?');
                    return Some('?');
                }
            }
        }
        let c = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|s| s.chars().next());
        self.lastchar_wide = c;
        c
    }

    fn getrestchar_keybuf(&mut self) -> Option<char> {
        self.lastchar_wide_valid = true;
        let mut i = self.keybuf.len();
        // Find the start of the last character in keybuf.
        while i > 0 && self.keybuf[i - 1] & 0xc0 == 0x80 {
            i -= 1;
        }
        let start = i.saturating_sub(1);
        let first = *self.keybuf.get(start)?;
        let need = utf8_len(first);
        while self.keybuf.len() - start < need {
            match self.getbyte(true) {
                Ok(b) => {
                    self.lastchar_wide_valid = true;
                    self.keybuf.push(b);
                }
                Err(true) => {
                    self.lastchar = b'?' as i32;
                    self.lastchar_wide = Some('?');
                    return Some('?');
                }
                Err(false) => {
                    self.lastchar_wide = None;
                    return None;
                }
            }
        }
        let c = std::str::from_utf8(&self.keybuf[start..])
            .ok()
            .and_then(|s| s.chars().next());
        self.lastchar_wide = c;
        c
    }

    /// Reads a key sequence bound in `km`, as zsh's getkeymapcmd.
    pub fn getkeymapcmd(&mut self, km: &KeymapRef) -> (Option<Bind>, bool) {
        let mut func: Option<Bind> = None;
        let mut lastlen = 0usize;
        let mut lastc = self.lastchar;
        let mut timeout = false;
        let mut csi = 0usize;
        let mut oscdcs = 0usize;
        self.keybuf.clear();
        while let Ok(c) = self.getbyte(timeout) {
            self.keybuf.push(c);
            let kb = self.keybuf.clone();
            let mut f: Option<Bind> = None;
            let mut ispfx = false;
            let mut loc = false;
            if let Some(lk) = &self.localkeymap {
                let lk = lk.borrow();
                f = lk.keybind(&kb).cloned();
                loc = f.is_some();
                ispfx = lk.is_prefix(&kb);
            }
            if !loc && !ispfx {
                f = km.borrow().keybind(&kb).cloned();
            }
            ispfx |= km.borrow().is_prefix(&kb);
            if let Some(fb) = &f {
                lastlen = self.keybuf.len();
                func = f.clone();
                lastc = self.lastchar;
                let vioper = match fb {
                    Bind::Widget(w) => thingy::widget_flags(w) & ZLE_VIOPER != 0,
                    Bind::Str(_) => false,
                };
                timeout = !(self.virangeflag == 0 && self.region_active == 0 && vioper);
                // A vi operator waits for its motion: show the
                // operator-pending cursor meanwhile.
                if !timeout {
                    let n = self.vichgflag;
                    self.vichgflag = 2;
                    termctl::cursor_form(self);
                    self.vichgflag = n;
                }
                let selfins = matches!(fb, Bind::Widget(w) if w == "self-insert" || w == "self-insert-unmeta" || w == ".self-insert");
                if selfins && !self.lastchar_wide_valid && !ispfx {
                    self.getrestchar_keybuf();
                    lastlen = self.keybuf.len();
                }
            }
            let n = self.keybuf.len();
            if csi == 0 && n >= 3 && self.keybuf[n - 3] == 0x1b && self.keybuf[n - 2] == b'[' {
                csi = n - 1;
            }
            if csi != 0 {
                let last = self.keybuf[n - 1];
                if !(0x20..=0x3f).contains(&last) {
                    if (0x40..=0x7e).contains(&last) && lastlen + 2 > csi && lastlen <= csi {
                        if self.keybuf[csi] == b'?' && (last == b'c' || last == b'u') {
                            self.keybuf.truncate(csi - 2);
                            timeout = false;
                            csi = 0;
                            continue;
                        }
                        func = Some(Bind::Widget("undefined-key".into()));
                        lastlen = n;
                    }
                    csi = 0;
                }
            }
            if oscdcs != 0 {
                let last = self.keybuf[n - 1];
                if last == 7 || (n >= 2 && self.keybuf[n - 2] == 0x1b && last == b'\\') {
                    self.keybuf.truncate(oscdcs - 2);
                    timeout = false;
                    oscdcs = 0;
                    continue;
                }
            } else if n >= 2
                && self.keybuf[n - 2] == 0x1b
                && (self.keybuf[n - 1] == b']' || self.keybuf[n - 1] == b'P')
            {
                oscdcs = n;
            }
            if !ispfx && csi == 0 && oscdcs == 0 {
                break;
            }
        }
        if lastlen == 0 && !self.keybuf.is_empty() {
            lastlen = self.keybuf.len();
        } else {
            self.lastchar = lastc;
        }
        if lastlen != self.keybuf.len() {
            let rest = self.keybuf.split_off(lastlen);
            self.ungetbytes(&rest);
            if self.vichgflag != 0 {
                let l = self.curvichg.buf.len().saturating_sub(rest.len());
                self.curvichg.buf.truncate(l);
            }
        }
        let empty = self.keybuf.is_empty();
        (func, empty)
    }

    /// Reads the next command: the widget name, None at end of input.
    pub fn getkeycmd(&mut self) -> Option<String> {
        let mut hops = 0;
        loop {
            let km = self.curkeymap.clone();
            let (bind, empty) = self.getkeymapcmd(&km);
            if empty {
                return None;
            }
            match bind {
                Some(Bind::Str(s)) => {
                    hops += 1;
                    if hops == 20 {
                        crate::shell::shell_err("string inserting another one too many times");
                        return None;
                    }
                    self.ungetbytes(&s);
                    continue;
                }
                Some(Bind::Widget(w)) => {
                    let mut f = w;
                    if f == "execute-named-cmd" && self.statusline.is_none() {
                        while f == "execute-named-cmd" {
                            match widgets::misc::executenamedcommand(self, "execute: ") {
                                Some(n) => f = n,
                                None => {
                                    f = "undefined-key".into();
                                    break;
                                }
                            }
                        }
                        if f != "execute-last-named-cmd" && f != "undefined-key" {
                            self.lastnamed = Some(f.clone());
                        }
                    }
                    if f == "execute-last-named-cmd" {
                        f = self
                            .lastnamed
                            .clone()
                            .unwrap_or_else(|| "undefined-key".into());
                    }
                    return Some(f);
                }
                None => return Some("undefined-key".into()),
            }
        }
    }

    pub fn ungetkeycmd(&mut self) {
        let kb = self.keybuf.clone();
        self.ungetbytes(&kb);
    }

    // ---- keymaps --------------------------------------------------------

    /// Selects keymap `name`; with `fallback`, .safe when it is missing.
    pub fn selectkeymap(&mut self, name: &str, fallback: bool) -> i32 {
        let mut name = name.to_string();
        let km = match keymap::open_keymap(&name) {
            Some(k) => k,
            None => {
                self.showmsg(&format!("No such keymap `{}'", keymap::nice(&name)));
                if !fallback {
                    return 1;
                }
                name = ".safe".into();
                keymap::open_keymap(".safe").unwrap()
            }
        };
        if name != self.curkeymapname {
            let old = std::mem::replace(&mut self.curkeymapname, name);
            self.curkeymap = km;
            if zle_active() && old != self.curkeymapname {
                self.callhook("zle-keymap-select", Some(&old));
            }
        } else {
            self.curkeymap = km;
        }
        0
    }

    pub fn reselectkeymap(&mut self) {
        let n = self.curkeymapname.clone();
        self.selectkeymap(&n, true);
    }

    pub fn selectlocalmap(&mut self, km: Option<KeymapRef>) {
        self.localkeymap = km;
    }

    // ---- widgets --------------------------------------------------------

    /// Runs widget `name`, as zsh's execzlefunc. Returns its status.
    pub fn execzlefunc(
        &mut self,
        name: &str,
        args: &[String],
        set_bindk: bool,
        set_lbindk: bool,
    ) -> i32 {
        let nestedvichg = self.vichgflag;
        let isrepeat = self.viinrepeat == 3;
        let save_bindk = self.bindk.clone();
        let save_lbindk = self.lbindk.clone();
        if set_bindk {
            self.bindk = Some(name.to_string());
        }
        if isrepeat {
            self.viinrepeat = 2;
        }
        let mut ran = false;
        let ret;
        match thingy::lookup(name) {
            None => {
                self.showmsg(&format!("No such widget `{}'", keymap::nice(name)));
                ret = self.execimmortal(name, args);
            }
            Some(w) => match w.kind.clone() {
                thingy::WidgetKind::Internal(func) => {
                    let wflags = w.flags.get();
                    let eof = self.keybuf.len() == 1
                        && self.keybuf[0] as i32 == eofchar()
                        && args.is_empty()
                        && self.line.is_empty()
                        && self.isfirstln
                        && self.zlereadflags & ZLRF_IGNOREEOF != 0;
                    if eof {
                        self.showmsg(if crate::options::is_set("login") {
                            "zsh: use 'logout' to logout."
                        } else {
                            "zsh: use 'exit' to exit."
                        });
                        self.eofsent = true;
                        ret = 1;
                    } else {
                        if wflags & ZLE_KEEPSUFFIX == 0 {
                            self.removesuffix();
                        }
                        if wflags & ZLE_MENUCMP == 0 {
                            self.fixsuffix();
                            self.invalidatelist();
                        }
                        if wflags & ZLE_LINEMOVE != 0 {
                            self.vilinerange = true;
                        }
                        if wflags & ZLE_LASTCOL == 0 {
                            self.lastcol = -1;
                        }
                        ret = match func {
                            Some(f) => f(self, args),
                            None => {
                                utils::handlefeep(self, &[]);
                                0
                            }
                        };
                        if wflags & ZLE_NOTCOMMAND == 0 {
                            self.lastcmd = wflags;
                        }
                    }
                    ran = true;
                }
                thingy::WidgetKind::Complete { builtin, func } => {
                    let wflags = w.flags.get();
                    if wflags & ZLE_KEEPSUFFIX == 0 {
                        self.removesuffix();
                    }
                    let atcurhist = self.histline == self.curhist;
                    ret = widgets::complete::completecall(self, &builtin, &func, args);
                    if atcurhist {
                        self.histline = self.curhist;
                    }
                    self.lastcmd = wflags;
                    ran = true;
                }
                thingy::WidgetKind::User(fname) => {
                    let exists = crate::shell::with_shell(|s| s.functions.contains_key(&fname));
                    if !exists {
                        self.showmsg(&format!(
                            "No such shell function `{}'",
                            keymap::nice(&fname)
                        ));
                        ret = self.execimmortal(name, args);
                    } else {
                        ret = self.call_user_widget(&fname, args);
                        if self.errflag {
                            self.errflag = false;
                            let r = self.execimmortal(name, args);
                            if r != 0 {
                                self.errflag = true;
                            }
                        }
                        self.lastcmd = w.flags.get() & !(WIDGET_INUSE | WIDGET_FREE);
                        ran = true;
                    }
                }
            },
        }
        if set_lbindk {
            self.lbindk = save_lbindk;
        } else if ran {
            self.lbindk = Some(name.to_string());
        }
        if set_bindk {
            self.bindk = save_bindk;
        }
        if self.cs > self.line.len() {
            self.cs = self.line.len();
        }
        if self.vichgflag == 2 && nestedvichg == 0 {
            if self.invicmdmode() {
                if ret == 0 {
                    self.lastvichg = std::mem::take(&mut self.curvichg);
                } else {
                    self.curvichg = ViChange::default();
                }
                self.vichgflag = 0;
            } else {
                self.vichgflag = 1;
            }
        }
        if isrepeat {
            self.viinrepeat = i32::from(!self.invicmdmode());
        }
        ret
    }

    fn execimmortal(&mut self, name: &str, args: &[String]) -> i32 {
        let dot = format!(".{}", name);
        if thingy::lookup(&dot).is_some() {
            self.execzlefunc(&dot, args, false, false)
        } else {
            1
        }
    }

    /// Calls shell function `fname` as a widget, with the ZLE special
    /// parameters set up around it.
    pub fn call_user_widget(&mut self, fname: &str, args: &[String]) -> i32 {
        let Some((body, line)) =
            crate::shell::with_shell(|s| s.functions.get(fname).map(|f| (f.body.clone(), f.line)))
        else {
            return 1;
        };
        // Standard input is the terminal's; widgets get /dev/null.
        let saved_in = unsafe { libc::dup(0) };
        let nul = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
        if nul >= 0 {
            unsafe {
                libc::dup2(nul, 0);
                libc::close(nul);
            }
        }
        let mut words = vec![fname.to_string()];
        words.extend(args.iter().cloned());
        let oxt = crate::options::is_set("xtrace");
        crate::shell::push_scope();
        params::make_params(self);
        crate::options::set("xtrace", false);
        self.widget_depth += 1;
        let ret = self.shell(|| {
            crate::shell::in_eval_context("shfunc", || {
                crate::exec::call_function(&body, &words, line)
            })
        });
        self.widget_depth -= 1;
        crate::options::set("xtrace", oxt);
        params::pull(self);
        crate::shell::pop_scope();
        params::end_scope();
        if self.widget_depth > 0 {
            params::push(self);
        }
        if saved_in >= 0 {
            unsafe {
                libc::dup2(saved_in, 0);
                libc::close(saved_in);
            }
        }
        if matches!(crate::exec::flow(), crate::types::Flow::Return(_)) {
            crate::exec::set_flow(crate::types::Flow::None);
        }
        ret
    }

    /// Calls hook widget `name` (zle-line-init and so on) if it exists.
    pub fn callhook(&mut self, name: &str, arg: Option<&str>) {
        if thingy::lookup(name).is_none() {
            return;
        }
        let saved = self.errflag;
        let args: Vec<String> = arg.map(|a| vec![a.to_string()]).unwrap_or_default();
        self.execzlefunc(name, &args, true, false);
        self.errflag = saved;
    }

    pub fn initmodifier(&mut self) {
        self.zmod = Modifier::new();
    }

    pub fn handleprefixes(&mut self) {
        if self.prefixflag {
            self.prefixflag = false;
            if self.zmod.flags & MOD_TMULT != 0 {
                self.zmod.flags |= MOD_MULT;
                self.zmod.mult = self.zmod.tmult;
            }
        } else {
            self.initmodifier();
        }
    }

    // ---- the read loop --------------------------------------------------

    pub fn run_core(&mut self) {
        self.zlecore();
    }

    fn zlecore(&mut self) {
        while !self.done && !self.errflag && !exit_pending() {
            self.statusline = None;
            self.vilinerange = false;
            self.reselectkeymap();
            let vis = if self.invicmdmode() && self.region_active != 0 {
                keymap::open_keymap("visual")
            } else {
                None
            };
            self.selectlocalmap(vis);
            let bindk = self.getkeycmd();
            self.selectlocalmap(None);
            match bindk {
                Some(b) => {
                    if self.line.is_empty()
                        && self.isfirstln
                        && self.zlereadflags & ZLRF_IGNOREEOF == 0
                        && self.lastchar == eofchar()
                    {
                        self.eofsent = true;
                        break;
                    }
                    self.bindk = Some(b.clone());
                    if self.execzlefunc(&b, &[], false, false) != 0 {
                        utils::handlefeep(self, &[]);
                        if self.eofsent {
                            break;
                        }
                    }
                    self.handleprefixes();
                    if self.invicmdmode()
                        && self.cs > self.findbol()
                        && (self.cs == self.line.len() || self.line[self.cs] == '\n')
                    {
                        self.cs -= 1;
                    }
                    self.handleundo();
                }
                None => {
                    self.errflag = true;
                    break;
                }
            }
            self.redrawhook();
            if self.unget.is_empty() {
                self.zrefresh();
            }
        }
    }

    fn redrawhook(&mut self) {
        if thingy::lookup("zle-line-pre-redraw").is_some() {
            let lastcmd = self.lastcmd;
            let saved = self.errflag;
            self.execzlefunc("zle-line-pre-redraw", &[], true, true);
            self.errflag = saved;
            self.lastcmd = lastcmd;
        }
    }

    pub fn redisplay_now(&mut self) {
        self.resetneeded = true;
        self.zrefresh();
    }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// The terminal's end-of-file character.
pub fn eofchar() -> i32 {
    EOFCHAR.with(|e| e.get())
}

thread_local! {
    static EOFCHAR: Cell<i32> = const { Cell::new(4) };
}

fn exit_pending() -> bool {
    matches!(crate::exec::flow(), crate::types::Flow::Exit(_))
}

fn set_raw(fd: i32) -> Option<libc::termios> {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return None;
        }
        let saved = t;
        EOFCHAR.with(|e| e.set(t.c_cc[libc::VEOF] as i32));
        let vdisable = libc::_POSIX_VDISABLE as libc::cc_t;
        if !crate::options::is_set("flowcontrol") {
            t.c_iflag &= !libc::IXON;
            t.c_cc[libc::VSTART] = vdisable;
            t.c_cc[libc::VSTOP] = vdisable;
        }
        t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::FLUSHO);
        t.c_oflag &= !libc::TABDLY;
        t.c_oflag |= libc::ONLCR;
        for c in [libc::VQUIT, libc::VDISCARD, libc::VSUSP, libc::VLNEXT] {
            t.c_cc[c] = vdisable;
        }
        // BSD and macOS also reserve ^Y (delayed suspend) in the line discipline.
        #[cfg(any(target_os = "macos", target_os = "freebsd"))]
        {
            t.c_cc[libc::VDSUSP] = vdisable;
        }
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        t.c_iflag |= libc::INLCR | libc::ICRNL;
        libc::tcsetattr(fd, libc::TCSADRAIN, &t);
        Some(saved)
    }
}

fn restore_term(fd: i32, t: &libc::termios) {
    unsafe {
        libc::tcsetattr(fd, libc::TCSADRAIN, t);
    }
}

thread_local! {
    static PENDING_INPUT: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Reads a line with the line editor, as zsh's zleread. None at end of
/// input or on interrupt (then `interrupted` is set).
pub fn zleread(
    lp: &str,
    rp: &str,
    flags: u32,
    context: i32,
    init: &str,
    finish: &str,
    initial: Option<&str>,
) -> ReadResult {
    let fd = refresh::tty_fd();
    let mut z = Zle::new(fd);
    // Type-ahead left over from the last line (as zsh's ungetbytes keeps it).
    z.unget = PENDING_INPUT.with(|p| p.take());
    z.keytimeout = crate::shell::var_get("KEYTIMEOUT")
        .trim()
        .parse()
        .unwrap_or(40);
    z.insmode = !crate::options::is_set("overstrike");
    z.lprompt = lp.to_string();
    z.rprompt = rp.to_string();
    z.zlereadflags = flags;
    z.zlecontext = context;
    widgets::hist::init_hist(&mut z);
    z.selectkeymap("main", true);
    if let Some(text) = initial.map(|s| s.to_string()).or_else(utils::take_bufstack) {
        z.setline(&text, ZSL_TOEND);
        if let Some(cs) = widgets::hist::take_stackcs() {
            z.cs = cs.min(z.line.len());
        }
    }
    z.initundo();
    if keymap::open_keymap("main")
        .zip(keymap::open_keymap("viins"))
        .is_some_and(|(a, b)| std::rc::Rc::ptr_eq(&a, &b))
    {
        widgets::vi::viinsert_init(&mut z);
    }
    // Terminal queries set their own modes, so this comes before raw mode.
    termctl::on_read_start(&mut z);
    struct ZleReadGuard {
        fd: i32,
        saved: Option<libc::termios>,
        editing: bool,
    }
    impl Drop for ZleReadGuard {
        fn drop(&mut self) {
            if self.editing {
                termctl::end_edit();
            }
            if let Some(t) = self.saved.take() {
                restore_term(self.fd, &t);
            }
            ZLE_ACTIVE.with(|a| a.set(false));
        }
    }

    z.saved_termios = set_raw(fd);
    let mut term_guard = ZleReadGuard {
        fd,
        saved: z.saved_termios,
        editing: false,
    };
    ZLE_ACTIVE.with(|a| a.set(true));
    if crate::options::is_set("promptcr") {
        refresh::write_tty("\r");
    }
    z.resetneeded = true;
    z.zrefresh();
    z.callhook(init, None);
    if !z.line.is_empty() {
        z.redrawhook();
    }
    termctl::start_edit();
    term_guard.editing = true;
    z.zrefresh();
    z.zlecore();
    let interrupted = z.errflag;
    if z.errflag {
        let name = if z.zlecontext == ZLCON_VARED {
            "ZLE_VARED_ABORTED"
        } else {
            "ZLE_LINE_ABORTED"
        };
        let l: String = z.line.iter().collect();
        crate::shell::var_set(name, &l, false);
    }
    termctl::end_edit();
    term_guard.editing = false;
    if z.done && !exit_pending() && !z.errflag {
        z.callhook(finish, None);
    }
    z.statusline = None;
    z.invalidatelist();
    z.trash();
    if let Some(t) = z.saved_termios.take() {
        term_guard.saved = None;
        restore_term(fd, &t);
    }
    drop(term_guard);
    PENDING_INPUT.with(|p| p.replace(std::mem::take(&mut z.unget)));
    if z.eofsent || z.errflag || exit_pending() {
        if interrupted {
            ReadResult::Interrupted
        } else {
            ReadResult::Eof
        }
    } else {
        let mut s: String = z.line.iter().collect();
        s.push('\n');
        ReadResult::Line(s)
    }
}

thread_local! {
    static PREBUFFER: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// The lines of a command already read before this one ($PREBUFFER).
pub fn prebuffer() -> String {
    PREBUFFER.with(|p| p.borrow().clone())
}

pub fn set_prebuffer(s: &str) {
    PREBUFFER.with(|p| *p.borrow_mut() = s.to_string());
}

pub enum ReadResult {
    Line(String),
    Eof,
    Interrupted,
}
