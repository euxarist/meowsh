//! `typeset` and the commands built on it (`declare`, `local`, `export`,
//! `readonly`, `integer`, `float`), following zsh's bin_typeset and
//! typeset_single in Src/builtin.c and printparamnode in Src/params.c.

use crate::params::*;
use crate::shell::builtin_err;
use crate::shell::with_shell;
use crate::sys::out;
use crate::types::Shell;

/// One argument to a declaration command: a plain word (option or name)
/// or an assignment, possibly of an array value.
#[derive(Clone, Debug)]
pub enum DeclArg {
    Word(String),
    Assign {
        name: String,
        append: bool,
        value: Option<String>,
    },
    Array {
        name: String,
        append: bool,
        items: Vec<crate::shell::ArrItem>,
    },
}

// Parameter kinds, used only in option bit sets.
const K_ARRAY: u32 = 1 << 28;
const K_ASSOC: u32 = 1 << 29;
const LOCAL: u32 = 1 << 30;
const TIED: u32 = 1 << 27;

fn letter_bit(c: char) -> Option<u32> {
    Some(match c {
        'a' => K_ARRAY,
        'A' => K_ASSOC,
        'i' => INTEGER,
        'E' => FLOAT_E,
        'F' => FLOAT_F,
        'L' => LEFT,
        'R' => RIGHT_B,
        'Z' => RIGHT_Z,
        'l' => LOWER,
        'u' => UPPER,
        'r' => READONLY,
        't' => TAG,
        'x' => EXPORT,
        'U' => UNIQUE,
        'h' => HIDE,
        'H' => HIDEVAL,
        'T' => TIED,
        'n' => NAMEREF,
        _ => return None,
    })
}

// Option letters each command accepts (from zsh's builtin table) and the
// options it implies.
fn spec(cmd: &str) -> (&'static str, &'static str) {
    match cmd {
        "export" => ("EFHLRTUZafhilprtu", "xg"),
        "readonly" => ("AEFHLRTUZafghilptux", "r"),
        "integer" => ("HLRZghilprtux", "i"),
        "float" => ("EFHLRZghlprtux", "E"),
        "local" => ("AEFHLPRTUZahilnprtux", ""),
        "private" => ("AEFHLPRTUZahilnprtux", ""),
        _ => ("AEFHLRTUZafghiklmnprtuxz", ""),
    }
}

// Letters taking an optional numeric argument.
fn takes_number(c: char) -> bool {
    matches!(c, 'E' | 'F' | 'L' | 'R' | 'Z' | 'i' | 'p')
}

#[derive(Default)]
struct Opts {
    minus: Vec<char>,
    plus: Vec<char>,
    args: Vec<(char, String)>,
    bare_plus: bool,
}

impl Opts {
    fn minus(&self, c: char) -> bool {
        self.minus.contains(&c)
    }
    fn plus(&self, c: char) -> bool {
        self.plus.contains(&c)
    }
    fn isset(&self, c: char) -> bool {
        self.minus(c) || self.plus(c)
    }
    fn arg(&self, c: char) -> Option<&str> {
        self.args.iter().find(|a| a.0 == c).map(|a| a.1.as_str())
    }
}

// zsh's checks for typeset on a special computed parameter (which has no
// entry in the parameter table here). Some(result) when handled.
fn special_typeset(
    cmd: &str,
    name: &str,
    on: u32,
    off: u32,
    ops: &Opts,
    level: usize,
) -> Option<bool> {
    if !crate::shell::DYNAMIC_NAMES.contains(&name) {
        return None;
    }
    let autoload = name == "SRANDOM";
    let global = on & LOCAL == 0 || level == 0;
    // zsh/datetime's parameters are hidden specials: a local replaces one,
    // unless `typeset +h` unhid it, when it stays read-only.
    if matches!(name, "EPOCHSECONDS" | "EPOCHREALTIME") {
        let unhidden = |set: Option<bool>| {
            let mut u = UNHIDDEN.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(v) = set {
                if v {
                    u.insert(name.to_string());
                } else {
                    u.remove(name);
                }
            }
            u.contains(name)
        };
        // An unloaded module takes its parameter (and the unhiding) away.
        if !crate::modules::feature_available(&format!("p:{}", name)) {
            unhidden(Some(false));
            return None;
        }
        if off & HIDE != 0 {
            unhidden(Some(true));
            return Some(true);
        }
        if !global {
            if unhidden(None) {
                crate::shell::shell_err(&format!("read-only variable: {}", name));
                crate::exec::abort_command(1);
                return Some(false);
            }
            return None;
        }
    }
    if on & HIDE != 0 {
        if global {
            warn(cmd, &format!("{}: can't change parameter attribute", name));
            crate::exec::abort_command(1);
            return Some(false);
        }
        // A hidden local replaces the special in its scope.
        return None;
    }
    let numeric = !matches!(name, "USERNAME" | "EPOCHREALTIME");
    // An autoloaded parameter not loaded yet has no type of its own.
    let unloaded = autoload && !SRANDOM_LOADED.load(std::sync::atomic::Ordering::Relaxed);
    let own = if numeric && !unloaded { INTEGER } else { 0 };
    let chflags =
        ((off & own) | (on & !own)) & (INTEGER | FLOAT_E | FLOAT_F | K_ASSOC | K_ARRAY | NAMEREF);
    if chflags != 0 && !(name == "SECONDS" && chflags & (FLOAT_E | FLOAT_F) != 0) {
        if autoload {
            if global {
                warn(
                    cmd,
                    &format!("{}: can't change type of autoloaded parameter", name),
                );
                crate::exec::abort_command(1);
                return Some(false);
            }
            return None;
        }
        warn(
            cmd,
            &format!("{}: can't change type of a special parameter", name),
        );
        crate::exec::abort_command(1);
        return Some(false);
    }
    // An autoloaded parameter not yet loaded is replaced by a local.
    if autoload && !global && !SRANDOM_LOADED.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    // SECONDS may be made a float (fractional seconds) or an integer again.
    if name == "SECONDS" {
        use std::sync::atomic::Ordering;
        if on & (FLOAT_E | FLOAT_F) != 0 {
            crate::shell::SECONDS_FLOAT.store(true, Ordering::Relaxed);
        } else if on & INTEGER != 0 {
            crate::shell::SECONDS_FLOAT.store(false, Ordering::Relaxed);
        }
    }
    if ops.isset('p') {
        let v = crate::shell::var_get(name);
        let g = if level > 0 { "-g " } else { "" };
        let float = name == "SECONDS"
            && crate::shell::SECONDS_FLOAT.load(std::sync::atomic::Ordering::Relaxed);
        out(&if float {
            format!("typeset {}-F {}={}\n", g, name, v)
        } else if numeric {
            format!("typeset {}-i10 {}={}\n", g, name, v)
        } else {
            format!(
                "typeset {}{}={}\n",
                g,
                name,
                crate::builtins::shell_quote(&v)
            )
        });
    }
    Some(true)
}

// Scalar parameters zsh treats as special.
const SCALAR_SPECIALS: &[&str] = &[
    "HOME", "PWD", "OLDPWD", "IFS", "TERM", "PS1", "PS2", "PS3", "PS4",
];

/// Set once $SRANDOM has been read: its module counts as loaded.
pub static SRANDOM_LOADED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn warn(cmd: &str, msg: &str) {
    builtin_err(cmd, msg);
}

pub fn typeset(cmd: &str, args: &[DeclArg]) -> i32 {
    crate::shell::without_assign_warnings(|| typeset_inner(cmd, args))
}

fn typeset_inner(cmd: &str, args: &[DeclArg]) -> i32 {
    let (allowed, implied) = spec(cmd);
    let mut ops = Opts::default();
    for c in implied.chars() {
        ops.minus.push(c);
    }
    // args[0] is the command name; options come next, as words starting
    // with - or +.
    let mut i = 1;
    while let Some(DeclArg::Word(w)) = args.get(i) {
        if w == "--" || w == "-" {
            i += 1;
            break;
        }
        if w == "+" {
            ops.bare_plus = true;
            i += 1;
            continue;
        }
        let (on, body) = match (w.strip_prefix('-'), w.strip_prefix('+')) {
            (Some(b), _) if !b.is_empty() => (true, b),
            (_, Some(b)) if !b.is_empty() => (false, b),
            _ => break,
        };
        let chars: Vec<char> = body.chars().collect();
        let mut k = 0;
        while k < chars.len() {
            let c = chars[k];
            if !allowed.contains(c) {
                warn(
                    cmd,
                    &format!("bad option: {}{}", if on { '-' } else { '+' }, c),
                );
                return 1;
            }
            if on {
                ops.minus.push(c);
                ops.plus.retain(|&x| x != c);
            } else {
                ops.plus.push(c);
                ops.minus.retain(|&x| x != c);
            }
            if takes_number(c) {
                let rest: String = chars[k + 1..].iter().collect();
                if !rest.is_empty() && rest.chars().all(|d| d.is_ascii_digit()) {
                    ops.args.push((c, rest));
                    k = chars.len();
                    continue;
                }
                if k + 1 == chars.len() {
                    if let Some(DeclArg::Word(n)) = args.get(i + 1) {
                        if !n.is_empty() && n.chars().all(|d| d.is_ascii_digit()) {
                            ops.args.push((c, n.clone()));
                            i += 1;
                        }
                    }
                }
            }
            k += 1;
        }
        i += 1;
    }
    // Words that expanded into `name=value` (`typeset $1=$2`) are
    // assignments too.
    let rest: Vec<DeclArg> = args[i..]
        .iter()
        .map(|a| match a {
            DeclArg::Word(w) => match w.find('=') {
                Some(k) if k > 0 => {
                    let (name, append) = match w[..k].strip_suffix('+') {
                        Some(n) => (n.to_string(), true),
                        None => (w[..k].to_string(), false),
                    };
                    DeclArg::Assign {
                        name,
                        append,
                        value: Some(w[k + 1..].to_string()),
                    }
                }
                _ => a.clone(),
            },
            other => other.clone(),
        })
        .collect();
    let rest = &rest[..];

    if ops.isset('f') {
        return crate::builtins::functions_cmd(cmd, &ops.minus, &ops.plus, rest);
    }

    let mut on = 0u32;
    let mut off = 0u32;
    let mut bad_ref = false;
    for c in "aiEFALRZlurtxUhHTn".chars() {
        let bit = letter_bit(c).unwrap();
        if ops.minus(c) {
            on |= bit;
        } else if ops.plus(c) {
            off |= bit;
        } else {
            continue;
        }
        // A nameref takes only -r, -u, -H and -h.
        if ops.minus('n') && c != 'n' && bit & !(READONLY | UPPER | HIDEVAL | HIDE) != 0 {
            warn(cmd, &format!("-{} not allowed with -n", c));
            bad_ref = true;
        }
    }
    if bad_ref {
        return 1;
    }
    if ops.minus('n') && ops.minus('m') && !ops.isset('p') {
        builtin_err(cmd, "-m not allowed with -n");
        return 1;
    }
    let roff = off;
    if on & FLOAT_F != 0 {
        off |= UPPER | K_ARRAY | K_ASSOC | INTEGER | FLOAT_E;
        on &= !FLOAT_E;
    }
    if on & FLOAT_E != 0 {
        off |= UPPER | K_ARRAY | K_ASSOC | INTEGER | FLOAT_F;
    }
    if on & INTEGER != 0 {
        off |= UPPER | K_ARRAY | K_ASSOC | FLOAT_E | FLOAT_F;
    }
    if on & (LEFT | RIGHT_Z) != 0 {
        off |= RIGHT_B;
    }
    if on & RIGHT_B != 0 {
        off |= LEFT | RIGHT_Z;
    }
    if on & UPPER != 0 {
        off |= LOWER;
    }
    if on & LOWER != 0 {
        off |= UPPER;
    }
    if on & K_ASSOC != 0 {
        off |= K_ARRAY;
    }
    if on & TIED != 0 {
        off |= INTEGER | FLOAT_E | FLOAT_F | K_ARRAY | K_ASSOC;
    }
    on &= !off;

    let posix = crate::options::is_set("posixbuiltins");
    if cmd == "readonly" && posix && !ops.plus('g') && !ops.minus('g') {
        ops.minus.push('g');
    }
    let mut pf = PrintFlags::default();
    if ops.isset('p') {
        pf.typeset = true;
        if posix && cmd == "export" {
            pf.posix_export = true;
        } else if posix && cmd == "readonly" {
            pf.posix_readonly = true;
        }
        pf.posix_arrays = ops.isset('a');
        if let Some(a) = ops.arg('p') {
            match a {
                "1" => pf.line = true,
                "0" => {}
                _ => {
                    warn(cmd, &format!("bad argument to -p: {}", a));
                    return 1;
                }
            }
        }
    }

    // POSIX `export -p name` / `readonly -p name` ignore -p: they only
    // export or mark readonly.
    if (pf.posix_export || pf.posix_readonly) && !rest.is_empty() {
        ops.minus.retain(|&c| c != 'p');
        ops.plus.retain(|&c| c != 'p');
    }
    if rest.is_empty() {
        if !ops.isset('p') {
            if on | roff == 0 {
                pf.type_ = true;
            }
            if roff != 0 || ops.bare_plus {
                pf.nameonly = true;
            }
        }
        list_params(on | roff, 0, &pf, None);
        return 0;
    }

    if (cmd == "private" || (cmd == "local" && ops.minus('P'))) && !ops.bare_plus {
        // A hidden local that functions it calls do not see.
        on |= LOCAL | HIDE | crate::params::PRIVATE;
    } else if cmd == "local" || ops.plus('g') {
        on |= LOCAL;
    } else if !ops.isset('g') {
        if ops.minus('x') {
            if !crate::options::is_set("globalexport") {
                on |= LOCAL;
            }
        } else if !(ops.isset('x') || ops.isset('m')) {
            on |= LOCAL;
        }
    }

    if on & TIED != 0 && !ops.isset('p') {
        return typeset_tied(cmd, on, off, roff, &ops, rest);
    }
    if off & TIED != 0 {
        warn(cmd, "use unset to remove tied variables");
        return 1;
    }

    let mut status = 0;
    if ops.isset('m') {
        if !ops.isset('p') {
            if on | roff == 0 {
                pf.type_ = true;
            }
            if on == 0 {
                pf.nameonly = true;
            }
        }
        for a in rest {
            let (pat, value) = match a {
                DeclArg::Word(w) => (w.clone(), None),
                DeclArg::Assign { name, value, .. } => (name.clone(), value.clone()),
                DeclArg::Array { name, .. } => (name.clone(), None),
            };
            if ops.plus('m') && value.is_none() {
                list_params(on | roff, 0, &pf, Some(&pat));
                if !crate::exec::sh_emulation_pub() && (on | roff) == 0 {
                    for (ec, line) in [
                        (
                            "zsh_eval_context",
                            "array readonly tied ZSH_EVAL_CONTEXT zsh_eval_context\n",
                        ),
                        (
                            "ZSH_EVAL_CONTEXT",
                            "readonly tied zsh_eval_context ZSH_EVAL_CONTEXT\n",
                        ),
                    ] {
                        if with_shell(|s| s.param(ec).is_none()) && pat == ec {
                            out(line);
                        }
                    }
                }
                continue;
            }
            let mut names: Vec<String> = with_shell(|s| {
                s.params
                    .iter()
                    .filter(|(_, p)| p.attrs & UNSET == 0)
                    .map(|(k, _)| k.clone())
                    .collect()
            });
            names.retain(|k| crate::pattern::matches(&pat, k));
            if value.is_none()
                && (on | roff) == 0
                && !ops.isset('p')
                && !crate::exec::sh_emulation_pub()
            {
                for ec in ["zsh_eval_context", "ZSH_EVAL_CONTEXT"] {
                    if with_shell(|s| s.param(ec).is_none()) && pat == ec {
                        names.push(ec.to_string());
                    }
                }
            }
            // zsh walks its parameter hash table here, unsorted: bucket
            // order of its string hash in the initial 151-slot table.
            let bucket = |n: &str| {
                n.bytes()
                    .fold(0u32, |h, c| h.wrapping_add(h << 5).wrapping_add(c as u32))
                    % 151
            };
            names.sort_by(|a, b| bucket(a).cmp(&bucket(b)).then(a.cmp(b)));
            for n in names {
                if n == "zsh_eval_context" && with_shell(|s| s.param(&n).is_none()) {
                    let ctx = crate::shell::eval_context();
                    out(&format!("zsh_eval_context=( {} )\n", ctx.join(" ")));
                    continue;
                }
                if n == "ZSH_EVAL_CONTEXT" && with_shell(|s| s.param(&n).is_none()) {
                    let ctx = crate::shell::eval_context();
                    out(&format!("ZSH_EVAL_CONTEXT={}\n", ctx.join(":")));
                    continue;
                }
                let asg = DeclArg::Assign {
                    name: n.clone(),
                    append: false,
                    value: value.clone(),
                };
                if !typeset_single(cmd, &n, on, off, roff, &asg, &ops, None) {
                    status = 1;
                }
            }
        }
        return status;
    }

    let mut p_out = String::new();
    for a in rest {
        let name = match a {
            DeclArg::Word(w) => w.clone(),
            DeclArg::Assign { name, .. } | DeclArg::Array { name, .. } => name.clone(),
        };
        if ops.isset('p') {
            let found = with_shell(|s| s.param(&name).cloned())
                .filter(|p| p.attrs & crate::params::PRIVATE_HIDDEN == 0);
            match found {
                Some(p) => {
                    p_out.push_str(&print_param(&name, &p, &pf, locallevel()));
                }
                // zsh's $0: the script/function name.
                None if name == "0" && !crate::exec::sh_emulation_pub() => {
                    let v = with_shell(|s| s.argv0.clone());
                    p_out.push_str(&print_param(
                        &name,
                        &Param::with(Value::Scalar(v)),
                        &pf,
                        locallevel(),
                    ));
                }
                // zsh's $argv: the positional parameters.
                None if name == "argv" && !crate::exec::sh_emulation_pub() => {
                    let pos = with_shell(|s| s.pos_params.clone());
                    p_out.push_str(&print_param(
                        &name,
                        &Param::with(Value::Array(pos)),
                        &pf,
                        locallevel(),
                    ));
                }
                // zsh's $status: a read-only integer special.
                None if matches!(name.as_str(), "zsh_eval_context" | "ZSH_EVAL_CONTEXT")
                    && !crate::exec::sh_emulation_pub() => {}
                None if name == "status" && !crate::exec::sh_emulation_pub() => {
                    p_out.push_str(&format!(
                        "typeset -i10 -r status={}\n",
                        with_shell(|s| s.last_status)
                    ));
                }
                None if crate::shell::DYNAMIC_NAMES.contains(&name.as_str())
                    && !crate::exec::sh_emulation_pub()
                    && crate::shell::var_exists(&name) =>
                {
                    let g = if locallevel() > 0 { "-g " } else { "" };
                    let float = name == "SECONDS"
                        && crate::shell::SECONDS_FLOAT.load(std::sync::atomic::Ordering::Relaxed);
                    p_out.push_str(&format!(
                        "typeset {}{} {}={}\n",
                        g,
                        if float { "-F" } else { "-i10" },
                        name,
                        crate::shell::var_get(&name)
                    ));
                }
                None => {
                    warn(cmd, &format!("no such variable: {}", name));
                    status = 1;
                }
            }
            continue;
        }
        let asg = match a {
            DeclArg::Word(w) => DeclArg::Assign {
                name: w.clone(),
                append: false,
                value: None,
            },
            other => other.clone(),
        };
        if on & crate::params::PRIVATE != 0
            && !ops.bare_plus
            && !private_allowed(cmd, &name, on, ops.minus('h'))
        {
            status = 1;
            continue;
        }
        if !typeset_single(cmd, &name, on, off, roff, &asg, &ops, None) {
            status = 1;
        }
    }
    if !p_out.is_empty() {
        out(&p_out);
    }
    status
}

// zsh/param/private's makeprivate checks: a special parameter (unless
// hidden with -h) or a local of this scope can't become private, and a
// private can't change type.
fn private_allowed(cmd: &str, name: &str, on: u32, hide: bool) -> bool {
    let level = locallevel();
    let existing = with_shell(|s| s.param(name).cloned());
    let special = crate::shell::DYNAMIC_NAMES.contains(&name)
        || crate::params::DEFAULT_TIES
            .iter()
            .any(|(sc, ar)| *sc == name || *ar == name)
        || existing.as_ref().is_some_and(|p| p.attrs & SPECIAL != 0);
    if special && !hide {
        warn(
            cmd,
            &format!("can't change scope of existing param: {}", name),
        );
        crate::exec::abort_command(1);
        return false;
    }
    let Some(p) = existing.filter(|p| p.level == level && level > 0) else {
        return true;
    };
    if p.attrs & crate::params::PRIVATE == 0 {
        warn(
            cmd,
            &format!("can't change scope of existing param: {}", name),
        );
        crate::exec::abort_command(1);
        return false;
    }
    let kind = |array: bool, assoc: bool, attrs: u32| {
        (
            array,
            assoc,
            attrs & (INTEGER | FLOAT_E | FLOAT_F | NAMEREF),
        )
    };
    let want = kind(on & K_ARRAY != 0, on & K_ASSOC != 0, on);
    let have = kind(p.is_array(), p.is_assoc(), p.attrs);
    if want != have {
        warn(
            cmd,
            &format!("can't change type of private param: {}", name),
        );
        crate::exec::abort_command(1);
        return false;
    }
    true
}

fn locallevel() -> usize {
    with_shell(|s| s.local_scopes.len())
}

fn has_value(asg: &DeclArg) -> bool {
    matches!(
        asg,
        DeclArg::Assign { value: Some(_), .. } | DeclArg::Array { .. }
    )
}

fn is_array_asg(asg: &DeclArg) -> bool {
    matches!(asg, DeclArg::Array { .. })
}

fn kind_bits(p: &Param) -> u32 {
    match p.value {
        Value::Array(_) => K_ARRAY,
        Value::Assoc(_) => K_ASSOC,
        Value::Scalar(_) => 0,
    }
}

fn flags_of(p: &Param) -> u32 {
    p.attrs | kind_bits(p) | if p.tie.is_some() { TIED } else { 0 }
}

// Applies typeset to one parameter. Mirrors zsh's typeset_single, minus
// special parameters with C-level storage and namespaces.
#[allow(clippy::too_many_arguments)]
fn typeset_single(
    cmd: &str,
    pname: &str,
    mut on: u32,
    mut off: u32,
    roff: u32,
    asg: &DeclArg,
    ops: &Opts,
    tie: Option<(String, String)>,
) -> bool {
    let (base_name, subscript) = match pname.find('[') {
        Some(k) if pname.ends_with(']') => (&pname[..k], Some(&pname[k + 1..pname.len() - 1])),
        _ => (pname, None),
    };
    if !crate::lexer::is_valid_name(base_name) {
        if base_name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            warn(cmd, &format!("not an identifier: {}", pname));
        } else {
            warn(cmd, &format!("not valid in this context: {}", pname));
        }
        crate::exec::abort_command(1);
        return false;
    }
    // $status stays the read-only special, even made local (but -h hides it).
    if base_name == "status"
        && on & HIDE == 0
        && !crate::exec::sh_emulation_pub()
        && with_shell(|s| s.param("status").is_none())
    {
        if has_value(asg) {
            crate::shell::shell_err("read-only variable: status");
            crate::exec::abort_command(1);
            return false;
        }
        return true;
    }
    if subscript.is_some() && on & NAMEREF != 0 {
        warn(cmd, "reference variable cannot be an array");
        return false;
    }
    let level = locallevel();
    // A nameref stands for what it refers to, unless -n/+n is given or a
    // new local is being made.
    let _swap = if (on | off) & NAMEREF == 0 {
        crate::shell::RefSwap::enter(base_name)
    } else {
        None
    };
    let resolved;
    let base_name = if (on | off) & NAMEREF == 0
        && with_shell(|s| {
            s.param(base_name)
                .is_some_and(|p| p.attrs & NAMEREF != 0 && (p.level == level || on & LOCAL == 0))
        }) {
        resolved = with_shell(|s| s.resolve_name(base_name));
        resolved.as_str()
    } else {
        base_name
    };
    let existing = with_shell(|s| s.param(base_name).cloned());
    // A placeholder reference (one referring to nothing yet) can't become
    // an array or a number.
    if on & NAMEREF == 0
        && on & (K_ARRAY | K_ASSOC | INTEGER | FLOAT_E | FLOAT_F | TAG) != 0
        && existing
            .as_ref()
            .is_some_and(|p| p.attrs & NAMEREF != 0 && (p.level == level || on & LOCAL == 0))
    {
        warn(
            cmd,
            &format!("{}: can't change type of a named reference", base_name),
        );
        return false;
    }
    if existing.is_none() && subscript.is_none() && !crate::exec::sh_emulation_pub() {
        if let Some(done) = special_typeset(cmd, base_name, on, off, ops, level) {
            return done;
        }
    }
    // A scalar special (HOME, IFS, ...) keeps its type unless hidden.
    if subscript.is_none()
        && on & HIDE == 0
        && SCALAR_SPECIALS.contains(&base_name)
        && existing
            .as_ref()
            .is_some_and(|p| p.level == 0 && p.attrs & HIDE == 0)
        && !crate::exec::sh_emulation_pub()
        && (on & (INTEGER | FLOAT_E | FLOAT_F | K_ASSOC | K_ARRAY | NAMEREF)) != 0
    {
        warn(
            cmd,
            &format!("{}: can't change type of a special parameter", base_name),
        );
        crate::exec::abort_command(1);
        return false;
    }

    // Array elements: `typeset a[2]=x`.
    if let Some(sub) = subscript {
        if on & READONLY != 0 {
            warn(
                cmd,
                &format!("{}: can't create readonly array elements", pname),
            );
            return false;
        }
        if on & LOCAL != 0 && level > 0 && existing.as_ref().is_none_or(|p| p.level != level) {
            warn(
                cmd,
                &format!("{}: can't create local array elements", pname),
            );
            return false;
        }
        let is_slice = sub.contains(',');
        let value = match asg {
            DeclArg::Assign { value, .. } => value.clone().unwrap_or_default(),
            // A slice takes a list: `typeset a[2,4]=(x y)`.
            DeclArg::Array { items, .. } if is_slice => {
                return crate::shell::array_splice(
                    base_name,
                    sub,
                    crate::shell::plain_items(items),
                );
            }
            _ => {
                warn(
                    cmd,
                    &format!("{}: inconsistent array element or slice assignment", pname),
                );
                return false;
            }
        };
        // A scalar's characters, or a slice: as an ordinary assignment.
        let is_scalar = existing
            .as_ref()
            .is_some_and(|p| matches!(p.value, crate::params::Value::Scalar(_)));
        if is_scalar || is_slice {
            return crate::exec::set_subscripted(base_name, sub, &value, false).unwrap_or(false);
        }
        return crate::shell::array_set_subscript(base_name, sub, value, false);
    }

    let mut usepm = existing
        .as_ref()
        .is_some_and(|p| p.attrs & UNSET == 0 || p.attrs & DECLARED != 0 || ops.isset('p'));
    if usepm && on & LOCAL != 0 && existing.as_ref().is_some_and(|p| p.level != level) {
        usepm = false;
    }
    let mut tc = false;
    if is_array_asg(asg)
        && on & (K_ARRAY | K_ASSOC) == 0
        && !(usepm && existing.as_ref().is_some_and(|p| kind_bits(p) != 0))
    {
        on |= K_ARRAY;
    }
    if let Some(pm) = existing.as_ref().filter(|_| usepm) {
        let pf = flags_of(pm);
        if is_array_asg(asg) && kind_bits(pm) == 0 {
            if on & (FLOAT_E | FLOAT_F | INTEGER) != 0 {
                warn(
                    cmd,
                    &format!("{}: can't assign array value to non-array", pname),
                );
                return false;
            }
            tc = true;
            usepm = false;
        } else {
            let chflags = ((off & pf) | (on & !pf))
                & (INTEGER | FLOAT_E | FLOAT_F | K_ASSOC | K_ARRAY | NAMEREF | TIED);
            if chflags != 0 && chflags != (FLOAT_E | FLOAT_F) {
                tc = true;
                usepm = if ops.minus('p') {
                    on & pf != 0
                } else if ops.plus('p') {
                    off & pf != 0
                } else {
                    false
                };
            }
        }
    }

    if usepm {
        let pm = existing.clone().unwrap();
        let pf = flags_of(&pm);
        let skip_minus =
            ops.minus('p') && on != 0 && !((on & pf) != 0 || (on & LOCAL != 0 && pm.level > 0));
        let skip_plus = ops.plus('p') && off != 0 && off & pf == 0;
        if skip_minus || skip_plus {
            return true;
        }
        let inconsistent = if is_array_asg(asg) {
            kind_bits(&pm) == 0
        } else {
            matches!(asg, DeclArg::Assign { value: Some(_), .. }) && kind_bits(&pm) != 0
        };
        if inconsistent {
            warn(
                cmd,
                &format!("{}: inconsistent type for assignment", base_name),
            );
            return false;
        }
        on &= !LOCAL;
        if on == 0 && roff == 0 && !has_value(asg) {
            // typeset -T on an existing parameter prints nothing; one not
            // tied yet becomes tied.
            if let Some(t) = &tie {
                with_shell(|s| {
                    if let Some(p) = s.param_mut(base_name) {
                        p.tie = Some(t.clone());
                    }
                });
            } else if ops.isset('p') {
                out(&print_param(
                    pname,
                    &pm,
                    &PrintFlags {
                        typeset: true,
                        ..Default::default()
                    },
                    level,
                ));
            } else if !ops.isset('g')
                && (!crate::options::is_set("typesetsilent") || ops.isset('m'))
            {
                out(&print_param(
                    pname,
                    &pm,
                    &PrintFlags {
                        include_value: true,
                        ..Default::default()
                    },
                    level,
                ));
            }
            return true;
        }
        // A read-only nameref can't be redeclared (zsh checks this only
        // for namerefs).
        if pm.attrs & READONLY != 0
            && off & READONLY == 0
            && pm.attrs & NAMEREF != 0
            && !ops.isset('p')
        {
            warn(cmd, &format!("{}: read-only reference", pname));
            return false;
        }
        // POSIX_BUILTINS: read-only can't be turned off.
        if pm.attrs & READONLY != 0
            && off & READONLY != 0
            && crate::options::is_set("posixbuiltins")
        {
            crate::shell::shell_err(&format!("read-only variable: {}", pname));
            return false;
        }
        if pm.attrs & READONLY != 0 && off & READONLY == 0 && has_value(asg) {
            // zsh reports this as a shell error, without the command name.
            crate::shell::shell_err(&format!("read-only variable: {}", pname));
            return false;
        }
        if ops.isset('p') {
            out(&print_param(
                pname,
                &pm,
                &PrintFlags {
                    typeset: true,
                    ..Default::default()
                },
                level,
            ));
            return true;
        }
        with_shell(|s| {
            let p = s.param_mut(base_name).unwrap();
            if p.attrs & DECLARED == 0 {
                off |= UNSET;
            }
            p.attrs = (p.attrs | (on & !(READONLY | K_ARRAY | K_ASSOC | TIED | LOCAL))) & !off;
            set_width_base(p, on, ops);
            if on & UNIQUE != 0 {
                if let Value::Array(a) = &mut p.value {
                    let mut seen = std::collections::HashSet::new();
                    a.retain(|x| seen.insert(x.clone()));
                }
            }
            if let Some((other, sep)) = &tie {
                p.tie = Some((other.clone(), sep.clone()));
            }
            // -U on a tied scalar uniquifies the array it is tied to.
            if on & UNIQUE != 0 && p.tie.is_some() {
                s.sync_tie(base_name);
            }
        });
        if !assign_value(base_name, asg) {
            return false;
        }
        with_shell(|s| {
            let partner = s.param(base_name).and_then(|p| {
                let (other, _) = p.tie.as_ref()?;
                (p.attrs & SPECIAL != 0).then(|| other.clone())
            });
            if let Some(p) = s.param_mut(base_name) {
                p.attrs |= on & READONLY;
            }
            if let Some(other) = partner {
                if let Some(o) = s.param_mut(&other) {
                    o.attrs = (o.attrs | (on & READONLY)) & !(off & READONLY);
                }
            }
        });
        return true;
    } else if ops.isset('p') {
        return true;
    }

    if if is_array_asg(asg) {
        on & (K_ARRAY | K_ASSOC) == 0
    } else {
        matches!(asg, DeclArg::Assign { value: Some(_), .. }) && on & (K_ARRAY | K_ASSOC) != 0
    } {
        warn(
            cmd,
            &format!("{}: inconsistent type for assignment", base_name),
        );
        return false;
    }
    // A local special array (path, fpath, ...) stays an array: a scalar
    // value becomes its one element.
    if on & (K_ARRAY | K_ASSOC | INTEGER | FLOAT_E | FLOAT_F | HIDE) == 0
        && on & LOCAL != 0
        && level > 0
        && crate::params::DEFAULT_TIES
            .iter()
            .any(|(_, a)| *a == base_name)
        && existing
            .as_ref()
            .is_some_and(|p| p.attrs & HIDE == 0 && kind_bits(p) & K_ARRAY != 0)
    {
        on |= K_ARRAY;
    }

    // A new parameter, a new local, or a type conversion.
    let mut carried: Option<String> = None;
    let mut keep_level = None;
    if tc {
        let pm = existing.clone().unwrap();
        on |= !off & (READONLY | EXPORT) & pm.attrs;
        keep_level = Some(pm.level);
        if !has_value(asg) && (flags_of(&pm) | on) & (K_ARRAY | K_ASSOC) == 0 {
            carried = Some(with_shell(|s| display_value(s, &pm)));
        }
        with_shell(|s| {
            s.remove_param(base_name);
        });
    }
    let new_local = on & LOCAL != 0 && level > 0 && keep_level.is_none();
    with_shell(|s| {
        if new_local {
            s.make_local(base_name);
        }
        // A local special tied pair (PATH/path): make_local tied it.
        let made_tie = if new_local {
            s.param(base_name)
                .and_then(|p| p.tie.clone().map(|t| (t, p.is_array())))
        } else {
            None
        };
        let value = if on & K_ASSOC != 0 {
            Value::Assoc(Default::default())
        } else if on & K_ARRAY != 0 || made_tie.as_ref().is_some_and(|m| m.1) {
            Value::Array(Vec::new())
        } else if on & (INTEGER | FLOAT_E | FLOAT_F) != 0 {
            Value::Scalar("0".to_string())
        } else {
            Value::Scalar(String::new())
        };
        let mut p = Param::with(value);
        p.attrs = on & !(READONLY | K_ARRAY | K_ASSOC | TIED | LOCAL);
        if on & NAMEREF != 0 && !has_value(asg) && carried.is_none() {
            p.attrs |= DECLARED;
        }
        if crate::options::is_set_in(s, "typesettounset") && !has_value(asg) && carried.is_none() {
            p.attrs |= UNSET | DECLARED;
        }
        // POSIX_BUILTINS: `readonly name` / `export name` leave a new
        // parameter unset.
        if crate::options::is_set_in(s, "posixbuiltins")
            && on & (READONLY | EXPORT) != 0
            && !has_value(asg)
            && carried.is_none()
        {
            p.attrs |= UNSET | DECLARED;
        }
        p.level = match keep_level {
            Some(l) => l,
            None if on & LOCAL != 0 => level,
            None => existing
                .as_ref()
                .filter(|_| !new_local)
                .map(|e| e.level)
                .unwrap_or(0),
        };
        set_width_base(&mut p, on, ops);
        p.tie = tie.clone();
        if let Some((t, _)) = made_tie.filter(|_| p.tie.is_none()) {
            p.tie = Some(t);
            p.attrs |= SPECIAL;
        }
        s.params.insert(base_name.to_string(), p);
    });
    let asg = match (carried, asg) {
        (Some(v), DeclArg::Assign { name, .. }) => DeclArg::Assign {
            name: name.clone(),
            append: false,
            value: Some(v),
        },
        (_, a) => a.clone(),
    };
    if has_value(&asg) && !assign_value(base_name, &asg) {
        return false;
    }
    with_shell(|s| {
        if let Some(p) = s.param_mut(base_name) {
            p.attrs |= on & READONLY;
        }
    });
    if base_name == "CURSOR_FORM" && !has_value(&asg) {
        crate::zle::termctl::cursor_form_param_changed();
    }
    true
}

fn set_width_base(p: &mut Param, on: u32, ops: &Opts) {
    let arg = |c: char| ops.arg(c).and_then(|a| a.parse::<usize>().ok());
    let width = [(LEFT, 'L'), (RIGHT_B, 'R'), (RIGHT_Z, 'Z')]
        .iter()
        .find_map(|&(bit, c)| (on & bit != 0).then(|| arg(c)).flatten());
    if let Some(n) = width {
        p.width = n;
    }
    if on & INTEGER != 0 {
        if let Some(n) = arg('i') {
            if n == 0 || (2..=36).contains(&n) {
                p.base = n as u32;
            } else {
                crate::shell::builtin_err("typeset", "invalid base (must be 2 to 36 inclusive)");
            }
        }
    } else if on & FLOAT_E != 0 {
        if let Some(n) = arg('E') {
            p.base = n as u32;
        }
    } else if on & FLOAT_F != 0 {
        if let Some(n) = arg('F') {
            p.base = n as u32;
        }
    }
}

fn assign_value(name: &str, asg: &DeclArg) -> bool {
    // A nameref's own value is the name it refers to.
    let is_ref = with_shell(|s| s.param(name).is_some_and(|p| p.attrs & NAMEREF != 0));
    if is_ref {
        if let DeclArg::Assign { value: Some(v), .. } = asg {
            return crate::shell::set_ref_target(name, v);
        }
    }
    match asg {
        DeclArg::Assign { value: None, .. } | DeclArg::Word(_) => true,
        DeclArg::Assign {
            value: Some(v),
            append,
            ..
        } => {
            let is_list = with_shell(|s| s.param(name).is_some_and(|p| kind_bits(p) != 0));
            if is_list {
                let items = if v.is_empty() {
                    Vec::new()
                } else {
                    vec![v.clone()]
                };
                return crate::shell::array_set(name, items);
            }
            let v = if *append {
                format!("{}{}", crate::shell::var_get(name), v)
            } else {
                v.clone()
            };
            crate::shell::var_set(name, &v, false)
        }
        DeclArg::Array { items, append, .. } => {
            crate::shell::array_assign_items(name, items.clone(), *append)
        }
    }
}

// typeset -T SCALAR array [sep]
fn typeset_tied(cmd: &str, on: u32, off: u32, roff: u32, ops: &Opts, rest: &[DeclArg]) -> i32 {
    if ops.isset('m') {
        warn(cmd, "incompatible options for -T");
        return 1;
    }
    if rest.len() < 2 {
        warn(cmd, "-T requires names of scalar and array");
        return 1;
    }
    if rest.len() > 3 {
        warn(cmd, "too many arguments for -T");
        return 1;
    }
    let name_of = |a: &DeclArg| match a {
        DeclArg::Word(w) => w.clone(),
        DeclArg::Assign { name, .. } | DeclArg::Array { name, .. } => name.clone(),
    };
    let (s_asg, a_asg) = (&rest[0], &rest[1]);
    let (sname, aname) = (name_of(s_asg), name_of(a_asg));
    if is_array_asg(s_asg) {
        warn(
            cmd,
            &format!("first argument of tie must be scalar: {}", sname),
        );
        return 1;
    }
    if matches!(a_asg, DeclArg::Assign { value: Some(_), .. }) {
        warn(
            cmd,
            &format!("second argument of tie must be array: {}", aname),
        );
        return 1;
    }
    if sname == aname {
        warn(cmd, &format!("can't tie a variable to itself: {}", sname));
        return 1;
    }
    if has_value(s_asg) && has_value(a_asg) {
        warn(
            cmd,
            &format!("only one tied parameter can have value: {}", sname),
        );
        return 1;
    }
    let sep = match rest.get(2) {
        None => ":".to_string(),
        Some(DeclArg::Word(w)) => w.chars().next().map(|c| c.to_string()).unwrap_or_default(),
        Some(_) => {
            warn(cmd, "third argument of tie must be join character");
            return 1;
        }
    };
    let level = locallevel();
    let existing = with_shell(|s| s.param(&sname).cloned());
    // The special tied pairs (PATH/path, ...) stay as they are, unless a
    // hidden local (-h) stands in for one of them.
    let special = |name: &str| with_shell(|s| s.param(name).is_none_or(|p| p.attrs & HIDE == 0));
    if let Some((_, arr)) = crate::params::DEFAULT_TIES
        .iter()
        .find(|(sc, _)| *sc == sname)
    {
        if special(&sname) {
            if *arr != aname || !special(&aname) {
                warn(
                    cmd,
                    &format!(
                        "{} special parameter can only be tied to special parameter {}",
                        sname, arr
                    ),
                );
                return 1;
            }
            if sep != ":" {
                warn(
                    cmd,
                    "cannot change the join character of special tied parameters",
                );
                return 1;
            }
        }
    }
    if let Some((sc, _)) = crate::params::DEFAULT_TIES
        .iter()
        .find(|(_, a)| *a == aname)
    {
        if special(&aname) && (*sc != sname || !special(&sname)) {
            warn(
                cmd,
                &format!(
                    "{} special parameter can only be tied to special parameter {}",
                    aname, sc
                ),
            );
            return 1;
        }
    }
    // Re-tying a pair with a new join character re-splits the value.
    let resplit = existing
        .as_ref()
        .and_then(|p| p.tie.as_ref())
        .is_some_and(|(o, old)| o == &aname && old != &sep)
        && !has_value(s_asg)
        && !has_value(a_asg);
    if resplit && with_shell(|s| s.param(&aname).is_some_and(|p| p.attrs & READONLY != 0)) {
        crate::shell::shell_err(&format!("read-only variable: {}", aname));
        return 1;
    }
    // Keep an existing scalar's value and export attribute.
    let mut on = on;
    let mut old_value = None;
    if let Some(p) = &existing {
        if p.attrs & UNSET == 0 && (level == p.level || on & LOCAL == 0) {
            if p.tie.is_some() && p.is_array() {
                warn(cmd, &format!("already tied as non-scalar: {}", sname));
                return 1;
            }
            if let Some((other, _)) = &p.tie {
                if other != &aname {
                    warn(cmd, &format!("can't tie already tied scalar: {}", sname));
                    return 1;
                }
            } else if !has_value(s_asg) && !has_value(a_asg) && kind_bits(p) == 0 {
                old_value = with_shell(|s| s.scalar(&sname).map(|v| v.to_string()));
            }
            on |= p.attrs & !roff & EXPORT;
        }
    }
    let a_plain = DeclArg::Assign {
        name: aname.clone(),
        append: false,
        value: None,
    };
    if !typeset_single(
        cmd,
        &aname,
        (on | K_ARRAY) & !EXPORT & !TIED,
        off & !K_ARRAY,
        roff,
        &a_plain,
        ops,
        Some((sname.clone(), sep.clone())),
    ) {
        return 1;
    }
    let s_plain = match s_asg {
        DeclArg::Assign { value: Some(v), .. } => DeclArg::Assign {
            name: sname.clone(),
            append: false,
            value: Some(v.clone()),
        },
        _ => DeclArg::Assign {
            name: sname.clone(),
            append: false,
            value: None,
        },
    };
    if !typeset_single(
        cmd,
        &sname,
        on & !TIED,
        off,
        roff,
        &s_plain,
        ops,
        Some((aname.clone(), sep.clone())),
    ) {
        return 1;
    }
    if resplit {
        let joined = with_shell(|s| {
            for (n, other) in [(&sname, &aname), (&aname, &sname)] {
                if let Some(p) = s.param_mut(n) {
                    p.tie = Some((other.clone(), sep.clone()));
                }
            }
            s.array(&aname).map(|a| a.join(&sep)).unwrap_or_default()
        });
        return i32::from(!crate::shell::var_set(&sname, &joined, false));
    }
    match a_asg {
        DeclArg::Array { items, .. } => {
            // As zsh's assignment error, a failure aborts the command line.
            if !crate::shell::array_assign_items(&aname, items.clone(), false) {
                return crate::exec::abort_command(1);
            }
        }
        _ => {
            if let Some(v) = old_value {
                crate::shell::var_set(&sname, &v, false);
            } else {
                with_shell(|s| s.sync_tie(&sname));
            }
        }
    }
    0
}

#[derive(Default, Clone)]
pub struct PrintFlags {
    pub typeset: bool,
    pub type_: bool,
    pub nameonly: bool,
    pub include_value: bool,
    pub line: bool,
    pub posix_export: bool,
    pub posix_readonly: bool,
    // POSIX `export -ap` / `readonly -ap`: arrays are listed too.
    pub posix_arrays: bool,
}

const PMTYPES: &[(u32, &str, char, u8)] = &[
    (INTEGER, "integer", 'i', 1),
    (FLOAT_E, "float", 'E', 0),
    (FLOAT_F, "float", 'F', 0),
    (K_ARRAY, "array", 'a', 0),
    (K_ASSOC, "association", 'A', 0),
    (0, "local", '\0', 3),
    (HIDE, "hide", 'h', 0),
    (LEFT, "left justified", 'L', 2),
    (RIGHT_B, "right justified", 'R', 2),
    (RIGHT_Z, "zero filled", 'Z', 2),
    (LOWER, "lowercase", 'l', 0),
    (UPPER, "uppercase", 'u', 0),
    (READONLY, "readonly", 'r', 0),
    (TAG, "tagged", 't', 0),
    (EXPORT, "exported", 'x', 0),
    (UNIQUE, "unique", 'U', 0),
    (TIED, "tied", 'T', 0),
    (NAMEREF, "nameref", 'n', 0),
];

/// zsh's quotedzputs: as is when nothing is special, `''` when empty,
/// `$'...'` for unprintable characters, otherwise single quotes.
pub fn quote_value(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .any(|c| c.is_control() || crate::shell::decode_raw_byte(c).is_some())
    {
        let mut out = String::from("$'");
        for c in s.chars() {
            if let Some(b) = crate::shell::decode_raw_byte(c) {
                out.push_str(&format!("\\{:03o}", b));
                continue;
            }
            match c {
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                '\x1b' => out.push_str("\\e"),
                '\'' => out.push_str("\\'"),
                '\\' => out.push_str("\\\\"),
                c if (c as u32) < 32 => {
                    out.push_str(&format!("\\C-{}", char::from(b'@' + c as u8)));
                }
                '\x7f' => out.push_str("\\C-?"),
                c => out.push(c),
            }
        }
        out.push('\'');
        return out;
    }
    let quote_bang = crate::options::quick("interactive") && crate::options::quick("banghist");
    let special = |c: char| "#$^*()=|{}[]`<>?~;&\n\t \\'\"".contains(c) || (quote_bang && c == '!');
    if !s.chars().any(special) {
        return s.to_string();
    }
    let mut out = String::new();
    let mut inquote = false;
    for c in s.chars() {
        if c == '\'' {
            if inquote {
                out.push('\'');
                inquote = false;
            }
            out.push_str("\\'");
        } else {
            if !inquote {
                out.push('\'');
                inquote = true;
            }
            out.push(c);
        }
    }
    if inquote {
        out.push('\'');
    }
    out
}

/// The value of a scalar as `$name` shows it: numbers formatted, case
/// converted and justified.
pub fn display_value(_s: &Shell, p: &Param) -> String {
    p.text()
}

fn value_text(p: &Param, pf: &PrintFlags) -> String {
    match &p.value {
        Value::Scalar(v) => {
            // A numeric parameter with no value yet is zero.
            let num = if v.trim().is_empty() { "0" } else { v.trim() };
            if p.attrs & INTEGER != 0 {
                num.parse::<i64>()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|_| v.clone())
            } else if p.attrs & (FLOAT_E | FLOAT_F) != 0 {
                num.parse::<f64>()
                    .map(|f| crate::shell::format_float(f, p.attrs, p.base))
                    .unwrap_or_else(|_| v.clone())
            } else {
                quote_value(v)
            }
        }
        Value::Array(a) => {
            if pf.line {
                if a.is_empty() {
                    "()".to_string()
                } else {
                    let mut s = String::from("(");
                    for x in a {
                        s.push_str("\n  ");
                        s.push_str(&quote_value(x));
                    }
                    s.push_str("\n)");
                    s
                }
            } else {
                // zsh prints `( a b )`, and `(  )` when empty.
                let mut s = String::from("( ");
                for x in a {
                    s.push_str(&quote_value(x));
                    s.push(' ');
                }
                if a.is_empty() {
                    s.push(' ');
                }
                s.push(')');
                s
            }
        }
        Value::Assoc(m) => {
            let mut s = String::from("(");
            if !pf.line {
                s.push(' ');
            }
            for (k, v) in m {
                if pf.line {
                    s.push_str("\n  ");
                }
                s.push_str(&format!("[{}]={}", quote_value(k), quote_value(v)));
                if !pf.line {
                    s.push(' ');
                }
            }
            if pf.line && !m.is_empty() {
                s.push('\n');
            }
            s.push(')');
            s
        }
    }
}

/// One line describing a parameter, as `typeset -p`, `typeset +m` and
/// friends print it (zsh's printparamnode).
pub fn print_param(name: &str, p: &Param, pf: &PrintFlags, locallevel: usize) -> String {
    let mut out = String::new();
    let mut pf = pf.clone();
    let flags = flags_of(p);
    if p.attrs & UNSET != 0 {
        if ((pf.posix_export || pf.posix_readonly) && p.attrs & (READONLY | EXPORT) != 0)
            || p.attrs & DECLARED != 0
        {
            pf.nameonly = true;
        } else {
            return out;
        }
    }
    let mut altname = '\0';
    if pf.typeset || pf.posix_export || pf.posix_readonly {
        if pf.posix_export {
            if p.attrs & EXPORT == 0 {
                return String::new();
            }
            altname = 'x';
            out.push_str("export ");
        } else if pf.posix_readonly {
            if p.attrs & READONLY == 0 {
                return String::new();
            }
            altname = 'r';
            out.push_str("readonly ");
        } else if p.attrs & EXPORT != 0 && kind_bits(p) == 0 {
            if p.level > 0 && p.level >= locallevel && p.attrs & SPECIAL == 0 {
                out.push_str("local ");
            } else {
                altname = 'x';
                out.push_str("export ");
            }
        } else if locallevel > 0
            && (p.level >= locallevel || (p.level > 0 && p.attrs & SPECIAL != 0))
        {
            out.push_str(if p.attrs & EXPORT != 0 {
                "local "
            } else {
                "typeset "
            });
        } else if locallevel > 0 {
            out.push_str("typeset -g ");
        } else {
            out.push_str("typeset ");
        }
    }
    // POSIX `export -p` and `readonly -p` show no flags and no hashes,
    // and arrays only when asked for with -a.
    let posix_list = pf.posix_export || pf.posix_readonly;
    if posix_list && (p.is_assoc() || (p.is_array() && !pf.posix_arrays)) {
        return String::new();
    }
    if (pf.type_ || pf.typeset) && !posix_list {
        let mut doneminus = false;
        for &(bit, word, letter, how) in PMTYPES {
            if altname != '\0' && altname == letter {
                continue;
            }
            let doprint = if how == 3 {
                p.level > 0
            } else {
                (bit != EXPORT || (p.level > 0 && p.attrs & SPECIAL == 0) || kind_bits(p) != 0)
                    && flags & bit != 0
                    && bit != 0
            };
            if !doprint {
                continue;
            }
            if pf.typeset {
                if letter != '\0' {
                    if !doneminus {
                        out.push('-');
                        doneminus = true;
                    }
                    out.push(letter);
                }
            } else {
                out.push_str(word);
                out.push(' ');
            }
            if how == 1 && p.base > 0 {
                out.push_str(&format!("{} ", p.base));
                doneminus = false;
            }
            if how == 2 && p.width > 0 {
                out.push_str(&format!("{} ", p.width));
                doneminus = false;
            }
        }
        if doneminus {
            out.push(' ');
        }
        if let Some((other, _)) = &p.tie {
            // Tied pairs: `-p` names the scalar first and shows the
            // array's value; `+m` names the partner first.
            let peer = if pf.typeset && kind_bits(p) == 0 {
                name.to_string()
            } else {
                other.clone()
            };
            out.push_str(&quote_value(&peer));
            out.push(' ');
        }
    }
    let swapped = pf.typeset && !posix_list && p.tie.is_some() && kind_bits(p) == 0;
    let shown_name = if swapped {
        p.tie.as_ref().unwrap().0.clone()
    } else {
        name.to_string()
    };
    if pf.nameonly
        || (p.attrs & HIDEVAL != 0 && !pf.include_value)
        || (p.attrs & NAMEREF != 0
            && p.attrs & DECLARED != 0
            && p.text().is_empty()
            && p.attrs & HIDE == 0)
    {
        out.push_str(&quote_value(&shown_name));
    } else {
        out.push_str(&quote_value(&shown_name));
        out.push('=');
        let shown = if swapped {
            let arr = with_shell(|s| s.param(&shown_name).cloned());
            arr.map(|a| value_text(&a, &pf)).unwrap_or_default()
        } else {
            value_text(p, &pf)
        };
        out.push_str(&shown);
    }
    if pf.typeset {
        if let Some((_, sep)) = &p.tie {
            if sep != ":" {
                out.push(' ');
                // zsh keeps the separator as a C string, so NUL shows as ''.
                out.push_str(&quote_value(sep.split('\0').next().unwrap_or("")));
            }
        }
    }
    out.push('\n');
    out
}

// Lists parameters having any of `flags` (all if 0), for typeset with no
// names, optionally limited to names matching `pattern`.
fn list_params(flags: u32, exclude: u32, pf: &PrintFlags, pattern: Option<&str>) {
    let level = locallevel();
    // Copied out first: printing a tied pair looks up its partner.
    let (mut params, stashed_flags): (
        Vec<(String, Param)>,
        std::collections::HashMap<String, u32>,
    ) = with_shell(|s| {
        let p = s
            .params
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut st = std::collections::HashMap::new();
        for (_, k, sp) in s.private_stash.iter().rev() {
            st.entry(k.clone())
                .or_insert_with(|| flags_of(sp) | if sp.level > 0 { LOCAL } else { 0 });
        }
        (p, st)
    });
    params.sort_by(|a, b| a.0.cmp(&b.0));
    let mut text = String::new();
    for (n, p) in &params {
        let f = flags_of(p) | if p.level > 0 { LOCAL } else { 0 };
        let st_f = (p.attrs & crate::params::PRIVATE_HIDDEN == 0)
            .then(|| stashed_flags.get(n).copied())
            .flatten()
            .unwrap_or(0);
        if flags != 0 && (f | st_f) & flags == 0 {
            continue;
        }
        if f & exclude != 0 {
            continue;
        }
        // Unset parameters (as after `unset`) are not listed.
        if p.attrs & UNSET != 0 && p.attrs & DECLARED == 0 {
            continue;
        }
        if let Some(pat) = pattern {
            if !crate::pattern::matches(pat, n) {
                continue;
            }
        }
        text.push_str(&print_param(n, p, pf, level));
    }
    out(&text);
}

/// Whether `typeset +h` has unhidden module special `name`.
pub fn is_unhidden(name: &str) -> bool {
    UNHIDDEN.lock().is_ok_and(|u| u.contains(name))
}

// Module specials made visible to locals with `typeset +h`.
static UNHIDDEN: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());
