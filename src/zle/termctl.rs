//! Terminal queries and control sequences, after zsh's termquery.c and the
//! parts of zle_main.c and zle_refresh.c that emit them: cursor shape and
//! colour ($zle_cursorform, $CURSOR_FORM), bracketed paste and the other
//! edit extensions, OSC 133 prompt marks, OSC 7, the OSC 52 clipboard,
//! $POSTEDIT and the `zle -T tc` transformation.
//!
//! State that zsh keeps in statics lives here in thread-locals, since a
//! [`Zle`] lasts for one read only.

use super::refresh::{flush_tty, write_tty};
use super::{thingy, Zle, ZLCON_LINE_CONT};
use crate::shell::{array_get, array_set, var_get, var_get_opt, var_set};
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

const EXTVAR: &str = ".term.extensions";
const IDVAR: &str = ".term.id";
const VERVAR: &str = ".term.version";
const MODEVAR: &str = ".term.mode";
const WAITVAR: &str = ".term.querywait";

// Cursor form bits, as zle.h.
const CURF_UNDERLINE: u32 = 1;
const CURF_BAR: u32 = 2;
const CURF_BLOCK: u32 = 3;
const CURF_SHAPE_MASK: u32 = 3;
const CURF_BLINK: u32 = 1 << 2;
const CURF_STEADY: u32 = 1 << 3;
const CURF_HIDDEN: u32 = 1 << 4;
const CURF_COLOR: u32 = 1 << 5;
const CURF_COLOR_MASK: u32 = (0xffffff << 8) | CURF_COLOR;
const CURF_RED_SHIFT: u32 = 24;
const CURF_GREEN_SHIFT: u32 = 16;
const CURF_BLUE_SHIFT: u32 = 8;
const CURF_INVALID: u32 = 3 << 6;

// Cursor contexts, indexes into the forms table.
const CURC_EDIT: usize = 0;
const CURC_COMMAND: usize = 1;
const CURC_INSERT: usize = 2;
const CURC_OVERWRITE: usize = 3;
const CURC_PENDING: usize = 4;
const CURC_REGION_START: usize = 5;
const CURC_REGION_END: usize = 6;
const CURC_VISUAL: usize = 7;
const CURC_DEFAULT: usize = 8;
const CURC_CUSTOM: usize = 9;
const NFORMS: usize = 10;

thread_local! {
    static BOOTED: Cell<bool> = const { Cell::new(false) };
    // The terminal's own cursor colour, from the OSC 12 query.
    static MEMO_CURSOR: Cell<u32> = const { Cell::new(0) };
    static CUSTOM_FORM: RefCell<String> = const { RefCell::new(String::new()) };
    static CURSOR_SETUP: Cell<bool> = const { Cell::new(false) };
    static CURSOR_MASK: Cell<u32> = const { Cell::new(0) };
    // The cursor form last sent, and the bits changed from the defaults.
    static CURSOR_STATE: Cell<u32> = const { Cell::new(0) };
    static CURSOR_ALTERED: Cell<u32> = const { Cell::new(0) };
    static AID: RefCell<String> = const { RefCell::new(String::new()) };
}

// ---- entry points for zleread ------------------------------------------

/// Start of a read, before the first refresh: the first time, sets
/// $zle_bracketed_paste and queries the terminal (zsh does both when the
/// zle module loads); then writes the OSC 133 mark that precedes a prompt.
pub fn on_read_start(z: &mut Zle) {
    if !BOOTED.with(|b| b.replace(true)) {
        array_set(
            "zle_bracketed_paste",
            vec!["\x1b[?2004h".into(), "\x1b[?2004l".into()],
        );
        // SAFETY: isatty only inspects the descriptor.
        if unsafe { libc::isatty(z.fd) } == 1 {
            query_terminal(z);
        }
    }
    if extension_enabled("integration", "prompt", true) {
        write_tty(&prompt_start_marker());
        flush_tty();
    }
}

/// Sequences that start editing (bracketed paste, OSC 133;B, ...). Called
/// after zle-line-init, before the second refresh.
pub fn start_edit() {
    collate_seq(false);
}

/// Sequences that end editing, in reverse order. Called once the editing
/// loop ends, before zle-line-finish.
pub fn end_edit() {
    collate_seq(true);
}

/// The mark for the left prompt (OSC 133;P), written with the prompt on a
/// full redraw only. Empty when disabled or there is no prompt.
pub fn lprompt_marker(z: &Zle) -> &'static str {
    if z.lprompt.is_empty() || !extension_enabled("integration", "prompt", true) {
        ""
    } else if z.zlecontext == ZLCON_LINE_CONT {
        "\x1b]133;P;k=s\x1b\\"
    } else {
        "\x1b]133;P;k=i\x1b\\"
    }
}

/// The mark for the right prompt, written with it on a full redraw only.
pub fn rprompt_marker(z: &Zle) -> &'static str {
    if z.rprompt.is_empty() || !extension_enabled("integration", "prompt", true) {
        ""
    } else {
        "\x1b]133;P;k=r\x1b\\"
    }
}

/// zsh's trashzle: puts the cursor back to the terminal's defaults and
/// forgets $CURSOR_FORM. Call after setting `z.trashed`, before moving
/// below the display.
pub fn on_trash(z: &mut Zle) {
    CUSTOM_FORM.with(|c| c.borrow_mut().clear());
    update_cursor(z, true);
}

/// Queues $POSTEDIT, as the end of zsh's trashzle.
pub fn write_postedit() {
    if let Some(p) = var_get_opt("POSTEDIT") {
        write_tty(&p);
    }
}

/// OSC 133;C before a command line runs (`start`) and ;D after it.
pub fn mark_output(start: bool) {
    if BOOTED.with(|b| b.get()) && extension_enabled("integration", "output", true) {
        write_tty(if start {
            "\x1b]133;C\x1b\\"
        } else {
            "\x1b]133;D\x1b\\"
        });
        flush_tty();
    }
}

/// OSC 7 with the current directory, after a change of directory.
pub fn notify_pwd() {
    if BOOTED.with(|b| b.get()) {
        write_pwd();
    }
}

// ---- zle -T tc ---------------------------------------------------------

/// Output for termcap capability `cap` through the `zle -T tc` function:
/// None when there is no such function (use the usual sequence), else
/// what it left in $REPLY.
pub fn tcout(cap: &str, arg: Option<i32>) -> Option<String> {
    let fname = thingy::tc_func()?;
    let func =
        crate::shell::with_shell(|s| s.functions.get(&fname).map(|f| (f.body.clone(), f.line)));
    let Some((body, line)) = func else {
        return Some(String::new());
    };
    let mut words = vec![fname, cap.to_string()];
    if let Some(a) = arg {
        words.push(a.to_string());
    }
    let status = crate::shell::with_shell(|s| s.last_status);
    crate::exec::call_function(&body, &words, line);
    if matches!(crate::exec::flow(), crate::types::Flow::Return(_)) {
        crate::exec::set_flow(crate::types::Flow::None);
    }
    crate::shell::with_shell(|s| s.last_status = status);
    Some(var_get_opt("REPLY").unwrap_or_default())
}

/// [`tcout`], falling back to `default`.
pub fn tcout_or(cap: &str, arg: Option<i32>, default: &str) -> String {
    tcout(cap, arg).unwrap_or_else(|| default.to_string())
}

// ---- cursor form -------------------------------------------------------

/// zsh's cursor_form: sends what changes the cursor to the form for the
/// current context. Called at the end of a refresh and when the context
/// changes without one.
pub fn cursor_form(z: &mut Zle) {
    update_cursor(z, false);
}

/// After $CURSOR_FORM is assigned or a local one goes out of scope.
pub fn cursor_form_param_changed() {
    let name = var_get_opt("CURSOR_FORM").unwrap_or_default();
    CUSTOM_FORM.with(|c| *c.borrow_mut() = name);
    crate::zle::with_active(cursor_form);
}

fn match_cursorform(spec: &str) -> u32 {
    const SHAPES: [(&str, u32, u32); 7] = [
        ("none", 0, 0xff),
        ("underline", CURF_UNDERLINE, CURF_SHAPE_MASK),
        ("bar", CURF_BAR, CURF_SHAPE_MASK),
        ("block", CURF_BLOCK, CURF_SHAPE_MASK),
        ("blink", CURF_BLINK, CURF_STEADY),
        ("steady", CURF_STEADY, CURF_BLINK),
        ("hidden", CURF_HIDDEN, 0),
    ];
    let s = spec.as_bytes();
    let mut form = 0u32;
    let mut i = 0;
    while i < s.len() {
        let mut found = false;
        if s[i..].starts_with(b"color=#") {
            i += 7;
            let digits = s[i..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
            let col = std::str::from_utf8(&s[i..i + digits])
                .ok()
                .and_then(|h| u64::from_str_radix(h, 16).ok())
                .unwrap_or(0);
            if digits == 4 {
                let red = (col >> 8) as u32;
                let green = ((col & 0xf0) >> 4) as u32;
                let blue = (col & 0xf) as u32;
                form &= 0xff;
                form |= CURF_COLOR
                    | (((red << 4) | red) << CURF_RED_SHIFT)
                    | (((green << 4) | green) << CURF_GREEN_SHIFT)
                    | (((blue << 4) | blue) << CURF_BLUE_SHIFT);
                found = true;
            } else if digits == 6 {
                form |= ((col << 8) as u32) | CURF_COLOR;
                found = true;
            }
            i += digits;
        }
        if !found {
            for (name, value, mask) in SHAPES {
                if s[i..].starts_with(name.as_bytes()) {
                    i += name.len();
                    form &= !mask;
                    form |= value;
                    found = true;
                    break;
                }
            }
        }
        if !found {
            match s[i..].iter().position(|&c| c == b',') {
                Some(p) => i += p,
                None => break,
            }
        }
        if i >= s.len() || s[i] != b',' {
            break;
        }
        i += 1;
    }
    form
}

/// zsh's zle_set_cursorform: the form for each context from
/// $zle_cursorform; also which kinds of change the terminal gets.
fn cursor_forms(trashed: bool) -> [u32; NFORMS] {
    const CONTEXTS: [&str; 9] = [
        "edit:",
        "command:",
        "insert:",
        "overwrite:",
        "pending:",
        "regionstart:",
        "regionend:",
        "visual:",
        "base:",
    ];
    let mut forms = [0u32; NFORMS];
    forms[CURC_INSERT] = CURF_BAR;
    forms[CURC_PENDING] = CURF_UNDERLINE;
    forms[CURC_DEFAULT] = CURF_STEADY;
    forms[CURC_CUSTOM] = CURF_INVALID;
    let custom = CUSTOM_FORM.with(|c| c.borrow().clone());
    for atr in array_get("zle_cursorform").unwrap_or_default() {
        if atr == "none" {
            forms = [0; NFORMS];
            continue;
        }
        if let Some(spec) = atr.strip_prefix("region:") {
            forms[CURC_REGION_END] = match_cursorform(spec);
            forms[CURC_REGION_START] = forms[CURC_REGION_END];
            continue;
        }
        if let Some(spec) = atr
            .strip_prefix(custom.as_str())
            .filter(|_| !custom.is_empty())
            .and_then(|r| r.strip_prefix(':'))
        {
            forms[CURC_CUSTOM] = match_cursorform(spec);
            continue;
        }
        if let Some(i) = CONTEXTS.iter().position(|c| atr.starts_with(c)) {
            forms[i] = match_cursorform(&atr[CONTEXTS[i].len()..]);
        }
    }
    // The extensions are read once per read, so they apply from the next.
    if trashed {
        CURSOR_SETUP.with(|s| s.set(false));
    } else if !CURSOR_SETUP.with(|s| s.replace(true)) {
        let mut mask = CURF_HIDDEN;
        if extension_enabled("cursor", "shape", true) {
            mask |= CURF_SHAPE_MASK | CURF_BLINK | CURF_STEADY;
        }
        if extension_enabled("cursor", "color", true) {
            mask |= CURF_COLOR_MASK;
        }
        CURSOR_MASK.with(|m| m.set(mask));
    }
    forms
}

fn update_cursor(z: &mut Zle, trashed: bool) {
    let forms = cursor_forms(trashed);
    let context = if trashed {
        CURC_DEFAULT
    } else if forms[CURC_CUSTOM] != CURF_INVALID {
        CURC_CUSTOM
    } else if !z.insmode {
        CURC_OVERWRITE
    } else if z.vichgflag == 2 {
        CURC_PENDING
    } else if z.region_active != 0 {
        if z.invicmdmode() {
            CURC_VISUAL
        } else if z.mark > z.cs {
            CURC_REGION_START
        } else {
            CURC_REGION_END
        }
    } else if z.invicmdmode() {
        CURC_COMMAND
    } else if z.vichgflag != 0 {
        CURC_INSERT
    } else {
        CURC_EDIT
    };

    // reset: bits going back to the defaults.
    let mut reset = CURF_INVALID;
    let mut want = 0u32;
    if context != CURC_DEFAULT {
        want = forms[context];
    }
    for ex in [
        CURF_HIDDEN,
        CURF_SHAPE_MASK,
        CURF_BLINK | CURF_STEADY,
        CURF_COLOR_MASK,
    ] {
        if want & ex == 0 {
            want |= forms[CURC_DEFAULT] & ex;
            reset |= ex;
        }
    }
    let mut resetcolor = false;
    if want & CURF_COLOR == 0 && reset & CURF_COLOR != 0 {
        want |= MEMO_CURSOR.with(|m| m.get());
        resetcolor = true;
    }

    let mut state = CURSOR_STATE.with(|s| s.get());
    let altered = CURSOR_ALTERED.with(|a| a.get());
    let mask = CURSOR_MASK.with(|m| m.get());
    // Bits still at the terminal's initial defaults need no reset.
    let mut changed = (want ^ state) & mask & !(reset & !altered);
    if changed == 0 {
        return;
    }

    // Setting the shape or showing the cursor also sets flashing.
    if changed & ((state & CURF_HIDDEN) | CURF_SHAPE_MASK) != 0 {
        changed |= CURF_BLINK | CURF_STEADY;
        if want & (CURF_BLINK | CURF_STEADY) == 0 {
            want |= forms[CURC_DEFAULT] & (CURF_BLINK | CURF_STEADY);
            reset |= CURF_BLINK | CURF_STEADY;
        }
    }

    let mut out = String::new();
    if changed & CURF_HIDDEN != 0 {
        // Shape and colour are still set when hiding, for terminals that
        // cannot hide the cursor.
        out.push_str(&if want & CURF_HIDDEN != 0 {
            tcout_or("vi", None, "\x1b[?25l")
        } else {
            tcout_or("ve", None, "\x1b[?12l\x1b[?25h")
        });
        state = (state & !CURF_HIDDEN) | (want & CURF_HIDDEN);
    }
    if changed & CURF_SHAPE_MASK != 0 {
        let blink = u8::from(want & CURF_BLINK != 0);
        let c = match want & CURF_SHAPE_MASK {
            CURF_BAR => b'6' - blink,
            CURF_UNDERLINE => b'4' - blink,
            CURF_BLOCK => b'2' - blink,
            _ => b'0',
        };
        if want & CURF_SHAPE_MASK != 0 {
            // A shape sequence selects flashing too.
            changed &= !(CURF_BLINK | CURF_STEADY);
        }
        out.push_str(&format!("\x1b[{} q", c as char));
        let bits = CURF_SHAPE_MASK | CURF_BLINK | CURF_STEADY;
        state = (state & !bits) | (want & bits);
    }
    if changed & want & (CURF_BLINK | CURF_STEADY) != 0 {
        out.push_str(if want & CURF_BLINK != 0 {
            "\x1b[?12h"
        } else {
            "\x1b[?12l"
        });
        let bits = CURF_BLINK | CURF_STEADY;
        state = (state & !bits) | (want & bits);
    }
    if changed & CURF_COLOR_MASK != 0 {
        if want & CURF_COLOR != 0 {
            out.push_str(&format!(
                "\x1b]12;rgb:{:02x}00/{:02x}00/{:02x}00\x1b\\",
                want >> CURF_RED_SHIFT,
                (want >> CURF_GREEN_SHIFT) & 0xff,
                (want >> CURF_BLUE_SHIFT) & 0xff
            ));
        }
        state &= !CURF_COLOR_MASK;
        if resetcolor {
            // The semicolon is for urxvt, which otherwise leaves the
            // final backslash unconsumed.
            out.push_str("\x1b]112;\x1b\\");
        } else {
            state |= want & CURF_COLOR_MASK;
        }
    }
    if !out.is_empty() {
        write_tty(&out);
        flush_tty();
    }
    CURSOR_STATE.with(|s| s.set(state));
    CURSOR_ALTERED.with(|a| a.set(!reset));
}

// ---- extensions --------------------------------------------------------

/// Whether `prefix` or `prefix-ext` is on in .term.extensions; the first
/// matching element decides, a leading `-` turning it off.
fn extension_enabled(prefix: &str, ext: &str, def: bool) -> bool {
    for e in array_get(EXTVAR).unwrap_or_default() {
        let negate = e.starts_with('-');
        let Some(rest) = e[usize::from(negate)..].strip_prefix(prefix) else {
            continue;
        };
        if rest.is_empty() || rest.strip_prefix('-') == Some(ext) {
            return !negate;
        }
    }
    def
}

fn collate_seq(end: bool) {
    // (name, [start, end] sequences, length of the shared prefix, on by default)
    const EDITEXT: [(&str, [Option<&str>; 2], usize, bool); 3] = [
        ("bracketed-paste", [None, None], 0, true),
        (
            "integration-prompt",
            [Some("\x1b]133;B\x1b\\"), None],
            11,
            true,
        ),
        (
            "modkeys-xterm",
            [Some("\x1b[>4;1m"), Some("\x1b[>4m")],
            7,
            false,
        ),
    ];
    let sindex = usize::from(end);
    let elist = array_get(EXTVAR).unwrap_or_default();
    let order: Vec<usize> = if end {
        (0..EDITEXT.len()).rev().collect()
    } else {
        (0..EDITEXT.len()).collect()
    };
    let mut out = String::new();
    for i in order {
        let (key, seq, prefix, on) = EDITEXT[i];
        if i != 0 && seq[sindex].is_none() {
            continue;
        }
        let mut enabled = on;
        for e in &elist {
            let negate = e.starts_with('-');
            if negate != enabled {
                continue;
            }
            let body = &e[usize::from(negate)..];
            if body.starts_with(&key[..prefix])
                && ((prefix != 0 && body.len() == prefix) || body[prefix..] == key[prefix..])
            {
                enabled = !negate;
                break;
            }
        }
        if !enabled {
            continue;
        }
        if let Some(s) = seq[sindex] {
            out.push_str(s);
        } else if let Some(b) = array_get("zle_bracketed_paste").filter(|b| b.len() == 2) {
            out.push_str(&b[sindex]);
        }
    }
    if !out.is_empty() {
        write_tty(&out);
        flush_tty();
    }
}

fn hasher(s: &str) -> u32 {
    s.bytes().fold(0u32, |h, c| {
        h.wrapping_add((h << 5).wrapping_add(u32::from(c)))
    })
}

fn prompt_start_marker() -> String {
    let mut aid = AID.with(|a| a.borrow().clone());
    if aid.is_empty() {
        // Host name and pid should identify a shell instance.
        let mut h = var_get_opt("HOST").map_or(0, |h| hasher(&h)) ^ std::process::id();
        if h == 0 {
            h = 1;
        }
        aid = base64_encode(&h.to_ne_bytes())[..6].to_string();
        AID.with(|a| *a.borrow_mut() = aid.clone());
    }
    format!("\x1b]133;A;cl=m;aid=z{}\x1b\\", aid)
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn write_pwd() {
    if !extension_enabled("integration", "pwd", true) {
        return;
    }
    let Some(host) = var_get_opt("HOST").filter(|h| !h.contains('/')) else {
        return;
    };
    write_tty(&format!(
        "\x1b]7;file://{}{}\x1b\\",
        url_encode(&host),
        url_encode(&var_get("PWD"))
    ));
    flush_tty();
}

// ---- clipboard ---------------------------------------------------------

/// Sends `content` to clipboard `clip` ('c' clipboard, 'p' primary) with
/// OSC 52, for a cut to the "+ or "* register.
pub fn clip_put(clip: char, content: &str) {
    write_tty(&format!(
        "\x1b]52;{};{}\x1b\\",
        clip,
        base64_encode(content.as_bytes())
    ));
}

/// Asks the terminal for clipboard `clip`, for a put from "+ or "*.
pub fn clip_get(z: &mut Zle, clip: char) -> Option<String> {
    let mut contents = None;
    probe(
        z,
        &format!("\x1b]52;{};?\x1b\\", clip),
        &osc52_states(),
        |_, _, cap: &[u8]| contents = Some(base64_decode(cap)),
    );
    contents
}

fn base64_encode(src: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(4 * src.len().div_ceil(3));
    for chunk in src.chunks(3) {
        let byte = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let n = (byte(0) << 16) | (byte(1) << 8) | byte(2);
        let sextet = |shift: u32| char::from(TABLE[((n >> shift) & 0x3f) as usize]);
        out.push(sextet(18));
        out.push(sextet(12));
        out.push(if chunk.len() > 1 { sextet(6) } else { '=' });
        out.push(if chunk.len() > 2 { sextet(0) } else { '=' });
    }
    out
}

fn base64_decode(src: &[u8]) -> String {
    let mut buf = vec![0u8; 3 * src.len() / 4 + 2];
    let mut b = 0;
    let mut i = 0;
    while i < src.len() && src[i] != b'=' {
        let c = src[i];
        let n = u32::from(match c {
            b'0'..=b'9' => c - b'0' + 52,
            b'a'..=b'z' => c - b'a' + 26,
            b'A'..=b'Z' => c - b'A',
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        });
        if i % 4 != 0 {
            buf[b] |= (n >> (2 * (3 - i % 4))) as u8;
            b += 1;
        }
        i += 1;
        if i >= src.len() {
            break;
        }
        buf[b] = (n << (2 * (i % 4))) as u8;
    }
    let end = buf.iter().position(|&x| x == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

// ---- terminal queries --------------------------------------------------

const TAG: u8 = 1 << 7;
const SEQ: u8 = TAG | (1 << 6);

// Tags in a state table.
const T_BEGIN: u8 = 0x80; // group start
const T_END: u8 = 0x81; // group end
const T_OR: u8 = 0x82; // alternation within a group
const T_REPEAT: u8 = 0x83; // repeat the preceding group
const T_NUM: u8 = 0x84; // decimal number, 0 if absent
const T_HEX: u8 = 0x85; // hex digit, part of a number
const T_HEXCH: u8 = 0x86; // hex digit, thrown away
const T_WILDCARD: u8 = 0x87; // any character
const T_RECORD: u8 = 0x88; // start of captured text
const T_CAPTURE: u8 = 0x89; // end of captured text
const T_DROP: u8 = 0x91; // drop the input and restart, matching nothing
const T_CONTINUE: u8 = 0x92; // after a match, do not restart
const T_NEXT: u8 = 0x94; // move on to the next number

// The action bits each of those three leaves in the collected actions.
const A_DROP: u8 = T_DROP & 7;
const A_CONTINUE: u8 = T_CONTINUE & 7;
const A_NEXT: u8 = T_NEXT & 7;

// Sequence completion tags: they precede the final character.
const S_DA: u8 = SEQ;
const S_COLOR: u8 = SEQ | 1;
const S_KITTY: u8 = SEQ | 2;
const S_TINFO: u8 = SEQ | 3;
const S_XTID: u8 = SEQ | 4;
const S_XTVER: u8 = SEQ | 5;
const S_CLIP: u8 = SEQ | 6;

fn sl(b: &[u8]) -> &[u8] {
    b
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn either(alts: &[&[u8]]) -> Vec<u8> {
    let mut v = vec![T_BEGIN];
    for (i, a) in alts.iter().enumerate() {
        if i > 0 {
            v.push(T_OR);
        }
        v.extend_from_slice(a);
    }
    v.push(T_END);
    v
}

fn opt(o: &[u8]) -> Vec<u8> {
    either(&[o, sl(&either(&[]))])
}

fn repeat(alts: &[&[u8]]) -> Vec<u8> {
    let mut v = either(alts);
    v.push(T_REPEAT);
    v
}

/// Ends a state table with the NULs zsh's C strings have, so lookahead
/// past the last state stays in bounds.
fn terminate(mut v: Vec<u8>) -> Vec<u8> {
    v.extend_from_slice(&[0, 0]);
    v
}

/// The automaton for responses to the queries. Alternatives are decided
/// by their first character, with no backtracking.
fn query_states() -> Vec<u8> {
    let part = cat(&[
        sl(&[T_HEX]),
        sl(&opt(&[T_HEX])),
        sl(&opt(&[T_HEXCH, T_HEXCH])),
    ]);
    let rgb = cat(&[
        sl(b"rgb:"),
        sl(&part),
        sl(b"/"),
        sl(&part),
        sl(b"/"),
        sl(&part),
    ]);
    let hash = [
        b'#', T_HEX, T_HEX, T_NEXT, T_HEX, T_HEX, T_NEXT, T_HEX, T_HEX,
    ];
    let colour = cat(&[
        sl(b"]1"),
        sl(&[T_NUM]),
        sl(b";"),
        sl(&either(&[sl(&rgb), sl(&hash)])),
        // urxvt 9.31 omits the backslash of ST.
        sl(&either(&[
            sl(&[S_COLOR, T_CONTINUE, 0x1b, T_DROP, b'\\']),
            sl(&[S_COLOR, 0x07]),
        ])),
    ]);
    let xtversion = cat(&[
        sl(b">|"),
        sl(&[T_RECORD]),
        sl(&repeat(&[
            sl(&cat(&[
                sl(&[T_CAPTURE]),
                sl(&either(&[
                    sl(&[S_XTID, T_CONTINUE, b'(']),
                    sl(&[S_XTID, T_CONTINUE, b' ']),
                ])),
                sl(&[T_RECORD]),
            ])),
            sl(&cat(&[
                sl(&[T_CAPTURE]),
                sl(&opt(b")")),
                sl(&[0x1b, S_XTVER, b'\\']),
            ])),
            sl(&[T_WILDCARD]),
        ])),
    ]);
    let truecolor = either(&[
        // mlterm answers without the name; kitty's 0 means no.
        sl(&cat(&[
            sl(b"0+r"),
            sl(&opt(b"524742")),
            sl(&[0x1b, T_DROP, b'\\']),
        ])),
        sl(&cat(&[
            sl(b"1+r524742="),
            sl(&repeat(&[sl(&[T_HEXCH, T_HEXCH])])),
            sl(&[0x1b, S_TINFO, b'\\']),
        ])),
    ]);
    let dcs = cat(&[sl(b"P"), sl(&either(&[sl(&xtversion), sl(&truecolor)]))]);
    let csi = cat(&[
        sl(b"[?"),
        sl(&repeat(&[sl(&cat(&[
            sl(&[T_NUM]),
            sl(&either(&[
                sl(b";"),
                sl(&[S_KITTY, b'u']),
                sl(&[S_DA, b'c']),
            ])),
        ]))])),
    ]);
    terminate(cat(&[
        sl(b"\x1b"),
        sl(&either(&[sl(&colour), sl(&dcs), sl(&csi)])),
    ]))
}

fn osc52_states() -> Vec<u8> {
    terminate(cat(&[
        sl(b"\x1b]52;"),
        sl(&either(&[sl(b"p"), sl(b"c")])),
        sl(b";"),
        sl(&[T_RECORD]),
        sl(&repeat(&[
            sl(&cat(&[
                sl(&[T_CAPTURE]),
                sl(&either(&[sl(&[0x1b, S_CLIP, b'\\']), sl(&[S_CLIP, 0x07])])),
            ])),
            sl(&[T_WILDCARD]),
        ])),
    ]))
}

fn find_branch(states: &[u8], pos: usize) -> usize {
    let mut nested = 0;
    let mut cur = pos + 1;
    loop {
        let s = states.get(cur).copied().unwrap_or(0);
        if s == 0 || (nested == 0 && (s == T_END || s == T_OR)) {
            return cur;
        }
        if s == T_BEGIN {
            nested += 1;
        } else if s == T_END {
            nested -= 1;
        }
        cur += 1;
    }
}

fn find_matching(states: &[u8], pos: usize, dir: isize) -> usize {
    let mut nested: isize = 1;
    let mut cur = pos as isize + dir;
    while cur >= 0 && nested != 0 {
        let s = states.get(cur as usize).copied().unwrap_or(0);
        if s == 0 {
            break;
        }
        if s == T_BEGIN {
            nested += dir;
            if nested == 0 {
                // Going backward, stop on the group start.
                break;
            }
        } else if s == T_END {
            nested -= dir;
        }
        cur += dir;
    }
    cur.max(0) as usize
}

fn hex_value(c: u8) -> Option<i32> {
    char::from(c).to_digit(16).map(|d| d as i32)
}

/// Milliseconds to wait for each byte of a response: .term.querywait in
/// hundredths of a second, else about half a second, enough on a 9600
/// baud line. Only a terminal that ignores device attributes costs it.
fn query_timeout_ms(z: &Zle) -> i32 {
    let t: i64 = match var_get_opt(WAITVAR) {
        Some(v) => -1 - v.trim().parse::<i64>().unwrap_or(0),
        None => -51,
    };
    if t == -1 {
        i32::MAX
    } else if t < 0 {
        t.saturating_neg()
            .saturating_mul(10)
            .min(i64::from(i32::MAX)) as i32
    } else {
        (z.keytimeout.max(0) * 10).min(i64::from(i32::MAX)) as i32
    }
}

fn read_byte(z: &mut Zle, ms: i32) -> Option<u8> {
    if let Some(b) = z.unget.pop() {
        return Some(b);
    }
    let deadline = Instant::now() + Duration::from_millis(ms.max(0) as u64);
    loop {
        let left = deadline
            .saturating_duration_since(Instant::now())
            .as_millis();
        if left == 0 {
            return None;
        }
        let mut pfd = libc::pollfd {
            fd: z.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let r = unsafe { libc::poll(&mut pfd, 1, left.min(i32::MAX as u128) as i32) };
        let errno = || std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if r < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            return None;
        }
        if r == 0 {
            return None;
        }
        let mut b = [0u8; 1];
        // SAFETY: reads one byte into a one-byte buffer.
        let n = unsafe { libc::read(z.fd, b.as_mut_ptr().cast(), 1) };
        if n == 1 {
            return Some(b[0]);
        }
        if n < 0 && matches!(errno(), libc::EINTR | libc::EAGAIN) {
            continue;
        }
        return None;
    }
}

fn cbreak(fd: i32) -> Option<libc::termios> {
    // SAFETY: termios calls on the terminal descriptor with a zeroed,
    // then filled, struct.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return None;
        }
        let saved = t;
        t.c_lflag &= !(libc::ECHO | libc::ICANON);
        // Bytes pushed back skip the editor's CR/NL swap, so read them as
        // typed.
        t.c_iflag &= !(libc::ICRNL | libc::INLCR);
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(fd, libc::TCSADRAIN, &t);
        Some(saved)
    }
}

/// Sends `query` and parses responses with the automaton `states`,
/// passing each completed sequence to `handle` as (sequence number,
/// numbers, captured text). Stops at device attributes, end of input or
/// a timeout; other input read meanwhile is pushed back as type-ahead.
fn probe(z: &mut Zle, query: &str, states: &[u8], mut handle: impl FnMut(u8, &[i32], &[u8])) {
    let at = |i: usize| states.get(i).copied().unwrap_or(0);
    let ms = query_timeout_ms(z);
    struct CbreakGuard {
        fd: i32,
        saved: Option<libc::termios>,
    }
    impl Drop for CbreakGuard {
        fn drop(&mut self) {
            if let Some(t) = self.saved.take() {
                // SAFETY: restores the settings read in cbreak().
                unsafe {
                    libc::tcsetattr(self.fd, libc::TCSADRAIN, &t);
                }
            }
        }
    }
    let _cbreak_guard = CbreakGuard {
        fd: z.fd,
        saved: cbreak(z.fd),
    };
    write_tty(query);
    flush_tty();
    write_pwd();

    let mut buf: Vec<u8> = Vec::new();
    let (mut start, mut current, mut illgotten) = (0usize, 0usize, 0usize);
    let (mut record, mut capture) = (0usize, 0usize);
    let mut numbers = vec![0i32; 16];
    let mut num = 0usize;
    let mut finish = false;
    let mut number = false;
    let mut cur = 0usize;

    while !finish && at(cur) != 0 {
        let mut consumed = false;
        let mut branches = 1i32;
        let mut triedstart = cur == 0;
        let mut action = 0u8;
        let mut sequence = 0u8;
        let ch = if illgotten < current {
            illgotten += 1;
            buf[illgotten - 1]
        } else {
            let Some(b) = read_byte(z, ms) else {
                break;
            };
            buf.truncate(current);
            buf.push(b);
            current += 1;
            illgotten = current;
            b
        };

        while !consumed && branches >= 1 && at(cur) != 0 {
            let mut increment = 0i32;
            let mut base = 1i32;
            loop {
                let s = at(cur);
                let tryhere = match s {
                    T_BEGIN => {
                        branches += 1;
                        cur += 1;
                        false
                    }
                    T_END => {
                        branches -= 1;
                        sequence = 0;
                        action = 0;
                        cur += 1;
                        false
                    }
                    T_OR => {
                        cur = find_matching(states, cur, 1);
                        false
                    }
                    T_REPEAT => {
                        sequence = 0;
                        action = 0;
                        if branches > 1 {
                            branches -= 1;
                            cur += 1;
                        } else {
                            branches += 1;
                            cur = find_matching(states, cur - 1, -1);
                        }
                        false
                    }
                    T_NUM => {
                        let digit = ch.is_ascii_digit();
                        if !digit {
                            cur += 1;
                        }
                        digit
                    }
                    T_RECORD => {
                        record = current - 1;
                        cur += 1;
                        false
                    }
                    T_CAPTURE => {
                        capture = current - 1;
                        cur += 1;
                        false
                    }
                    T_DROP | T_CONTINUE | T_NEXT => {
                        action |= s;
                        cur += 1;
                        false
                    }
                    _ if s & SEQ == SEQ => {
                        sequence = s;
                        cur += 1;
                        false
                    }
                    _ => true,
                };
                if tryhere {
                    break;
                }
            }

            match at(cur) {
                T_HEX => {
                    if let Some(v) = hex_value(ch) {
                        increment = v;
                        consumed = true;
                        number = true;
                        base = 16;
                        if action & A_NEXT != 0 {
                            num += 1;
                        }
                    }
                }
                T_HEXCH => {
                    consumed = hex_value(ch).is_some();
                    if consumed && number {
                        num += 1;
                        number = false;
                    }
                }
                T_NUM => {
                    if ch.is_ascii_digit() {
                        increment = i32::from(ch - b'0');
                        base = 10;
                        consumed = true;
                        number = true;
                        // Step back so the number can repeat.
                        cur -= 1;
                    }
                }
                T_WILDCARD => {
                    consumed = true;
                    if number {
                        num += 1;
                        number = false;
                    }
                }
                s => {
                    if s & TAG == 0 {
                        consumed = s == ch;
                        if consumed && number {
                            num += 1;
                            number = false;
                        }
                    }
                }
            }
            if num == numbers.len() {
                numbers.resize(numbers.len() * 2, 0);
            }
            if number {
                numbers[num] = numbers[num].wrapping_mul(base).wrapping_add(increment);
            }

            // No match: on to the next alternative.
            if !consumed && branches > 1 {
                sequence = 0;
                action = 0;
                while branches > 1 {
                    cur = find_branch(states, cur);
                    if at(cur) == T_OR {
                        cur += 1;
                        break;
                    }
                    // A repeated group can match zero times.
                    if at(cur + 1) == T_REPEAT {
                        break;
                    }
                    branches -= 1;
                }
            }
            // Retry the character from the start if it is the only one
            // buffered and was tried from a later state.
            if !consumed && branches <= 1 {
                if triedstart || start + 1 != current {
                    break;
                }
                branches = 1;
                cur = 0;
                triedstart = true;
                numbers.fill(0);
                num = 0;
                sequence = 0;
                action = 0;
                number = false;
            }
        }

        if !consumed {
            start += 1;
            illgotten = start;
            cur = 0;
            numbers.fill(0);
            num = 0;
            number = false;
            continue;
        }
        if sequence != 0 {
            finish = sequence == S_DA;
            if !finish {
                let cap = buf.get(record..capture).unwrap_or(&[]);
                handle(sequence & !SEQ, &numbers[..num], cap);
            }
        }
        let dropped = sequence != 0 || action & A_DROP != 0;
        if dropped {
            current = start;
        }
        if dropped && action & A_CONTINUE == 0 {
            cur = 0;
            numbers.fill(0);
            num = 0;
            number = false;
        } else {
            cur += 1;
            while at(cur) == T_END {
                cur += 1;
            }
        }
    }

    if current > 0 {
        z.ungetbytes(&buf[..current]);
    }
}

fn add_extension(feature: &str) {
    let mut list = array_get(EXTVAR).unwrap_or_default();
    list.push(feature.to_string());
    array_set(EXTVAR, list);
}

fn handle_color(which: i32, red: i32, green: i32, blue: i32) {
    let name = match which {
        0 => {
            super::refresh::set_memo_term_color(false, red as u8, green as u8, blue as u8);
            ".term.fg"
        }
        1 => {
            // Opacity in highlighting mixes with the real background.
            super::refresh::set_memo_term_color(true, red as u8, green as u8, blue as u8);
            // Rec. 709 coefficients for lightness.
            let light = 0.2126f32 * red as f32 + 0.7152 * green as f32 + 0.0722 * blue as f32;
            var_set(
                MODEVAR,
                if light <= 127.0 { "dark" } else { "light" },
                false,
            );
            ".term.bg"
        }
        2 => {
            let c = |v: i32| (v & 0xff) as u32;
            MEMO_CURSOR.with(|m| {
                m.set(
                    (c(red) << CURF_RED_SHIFT)
                        | (c(green) << CURF_GREEN_SHIFT)
                        | (c(blue) << CURF_BLUE_SHIFT)
                        | CURF_COLOR,
                )
            });
            ".term.cursor"
        }
        _ => return,
    };
    var_set(
        name,
        &format!("#{:02x}{:02x}{:02x}", red, green, blue),
        false,
    );
}

fn handle_query(sequence: u8, numbers: &[i32], capture: &[u8]) {
    match sequence {
        1 => {
            if let &[which, r, g, b] = numbers {
                handle_color(which, r, g, b);
            }
        }
        2 => add_extension("modkeys-kitty"),
        3 => add_extension("truecolor"),
        4 => {
            var_set(IDVAR, &String::from_utf8_lossy(capture), false);
        }
        5 => {
            var_set(VERVAR, &String::from_utf8_lossy(capture), false);
        }
        _ => {}
    }
}

/// zsh's query_terminal: asks for the default colours, keyboard protocol,
/// truecolor support and terminal name, as .term.extensions allows.
fn query_terminal(z: &mut Zle) {
    // Roughly the feature for each query; the last, device attributes,
    // has none and only ends the wait.
    const FEATURES: [&str; 6] = [
        "bg",
        "fg",
        "cursorcolor",
        "modkeys-kitty",
        "truecolor",
        "id",
    ];
    const QUERIES: [&str; 7] = [
        "\x1b]11;?\x1b\\",
        "\x1b]10;?\x1b\\",
        "\x1b]12;?\x1b\\",
        "\x1b[?u",
        "\x1bP+q524742\x1b\\",
        "\x1b[>0q",
        // The spaces and CR clear fragments of sequences that some
        // terminals (GNU screen) show.
        "\x1b[c  \r",
    ];
    let flist = array_get(EXTVAR).unwrap_or_default();
    let envid = var_get_opt("TERM_PROGRAM");
    let mut badapple = false;
    if let Some(id) = &envid {
        var_set(IDVAR, id, false);
        if let Some(ver) = var_get_opt("TERM_PROGRAM_VERSION") {
            var_set(VERVAR, &ver, false);
            // Older macOS Terminal neither consumes the truecolor query
            // nor supports it.
            let major: i64 = ver
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            badapple = id == "Apple_Terminal" && major < 470;
        }
    }
    if flist.iter().any(|f| f == "-query") {
        return;
    }
    let colorterm = var_get_opt("COLORTERM");
    let mut tquery = String::new();
    for (i, q) in QUERIES.iter().enumerate() {
        let last = i >= FEATURES.len();
        let mut found = last && tquery.is_empty();
        let mut enable = false;
        if !last {
            for f in &flist {
                if found {
                    break;
                }
                // Only the keyboard protocol query is off by default.
                enable = i == 3 && f.strip_prefix("query-") == Some(FEATURES[i]);
                found = enable
                    || f.strip_prefix('-').unwrap_or(f) == FEATURES[i]
                    || f.strip_prefix("-query-") == Some(FEATURES[i]);
            }
        }
        let skip = if found { !enable } else { i == 3 };
        if skip {
            continue;
        }
        if i == 4 && matches!(colorterm.as_deref(), Some("truecolor" | "24bit")) {
            add_extension("truecolor");
        } else if (i != 4 || !badapple) && (i != 5 || envid.is_none()) {
            tquery.push_str(q);
        }
    }
    if !tquery.is_empty() {
        probe(z, &tquery, &query_states(), handle_query);
    }
}
