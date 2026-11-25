//! Widgets and the zle builtin, after zsh's zle_thingy.c.
//!
//! A name (a "thingy") refers to a widget. Several names may share one
//! widget (`zle -A`). Each internal widget also has an immortal name with
//! a leading dot, which always refers to the original.

use super::*;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

#[derive(Clone, Debug)]
pub enum WidgetKind {
    /// None: listed by zsh but not implemented here; it beeps.
    Internal(Option<WidgetFn>),
    User(String),
    Complete {
        builtin: String,
        func: String,
    },
}

#[derive(Debug)]
pub struct Widget {
    pub kind: WidgetKind,
    pub flags: Cell<u32>,
}

struct Thingy {
    widget: Rc<Widget>,
    immortal: bool,
}

thread_local! {
    static THINGIES: RefCell<Option<BTreeMap<String, Thingy>>> = const { RefCell::new(None) };
}

fn with_table<R>(f: impl FnOnce(&mut BTreeMap<String, Thingy>) -> R) -> R {
    THINGIES.with(|t| {
        let mut t = t.borrow_mut();
        if t.is_none() {
            let mut m = BTreeMap::new();
            for (name, flags) in INTERNAL {
                let w = Rc::new(Widget {
                    kind: WidgetKind::Internal(widgets::lookup(name)),
                    flags: Cell::new(WIDGET_INT | flags),
                });
                m.insert(
                    format!(".{}", name),
                    Thingy {
                        widget: w.clone(),
                        immortal: true,
                    },
                );
                m.insert(
                    name.to_string(),
                    Thingy {
                        widget: w,
                        immortal: false,
                    },
                );
            }
            *t = Some(m);
        }
        f(t.as_mut().unwrap())
    })
}

pub fn lookup(name: &str) -> Option<Rc<Widget>> {
    with_table(|t| t.get(name).map(|th| th.widget.clone()))
}

/// A widget as zsh's $widgets shows it: builtin, user:func or
/// completion:widget:func.
pub fn describe(name: &str) -> String {
    match lookup(name).map(|w| w.kind.clone()) {
        Some(WidgetKind::Internal(_)) => "builtin".into(),
        Some(WidgetKind::User(f)) => format!("user:{}", f),
        Some(WidgetKind::Complete { builtin, func }) => format!("completion:{}:{}", builtin, func),
        None => String::new(),
    }
}

/// Every widget name, sorted, dotted names included.
pub fn names() -> Vec<String> {
    with_table(|t| t.keys().cloned().collect())
}

pub fn widget_flags(name: &str) -> u32 {
    lookup(name).map(|w| w.flags.get()).unwrap_or(0)
}

/// Adds internal widget `name` (from a module), with its dotted name.
pub fn add_internal(name: &str, f: WidgetFn, flags: u32) {
    with_table(|t| {
        let w = Rc::new(Widget {
            kind: WidgetKind::Internal(Some(f)),
            flags: Cell::new(WIDGET_INT | flags),
        });
        t.insert(
            format!(".{}", name),
            Thingy {
                widget: w.clone(),
                immortal: true,
            },
        );
        t.insert(
            name.to_string(),
            Thingy {
                widget: w,
                immortal: false,
            },
        );
    });
}

/// Removes the widget names of a module's internal widget.
pub fn delete_internal(name: &str) {
    with_table(|t| {
        t.remove(name);
        t.remove(&format!(".{}", name));
    });
}

// Binds `name` to widget `w`; false if the name is protected.
fn bind(name: &str, w: Rc<Widget>) -> bool {
    with_table(|t| {
        if t.get(name).is_some_and(|th| th.immortal) {
            return false;
        }
        t.insert(
            name.to_string(),
            Thingy {
                widget: w,
                immortal: false,
            },
        );
        true
    })
}

fn warn(msg: &str) {
    crate::shell::builtin_err("zle", msg);
}

fn zle_usable() -> bool {
    zle_active() && with_active(|_| ()).is_some() && !widgets::complete::in_completion()
}

/// The zle builtin.
pub fn builtin_zle(words: &[String]) -> i32 {
    let mut opts: Vec<char> = Vec::new();
    let mut i = 1;
    while i < words.len() {
        let w = &words[i];
        if w == "--" {
            i += 1;
            break;
        }
        if w == "-" || !w.starts_with('-') {
            break;
        }
        for c in w[1..].chars() {
            if !"aAcCDfFgGIKlLmMNrRTUw".contains(c) {
                warn(&format!("bad option: -{}", c));
                return 1;
            }
            opts.push(c);
        }
        i += 1;
    }
    let args = &words[i..];
    let isset = |c: char| opts.contains(&c);
    let ops: [(char, usize, i32); 14] = [
        ('l', 0, -1),
        ('D', 1, -1),
        ('A', 2, 2),
        ('N', 1, 2),
        ('C', 3, 3),
        ('R', 0, -1),
        ('M', 1, 1),
        ('U', 1, 1),
        ('K', 1, 1),
        ('I', 0, 0),
        ('f', 1, -1),
        ('F', 0, 2),
        ('T', 0, 2),
        ('\0', 0, -1),
    ];
    let mut op = ops.len() - 1;
    for (k, o) in ops.iter().enumerate().take(ops.len() - 1) {
        if isset(o.0) {
            op = k;
            break;
        }
    }
    if ops[op].0 != '\0' {
        for o in &ops[op + 1..ops.len() - 1] {
            if isset(o.0) {
                warn("incompatible operation selection options");
                return 1;
            }
        }
    }
    let (opc, min, max) = ops[op];
    if args.len() < min {
        warn(&format!("not enough arguments for -{}", opc));
        return 1;
    }
    if max != -1 && args.len() > max as usize {
        warn(&format!("too many arguments for -{}", opc));
        return 1;
    }
    match opc {
        'l' => zle_list(args, isset('a'), isset('L')),
        'D' => {
            let mut ret = 0;
            for a in args {
                let r = with_table(|t| match t.get(a.as_str()) {
                    None => 1,
                    Some(th) if th.immortal => 2,
                    Some(_) => {
                        t.remove(a.as_str());
                        0
                    }
                });
                match r {
                    1 => {
                        warn(&format!("no such widget `{}'", a));
                        ret = 1;
                    }
                    2 => {
                        warn(&format!("widget name `{}' is protected", a));
                        ret = 1;
                    }
                    _ => {}
                }
            }
            ret
        }
        'A' => {
            let Some(w) = lookup(&args[0]) else {
                warn(&format!("no such widget `{}'", args[0]));
                return 1;
            };
            if !bind(&args[1], w) {
                warn(&format!("widget name `{}' is protected", args[1]));
                return 1;
            }
            0
        }
        'N' => {
            let w = Rc::new(Widget {
                kind: WidgetKind::User(args.get(1).unwrap_or(&args[0]).clone()),
                flags: Cell::new(0),
            });
            if !bind(&args[0], w) {
                warn(&format!("widget name `{}' is protected", args[0]));
                return 1;
            }
            0
        }
        'C' => {
            let base = if args[1].starts_with('.') {
                args[1].clone()
            } else {
                format!(".{}", args[1])
            };
            let ok = lookup(&base).is_some_and(|w| w.flags.get() & ZLE_ISCOMP != 0);
            if !ok {
                warn(&format!("invalid widget `{}'", args[1]));
                return 1;
            }
            let w = Rc::new(Widget {
                kind: WidgetKind::Complete {
                    builtin: args[1].clone(),
                    func: args[2].clone(),
                },
                flags: Cell::new(WIDGET_NCOMP | ZLE_MENUCMP | ZLE_KEEPSUFFIX),
            });
            if !bind(&args[0], w) {
                warn(&format!("widget name `{}' is protected", args[0]));
                return 1;
            }
            0
        }
        'R' => {
            if !zle_active() {
                return 1;
            }
            let c = isset('c');
            with_active(|z| {
                let saved = z.statusline.take();
                let ocl = z.clearlist;
                if let Some(first) = args.first() {
                    if !first.is_empty() {
                        z.statusline = Some(first.clone());
                    }
                    if args.len() > 1 {
                        let items: Vec<String> = args[1..].to_vec();
                        widgets::complete::listlist(z, &items);
                        z.showinglist = 0;
                        z.clearlist = false;
                    } else if c {
                        z.clearlist = true;
                    }
                } else if c {
                    z.clearlist = true;
                }
                z.zrefresh();
                z.clearlist = ocl;
                z.statusline = saved;
            })
            .map(|_| 0)
            .unwrap_or(1)
        }
        'M' => {
            if !zle_active() {
                warn("can only be called from widget function");
                return 1;
            }
            with_active(|z| {
                z.showmsg(&args[0]);
                if z.widget_depth == 0 {
                    z.zrefresh();
                }
            });
            0
        }
        'U' => {
            if !zle_active() {
                warn("can only be called from widget function");
                return 1;
            }
            with_active(|z| {
                for &b in args[0].as_bytes().iter().rev() {
                    z.ungetbyte(b);
                }
            });
            0
        }
        'K' => {
            if !zle_active() {
                warn("can only be called from widget function");
                return 1;
            }
            with_active(|z| z.selectkeymap(&args[0], false)).unwrap_or(1)
        }
        'I' => {
            if zle_active() {
                with_active(|z| z.trash());
                0
            } else {
                1
            }
        }
        'f' => {
            if !zle_usable() {
                warn("can only set flags from a widget");
                return 1;
            }
            with_active(|z| {
                let mut ret = 0;
                let Some(w) = z.bindk.as_deref().and_then(lookup) else {
                    return 0;
                };
                for f in args {
                    match f.as_str() {
                        "yank" => w.flags.set(w.flags.get() | ZLE_YANKAFTER),
                        "yankbefore" => w.flags.set(w.flags.get() | ZLE_YANKBEFORE),
                        "kill" => w.flags.set(w.flags.get() | ZLE_KILL),
                        "vichange" => {
                            if z.invicmdmode() {
                                widgets::vi::startvichange(z, -1);
                                // A repeat restores the count: $NUMERIC
                                // shows it from here on.
                                params::push(z);
                            }
                        }
                        _ => {
                            warn(&format!("invalid flag `{}' given to zle -f", f));
                            ret = 1;
                        }
                    }
                }
                ret
            })
            .unwrap_or(0)
        }
        'F' => zle_fd(args, isset('L'), isset('w')),
        'T' => zle_transform(args, isset('L'), isset('r')),
        _ => zle_call(args),
    }
}

fn zle_list(args: &[String], all: bool, list: bool) -> i32 {
    let entry = |name: &str, w: &Widget, list: i32| -> Option<String> {
        if list < 0 {
            return Some(format!("{}\n", name));
        }
        let mut o = String::new();
        match &w.kind {
            WidgetKind::Internal(_) => return None,
            WidgetKind::User(f) => {
                if list > 0 {
                    o.push_str("zle -N ");
                    if name.starts_with('-') {
                        o.push_str("-- ");
                    }
                    o.push_str(&keymap::quoted(name));
                    if f != name {
                        o.push(' ');
                        o.push_str(&keymap::quoted(f));
                    }
                } else {
                    o.push_str(&keymap::nice(name));
                    if f != name {
                        o.push_str(&format!(" ({})", keymap::nice(f)));
                    }
                }
            }
            WidgetKind::Complete { builtin, func } => {
                if list > 0 {
                    o.push_str("zle -C ");
                    if name.starts_with('-') {
                        o.push_str("-- ");
                    }
                    o.push_str(&format!(
                        "{} {} {}",
                        keymap::quoted(name),
                        keymap::quoted(builtin),
                        keymap::quoted(func)
                    ));
                } else {
                    o.push_str(&format!(
                        "{} -C {} {}",
                        keymap::nice(name),
                        keymap::nice(builtin),
                        keymap::nice(func)
                    ));
                }
            }
        }
        o.push('\n');
        Some(o)
    };
    if args.is_empty() {
        let mode = if all { -1 } else { i32::from(list) };
        let out: String = with_table(|t| {
            t.iter()
                .filter_map(|(n, th)| entry(n, &th.widget, mode))
                .collect()
        });
        crate::sys::out(&out);
        return 0;
    }
    for a in args {
        let Some(w) = lookup(a) else { return 1 };
        if !all && matches!(w.kind, WidgetKind::Internal(_)) {
            return 1;
        }
        if list {
            if let Some(o) = entry(a, &w, 1) {
                crate::sys::out(&o);
            }
        }
    }
    0
}

fn zle_call(args: &[String]) -> i32 {
    let Some(wname) = args.first() else {
        return i32::from(!zle_usable());
    };
    if !zle_usable() {
        warn("widgets can only be called when ZLE is active");
        return 1;
    }
    with_active(|z| {
        params::pull(z);
        let modsave = z.zmod;
        let mut saveflag = false;
        let mut setbindk = false;
        let mut setlbindk = false;
        let mut keymap_restore: Option<String> = None;
        let mut i = 1;
        let mut bad = false;
        'outer: while i < args.len() && args[i].starts_with('-') {
            let a = &args[i];
            if a.len() == 1 || a == "--" {
                i += 1;
                break;
            }
            let cs: Vec<char> = a[1..].chars().collect();
            let mut j = 0;
            while j < cs.len() {
                let c = cs[j];
                let rest: String = cs[j + 1..].iter().collect();
                let take = |i: &mut usize| -> Option<String> {
                    if !rest.is_empty() {
                        Some(rest.clone())
                    } else {
                        *i += 1;
                        args.get(*i).cloned()
                    }
                };
                match c {
                    'f' => {
                        let f = take(&mut i);
                        if f.as_deref() != Some("nolast") {
                            warn("'nolast' expected after -f");
                            bad = true;
                            break 'outer;
                        }
                        setlbindk = true;
                        break;
                    }
                    'n' => {
                        let Some(n) = take(&mut i) else {
                            warn("number expected after -n");
                            bad = true;
                            break 'outer;
                        };
                        saveflag = true;
                        z.zmod.mult = n.trim().parse().unwrap_or(0);
                        z.zmod.flags |= MOD_MULT;
                        break;
                    }
                    'N' => {
                        saveflag = true;
                        z.zmod.mult = 1;
                        z.zmod.flags &= !MOD_MULT;
                    }
                    'K' => {
                        let Some(k) = take(&mut i) else {
                            warn("keymap expected after -K");
                            bad = true;
                            break 'outer;
                        };
                        keymap_restore = Some(z.curkeymapname.clone());
                        if z.selectkeymap(&k, false) != 0 {
                            bad = true;
                            break 'outer;
                        }
                        break;
                    }
                    'w' => setbindk = true,
                    _ => {
                        warn(&format!("bad option: {}", a));
                        bad = true;
                        break 'outer;
                    }
                }
                j += 1;
            }
            i += 1;
        }
        let mut ret = 1;
        if !bad {
            let rest: Vec<String> = args[i.min(args.len())..].to_vec();
            let wf = lookup(wname).map(|w| w.flags.get());
            setbindk = setbindk || wf.is_some_and(|f| f & (WIDGET_INT | ZLE_VIOPER) == WIDGET_INT);
            setlbindk |= wf.is_some_and(|f| f & ZLE_NOLAST != 0);
            ret = z.execzlefunc(wname, &rest, setbindk, setlbindk);
        }
        if saveflag {
            z.zmod = modsave;
        }
        if let Some(k) = keymap_restore {
            z.selectkeymap(&k, false);
        }
        params::push(z);
        ret
    })
    .unwrap_or(1)
}

thread_local! {
    static WATCH_FDS: RefCell<Vec<(i32, String, bool)>> = const { RefCell::new(Vec::new()) };
    static TC_FUNC: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub fn watch_fds() -> Vec<(i32, String, bool)> {
    WATCH_FDS.with(|w| w.borrow().clone())
}

pub fn remove_watch_fd(fd: i32) {
    WATCH_FDS.with(|w| w.borrow_mut().retain(|e| e.0 != fd));
}

pub fn tc_func() -> Option<String> {
    TC_FUNC.with(|t| t.borrow().clone())
}

fn zle_fd(args: &[String], list: bool, widget: bool) -> i32 {
    let mut fd = 0;
    if let Some(a) = args.first() {
        match a.parse::<i32>() {
            Ok(n) if n >= 0 => fd = n,
            _ => {
                warn(&format!("Bad file descriptor number for -F: {}", a));
                return 1;
            }
        }
    }
    if list || args.is_empty() {
        if args.len() > 1 {
            warn("too many arguments for -FL");
            return 1;
        }
        let mut found = false;
        let mut out = String::new();
        for (f, func, w) in watch_fds() {
            if !args.is_empty() && f != fd {
                continue;
            }
            found = true;
            out.push_str(&format!(
                "zle -F {}{} {}\n",
                if w { "-w " } else { "" },
                f,
                func
            ));
        }
        crate::sys::out(&out);
        return i32::from(!args.is_empty() && !found);
    }
    WATCH_FDS.with(|w| {
        let mut w = w.borrow_mut();
        if let Some(func) = args.get(1) {
            if let Some(e) = w.iter_mut().find(|e| e.0 == fd) {
                e.1 = func.clone();
                e.2 = widget;
            } else {
                w.push((fd, func.clone(), widget));
            }
            0
        } else if let Some(p) = w.iter().position(|e| e.0 == fd) {
            w.remove(p);
            0
        } else {
            warn(&format!("No handler installed for fd {}", fd));
            1
        }
    })
}

fn zle_transform(args: &[String], list: bool, remove: bool) -> i32 {
    let mut bad = 0;
    if list {
        if let Some(a) = args.first() {
            if args.len() > 1 {
                bad = 1;
            } else if a != "tc" {
                bad = 2;
            }
        }
        if bad == 0 {
            if let Some(f) = tc_func() {
                crate::sys::out(&format!("zle -T tc {}\n", keymap::quoted(&f)));
            }
        }
    } else if remove {
        if args.is_empty() {
            bad = -1;
        } else if args.len() > 1 {
            bad = 1;
        } else {
            TC_FUNC.with(|t| *t.borrow_mut() = None);
        }
    } else if args.len() < 2 {
        bad = -1;
    } else if args[0] == "tc" {
        TC_FUNC.with(|t| *t.borrow_mut() = Some(args[1].clone()));
    } else {
        bad = 2;
    }
    match bad {
        0 => 0,
        2 => {
            warn(&format!("-T: no such transformation '{}'", args[0]));
            1
        }
        b => {
            warn(&format!(
                "too {} arguments for option -T",
                if b > 0 { "many" } else { "few" }
            ));
            1
        }
    }
}

/// Internal widgets, with their flags, from zsh's iwidgets.list.
const INTERNAL: &[(&str, u32)] = &[
    ("accept-and-hold", 0),
    ("accept-and-infer-next-history", 0),
    ("accept-and-menu-complete", ZLE_MENUCMP | ZLE_KEEPSUFFIX),
    ("accept-line", 0),
    ("accept-line-and-down-history", 0),
    ("accept-search", 0),
    (
        "argument-base",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL | ZLE_NOTCOMMAND,
    ),
    ("auto-suffix-remove", ZLE_NOTCOMMAND),
    ("auto-suffix-retain", ZLE_KEEPSUFFIX | ZLE_NOTCOMMAND),
    ("backward-char", 0),
    ("backward-delete-char", ZLE_KEEPSUFFIX),
    ("backward-delete-word", ZLE_KEEPSUFFIX),
    ("backward-kill-line", ZLE_KILL | ZLE_KEEPSUFFIX),
    ("backward-kill-word", ZLE_KILL | ZLE_KEEPSUFFIX),
    ("backward-word", 0),
    ("beep", 0),
    ("beginning-of-buffer-or-history", ZLE_LINEMOVE),
    ("beginning-of-history", 0),
    ("beginning-of-line", 0),
    ("beginning-of-line-hist", 0),
    (
        "bracketed-paste",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_YANKBEFORE,
    ),
    ("capitalize-word", 0),
    (
        "clear-screen",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL | ZLE_NOTCOMMAND,
    ),
    ("complete-word", ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_ISCOMP),
    ("copy-prev-word", ZLE_KEEPSUFFIX),
    ("copy-prev-shell-word", ZLE_KEEPSUFFIX),
    ("copy-region-as-kill", ZLE_KEEPSUFFIX),
    ("deactivate-region", 0),
    ("delete-char", ZLE_KEEPSUFFIX),
    (
        "delete-char-or-list",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_ISCOMP,
    ),
    ("delete-word", ZLE_KEEPSUFFIX),
    (
        "describe-key-briefly",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL,
    ),
    (
        "digit-argument",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL | ZLE_NOTCOMMAND,
    ),
    ("down-case-word", 0),
    ("down-history", 0),
    ("down-line", ZLE_LINEMOVE | ZLE_LASTCOL),
    ("down-line-or-history", ZLE_LINEMOVE | ZLE_LASTCOL),
    ("down-line-or-search", ZLE_LINEMOVE | ZLE_LASTCOL),
    ("emacs-backward-word", 0),
    ("emacs-forward-word", 0),
    ("end-of-buffer-or-history", ZLE_LINEMOVE),
    ("end-of-history", 0),
    ("end-of-line", 0),
    ("end-of-line-hist", 0),
    ("end-of-list", ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL),
    ("exchange-point-and-mark", 0),
    ("execute-last-named-cmd", 0),
    ("execute-named-cmd", 0),
    ("expand-cmd-path", 0),
    ("expand-history", 0),
    (
        "expand-or-complete",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_ISCOMP,
    ),
    (
        "expand-or-complete-prefix",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_ISCOMP,
    ),
    ("expand-word", 0),
    ("forward-char", 0),
    ("forward-word", 0),
    ("get-line", 0),
    ("gosmacs-transpose-chars", 0),
    ("history-beginning-search-backward", 0),
    ("history-beginning-search-forward", 0),
    ("history-incremental-search-backward", 0),
    ("history-incremental-search-forward", 0),
    ("history-incremental-pattern-search-backward", 0),
    ("history-incremental-pattern-search-forward", 0),
    ("history-search-backward", 0),
    ("history-search-forward", 0),
    ("infer-next-history", 0),
    ("insert-last-word", ZLE_MENUCMP | ZLE_KEEPSUFFIX),
    ("kill-buffer", ZLE_KILL | ZLE_KEEPSUFFIX),
    ("kill-line", ZLE_KILL | ZLE_KEEPSUFFIX),
    ("kill-region", ZLE_KILL | ZLE_KEEPSUFFIX),
    ("kill-whole-line", ZLE_KILL | ZLE_KEEPSUFFIX),
    ("kill-word", ZLE_KILL | ZLE_KEEPSUFFIX),
    (
        "list-choices",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL | ZLE_ISCOMP,
    ),
    ("list-expand", ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL),
    ("magic-space", ZLE_KEEPSUFFIX | ZLE_MENUCMP),
    ("menu-complete", ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_ISCOMP),
    (
        "menu-expand-or-complete",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_ISCOMP,
    ),
    (
        "neg-argument",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL | ZLE_NOTCOMMAND,
    ),
    ("overwrite-mode", 0),
    ("pound-insert", 0),
    ("push-input", 0),
    ("push-line", 0),
    ("push-line-or-edit", 0),
    ("put-replace-selection", ZLE_KEEPSUFFIX),
    ("quoted-insert", ZLE_MENUCMP | ZLE_KEEPSUFFIX),
    ("quote-line", 0),
    ("quote-region", 0),
    ("read-command", ZLE_NOTCOMMAND),
    ("recursive-edit", ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL),
    ("redisplay", ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL),
    ("redo", ZLE_KEEPSUFFIX),
    (
        "reset-prompt",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL | ZLE_NOTCOMMAND | ZLE_NOLAST,
    ),
    (
        "reverse-menu-complete",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_ISCOMP,
    ),
    ("run-help", ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL),
    ("select-a-word", ZLE_KEEPSUFFIX),
    ("select-in-word", ZLE_KEEPSUFFIX),
    ("select-a-blank-word", ZLE_KEEPSUFFIX),
    ("select-in-blank-word", ZLE_KEEPSUFFIX),
    ("select-a-shell-word", ZLE_KEEPSUFFIX),
    ("select-in-shell-word", ZLE_KEEPSUFFIX),
    ("self-insert", ZLE_MENUCMP | ZLE_KEEPSUFFIX),
    ("self-insert-unmeta", ZLE_MENUCMP | ZLE_KEEPSUFFIX),
    ("send-break", 0),
    (
        "set-mark-command",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL,
    ),
    (
        "split-undo",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL | ZLE_NOTCOMMAND,
    ),
    ("spell-word", 0),
    ("set-local-history", ZLE_LASTCOL),
    ("transpose-chars", 0),
    ("transpose-words", 0),
    ("undefined-key", 0),
    ("undo", ZLE_KEEPSUFFIX),
    (
        "universal-argument",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL | ZLE_NOTCOMMAND,
    ),
    ("up-case-word", 0),
    ("up-history", 0),
    ("up-line", ZLE_LINEMOVE | ZLE_LASTCOL),
    ("up-line-or-history", ZLE_LINEMOVE | ZLE_LASTCOL),
    ("up-line-or-search", ZLE_LINEMOVE | ZLE_LASTCOL),
    ("vi-add-eol", 0),
    ("vi-add-next", 0),
    ("vi-backward-blank-word", 0),
    ("vi-backward-char", 0),
    ("vi-backward-delete-char", ZLE_KEEPSUFFIX),
    ("vi-backward-kill-word", ZLE_KEEPSUFFIX),
    ("vi-backward-word", 0),
    ("vi-backward-word-end", 0),
    ("vi-backward-blank-word-end", 0),
    ("vi-beginning-of-line", 0),
    ("vi-caps-lock-panic", ZLE_LASTCOL),
    ("vi-change", ZLE_LASTCOL | ZLE_VIOPER),
    ("vi-change-eol", 0),
    ("vi-change-whole-line", 0),
    ("vi-cmd-mode", 0),
    ("vi-delete", ZLE_KEEPSUFFIX | ZLE_LASTCOL | ZLE_VIOPER),
    ("vi-delete-char", ZLE_KEEPSUFFIX),
    ("vi-digit-or-beginning-of-line", 0),
    ("vi-down-case", ZLE_LASTCOL | ZLE_VIOPER),
    ("vi-down-line-or-history", ZLE_LINEMOVE),
    ("vi-end-of-line", ZLE_LASTCOL),
    ("vi-fetch-history", ZLE_LINEMOVE),
    ("vi-find-next-char", 0),
    ("vi-find-next-char-skip", 0),
    ("vi-find-prev-char", 0),
    ("vi-find-prev-char-skip", 0),
    ("vi-first-non-blank", 0),
    ("vi-forward-blank-word", 0),
    ("vi-forward-blank-word-end", 0),
    ("vi-forward-char", 0),
    ("vi-forward-word", 0),
    ("vi-forward-word-end", 0),
    ("vi-goto-column", 0),
    ("vi-goto-mark", 0),
    ("vi-goto-mark-line", ZLE_LINEMOVE),
    ("vi-history-search-backward", 0),
    ("vi-history-search-forward", 0),
    ("vi-indent", ZLE_LASTCOL | ZLE_VIOPER),
    ("vi-insert", 0),
    ("vi-insert-bol", 0),
    ("vi-join", 0),
    ("vi-kill-eol", ZLE_KEEPSUFFIX),
    ("vi-kill-line", ZLE_KEEPSUFFIX),
    ("vi-match-bracket", 0),
    ("vi-open-line-above", 0),
    ("vi-open-line-below", 0),
    ("vi-oper-swap-case", ZLE_LASTCOL | ZLE_VIOPER),
    ("vi-pound-insert", 0),
    ("vi-put-after", ZLE_YANKAFTER | ZLE_KEEPSUFFIX),
    ("vi-put-before", ZLE_YANKBEFORE | ZLE_KEEPSUFFIX),
    ("vi-quoted-insert", ZLE_MENUCMP | ZLE_KEEPSUFFIX),
    ("vi-repeat-change", 0),
    ("vi-repeat-find", 0),
    ("vi-repeat-search", 0),
    ("vi-replace", 0),
    ("vi-replace-chars", 0),
    ("vi-rev-repeat-find", 0),
    ("vi-rev-repeat-search", 0),
    ("vi-set-buffer", ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL),
    ("vi-set-mark", ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL),
    ("vi-substitute", 0),
    ("vi-swap-case", ZLE_LASTCOL),
    ("vi-undo-change", ZLE_KEEPSUFFIX),
    ("vi-unindent", ZLE_LASTCOL | ZLE_VIOPER),
    ("vi-up-case", ZLE_LASTCOL | ZLE_VIOPER),
    ("vi-up-line-or-history", ZLE_LINEMOVE),
    ("vi-yank", ZLE_LASTCOL | ZLE_VIOPER),
    ("vi-yank-eol", 0),
    ("vi-yank-whole-line", 0),
    ("visual-line-mode", ZLE_MENUCMP | ZLE_LASTCOL),
    ("visual-mode", ZLE_MENUCMP | ZLE_LASTCOL),
    (
        "what-cursor-position",
        ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL,
    ),
    ("where-is", ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL),
    ("which-command", ZLE_MENUCMP | ZLE_KEEPSUFFIX | ZLE_LASTCOL),
    ("yank", ZLE_YANKBEFORE | ZLE_KEEPSUFFIX),
    ("yank-pop", ZLE_KEEPSUFFIX | ZLE_NOTCOMMAND),
];
