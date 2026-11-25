//! Word expansion: brace, tilde, parameter, command substitution,
//! arithmetic, field splitting, pathname expansion and quote removal.
//!
//! Words arrive raw from the parser, quotes included. Expansion walks a
//! word once and produces characters tagged with how they were produced,
//! so later stages know which characters may be split on or globbed.

use crate::lexer::{
    scan_arith, scan_bquote, scan_brace, scan_dollar_squote, scan_dquote, scan_paren, scan_squote,
};
use crate::shell::{
    assoc_get, assoc_keys, assoc_values, opt_set, var_get, var_get_opt, var_set, with_shell,
};
use crate::types::{OPT_NOGLOB, OPT_NOUNSET};

/// An expansion failed. The message has already been printed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExpError {
    pub status: i32,
}

pub type ExpResult<T> = Result<T, ExpError>;

fn fail<T>(msg: &str) -> ExpResult<T> {
    // An empty message: the error was already reported.
    if !msg.is_empty() {
        crate::shell::shell_err(msg);
    }
    Err(ExpError { status: 1 })
}

// Character tags.
const Q: u8 = 1; // quoted: never split or globbed
const S: u8 = 2; // result of an unquoted expansion: subject to field splitting
const MARK: u8 = 4; // zero-width marker: the field exists even if empty
const NG: u8 = 8; // expansion result that pathname expansion leaves literal
const B: u8 = 16; // unquoted literal text: its braces and commas brace-expand

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Unquoted,
    Dq,
    Heredoc,
}

struct Ctx {
    fields: Vec<Vec<(char, u8)>>,
    // Lists such as "$@" produce separate fields.
    split: bool,
    // Assignment value: tilde expansion also follows ':'.
    assign: bool,
    // Count of non-list emissions; used to decide whether a pair of
    // double quotes that contained only "$@" produces a field.
    content: usize,
    at_lists: usize,
    // Tag for unquoted literal characters: 0 normally, S inside the word
    // of an unquoted `${x-word}`, whose text is part of the expansion.
    unquoted_lit: u8,
    // After `${^arr}`: later text is appended to every field from here.
    rc_from: Option<usize>,
    // RC_EXPAND_PARAM with an empty array: the whole word goes.
    rc_dead: bool,
    // Tags for unquoted parameter and command substitution results, from
    // SH_WORD_SPLIT and GLOB_SUBST: zsh splits parameters only with
    // SH_WORD_SPLIT, always splits command substitution, and globs
    // neither unless GLOB_SUBST.
    param_tag: u8,
    subst_tag: u8,
    // MAGIC_EQUAL_SUBST argument: `~` may follow `=`.
    magic_eq: bool,
    // Brace expansion follows: an `=cmd` holding braces waits for it.
    braces_after: bool,
    // Inside the default word of `${x:-word}`: `=cmd` expands at index 0.
    in_braced_word: bool,
}

impl Ctx {
    fn new(split: bool, assign: bool) -> Self {
        Ctx {
            fields: vec![Vec::new()],
            split,
            assign,
            content: 0,
            at_lists: 0,
            unquoted_lit: 0,
            rc_from: None,
            rc_dead: false,
            param_tag: 0,
            subst_tag: 0,
            magic_eq: false,
            braces_after: false,
            in_braced_word: false,
        }
        .with_option_tags()
    }

    fn with_option_tags(mut self) -> Self {
        let (split, glob) = with_shell(|s| {
            (
                crate::options::is_set_in(s, "shwordsplit"),
                crate::options::is_set_in(s, "globsubst"),
            )
        });
        let no_glob = if glob { 0 } else { NG };
        self.param_tag = if split { S } else { 0 } | no_glob;
        self.subst_tag = S | no_glob;
        self
    }

    fn push(&mut self, c: char, flag: u8) {
        match self.rc_from {
            Some(start) => self.fields[start..]
                .iter_mut()
                .for_each(|f| f.push((c, flag))),
            None => self.fields.last_mut().unwrap().push((c, flag)),
        }
        self.content += 1;
    }

    fn mark(&mut self) {
        self.push('\0', MARK);
    }

    fn new_field(&mut self) {
        self.rc_from = None;
        self.fields.push(Vec::new());
    }

    fn joined(&self) -> String {
        self.fields
            .iter()
            .flatten()
            .filter(|(_, f)| f & MARK == 0)
            .map(|(c, _)| *c)
            .collect()
    }
}

enum Val {
    Unset,
    Str(String),
    // Items and whether the list joins into one word inside quotes
    // (`$*`, `${a[*]}`) rather than staying separate (`$@`).
    List(Vec<String>, bool),
    // `${^arr}`: each element combines with the surrounding text.
    Rc(Vec<String>),
    // Expansion of a `${x-word}` word, quoting tags kept.
    Tagged(Vec<Vec<(char, u8)>>),
}

fn ifs() -> String {
    var_get_opt("IFS").unwrap_or_else(|| " \t\n\0".to_string())
}

fn ifs_first() -> String {
    ifs()
        .chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_default()
}

fn pos_params() -> Vec<String> {
    with_shell(|s| s.pos_params.clone())
}

fn lookup(name: &str) -> Val {
    let target = with_shell(|s| s.resolve_ref_target(name));
    let target_base = crate::shell::split_target_pub(&target.name);
    if crate::modules::has_autoparam(&target_base) {
        let missing_or_bypassed = !crate::modules::feature_enabled(&format!("p:{}", target_base))
            && (target.scope.is_some()
                || with_shell(|s| {
                    s.param(&target_base).is_none_or(|tp| {
                        name != target_base
                            && tp.level > 0
                            && s.param(name).is_some_and(|rp| rp.ref_level == Some(0))
                    })
                }));
        if missing_or_bypassed {
            if let Some(Err(msg)) = crate::modules::autoload_feature(&format!("p:{}", target_base))
            {
                if !msg.is_empty() {
                    crate::shell::shell_err(&msg);
                    crate::exec::abort_command(1);
                }
                return Val::Unset;
            }
        }
    }
    let _swap = crate::shell::RefSwap::enter(name);
    crate::zparam::ensure(&target_base);
    match name {
        "@" => return Val::List(pos_params(), false),
        "*" => return Val::List(pos_params(), true),
        // zsh's $dirstack: the directory stack without the current one.
        "dirstack" if !crate::shell::var_exists("dirstack") => {
            return Val::List(with_shell(|s| s.dir_stack.clone()), true)
        }
        // zsh's $argv is the positional parameters.
        "argv" if !crate::shell::var_exists("argv") => return Val::List(pos_params(), true),
        "jobstates" if !crate::shell::var_exists("jobstates") => {
            let v = crate::jobs::job_ids()
                .into_iter()
                .filter_map(crate::jobs::job_state_text)
                .collect();
            return Val::List(v, true);
        }
        // zsh's $status is $? (not in sh, where it is an ordinary name).
        "status" if !crate::shell::var_exists("status") && !crate::exec::sh_emulation_pub() => {
            return Val::Str(with_shell(|s| s.last_status).to_string());
        }
        "zsh_eval_context" if !crate::shell::var_exists("zsh_eval_context") => {
            return Val::List(crate::shell::eval_context(), true)
        }
        "ZSH_EVAL_CONTEXT" if !crate::shell::var_exists("ZSH_EVAL_CONTEXT") => {
            return Val::Str(crate::shell::eval_context().join(":"))
        }
        // zsh/parameter's $funcstack: the functions (and sourced files)
        // running, innermost first.
        "funcstack" if !crate::shell::var_exists("funcstack") => {
            let mut v: Vec<String> =
                crate::shell::with_err_ctx(|e| e.stack.iter().map(|(n, _)| n.clone()).collect());
            v.reverse();
            return Val::List(v, true);
        }
        // $functrace / $funcfiletrace: where each of those was called
        // from, innermost first.
        "functrace" | "funcfiletrace" | "funcsourcetrace" if !crate::shell::var_exists(name) => {
            let mut v: Vec<String> = crate::shell::with_err_ctx(|e| {
                e.calls
                    .iter()
                    .map(|(t, f, d)| match name {
                        "functrace" => t.clone(),
                        "funcfiletrace" => f.clone(),
                        _ => d.clone(),
                    })
                    .collect()
            });
            v.reverse();
            return Val::List(v, true);
        }
        "LINENO" if !crate::shell::var_exists("LINENO") => {
            return Val::Str(crate::shell::lineno().to_string())
        }
        _ => {}
    }
    if with_shell(|s| {
        s.param(name)
            .is_some_and(|p| p.attrs & crate::params::NAMEREF != 0 && p.text().is_empty())
    }) {
        return Val::Unset;
    }
    let resolved = crate::shell::resolve_ref(name);
    if resolved != name {
        return match resolved.find('[') {
            Some(k) if resolved.ends_with(']') => {
                param_value(&resolved[..k], Some(&resolved[k + 1..resolved.len() - 1]))
                    .unwrap_or(Val::Unset)
            }
            _ => lookup(&resolved),
        };
    }
    if let Some(items) = with_shell(|s| s.array(name).cloned()) {
        return Val::List(items, true);
    }
    if let Some(items) = assoc_values(name) {
        return Val::List(items, true);
    }
    match var_get_opt(name) {
        Some(v) => Val::Str(v),
        None => Val::Unset,
    }
}

fn check_set(name: &str, val: &Val) -> ExpResult<()> {
    if matches!(val, Val::Unset) && name != "@" && name != "*" && opt_set(OPT_NOUNSET) {
        return fail(&format!("{}: parameter not set", name));
    }
    Ok(())
}

fn emit_str(ctx: &mut Ctx, s: &str, quoted: bool) {
    let flag = if quoted { Q } else { ctx.param_tag };
    emit_tagged(ctx, s, flag);
}

// Command substitution output.
fn emit_subst(ctx: &mut Ctx, s: &str, quoted: bool) {
    let flag = if quoted { Q } else { ctx.subst_tag };
    emit_tagged(ctx, s, flag);
}

fn emit_tagged(ctx: &mut Ctx, s: &str, flag: u8) {
    // A pattern value's backslash quotes within the value only: a final
    // unpaired one is literal, not an escape of the text after it.
    let trailing = s.chars().rev().take_while(|&c| c == '\\').count();
    let lone = flag & (Q | NG) == 0 && trailing % 2 == 1;
    let n = s.chars().count();
    for (k, c) in s.chars().enumerate() {
        ctx.push(c, if lone && k + 1 == n { flag | Q } else { flag });
    }
    ctx.content += 1;
}

fn emit_list(ctx: &mut Ctx, items: &[String], star: bool, quoted: bool) {
    if !ctx.split || (quoted && star) {
        let sep = ifs_first();
        emit_str(ctx, &items.join(&sep), quoted);
        return;
    }
    if quoted {
        ctx.at_lists += 1;
        for (k, item) in items.iter().enumerate() {
            if k > 0 {
                ctx.new_field();
            }
            ctx.mark();
            for c in item.chars() {
                ctx.push(c, Q);
            }
        }
    } else {
        for (k, item) in items.iter().enumerate() {
            if k > 0 {
                ctx.new_field();
            }
            let tag = ctx.param_tag;
            for c in item.chars() {
                ctx.push(c, tag);
            }
        }
    }
}

fn emit_val(ctx: &mut Ctx, val: &Val, quoted: bool) {
    match val {
        Val::Unset => emit_str(ctx, "", quoted),
        Val::Str(s) => emit_str(ctx, s, quoted),
        Val::List(items, star) => emit_list(ctx, items, *star, quoted),
        Val::Tagged(fields) => {
            for (k, field) in fields.iter().enumerate() {
                if k > 0 {
                    ctx.new_field();
                }
                for &(c, t) in field {
                    ctx.push(c, t);
                }
            }
            ctx.content += 1;
        }
        Val::Rc(items) => {
            if items.is_empty() {
                ctx.rc_dead = true;
                return;
            }
            // After an earlier `${^...}` in the word, every field it made
            // combines with every item.
            let start = ctx.rc_from.unwrap_or(ctx.fields.len() - 1);
            let prefixes = ctx.fields.split_off(start);
            let tag = if quoted { Q } else { ctx.param_tag };
            for prefix in prefixes {
                for item in items {
                    let mut f = prefix.clone();
                    f.extend(item.chars().map(|c| (c, tag)));
                    ctx.fields.push(f);
                }
            }
            ctx.rc_from = Some(start);
            ctx.content += 1;
        }
    }
}

// Splits `chars[i..]` for an unterminated construct: returns the end of
// the inner text and the index to resume at.
fn bounds(found: Option<usize>, len: usize) -> (usize, usize) {
    match found {
        Some(e) => (e - 1, e),
        None => (len, len),
    }
}

fn walk(chars: &[char], mode: Mode, ctx: &mut Ctx) -> ExpResult<()> {
    let n = chars.len();
    let lit = if mode == Mode::Unquoted {
        ctx.unquoted_lit
    } else {
        Q
    };
    let mut i = 0;
    while i < n {
        let c = chars[i];
        match c {
            '\\' => {
                let next = chars.get(i + 1).copied();
                let escapes = match mode {
                    Mode::Unquoted => next.is_some(),
                    Mode::Dq => matches!(next, Some('$' | '`' | '"' | '\\' | '\n')),
                    Mode::Heredoc => matches!(next, Some('$' | '`' | '\\' | '\n')),
                };
                if escapes {
                    let e = next.unwrap();
                    if e != '\n' {
                        ctx.push(e, Q);
                    }
                    i += 2;
                } else {
                    ctx.push('\\', Q);
                    i += 1;
                }
            }
            '\'' if mode == Mode::Unquoted => {
                let (ie, mut next) = bounds(scan_squote(chars, i), n);
                for &q in &chars[i + 1..ie.max(i + 1)] {
                    ctx.push(q, Q);
                }
                // RC_QUOTES: `''` inside single quotes is a quote.
                while chars.get(next) == Some(&'\'') && crate::options::is_set("rcquotes") {
                    ctx.push('\'', Q);
                    let (ie2, next2) = bounds(scan_squote(chars, next), n);
                    for &q in &chars[next + 1..ie2.max(next + 1)] {
                        ctx.push(q, Q);
                    }
                    next = next2;
                }
                ctx.mark();
                i = next;
            }
            '"' if mode == Mode::Unquoted => {
                let (ie, next) = bounds(scan_dquote(chars, i), n);
                let before_content = ctx.content;
                let before_at = ctx.at_lists;
                walk(&chars[i + 1..ie.max(i + 1)], Mode::Dq, ctx)?;
                if ctx.content > before_content || ctx.at_lists == before_at {
                    ctx.mark();
                }
                i = next;
            }
            // Quotes nested inside `${...}` within double quotes.
            '"' if mode == Mode::Dq => i += 1,
            '`' => {
                let (ie, next) = bounds(scan_bquote(chars, i), n);
                let src = unescape_bquote(&chars[i + 1..ie.max(i + 1)], mode == Mode::Dq);
                let out = command_subst(&src)?;
                emit_subst(ctx, &out, mode != Mode::Unquoted);
                i = next;
            }
            '$' => i = expand_dollar(chars, i, mode, ctx)?,
            // `=(cmd)`: a temporary file holding cmd's output.
            '=' if mode == Mode::Unquoted
                && i == 0
                && (ctx.unquoted_lit == 0 || ctx.in_braced_word)
                && chars.get(1) == Some(&'(')
                && !sh_mode() =>
            {
                let Some(next) = scan_paren(chars, 1) else {
                    return fail("unterminated `=(...)'");
                };
                let ie = next - 1;
                let src: String = chars[2..ie.max(2)].iter().collect();
                match crate::exec::output_file_subst(&src) {
                    Some(path) => {
                        for d in path.chars() {
                            ctx.push(d, Q);
                        }
                    }
                    None => return Err(ExpError { status: 1 }),
                }
                i = next;
            }
            // EQUALS: `=cmd` is the path of command cmd.
            '=' if mode == Mode::Unquoted
                && i == 0
                && (ctx.unquoted_lit == 0 || ctx.in_braced_word)
                && chars.get(1).is_some_and(|c| {
                    c.is_alphanumeric() || "_-.".contains(*c) || (ctx.in_braced_word && *c == ' ')
                })
                && !ctx.magic_eq
                && !(ctx.braces_after && chars.contains(&'{'))
                && crate::options::is_set("equals") =>
            {
                let end = (1..n)
                    .find(|&k| matches!(chars[k], '(' | '[' | '/' | '$' | '\'' | '"' | '`'))
                    .unwrap_or(n);
                let name: String = chars[1..end].iter().collect();
                let path = crate::shell::find_command(&name);
                match path {
                    Some(p) => {
                        for d in p.chars() {
                            ctx.push(d, Q);
                        }
                    }
                    None => return fail(&format!("{} not found", name)),
                }
                i = end;
            }
            // zsh: `<(...)` and `>(...)` anywhere in a word.
            '<' | '>'
                if mode == Mode::Unquoted
                    && IN_BRACES.with(|b| b.get()) == 0
                    && (i > 0 || ctx.unquoted_lit == 0 || ctx.in_braced_word)
                    && chars.get(i + 1) == Some(&'(')
                    // Not the end of a numeric glob such as `<->(.)`.
                    && !(c == '>'
                        && chars[..i].iter().rposition(|&x| x == '<').is_some_and(|k| {
                            crate::lexer::numeric_range_len(chars, k) == Some(i + 1 - k)
                        })) =>
            {
                let Some(next) = scan_paren(chars, i + 1) else {
                    return fail(&format!("unterminated `{}(...)'", c));
                };
                let ie = next - 1;
                let src: String = chars[i + 2..ie.max(i + 2)].iter().collect();
                match crate::exec::process_subst(&src, c == '<') {
                    Some(path) => {
                        for d in path.chars() {
                            ctx.push(d, Q);
                        }
                    }
                    None => return Err(ExpError { status: 1 }),
                }
                i = next;
            }
            '~' if mode == Mode::Unquoted
                && (i == 0
                    || (ctx.assign && chars[i - 1] == ':')
                    || (ctx.magic_eq && chars[i - 1] == '=')) =>
            {
                // `~[name]`: a dynamic named directory from zsh_directory_name.
                if chars.get(i + 1) == Some(&'[') {
                    if let Some(close) = (i + 2..n).find(|&k| chars[k] == ']') {
                        let name: String = chars[i + 2..close].iter().collect();
                        match dynamic_dir('n', &name).and_then(|r| r.into_iter().next()) {
                            Some(dir) => {
                                for d in dir.chars() {
                                    ctx.push(d, Q);
                                }
                                i = close + 1;
                                continue;
                            }
                            None if crate::options::is_set("nomatch") => {
                                return fail(&format!("no directory expansion: ~[{}]", name));
                            }
                            None => {
                                for &d in &chars[i..=close] {
                                    ctx.push(d, Q);
                                }
                                i = close + 1;
                                continue;
                            }
                        }
                    }
                }
                match tilde(chars, i, ctx.assign) {
                    Ok(Some((dir, next))) => {
                        for d in dir.chars() {
                            ctx.push(d, Q);
                        }
                        i = next;
                    }
                    Ok(None) => {
                        ctx.push('~', 0);
                        i += 1;
                    }
                    Err(msg) => return fail(&msg),
                }
            }
            _ => {
                ctx.push(c, if mode == Mode::Unquoted { lit | B } else { lit });
                i += 1;
            }
        }
    }
    Ok(())
}

/// zsh's dynamic named directories: calls zsh_directory_name and the
/// functions in $zsh_directory_name_functions with `kind` (n: name to
/// directory, d: directory to name, c: completion) and `arg`; the $reply
/// of the first that succeeds.
pub fn dynamic_dir(kind: char, arg: &str) -> Option<Vec<String>> {
    let mut names = vec!["zsh_directory_name".to_string()];
    names.extend(
        with_shell(|s| s.array("zsh_directory_name_functions").cloned()).unwrap_or_default(),
    );
    for f in names {
        let Some((body, line)) =
            with_shell(|s| s.functions.get(&f).map(|d| (d.body.clone(), d.line)))
        else {
            continue;
        };
        let saved = with_shell(|s| s.param("reply").cloned());
        with_shell(|s| {
            s.remove_param("reply");
        });
        let saved_icf = crate::zle::with_active(|z| {
            let icf = z.comp.incompfunc;
            z.comp.incompfunc = 0;
            icf
        });
        let st = crate::exec::call_function(&body, &[f, kind.to_string(), arg.to_string()], line);
        if let Some(icf) = saved_icf {
            crate::zle::with_active(|z| {
                z.comp.incompfunc = icf;
            });
        }
        let reply = with_shell(|s| {
            let r = s
                .array("reply")
                .cloned()
                .or_else(|| s.scalar("reply").map(|v| vec![v.to_string()]));
            match saved {
                Some(p) => {
                    s.params.insert("reply".to_string(), p);
                }
                None => {
                    s.remove_param("reply");
                }
            }
            r
        });
        if st == 0 {
            return Some(reply.unwrap_or_default());
        }
    }
    None
}

fn tilde(chars: &[char], i: usize, assign: bool) -> Result<Option<(String, usize)>, String> {
    let mut j = i + 1;
    while j < chars.len() && chars[j] != '/' && !(assign && chars[j] == ':') {
        if matches!(chars[j], '\'' | '"' | '\\' | '$' | '`') {
            return Ok(None);
        }
        j += 1;
    }
    let user: String = chars[i + 1..j].iter().collect();
    let dir = match user.as_str() {
        "" => var_get_opt("HOME").or_else(|| home_of(None)),
        "+" => var_get_opt("PWD"),
        "-" => var_get_opt("OLDPWD"),
        u => match crate::shell::named_dir(u).or_else(|| home_of(Some(u))) {
            Some(d) => Some(d),
            None => {
                if !u.is_empty()
                    && u.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
                    && crate::options::is_set("nomatch")
                {
                    return Err(format!("no such user or named directory: {}", u));
                }
                None
            }
        },
    };
    Ok(dir.map(|d| (d, j)))
}

fn home_of(user: Option<&str>) -> Option<String> {
    unsafe {
        let pw = match user {
            Some(u) => {
                let cu = std::ffi::CString::new(u).ok()?;
                libc::getpwnam(cu.as_ptr())
            }
            None => libc::getpwuid(libc::getuid()),
        };
        if pw.is_null() || (*pw).pw_dir.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr((*pw).pw_dir)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

fn unescape_bquote(inner: &[char], in_dq: bool) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < inner.len() {
        if inner[i] == '\\' && i + 1 < inner.len() {
            let n = inner[i + 1];
            if n == '$' || n == '`' || n == '\\' || (in_dq && n == '"') {
                out.push(n);
                i += 2;
                continue;
            }
        }
        out.push(inner[i]);
        i += 1;
    }
    out
}

fn command_subst(src: &str) -> ExpResult<String> {
    // The command is new code, outside any enclosing `${...}`.
    let saved = IN_BRACES.with(|b| b.replace(0));
    let r = command_subst_braces(src);
    IN_BRACES.with(|b| b.set(saved));
    r
}

fn command_subst_braces(src: &str) -> ExpResult<String> {
    // `$(<file)` reads the file without running a command.
    let trimmed = src.trim();
    if let Some(file) = trimmed
        .strip_prefix('<')
        .filter(|f| !f.starts_with(['<', '&', '(']))
    {
        let path = expand_string(file.trim())?;
        let (mut out, status) = match std::fs::read(&path) {
            Ok(b) => (crate::shell::bytes_to_zstr(&b), 0),
            Err(e) => {
                eprintln!("meowsh: {}: {}", path, e);
                (String::new(), 1)
            }
        };
        while out.ends_with('\n') {
            out.pop();
        }
        with_shell(|s| s.last_cmdsub_status = Some(status));
        return Ok(out);
    }
    let (mut out, status) = crate::exec::command_subst(src);
    while out.ends_with('\n') {
        out.pop();
    }
    with_shell(|s| s.last_cmdsub_status = Some(status));
    Ok(out)
}

fn eval_arith_str(expr: &str) -> ExpResult<i64> {
    match crate::arith::eval(expr) {
        Ok(v) => Ok(v),
        Err(e) => fail(&e),
    }
}

// `$(( ... ))` as text. A leading `[#base]` gives the output base, with
// its `base#` prefix (or C's `0x`/`0` with C_BASES); `[##base]` omits it.
// Digits in groups of `g` from the right (or from the left, for a
// fraction), joined by underscores.
fn group_digits(d: &str, g: usize, from_left: bool) -> String {
    let chars: Vec<char> = d.chars().collect();
    let mut out = String::new();
    for (k, c) in chars.iter().enumerate() {
        let boundary = if from_left { k } else { chars.len() - k };
        if k > 0 && boundary % g == 0 {
            out.push('_');
        }
        out.push(*c);
    }
    out
}

// `$(( [#base_group] expr ))`: the output base (`##` drops the prefix)
// and digit grouping with underscores.
fn arith_text(expr: &[char]) -> ExpResult<String> {
    arith_text_depth(expr, 0)
}

fn arith_text_depth(expr: &[char], depth: usize) -> ExpResult<String> {
    let s: String = expr.iter().collect();
    let t = s.trim_start();
    // `[$fmt]`: the base spec may come from a parameter.
    if depth < 16 && t.starts_with('[') && !t.starts_with("[#") {
        if let Some(close) = t.find(']') {
            let inner: Vec<char> = t[1..close].chars().collect();
            if inner.contains(&'$') {
                let spec = word_string(&inner, Mode::Dq)?;
                let rebuilt: Vec<char> = format!("[{}]{}", spec, &t[close + 1..]).chars().collect();
                return arith_text_depth(&rebuilt, depth + 1);
            }
        }
    }
    if let Some(rest) = t.strip_prefix("[#") {
        if let Some(close) = rest.find(']') {
            let spec = &rest[..close];
            let (bare, spec) = match spec.strip_prefix('#') {
                Some(d) => (true, d),
                None => (false, spec),
            };
            let (digits, group) = match spec.split_once('_') {
                Some((d, g)) => (d, Some(g.trim().parse::<usize>().unwrap_or(3).max(1))),
                None => (spec, None),
            };
            let base = if digits.trim().is_empty() && group.is_some() {
                Some(10)
            } else {
                digits.trim().parse::<u32>().ok()
            };
            if let Some(base) = base {
                let body: Vec<char> = rest[close + 1..].chars().collect();
                let v = arith_num(&body)?;
                if let (crate::arith::Num::F(_), Some(g)) = (v, group) {
                    let text = v.to_string();
                    let (sign, text) = match text.strip_prefix('-') {
                        Some(t) => ("-", t.to_string()),
                        None => ("", text),
                    };
                    let (int, frac) = text.split_once('.').unwrap_or((&text, ""));
                    return Ok(format!(
                        "{}{}.{}",
                        sign,
                        group_digits(int, g, false),
                        group_digits(frac, g, true)
                    ));
                }
                let n = v.as_int();
                let text = if !bare {
                    crate::shell::format_int(n, base)
                } else if !(2..=36).contains(&base) {
                    n.to_string()
                } else {
                    let mut u = n.unsigned_abs();
                    let mut digits = Vec::new();
                    loop {
                        digits.push(
                            std::char::from_digit((u % base as u64) as u32, base)
                                .unwrap_or('0')
                                .to_ascii_uppercase(),
                        );
                        u /= base as u64;
                        if u == 0 {
                            break;
                        }
                    }
                    let sign = if n < 0 { "-" } else { "" };
                    format!("{}{}", sign, digits.iter().rev().collect::<String>())
                };
                let Some(g) = group else {
                    return Ok(text);
                };
                // Group only the digits, after any sign and base prefix.
                let split = text
                    .find('#')
                    .map(|k| k + 1)
                    .or_else(|| text.find(['x', 'X']).map(|k| k + 1))
                    .unwrap_or(usize::from(text.starts_with('-')));
                return Ok(format!(
                    "{}{}",
                    &text[..split],
                    group_digits(&text[split..], g, false)
                ));
            }
        }
    }
    Ok(arith_num(expr)?.to_string())
}

fn arith_num(expr: &[char]) -> ExpResult<crate::arith::Num> {
    let mut ctx = Ctx::new(false, false);
    walk(expr, Mode::Dq, &mut ctx)?;
    let text = ctx.joined();
    crate::arith::eval_num(&text).or_else(|e| fail(&e))
}

/// Expands and evaluates the text of an arithmetic command; the result
/// is true when non-zero.
pub fn eval_arith(expr: &str) -> ExpResult<bool> {
    let chars: Vec<char> = expr.chars().collect();
    arith_num(&chars).map(crate::arith::Num::truthy)
}

fn is_special_param(c: char) -> bool {
    matches!(c, '@' | '*' | '#' | '?' | '-' | '$' | '!' | '0'..='9')
}

fn expand_dollar(chars: &[char], i: usize, mode: Mode, ctx: &mut Ctx) -> ExpResult<usize> {
    let n = chars.len();
    let quoted = mode != Mode::Unquoted;
    let lit = if quoted { Q } else { 0 };
    let Some(&next) = chars.get(i + 1) else {
        ctx.push('$', lit);
        return Ok(i + 1);
    };
    match next {
        '{' => {
            let (ie, end) = bounds(scan_brace(chars, i + 1, mode == Mode::Dq), n);
            if ie == n {
                return fail("bad substitution");
            }
            // A nofork substitution is parsed as a command list: a `}`
            // straight after it has nothing to close.
            let nofork = matches!(chars.get(i + 2), Some('|' | ' ' | '\t' | '\n'));
            if nofork
                && chars.get(end) == Some(&'}')
                && mode == Mode::Unquoted
                && !crate::options::is_set("ignorebraces")
            {
                return fail("parse error near `}'");
            }
            expand_braced(&chars[i + 2..ie], mode, ctx)?;
            Ok(end)
        }
        // zsh's old `$[expr]`, the same as `$((expr))`.
        '[' if crate::lexer::scan_dollar_bracket(chars, i + 1).is_some() => {
            let end = crate::lexer::scan_dollar_bracket(chars, i + 1).unwrap();
            let v = arith_text(&chars[i + 2..end - 1])?;
            emit_str(ctx, &v, quoted);
            Ok(end)
        }
        // `$(\<newline>(`: the escaped newline goes, so this can still be
        // `$((...))` (the lexer joins the line before deciding).
        '(' if chars.get(i + 2) == Some(&'\\')
            && chars.get(i + 3) == Some(&'\n')
            && chars.get(i + 4) == Some(&'(') =>
        {
            let joined: Vec<char> = chars[..i + 2]
                .iter()
                .chain(&chars[i + 4..])
                .copied()
                .collect();
            Ok(expand_dollar(&joined, i, mode, ctx)? + 2)
        }
        '(' => {
            let arith_end = scan_arith(chars, i + 1);
            let Some(end) = arith_end.or_else(|| scan_paren(chars, i + 1)) else {
                return fail("unterminated command substitution");
            };
            if arith_end.is_some() {
                let v = arith_text(&chars[i + 3..end - 2])?;
                emit_str(ctx, &v, quoted);
            } else {
                let src: String = chars[i + 2..end - 1].iter().collect();
                let out = command_subst(&src)?;
                emit_subst(ctx, &out, quoted);
            }
            Ok(end)
        }
        '\'' if mode == Mode::Unquoted => {
            let (ie, end) = bounds(scan_dollar_squote(chars, i + 1), n);
            let s = dollar_squote_text(&chars[i + 2..ie.max(i + 2)])?;
            for c in s.chars() {
                ctx.push(c, Q);
            }
            ctx.mark();
            Ok(end)
        }
        // zsh short forms: $+name, $#name, $=name, $^name, $~name, each
        // optionally with a subscript.
        '+' | '#' | '=' | '^' | '~'
            if chars.get(i + 2).is_some_and(|&c| {
                crate::lexer::is_name_start(c)
                    || (next == '#' && c.is_ascii_digit())
                    || (next != '#' && (c.is_ascii_digit() || c == '@' || c == '*'))
                    // zsh: `$#*` and `$#@` are `${#*}`, `${#@}`.
                    || (next == '#' && matches!(c, '@' | '*') && !crate::exec::sh_emulation_pub())
            }) =>
        {
            let end = match chars[i + 2] {
                '@' | '*' => i + 3,
                c if c.is_ascii_digit() => {
                    let mut e = i + 2;
                    while chars.get(e).is_some_and(char::is_ascii_digit) {
                        e += 1;
                    }
                    e
                }
                _ => name_with_subscript(chars, i + 2),
            };
            expand_braced(&chars[i + 1..end], mode, ctx)?;
            Ok(end)
        }
        c if crate::lexer::is_name_start(c) => {
            let mut j = i + 1;
            while j < n && crate::lexer::is_name_char(chars[j]) {
                j += 1;
            }
            // zsh: `$name[sub]` subscripts without braces.
            // (Not with KSH_ARRAYS, where `$a[2]` is `${a}[2]`.)
            let end = name_with_subscript(chars, i + 1);
            if end > j && !ksh_arrays() {
                expand_braced(&chars[i + 1..end], mode, ctx)?;
                return Ok(end);
            }
            let name: String = chars[i + 1..j].iter().collect();
            let mut val = ksh_first(&name, lookup(&name));
            if !quoted && crate::options::is_set("rcexpandparam") {
                if let Val::List(items, _) = val {
                    val = Val::Rc(items);
                }
            }
            // KSH_ARRAYS: a `[...]` after `$name` is plain text.
            if end > j && ksh_arrays() {
                check_set(&name, &val)?;
                emit_val(ctx, &val, quoted);
                for &c in &chars[j..end] {
                    ctx.push(c, Q);
                }
                return Ok(end);
            }
            check_set(&name, &val)?;
            // zsh: `$name:h` and other modifiers without braces.
            if chars.get(j) == Some(&':') && !crate::exec::sh_emulation_pub() {
                let (val, used) = unbraced_modifiers(val, &chars[j..])?;
                emit_val(ctx, &val, quoted);
                return Ok(j + used);
            }
            emit_val(ctx, &val, quoted);
            Ok(j)
        }
        c if is_special_param(c) => {
            // zsh reads all the digits of `$11`; sh and ksh only one.
            let mut end = i + 2;
            if c.is_ascii_digit()
                && with_shell(|s| {
                    matches!(
                        s.emulation,
                        crate::options::Emulation::Zsh | crate::options::Emulation::Csh
                    )
                })
            {
                while end < n && chars[end].is_ascii_digit() {
                    end += 1;
                }
            }
            let name: String = chars[i + 1..end].iter().collect();
            // zsh: `$@[2,-1]` and `$*[1]` subscript as `${@[2,-1]}` does.
            if chars.get(end) == Some(&'[')
                && matches!(c, '@' | '*')
                && !crate::exec::sh_emulation_pub()
                && !ksh_arrays()
            {
                let close = scan_bracket_end(chars, end);
                if close < n {
                    expand_braced(&chars[i + 1..=close], mode, ctx)?;
                    return Ok(close + 1);
                }
            }
            let val = lookup(&name);
            check_set(&name, &val)?;
            // Modifiers apply here too: `$@:a`, `$0:t`.
            if chars.get(end) == Some(&':') && !crate::exec::sh_emulation_pub() {
                let (val, used) = unbraced_modifiers(val, &chars[end..])?;
                emit_val(ctx, &val, quoted);
                return Ok(end + used);
            }
            emit_val(ctx, &val, quoted);
            Ok(end)
        }
        _ => {
            ctx.push('$', lit);
            Ok(i + 1)
        }
    }
}

// End of a name starting at `i`, plus a `[...]` subscript directly after
// it when the brackets close.
fn name_with_subscript(chars: &[char], i: usize) -> usize {
    let mut j = i;
    while j < chars.len() && crate::lexer::is_name_char(chars[j]) {
        j += 1;
    }
    if chars.get(j) != Some(&'[') {
        return j;
    }
    let mut depth = 0;
    let mut k = j;
    while k < chars.len() {
        match chars[k] {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return k + 1;
                }
            }
            '\\' => k += 1,
            // Quotes may appear inside a subscript (or a single raw `"`).
            q @ ('"' | '\'') => {
                let mut p = k + 1;
                while p < chars.len() && chars[p] != q && chars[p] != '\n' {
                    if chars[p] == '\\' && q == '"' {
                        p += 1;
                    }
                    p += 1;
                }
                if chars.get(p) == Some(&q) {
                    k = p;
                }
            }
            // Blanks may appear inside, as in "$x[p + 1, -1]"; words the
            // lexer split never reach here with them.
            '\n' => return j,
            _ => {}
        }
        k += 1;
    }
    j
}

// Parses a parameter name and optional subscript at the start of `body`.
// Returns (name, subscript, index after them).
fn parse_param(body: &[char]) -> Option<(String, Option<String>, usize)> {
    let first = *body.first()?;
    let mut j;
    if crate::lexer::is_name_start(first) || first == '.' {
        j = crate::lexer::name_end(body, 0)?;
    } else if first.is_ascii_digit() {
        j = 1;
        while j < body.len() && body[j].is_ascii_digit() {
            j += 1;
        }
    } else if is_special_param(first) {
        j = 1;
    } else {
        return None;
    }
    let name: String = body[..j].iter().collect();
    let mut sub = None;
    if j < body.len() && body[j] == '[' {
        let mut depth = 0;
        let mut k = j;
        while k < body.len() {
            match body[k] {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                '\\' => k += 1,
                _ => {}
            }
            k += 1;
        }
        if k < body.len() {
            sub = Some(body[j + 1..k].iter().collect());
            j = k + 1;
        }
    }
    Some((name, sub, j))
}

// ---- ${...}: zsh-style parameter expansion --------------------------------
//
// `braced` computes the value of a `${...}` body in zsh's order: nested
// substitution or name lookup, subscripts, operators or `:` modifiers,
// then flags, length and the `+` set test. Values from the word of
// `${x:-word}` stay tagged so their quoting survives until emission.

#[derive(Default)]
struct PFlags {
    // (#): each word is an arithmetic expression, giving that character.
    charcode: bool,
    // (X): report errors in flags such as (#) instead of ignoring them.
    report: bool,
    extglob: bool,
    at: bool,
    keys: bool,
    values: bool,
    join: Option<String>,
    // `~` came before the j flag: its separator is pattern text.
    join_pattern: bool,
    tilde: bool,
    split: Option<String>,
    zsplit: bool,
    quote: u8,
    quote_min: bool,
    // (q+): like (q-), with $'...' for control characters.
    quote_plus: bool,
    unquote: bool,
    sort: Option<char>,
    numeric: bool,
    icase: bool,
    unique: bool,
    case: Option<char>,
    prompt: u8,
    visible: bool,
    count_chars: bool,
    words: bool,
    words_empty: bool,
    gflags: Option<String>,
    // (A): `=` assigns an array; (AA) an associative array.
    assign_array: u8,
    backslash: bool,
    eval: bool,
    indirect: bool,
    // (!): a nameref itself, not what it refers to.
    no_deref: bool,
    type_of: bool,
    // (l:n::fill::once:) / (r...): width, repeated fill, one-off string.
    pad_left: Option<(usize, Option<String>, Option<String>)>,
    pad_right: Option<(usize, Option<String>, Option<String>)>,
    escapes: bool,
    tilde_dirs: bool,
    matching: bool,
    substr: bool,
    rest_part: bool,
    bind: bool,
    eind: bool,
    mlen: bool,
    nth: Option<String>,
    // (-): numeric sort of signed numbers.
    signed: bool,
    // (Z:opts:) letters for (z) splitting.
    zopts: String,
    // `${=name}`: split on IFS.
    ifs_split: bool,
    any: bool,
}

fn closing_delim(c: char) -> char {
    match c {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        c => c,
    }
}

// Parses `(flags)` with `body[0] == '('`. Returns the flags and the index
// after the closing `)`.
thread_local! {
    // Where parse_flags stopped on a bad flag: an index into the body.
    static FLAG_ERR_AT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

fn parse_flags(body: &[char]) -> Option<(PFlags, usize)> {
    let mut f = PFlags {
        any: true,
        ..PFlags::default()
    };
    let mut i = 1;
    let arg = |i: &mut usize| -> Option<String> {
        let open = *body.get(*i)?;
        let close = closing_delim(open);
        let start = *i + 1;
        let end = (start..body.len()).find(|&k| body[k] == close)?;
        *i = end + 1;
        Some(body[start..end].iter().collect())
    };
    while i < body.len() {
        let c = body[i];
        i += 1;
        match c {
            ')' => return Some((f, i)),
            '@' => f.at = true,
            'k' => f.keys = true,
            'v' => f.values = true,
            'j' => {
                let a = arg(&mut i)?;
                f.join = Some(if f.escapes { p_flag_arg(&a) } else { a });
                f.join_pattern = f.tilde;
            }
            // Strings later flags insert are patterns.
            '~' => f.tilde = !f.tilde,
            's' => {
                let a = arg(&mut i)?;
                f.split = Some(if f.escapes { p_flag_arg(&a) } else { a });
            }
            'f' => f.split = Some("\n".into()),
            'F' => f.join = Some("\n".into()),
            '0' => f.split = Some("\0".into()),
            'z' => f.zsplit = true,
            'Z' => {
                f.zsplit = true;
                let start = i + 1;
                f.zopts = arg(&mut i)?;
                // Only c, C and n are (Z) options.
                if let Some(k) = f.zopts.chars().position(|c| !"cCn".contains(c)) {
                    FLAG_ERR_AT.with(|e| e.set(Some(start + k)));
                    return None;
                }
            }
            'q' => {
                if matches!(body.get(i), Some('-' | '+')) {
                    f.quote_min = true;
                    f.quote_plus = body[i] == '+';
                    i += 1;
                } else {
                    f.quote += 1;
                }
            }
            'Q' => f.unquote = true,
            'o' | 'O' => f.sort = Some(c),
            'a' => {}
            'n' => f.numeric = true,
            '-' => f.signed = true,
            'i' => f.icase = true,
            'u' => f.unique = true,
            'U' | 'L' | 'C' => f.case = Some(c),
            '%' => f.prompt += 1,
            'V' => f.visible = true,
            'c' => f.count_chars = true,
            'w' => f.words = true,
            'W' => f.words_empty = true,
            'g' => f.gflags = Some(arg(&mut i)?),
            'b' => f.backslash = true,
            'e' => f.eval = true,
            'P' => f.indirect = true,
            '!' => f.no_deref = true,
            't' => f.type_of = true,
            'D' => f.tilde_dirs = true,
            'M' => f.matching = true,
            'S' => f.substr = true,
            'R' => f.rest_part = true,
            'B' => f.bind = true,
            'E' => f.eind = true,
            'N' => f.mlen = true,
            'I' => f.nth = Some(arg(&mut i)?),
            'p' => f.escapes = true,
            'l' | 'r' => {
                let d = *body.get(i)?;
                // The width is expanded, then evaluated (zsh's get_intarg).
                let n_text = expand_string(&arg(&mut i)?).ok()?;
                let n = eval_arith_str(n_text.trim()).ok()?.max(0) as usize;
                // Further strings must use the same delimiter.
                let mut more = Vec::new();
                while more.len() < 2 && body.get(i) == Some(&d) {
                    let s = arg(&mut i)?;
                    more.push(if f.escapes { decode_escapes(&s) } else { s });
                }
                let mut it = more.into_iter();
                let entry = (n, it.next(), it.next());
                if c == 'l' {
                    f.pad_left = Some(entry);
                } else {
                    f.pad_right = Some(entry);
                }
            }
            // Accepted without effect: A (assignment), R/B/E/N (match
            // parts), V, w, W, c, X, ~, #, m, S, I, p, g.
            'A' => f.assign_array += 1,
            '#' => f.charcode = true,
            'X' => f.report = true,
            // (*): EXTENDED_GLOB for this expansion's patterns.
            '*' => f.extglob = true,
            'm' => {}
            _ => {
                FLAG_ERR_AT.with(|e| e.set(Some(i - 1)));
                return None;
            }
        }
    }
    None
}

// A flag argument after (p): `$name` is that parameter's value, other
// text has print's escapes.
fn p_flag_arg(a: &str) -> String {
    match a.strip_prefix('$') {
        Some(name) if crate::lexer::is_valid_name(name) => var_get(name),
        _ => decode_escapes(a),
    }
}

fn val_items(v: &Val) -> Vec<String> {
    match v {
        Val::Unset => Vec::new(),
        Val::Str(s) => vec![s.clone()],
        Val::List(items, _) | Val::Rc(items) => items.clone(),
        Val::Tagged(fields) => fields.iter().map(|f| tagged_text(f)).collect(),
    }
}

fn tagged_text(f: &[(char, u8)]) -> String {
    f.iter()
        .filter(|(_, t)| t & MARK == 0)
        .map(|(c, _)| *c)
        .collect()
}

// Drops quoting tags: one field becomes a scalar, several a list.
fn plain(v: Val) -> Val {
    match v {
        Val::Tagged(fields) => {
            if fields.len() == 1 {
                Val::Str(tagged_text(&fields[0]))
            } else {
                Val::List(fields.iter().map(|f| tagged_text(f)).collect(), false)
            }
        }
        other => other,
    }
}

fn is_list(v: &Val) -> bool {
    matches!(v, Val::List(..) | Val::Rc(_))
}

fn map_items(v: Val, f: impl Fn(&str) -> String) -> Val {
    match plain(v) {
        Val::Unset => Val::Unset,
        Val::Str(s) => Val::Str(f(&s)),
        Val::List(items, star) => Val::List(items.iter().map(|s| f(s)).collect(), star),
        Val::Rc(items) => Val::Rc(items.iter().map(|s| f(s)).collect()),
        Val::Tagged(_) => unreachable!(),
    }
}

// Expands the word operand of `${x-word}` and friends, keeping tags.
fn word_val(word: &[char], mode: Mode) -> ExpResult<Val> {
    let mut tmp = Ctx::new(true, false);
    if mode == Mode::Unquoted {
        tmp.unquoted_lit = tmp.param_tag;
        tmp.in_braced_word = true;
    }
    let m = if mode == Mode::Heredoc {
        Mode::Dq
    } else {
        mode
    };
    walk(word, m, &mut tmp)?;
    if mode == Mode::Unquoted && !crate::options::is_set("ignorebraces") {
        tmp.fields = tmp.fields.iter().flat_map(|f| brace_expand(f)).collect();
    }
    Ok(Val::Tagged(tmp.fields))
}

fn sh_mode() -> bool {
    with_shell(|s| matches!(s.emulation, crate::options::Emulation::Sh))
}

fn ksh_arrays() -> bool {
    crate::options::quick("ksharrays")
}

// KSH_ARRAYS subscripts count from 0: shifts a non-negative index to
// zsh's 1-based numbering.
fn ksh_index(i: i64) -> i64 {
    if i >= 0 && ksh_arrays() {
        i + 1
    } else {
        i
    }
}

// KSH_ARRAYS: `$arr` without a subscript is the first element (or key `"0"`
// of an associative array in ksh emulation).
fn ksh_first(name: &str, val: Val) -> Val {
    if !ksh_arrays() {
        return val;
    }
    let _swap = crate::shell::RefSwap::enter(name);
    let resolved = crate::shell::resolve_ref(name);
    if resolved.contains('[') {
        return val;
    }
    if with_shell(|s| s.assoc(&resolved).is_some()) {
        if with_shell(|s| s.emulation == crate::options::Emulation::Ksh) {
            return match assoc_get(&resolved, "0") {
                Some(v) => Val::Str(v),
                None => Val::Unset,
            };
        }
        return match val {
            Val::List(items, _) => items.into_iter().next().map(Val::Str).unwrap_or(Val::Unset),
            v => v,
        };
    }
    if !with_shell(|s| s.array(&resolved).is_some()) {
        return val;
    }
    match val {
        Val::List(items, _) => items.into_iter().next().map(Val::Str).unwrap_or(Val::Unset),
        v => v,
    }
}

// 1-based zsh index into `len` items (negative counts from the end).
fn index_of(i: i64, len: usize) -> Option<usize> {
    let len = len as i64;
    let k = if i > 0 {
        i - 1
    } else if i < 0 {
        len + i
    } else if sh_mode() {
        0
    } else {
        // zsh: element 0 does not exist (KSH_ZERO_SUBSCRIPT aside).
        -1
    };
    (0..len).contains(&k).then_some(k as usize)
}

// `[a,b]` range bounds, inclusive, as a Rust range.
fn range_of(a: i64, b: i64, len: usize) -> std::ops::Range<usize> {
    let l = len as i64;
    let norm = |x: i64| if x < 0 { l + x + 1 } else { x };
    let (s, e) = (norm(a).max(1), norm(b).min(l));
    if s > e {
        0..0
    } else {
        (s - 1) as usize..e as usize
    }
}

// Applies a subscript to a value that is not a named associative array.
fn apply_subscript(val: Val, sub: &str) -> ExpResult<Val> {
    let val = plain(val);
    if sub == "@" || sub == "*" {
        let items = val_items(&val);
        return Ok(Val::List(items, sub == "*"));
    }
    let list = is_list(&val);
    let items = val_items(&val);
    let subc: Vec<char> = sub.chars().collect();
    let (first, second) = match top_comma(&subc) {
        Some(k) => (
            subc[..k].iter().collect::<String>(),
            Some(subc[k + 1..].iter().collect::<String>()),
        ),
        None => (sub.to_string(), None),
    };
    // A search without a range gives the value or the index found.
    if second.is_none() {
        if let Some((f, word)) = parse_sub_flags(&first) {
            if f.search() {
                if matches!(val, Val::Unset) {
                    return Ok(Val::Unset);
                }
                return search_single(&items, list, &f, word);
            }
        }
    }
    // (w) or (s:sep:) on a scalar without a search: words by number.
    if let Some((f, rest)) = parse_sub_flags(&first).filter(|(f, _)| !list && f.word) {
        let units = search_units(&items, false, &f).unwrap_or_default();
        let a = ksh_index(eval_arith_str(&expand_string(rest)?)?);
        let b = match &second {
            Some(s) => ksh_index(eval_arith_str(&expand_string(s)?)?),
            None => a,
        };
        let range = range_of(if a == 0 { 1 } else { a }, b, units.len());
        let sep = f.sep.clone().unwrap_or_else(|| " ".to_string());
        return Ok(Val::Str(units[range].join(&sep)));
    }
    let a = sub_index(&first, &items, list, false)?;
    let b = match &second {
        Some(s) => Some(sub_index(s, &items, list, true)?),
        None => None,
    };
    Ok(match val {
        Val::Unset => Val::Unset,
        Val::List(items, _) | Val::Rc(items) if !items.is_empty() || b.is_some() => match b {
            Some(b) => Val::List(items[range_of(a, b, items.len())].to_vec(), true),
            None => match index_of(
                if a == 0 && crate::options::quick("kshzerosubscript") {
                    1
                } else {
                    a
                },
                items.len(),
            ) {
                Some(k) => Val::Str(items[k].clone()),
                None => Val::Unset,
            },
        },
        Val::List(..) | Val::Rc(_) => Val::Unset,
        Val::Str(s) => {
            let chars: Vec<char> = s.chars().collect();
            match b {
                Some(b) => Val::Str(chars[range_of(a, b, chars.len())].iter().collect()),
                // Index 0: bash's `${scalar[0]}` is the value; zsh's is
                // empty, or the first character with KSH_ZERO_SUBSCRIPT.
                None if a == 0 && crate::options::quick("kshzerosubscript") => {
                    Val::Str(chars.first().map(|c| c.to_string()).unwrap_or_default())
                }
                None if a == 0 && sh_mode() => Val::Str(s),
                None => match index_of(a, chars.len()) {
                    Some(k) => Val::Str(chars[k].to_string()),
                    None => Val::Unset,
                },
            }
        }
        Val::Tagged(_) => unreachable!(),
    })
}

// The `,` separating the two ends of a range, outside brackets,
// parentheses, quotes and nested expansions.
fn top_comma(c: &[char]) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\\' => i += 1,
            '\'' => i = scan_squote(c, i)?.saturating_sub(1),
            '"' => i = scan_dquote(c, i)?.saturating_sub(1),
            '$' if c.get(i + 1) == Some(&'{') => i = scan_brace(c, i + 1, false)?.saturating_sub(1),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

// Subscript flags (zsh's getarg): (r)/(R) value, (i)/(I) index searches,
// (e) exact, (w)/(s:sep:) words, (k)/(K) keys, (n:N:) Nth match,
// (b:N:) where to begin.
#[derive(Default)]
struct SubSearch {
    letters: String,
    rev: bool,
    down: bool,
    ind: bool,
    exact: bool,
    word: bool,
    sep: Option<String>,
    num: i64,
    beg: Option<i64>,
}

impl SubSearch {
    fn search(&self) -> bool {
        self.rev
    }
}

pub fn has_subscript_flag(sub: &str) -> bool {
    parse_sub_flags(sub).is_some()
}

fn parse_sub_flags(sub: &str) -> Option<(SubSearch, &str)> {
    let rest = sub.strip_prefix('(')?;
    let chars: Vec<char> = rest.chars().collect();
    let mut f = SubSearch {
        num: 1,
        ..SubSearch::default()
    };
    let mut i = 0;
    let mut escapes = false;
    let arg = |i: &mut usize| -> Option<String> {
        let d = *chars.get(*i)?;
        let close = closing_delim(d);
        let end = (*i + 1..chars.len()).find(|&k| chars[k] == close)?;
        let v: String = chars[*i + 1..end].iter().collect();
        *i = end + 1;
        Some(v)
    };
    while i < chars.len() && chars[i] != ')' {
        let c = chars[i];
        i += 1;
        match c {
            'r' | 'R' | 'i' | 'I' | 'k' | 'K' => {
                f.rev = true;
                f.down = c.is_ascii_uppercase();
                f.ind = matches!(c, 'i' | 'I');
            }
            'e' => f.exact = true,
            'w' => f.word = true,
            'f' => {
                f.word = true;
                f.sep = Some("\n".into());
            }
            // (p): escapes in later separator arguments, as `(ps:\0:)`.
            'p' => escapes = true,
            's' => {
                let a = arg(&mut i)?;
                f.sep = Some(if escapes { p_flag_arg(&a) } else { a });
            }
            'n' => {
                let v = eval_arith_str(&arg(&mut i)?).ok()?;
                f.num = if v == 0 { 1 } else { v };
            }
            'b' => {
                let v = eval_arith_str(&arg(&mut i)?).ok()?;
                f.beg = Some(if v > 0 { v - 1 } else { v });
            }
            _ => return None,
        }
        f.letters.push(c);
    }
    if i >= chars.len() || f.letters.is_empty() {
        return None;
    }
    if f.num < 0 {
        f.down = !f.down;
        f.num = f.num.saturating_neg();
    }
    let close = rest.char_indices().nth(i).map(|(b, _)| b)?;
    Some((f, &rest[close + 1..]))
}

// A subscript pattern: unlike other patterns, characters that come from
// parameter values are active (zsh tokenizes the expanded subscript).
fn subscript_pattern(word: &str, in_dq: bool) -> ExpResult<String> {
    // Unquoted, zsh keeps the backslash of `\"` in a subscript pattern, so
    // `a\"b` finds the key `a\"b`; in double quotes `\"` was unquoted to `"`.
    let mut text = String::new();
    if !in_dq {
        let mut bs = 0;
        for c in word.chars() {
            if c == '"' && bs % 2 == 1 {
                text.pop();
                text.push_str("\\\\\\\\\\\"");
                bs = 0;
                continue;
            }
            bs = if c == '\\' { bs + 1 } else { 0 };
            text.push(c);
        }
    } else {
        let mut bs = 0;
        for c in word.chars() {
            if c == '"' && bs % 2 == 0 {
                text.push_str("\\\"");
                continue;
            }
            bs = if c == '\\' { bs + 1 } else { 0 };
            text.push(c);
        }
    }
    let chars: Vec<char> = text.chars().collect();
    let mut ctx = Ctx::new(false, false);
    ctx.param_tag &= !NG;
    ctx.subst_tag &= !NG;
    walk(&chars, Mode::Unquoted, &mut ctx)?;
    // A quoted backslash stays an active escape (`\\\\?` matches `\x`).
    let mut out = String::new();
    for &(c, f) in ctx.fields.iter().flatten() {
        if f & MARK != 0 {
            continue;
        }
        if f & (Q | NG) != 0 && c != '\\' && "*?[]()|<>^#~!-".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    Ok(out)
}

// Elements searched: the list, a scalar's words, or None for characters.
fn search_units(items: &[String], list: bool, f: &SubSearch) -> Option<Vec<String>> {
    if list {
        return Some(items.to_vec());
    }
    if f.word {
        let s = items.first().cloned().unwrap_or_default();
        return Some(match &f.sep {
            Some(sep) => s.split(sep.as_str()).map(|w| w.to_string()).collect(),
            None => s.split_whitespace().map(|w| w.to_string()).collect(),
        });
    }
    None
}

// The 1-based position the search finds among `units`; 0 or len+1 when
// nothing matches (down or up).
fn search_units_index(units: &[String], f: &SubSearch, hit: &dyn Fn(&str) -> bool) -> i64 {
    let len = units.len() as i64;
    let mut beg = f.beg.unwrap_or(0);
    if beg < 0 {
        beg += len;
    }
    if f.down {
        if beg < 0 {
            return 0;
        }
    } else if beg >= len {
        return len + 1;
    }
    let mut num = f.num;
    if beg >= 0 && beg < len {
        if f.down {
            let start = if f.beg.is_some() { beg } else { len - 1 };
            for r in (0..=start).rev() {
                if hit(&units[r as usize]) {
                    num -= 1;
                    if num == 0 {
                        return r + 1;
                    }
                }
            }
        } else {
            for r in beg..len {
                if hit(&units[r as usize]) {
                    num -= 1;
                    if num == 0 {
                        return r + 1;
                    }
                }
            }
        }
    }
    if f.down {
        0
    } else {
        len + 1
    }
}

// Searching a scalar's characters: the first end of a range matches
// the pattern followed by anything at each position, the second the
// text up to each position ending in the pattern. Positions are 1-based;
// 0 or len+1 when nothing matches.
fn search_chars(s: &str, f: &SubSearch, word: &str, a2: bool) -> ExpResult<i64> {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i64;
    let mut pat = if f.exact {
        crate::pattern::escape(&expand_string(word)?)
    } else {
        subscript_pattern(word, IN_DQ_SUB.with(|d| d.get()))?
    };
    if a2 {
        if !pat.starts_with('*') {
            pat.insert(0, '*');
        }
    } else if !pat.ends_with('*') || pat.ends_with("\\*") {
        pat.push('*');
    }
    let p = crate::pattern::Pattern::new(&pat);
    let mut beg = f.beg.unwrap_or(0);
    if beg < 0 {
        beg += len;
    }
    let miss = if f.down { 0 } else { len + 1 };
    if !(0..len).contains(&beg) {
        return Ok(miss);
    }
    let test = |t: usize| {
        if a2 {
            p.matches_chars(&chars[..t])
        } else {
            p.matches_chars(&chars[t..])
        }
    };
    let mut num = f.num;
    if f.down {
        let last = if f.beg.is_some() { beg } else { len };
        let found: Vec<usize> = (0..=last as usize)
            .filter(|&t| t <= chars.len() && test(t))
            .collect();
        if num > 0 && found.len() as i64 >= num {
            let t = found[found.len() - num as usize] as i64;
            return Ok(if a2 { t } else { t + 1 });
        }
        return Ok(0);
    }
    for t in beg as usize..=chars.len() {
        if test(t) {
            num -= 1;
            if num == 0 {
                return Ok(if a2 { t as i64 } else { t as i64 + 1 });
            }
        }
    }
    Ok(miss)
}

// One end of a subscript: a search or an arithmetic index (1-based).
fn sub_index(part: &str, items: &[String], list: bool, a2: bool) -> ExpResult<i64> {
    if let Some((f, word)) = parse_sub_flags(part) {
        if f.search() {
            return match search_units(items, list, &f) {
                Some(units) => {
                    let hit = unit_matcher(word, f.exact)?;
                    let r = search_units_index(&units, &f, &hit);
                    Ok(if list {
                        r
                    } else {
                        word_char_index(items, &units, r, a2)
                    })
                }
                None => search_chars(&items.first().cloned().unwrap_or_default(), &f, word, a2),
            };
        }
    }
    let text = expand_string(part)?;
    Ok(ksh_index(eval_arith_str(&text)?))
}

// (w) searches on a scalar give character positions: the first or last
// character of the word found.
fn word_char_index(items: &[String], units: &[String], r: i64, a2: bool) -> i64 {
    let s = items.first().cloned().unwrap_or_default();
    if r < 1 || r > units.len() as i64 {
        return if r < 1 {
            0
        } else {
            s.chars().count() as i64 + 1
        };
    }
    let mut pos = 0;
    let mut rest = s.as_str();
    for (k, w) in units.iter().enumerate() {
        let off = rest.find(w.as_str()).unwrap_or(0);
        let start = pos + rest[..off].chars().count();
        if k as i64 + 1 == r {
            let n = w.chars().count();
            return if a2 {
                (start + n) as i64
            } else {
                start as i64 + 1
            };
        }
        pos = start + w.chars().count();
        rest = &rest[off + w.len()..];
    }
    0
}

fn unit_matcher(word: &str, exact: bool) -> ExpResult<impl Fn(&str) -> bool> {
    let pat = if exact {
        crate::pattern::escape(&expand_string(word)?)
    } else {
        subscript_pattern(word, IN_DQ_SUB.with(|d| d.get()))?
    };
    let p = crate::pattern::Pattern::new(&pat);
    Ok(move |v: &str| p.matches(v))
}

// A search subscript alone: the value found ((r)/(R)) or its index
// ((i)/(I)).
fn search_single(items: &[String], list: bool, f: &SubSearch, word: &str) -> ExpResult<Val> {
    let r = match search_units(items, list, f) {
        Some(units) => {
            let hit = unit_matcher(word, f.exact)?;
            let r = search_units_index(&units, f, &hit);
            // A miss going down is index 0, which KSH_ZERO_SUBSCRIPT reads
            // as the first element.
            if !f.ind && r == 0 && list && crate::options::is_set("kshzerosubscript") {
                return Ok(units.first().map_or(Val::Unset, |v| Val::Str(v.clone())));
            }
            if !f.ind {
                return Ok(
                    match units.get((r - 1).max(0) as usize).filter(|_| r >= 1) {
                        Some(v) if r as usize <= units.len() => Val::Str(v.clone()),
                        _ => Val::Unset,
                    },
                );
            }
            if list {
                r
            } else {
                word_char_index(items, &units, r, false)
            }
        }
        None => {
            let s = items.first().cloned().unwrap_or_default();
            let r = search_chars(&s, f, word, false)?;
            if !f.ind {
                let chars: Vec<char> = s.chars().collect();
                return Ok(if r >= 1 && r as usize <= chars.len() {
                    Val::Str(chars[r as usize - 1].to_string())
                } else {
                    Val::Unset
                });
            }
            r
        }
    };
    // KSH_ARRAYS counts from 0; "not found" past the end stays.
    let r = if ksh_arrays() && r > 0 { r - 1 } else { r };
    Ok(Val::Str(r.to_string()))
}

/// For an assignment to `name[sub]` whose subscript has search flags
/// (`a[(R)x,(r)y]=...`): the range as plain indices. None otherwise.
pub fn search_subscript_range(name: &str, sub: &str) -> Option<ExpResult<String>> {
    let mut depth = 0;
    let comma = sub.char_indices().find(|&(_, c)| {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            _ => {}
        }
        c == ',' && depth == 0
    });
    let (a, b) = match comma {
        Some((k, _)) => (&sub[..k], Some(&sub[k + 1..])),
        None => (sub, None),
    };
    if parse_sub_flags(a).is_none() && b.is_none_or(|b| parse_sub_flags(b).is_none()) {
        return None;
    }
    let items = with_shell(|s| s.array(name).cloned()).unwrap_or_default();
    let end = |part: &str, a2: bool| sub_index(part, &items, true, a2);
    Some((|| {
        let x = end(a, false)?;
        Ok(match b {
            Some(b) => format!("{},{}", x, end(b, true)?),
            None => x.to_string(),
        })
    })())
}

fn search_flags(sub: &str) -> Option<(String, &str)> {
    let (f, word) = parse_sub_flags(sub)?;
    Some((f.letters, word))
}

// A leading `^` negates the pattern (zsh EXTENDED_GLOB).
fn matcher(word: &str, exact: bool) -> ExpResult<impl Fn(&str) -> bool> {
    let (negate, word) = match word.strip_prefix('^') {
        Some(rest) if !exact => (true, rest),
        _ => (false, word),
    };
    let pat = if exact {
        expand_string(word)?
    } else {
        subscript_pattern(word, IN_DQ_SUB.with(|d| d.get()))?
    };
    Ok(move |v: &str| {
        let m = if exact {
            v == pat
        } else {
            crate::pattern::matches(&pat, v)
        };
        m != negate
    })
}

// `${(k)name[sub]}` (and ksh `${!name[sub]}`): on a hash, the key; on an
// array, the index (and a range is an invalid subscript).
fn keys_subscript_value(name: &str, sub: &str) -> ExpResult<Val> {
    let _swap = crate::shell::RefSwap::enter(name);
    let resolved = crate::shell::resolve_ref(name);
    if resolved.contains('[') {
        return param_value(name, Some(sub));
    }
    if resolved == "functions" && !with_shell(|s| s.assoc("functions").is_some()) {
        let k = assoc_key(sub)?;
        return Ok(if with_shell(|s| s.functions.contains_key(&k)) {
            Val::Str(k)
        } else {
            Val::Unset
        });
    }
    if resolved == "commands"
        && crate::modules::feature_available("p:commands")
        && with_shell(|s| {
            s.param("commands")
                .is_none_or(|p| p.attrs & crate::params::SPECIAL != 0 && p.level == 0)
        })
    {
        let k = assoc_key(sub)?;
        return Ok(if crate::shell::lookup_command(&k).is_some() {
            Val::Str(k)
        } else {
            Val::Unset
        });
    }
    if assoc_keys(&resolved).is_some() {
        let k = assoc_key(sub)?;
        return Ok(if assoc_get(&resolved, &k).is_some() {
            Val::Str(k)
        } else {
            Val::Unset
        });
    }
    if let Some(items) = with_shell(|s| s.array(&resolved).cloned()) {
        let subc: Vec<char> = sub.chars().collect();
        if top_comma(&subc).is_some() {
            return fail("invalid subscript");
        }
        let mut a = sub_index(sub, &items, true, false)?;
        if a < 0 {
            a += items.len() as i64 + 1;
        }
        if a > 0 && ksh_arrays() {
            a -= 1;
        }
        return Ok(Val::Str(a.to_string()));
    }
    param_value(name, Some(sub))
}

// Value of `name[sub]` for a named parameter.
fn param_value(name: &str, sub: Option<&str>) -> ExpResult<Val> {
    let _swap = crate::shell::RefSwap::enter(name);
    let resolved = crate::shell::resolve_ref(name);
    let target_base = crate::shell::split_target_pub(&resolved);
    crate::zparam::ensure(&target_base);
    let Some(sub) = sub else {
        return Ok(lookup(name));
    };
    let name = if resolved.contains('[') {
        name
    } else {
        resolved.as_str()
    };
    // zsh/parameter's $jobstates: a job's state, mark and processes.
    if name == "jobstates" && !with_shell(|s| s.assoc("jobstates").is_some()) {
        let key = expand_string(sub)?;
        return Ok(
            match key
                .parse::<i32>()
                .ok()
                .and_then(crate::jobs::job_state_text)
            {
                Some(t) => Val::Str(t),
                None => Val::Unset,
            },
        );
    }
    // zsh/system's $sysparams.
    if name == "sysparams" && !with_shell(|s| s.assoc("sysparams").is_some()) {
        let key = expand_string(sub)?;
        return Ok(crate::zsystem::sysparam(&key).map_or(Val::Unset, Val::Str));
    }
    // zsh/terminfo's $terminfo and zsh/termcap's $termcap.
    if name == "terminfo"
        && crate::modules::feature_available("p:terminfo")
        && !with_shell(|s| s.assoc("terminfo").is_some())
    {
        let key = assoc_key(sub)?;
        return Ok(
            crate::zle::refresh::refresh_term::terminfo_lookup(&key).map_or(Val::Unset, Val::Str)
        );
    }
    if name == "termcap"
        && crate::modules::feature_available("p:termcap")
        && !with_shell(|s| s.assoc("termcap").is_some())
    {
        let key = assoc_key(sub)?;
        return Ok(
            crate::zle::refresh::refresh_term::termcap_lookup(&key).map_or(Val::Unset, Val::Str)
        );
    }
    // zsh/parameter's $functions: a function's body as text.
    if name == "functions" && !with_shell(|s| s.assoc("functions").is_some()) {
        let key = expand_string(sub)?;
        return Ok(
            match with_shell(|s| s.functions.get(&key).map(|f| f.body.clone())) {
                Some(body) => {
                    let text = crate::deparse::function_text(&key, &body, None);
                    let lines: Vec<&str> = text.trim_end_matches('\n').lines().collect();
                    // A body with redirections keeps its braces.
                    if matches!(body.as_ref(), crate::types::Node::Redirected { .. })
                        && lines.len() >= 2
                    {
                        let mut t = String::from("{\n");
                        t.push_str(&lines[1..].join("\n"));
                        Val::Str(t)
                    } else {
                        let inner = if lines.len() >= 2 {
                            &lines[1..lines.len() - 1]
                        } else {
                            &lines[..]
                        };
                        Val::Str(inner.join("\n"))
                    }
                }
                None => Val::Unset,
            },
        );
    }
    if name == "commands"
        && crate::modules::feature_available("p:commands")
        && with_shell(|s| {
            s.param("commands")
                .is_none_or(|p| p.attrs & crate::params::SPECIAL != 0 && p.level == 0)
        })
    {
        let key = assoc_key(sub)?;
        return Ok(match crate::shell::lookup_command(&key) {
            Some(p) => Val::Str(p),
            None => Val::Unset,
        });
    }
    let is_assoc = with_shell(|s| s.assoc(name).is_some());
    if !is_assoc {
        return apply_subscript(lookup(name), sub);
    }
    if sub == "@" || sub == "*" {
        return Ok(Val::List(
            assoc_values(name).unwrap_or_default(),
            sub == "*",
        ));
    }
    // (k)/(K): the keys are patterns matched against the text; the
    // value of the first (or of every) matching key.
    if let Some((flags, word)) = search_flags(sub).filter(|(f, _)| f.contains(['k', 'K'])) {
        let text = assoc_key(word)?;
        let mut keys = assoc_keys(name).unwrap_or_default();
        if name == "history" {
            // zsh's $history iterates from most recent event down to 1 so
            // ${history[(r)pat]} returns the most recent matching command.
            keys.sort_by(|a, b| {
                b.parse::<usize>()
                    .unwrap_or(0)
                    .cmp(&a.parse::<usize>().unwrap_or(0))
            });
        }
        let mut found = keys
            .iter()
            .filter(|k| crate::pattern::matches(k, &text))
            .map(|k| assoc_get(name, k).unwrap_or_default());
        return Ok(if flags.contains('K') {
            Val::List(found.collect(), false)
        } else {
            match found.next() {
                Some(v) => Val::Str(v),
                None => Val::Unset,
            }
        });
    }
    if let Some((flags, word)) = search_flags(sub).filter(|(f, _)| f.contains(['i', 'I', 'r', 'R']))
    {
        let hit = matcher(word, flags.contains('e'))?;
        let last = flags.contains('I') || flags.contains('R');
        let want_value = flags.contains('r') || flags.contains('R');
        let mut keys = assoc_keys(name).unwrap_or_default();
        if name == "history" {
            // zsh's $history iterates from most recent event down to 1 so
            // ${history[(r)pat]} returns the most recent matching command.
            keys.sort_by(|a, b| {
                b.parse::<usize>()
                    .unwrap_or(0)
                    .cmp(&a.parse::<usize>().unwrap_or(0))
            });
        }
        let mut found = keys.iter().filter_map(|k| {
            let v = assoc_get(name, k).unwrap_or_default();
            if want_value {
                hit(&v).then(|| (k.clone(), v))
            } else {
                hit(k).then(|| (k.clone(), v))
            }
        });
        // (I)/(R) on a hash give every match; (i)/(r) the first.
        if last {
            let all: Vec<String> = found.map(|(k, v)| if want_value { v } else { k }).collect();
            return Ok(Val::List(all, false));
        }
        // (r)/(R) give the value, (i)/(I) the key; `(k)` on the whole
        // expansion turns a value search into its key (handled by caller
        // through `keys` flag).
        return Ok(match found.next() {
            Some((k, v)) => Val::Str(if want_value { v } else { k }),
            None => Val::Unset,
        });
    }
    // (e) alone: the text is the key, exactly.
    let key = match search_flags(sub).filter(|(f, _)| f.as_str() == "e") {
        Some((_, w)) => expand_string(w)?,
        None => assoc_key(sub)?,
    };
    Ok(match assoc_get(name, &key) {
        Some(v) => Val::Str(v),
        None => Val::Unset,
    })
}

/// Applies `:` modifiers (`mods` starts with `:`) to one word, as glob
/// qualifiers such as `*(:h)` do.
// `\\` pairs as single backslashes; other escapes stay.
fn collapse_backslashes(s: &str) -> String {
    let mut out = String::new();
    let mut cs = s.chars().peekable();
    while let Some(c) = cs.next() {
        out.push(c);
        if c == '\\' && cs.peek() == Some(&'\\') {
            cs.next();
        }
    }
    out
}

pub fn modify_word(word: &str, mods: &str) -> String {
    let rest: Vec<char> = mods.chars().collect();
    match apply_modifiers(Val::Str(word.to_string()), &rest) {
        Ok(Some(v)) => val_items(&v).join(" "),
        _ => word.to_string(),
    }
}

/// History-style modifiers at the start of `text` (`:h2:s/a/b/...`)
/// applied to `word`: the result and how many characters were used.
pub fn history_modifiers(word: &str, text: &[char]) -> (String, usize) {
    match parse_modifiers(text, true) {
        Ok((mods, used)) if !mods.is_empty() => {
            match run_modifiers(Val::Str(word.to_string()), &mods) {
                Ok(v) => (val_items(&v).join(" "), used),
                Err(_) => (word.to_string(), used),
            }
        }
        _ => (word.to_string(), 0),
    }
}

/// Splits text into shell words, as `${(z)...}` does.
pub fn split_shell_words(s: &str) -> Vec<String> {
    shell_words(s, "")
}

// One `:` modifier, as zsh's modify() reads it.
struct Modifier {
    op: char,
    // :h / :t component count (braced only).
    count: usize,
    global: bool,
    // :w / :W — each word separately, split on `sep` (blanks if None).
    words: bool,
    sep: Option<String>,
    // :f repeats until nothing changes (-1); :F:n: n times.
    rec: i64,
    // :s/l/r/ — raw left and right sides.
    subst: Option<(String, String)>,
    // :S — the left side is a pattern.
    pattern: bool,
}

// The last :s substitution, which :& repeats.
static HIST_SUBST: std::sync::Mutex<Option<(String, String, bool)>> = std::sync::Mutex::new(None);

// The text between a delimiter at `rest[i]` and its closing partner, as
// zsh's get_strarg reads `:W` and `:F` arguments.
fn delimited(rest: &[char], i: usize) -> Option<(String, usize)> {
    let open = *rest.get(i)?;
    let close = match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        c => c,
    };
    let k = rest[i + 1..].iter().position(|&c| c == close)? + i + 1;
    Some((rest[i + 1..k].iter().collect(), k + 1))
}

// Parses `:` modifiers at the start of `rest`. Returns them and how
// many characters they use; parsing stops (keeping what came before) at
// the first `:` that does not start a modifier.
fn parse_modifiers(rest: &[char], inbrace: bool) -> ExpResult<(Vec<Modifier>, usize)> {
    let mut mods = Vec::new();
    let mut i = 0;
    'outer: while rest.get(i) == Some(&':') {
        let start = i;
        i += 1;
        let mut m = Modifier {
            op: '\0',
            count: 0,
            global: false,
            words: false,
            sep: None,
            rec: 1,
            subst: None,
            pattern: false,
        };
        while m.op == '\0' && i < rest.len() {
            match rest[i] {
                c @ ('a' | 'A' | 'c' | 'r' | 'e' | 'l' | 'u' | 'q' | 'Q' | 'P' | 'x') => {
                    m.op = c;
                    i += 1;
                }
                c @ ('h' | 't') => {
                    m.op = c;
                    i += 1;
                    if inbrace {
                        while let Some(d) = rest.get(i).and_then(|c| c.to_digit(10)) {
                            m.count = m.count.saturating_mul(10).saturating_add(d as usize);
                            i += 1;
                        }
                    }
                }
                c @ ('s' | 'S') => {
                    m.pattern = c == 'S' || crate::options::is_set("histsubstpattern");
                    i += 1;
                    let Some(&del) = rest.get(i) else {
                        return fail("bad substitution");
                    };
                    i += 1;
                    // Each side ends at an unescaped delimiter; the last
                    // one is optional.
                    let side = |i: &mut usize, need: bool| -> ExpResult<String> {
                        let mut out = String::new();
                        while *i < rest.len() && rest[*i] != del {
                            if rest[*i] == '\\' && *i + 1 < rest.len() {
                                out.push('\\');
                                out.push(rest[*i + 1]);
                                *i += 2;
                                continue;
                            }
                            out.push(rest[*i]);
                            *i += 1;
                        }
                        if *i < rest.len() {
                            *i += 1;
                        } else if need {
                            return fail("bad substitution");
                        }
                        Ok(out)
                    };
                    let l = side(&mut i, true)?;
                    let r = side(&mut i, false)?;
                    m.op = 's';
                    m.subst = Some((l, r));
                }
                '&' => {
                    m.op = '&';
                    i += 1;
                }
                'g' => {
                    m.global = true;
                    i += 1;
                }
                'w' => {
                    m.words = true;
                    i += 1;
                }
                'W' => {
                    let Some((sep, next)) = delimited(rest, i + 1) else {
                        i = start;
                        break 'outer;
                    };
                    m.words = true;
                    m.sep = Some(sep);
                    i = next;
                }
                'f' => {
                    m.rec = -1;
                    i += 1;
                }
                'F' => {
                    let Some((n, next)) = delimited(rest, i + 1) else {
                        i = start;
                        break 'outer;
                    };
                    m.rec = match crate::arith::eval(&n) {
                        Ok(v) => v,
                        Err(e) => return fail(&e),
                    };
                    i = next;
                }
                _ => {
                    i = start;
                    break 'outer;
                }
            }
        }
        if m.op == '\0' {
            i = start;
            break;
        }
        mods.push(m);
    }
    Ok((mods, i))
}

// zsh's remtpath: drops the last `count` components (`count` 0: one),
// or with a count keeps the first `count`.
fn rem_tail_path(s: &str, count: usize) -> String {
    let b = s.as_bytes();
    let mut end = b.len() as isize - 1;
    while end >= 0 && b[end as usize] == b'/' {
        end -= 1;
    }
    if count == 0 {
        while end >= 0 && b[end as usize] != b'/' {
            end -= 1;
        }
    }
    if end < 0 {
        return if s.starts_with('/') { "/" } else { "." }.to_string();
    }
    let end = end as usize;
    if count > 0 {
        let mut left = count;
        let mut k = 0;
        while k < end {
            if b[k] == b'/' {
                left -= 1;
                if left == 0 {
                    return s[..if k == 0 { 1 } else { k }].to_string();
                }
                while k + 1 < b.len() && b[k + 1] == b'/' {
                    k += 1;
                }
            }
            k += 1;
        }
        return s.to_string();
    }
    let mut e = end;
    while e > 0 && b[e - 1] == b'/' {
        e -= 1;
    }
    if e == 0 {
        e = 1;
        if b.get(1) == Some(&b'/') && b.get(2) != Some(&b'/') {
            e = 2;
        }
    }
    s[..e].to_string()
}

// zsh's remlpaths: keeps the last `count` components (at least one).
fn rem_lead_paths(s: &str, count: usize) -> String {
    let mut t = s.to_string();
    if t.ends_with('/') {
        let trimmed = t.trim_end_matches('/').len();
        t.truncate(trimmed);
        if t.is_empty() {
            return String::new();
        }
    }
    let b = t.as_bytes();
    let mut left = count as isize;
    let mut k = b.len() as isize - 1;
    loop {
        while k >= 0 {
            if b[k as usize] == b'/' {
                left -= 1;
                if left > 0 {
                    if k > 0 {
                        k -= 1;
                        break;
                    }
                    return t;
                }
                return t[k as usize + 1..].to_string();
            }
            k -= 1;
        }
        while k >= 0 && b[k as usize] == b'/' {
            k -= 1;
        }
        if k <= 0 {
            break;
        }
    }
    t
}

// :s with zsh's rules: `\x` in the left side is x; in the right side `&`
// is the left side and `\&` a literal &.
fn hist_subst(s: &str, l: &str, r: &str, global: bool) -> String {
    let unescape = |t: &str| {
        let mut out = String::new();
        let mut it = t.chars();
        while let Some(c) = it.next() {
            if c == '\\' {
                if let Some(n) = it.next() {
                    out.push(n);
                }
            } else {
                out.push(c);
            }
        }
        out
    };
    let lhs = if l.is_empty() {
        s.to_string()
    } else {
        unescape(l)
    };
    let global = global && !l.is_empty();
    let mut rep = String::new();
    let mut it = r.chars();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                if let Some(n) = it.next() {
                    rep.push(n);
                }
            }
            '&' => rep.push_str(&lhs),
            c => rep.push(c),
        }
    }
    if lhs.is_empty() || !s.contains(&lhs) {
        return s.to_string();
    }
    if global {
        s.replace(&lhs, &rep)
    } else {
        s.replacen(&lhs, &rep, 1)
    }
}

fn apply_one(m: &Modifier, s: &str, subst: &Option<(String, String)>) -> String {
    match m.op {
        'h' => rem_tail_path(s, m.count),
        't' => rem_lead_paths(s, m.count),
        'r' => path_root(s),
        'e' => path_ext(s),
        'a' => absolute_path(s),
        'A' => std::fs::canonicalize(absolute_path(s))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| absolute_path(s)),
        'P' => realpath_p(s),
        'c' => crate::shell::find_command(s).unwrap_or_else(|| s.to_string()),
        'l' => s.to_lowercase(),
        'u' => s.to_uppercase(),
        'q' => backslash_quote(s),
        'Q' => unquote(s),
        'x' => s
            .split_whitespace()
            .map(crate::builtins::shell_quote)
            .collect::<Vec<_>>()
            .join(" "),
        's' | '&' if m.pattern => match subst {
            Some((l, r)) => {
                // As in ${x/pat/repl}: `#` and `%` anchor the pattern, and
                // the replacement is expanded per match (so $match works);
                // `&` is the matched text.
                let mut l = l.as_str();
                let mut fl = crate::pattern::SubFlags {
                    long: true,
                    global: m.global,
                    ..Default::default()
                };
                if let Some(rest) = l.strip_prefix('#') {
                    fl.start = true;
                    l = rest;
                }
                if let Some(rest) = l.strip_prefix('%') {
                    fl.end = true;
                    l = rest;
                }
                fl.substr = !(fl.start || fl.end);
                let pat = crate::pattern::Pattern::new(l);
                let repl = || {
                    let text = if r.contains(['$', '`']) {
                        expand_heredoc(r).unwrap_or_else(|_| r.clone())
                    } else {
                        r.clone()
                    };
                    let matched = crate::shell::var_get("MATCH");
                    let mut out = String::new();
                    let mut cs = text.chars().peekable();
                    while let Some(c) = cs.next() {
                        match c {
                            '\\' if cs.peek() == Some(&'&') => out.push(cs.next().unwrap()),
                            '&' => out.push_str(&matched),
                            _ => out.push(c),
                        }
                    }
                    out
                };
                pat.getmatch(s, fl, 1, Some(&repl))
            }
            None => s.to_string(),
        },
        's' | '&' => match subst {
            Some((l, r)) => hist_subst(s, l, r, m.global),
            None => s.to_string(),
        },
        _ => s.to_string(),
    }
}

// One modifier on one value, honouring :w/:W and :f/:F.
fn apply_modifier(m: &Modifier, s: &str, subst: &Option<(String, String)>) -> String {
    let once = |s: &str| -> String {
        if !m.words {
            return apply_one(m, s, subst);
        }
        match &m.sep {
            Some(sep) if !sep.is_empty() => s
                .split(sep.as_str())
                .map(|w| apply_one(m, w, subst))
                .collect::<Vec<_>>()
                .join(sep),
            _ => {
                // Words between blanks; the blanks stay as they were.
                let mut out = String::new();
                let mut word = String::new();
                for c in s.chars() {
                    if c == ' ' || c == '\t' || c == '\n' {
                        if !word.is_empty() {
                            out.push_str(&apply_one(m, &word, subst));
                            word.clear();
                        }
                        out.push(c);
                    } else {
                        word.push(c);
                    }
                }
                if !word.is_empty() {
                    out.push_str(&apply_one(m, &word, subst));
                }
                out
            }
        }
    };
    if m.rec < 0 {
        let mut cur = s.to_string();
        for _ in 0..10_000 {
            let next = once(&cur);
            if next == cur || next.len() > 10_000_000 {
                return next;
            }
            cur = next;
        }
        return cur;
    }
    let mut cur = s.to_string();
    for _ in 0..m.rec {
        cur = once(&cur);
    }
    cur
}

fn run_modifiers(mut val: Val, mods: &[Modifier]) -> ExpResult<Val> {
    val = plain(val);
    for m in mods {
        // :s sides may hold expansions; :& reuses the last ones.
        let subst = match (&m.subst, m.op) {
            (Some((l, r)), _) => {
                let ex = |t: &str| -> ExpResult<String> {
                    if t.contains(['$', '`']) {
                        expand_heredoc(t)
                    } else {
                        Ok(t.to_string())
                    }
                };
                // A pattern's replacement is expanded per match, after $match.
                let l = if m.pattern {
                    // `\\` is one backslash, which then quotes the next character.
                    collapse_backslashes(l)
                } else {
                    l.clone()
                };
                let pair = (ex(&l)?, if m.pattern { r.clone() } else { ex(r)? });
                if let Ok(mut h) = HIST_SUBST.lock() {
                    *h = Some((pair.0.clone(), pair.1.clone(), m.pattern));
                }
                Some(pair)
            }
            (None, '&') => HIST_SUBST
                .lock()
                .ok()
                .and_then(|h| h.clone())
                .map(|(l, r, _)| (l, r)),
            _ => None,
        };
        if m.op == '&' && subst.is_none() {
            return fail("no previous substitution");
        }
        // `:&` repeats the last substitution, pattern or not.
        let repeat;
        let m = if m.op == '&' {
            let pattern = HIST_SUBST
                .lock()
                .ok()
                .and_then(|h| h.as_ref().map(|t| t.2))
                .unwrap_or(false);
            repeat = Modifier {
                op: '&',
                count: m.count,
                global: m.global,
                words: m.words,
                sep: m.sep.clone(),
                rec: m.rec,
                subst: None,
                pattern,
            };
            &repeat
        } else {
            m
        };
        val = map_items(val, |s| apply_modifier(m, s, &subst));
    }
    Ok(val)
}

// Applies `:` modifiers filling all of `rest` (`${name:mods}`). None if
// `rest` is not a modifier list.
fn apply_modifiers(val: Val, rest: &[char]) -> ExpResult<Option<Val>> {
    let (mods, used) = parse_modifiers(rest, true)?;
    if mods.is_empty() || used != rest.len() {
        return Ok(None);
    }
    run_modifiers(val, &mods).map(Some)
}

/// `$name:mods` without braces: the modifiers at `rest` and how many
/// characters they take (0 if none).
fn unbraced_modifiers(val: Val, rest: &[char]) -> ExpResult<(Val, usize)> {
    let (mods, used) = parse_modifiers(rest, false)?;
    if mods.is_empty() {
        return Ok((val, 0));
    }
    Ok((run_modifiers(val, &mods)?, used))
}

fn path_root(s: &str) -> String {
    let base = s.rfind('/').map(|k| k + 1).unwrap_or(0);
    match s[base..].rfind('.') {
        Some(k) => s[..base + k].to_string(),
        None => s.to_string(),
    }
}

fn path_ext(s: &str) -> String {
    let base = s.rfind('/').map(|k| k + 1).unwrap_or(0);
    match s[base..].rfind('.') {
        Some(k) => s[base + k + 1..].to_string(),
        None => String::new(),
    }
}

fn absolute_path(s: &str) -> String {
    let full = if s.starts_with('/') {
        s.to_string()
    } else {
        format!("{}/{}", var_get("PWD"), s)
    };
    let mut parts: Vec<&str> = Vec::new();
    for p in full.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    format!("/{}", parts.join("/"))
}

// `:P`: resolves symlinks component-by-component before collapsing `..`,
// tolerating missing suffixes, long segments, and symlink loops.
fn realpath_p(s: &str) -> String {
    let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    if !s.starts_with('/') {
        for p in var_get("PWD").split('/') {
            if !p.is_empty() {
                queue.push_back(p.to_string());
            }
        }
    }
    for p in s.split('/') {
        if !p.is_empty() {
            queue.push_back(p.to_string());
        }
    }
    let mut resolved = std::path::PathBuf::from("/");
    let mut hops = 0usize;
    while let Some(comp) = queue.pop_front() {
        match comp.as_str() {
            "" | "." => {}
            ".." => {
                resolved.pop();
            }
            _ => {
                let cand = resolved.join(&comp);
                if hops < 40
                    && std::fs::symlink_metadata(&cand).is_ok_and(|m| m.file_type().is_symlink())
                {
                    if let Ok(target) = std::fs::read_link(&cand) {
                        hops += 1;
                        let target_str = target.to_string_lossy();
                        if target.is_absolute() {
                            resolved = std::path::PathBuf::from("/");
                        }
                        let tparts: Vec<String> = target_str
                            .split('/')
                            .filter(|x| !x.is_empty())
                            .map(str::to_string)
                            .collect();
                        for tp in tparts.into_iter().rev() {
                            queue.push_front(tp);
                        }
                        continue;
                    }
                }
                resolved.push(comp);
            }
        }
    }
    resolved.to_string_lossy().into_owned()
}

// zsh `:q` / `(q)`: backslash before the characters special to the
// shell; `~` only where it would expand (at the start).
// (V): control characters as ^X, DEL as ^?.
fn visible(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if let Some(b) = crate::shell::decode_raw_byte(c) {
            out.push_str(&crate::shell::nice_byte_char(b));
        } else {
            match c {
                '\x7f' => out.push_str("^?"),
                c if (c as u32) < 32 => {
                    out.push('^');
                    out.push(char::from(b'@' + c as u8));
                }
                c => out.push(c),
            }
        }
    }
    out
}

// (qqqq): $'...' quoting.
// (qqqq): $'...' with C escapes, other control characters in octal.
fn dollar_quote(s: &str) -> String {
    let mut out = String::from("$'");
    for c in s.chars() {
        if let Some(b) = crate::shell::decode_raw_byte(c) {
            out.push_str(&format!("\\{:03o}", b));
            continue;
        }
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if (c as u32) < 32 || c == '\x7f' => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

// (q-) and (q+): quoted only as much as needed. (q-) leaves a string
// whose only special characters are `'` unquoted, escaping them; (q+)
// uses $'...' when there are control characters.
fn quote_minimal(s: &str, plus: bool) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if plus
        && s.chars()
            .any(|c| (c as u32) < 32 || c == '\x7f' || crate::shell::decode_raw_byte(c).is_some())
    {
        let mut out = String::from("$'");
        for c in s.chars() {
            if let Some(b) = crate::shell::decode_raw_byte(c) {
                out.push_str(&format!("\\{:03o}", b));
                continue;
            }
            match c {
                '\\' => out.push_str("\\\\"),
                '\'' => out.push_str("\\'"),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                '\x7f' => out.push_str("\\C-?"),
                c if (c as u32) < 32 => {
                    out.push_str("\\C-");
                    out.push(char::from(b'@' + c as u8));
                }
                c => out.push(c),
            }
        }
        out.push('\'');
        return out;
    }
    let special = |k: usize, c: char| {
        c == '\''
            || c.is_whitespace()
            || "|&;<>()$`\\\"*?[]#^!=".contains(c)
            || (c == '~' && k == 0)
    };
    if !s.chars().enumerate().any(|(k, c)| special(k, c)) {
        return s.to_string();
    }
    let rc = crate::options::is_set("rcquotes");
    if plus {
        let mut out = String::new();
        let mut inquote = false;
        for c in s.chars() {
            if c == '\'' {
                if rc {
                    if !inquote {
                        out.push('\'');
                        inquote = true;
                    }
                    out.push_str("''");
                } else {
                    if inquote {
                        out.push('\'');
                        inquote = false;
                    }
                    out.push_str("\\'");
                }
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
        return out;
    }
    let mut out = String::new();
    let mut quotesub = false;
    let mut quotestart = 0;
    for (k, c) in s.chars().enumerate() {
        if special(k, c) {
            if !quotesub {
                if c == '\'' {
                    out.push('\\');
                    out.push('\'');
                } else {
                    out.insert(quotestart, '\'');
                    quotesub = true;
                    out.push(c);
                }
                quotestart = out.len();
            } else if c == '\'' {
                if !rc {
                    out.push_str("'\\'");
                    quotesub = false;
                    quotestart = out.len();
                } else {
                    out.push_str("''");
                }
            } else {
                out.push(c);
            }
        } else {
            out.push(c);
        }
    }
    if quotesub {
        out.push('\'');
    }
    out
}

// (g:opts:): echo/print-style escapes; `o` bare octal, `e` \E/\M-/\C-, `c` ^X.
fn g_escapes(s: &str, opts: &str) -> ExpResult<String> {
    let chars: Vec<char> = s.chars().collect();
    let emacs = opts.contains('e');
    ansi_c_opts(
        &chars,
        !emacs,
        opts.contains('o'),
        emacs,
        opts.contains('c'),
    )
    .or_else(|msg| fail(&msg))
}

pub(crate) fn backslash_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let is_unprint =
        |c: char| (c as u32) < 32 || c == '\x7f' || crate::shell::decode_raw_byte(c).is_some();
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    for (k, &c) in chars.iter().enumerate() {
        if is_unprint(c) {
            // zsh quotes each unprintable character on its own.
            out.push_str("$'");
            if let Some(b) = crate::shell::decode_raw_byte(c) {
                out.push_str(&format!("\\{:03o}", b));
            } else {
                match c {
                    '\0' => {
                        if chars.get(k + 1).is_some_and(|n| ('0'..='7').contains(n)) {
                            out.push_str("\\000");
                        } else {
                            out.push_str("\\0");
                        }
                    }
                    '\x07' => out.push_str("\\a"),
                    '\x08' => out.push_str("\\b"),
                    '\x0c' => out.push_str("\\f"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    '\x0b' => out.push_str("\\v"),
                    other => out.push_str(&format!("\\{:03o}", other as u32)),
                }
            }
            out.push('\'');
            continue;
        }
        if " '\\\"`$&|;<>()[]{}*?#^".contains(c) || (matches!(c, '~' | '=') && k == 0) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// One level of shell quote removal, without expansion.
fn unquote(s: &str) -> String {
    unquote_checked(s, false).unwrap_or_default()
}

fn unquote_checked(s: &str, report: bool) -> ExpResult<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' if i + 1 < chars.len() => {
                out.push(chars[i + 1]);
                i += 2;
            }
            '\'' => {
                let Some(end) = scan_squote(&chars, i) else {
                    if report {
                        return fail("unmatched '");
                    }
                    out.extend(&chars[i + 1..]);
                    break;
                };
                out.extend(&chars[i + 1..end - 1]);
                i = end;
            }
            '$' if chars.get(i + 1) == Some(&'\'') => {
                let Some(end) = scan_dollar_squote(&chars, i + 1) else {
                    if report {
                        return fail("unmatched '");
                    }
                    out.push_str(&dollar_squote_text(&chars[i + 2..])?);
                    break;
                };
                out.push_str(&dollar_squote_text(&chars[i + 2..end - 1])?);
                i = end;
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\'
                        && chars
                            .get(i + 1)
                            .is_some_and(|c| matches!(c, '$' | '`' | '"' | '\\'))
                    {
                        i += 1;
                    }
                    out.push(chars[i]);
                    i += 1;
                }
                if i >= chars.len() && report {
                    return fail("unmatched \"");
                }
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

// `(z)`: splits like the shell lexer, keeping quotes in the words.
// `(z)`: splits like the shell's lexer. `opts` are (Z) letters: `c`
// keeps comments as words, `C` drops them, `n` treats newlines as
// blanks. From a parse error on, the rest of the text is one word.
fn shell_words(s: &str, opts: &str) -> Vec<String> {
    use crate::lexer::Tok;
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let keep_comments = opts.contains('c');
    let drop_comments = opts.contains('C');
    let nl_blank = opts.contains('n');
    let mut lx = crate::lexer::Lexer::new(s, true);
    lx.comments = false;
    let mut out: Vec<String> = Vec::new();
    let rest_from = |k: usize| -> String { chars[k..].iter().collect() };
    // Unbalanced parentheses from `k` on: an unfinished `(`, `((` or `$(`.
    let unbalanced = |k: usize| {
        let mut depth = 0i32;
        for &c in &chars[k..] {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
        }
        depth > 0
    };
    loop {
        let mut p = lx.pos();
        while p < n && (matches!(chars[p], ' ' | '\t') || (nl_blank && chars[p] == '\n')) {
            p += 1;
        }
        lx.set_pos(p);
        if p >= n {
            break;
        }
        if chars[p] == '#' && (keep_comments || drop_comments) {
            let mut e = p;
            while e < n && chars[e] != '\n' {
                e += 1;
            }
            if keep_comments {
                out.push(chars[p..e].iter().collect());
            }
            lx.set_pos(e);
            continue;
        }
        let tok = match lx.next_token() {
            Ok(t) => t,
            Err(_) => {
                if crate::options::is_set("cshjunkiequotes") && matches!(chars[p], '\'' | '"' | '`')
                {
                    let mut nl = p + 1;
                    while nl < n && (chars[nl] != '\n' || chars[nl - 1] == '\\') {
                        nl += 1;
                    }
                    out.push(chars[p..nl].iter().collect());
                    lx.set_pos(nl);
                    continue;
                }
                out.push(rest_from(p));
                break;
            }
        };
        match tok {
            Tok::Eof => break,
            Tok::Word(w) => {
                // A command substitution that does not parse ends the split.
                let bad_subst = w.find("$(").is_some_and(|k| {
                    let wc: Vec<char> = w.chars().collect();
                    let k = w[..k].chars().count();
                    if wc.get(k + 2) == Some(&'(') {
                        return false;
                    }
                    match crate::lexer::scan_paren(&wc, k + 1) {
                        Some(end) => {
                            let inner: String = wc[k + 2..end - 1].iter().collect();
                            crate::parser::Parser::new(&inner, true)
                                .parse_program()
                                .is_err()
                        }
                        None => true,
                    }
                });
                if bad_subst {
                    out.push(rest_from(p));
                    break;
                }
                // `name=(`: an array assignment starts one word.
                if w.ends_with('=') && chars.get(lx.pos()) == Some(&'(') {
                    lx.set_pos(lx.pos() + 1);
                    out.push(format!("{}(", w));
                    continue;
                }
                out.push(w);
            }
            Tok::Op(o) => {
                if o == "(" && unbalanced(p) {
                    out.push(rest_from(p));
                    break;
                }
                out.push(o.to_string());
            }
            Tok::IoNumber(n) => out.push(n.to_string()),
            Tok::IoVar(v) => out.push(format!("{{{}}}", v)),
            Tok::Arith(e) => {
                // `for (( a; b; c ))`: the parts, each with its `;`.
                if out.last().is_some_and(|w| w == "for") {
                    out.push("((".to_string());
                    let mut part = String::new();
                    for c in e.chars() {
                        if part.is_empty() && c.is_whitespace() {
                            continue;
                        }
                        part.push(c);
                        if c == ';' {
                            out.push(std::mem::take(&mut part));
                        }
                    }
                    if !part.is_empty() {
                        out.push(part);
                    }
                    out.push("))".to_string());
                } else {
                    out.push(format!("(({}))", e));
                }
            }
            Tok::Newline => {
                if !nl_blank {
                    out.push(";".to_string());
                }
            }
        }
    }
    out
}

/// `${(t)name}`: zsh's type string for parameter `name`.
pub fn param_type(name: &str) -> String {
    type_of(name)
}

// `${(t)...}` of the specials computed on each read (no parameter of
// their own unless a user's hides them), as zsh reports them.
fn computed_special_type(name: &str) -> Option<String> {
    if !crate::exec::sh_emulation_pub() && !with_shell(|s| s.param(name).is_some()) {
        match name {
            "zsh_eval_context" => return Some("array-readonly-tied-special".to_string()),
            "ZSH_EVAL_CONTEXT" => return Some("scalar-readonly-tied-special".to_string()),
            _ => {}
        }
    }
    if !crate::shell::DYNAMIC_NAMES.contains(&name) || with_shell(|s| s.param(name).is_some()) {
        return None;
    }
    let module = |f: &str| crate::modules::feature_available(&format!("p:{}", f));
    let hide = if crate::typeset::is_unhidden(name) {
        ""
    } else {
        "-hide"
    };
    Some(match name {
        "SECONDS" if crate::shell::SECONDS_FLOAT.load(std::sync::atomic::Ordering::Relaxed) => {
            "float-special".to_string()
        }
        "RANDOM" | "SECONDS" | "UID" | "EUID" | "GID" | "EGID" | "COLUMNS" | "LINES" => {
            "integer-special".to_string()
        }
        "ZSH_SUBSHELL" | "LINENO" => "integer-readonly-special".to_string(),
        "ZSH_EXEPATH" => "scalar-readonly-special".to_string(),
        "USERNAME" => "scalar-special".to_string(),
        "EPOCHSECONDS" if module(name) => format!("integer-readonly{}-hideval-special", hide),
        "EPOCHREALTIME" if module(name) => format!("float-readonly{}-hideval-special", hide),
        _ => return None,
    })
}

// `${(t)name}`, in zsh's format: the type, then -local and attributes.
fn type_of(name: &str) -> String {
    use crate::params::*;
    if let Some(t) = computed_special_type(name) {
        return t;
    }
    with_shell(|s| {
        let Some(p) = s.param(name) else {
            return String::new();
        };
        let a = p.attrs;
        // An unset parameter has no type, unless declared (TYPESET_TO_UNSET).
        if a & PRIVATE_HIDDEN != 0 || (a & UNSET != 0 && a & DECLARED == 0) {
            return String::new();
        }
        let mut t = String::from(match &p.value {
            Value::Array(_) => "array",
            Value::Assoc(_) => "association",
            Value::Scalar(_) if a & NAMEREF != 0 => "nameref",
            Value::Scalar(_) if a & INTEGER != 0 => "integer",
            Value::Scalar(_) if a & (FLOAT_E | FLOAT_F) != 0 => "float",
            Value::Scalar(_) => "scalar",
        });
        if p.level > 0 {
            t.push_str("-local");
        }
        for (bit, word) in [
            (LEFT, "-left"),
            (RIGHT_B, "-right_blanks"),
            (RIGHT_Z, "-right_zeros"),
            (LOWER, "-lower"),
            (UPPER, "-upper"),
            (READONLY, "-readonly"),
            (TAG, "-tag"),
        ] {
            if a & bit != 0 {
                t.push_str(word);
            }
        }
        if p.tie.is_some() {
            t.push_str("-tied");
        }
        for (bit, word) in [
            (EXPORT, "-export"),
            (UNIQUE, "-unique"),
            (HIDE, "-hide"),
            (HIDEVAL, "-hideval"),
            // zsh makes a private parameter special.
            (SPECIAL | PRIVATE, "-special"),
        ] {
            if a & bit != 0 {
                t.push_str(word);
            }
        }
        t
    })
}

// zsh's dopadding (Src/subst.c): pads or truncates `s` to `prenum`
// characters on the left and/or `postnum` on the right. `*one` strings
// go next to the text once; `*mul` strings repeat to fill.
fn dopadding(
    s: &str,
    prenum: usize,
    postnum: usize,
    preone: Option<&str>,
    postone: Option<&str>,
    premul: Option<&str>,
    postmul: Option<&str>,
) -> String {
    let def: Vec<char> = ifs().chars().next().map(|c| vec![c]).unwrap_or_default();
    let one = |o: Option<&str>| {
        o.map(|x| {
            if x.is_empty() {
                def.clone()
            } else {
                x.chars().collect::<Vec<_>>()
            }
        })
    };
    let mul = |m: Option<&str>| match m {
        Some(x) if !x.is_empty() => x.chars().collect::<Vec<_>>(),
        _ => def.clone(),
    };
    let (preone, postone, premul, postmul) = (one(preone), one(postone), mul(premul), mul(postmul));
    let s: Vec<char> = s.chars().collect();
    let ls = s.len();
    const MAX_PAD_WIDTH: usize = 1_000_000;
    let prenum = prenum.min(MAX_PAD_WIDTH);
    let postnum = postnum.min(MAX_PAD_WIDTH);
    if prenum + postnum == ls {
        return s.iter().collect();
    }
    let mut r: Vec<char> = Vec::new();
    // Left fill of width f before text (f > 0).
    let left_fill = |r: &mut Vec<char>, f: usize| {
        let lone = preone.as_ref().map(|p| p.len()).unwrap_or(0);
        if f <= lone {
            if let Some(p) = &preone {
                r.extend(&p[lone - f..]);
            }
        } else {
            let f = f - lone;
            let lm = premul.len();
            if let Some(reps) = f.checked_div(lm) {
                let m = f % lm;
                if m > 0 {
                    r.extend(&premul[lm - m..]);
                }
                for _ in 0..reps {
                    r.extend(&premul);
                }
            }
            if let Some(p) = &preone {
                r.extend(p);
            }
        }
    };
    let right_fill = |r: &mut Vec<char>, f: usize| {
        let lone = postone.as_ref().map(|p| p.len()).unwrap_or(0);
        if f <= lone {
            if let Some(p) = &postone {
                r.extend(&p[..f]);
            }
        } else {
            let f = f - lone;
            if let Some(p) = &postone {
                r.extend(p);
            }
            let lm = postmul.len();
            if let Some(reps) = f.checked_div(lm) {
                for _ in 0..reps {
                    r.extend(&postmul);
                }
                r.extend(&postmul[..f % lm]);
            }
        }
    };
    let mut pos = 0;
    if prenum > 0 {
        let first = if postnum > 0 { ls / 2 } else { ls };
        if prenum <= first {
            pos = first - prenum;
            r.extend(&s[pos..pos + prenum]);
            pos += prenum;
        } else {
            left_fill(&mut r, prenum - first);
            r.extend(&s[..first]);
            pos = first;
        }
        if postnum == 0 {
            return r.into_iter().collect();
        }
    }
    if postnum > 0 {
        let rest = &s[pos..];
        let need = if prenum > 0 { ls - ls / 2 } else { ls };
        if postnum <= need {
            r.extend(rest.iter().take(postnum));
        } else {
            r.extend(rest);
            right_fill(&mut r, postnum - need);
        }
    }
    r.into_iter().collect()
}

fn split_items(items: Vec<String>, sep: &str, keep_empty: bool) -> Vec<String> {
    let mut out = Vec::new();
    for item in items {
        for part in item.split(sep) {
            if keep_empty || !part.is_empty() {
                out.push(part.to_string());
            }
        }
    }
    out
}

/// Name order with digit runs compared by value, as NUMERIC_GLOB_SORT
/// and the (n) flag use.
pub fn numeric_order(a: &str, b: &str) -> std::cmp::Ordering {
    numeric_cmp(a, b, false)
}

// zsh's numeric sort ((n), and (-) for signed numbers): runs of digits
// compare by value, everything else character by character.
fn numeric_cmp(a: &str, b: &str, signed: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (mut i, mut j) = (0, 0);
    let number = |s: &[char], k: usize| -> Option<(i128, usize)> {
        let neg =
            signed && s.get(k) == Some(&'-') && s.get(k + 1).is_some_and(|c| c.is_ascii_digit());
        let start = if neg { k + 1 } else { k };
        if !s.get(start).is_some_and(|c| c.is_ascii_digit()) {
            return None;
        }
        let mut end = start;
        while end < s.len() && s[end].is_ascii_digit() {
            end += 1;
        }
        let text: String = s[start..end].iter().collect();
        let v: i128 = text.parse().unwrap_or(i128::MAX);
        Some((if neg { -v } else { v }, end))
    };
    while i < a.len() && j < b.len() {
        if let (Some((x, ni)), Some((y, nj))) = (number(&a, i), number(&b, j)) {
            match x.cmp(&y) {
                Ordering::Equal => {
                    i = ni;
                    j = nj;
                    continue;
                }
                o => return o,
            }
        }
        match a[i].cmp(&b[j]) {
            Ordering::Equal => {
                i += 1;
                j += 1;
            }
            o => return o,
        }
    }
    (a.len() - i).cmp(&(b.len() - j))
}

// The flags in zsh's order (the "Rules" in zshexpn): forced joining,
// splitting, case, escapes, quoting, directory names, visibility,
// lexical splitting, uniqueness, ordering, re-evaluation, padding.
fn apply_flags(val: Val, f: &PFlags, quoted: bool) -> ExpResult<Val> {
    // `${=x-"a b" $y}`: splitting leaves the quoted parts of the word whole.
    let mut ifs_done = false;
    let val = match val {
        Val::Tagged(fields) if f.ifs_split && f.split.is_none() => {
            let ifs = ifs();
            let mut out = Vec::new();
            for field in &fields {
                let marked: Vec<(char, u8)> = field
                    .iter()
                    .map(|&(c, t)| (c, if t & Q != 0 { t } else { t | S }))
                    .collect();
                out.extend(split_field(&marked, &ifs));
            }
            ifs_done = true;
            Val::Tagged(out)
        }
        Val::Tagged(fields)
            if !quoted
                && f.quote == 1
                && !f.quote_min
                && !f.unquote
                && !f.eval
                && f.split.is_none()
                && f.join.is_none()
                && !f.ifs_split
                && f.case.is_none()
                && f.pad_left.is_none()
                && f.pad_right.is_none() =>
        {
            // zsh's (q) (QT_BACKSLASH) does not untokenize an unquoted
            // literal word from ${:-word}: tokenized `=` (at index 0) and
            // pattern characters stay unescaped and active.
            let mut out = Vec::new();
            for field in &fields {
                let clean: Vec<(char, u8)> = field
                    .iter()
                    .copied()
                    .filter(|(_, t)| t & MARK == 0)
                    .collect();
                if clean.is_empty() {
                    out.push(vec![('\'', Q), ('\'', Q)]);
                    continue;
                }
                let mut qf = Vec::new();
                for (k, &(c, t)) in clean.iter().enumerate() {
                    if t & B != 0 && ((c == '=' && k == 0) || "*?[]~#^".contains(c)) {
                        qf.push((c, 0));
                        continue;
                    }
                    if " \t\n\\'\"`$&|;<>()[]{}*?#^".contains(c)
                        || (matches!(c, '~' | '=') && k == 0)
                    {
                        qf.push(('\\', Q));
                    }
                    qf.push((c, Q));
                }
                out.push(qf);
            }
            return Ok(Val::Tagged(out));
        }
        v => v,
    };
    let mut val = plain(val);
    let list_in = is_list(&val);
    let wrap = |items: Vec<String>, was_list: bool| {
        if was_list {
            Val::List(items, false)
        } else {
            Val::Str(items.into_iter().next().unwrap_or_default())
        }
    };
    if f.charcode {
        let was_list = is_list(&val);
        let mut out = Vec::new();
        for w in val_items(&val) {
            match crate::arith::eval(&w) {
                Ok(n) => {
                    let c_loc = crate::shell::is_c_locale();
                    let valid = u32::try_from(n)
                        .ok()
                        .filter(|&v| !c_loc || v <= 0x7f)
                        .and_then(char::from_u32);
                    match valid {
                        Some(c) => out.push(c.to_string()),
                        None if f.report => return fail("character not in range"),
                        None if n >= 0 => {
                            out.push(crate::shell::raw_byte_char((n & 0xff) as u8).to_string());
                        }
                        None => {}
                    }
                }
                Err(e) if f.report => return fail(&e),
                Err(_) => {}
            }
        }
        val = if was_list {
            Val::List(out, false)
        } else {
            match out.pop() {
                Some(c) => Val::Str(c),
                None => Val::List(Vec::new(), false),
            }
        };
    }
    if f.indirect {
        val = match val {
            // (Pk), (Pkv): the keys (and values) of the named association.
            Val::Str(name) if f.keys => match assoc_keys(&name) {
                Some(keys) if f.values => Val::List(
                    keys.iter()
                        .flat_map(|k| [k.clone(), assoc_get(&name, k).unwrap_or_default()])
                        .collect(),
                    false,
                ),
                Some(keys) => Val::List(keys, true),
                None => lookup(&name),
            },
            Val::Str(name) => lookup(&name),
            v => v,
        };
    }
    let splitting = f.split.is_some() || (f.ifs_split && !ifs_done);
    let mut list = is_list(&val) || list_in;
    // Rule 10: (j), or splitting, first joins the words; (@) keeps them
    // apart for splitting.
    if (f.join.is_some() || (splitting && !f.at)) && is_list(&val) {
        let sep = f.join.clone().unwrap_or_else(ifs_first);
        // (~j): the words stay literal around a separator that is pattern
        // text; the whole value is then substituted as a pattern.
        let items: Vec<String> = if f.join_pattern && !quoted {
            val_items(&val).iter().map(|w| escape_pattern(w)).collect()
        } else {
            val_items(&val)
        };
        val = Val::Str(items.join(&sep));
        list = false;
    }
    if let Some(sep) = &f.split {
        let items = val_items(&val);
        val = Val::List(
            split_items(items, sep, (f.at && quoted) || f.words_empty),
            false,
        );
    } else if f.ifs_split && !ifs_done {
        let ifs = ifs();
        let words: Vec<String> = val_items(&val)
            .iter()
            .flat_map(|s| {
                s.split(|c| ifs.contains(c))
                    .filter(|w| !w.is_empty())
                    .map(|w| w.to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        val = Val::List(words, false);
    } else if f.zsplit {
        let items = val_items(&val);
        val = Val::List(
            items
                .iter()
                .flat_map(|s| shell_words(s, &f.zopts))
                .collect(),
            false,
        );
    }
    list = is_list(&val) || list;
    let mut items = val_items(&val);
    if matches!(val, Val::Unset) && !list {
        if f.quote > 0 || f.quote_min || f.pad_left.is_some() || f.pad_right.is_some() {
            items.push(String::new());
        } else {
            return Ok(Val::Unset);
        }
    }
    for s in items.iter_mut() {
        match f.case {
            Some('U') => *s = s.to_uppercase(),
            Some('L') => *s = s.to_lowercase(),
            Some('C') => {
                let mut out = String::new();
                let mut start = true;
                for c in s.chars() {
                    if start {
                        out.extend(c.to_uppercase());
                    } else {
                        out.extend(c.to_lowercase());
                    }
                    start = !c.is_alphanumeric();
                }
                *s = out;
            }
            _ => {}
        }
        if f.prompt == 1 {
            *s = crate::shell::expand_prompt(s);
        } else if f.prompt > 1 {
            *s = crate::shell::render_prompt(s);
        }
        if let Some(g) = &f.gflags {
            *s = g_escapes(s, g)?;
        }
        if f.unquote {
            *s = unquote_checked(s, f.report)?;
        }
        if f.quote_min {
            *s = quote_minimal(s, f.quote_plus);
        } else {
            match f.quote {
                0 => {}
                1 => *s = backslash_quote(s),
                2 => *s = format!("'{}'", s.replace('\'', "'\\''")),
                4.. => *s = dollar_quote(s),
                _ => {
                    let mut o = String::from('"');
                    for c in s.chars() {
                        if matches!(c, '\\' | '"' | '$' | '`') {
                            o.push('\\');
                        }
                        o.push(c);
                    }
                    o.push('"');
                    *s = o;
                }
            }
        }
        if f.backslash {
            let mut out = String::new();
            for c in s.chars() {
                if "*?[]()|<>~#^\\".contains(c) {
                    out.push('\\');
                }
                out.push(c);
            }
            *s = out;
        }
        if f.tilde_dirs {
            let home = var_get("HOME");
            if !home.is_empty() && (s == &home || s.starts_with(&format!("{}/", home))) {
                *s = format!("~{}", &s[home.len()..]);
            }
        }
        if f.visible {
            *s = visible(s);
        }
    }
    if f.unique {
        let mut seen = std::collections::HashSet::new();
        items.retain(|s| seen.insert(s.clone()));
    }
    if f.sort.is_some() || f.numeric || f.signed {
        if f.numeric || f.signed {
            items.sort_by(|a, b| numeric_cmp(a, b, f.signed));
        } else if f.icase {
            items.sort_by_key(|s| s.to_lowercase());
        } else {
            items.sort();
        }
        if f.sort == Some('O') {
            items.reverse();
        }
    }
    if f.eval {
        // (e): expand each value again, as if it appeared here; the
        // results split and glob like any expansion.
        let mut fields: Vec<Vec<(char, u8)>> = Vec::new();
        for it in &items {
            let chars: Vec<char> = it.chars().collect();
            let mut tmp = Ctx::new(true, false);
            walk(
                &chars,
                if quoted { Mode::Dq } else { Mode::Unquoted },
                &mut tmp,
            )?;
            fields.extend(tmp.fields);
        }
        // Like any expansion result, it is not globbed without GLOB_SUBST.
        if !crate::options::is_set("globsubst") {
            for f in fields.iter_mut() {
                for c in f.iter_mut() {
                    c.1 |= NG;
                }
            }
        }
        // Padding still applies to what (e) produced.
        if f.pad_left.is_none() && f.pad_right.is_none() {
            return Ok(Val::Tagged(fields));
        }
        items = fields.iter().map(|f| tagged_text(f)).collect();
    }
    if f.pad_left.is_some() || f.pad_right.is_some() {
        let (pre, premul, preone) = f.pad_left.clone().unwrap_or((0, None, None));
        let (post, postmul, postone) = f.pad_right.clone().unwrap_or((0, None, None));
        for s in items.iter_mut() {
            *s = dopadding(
                s,
                pre,
                post,
                preone.as_deref(),
                postone.as_deref(),
                premul.as_deref(),
                postmul.as_deref(),
            );
        }
    }
    let mut out = wrap(items, list);
    if f.at {
        if let Val::List(items, _) = out {
            out = Val::List(items, false);
        }
    }
    Ok(out)
}

// zsh's non-forking command substitutions: `${ cmd }` (its output),
// `${| cmd }` (the value it leaves in a local REPLY) and `${{name} cmd }`
// (the value of name). The commands run in this shell, as if in a
// function: `local` and `return` work, positional parameters are shared.
fn nofork(body: &[char], quoted: bool) -> ExpResult<Option<Val>> {
    enum Kind {
        Stdout,
        Reply,
        Var(String),
    }
    let (kind, code) = match body.first() {
        Some(' ' | '\t' | '\n') => (Kind::Stdout, &body[1..]),
        Some('|') => (Kind::Reply, &body[1..]),
        Some('{') => {
            let Some(close) = body.iter().position(|&c| c == '}') else {
                return Ok(None);
            };
            let name: String = body[1..close].iter().collect();
            if !crate::lexer::is_valid_name(&name) {
                return Ok(None);
            }
            (Kind::Var(name), &body[close + 1..])
        }
        _ => return Ok(None),
    };
    let code: String = code.iter().collect();
    // Wrap `${ ... }` as zsh does (`{| ...\n}` / `{\n...\n;}`) so trailing
    // comments and closing-brace line numbers match `parse_string(..., 0)`.
    let parse_src = if matches!(kind, Kind::Stdout) {
        format!("{{\n{}\n;}}", code)
    } else {
        code.clone()
    };
    let mut parser = crate::parser::Parser::new(&parse_src, true);
    loop {
        match parser.parse_complete_command() {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                let msg = match e {
                    crate::parser::ParseError::Syntax(m) => m,
                    crate::parser::ParseError::Incomplete => "closing brace expected".to_string(),
                };
                let delta = parser.line().saturating_sub(1);
                let saved = crate::shell::with_err_ctx(|ctx| {
                    let prev = ctx.lineno;
                    ctx.lineno = prev.saturating_add(delta);
                    prev
                });
                let res = fail(&msg);
                crate::shell::with_err_ctx(|ctx| ctx.lineno = saved);
                return res;
            }
        }
    }
    let capture = match kind {
        Kind::Stdout => {
            let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
            let path = format!(
                "{}/meowsh-nofork-{}-{}",
                dir,
                std::process::id(),
                NOFORK_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            );
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path);
            let _ = std::fs::remove_file(&path);
            match file {
                Ok(f) => Some(f),
                Err(e) => return fail(&format!("{}: {}", path, e)),
            }
        }
        _ => None,
    };
    use std::os::fd::AsRawFd;
    let saved_out = capture.as_ref().map(|f| unsafe {
        let saved = libc::dup(1);
        libc::dup2(f.as_raw_fd(), 1);
        saved
    });
    with_shell(|s| s.func_depth += 1);
    crate::shell::push_scope();
    if matches!(kind, Kind::Reply) {
        crate::shell::declare_local("REPLY");
        with_shell(|s| {
            if let Some(p) = s.param_mut("REPLY") {
                p.value = crate::params::Value::Scalar(String::new());
                p.attrs = crate::params::UNSET;
            }
        });
    }
    let mut st =
        crate::shell::in_eval_context("cmdsubst", || crate::exec::run_string_named(&code, None));
    with_shell(|s| match s.flow {
        crate::types::Flow::Return(n) => {
            st = n;
            s.flow = crate::types::Flow::None;
        }
        f @ (crate::types::Flow::Break(_) | crate::types::Flow::Continue(_)) => {
            s.flow = crate::types::Flow::None;
            if let Ok(mut d) = crate::exec::DEFERRED_FLOW.lock() {
                *d = Some(f);
            }
        }
        _ => {}
    });
    let value = |name: &str| -> Val {
        with_shell(|s| match s.array(name) {
            Some(items) => Val::List(items.to_vec(), true),
            None => Val::Str(s.scalar(name).map(str::to_string).unwrap_or_default()),
        })
    };
    let result = match &kind {
        Kind::Reply => value("REPLY"),
        Kind::Var(name) => value(name),
        Kind::Stdout => Val::Unset,
    };
    crate::shell::pop_scope();
    with_shell(|s| s.func_depth -= 1);
    let result = match (capture, saved_out) {
        (Some(mut f), Some(saved)) => {
            unsafe {
                libc::dup2(saved, 1);
                libc::close(saved);
            }
            use std::io::{Read, Seek};
            let mut bytes = Vec::new();
            let _ = f.seek(std::io::SeekFrom::Start(0));
            let _ = f.read_to_end(&mut bytes);
            let mut out = String::from_utf8_lossy(&bytes).into_owned();
            // Unquoted, zsh drops one trailing newline; emulations drop
            // them all, quoted or not.
            if with_shell(|s| s.emulation != crate::options::Emulation::Zsh) {
                while out.ends_with('\n') {
                    out.pop();
                }
            } else if !quoted && out.ends_with('\n') {
                out.pop();
            }
            Val::Str(out)
        }
        _ => result,
    };
    with_shell(|s| {
        s.last_cmdsub_status = Some(st);
        s.last_status = st;
    });
    Ok(Some(result))
}

static NOFORK_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

thread_local! {
    // How many `${...}` are being expanded; `<(` inside one is not
    // process substitution (zsh's lexer only sees it outside braces).
    static IN_BRACES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

thread_local! {
    // Set when a (*) flag turned EXTENDED_GLOB on for its expansion.
    static STAR_EXTGLOB: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn braced(body: &[char], mode: Mode) -> ExpResult<Val> {
    IN_BRACES.with(|b| b.set(b.get() + 1));
    let prev_dq = IN_DQ_SUB.with(|d| {
        let p = d.get();
        if mode == Mode::Dq {
            d.set(true);
        }
        p
    });
    let outer = STAR_EXTGLOB.with(|s| s.replace(false));
    let r = braced_inner(body, mode);
    if STAR_EXTGLOB.with(|s| s.replace(outer)) {
        crate::options::set("extendedglob", false);
    }
    IN_DQ_SUB.with(|d| d.set(prev_dq));
    IN_BRACES.with(|b| b.set(b.get() - 1));
    r
}

fn braced_inner(body: &[char], mode: Mode) -> ExpResult<Val> {
    if let Some(v) = nofork(body, mode != Mode::Unquoted)? {
        return Ok(v);
    }
    let text: String = body.iter().collect();
    let bad = || fail::<Val>("bad substitution");
    if body.is_empty() {
        return bad();
    }
    let quoted = mode != Mode::Unquoted;
    let mut i = 0;
    let mut flags = PFlags::default();
    if body[0] == '(' {
        FLAG_ERR_AT.with(|e| e.set(None));
        let Some((f, end)) = parse_flags(body) else {
            // zsh counts from the `$`, 1-based.
            return match FLAG_ERR_AT.with(|e| e.get()) {
                Some(k) => fail(&format!(
                    "error in flags near position {} in '${{{}}}'",
                    k + 3,
                    text
                )),
                None => fail(&format!("error in flags in '${{{}}}'", text)),
            };
        };
        if f.extglob && !crate::options::quick("extendedglob") {
            crate::options::set("extendedglob", true);
            STAR_EXTGLOB.with(|s| s.set(true));
        }
        flags = f;
        i = end;
    }
    // Prefix operators: # length, + set test, ^ rc-expansion, = split,
    // ~ glob, ! bash indirection.
    // RC_EXPAND_PARAM makes every array expand as ${^name}.
    let (mut length, mut set_test, mut rc, mut ifs_split, mut indirect) = (
        false,
        false,
        crate::options::is_set("rcexpandparam"),
        false,
        false,
    );
    while let Some(&c) = body.get(i) {
        let next = body.get(i + 1).copied();
        let starts_param = |n: Option<char>| {
            n.is_some_and(|n| {
                n == '_'
                    || n == '.'
                    || n == '$'
                    || n == '('
                    || n.is_alphanumeric()
                    || is_special_param(n)
            })
        };
        match c {
            // `${##}` is the length of $#, but `${##pat}` removes pat
            // from $#.
            '#' if starts_param(next) && !(next == Some('#') && i + 2 < body.len()) => {
                length = true
            }
            '+' if starts_param(next) => set_test = true,
            // `^` turns RC_EXPAND_PARAM on for this expansion, `^^` off.
            '^' if starts_param(next) || matches!(next, Some('^' | ':' | '~' | '=')) => {
                rc = next != Some('^');
                if !rc {
                    i += 1;
                }
            }
            // `=` splits words, `==` turns SH_WORD_SPLIT off; `~` and
            // `~~` do the same for GLOB_SUBST (see prefix_toggles).
            '=' if starts_param(next) || matches!(next, Some('=' | ':' | '~' | '^' | '"')) => {
                ifs_split = next != Some('=');
                if !ifs_split {
                    i += 1;
                }
            }
            // `~` is never a parameter name: ${~:-*} has no name.
            '~' => {}
            '!' if starts_param(next) && i == 0 => indirect = true,
            _ => break,
        }
        i += 1;
    }

    // The value source: a nested ${...} or $(...), or a parameter name.
    let mut name = String::new();
    let mut sub = None;
    let mut val;
    let nested = body.get(i) == Some(&'$');
    let nested_brace = nested && body.get(i + 1) == Some(&'{');
    // `${${(P)name}[sub]}` and `${(k)${(P)name}}`: the parameter name holds
    // stays a parameter, so the outer subscript and flags act on it (a
    // hash keeps its keys).
    let p_target = (body.get(i) == Some(&'$') && body.get(i + 1) == Some(&'{'))
        .then(|| scan_brace(body, i + 1, false))
        .flatten()
        .filter(|&end| body.get(end).is_none_or(|&c| c == '['))
        .and_then(|end| {
            let inner: String = body[i + 2..end - 1].iter().collect();
            let ident = inner.strip_prefix("(P)")?;
            let simple = crate::lexer::is_valid_name(ident)
                || (!ident.is_empty() && ident.bytes().all(|b| b.is_ascii_digit()));
            let nested = ident.starts_with("${")
                && scan_brace(&ident.chars().collect::<Vec<_>>(), 1, false)
                    == Some(ident.chars().count());
            (simple || nested).then(|| (ident.to_string(), end))
        });
    // ${="..."}: a quoted string is the value.
    let dq_end = (body.get(i) == Some(&'"'))
        .then(|| crate::lexer::scan_dquote(body, i))
        .flatten()
        .filter(|&e| e == body.len());
    if let Some(end) = dq_end {
        let text: String = body[i..end].iter().collect();
        val = Val::Str(expand_string(&text)?);
        i = end;
    } else if let Some((ident, end)) = p_target {
        let target = if ident.starts_with('$') {
            expand_string(&ident)?
        } else {
            match with_shell(|s| s.array(&ident).cloned()) {
                Some(a) if a.len() == 1 => a[0].clone(),
                Some(_) => return fail("parameter name reference used with array"),
                None => var_get(&ident),
            }
        };
        if body.get(end).is_none() {
            // No outer subscript: the outer flags apply to the referenced
            // parameter, as if it were named in place of the nested one.
            let base = target.split('[').next().unwrap_or_default();
            if crate::lexer::is_valid_name(base) {
                let nb: Vec<char> = body[..i].iter().copied().chain(target.chars()).collect();
                return braced_inner(&nb, mode);
            }
            val = Val::Str(String::new());
            i = end;
        } else {
            let (n, s) = match target.find('[') {
                Some(k) if target.ends_with(']') => (
                    target[..k].to_string(),
                    Some(target[k + 1..target.len() - 1].to_string()),
                ),
                _ => (target.clone(), None),
            };
            val = match &s {
                Some(sub) => param_value(&n, Some(sub))?,
                None => {
                    // Let the outer subscript act on the parameter itself.
                    name = n;
                    let k = scan_bracket_end(body, end);
                    if k <= end || body.get(k) != Some(&']') {
                        return bad();
                    }
                    sub = Some(body[end + 1..k].iter().collect());
                    i = k + 1;
                    param_value(&name, sub.as_deref())?
                }
            };
            if s.is_some() {
                i = end;
            }
        }
    } else if body.get(i) == Some(&'$') && body.get(i + 1) == Some(&'{') {
        let Some(end) = scan_brace(body, i + 1, false) else {
            return bad();
        };
        // A nested expansion is quoted when the outer one is, so a split
        // in it keeps empty fields.
        val = braced(&body[i + 2..end - 1], mode)?;
        // In double quotes a nested array joins unless it has its own (@),
        // (A), or split flag (`Val::List(_, false)`).
        let inner_at = body.get(i + 2) == Some(&'(')
            && body[i + 3..end - 1]
                .iter()
                .take_while(|&&c| c != ')')
                .any(|&c| c == '@' || c == 'A');
        if mode != Mode::Unquoted && !inner_at && body.get(end) == Some(&'[') {
            if let Val::List(items, true) = &val {
                val = Val::Str(items.join(&ifs_first()));
            }
        }
        // SH_WORD_SPLIT: an unquoted nested value is split into words
        // before the outer operators apply.
        if mode == Mode::Unquoted && crate::options::is_set("shwordsplit") {
            if let Val::Str(t) = &val {
                let ifs = ifs();
                let words: Vec<String> = t
                    .split(|c| ifs.contains(c))
                    .filter(|w| !w.is_empty())
                    .map(str::to_string)
                    .collect();
                if words.len() > 1 {
                    val = Val::List(words, false);
                }
            }
        }
        i = end;
    } else if body.get(i) == Some(&'$') && body.get(i + 1) == Some(&'(') {
        let arith_end = scan_arith(body, i + 1);
        let Some(end) = arith_end.or_else(|| scan_paren(body, i + 1)) else {
            return bad();
        };
        val = if arith_end.is_some() {
            Val::Str(arith_text(&body[i + 3..end - 2])?)
        } else {
            let src: String = body[i + 2..end - 1].iter().collect();
            Val::Str(command_subst(&src)?)
        };
        i = end;
    } else if body.get(i) == Some(&'"') {
        // ${(f)"$(cmd)"}: a quoted word as the value source.
        let Some(end) = scan_dquote(body, i) else {
            return bad();
        };
        val = Val::Str(word_string(&body[i + 1..end - 1], Mode::Dq)?);
        i = end;
    } else if let Some((n, s, end)) = parse_param(&body[i..]) {
        crate::zparam::ensure(&n);
        name = n;
        // In double quotes `\"` in a subscript is a `"`.
        sub = if mode == Mode::Dq {
            s.map(|t| t.replace("\\\"", "\""))
        } else {
            s
        };
        i += end;
        // In ksh emulation, `${!name...}` is zsh's `${(k)name...}`.
        if indirect && with_shell(|s| s.emulation == crate::options::Emulation::Ksh) {
            flags.keys = true;
            indirect = false;
        }
        // ${!ref} on a nameref: the name it refers to.
        let is_ref = with_shell(|s| {
            s.param(&name)
                .is_some_and(|p| p.attrs & crate::params::NAMEREF != 0)
        });
        if indirect && is_ref {
            return Ok(Val::Str(with_shell(|s| {
                s.scalar(&name).unwrap_or("").to_string()
            })));
        }
        if indirect {
            let target = var_get(&name);
            let tchars: Vec<char> = target.chars().collect();
            match parse_param(&tchars) {
                Some((n2, s2, e2)) if e2 == tchars.len() => {
                    name = n2;
                    sub = s2;
                }
                _ if target.is_empty() => return Ok(Val::Str(String::new())),
                _ => return fail(&format!("{}: bad substitution", target)),
            }
        }
        if set_test && i == body.len() {
            let set = if flags.no_deref && sub.is_none() {
                with_shell(|s| {
                    s.param(&name).is_some_and(|p| {
                        p.is_set()
                            && !(p.attrs & crate::params::NAMEREF != 0
                                && p.attrs & crate::params::DECLARED != 0
                                && p.text().is_empty())
                    })
                })
            } else if (name == "commands" || name == "functions") && sub.is_some() {
                !matches!(param_value(&name, sub.as_deref())?, Val::Unset)
            } else if let Some(s) = &sub {
                // ${+name[key]}: is the element set?
                !matches!(param_value(&name, Some(s))?, Val::Unset)
            } else {
                crate::shell::var_exists(&name)
            };
            return Ok(Val::Str(if set { "1" } else { "0" }.into()));
        }
        let own_ref = flags
            .no_deref
            .then(|| {
                with_shell(|s| {
                    s.param(&name)
                        .filter(|p| p.attrs & crate::params::NAMEREF != 0)
                        .map(|p| {
                            if p.attrs & crate::params::UNSET != 0
                                || (p.attrs & crate::params::DECLARED != 0 && p.text().is_empty())
                            {
                                Val::Unset
                            } else {
                                Val::Str(p.text())
                            }
                        })
                })
            })
            .flatten()
            .map(|v| match &sub {
                Some(s) => apply_subscript(v, s),
                None => Ok(v),
            })
            .transpose()?;
        val = if let Some(v) = own_ref {
            v
        } else if name == "commands" && sub.is_none() && flags.keys {
            Val::List(crate::shell::all_commands(), false)
        } else if name == "functions"
            && sub.is_none()
            && flags.keys
            && !with_shell(|s| s.assoc("functions").is_some())
        {
            let mut names: Vec<String> = with_shell(|s| s.functions.keys().cloned().collect());
            names.sort();
            Val::List(names, false)
        } else if flags.keys
            && (sub.is_none()
                || (matches!(sub.as_deref(), Some("@" | "*")) && assoc_keys(&name).is_some()))
        {
            // `${(k)h}`, and `${(k)h[@]}` on an association.
            match assoc_keys(&name) {
                Some(_) if sub.is_none() && ksh_arrays() => {
                    ksh_first(&name, param_value(&name, None)?)
                }
                Some(keys) if flags.values => Val::List(
                    keys.iter()
                        .flat_map(|k| [k.clone(), assoc_get(&name, k).unwrap_or_default()])
                        .collect(),
                    false,
                ),
                Some(keys) => Val::List(keys, true),
                None => ksh_first(&name, param_value(&name, None)?),
            }
        } else if flags.keys
            && assoc_keys(&name).is_some()
            && sub
                .as_deref()
                .and_then(search_flags)
                .is_some_and(|(sf, _)| sf.contains(['r', 'R']))
        {
            // ${(k)assoc[(r)pat]}: the key whose value matches; (R) all of them.
            // ((i) and (I) give keys anyway.)
            let s = sub.clone().unwrap();
            let (sf, word) = search_flags(&s).unwrap();
            let sf = sf.as_str();
            let hit = matcher(word, sf.contains('e'))?;
            let keys = assoc_keys(&name).unwrap_or_default();
            let mut found = keys
                .into_iter()
                .filter(|k| hit(&assoc_get(&name, k).unwrap_or_default()));
            match found.next() {
                Some(k) if sf.contains('R') && !flags.values => {
                    Val::List(std::iter::once(k).chain(found).collect(), true)
                }
                // (kv): the key and its value.
                Some(k) if flags.values => {
                    let v = assoc_get(&name, &k).unwrap_or_default();
                    Val::List(vec![k, v], false)
                }
                Some(k) => Val::Str(k),
                None => Val::Unset,
            }
        } else if flags.keys
            && !flags.values
            && sub
                .as_deref()
                .is_some_and(|s| !matches!(s, "@" | "*") && search_flags(s).is_none())
        {
            let s = sub.clone().unwrap();
            keys_subscript_value(&name, &s)?
        } else if sub.is_none() {
            ksh_first(&name, param_value(&name, None)?)
        } else {
            param_value(&name, sub.as_deref())?
        };
    } else if matches!(body.get(i), Some(':' | '-' | '+' | '=' | '?')) || i == body.len() {
        // ${:-word}, ${(%):-%x}: no parameter.
        val = Val::Unset;
    } else {
        return bad();
    }
    if flags.type_of {
        // (Pt): the type of the parameter the value names.
        let name = match (&val, flags.indirect) {
            (Val::Str(s), true) => s.clone(),
            _ => name,
        };
        // A bound reference reports the type of what it refers to.
        let _swap = crate::shell::RefSwap::enter(&name);
        let target = crate::shell::split_target_pub(&crate::shell::resolve_ref(&name));
        return Ok(Val::Str(type_of(&target)));
    }
    // Rule 5: in double quotes, without (@), an array joins into one word
    // before nested subscripts, operators, modifiers and sorting apply.
    // A nested (@) result stays an array for the subscripts.
    let dq_join = quoted
        && !flags.at
        && !length
        && flags.join.is_none()
        && flags.split.is_none()
        && !ifs_split;
    if dq_join && body.get(i) == Some(&'[') {
        if let Val::List(items, true) = &val {
            val = Val::Str(items.join(&ifs_first()));
        }
    }
    // Further subscripts apply to the value: ${${x}[2]}, ${a[2][1]}.
    while body.get(i) == Some(&'[') {
        let mut depth = 0;
        let mut k = i;
        while k < body.len() {
            match body[k] {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        if k >= body.len() {
            return bad();
        }
        let s: String = body[i + 1..k].iter().collect();
        val = if flags.keys
            && !flags.values
            && is_list(&val)
            && !matches!(s.as_str(), "@" | "*")
            && search_flags(&s).is_none()
        {
            let subc: Vec<char> = s.chars().collect();
            if top_comma(&subc).is_some() {
                return fail("invalid subscript");
            }
            let items = val_items(&val);
            let mut a = sub_index(&s, &items, true, false)?;
            if a < 0 {
                a += items.len() as i64 + 1;
            }
            if a > 0 && ksh_arrays() {
                a -= 1;
            }
            Val::Str(a.to_string())
        } else {
            apply_subscript(val, &s)?
        };
        i = k + 1;
    }

    let rest = &body[i..];
    let sorts = (flags.sort.is_some() || flags.unique || flags.numeric || flags.signed)
        && !flags.keys
        && !flags.values;
    // A bash-style offset slices the array before it is joined.
    let offset_op = rest.first() == Some(&':')
        && rest
            .get(1)
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, ' ' | '$' | '(' | '\t'));
    if dq_join && (!rest.is_empty() || sorts) && !offset_op {
        if let Val::List(items, star) = &val {
            if nested || *star {
                val = Val::Str(items.join(&ifs_first()));
            }
        }
    }
    if ifs_split {
        flags.ifs_split = true;
        flags.any = true;
    }
    let mut val = if rest.is_empty() {
        if !name.is_empty() {
            check_set(&name, &val)?;
        }
        val
    } else if rest[0] == ':' && rest.get(1) == Some(&'^') {
        // ${a:^b} zips two arrays; ${a:^^b} repeats the shorter one.
        let long = rest.get(2) == Some(&'^');
        let other: String = rest[if long { 3 } else { 2 }..].iter().collect();
        if !crate::lexer::is_valid_name(&other) {
            return fail(&format!("not an identifier: {}", other));
        }
        if !name.is_empty() {
            check_set(&name, &val)?;
        }
        let other_val = lookup(&other);
        check_set(&other, &other_val)?;
        let a = val_items(&plain(val));
        let b = val_items(&other_val);
        let n = if long {
            a.len().max(b.len())
        } else {
            a.len().min(b.len())
        };
        let mut items = Vec::new();
        // ${a:^^b} with one side empty is just the other side.
        if long && (a.is_empty() || b.is_empty()) {
            items.extend(a.iter().chain(b.iter()).cloned());
            return Ok(Val::List(items, false));
        }
        for k in 0..n {
            items.push(a[k % a.len()].clone());
            items.push(b[k % b.len()].clone());
        }
        Val::List(items, false)
    } else if rest[0] == ':' && rest.get(1) == Some(&'#') {
        // ${arr:#pattern}: drop elements that match.
        let pat = pattern_of(&rest[2..])?;
        let was_list = is_list(&val);
        let keep = flags.matching;
        let items: Vec<String> = val_items(&val)
            .into_iter()
            .filter(|s| crate::pattern::matches(&pat, s) == keep)
            .collect();
        if was_list {
            Val::List(items, false)
        } else {
            items.into_iter().next().map(Val::Str).unwrap_or(Val::Unset)
        }
    } else if let Some(v) = (rest[0] == ':')
        .then(|| apply_modifiers(plain_clone(&val), rest))
        .transpose()?
        .flatten()
    {
        v
    } else {
        ASSIGNED.with(|a| *a.borrow_mut() = None);
        apply_operator(val, &name, sub.as_deref(), rest, mode, &text, &flags)?
    };
    let assigned = ASSIGNED.with(|a| a.borrow_mut().take());
    let did_assign = assigned.is_some();
    if let (true, Some(items)) = (ifs_split, assigned) {
        val = Val::List(items, false);
        flags.ifs_split = false;
    }
    // (P) follows the stored name, not its -u/-l display form.
    if flags.indirect && sub.is_none() && matches!(val, Val::Str(_)) {
        let raw = with_shell(|s| {
            s.param(&name).and_then(|p| match &p.value {
                crate::params::Value::Scalar(v)
                    if p.attrs & (crate::params::UPPER | crate::params::LOWER) != 0 =>
                {
                    Some(v.clone())
                }
                _ => None,
            })
        });
        if let Some(r) = raw {
            val = Val::Str(r);
        }
    }
    if flags.any {
        val = apply_flags(val, &flags, quoted)?;
    }
    // "${(A)name=...}": the array assigned joins in double quotes, unless (@).
    if quoted && did_assign && !flags.at {
        if let Val::List(items, false) = val {
            val = Val::List(items, true);
        }
    }
    if length {
        let v = plain(val);
        let n = if flags.count_chars {
            val_items(&v).join(" ").chars().count()
        } else if (flags.words || flags.words_empty) && flags.split.is_none() {
            let ifs = ifs();
            val_items(&v)
                .iter()
                .map(|s| {
                    let parts = s.split(|c| ifs.contains(c));
                    if flags.words_empty {
                        parts.count()
                    } else {
                        parts.filter(|w| !w.is_empty()).count()
                    }
                })
                .sum()
        } else {
            match &v {
                Val::Unset => 0,
                Val::Str(s) => s.chars().count(),
                other => val_items(other).len(),
            }
        };
        return Ok(Val::Str(n.to_string()));
    }
    // In double quotes an outer expansion without (@) joins what a
    // nested one kept as separate words: "${${(@)a}}" is one word.
    if quoted
        && nested_brace
        && !flags.at
        && flags.split.is_none()
        && flags.join.is_none()
        && !ifs_split
    {
        if let Val::List(items, false) = val {
            val = Val::List(items, true);
        }
    }
    if rc {
        // In double quotes only an array kept as separate words ("$a[@]",
        // (@)) is expanded element by element.
        val = match plain(val) {
            Val::List(items, false) => Val::Rc(items),
            Val::List(items, true) if mode == Mode::Unquoted => Val::Rc(items),
            Val::Str(s) if mode == Mode::Unquoted => Val::Rc(vec![s]),
            other => other,
        };
    }
    Ok(val)
}

// Backslash-quotes pattern characters, so `s` matches only itself.
fn escape_pattern(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\*?[]()|~#^<>{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn plain_clone(v: &Val) -> Val {
    match v {
        Val::Unset => Val::Unset,
        Val::Str(s) => Val::Str(s.clone()),
        Val::List(items, star) => Val::List(items.clone(), *star),
        Val::Rc(items) => Val::Rc(items.clone()),
        Val::Tagged(f) => Val::Tagged(f.clone()),
    }
}

// The assignment of `${name=word}`; with (A) the word's fields become an
// array, with (AA) key/value pairs.
// The result is the parameter read back, so attributes such as -Z3
// show; `name[sub]` assigns an element or range, digits a positional
// parameter.
thread_local! {
    // Set when `${x=word}` just assigned: the assigned array is not split again.
    static ASSIGNED: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

fn assign_word(
    name: &str,
    sub: Option<&str>,
    word: &[char],
    mode: Mode,
    pf: &PFlags,
    text: &str,
) -> ExpResult<Val> {
    let positional = name.parse::<usize>().ok().filter(|&n| n > 0);
    if !crate::lexer::is_valid_name(name) && positional.is_none() {
        return fail(&format!("${{{}}}: cannot assign in this way", text));
    }
    if let Some(n) = positional {
        const MAX_POS_PARAMS: usize = 65_536;
        if n > MAX_POS_PARAMS {
            return fail(&format!(
                "${{{}}}: positional parameter index too large",
                text
            ));
        }
        let v = word_string(word, mode)?;
        with_shell(|s| {
            if s.pos_params.len() < n {
                s.pos_params.resize(n, String::new());
            }
            s.pos_params[n - 1] = v.clone();
        });
        return Ok(Val::Str(v));
    }
    if let Some(sub) = sub {
        let ok = if pf.assign_array > 0 {
            let items = val_items(&plain(word_val(word, mode)?));
            let range = expand_string(sub)?;
            let range = if range.contains(',') {
                range
            } else {
                format!("{},{}", range, range)
            };
            crate::shell::array_splice(name, &range, items)
        } else {
            let v = word_string(word, mode)?;
            crate::exec::set_subscripted(name, sub, &v, false)
                .map_err(|status| ExpError { status })?
        };
        if !ok {
            return Err(ExpError { status: 1 });
        }
        return param_value(name, Some(sub));
    }
    assign_whole(name, word, mode, pf)?;
    if pf.ifs_split && pf.assign_array == 0 {
        // `${=x=a "b c"}` splits the word, not the stored string.
        return word_val(word, mode);
    }
    if pf.no_deref {
        if let Some(v) = with_shell(|s| {
            s.param(name)
                .filter(|p| p.attrs & crate::params::NAMEREF != 0)
                .map(|p| {
                    if p.attrs & crate::params::UNSET != 0 {
                        Val::Unset
                    } else {
                        Val::Str(p.text())
                    }
                })
        }) {
            return Ok(v);
        }
    }
    param_value(name, None)
}

fn assign_whole(name: &str, word: &[char], mode: Mode, pf: &PFlags) -> ExpResult<Val> {
    if pf.no_deref
        && pf.assign_array == 0
        && with_shell(|s| {
            s.param(name)
                .is_some_and(|p| p.attrs & crate::params::NAMEREF != 0)
        })
    {
        let v = word_string(word, mode)?;
        if !crate::shell::set_ref_target(name, &v) {
            return Err(ExpError { status: 1 });
        }
        return Ok(Val::Str(v));
    }
    if pf.assign_array > 0 {
        let items = match word_val(word, mode)? {
            Val::Tagged(fields) => {
                let mut out = Vec::new();
                for f in fields {
                    // With `=`, the word's unquoted text splits too.
                    let f: Vec<(char, u8)> = if pf.ifs_split {
                        f.into_iter()
                            .map(|(c, t)| (c, if t & Q != 0 { t } else { t | S }))
                            .collect()
                    } else {
                        f
                    };
                    for piece in split_field(&f, &ifs()) {
                        out.push(tagged_text(&piece));
                    }
                }
                out
            }
            other => val_items(&other),
        };
        if pf.assign_array > 1 {
            crate::shell::assoc_declare(name);
        }
        if !crate::shell::array_set(name, items.clone()) {
            return Err(ExpError { status: 1 });
        }
        ASSIGNED.with(|a| *a.borrow_mut() = Some(items.clone()));
        return Ok(Val::List(items, false));
    }
    let v = word_string(word, mode)?;
    if !var_set(name, &v, false) {
        return Err(ExpError { status: 1 });
    }
    Ok(Val::Str(v))
}

// The POSIX/bash operators after the name: - = ? + # % / : ^ ,
fn apply_operator(
    val: Val,
    name: &str,
    sub: Option<&str>,
    rest: &[char],
    mode: Mode,
    text: &str,
    pf: &PFlags,
) -> ExpResult<Val> {
    let bad = || fail::<Val>("bad substitution");
    let is_set = !matches!(val, Val::Unset);
    let is_null = match &val {
        Val::Unset => true,
        Val::Str(s) => s.is_empty(),
        Val::List(items, _) | Val::Rc(items) => items.iter().all(|s| s.is_empty()),
        Val::Tagged(f) => f.iter().all(|x| tagged_text(x).is_empty()),
    };
    // ${name::=word}: assign unconditionally.
    if rest.starts_with(&[':', ':', '=']) {
        return assign_word(name, sub, &rest[3..], mode, pf, text);
    }
    // ${a:|b} removes b's elements from a; ${a:*b} keeps only those.
    if rest.len() > 1 && rest[0] == ':' && matches!(rest[1], '|' | '*') {
        let other: String = rest[2..].iter().collect();
        let others = val_items(&lookup(&other));
        let keep = rest[1] == '*';
        let was_list = is_list(&val);
        let items: Vec<String> = val_items(&plain(val))
            .into_iter()
            .filter(|x| others.contains(x) == keep)
            .collect();
        return Ok(if was_list {
            Val::List(items, false)
        } else {
            items
                .into_iter()
                .next()
                .map(Val::Str)
                .unwrap_or(Val::Str(String::new()))
        });
    }
    let (colon, op, word) =
        if rest[0] == ':' && rest.len() > 1 && matches!(rest[1], '-' | '=' | '?' | '+' | '/') {
            (true, rest[1], &rest[2..])
        } else {
            (false, rest[0], &rest[1..])
        };
    let missing = if colon { !is_set || is_null } else { !is_set };
    match op {
        '-' => {
            if missing {
                word_val(word, mode)
            } else {
                Ok(val)
            }
        }
        '+' => {
            if missing {
                Ok(Val::Str(String::new()))
            } else {
                word_val(word, mode)
            }
        }
        '=' => {
            if !missing {
                return Ok(val);
            }
            assign_word(name, sub, word, mode, pf, text)
        }
        '?' => {
            if !missing {
                return Ok(val);
            }
            let msg = word_string(word, mode)?;
            let msg = if msg.is_empty() {
                "parameter null or not set".to_string()
            } else {
                msg
            };
            fail(&format!("{}: {}", name, msg))
        }
        '#' | '%' | '/' => {
            check_set(name, &val)?;
            let mut fl = crate::pattern::SubFlags {
                substr: pf.substr,
                matched: pf.matching,
                rest: pf.rest_part,
                bind: pf.bind,
                eind: pf.eind,
                len: pf.mlen,
                ..Default::default()
            };
            let mut w = word;
            let mut repl = None;
            if op == '/' {
                fl.long = !pf.substr;
                fl.substr = false;
                if w.first() == Some(&'/') {
                    fl.global = true;
                    w = &w[1..];
                }
                if w.first() == Some(&'#') {
                    fl.start = true;
                    w = &w[1..];
                }
                if w.first() == Some(&'%') {
                    fl.end = true;
                    w = &w[1..];
                }
                if !(fl.start || fl.end) {
                    fl.substr = true;
                }
                fl.all = colon;
                let (pat_part, repl_part) = match find_unquoted(w, '/') {
                    Some(k) => (&w[..k], &w[k + 1..]),
                    None => (w, &w[w.len()..]),
                };
                w = pat_part;
                repl = Some(repl_part);
            } else {
                fl.end = op == '%';
                if w.first() == Some(&op) {
                    fl.long = true;
                    w = &w[1..];
                }
            }
            let n = match &pf.nth {
                Some(e) => eval_arith_str(e)?.max(1) as usize,
                None => 1,
            };
            let pat = crate::pattern::Pattern::new(&pattern_of(w)?);
            // Expanded per match, after $MATCH and $match are set.
            let err = std::cell::Cell::new(None);
            let expand_repl = |r: &[char]| match word_string(r, mode) {
                Ok(v) => v,
                Err(e) => {
                    err.set(Some(e));
                    String::new()
                }
            };
            let f = repl.map(|r| move || expand_repl(r));
            let f = f.as_ref().map(|f| f as &dyn Fn() -> String);
            let out = map_items(val, |s| pat.getmatch(s, fl, n, f));
            match err.take() {
                Some(e) => Err(e),
                None => Ok(out),
            }
        }
        ':' => {
            check_set(name, &val)?;
            let (off_part, len_part) = match find_unquoted(word, ':') {
                Some(k) => (&word[..k], Some(&word[k + 1..])),
                None => (word, None),
            };
            let off = eval_arith_str(&word_string(off_part, Mode::Unquoted)?)?;
            let len = match len_part {
                Some([]) => return fail("unrecognized modifier"),
                Some(l) if l.first() == Some(&'$') && l.contains(&'"') => {
                    return fail(&format!("unrecognized modifier `{}'", l[0]));
                }
                Some(l) => Some(eval_arith_str(&word_string(l, Mode::Unquoted)?)?),
                None => None,
            };
            // A negative length that ends before the start is an error.
            let check = |n: usize| -> ExpResult<()> {
                if let Some(l) = len.filter(|&l| l < 0) {
                    let n = n as i64;
                    let start = if off < 0 {
                        (n + off).max(0)
                    } else {
                        off.min(n)
                    };
                    if n + l < start {
                        return fail(&format!("substring expression: {} < {}", n + l, start));
                    }
                }
                Ok(())
            };
            Ok(match plain(val) {
                Val::List(items, star) => {
                    let items = if name == "@" || name == "*" {
                        let mut all = vec![var_get("0")];
                        all.extend(items);
                        all
                    } else {
                        items
                    };
                    check(items.len())?;
                    Val::List(slice(&items, off, len), star)
                }
                other => {
                    let chars: Vec<String> = val_items(&other)
                        .concat()
                        .chars()
                        .map(|c| c.to_string())
                        .collect();
                    check(chars.len())?;
                    Val::Str(slice(&chars, off, len).concat())
                }
            })
        }
        '^' | ',' => {
            check_set(name, &val)?;
            let all = word.first() == Some(&op);
            let upper = op == '^';
            Ok(map_items(val, |s| {
                if all {
                    if upper {
                        s.to_uppercase()
                    } else {
                        s.to_lowercase()
                    }
                } else {
                    let mut cs = s.chars();
                    match cs.next() {
                        Some(c) => {
                            let first: String = if upper {
                                c.to_uppercase().collect()
                            } else {
                                c.to_lowercase().collect()
                            };
                            first + cs.as_str()
                        }
                        None => String::new(),
                    }
                }
            }))
        }
        _ => bad(),
    }
}

// The `${=...}`/`${==...}` and `${~...}`/`${~~...}` prefixes: whether
// to force word splitting and pattern characters on or off.
fn prefix_toggles(body: &[char]) -> (Option<bool>, Option<bool>) {
    let mut i = 0;
    let mut glob = None;
    if body.first() == Some(&'(') {
        match parse_flags(body) {
            Some((f, end)) => {
                i = end;
                if f.join_pattern {
                    glob = Some(true);
                }
            }
            None => return (None, None),
        }
    }
    let mut split = None;
    while let Some(&c) = body.get(i) {
        let doubled = body.get(i + 1) == Some(&c);
        match c {
            '=' => split = Some(!doubled),
            '~' => glob = Some(!doubled),
            '#' | '+' | '^' | '!' => {}
            _ => break,
        }
        i += if doubled { 2 } else { 1 };
    }
    (split, glob)
}

fn expand_braced(body: &[char], mode: Mode, ctx: &mut Ctx) -> ExpResult<()> {
    let val = braced(body, mode)?;
    let quoted = mode != Mode::Unquoted;
    let (split, glob) = prefix_toggles(body);
    if quoted || (split.is_none() && glob.is_none()) {
        emit_val(ctx, &val, quoted);
        return Ok(());
    }
    let saved = ctx.param_tag;
    if split == Some(false) {
        ctx.param_tag &= !S;
    }
    match glob {
        Some(true) => ctx.param_tag &= !NG,
        Some(false) => ctx.param_tag |= NG,
        None => {}
    }
    // ${==x:-word}: nor do the word's own expansions split.
    let val = match val {
        Val::Tagged(fields) if split == Some(false) => Val::Tagged(
            fields
                .into_iter()
                .map(|f| f.into_iter().map(|(c, t)| (c, t & !S)).collect())
                .collect(),
        ),
        v => v,
    };
    let val = match val {
        // ${~x-"word"}: even quoted parts of the word become patterns.
        Val::Tagged(fields) if glob == Some(true) => Val::Tagged(
            fields
                .into_iter()
                .map(|f| {
                    f.into_iter()
                        .map(|(c, t)| (c, if t == Q { 0 } else { t & !NG }))
                        .collect()
                })
                .collect(),
        ),
        v => v,
    };
    emit_val(ctx, &val, quoted);
    ctx.param_tag = saved;
    Ok(())
}

fn slice<T: Clone>(items: &[T], off: i64, len: Option<i64>) -> Vec<T> {
    let n = items.len() as i64;
    let start = if off < 0 {
        n.saturating_add(off).max(0)
    } else {
        off.min(n)
    };
    let end = match len {
        Some(l) if l < 0 => n.saturating_add(l).max(start),
        Some(l) => start.saturating_add(l).min(n),
        None => n,
    };
    items[start as usize..end as usize].to_vec()
}

// Index of the first `target` not inside quotes or escaped.
fn find_unquoted(chars: &[char], target: char) -> Option<usize> {
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '\'' => i = scan_squote(chars, i)?,
            '"' => i = scan_dquote(chars, i)?,
            '$' if chars.get(i + 1) == Some(&'{') => i = scan_brace(chars, i + 1, false)?,
            '$' if chars.get(i + 1) == Some(&'(') => i = scan_paren(chars, i + 1)?,
            c if c == target => return Some(i),
            _ => i += 1,
        }
    }
    None
}

// Expands a word to a single string (no splitting, no globbing).
fn word_string(word: &[char], mode: Mode) -> ExpResult<String> {
    let mut ctx = Ctx::new(false, false);
    let m = if mode == Mode::Heredoc {
        Mode::Dq
    } else {
        mode
    };
    walk(word, m, &mut ctx)?;
    Ok(ctx.joined())
}

// Expands a word into a pattern for `crate::pattern`: quoted characters
// are backslash-escaped so they match literally.
fn pattern_of(word: &[char]) -> ExpResult<String> {
    let mut ctx = Ctx::new(false, false);
    walk(word, Mode::Unquoted, &mut ctx)?;
    Ok(pattern_text(&ctx))
}

thread_local! {
    static IN_DQ_SUB: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// An associative array key as zsh reads a subscript: quotes are
/// literal, a backslash is dropped only before `\`, `[` or `]`, and
/// parameter, command and arithmetic substitutions apply.
pub fn assoc_key(sub: &str) -> ExpResult<String> {
    let mut pre = String::new();
    let chars: Vec<char> = sub.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '\\' {
            match next {
                Some('[' | ']') => {
                    pre.push(next.unwrap());
                    i += 2;
                    continue;
                }
                // Kept for the next step, where `\\` is one backslash.
                Some('\\') => {
                    pre.push_str("\\\\");
                    i += 2;
                    continue;
                }
                Some('$' | '`') => {
                    pre.push_str("\\\\");
                    i += 1;
                    continue;
                }
                _ => {}
            }
        }
        // A nested expansion (`$A[(i)\]]`, `${...}`) keeps its own text.
        if c == '$' {
            let end = match next {
                Some('{') => crate::lexer::scan_brace(&chars, i + 1, false),
                Some('(') => crate::lexer::scan_paren(&chars, i + 1),
                Some(n) if crate::lexer::is_name_start(n) => {
                    let mut e = i + 1;
                    while chars.get(e).is_some_and(|&x| crate::lexer::is_name_char(x)) {
                        e += 1;
                    }
                    while chars.get(e) == Some(&'[') {
                        e = scan_bracket_end(&chars, e) + 1;
                    }
                    Some(e)
                }
                _ => None,
            };
            if let Some(end) = end.filter(|&e| e <= chars.len()) {
                pre.extend(&chars[i..end]);
                i = end;
                continue;
            }
        }
        pre.push(c);
        i += 1;
    }
    expand_heredoc(&pre)
}

// Index of the `]` closing the `[` at `open`.
fn scan_bracket_end(chars: &[char], open: usize) -> usize {
    let mut depth = 0;
    let mut k = open;
    while k < chars.len() {
        match chars[k] {
            '\\' => k += 1,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return k;
                }
            }
            _ => {}
        }
        k += 1;
    }
    chars.len().saturating_sub(1)
}

fn pattern_text(ctx: &Ctx) -> String {
    let mut out = String::new();
    for &(c, f) in ctx.fields.iter().flatten() {
        if f & MARK != 0 {
            continue;
        }
        if f & (Q | NG) != 0 && "*?[]()|<>^#~\\!-".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// `keep_unknown`: an unknown escape keeps its backslash (echo); `$'...'` drops it, as zsh does.
fn ansi_c(chars: &[char], keep_unknown: bool) -> Result<String, String> {
    ansi_c_opts(chars, keep_unknown, true, true, false)
}

fn ansi_c_opts(
    chars: &[char],
    keep_unknown: bool,
    octal_no_zero: bool,
    emacs: bool,
    caret: bool,
) -> Result<String, String> {
    let mut out = String::new();
    let mut i = 0;
    let n = chars.len();
    let take_digits = |i: &mut usize, radix: u32, max: usize| -> Option<u32> {
        let start = *i;
        while *i < n && *i - start < max && chars[*i].is_digit(radix) {
            *i += 1;
        }
        if *i == start {
            return None;
        }
        let s: String = chars[start..*i].iter().collect();
        u32::from_str_radix(&s, radix).ok()
    };
    // `\M-x` sets the top bit of the next character, `\C-x` makes it a
    // control character; the next character may be an escape itself.
    let (mut meta, mut ctrl) = (false, false);
    let mut mark = 0;
    let modify = |out: &mut String, meta: &mut bool, ctrl: &mut bool, mark: usize| {
        if (*meta || *ctrl) && out.chars().count() > mark {
            let ch = out.pop().unwrap_or_default();
            let mut v = crate::shell::decode_raw_byte(ch)
                .map(u32::from)
                .unwrap_or(ch as u32);
            if *ctrl {
                v = if v == u32::from(b'?') {
                    0x7f
                } else {
                    (v as u8).to_ascii_uppercase() as u32 & 0x1f
                };
            }
            if *meta {
                v |= 0x80;
            }
            out.push(crate::shell::raw_byte_char((v & 0xff) as u8));
            *meta = false;
            *ctrl = false;
        }
    };
    while i < n {
        modify(&mut out, &mut meta, &mut ctrl, mark);
        mark = out.chars().count();
        if caret && !ctrl && chars[i] == '^' && i + 1 < n {
            ctrl = true;
            i += 1;
            continue;
        }
        if chars[i] != '\\' || i + 1 >= n {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        let c = chars[i];
        i += 1;
        match c {
            'M' | 'C' if emacs => {
                if chars.get(i) == Some(&'-') {
                    i += 1;
                }
                if c == 'M' {
                    meta = true;
                } else {
                    ctrl = true;
                }
                continue;
            }
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'e' => out.push('\x1b'),
            'E' if emacs => out.push('\x1b'),
            'f' => out.push('\x0c'),
            'v' => out.push('\x0b'),
            '\\' | '\'' | '"' | '?' => out.push(c),
            '0'..='7' if octal_no_zero || c == '0' => {
                if octal_no_zero {
                    i -= 1;
                }
                let v = take_digits(&mut i, 8, 3).unwrap_or(0);
                out.push(crate::shell::raw_byte_char((v & 0xff) as u8));
            }
            'x' => match take_digits(&mut i, 16, 2) {
                Some(v) => out.push(crate::shell::raw_byte_char((v & 0xff) as u8)),
                None => out.push_str("\\x"),
            },
            'u' | 'U' => {
                let max = if c == 'u' { 4 } else { 8 };
                match take_digits(&mut i, 16, max) {
                    Some(v) => {
                        if v > 0x7f && crate::shell::is_c_locale() {
                            return Err("character not in range".to_string());
                        }
                        out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                    }
                    None => {
                        out.push('\\');
                        out.push(c);
                    }
                }
            }
            'c' if i < n => {
                out.push(char::from_u32((chars[i] as u32) & 0x1f).unwrap_or('?'));
                i += 1;
            }
            _ => {
                if keep_unknown {
                    out.push('\\');
                }
                out.push(c);
            }
        }
    }
    modify(&mut out, &mut meta, &mut ctrl, mark);
    Ok(crate::shell::normalize_zstr(&out))
}

// `$'...'`: with POSIX_STRINGS a null character ends the string.
fn dollar_squote_text(chars: &[char]) -> ExpResult<String> {
    let mut s = ansi_c(chars, false).or_else(|msg| fail(&msg))?;
    if crate::options::is_set("posixstrings") {
        if let Some(k) = s.find('\0') {
            s.truncate(k);
        }
    }
    Ok(s)
}

/// Decodes backslash escapes the way `$'...'` does (used by `echo -e`).
pub fn decode_escapes(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    match ansi_c(&chars, true) {
        Ok(v) => v,
        Err(msg) => {
            crate::shell::shell_err(&msg);
            String::new()
        }
    }
}

// ---- brace expansion ------------------------------------------------------

// zsh brace-expands after parameter expansion, so `IF{${^arr},x}` works;
// only braces and commas from unquoted literal text count.
fn brace_expand(field: &[(char, u8)]) -> Vec<Vec<(char, u8)>> {
    let active = |i: usize| field[i].1 & B != 0;
    for i in 0..field.len() {
        if field[i].0 != '{' || !active(i) {
            continue;
        }
        let Some((close, commas)) = brace_close(field, i) else {
            continue;
        };
        let mut alts: Vec<Vec<(char, u8)>> = Vec::new();
        if !commas.is_empty() {
            let mut last = i + 1;
            for &c in commas.iter().chain(std::iter::once(&close)) {
                alts.push(field[last..c].to_vec());
                last = c + 1;
            }
        } else {
            let inner: Vec<char> = field[i + 1..close]
                .iter()
                .filter(|&&(_, t)| t & MARK == 0)
                .map(|&(c, _)| c)
                .collect();
            if let Some((seq, chars)) = sequence(&inner) {
                let tag = if chars { Q } else { field[i].1 };
                alts = seq
                    .into_iter()
                    .map(|w| w.chars().map(|c| (c, tag)).collect())
                    .collect();
            } else if !inner.is_empty() && crate::options::is_set("braceccl") {
                // BRACE_CCL: `{a-dz}` gives each character, in order.
                let mut set = std::collections::BTreeSet::new();
                let mut k = 0;
                while k < inner.len() {
                    if k + 2 < inner.len() && inner[k + 1] == '-' {
                        let (lo, hi) = (inner[k] as u32, inner[k + 2] as u32);
                        for c in lo.min(hi)..=lo.max(hi) {
                            if let Some(c) = char::from_u32(c) {
                                set.insert(c);
                            }
                        }
                        k += 3;
                    } else {
                        set.insert(inner[k]);
                        k += 1;
                    }
                }
                alts = set.into_iter().map(|c| vec![(c, field[i].1)]).collect();
            }
        }
        if alts.is_empty() {
            continue;
        }
        let mut out = Vec::new();
        for alt in alts {
            let mut w = field[..i].to_vec();
            w.extend(alt);
            w.extend_from_slice(&field[close + 1..]);
            out.extend(brace_expand(&w));
        }
        return out;
    }
    vec![field.to_vec()]
}

// Finds the `}` matching the `{` at `open` and the top-level commas.
fn brace_close(field: &[(char, u8)], open: usize) -> Option<(usize, Vec<usize>)> {
    let mut depth = 0;
    let mut commas = Vec::new();
    for (i, &(c, t)) in field.iter().enumerate().skip(open) {
        if t & B == 0 {
            continue;
        }
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((i, commas));
                }
            }
            ',' if depth == 1 => commas.push(i),
            _ => {}
        }
    }
    None
}

// `{a..b}` and `{a..b..step}` over integers or single characters.
// `{a..b}` and `{a..b..step}` as zsh's xpandbraces does them: numbers
// (zero-padded when an operand has a leading zero; a negative step
// reverses the order, a step aligns on the first number) or single
// characters. The bool is true for characters, which stay literal.
fn sequence(inner: &[char]) -> Option<(Vec<String>, bool)> {
    let s: String = inner.iter().collect();
    let parts: Vec<&str> = s.split("..").collect();
    if parts.len() == 2 {
        let (ca, cb): (Vec<char>, Vec<char>) =
            (parts[0].chars().collect(), parts[1].chars().collect());
        if ca.len() == 1 && cb.len() == 1 && !(ca[0].is_ascii_digit() && cb[0].is_ascii_digit()) {
            let to_ord = |c: char| -> u32 {
                crate::shell::decode_raw_byte(c)
                    .map(u32::from)
                    .unwrap_or(c as u32)
            };
            let (mut a, mut b) = (to_ord(ca[0]), to_ord(cb[0]));
            let rev = b < a;
            if rev {
                std::mem::swap(&mut a, &mut b);
            }
            let mb = crate::options::is_set("multibyte");
            let mut out: Vec<String> = (a..=b)
                .filter_map(char::from_u32)
                .map(|c| {
                    if (!mb && (c as u32) <= 0xff) || (c as u32) < 32 || c == '\x7f' {
                        crate::shell::nice_byte_char(c as u8)
                    } else if (0x80..=0xff).contains(&(c as u32)) {
                        crate::shell::raw_byte_char(c as u8).to_string()
                    } else {
                        c.to_string()
                    }
                })
                .collect();
            if rev {
                out.reverse();
            }
            return Some((out, true));
        }
    }
    if parts.len() != 2 && parts.len() != 3 {
        return None;
    }
    let num = |p: &str| -> Option<i64> {
        let body = p.strip_prefix('-').unwrap_or(p);
        if body.is_empty() || !body.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        p.parse().ok()
    };
    let rstart = num(parts[0])?;
    let rend = num(parts[1])?;
    let rincr = match parts.get(2) {
        Some(p) => num(p).filter(|&n| n != 0)?,
        None => 1,
    };
    let zero = |p: &str| p.strip_prefix('-').unwrap_or(p).starts_with('0');
    let minw = if zero(parts[0]) {
        parts[0].len()
    } else if zero(parts[1]) {
        parts[1].len()
    } else if parts.get(2).is_some_and(|p| zero(p)) {
        parts[2].len()
    } else {
        0
    };
    // Ends near the limits of i64 overflow differences and steps.
    let (mut rstart, mut rend) = (i128::from(rstart), i128::from(rend));
    let mut rincr = i128::from(rincr);
    let mut rev = false;
    if rincr < 0 {
        rincr = -rincr;
        rev = !rev;
    }
    if rstart > rend {
        std::mem::swap(&mut rstart, &mut rend);
        rev = !rev;
    } else if rincr > 1 {
        rend -= (rend - rstart) % rincr;
    }
    if (rend - rstart) / rincr > 100_000 {
        return None;
    }
    let mut out = Vec::new();
    let mut x = rend;
    while x >= rstart {
        out.push(format!("{:0width$}", x, width = minw));
        x -= rincr;
    }
    // Generated from the top: ascending unless reversed.
    if !rev {
        out.reverse();
    }
    Some((out, false))
}

// ---- field splitting and pathname expansion -------------------------------

fn split_field(field: &[(char, u8)], ifs: &str) -> Vec<Vec<(char, u8)>> {
    let is_delim = |&(c, f): &(char, u8)| f & S != 0 && ifs.contains(c);
    let is_ws = |c: char| matches!(c, ' ' | '\t' | '\n');
    let n = field.len();
    let mut out = Vec::new();
    let mut cur: Vec<(char, u8)> = Vec::new();
    let mut i = 0;
    while i < n && is_delim(&field[i]) && is_ws(field[i].0) {
        i += 1;
    }
    while i < n {
        if !is_delim(&field[i]) {
            cur.push(field[i]);
            i += 1;
            continue;
        }
        if is_ws(field[i].0) {
            let mut j = i;
            while j < n && is_delim(&field[j]) && is_ws(field[j].0) {
                j += 1;
            }
            if j < n && is_delim(&field[j]) {
                // Whitespace before a non-whitespace delimiter.
                i = j;
                continue;
            }
            if j == n {
                break;
            }
            out.push(std::mem::take(&mut cur));
            i = j;
        } else {
            out.push(std::mem::take(&mut cur));
            i += 1;
            while i < n && is_delim(&field[i]) && is_ws(field[i].0) {
                i += 1;
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

enum Globbed {
    // Not a pattern, or globbing is off.
    Word(String),
    // A malformed pattern (BAD_PATTERN).
    Bad(String),
    // Qualifier parse failure.
    Err(String),
    Matches(Vec<String>),
    // A pattern that matched nothing, with its literal text.
    NoMatch(String),
}

// An unescaped `[` with text but no `]` after it (a lone `[`, the test
// command, is fine).
fn unclosed_bracket(pat: &str) -> bool {
    let chars: Vec<char> = pat.chars().collect();
    let mut k = 0;
    while k < chars.len() {
        match chars[k] {
            '\\' => k += 1,
            '[' => return chars.len() > 1 && !chars[k + 1..].contains(&']'),
            _ => {}
        }
        k += 1;
    }
    false
}

// EQUALS on an expanded field starting with an unquoted `=cmd`: the
// command's path. None when the field is not one.
fn equals_field(field: &[(char, u8)]) -> ExpResult<Option<Vec<(char, u8)>>> {
    let unquoted = |k: usize| field.get(k).is_some_and(|&(_, f)| f & (Q | NG | MARK) == 0);
    let starts = field.first().is_some_and(|&(c, _)| c == '=')
        && unquoted(0)
        && field
            .get(1)
            .is_some_and(|&(c, _)| c.is_alphanumeric() || "_-.".contains(c))
        && crate::options::is_set("equals");
    if !starts {
        return Ok(None);
    }
    let end = (1..field.len())
        .find(|&k| field[k].0 == '/')
        .unwrap_or(field.len());
    let name: String = field[1..end].iter().map(|&(c, _)| c).collect();
    match crate::shell::find_command(&name) {
        Some(p) => Ok(Some(
            p.chars()
                .map(|c| (c, Q))
                .chain(field[end..].iter().copied())
                .collect(),
        )),
        None => fail(&format!("{} not found", name)),
    }
}

fn glob_piece(piece: &[(char, u8)], do_glob: bool) -> Globbed {
    // GLOB_SUBST: a substituted `~name` at the start is expanded too.
    let expanded;
    let piece = match piece.first() {
        Some(&('~', f))
            if do_glob
                && f & (Q | NG | MARK) == 0
                && !crate::options::is_set("shfileexpansion") =>
        {
            let chars: Vec<char> = piece.iter().map(|&(c, _)| c).collect();
            match tilde(&chars, 0, false) {
                Ok(Some((dir, end))) => {
                    expanded = dir
                        .chars()
                        .map(|c| (c, Q))
                        .chain(piece[end..].iter().copied())
                        .collect::<Vec<_>>();
                    &expanded[..]
                }
                Ok(None) => piece,
                Err(msg) => return Globbed::Err(msg),
            }
        }
        _ => piece,
    };
    let literal: String = piece
        .iter()
        .filter(|(_, f)| f & MARK == 0)
        .map(|(c, _)| *c)
        .collect();
    if !do_glob {
        return Globbed::Word(literal);
    }
    // Quoted characters are escaped so they match literally.
    let items: Vec<(char, bool)> = piece
        .iter()
        .filter(|&&(_, f)| f & MARK == 0)
        .map(|&(c, f)| (c, f & (Q | NG) != 0))
        .collect();
    let back_slash_raw: Vec<bool> = piece
        .iter()
        .filter(|&&(_, f)| f & MARK == 0)
        .map(|&(c, f)| c == '\\' && f & (Q | NG) == 0 && f & B != 0)
        .collect();
    let (split, quals) = match crate::globber::split_qualifiers_items(&items) {
        Ok(res) => res,
        Err(msg) => return Globbed::Err(msg),
    };
    let mut pat = String::new();
    for (idx, &(c, quoted)) in items[..split].iter().enumerate() {
        // A backslash from a GLOB_SUBST value quotes the next character,
        // as in zsh; one in the literal text is itself.
        if (quoted && "*?[]()|<>^#~\\!-".contains(c)) || back_slash_raw[idx] {
            pat.push('\\');
        }
        pat.push(c);
    }
    // BAD_PATTERN: an unclosed `[` is an error rather than text.
    if crate::options::is_set("badpattern") && unclosed_bracket(&pat) {
        return Globbed::Bad(literal);
    }
    if quals.is_none() && !crate::pattern::has_meta(&pat) {
        return Globbed::Word(literal);
    }
    let mut quals = quals.unwrap_or_default();
    let explicit_null = quals.null;
    if crate::options::is_set("globdots") {
        quals.dots = true;
    }
    let mut matches = crate::globber::glob(&pat, Some(&quals));
    if matches.is_empty() {
        return if explicit_null {
            Globbed::Matches(Vec::new())
        } else {
            Globbed::NoMatch(literal)
        };
    }
    if crate::options::is_set("markdirs") {
        for m in matches.iter_mut() {
            if !m.ends_with('/') && std::path::Path::new(m).is_dir() {
                m.push('/');
            }
        }
    }
    Globbed::Matches(matches)
}

// Globs every word of a command and applies NULL_GLOB, CSH_NULL_GLOB and
// NOMATCH to patterns that matched nothing.
fn finish_all(ctxs: Vec<Ctx>, do_glob: bool) -> ExpResult<Vec<String>> {
    let ifs = ifs();
    let (null, csh_null, nomatch) = with_shell(|s| {
        let on = |o| crate::options::is_set_in(s, o);
        (on("nullglob"), on("cshnullglob"), on("nomatch"))
    });
    let mut out = Vec::new();
    let (mut any_match, mut failed) = (false, None::<String>);
    for ctx in ctxs.iter().filter(|c| !c.rc_dead) {
        for field in &ctx.fields {
            for mut piece in split_field(field, &ifs) {
                // GLOB_SUBST: a substituted `=cmd` is expanded too.
                if do_glob && !crate::options::is_set("shfileexpansion") {
                    if let Some(e) = equals_field(&piece)? {
                        piece = e;
                    }
                }
                match glob_piece(&piece, do_glob) {
                    Globbed::Word(w) => out.push(w),
                    Globbed::Bad(w) => return fail(&format!("bad pattern: {}", w)),
                    Globbed::Err(msg) => return fail(&msg),
                    Globbed::Matches(m) => {
                        any_match = true;
                        out.extend(m);
                    }
                    Globbed::NoMatch(lit) => {
                        if null {
                        } else if csh_null {
                            failed.get_or_insert(lit);
                        } else if nomatch {
                            return fail(&format!("no matches found: {}", lit));
                        } else {
                            out.push(lit);
                        }
                    }
                }
            }
        }
    }
    if csh_null && !any_match && failed.is_some() {
        return fail("no match");
    }
    Ok(out)
}

// ---- public entry points --------------------------------------------------

/// Fully expands command words into fields.
pub fn expand_words(words: &[String]) -> ExpResult<Vec<String>> {
    let do_glob = !opt_set(OPT_NOGLOB);
    let mut ctxs = Vec::new();
    let braces = !crate::options::is_set("ignorebraces");
    let magic = crate::options::is_set("magicequalsubst");
    // KSH_TYPESET: typeset run as a builtin does not split `name=value`.
    let ksh_typeset = crate::options::is_set("kshtypeset")
        && words.first().is_some_and(|w| {
            matches!(
                w.as_str(),
                "typeset" | "declare" | "local" | "export" | "readonly" | "integer" | "float"
            )
        });
    for (k, word) in words.iter().enumerate() {
        let chars: Vec<char> = word.chars().collect();
        // With MAGIC_EQUAL_SUBST, KSH_TYPESET keeps every `x=...`
        // argument unsplit.
        let unsplit = (ksh_typeset && crate::lexer::is_assignment(word))
            || (magic && crate::options::is_set("kshtypeset") && word.contains('='));
        if k > 0 && unsplit {
            let mut c = Ctx::new(false, true);
            c.magic_eq = magic;
            walk(&chars, Mode::Unquoted, &mut c)?;
            let v = c.joined();
            let mut ctx = Ctx::new(true, false);
            ctx.fields = vec![v.chars().map(|c| (c, Q)).collect()];
            ctxs.push(ctx);
            continue;
        }
        let mut ctx = Ctx::new(true, false);
        // MAGIC_EQUAL_SUBST: `~` after the `=` of an argument expands.
        if k > 0 && magic && word.contains('=') {
            ctx.assign = true;
            ctx.magic_eq = true;
        }
        ctx.braces_after = braces && !crate::options::is_set("shfileexpansion");
        walk(&chars, Mode::Unquoted, &mut ctx)?;
        if braces {
            ctx.fields = ctx.fields.iter().flat_map(|f| brace_expand(f)).collect();
            // Filename expansion follows brace expansion: `={ls,}`.
            if ctx.braces_after && word.starts_with('=') && word.contains('{') {
                for f in ctx.fields.iter_mut() {
                    if let Some(e) = equals_field(f)? {
                        *f = e;
                    }
                }
            }
        }
        ctxs.push(ctx);
    }
    finish_all(ctxs, do_glob)
}

/// Validates parameter expansion syntax in `word` without executing
/// substitutions or failing on unset parameters (used under NO_EXEC).
pub fn validate_noexec_word(word: &str) -> ExpResult<()> {
    let chars: Vec<char> = word.chars().collect();
    validate_walk(&chars, Mode::Unquoted)
}

fn validate_walk(chars: &[char], mode: Mode) -> ExpResult<()> {
    let mut i = 0;
    let n = chars.len();
    while i < n {
        match chars[i] {
            '\\' => i += 2,
            '\'' if mode == Mode::Unquoted => {
                i = scan_squote(chars, i).unwrap_or(n);
            }
            '"' if mode == Mode::Unquoted => {
                let Some(end) = scan_dquote(chars, i) else {
                    return Ok(());
                };
                validate_walk(&chars[i + 1..end - 1], Mode::Dq)?;
                i = end;
            }
            '`' => i = scan_bquote(chars, i).unwrap_or(n),
            '$' if i + 1 < n => match chars[i + 1] {
                '{' => {
                    let Some(end) = scan_brace(chars, i + 1, mode == Mode::Dq) else {
                        return fail("bad substitution");
                    };
                    validate_braced(&chars[i + 2..end - 1], mode)?;
                    i = end;
                }
                '(' => {
                    i = scan_arith(chars, i + 1)
                        .or_else(|| scan_paren(chars, i + 1))
                        .unwrap_or(n);
                }
                '\'' if mode == Mode::Unquoted => {
                    i = scan_dollar_squote(chars, i + 1).unwrap_or(n);
                }
                _ => i += 1,
            },
            _ => i += 1,
        }
    }
    Ok(())
}

fn validate_braced(body: &[char], mode: Mode) -> ExpResult<()> {
    if body.is_empty() {
        return fail("bad substitution");
    }
    if matches!(body.first(), Some('|' | ' ' | '\t' | '\n')) {
        return Ok(());
    }
    let mut i = 0;
    if body[0] == '(' {
        let Some((_, end)) = parse_flags_syntax(body) else {
            return fail("bad substitution");
        };
        i = end;
    }
    while let Some(&c) = body.get(i) {
        let next = body.get(i + 1).copied();
        let starts_param = |n: Option<char>| {
            n.is_some_and(|n| {
                n == '_'
                    || n == '.'
                    || n == '$'
                    || n == '('
                    || n.is_alphanumeric()
                    || is_special_param(n)
            })
        };
        match c {
            '#' if starts_param(next) && !(next == Some('#') && i + 2 < body.len()) => {}
            '+' if starts_param(next) => {}
            '^' if starts_param(next) || matches!(next, Some('^' | ':' | '~' | '=')) => {
                if next == Some('^') {
                    i += 1;
                }
            }
            '=' if starts_param(next) || matches!(next, Some('=' | ':' | '~' | '^' | '"')) => {
                if next == Some('=') {
                    i += 1;
                }
            }
            '~' => {}
            '!' if starts_param(next) && i == 0 => {}
            _ => break,
        }
        i += 1;
    }
    if body.get(i) == Some(&'"') {
        let Some(end) = scan_dquote(body, i) else {
            return fail("bad substitution");
        };
        validate_walk(&body[i + 1..end - 1], Mode::Dq)?;
        i = end;
    } else if body.get(i) == Some(&'$') && body.get(i + 1) == Some(&'{') {
        let Some(end) = scan_brace(body, i + 1, false) else {
            return fail("bad substitution");
        };
        validate_braced(&body[i + 2..end - 1], mode)?;
        i = end;
    } else if body.get(i) == Some(&'$') && body.get(i + 1) == Some(&'(') {
        let Some(end) = scan_arith(body, i + 1).or_else(|| scan_paren(body, i + 1)) else {
            return fail("bad substitution");
        };
        i = end;
    } else if let Some((_, _, end)) = parse_param(&body[i..]) {
        i += end;
    } else if matches!(body.get(i), Some(':' | '-' | '+' | '=' | '?')) || i == body.len() {
    } else {
        return fail("bad substitution");
    }
    while body.get(i) == Some(&'[') {
        let end = scan_bracket_end(body, i);
        if body.get(end) != Some(&']') {
            return fail("bad substitution");
        }
        i = end + 1;
    }
    if i < body.len() {
        if !matches!(
            body[i],
            ':' | '-' | '+' | '=' | '?' | '#' | '%' | '/' | '^' | ','
        ) {
            return fail("bad substitution");
        }
        validate_walk(&body[i..], mode)?;
    }
    Ok(())
}

// Lightweight flag parser that ignores runtime arithmetic errors inside
// `(l:expr:)` during NO_EXEC syntax checks.
fn parse_flags_syntax(body: &[char]) -> Option<(PFlags, usize)> {
    if parse_flags(body).is_some() {
        return parse_flags(body);
    }
    let mut i = 1;
    while i < body.len() {
        if body[i] == ')' {
            return Some((PFlags::default(), i + 1));
        }
        if matches!(body[i], 'l' | 'r') {
            i += 1;
            let d = *body.get(i)?;
            let close = closing_delim(d);
            let mut k = 0;
            while k < 3 && body.get(i) == Some(&d) {
                i += 1;
                while i < body.len() && body[i] != close {
                    i += 1;
                }
                if i >= body.len() {
                    return None;
                }
                i += 1;
                k += 1;
            }
            continue;
        }
        i += 1;
    }
    None
}

/// Expands a word to one string without field splitting or globbing
/// (redirection targets, `case` words, `[[ ]]` operands).
pub fn expand_string(word: &str) -> ExpResult<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut ctx = Ctx::new(false, false);
    walk(&chars, Mode::Unquoted, &mut ctx)?;
    Ok(ctx.joined())
}

/// Expands the value of an assignment (`name=value`).
pub fn expand_assign_value(word: &str) -> ExpResult<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut ctx = Ctx::new(false, true);
    walk(&chars, Mode::Unquoted, &mut ctx)?;
    Ok(ctx.joined())
}

/// A pattern for XTRACE: quoted characters that would be special or
/// split shown with a backslash (`'f x'*` traces as `f\ x*`).
pub fn pattern_for_trace(word: &str) -> ExpResult<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut ctx = Ctx::new(false, false);
    walk(&chars, Mode::Unquoted, &mut ctx)?;
    let mut out = String::new();
    for &(c, f) in ctx.fields.iter().flatten() {
        if f & MARK != 0 {
            continue;
        }
        if f & (Q | NG) != 0 && (c.is_whitespace() || "*?[]()|<>^#~\\'\"$`;&".contains(c)) {
            out.push('\\');
        }
        out.push(c);
    }
    Ok(out)
}

/// Expands a word used as a pattern; quoted characters match literally.
pub fn expand_pattern(word: &str) -> ExpResult<String> {
    let chars: Vec<char> = word.chars().collect();
    pattern_of(&chars)
}

/// Expands the body of an unquoted here-document.
pub fn expand_heredoc(body: &str) -> ExpResult<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut ctx = Ctx::new(false, false);
    walk(&chars, Mode::Heredoc, &mut ctx)?;
    Ok(ctx.joined())
}

/// Expands parameters, command substitutions and arithmetic in a prompt.
pub fn expand_prompt_string(s: &str) -> ExpResult<String> {
    expand_heredoc(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{shell_init, var_set};
    use crate::TEST_LOCK;

    // These tests check POSIX expansion, so they run in sh emulation.
    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        with_shell(|s| crate::options::init(s, crate::options::Emulation::Sh));
        lock
    }

    #[test]
    fn zsh_defaults_neither_split_nor_glob_parameters() {
        let _g = setup();
        with_shell(|s| crate::options::init(s, crate::options::Emulation::Zsh));
        var_set("x", "a b *", false);
        assert_eq!(words(&["$x"]), vec!["a b *"]);
        assert_eq!(words(&["$(echo c d)"]), vec!["c", "d"]);
        assert_eq!(words(&["${=x}"]), vec!["a", "b", "*"]);
        assert_eq!(words(&["${unset_v:-a b}"]), vec!["a b"]);
        assert!(expand_words(&["/nonexistent-dir/*".to_string()]).is_err());
        crate::options::set("nullglob", true);
        assert!(words(&["/nonexistent-dir/*"]).is_empty());
    }

    fn words(s: &[&str]) -> Vec<String> {
        expand_words(&s.iter().map(|w| w.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn field_splitting_uses_ifs() {
        let _l = setup();
        var_set("x", " a  b ", false);
        assert_eq!(words(&["$x"]), vec!["a", "b"]);
        assert_eq!(words(&["\"$x\""]), vec![" a  b "]);
        var_set("IFS", ":", false);
        var_set("y", "a::b:", false);
        assert_eq!(words(&["$y"]), vec!["a", "", "b"]);
        var_set("IFS", " \t\n", false);
    }

    #[test]
    fn quoted_empty_and_unset() {
        let _l = setup();
        var_set("e", "", false);
        assert_eq!(words(&["$e"]), Vec::<String>::new());
        assert_eq!(words(&["\"$e\""]), vec![""]);
        assert_eq!(words(&["''"]), vec![""]);
    }

    #[test]
    fn positional_at_and_star() {
        let _l = setup();
        with_shell(|s| s.pos_params = vec!["a b".into(), "".into(), "c".into()]);
        assert_eq!(words(&["\"$@\""]), vec!["a b", "", "c"]);
        assert_eq!(words(&["\"$*\""]), vec!["a b  c"]);
        assert_eq!(words(&["$@"]), vec!["a", "b", "c"]);
        assert_eq!(words(&["x\"$@\"y"]), vec!["xa b", "", "cy"]);
        with_shell(|s| s.pos_params.clear());
        assert_eq!(words(&["\"$@\""]), Vec::<String>::new());
    }

    #[test]
    fn quotes_suppress_glob_and_tilde() {
        let _l = setup();
        var_set("HOME", "/home/u", false);
        assert_eq!(words(&["'*'"]), vec!["*"]);
        assert_eq!(words(&["\"~\""]), vec!["~"]);
        assert_eq!(words(&["~/x"]), vec!["/home/u/x"]);
        assert_eq!(expand_assign_value("a:~/b").unwrap(), "a:/home/u/b");
    }

    #[test]
    fn parameter_operators() {
        let _l = setup();
        var_set("p", "/usr/local/bin", false);
        assert_eq!(expand_string("${p##*/}").unwrap(), "bin");
        assert_eq!(expand_string("${p%/*}").unwrap(), "/usr/local");
        assert_eq!(expand_string("${p#\"/usr\"}").unwrap(), "/local/bin");
        assert_eq!(expand_string("${unset_v-def}").unwrap(), "def");
        assert_eq!(expand_string("${p:+alt}").unwrap(), "alt");
        assert_eq!(expand_string("${#p}").unwrap(), "14");
        assert_eq!(expand_string("${p:1:3}").unwrap(), "usr");
        assert_eq!(words(&["${unset_v:-\"a b\"}"]), vec!["a b"]);
        assert_eq!(words(&["${unset_v:-a b}"]), vec!["a", "b"]);
    }

    #[test]
    fn arithmetic_and_brace() {
        let _l = setup();
        assert_eq!(expand_string("$((1 + 2 * 3))").unwrap(), "7");
        assert_eq!(words(&["{a,b}{1..2}"]), vec!["a1", "a2", "b1", "b2"]);
        assert_eq!(words(&["'{a,b}'"]), vec!["{a,b}"]);
        assert_eq!(words(&["{a}"]), vec!["{a}"]);
    }

    #[test]
    fn non_ascii_words() {
        let _l = setup();
        var_set("v", "héllo wörld", false);
        assert_eq!(words(&["$v"]), vec!["héllo", "wörld"]);
        assert_eq!(expand_string("${v%wörld}").unwrap(), "héllo ");
    }

    #[test]
    fn heredoc_keeps_quotes() {
        let _l = setup();
        var_set("x", "v", false);
        assert_eq!(expand_heredoc("'$x' \"\\$x\"\n").unwrap(), "'v' \"$x\"\n");
    }

    #[test]
    fn brace_sequences_at_the_ends_of_i64_do_not_overflow() {
        let seq = |s: &str| sequence(&s.chars().collect::<Vec<_>>()).map(|(v, _)| v);
        // The step past the minimum wrapped around and never ended.
        assert_eq!(
            seq("-9223372036854775808..-9223372036854775807"),
            Some(vec![
                "-9223372036854775808".to_string(),
                "-9223372036854775807".to_string()
            ])
        );
        // Too long a range is left unexpanded instead of filling memory.
        assert_eq!(seq("-9223372036854775808..0"), None);
        assert_eq!(seq("0..9223372036854775807"), None);
        assert_eq!(
            seq("1..10..-9223372036854775808"),
            Some(vec!["1".to_string()])
        );
    }
}
