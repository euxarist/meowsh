//! The `print` and `printf` builtins, following zsh's bin_print.

use std::collections::HashMap;

use crate::shell::{builtin_err, shell_err, var_get, var_set, with_shell};

const PRINT_OPTS: &str = "abcC:Df:ilmnNoOpPrRsSu:v:x:X:z-";
const PRINTF_OPTS: &str = "v:";

// The editor buffer stack: `print -z` pushes, `read -z` pops.
static BUFFER_STACK: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

pub fn push_buffer_stack(s: String) {
    if let Ok(mut b) = BUFFER_STACK.lock() {
        b.push(s);
    }
}

pub fn buffer_stack_len() -> usize {
    BUFFER_STACK.lock().map(|b| b.len()).unwrap_or(0)
}

pub fn pop_buffer_stack() -> Option<String> {
    BUFFER_STACK.lock().ok()?.pop()
}

#[derive(Default)]
struct Ops {
    set: Vec<char>,
    args: HashMap<char, String>,
}

impl Ops {
    fn has(&self, c: char) -> bool {
        self.set.contains(&c)
    }
    fn arg(&self, c: char) -> Option<&str> {
        self.args.get(&c).map(String::as_str)
    }
}

// zsh's builtin option parsing for print (BINF_PRINTOPTS) and printf
// (BINF_SKIPINVALID | BINF_SKIPDASH). Returns the options and the index
// of the first argument.
fn parse_opts(name: &str, words: &[String], printf: bool) -> Result<(Ops, usize), i32> {
    let mut ops = Ops::default();
    let mut optstr = if printf { PRINTF_OPTS } else { PRINT_OPTS };
    let mut skip_invalid = printf;
    let mut i = 1;
    while let Some(arg) = words.get(i) {
        let Some(body) = arg.strip_prefix('-') else {
            break;
        };
        if body.starts_with(|c: char| c.is_ascii_digit()) || (printf && body.is_empty()) {
            break;
        }
        if skip_invalid && body.chars().any(|c| !optstr.contains(c) || c == ':') {
            break;
        }
        let body = body.strip_prefix('-').unwrap_or(body);
        let mut dash = body.is_empty();
        let chars: Vec<char> = body.chars().collect();
        let mut k = 0;
        while k < chars.len() {
            let c = chars[k];
            match optstr.find(c).filter(|_| c != ':') {
                Some(p) => {
                    if c == '-' {
                        dash = true;
                    }
                    ops.set.push(c);
                    if optstr[p + 1..].starts_with(':') {
                        let value = if k + 1 < chars.len() {
                            chars[k + 1..].iter().collect()
                        } else if let Some(next) = words.get(i + 1) {
                            i += 1;
                            next.clone()
                        } else {
                            builtin_err(name, &format!("argument expected: -{}", c));
                            return Err(1);
                        };
                        ops.args.insert(c, value);
                        break;
                    }
                }
                None => {
                    builtin_err(name, &format!("bad option: -{}", c));
                    return Err(1);
                }
            }
            k += 1;
        }
        i += 1;
        // After -R, only echo's options follow.
        if !printf && ops.has('R') && !ops.has('f') {
            optstr = "ne";
            skip_invalid = true;
        }
        if dash {
            break;
        }
    }
    Ok((ops, i))
}

#[derive(Clone, Copy, PartialEq)]
enum Keys {
    // print without -r: \NNN octal, unknown escapes lose the backslash.
    Print,
    // A printf format: \NNN octal, unknown escapes kept.
    PrintfFmt,
    // A printf %b argument (and echo): \0NNN octal, unknown kept.
    PrintfArg,
}

// An escaped character; in a printf format an escaped `%` is literal.
fn push_esc(out: &mut String, c: char, how: Keys) {
    if c == '%' && how == Keys::PrintfFmt {
        out.push('%');
    }
    out.push(c);
}

/// Backslash escapes as print and printf read them; `\c` stops the
/// text and reports it.
fn getkeystring(s: &str, how: Keys) -> (String, bool) {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut out = String::new();
    let mut i = 0;
    let digits = |i: &mut usize, radix: u32, max: usize| -> Option<u32> {
        let start = *i;
        while *i < n && *i - start < max && chars[*i].is_digit(radix) {
            *i += 1;
        }
        (*i > start).then(|| {
            chars[start..*i]
                .iter()
                .fold(0, |v, c| v * radix + c.to_digit(radix).unwrap())
        })
    };
    while i < n {
        if chars[i] != '\\' || i + 1 >= n {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let c = chars[i + 1];
        i += 2;
        match c {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'e' | 'E' => out.push('\x1b'),
            'f' => out.push('\x0c'),
            'v' => out.push('\x0b'),
            '\\' => out.push('\\'),
            'c' => return (out, true),
            '0' if how == Keys::PrintfArg => {
                let v = digits(&mut i, 8, 3).unwrap_or(0);
                out.push(crate::shell::raw_byte_char((v & 0xff) as u8));
            }
            '0'..='7' if how != Keys::PrintfArg => {
                i -= 1;
                let v = digits(&mut i, 8, 3).unwrap_or(0);
                push_esc(&mut out, crate::shell::raw_byte_char((v & 0xff) as u8), how);
            }
            'x' => match digits(&mut i, 16, 2) {
                Some(v) => push_esc(&mut out, crate::shell::raw_byte_char((v & 0xff) as u8), how),
                None => out.push_str("\\x"),
            },
            'u' | 'U' => match digits(&mut i, 16, if c == 'u' { 4 } else { 8 }) {
                Some(v) => out.push(char::from_u32(v).unwrap_or('\u{fffd}')),
                None => {
                    out.push('\\');
                    out.push(c);
                }
            },
            _ if how == Keys::Print => out.push(c),
            _ => {
                out.push('\\');
                out.push(c);
            }
        }
    }
    (out, false)
}

pub fn builtin_print(words: &[String]) -> i32 {
    match parse_opts("print", words, false) {
        Ok((ops, i)) => print_main("print", &ops, words[i..].to_vec(), false),
        Err(st) => st,
    }
}

pub fn builtin_printf(words: &[String]) -> i32 {
    match parse_opts("printf", words, true) {
        Ok((ops, i)) => {
            if i >= words.len() {
                builtin_err("printf", "not enough arguments");
                return 1;
            }
            print_main("printf", &ops, words[i..].to_vec(), true)
        }
        Err(st) => st,
    }
}

fn print_main(name: &str, ops: &Ops, mut args: Vec<String>, printf: bool) -> i32 {
    let o = |c| ops.has(c);
    if [o('z'), o('s'), o('S'), o('v')]
        .iter()
        .filter(|&&b| b)
        .count()
        > 1
    {
        builtin_err(name, "only one of -s, -S, -v, or -z allowed");
        return 1;
    }
    if (o('z') || o('s') || o('S')) && (o('c') || o('C')) {
        builtin_err(name, "-c or -C not allowed with -s, -S, or -z");
        return 1;
    }
    if (o('z') || o('v') || o('s') || o('S')) && (o('p') || o('u')) {
        builtin_err(name, "-p or -u not allowed with -s, -S, -v, or -z");
        return 1;
    }
    let mut ncols = 0usize;
    if let Some(c) = ops.arg('C').filter(|_| !o('f')) {
        match c.parse::<i64>() {
            Err(_) => {
                builtin_err(name, &format!("number expected after -C: {}", c));
                return 1;
            }
            Ok(n) if n <= 0 => {
                builtin_err(name, &format!("invalid number of columns: {}", c));
                return 1;
            }
            Ok(n) => ncols = n as usize,
        }
    }
    let mut fmt = None;
    if printf {
        if args[0] == "--" {
            args.remove(0);
            if args.is_empty() {
                builtin_err(name, "not enough arguments");
                return 1;
            }
        }
        fmt = Some(args.remove(0));
    } else if let Some(f) = ops.arg('f') {
        fmt = Some(f.to_string());
    }
    let fmt = fmt.map(|f| getkeystring(&f, Keys::PrintfFmt));

    if o('m') {
        if args.is_empty() {
            builtin_err(name, "no pattern specified");
            return 1;
        }
        let pat = args.remove(0);
        args.retain(|a| crate::pattern::matches(&pat, a));
        if fmt.is_some() && args.is_empty() {
            return 0;
        }
    }

    let mut nnl = false;
    let raw = fmt.is_some() || (!o('e') && (o('R') || o('r') || o('E')));
    let mut n = 0;
    while n < args.len() {
        if !raw {
            let (t, cut) = getkeystring(&args[n], Keys::Print);
            args[n] = t;
            if cut {
                nnl = true;
                args.truncate(n + 1);
            }
        }
        if o('P') {
            args[n] = crate::shell::render_prompt(&args[n]);
        }
        if o('D') {
            args[n] = crate::builtins::print_dir(&args[n]);
        }
        n += 1;
    }

    if o('o') || o('O') {
        if fmt.is_some() && args.is_empty() {
            return 0;
        }
        if o('i') {
            args.sort_by_key(|a| a.to_lowercase());
        } else {
            args.sort();
        }
        if o('O') {
            args.reverse();
        }
    }

    let mut fd = 1;
    if (o('u') || o('p')) && !(o('z') || o('v') || o('s') || o('S')) {
        let spec = ops.arg('u').unwrap_or("p");
        if o('p') || spec == "p" {
            match with_shell(|s| s.coproc) {
                Some((w, _)) => fd = w,
                None => {
                    builtin_err(name, "-p: no coprocess");
                    return 1;
                }
            }
        } else {
            match spec.parse::<i32>() {
                Ok(v) => fd = v,
                Err(_) => {
                    builtin_err(name, &format!("number expected after -u: {}", spec));
                    return 1;
                }
            }
        }
    }

    let mut out = String::new();
    let mut ret = 0;
    let mut array_out = None;
    match &fmt {
        None if o('c') || o('C') => columns(&args, ncols, o('a'), o('C'), o('N'), &mut out),
        None => {
            if o('z') {
                push_buffer_stack(args.join(" "));
                return 0;
            }
            if o('s') || o('S') {
                crate::exec::history_push(&args.join(" "));
                return 0;
            }
            let sep = if o('l') {
                "\n"
            } else if o('N') {
                "\0"
            } else {
                " "
            };
            let tabs = ops
                .arg('X')
                .map(|x| (x, true))
                .or(ops.arg('x').map(|x| (x, false)));
            if let Some((x, all)) = tabs {
                let width = match x.parse::<usize>() {
                    Ok(w) if w > 0 => w,
                    _ => {
                        builtin_err(name, &format!("positive integer expected after -x: {}", x));
                        return 1;
                    }
                };
                let mut pos = 0;
                for (k, a) in args.iter().enumerate() {
                    pos = expand_tabs(a, width, pos, all, &mut out);
                    if k + 1 < args.len() {
                        out.push_str(sep);
                        pos = if o('l') {
                            0
                        } else {
                            pos + usize::from(!o('N'))
                        };
                    }
                }
            } else {
                out = args.join(sep);
            }
            if !(o('n') || nnl || (o('v') && !o('l'))) {
                out.push(if o('N') { '\0' } else { '\n' });
            }
        }
        Some((f, trunc)) => {
            let visarr = ops
                .arg('v')
                .is_some_and(|v| with_shell(|s| s.array(v).is_some()));
            let r = format_all(name, f, *trunc, &args, o('r'));
            out = r.out;
            if r.abort {
                let _ = crate::sys::write_fd(fd, &crate::shell::zstr_to_bytes(&out));
                return 1;
            }
            ret = r.ret;
            if visarr && !r.splits.is_empty() {
                let mut items = Vec::new();
                let mut start = 0;
                for &s in &r.splits {
                    items.push(out[start..s].to_string());
                    start = s;
                }
                items.push(out[start..].to_string());
                array_out = Some(items);
            }
        }
    }

    if let Some(v) = ops.arg('v') {
        let ok = match array_out {
            Some(items) => crate::shell::array_set(v, items),
            // -v 'name[sub]': one element of an array or hash.
            None => match v.split_once('[').filter(|_| v.ends_with(']')) {
                Some((n, sub)) if crate::lexer::is_valid_name(n) => {
                    crate::exec::set_subscripted(n, &sub[..sub.len() - 1], &out, false)
                        .unwrap_or(false)
                }
                _ => var_set(v, &out, false),
            },
        };
        return if ok { ret } else { 1 };
    }
    match crate::sys::write_fd(fd, &crate::shell::zstr_to_bytes(&out)) {
        Ok(()) => ret,
        Err(e) if fd == 1 && e.raw_os_error() == Some(libc::EBADF) => {
            // Closed for this command alone (`print foo >&-`) it is not
            // worth a word, as in zsh; closed for good (`exec >&-`) zsh
            // reports it but keeps the status.
            if !crate::exec::STDOUT_CLOSED_FOR_CMD.load(std::sync::atomic::Ordering::Relaxed) {
                crate::shell::shell_err("write error: bad file descriptor");
            }
            ret
        }
        Err(e) => {
            builtin_err(name, &format!("write error: {}", e));
            1
        }
    }
}

// print -c / -C: arguments in columns, down (or across with -a).
fn columns(args: &[String], ncols: usize, across: bool, fixed: bool, nul: bool, out: &mut String) {
    let widths: Vec<usize> = args.iter().map(|a| a.chars().count()).collect();
    let argc = args.len();
    let (nc, nr, sc);
    if fixed {
        nc = ncols;
        nr = argc.div_ceil(nc);
        let mut l = 0;
        for (i, &w) in widths.iter().enumerate() {
            if across {
                if i % nc == nc - 1 {
                    continue;
                }
            } else if i >= nr * (nc - 1) {
                break;
            }
            l = l.max(w);
        }
        sc = l + 2;
    } else {
        let l = widths.iter().copied().max().unwrap_or(0);
        sc = l + 2;
        // Without a terminal zsh's width is 0: a single column.
        let cols = var_get("COLUMNS").parse::<usize>().unwrap_or(0);
        nc = ((cols + 1) / sc).max(1);
        nr = argc.div_ceil(nc);
    }
    let end = if nul { '\0' } else { '\n' };
    let mut n = 0;
    for i in 0..nr {
        if across {
            let mut ic = 0;
            while ic < nc && n < argc {
                out.push_str(&args[n]);
                if ic < nc - 1 {
                    out.extend(std::iter::repeat_n(' ', sc.saturating_sub(widths[n])));
                }
                ic += 1;
                n += 1;
            }
        } else {
            n = i;
            loop {
                out.push_str(&args[n]);
                let w = widths[n];
                n += nr;
                if n < argc {
                    out.extend(std::iter::repeat_n(' ', sc.saturating_sub(w)));
                } else {
                    break;
                }
            }
        }
        out.push(end);
    }
}

// zsh's zexpandtabs: tabs to spaces (only leading ones unless `all`).
fn expand_tabs(s: &str, width: usize, mut pos: usize, all: bool, out: &mut String) -> usize {
    let mut at_start = true;
    for c in s.chars() {
        match c {
            '\t' if all || at_start => {
                if pos.is_multiple_of(width) {
                    out.push(' ');
                    pos += 1;
                }
                while !pos.is_multiple_of(width) {
                    out.push(' ');
                    pos += 1;
                }
            }
            '\t' => {
                pos += width - pos % width;
                out.push('\t');
            }
            '\n' | '\r' => {
                out.push(c);
                pos = 0;
                at_start = true;
            }
            _ => {
                at_start = false;
                out.push(c);
                // Columns, as zsh counts them: wide characters take two.
                let mut b = [0u8; 4];
                pos += crate::display_width(c.encode_utf8(&mut b));
            }
        }
    }
    pos
}

struct Formatted {
    out: String,
    // Output offsets where the format was reused (for print -v arrays).
    splits: Vec<usize>,
    ret: i32,
    // A fatal error: the output so far is written, nothing assigned.
    abort: bool,
}

fn math_int(s: &str, ret: &mut i32) -> i64 {
    match crate::arith::eval(s) {
        Ok(v) => v,
        Err(e) => {
            shell_err(&e);
            *ret = 1;
            0
        }
    }
}

fn pad(s: &str, width: i64, left: bool, out: &mut String) {
    let len = s.chars().count() as i64;
    let w = width.abs();
    let left = left || width < 0;
    if !left && len < w {
        out.extend(std::iter::repeat_n(' ', (w - len) as usize));
    }
    out.push_str(s);
    if left && len < w {
        out.extend(std::iter::repeat_n(' ', (w - len) as usize));
    }
}

enum CArg {
    Int(i64),
    Uint(u64),
    Float(f64),
}

// One numeric conversion through the C library, so flags, width and
// precision behave exactly as printf(3) has them.
fn c_format(flags: &str, width: i64, prec: i64, conv: char, v: CArg) -> String {
    let mut spec = format!("%{}*", flags);
    if prec >= 0 {
        spec.push_str(".*");
    }
    if !matches!(v, CArg::Float(_)) {
        spec.push_str("ll");
    }
    spec.push(conv);
    let Ok(cspec) = std::ffi::CString::new(spec) else {
        return String::new();
    };
    let w = width.clamp(-1_000_000, 1_000_000) as libc::c_int;
    let p = prec.clamp(-1, 1_000_000) as libc::c_int;
    let call = |buf: *mut libc::c_char, len: usize| -> i32 {
        // SAFETY: the format has exactly one conversion whose argument
        // types are passed below, after the `*` width (and precision).
        unsafe {
            match (&v, prec >= 0) {
                (CArg::Int(i), true) => {
                    libc::snprintf(buf, len, cspec.as_ptr(), w, p, *i as libc::c_longlong)
                }
                (CArg::Int(i), false) => {
                    libc::snprintf(buf, len, cspec.as_ptr(), w, *i as libc::c_longlong)
                }
                (CArg::Uint(u), true) => {
                    libc::snprintf(buf, len, cspec.as_ptr(), w, p, *u as libc::c_ulonglong)
                }
                (CArg::Uint(u), false) => {
                    libc::snprintf(buf, len, cspec.as_ptr(), w, *u as libc::c_ulonglong)
                }
                (CArg::Float(f), true) => libc::snprintf(buf, len, cspec.as_ptr(), w, p, *f),
                (CArg::Float(f), false) => libc::snprintf(buf, len, cspec.as_ptr(), w, *f),
            }
        }
    };
    let need = call(std::ptr::null_mut(), 0);
    if need < 0 {
        return String::new();
    }
    let mut buf = vec![0u8; need as usize + 1];
    call(buf.as_mut_ptr() as *mut libc::c_char, buf.len());
    buf.truncate(need as usize);
    String::from_utf8_lossy(&buf).into_owned()
}

// The printf engine: the format is applied, and reused while arguments
// remain.
fn format_all(name: &str, fmt: &str, mut fmttrunc: bool, args: &[String], once: bool) -> Formatted {
    let fmt: Vec<char> = fmt.chars().collect();
    let mut flen = fmt.len();
    let mut f = Formatted {
        out: String::new(),
        splits: Vec::new(),
        ret: 0,
        abort: false,
    };
    let mut argc = args.len();
    let (mut first, mut argp, mut maxarg) = (0usize, 0usize, 0usize);
    let digits = |c: &mut usize| -> usize {
        let mut v = 0usize;
        while *c < fmt.len() && fmt[*c].is_ascii_digit() {
            v = v
                .saturating_mul(10)
                .saturating_add(fmt[*c] as usize - '0' as usize);
            *c += 1;
        }
        v
    };
    macro_rules! out_of_range {
        ($n:expr) => {{
            builtin_err(name, &format!("{}: argument specifier out of range", $n));
            f.abort = true;
            return f;
        }};
    }
    loop {
        let rcount = f.out.len();
        if argp > 0 {
            f.splits.push(f.out.len());
        }
        if maxarg > 0 {
            first += maxarg;
            argc -= maxarg;
            maxarg = 0;
        }
        let mut c = 0;
        while c < flen {
            if fmt[c] != '%' {
                f.out.push(fmt[c]);
                c += 1;
                continue;
            }
            let start = c;
            c += 1;
            if fmt.get(c) == Some(&'%') {
                f.out.push('%');
                c += 1;
                continue;
            }
            let (mut width, mut prec) = (0i64, -1i64);
            let mut curarg: Option<&str> = None;
            if fmt.get(c).is_some_and(|d| ('1'..='9').contains(d)) {
                let mut e = c;
                let narg = digits(&mut e);
                if fmt.get(e) == Some(&'$') {
                    c = e + 1;
                    if narg > argc {
                        out_of_range!(narg);
                    }
                    maxarg = maxarg.max(narg);
                    curarg = Some(&args[first + narg - 1]);
                }
            }
            let mut flags = String::new();
            let mut minus = false;
            while let Some(&fc) = fmt.get(c).filter(|fc| "'0+- #".contains(**fc)) {
                if !flags.contains(fc) {
                    flags.push(fc);
                }
                minus |= fc == '-';
                c += 1;
            }
            if fmt.get(c).is_some_and(char::is_ascii_digit) {
                width = digits(&mut c) as i64;
            } else if fmt.get(c) == Some(&'*') {
                c += 1;
                if fmt.get(c).is_some_and(char::is_ascii_digit) {
                    let mut e = c;
                    let narg = digits(&mut e);
                    if fmt.get(e) == Some(&'$') {
                        c = e + 1;
                        if narg > argc || narg == 0 {
                            out_of_range!(narg);
                        }
                        maxarg = maxarg.max(narg);
                        argp = first + narg - 1;
                    }
                }
                if argp < args.len() {
                    width = math_int(&args[argp], &mut f.ret);
                    argp += 1;
                }
            }
            if fmt.get(c) == Some(&'.') {
                c += 1;
                if fmt.get(c) == Some(&'*') {
                    c += 1;
                    if fmt.get(c).is_some_and(char::is_ascii_digit) {
                        let mut e = c;
                        let narg = digits(&mut e);
                        if fmt.get(e) == Some(&'$') {
                            c = e + 1;
                            if narg > argc || narg == 0 {
                                out_of_range!(narg);
                            }
                            maxarg = maxarg.max(narg);
                            argp = first + narg - 1;
                        }
                    }
                    if argp < args.len() {
                        prec = math_int(&args[argp], &mut f.ret);
                        argp += 1;
                    }
                } else if fmt.get(c).is_some_and(char::is_ascii_digit) {
                    prec = digits(&mut c) as i64;
                } else {
                    prec = 0;
                }
            }
            if matches!(fmt.get(c), Some('l' | 'L' | 'h')) {
                c += 1;
            }
            if curarg.is_none() && argp < args.len() {
                curarg = Some(&args[argp]);
                argp += 1;
            }
            let conv = fmt.get(c).copied().filter(|&ch| ch != '\0' && c < flen);
            let left = minus && width > 0;
            let mut kind = 0;
            match conv {
                Some('c') => {
                    let ch: String = curarg
                        .and_then(|a| a.chars().next())
                        .unwrap_or('\0')
                        .to_string();
                    pad(&ch, width, left, &mut f.out);
                }
                Some(conv @ ('s' | 'b')) => match curarg {
                    Some(a) => {
                        let (mut b, cut) = if conv == 'b' {
                            getkeystring(a, Keys::PrintfArg)
                        } else {
                            (a.to_string(), false)
                        };
                        if prec >= 0 {
                            b = b.chars().take(prec as usize).collect();
                        }
                        pad(&b, width, left, &mut f.out);
                        if cut {
                            flen = c + 1;
                            fmttrunc = true;
                        }
                    }
                    None if width != 0 => pad("", width, false, &mut f.out),
                    None => {}
                },
                Some('q') => {
                    let mut q = crate::expand::backslash_quote(curarg.unwrap_or(""));
                    if curarg.is_none() {
                        q.clear();
                    }
                    if prec >= 0 {
                        q = q.chars().take(prec as usize).collect();
                    }
                    pad(&q, width, left, &mut f.out);
                }
                Some('d' | 'i') => kind = 1,
                Some('e' | 'E' | 'f' | 'g' | 'G') => kind = 2,
                Some('o' | 'u' | 'x' | 'X') => kind = 3,
                Some('n') => {
                    if let Some(a) = curarg {
                        if !crate::lexer::is_valid_name(a) {
                            shell_err(&format!("not an identifier: {}", a));
                            f.abort = true;
                            return f;
                        }
                        var_set(a, &(f.out.len() - rcount).to_string(), false);
                    }
                }
                _ => {
                    let end = if conv.is_some() { c + 1 } else { c };
                    let text: String = fmt[start..end.min(fmt.len())].iter().collect();
                    builtin_err(
                        name,
                        &format!("{}: invalid directive", text.trim_end_matches('\0')),
                    );
                    f.abort = true;
                    return f;
                }
            }
            if kind > 0 {
                let conv = conv.unwrap();
                let quoted = curarg.and_then(|a| {
                    a.strip_prefix(['\'', '"']).map(|r| {
                        r.chars().next().map_or(0, |ch| {
                            crate::shell::decode_raw_byte(ch)
                                .map(u32::from)
                                .unwrap_or(ch as u32)
                        })
                    })
                });
                let text = match (quoted, kind) {
                    (Some(cc), 2) => c_format(&flags, width, prec, conv, CArg::Float(cc as f64)),
                    (Some(cc), 1) => c_format(&flags, width, prec, conv, CArg::Int(cc as i64)),
                    (Some(cc), _) => c_format(&flags, width, prec, conv, CArg::Uint(cc as u64)),
                    (None, 1) => {
                        let v = curarg.map_or(0, |a| math_int(a, &mut f.ret));
                        c_format(&flags, width, prec, conv, CArg::Int(v))
                    }
                    (None, 2) => {
                        let v = match curarg {
                            None => 0.0,
                            Some(a) => match a.trim_start().parse::<f64>() {
                                Ok(v) => v,
                                Err(_) => match crate::arith::eval_num(a) {
                                    Ok(crate::arith::Num::F(v)) => v,
                                    Ok(crate::arith::Num::I(v)) => v as f64,
                                    Err(e) => {
                                        shell_err(&e);
                                        f.ret = 1;
                                        0.0
                                    }
                                },
                            },
                        };
                        if v.is_nan() {
                            "nan".to_string()
                        } else if v.is_infinite() {
                            if v < 0.0 { "-inf" } else { "inf" }.to_string()
                        } else {
                            c_format(&flags, width, prec, conv, CArg::Float(v))
                        }
                    }
                    (None, _) => {
                        let v = match curarg {
                            None => 0,
                            Some(a) => match a.replace('_', "").parse::<u64>() {
                                Ok(v) if !a.starts_with('_') => v,
                                _ => math_int(a, &mut f.ret) as u64,
                            },
                        };
                        c_format(&flags, width, prec, conv, CArg::Uint(v))
                    }
                };
                f.out.push_str(&text);
            }
            if maxarg > 0 && argp - first > maxarg {
                maxarg = argp - first;
            }
            c += 1;
        }
        if maxarg > 0 {
            argp = first + maxarg;
        }
        if !(argp < args.len() && argp != first && !fmttrunc && !once) {
            break;
        }
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_follow_the_mode() {
        assert_eq!(
            getkeystring("\\101\\q", Keys::Print),
            ("A".to_string() + "q", false)
        );
        assert_eq!(getkeystring("\\101\\q", Keys::PrintfFmt).0, "A\\q");
        assert_eq!(getkeystring("\\101\\0101", Keys::PrintfArg).0, "\\101A");
        assert_eq!(
            getkeystring("a\\cb", Keys::PrintfArg),
            ("a".to_string(), true)
        );
    }

    #[test]
    fn numbers_use_c_semantics() {
        assert_eq!(c_format("0+", 6, 3, 'd', CArg::Int(3)), "  +003");
        assert_eq!(c_format("#", 0, -1, 'x', CArg::Uint(1)), "0x1");
        assert_eq!(c_format("", 0, 1, 'E', CArg::Float(66.0)), "6.6E+01");
    }
}
