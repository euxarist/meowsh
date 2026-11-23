//! Builtin commands. Each takes the full argument vector (the name in
//! `words[0]`) and returns an exit status. Output goes straight to the
//! file descriptors so redirections apply.

use crate::exec::{call_function, last_status, set_flow, strerror};
use crate::jobs::{builtin_bg, builtin_fg, builtin_jobs, builtin_wait, kill_targets};
use crate::lexer::is_valid_name;
use crate::shell::{
    array_set, assoc_declare, builtin_err, find_command, shell_err, var_get, var_get_opt, var_set,
    with_shell,
};
use crate::sys::{err_line, out, read_byte};
use crate::trap::{set_trap, signal_name, signal_number, SIGNALS};
use crate::types::{Flow, FuncDef, Node, VAR_READONLY};
use std::sync::Arc;

const SPECIAL: &[&str] = &[
    "break", ":", "continue", ".", "eval", "exec", "exit", "export", "readonly", "return", "set",
    "shift", "times", "trap", "unset",
];

const REGULAR: &[&str] = &[
    "alias",
    "autoload",
    "bg",
    "cd",
    "command",
    "declare",
    "disable",
    "enable",
    "dirs",
    "echo",
    "false",
    "fg",
    "getopts",
    "hash",
    "rehash",
    "history",
    "jobs",
    "disown",
    "kill",
    "local",
    "private",
    "popd",
    "pushd",
    "pwd",
    "read",
    "setopt",
    "source",
    "test",
    "[",
    "true",
    "type",
    "typeset",
    "ulimit",
    "umask",
    "unalias",
    "unsetopt",
    "wait",
    "zstyle",
    "add-zsh-hook",
    "bindkey",
    "compinit",
    "compdef",
    "compaudit",
    "zmodload",
    "emulate",
    "print",
    "printf",
    "let",
    "limit",
    "unlimit",
    "zselect",
    "zstat",
    "zpty",
    "zle",
    "zcompile",
    "zrecompile",
    "builtin",
    "unfunction",
    "whence",
    "which",
    "where",
    "integer",
    "float",
    "functions",
    "is-at-least",
    "zparseopts",
    "fc",
    "colors",
    "bashcompinit",
    "complete",
    "compctl",
    "compadd",
    "compset",
    "compcall",
    "zformat",
    "zregexparse",
    "strftime",
    "sysread",
    "syswrite",
    "sysopen",
    "sysseek",
    "syserror",
    "zsystem",
    "compdescribe",
    "comparguments",
    "compvalues",
    "compquote",
    "comptags",
    "comptry",
    "compfiles",
    "compgroups",
    "echoti",
    "echotc",
    "log",
    "pcre_compile",
    "pcre_match",
    "pcre_study",
    "ztie",
    "zuntie",
    "zgdbmpath",
    "zgdbmclear",
];

const KEYWORDS: &[&str] = &[
    "!",
    "{",
    "}",
    "case",
    "do",
    "done",
    "elif",
    "else",
    "esac",
    "fi",
    "for",
    "function",
    "if",
    "in",
    "then",
    "until",
    "while",
    "[[",
    "]]",
    "repeat",
    "select",
    "coproc",
    "foreach",
    "end",
    "time",
    "nocorrect",
    "declare",
    "export",
    "float",
    "integer",
    "local",
    "readonly",
    "typeset",
];

pub fn reserved_words() -> Vec<String> {
    KEYWORDS.iter().map(|s| s.to_string()).collect()
}

pub fn builtin_names() -> impl Iterator<Item = &'static str> {
    SPECIAL.iter().chain(REGULAR.iter()).copied()
}

pub fn is_special_builtin(name: &str) -> bool {
    SPECIAL.contains(&name) && !builtin_disabled(name)
}

pub fn is_builtin(name: &str) -> bool {
    if matches!(
        name,
        "stat"
            | "zstat"
            | "ztie"
            | "zuntie"
            | "zgdbmpath"
            | "zgdbmclear"
            | "pcre_compile"
            | "pcre_match"
            | "pcre_study"
    ) {
        return !builtin_disabled(name) && crate::modules::feature_enabled(&format!("b:{}", name));
    }
    (SPECIAL.contains(&name) || REGULAR.contains(&name))
        && !builtin_disabled(name)
        && crate::modules::feature_available(&format!("b:{}", name))
}

// `disable`: (kind, name) pairs, kind `b` builtin, `r` reserved word,
// `a` alias, `f` function. Kept outside the shell state so the parser
// can ask without the shell lock.
static DISABLED: std::sync::RwLock<Vec<(char, String)>> = std::sync::RwLock::new(Vec::new());

pub const PAT_CHARS: &[&str] = &[
    "|", "~", "(", "?", "*", "[", "<", "^", "#", "?(", "*(", "+(", "!(", "\\!(", "@(",
];

pub fn disabled_patterns() -> Vec<String> {
    DISABLED
        .read()
        .map(|d| {
            d.iter()
                .filter(|(k, _)| *k == 'p')
                .map(|(_, n)| n.clone())
                .collect()
        })
        .unwrap_or_default()
}

pub fn set_disabled_patterns(pats: &[String]) {
    if let Ok(mut d) = DISABLED.write() {
        d.retain(|(k, _)| *k != 'p');
        for p in pats {
            d.push(('p', p.clone()));
        }
    }
}

pub fn is_disabled(kind: char, name: &str) -> bool {
    DISABLED
        .read()
        .map(|d| d.iter().any(|(k, n)| *k == kind && n == name))
        .unwrap_or(false)
}

// A disabled declaration builtin still runs through its reserved word.
fn builtin_disabled(name: &str) -> bool {
    !crate::parser::is_decl_word(name) && is_disabled('b', name)
}

/// `enable` / `disable [-afmprs] [name ...]`.
fn builtin_enable(words: &[String]) -> i32 {
    let enable = words[0] == "enable";
    let mut kind = 'b';
    let mut pattern = false;
    let mut i = 1;
    while let Some(a) = words.get(i) {
        let Some(flags) = a.strip_prefix('-').filter(|f| !f.is_empty()) else {
            break;
        };
        if a == "--" {
            i += 1;
            break;
        }
        for c in flags.chars() {
            match c {
                'a' | 'f' | 'p' | 'r' => kind = c,
                'm' => pattern = true,
                's' => {}
                _ => return usage(&words[0], &format!("bad option: -{}", c)),
            }
        }
        i += 1;
    }
    let known: Vec<String> = match kind {
        'b' => SPECIAL
            .iter()
            .chain(REGULAR.iter())
            .map(|s| s.to_string())
            .collect(),
        'p' => PAT_CHARS.iter().map(|s| s.to_string()).collect(),
        'r' => KEYWORDS.iter().map(|s| s.to_string()).collect(),
        'a' => with_shell(|s| {
            s.assoc(crate::types::ALIASES)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default()
        }),
        _ => with_shell(|s| s.functions.keys().cloned().collect()),
    };
    let names = &words[i..];
    if names.is_empty() {
        if kind == 'p' {
            let items: Vec<String> = PAT_CHARS
                .iter()
                .filter(|p| is_disabled('p', p) != enable)
                .map(|p| format!("'{}'", p))
                .collect();
            if !items.is_empty() {
                out(&format!("{}\n", items.join(" ")));
            }
            return 0;
        }
        let mut list: Vec<&String> = known
            .iter()
            .filter(|n| is_disabled(kind, n) != enable)
            .collect();
        list.sort();
        list.dedup();
        out(&list.iter().map(|n| format!("{}\n", n)).collect::<String>());
        return 0;
    }
    let mut status = 0;
    for name in names {
        let targets: Vec<String> = if pattern {
            known
                .iter()
                .filter(|n| crate::pattern::matches(name, n))
                .cloned()
                .collect()
        } else if known.contains(name) {
            vec![name.clone()]
        } else {
            let msg = if kind == 'p' {
                format!("invalid pattern: {}", name)
            } else {
                format!("no such hash table element: {}", name)
            };
            builtin_err(&words[0], &msg);
            status = 1;
            continue;
        };
        if let Ok(mut d) = DISABLED.write() {
            for t in targets {
                d.retain(|(k, n)| !(*k == kind && *n == t));
                if !enable {
                    d.push((kind, t));
                }
            }
        }
    }
    status
}

pub fn run(name: &str, words: &[String]) -> i32 {
    match name {
        ":" | "true" => 0,
        "false" => 1,
        "break" => builtin_break(words, true),
        "continue" => builtin_break(words, false),
        "." | "source" => builtin_source(words),
        "eval" => builtin_eval(words),
        "exit" => builtin_exit(words),
        "export" | "readonly" => decl_from_words(words),
        "return" => builtin_return(words),
        "set" => builtin_set(words),
        "shift" => builtin_shift(words),
        "times" => builtin_times(),
        "trap" => builtin_trap(words),
        "unset" => builtin_unset(words),
        "alias" => builtin_alias(words),
        "unalias" => builtin_unalias(words),
        "autoload" => builtin_autoload(words),
        "bg" => builtin_bg(&words[1..]),
        "fg" => builtin_fg(&words[1..]),
        "jobs" => builtin_jobs(&words[1..]),
        "disown" => crate::jobs::builtin_disown(&words[1..]),
        "wait" => builtin_wait(&words[1..]),
        "cd" => builtin_cd(words),
        "pushd" => builtin_pushd(words),
        "popd" => builtin_popd(words),
        "dirs" => builtin_dirs(words),
        "command" => builtin_command(words),
        "echo" => builtin_echo(words),
        "getopts" => builtin_getopts(words),
        "hash" => builtin_hash(words),
        "rehash" => builtin_hash(&["hash".to_string(), "-r".to_string()]),
        "history" => builtin_history(),
        "kill" => builtin_kill(words),
        "local" | "private" | "typeset" | "declare" | "integer" | "float" => decl_from_words(words),
        "functions" => builtin_functions(words),
        "pwd" => builtin_pwd(words),
        "read" => builtin_read(words),
        "setopt" => builtin_setopt(words, true),
        "unsetopt" => builtin_setopt(words, false),
        "test" | "[" => crate::testexpr::builtin_test(words),
        "type" | "whence" | "which" | "where" => builtin_whence(words),
        "enable" | "disable" => builtin_enable(words),
        "ulimit" => builtin_ulimit(words),
        "umask" => builtin_umask(words),
        "zstyle" => crate::zstyle::builtin_zstyle(words),
        "add-zsh-hook" => builtin_add_zsh_hook(words),
        "print" => crate::print::builtin_print(words),
        "let" => builtin_let(words),
        "limit" => builtin_limit(words),
        "unlimit" => builtin_unlimit(words),
        "zselect" => builtin_zselect(words),
        "zstat" | "stat" => crate::zstat::builtin_zstat(words),
        "zpty" => crate::zpty::builtin_zpty(words),
        "printf" => crate::print::builtin_printf(words),
        // Accepted for zsh startup-file compatibility; no effect. There is
        // no zle, so widgets defined with `zle -N` never run.
        "emulate" => builtin_emulate(words),
        "unfunction" => {
            builtin_unset(&[&["unset".to_string(), "-f".to_string()], &words[1..]].concat())
        }
        "is-at-least" => builtin_is_at_least(words),
        "zparseopts" => builtin_zparseopts(words),
        "fc" => builtin_fc(words),
        "colors" => builtin_colors(),
        "zmodload" => builtin_zmodload(words),
        "bindkey" => crate::zle::keymap::builtin_bindkey(words),
        "zle" => crate::zle::thingy::builtin_zle(words),
        "compinit" | "compaudit" | "bashcompinit" => run_fpath_function(words),
        "compadd" => crate::zle::comp::complete::builtin_compadd(words),
        "compset" => crate::zle::comp::complete::builtin_compset(words),
        "compcall" => crate::zle::comp::complete::builtin_compcall(words),
        "zformat" => crate::zutil::builtin_zformat(words),
        "zregexparse" => crate::zregexparse::builtin_zregexparse(words),
        "strftime" => crate::zdatetime::builtin_strftime(words),
        "sysread" => crate::zsystem::builtin_sysread(words),
        "syswrite" => crate::zsystem::builtin_syswrite(words),
        "sysopen" => crate::zsystem::builtin_sysopen(words),
        "sysseek" => crate::zsystem::builtin_sysseek(words),
        "syserror" => crate::zsystem::builtin_syserror(words),
        "zsystem" => crate::zsystem::builtin_zsystem(words),
        "compdescribe" => crate::zle::comp::computil_args::builtin_compdescribe(words),
        "comparguments" => crate::zle::comp::computil_args::builtin_comparguments(words),
        "compvalues" => crate::zle::comp::computil_rest::builtin_compvalues(words),
        "compquote" => crate::zle::comp::computil_rest::builtin_compquote(words),
        "comptags" => crate::zle::comp::computil_rest::builtin_comptags(words),
        "comptry" => crate::zle::comp::computil_rest::builtin_comptry(words),
        "compfiles" => crate::zle::comp::computil_rest::builtin_compfiles(words),
        "compgroups" => crate::zle::comp::computil_rest::builtin_compgroups(words),
        "compdef" | "zrecompile" | "complete" | "compctl" | "log" => 0,
        "zcompile" => crate::zwc::builtin_zcompile(words),
        "echoti" => crate::zle::refresh::refresh_term::builtin_echoti(words),
        "echotc" => crate::zle::refresh::refresh_term::builtin_echotc(words),
        "pcre_compile" => crate::zpcre::builtin_pcre_compile(words),
        "pcre_match" => crate::zpcre::builtin_pcre_match(words),
        "pcre_study" => crate::zpcre::builtin_pcre_study(words),
        "ztie" => crate::zgdbm::builtin_ztie(words),
        "zuntie" => crate::zgdbm::builtin_zuntie(words),
        "zgdbmpath" => crate::zgdbm::builtin_zgdbmpath(words),
        "zgdbmclear" => crate::zgdbm::builtin_zgdbmclear(words),
        _ => {
            builtin_err(name, "not a builtin");
            1
        }
    }
}

/// Quotes a value so the shell reads it back unchanged.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

// zsh/zselect: `zselect -t centiseconds [-r|-w|-e fd...]` waits for an
// fd to be ready or the timeout; 1 on timeout.
fn builtin_zselect(words: &[String]) -> i32 {
    let mut timeout: i32 = -1;
    let mut fds: Vec<libc::pollfd> = Vec::new();
    let mut mode = libc::POLLIN;
    let mut i = 1;
    while let Some(a) = words.get(i) {
        let arg = |i: &mut usize, rest: &str| -> Option<String> {
            if !rest.is_empty() {
                Some(rest.to_string())
            } else {
                *i += 1;
                words.get(*i).cloned()
            }
        };
        if let Some(rest) = a.strip_prefix("-t") {
            match arg(&mut i, rest).and_then(|t| t.parse::<i32>().ok()) {
                Some(t) => timeout = t.saturating_mul(10),
                None => {
                    builtin_err("zselect", "timeout expected");
                    return 2;
                }
            }
        } else if let Some(rest) = a
            .strip_prefix("-r")
            .or(a.strip_prefix("-w"))
            .or(a.strip_prefix("-e"))
        {
            mode = match &a[..2] {
                "-w" => libc::POLLOUT,
                "-e" => libc::POLLPRI,
                _ => libc::POLLIN,
            };
            if let Some(fd) = arg(&mut i, rest).and_then(|f| f.parse().ok()) {
                fds.push(libc::pollfd {
                    fd,
                    events: mode,
                    revents: 0,
                });
            }
        } else if let Ok(fd) = a.parse::<i32>() {
            fds.push(libc::pollfd {
                fd,
                events: mode,
                revents: 0,
            });
        } else if a == "-a" || a == "-A" {
            i += 1;
        }
        i += 1;
    }
    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
    if n <= 0 {
        1
    } else {
        0
    }
}

// let expr...: each argument is evaluated; true when the last is non-zero.
fn builtin_let(words: &[String]) -> i32 {
    if words.len() < 2 {
        builtin_err("let", "not enough arguments");
        return 1;
    }
    let mut st = 1;
    for w in &words[1..] {
        st = match crate::arith::eval_num(w) {
            Ok(v) => i32::from(!v.truthy()),
            Err(e) => {
                if !e.is_empty() {
                    shell_err(&e);
                }
                return 2;
            }
        };
    }
    st
}

fn usage(name: &str, msg: &str) -> i32 {
    builtin_err(name, msg);
    2
}

// ---- control flow ---------------------------------------------------------

fn builtin_break(words: &[String], is_break: bool) -> i32 {
    let name = &words[0];
    let zsh = !crate::exec::sh_emulation_pub();
    let n = match words.get(1) {
        None => 1,
        Some(a) => match a.parse::<i64>().or_else(|_| crate::arith::eval(a)) {
            Ok(n) if n >= 1 => n as usize,
            // zsh: a fatal error.
            Ok(n) if zsh => {
                builtin_err(name, &format!("argument is not positive: {}", n));
                return crate::exec::abort_command(1);
            }
            _ => {
                builtin_err(name, &format!("{}: loop count out of range", a));
                return 1;
            }
        },
    };
    let (depth, outer) = with_shell(|s| (s.loop_depth, s.subshell_loop_depth));
    if depth == 0 {
        if !zsh {
            err_line(&format!(
                "meowsh: {}: only meaningful in a `for', `while', or `until' loop",
                name
            ));
            return 0;
        }
        builtin_err(
            name,
            if outer > 0 {
                "not in same subshell as first enclosing loop"
            } else {
                "not in for, while, until, select, or repeat loop"
            },
        );
        // zsh: a fatal error.
        return crate::exec::abort_command(1);
    }
    let n = n.min(depth);
    set_flow(if is_break {
        Flow::Break(n)
    } else {
        Flow::Continue(n)
    });
    0
}

// zsh: an error inside eval ends the eval with status 1 instead of
// leaving the shell.
fn builtin_eval(words: &[String]) -> i32 {
    let st = crate::shell::in_eval_context("eval", || {
        crate::exec::run_string_named(&words[1..].join(" "), Some("(eval)"))
    });
    with_shell(|s| {
        if s.error_exit {
            s.error_exit = false;
            match s.flow {
                Flow::Exit(code) => {
                    s.flow = Flow::None;
                    return code;
                }
                Flow::Interrupt => {
                    s.flow = Flow::None;
                    return if st == 0 { 1 } else { st };
                }
                _ => {}
            }
        }
        st
    })
}

// zmodload: modules are built in, so loading always works. `-ap` lists
// the parameters that autoload from a module (SRANDOM from zsh/random,
// until a parameter of that name replaces it).
fn builtin_zmodload(words: &[String]) -> i32 {
    let flags: String = words[1..]
        .iter()
        .take_while(|w| w.starts_with('-'))
        .flat_map(|w| w[1..].chars())
        .collect();
    if flags.contains('u') && words.iter().any(|w| w == "zsh/random") {
        crate::typeset::SRANDOM_LOADED.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    let status = crate::modules::builtin_zmodload(words);
    run_module_setup();
    status
}

// Setup code of modules that just loaded: zsh/complist switches the
// completion listing to its colored version and adds menu selection.
pub fn run_module_setup() {
    if crate::modules::take_newly_loaded()
        .iter()
        .any(|m| m == "zsh/complist")
        && !crate::zle::comp::complist::is_loaded()
    {
        crate::zle::comp::complist::setup();
    }
}

fn builtin_exit(words: &[String]) -> i32 {
    let code = match words.get(1) {
        None => last_status(),
        // zsh evaluates the status as arithmetic: `exit $?+8`.
        Some(a) => match a.trim().parse::<i64>() {
            Ok(n) => (n & 0xff) as i32,
            Err(_) if !crate::exec::sh_emulation_pub() => match crate::arith::eval(a) {
                Ok(n) => (n & 0xff) as i32,
                Err(e) => {
                    shell_err(&e);
                    1
                }
            },
            Err(_) => {
                builtin_err("exit", &format!("{}: numeric argument required", a));
                2
            }
        },
    };
    with_shell(|s| s.error_exit = false);
    // CHECK_JOBS: the first `exit` with jobs only warns.
    if crate::jobs::exit_blocked() {
        return 1;
    }
    set_flow(Flow::Exit(code));
    code
}

fn builtin_return(words: &[String]) -> i32 {
    let (funcs, sources) = with_shell(|s| (s.func_depth, s.source_depth));
    if funcs == 0 && sources == 0 {
        builtin_err(
            "return",
            "can only `return' from a function or sourced script",
        );
        return 1;
    }
    let code = match words.get(1) {
        None => last_status(),
        // zsh evaluates the status as arithmetic: `exit $?+8`.
        // zsh: a function may return any integer, `return -1` included.
        Some(a) => match a.trim().parse::<i64>() {
            Ok(n) if !crate::exec::sh_emulation_pub() && funcs > 0 => n as i32,
            Ok(n) => (n & 0xff) as i32,
            Err(_) if !crate::exec::sh_emulation_pub() => match crate::arith::eval(a) {
                Ok(n) if funcs > 0 => n as i32,
                Ok(n) => (n & 0xff) as i32,
                Err(e) => {
                    shell_err(&e);
                    1
                }
            },
            Err(_) => {
                builtin_err("return", &format!("{}: numeric argument required", a));
                2
            }
        },
    };
    set_flow(Flow::Return(code));
    code
}

fn is_zsh_completion_script(content: &str) -> bool {
    content
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("#compdef") || trimmed.starts_with("#autoload")
        })
        .unwrap_or(false)
}

fn find_sourced(file: &str) -> Option<String> {
    if file.contains('/') {
        return Some(file.to_string());
    }
    for dir in var_get("PATH").split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let p = std::path::Path::new(dir).join(file);
        if p.is_file() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    if std::path::Path::new(file).is_file() {
        return Some(file.to_string());
    }
    None
}

/// Runs a file in the current shell, as `.` does.
pub fn source_file(path: &str, args: Option<&[String]>) -> i32 {
    let compiled = crate::zwc::try_load_compiled(path);
    let content = if compiled.is_some() {
        None
    } else {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                err_line(&format!(
                    "meowsh: {}: {}",
                    path,
                    strerror(e.raw_os_error().unwrap_or(0))
                ));
                return 1;
            }
        };
        let text = crate::shell::bytes_to_zstr(&bytes);
        if is_zsh_completion_script(&text) {
            return 0;
        }
        Some(text)
    };
    let saved = args.map(|a| with_shell(|s| std::mem::replace(&mut s.pos_params, a.to_vec())));
    // zsh: $0 is the sourced file while it runs.
    crate::exec::set_next_frame_file(crate::exec::code_origin());
    let (saved_argv0, saved_source) = with_shell(|s| {
        s.source_depth += 1;
        (
            std::mem::replace(&mut s.argv0, path.to_string()),
            s.source_file.replace(path.to_string()),
        )
    });
    let mut st = crate::shell::in_eval_context("file", || match (&compiled, &content) {
        (Some(nodes), _) => crate::exec::run_nodes_named(nodes, Some(path)),
        (None, Some(text)) => crate::exec::run_string_named(text, Some(path)),
        (None, None) => 0,
    });
    with_shell(|s| {
        s.source_depth -= 1;
        s.argv0 = saved_argv0;
        s.source_file = saved_source;
        if let Flow::Return(n) = s.flow {
            st = n;
            s.flow = Flow::None;
        }
        if let Some(p) = saved {
            s.pos_params = p;
        }
    });
    st
}

fn builtin_source(words: &[String]) -> i32 {
    let Some(file) = words.get(1) else {
        return usage(&words[0], "filename argument required");
    };
    let path = find_sourced(file)
        .filter(|p| std::path::Path::new(p).exists() && !std::path::Path::new(p).is_dir());
    let Some(path) = path else {
        builtin_err(&words[0], &format!("no such file or directory: {}", file));
        // POSIX_BUILTINS: `.` is special, and its failure ends a script.
        if crate::options::is_set("posixbuiltins") && !with_shell(|s| s.interactive) {
            with_shell(|s| s.flow = Flow::Exit(1));
            return 1;
        }
        return 127;
    };
    let args = if words.len() > 2 {
        Some(&words[2..])
    } else {
        None
    };
    source_file(&path, args)
}

// ---- variables ------------------------------------------------------------

fn split_assignment(arg: &str) -> (&str, Option<&str>, bool) {
    match arg.find('=') {
        Some(i) => {
            let (name, append) = match arg[..i].strip_suffix('+') {
                Some(n) => (n, true),
                None => (&arg[..i], false),
            };
            (name, Some(&arg[i + 1..]), append)
        }
        None => (arg, None, false),
    }
}

fn list_vars(prefix: &str, flag: u32) {
    let mut vars: Vec<(String, String)> = with_shell(|s| {
        s.params
            .iter()
            .filter(|(_, p)| (flag == 0 || p.attrs & flag != 0) && !p.is_array() && !p.is_assoc())
            .map(|(k, p)| (k.clone(), p.text()))
            .collect()
    });
    vars.sort();
    let mut text = String::new();
    for (k, v) in vars {
        if prefix.is_empty() {
            text.push_str(&format!("{}={}\n", k, shell_quote(&v)));
        } else {
            text.push_str(&format!("{} {}={}\n", prefix, k, shell_quote(&v)));
        }
    }
    out(&text);
}

fn builtin_unset(words: &[String]) -> i32 {
    // -m: the arguments are patterns matched against parameter names.
    let opts: Vec<&String> = words[1..]
        .iter()
        .take_while(|w| w.starts_with('-') && w.len() > 1)
        .collect();
    if opts.iter().any(|o| o.contains('m')) && !opts.iter().any(|o| o.contains('f')) {
        let pats = &words[1 + opts.len()..];
        let names: Vec<String> = with_shell(|s| {
            let mut v: Vec<String> = s.params.keys().cloned().collect();
            v.sort();
            v
        });
        let mut cmd = vec!["unset".to_string()];
        if opts.iter().any(|o| o.contains('n')) {
            cmd.push("-n".to_string());
        }
        cmd.push("--".to_string());
        for pat in pats {
            cmd.extend(
                names
                    .iter()
                    .filter(|n| crate::pattern::matches(pat, n))
                    .cloned(),
            );
        }
        if cmd.last().is_some_and(|l| l == "--") {
            return 1;
        }
        return builtin_unset(&cmd);
    }
    let mut functions = false;
    let mut no_deref = false;
    let mut status = 0;
    for a in &words[1..] {
        match a.as_str() {
            "-f" => {
                functions = true;
                continue;
            }
            "-v" => {
                functions = false;
                continue;
            }
            // -n: unset a nameref itself, not what it refers to.
            "-n" => {
                no_deref = true;
                continue;
            }
            "--" => continue,
            _ => {}
        }
        if functions {
            with_shell(|s| s.functions.remove(a.as_str()));
            crate::trap::function_removed(a);
            continue;
        }
        // `unset name[sub]`: an element (a key, array elements emptied,
        // or characters of a scalar), through a reference too.
        if let Some((name, sub)) = a.strip_suffix(']').and_then(|x| x.split_once('[')) {
            let _swap = crate::shell::RefSwap::enter(name);
            let target = crate::shell::split_target_pub(&with_shell(|s| s.resolve_name(name)));
            let kind = with_shell(|s| {
                s.param(&target)
                    .map(|p| (p.is_assoc(), p.is_array(), p.attrs & VAR_READONLY != 0))
            });
            match kind {
                None => {}
                Some((_, _, true)) => {
                    crate::shell::shell_err(&crate::shell::readonly_message(&target));
                    status = 1;
                }
                Some((true, _, _)) => {
                    let key = crate::expand::expand_string(sub).unwrap_or_else(|_| sub.to_string());
                    let lvl = with_shell(|s| {
                        let lvl = s.param(&target).map(|p| p.level).unwrap_or(0);
                        if let Some(crate::params::Value::Assoc(m)) =
                            s.param_mut(&target).map(|p| &mut p.value)
                        {
                            m.remove(&key);
                        }
                        lvl
                    });
                    crate::zgdbm::on_del_key(&target, lvl, &key);
                }
                Some((false, is_array, _)) => {
                    let sub = crate::expand::expand_string(sub).unwrap_or_else(|_| sub.to_string());
                    let range = if sub.contains(',') {
                        sub.clone()
                    } else {
                        format!("{0},{0}", sub)
                    };
                    let fill = if is_array {
                        vec![String::new()]
                    } else {
                        Vec::new()
                    };
                    crate::shell::array_splice(&target, &range, fill);
                }
            }
            continue;
        }
        let _swap = if no_deref {
            None
        } else {
            crate::shell::RefSwap::enter(a)
        };
        let target = if no_deref {
            a.clone()
        } else {
            with_shell(|s| s.resolve_name(a))
        };
        let a = &target;
        if a == "ARGC"
            && with_shell(|s| s.param("ARGC").is_none())
            && !crate::exec::sh_emulation_pub()
        {
            crate::shell::shell_err("read-only variable: ARGC");
            status = 1;
            continue;
        }
        if !a.is_empty() && a.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = a.parse::<usize>() {
                if n > 0 {
                    with_shell(|s| {
                        if n <= s.pos_params.len() {
                            s.pos_params.remove(n - 1);
                        }
                    });
                }
                continue;
            }
        }
        // A placeholder reference (referring to nothing) stays.
        if !no_deref
            && with_shell(|s| {
                s.param(a.as_str())
                    .is_some_and(|p| p.attrs & crate::params::NAMEREF != 0)
            })
        {
            continue;
        }
        crate::zparam::ensure(a.as_str());
        let readonly = with_shell(|s| {
            s.param(a.as_str())
                .is_some_and(|p| p.attrs & VAR_READONLY != 0)
        });
        if readonly {
            crate::shell::shell_err(&crate::shell::readonly_message(a));
            status = 1;
            continue;
        }
        // A special tied pair (MANPATH/manpath) stays special and tied:
        // both sides go unset, and assigning either sets both again.
        let special_pair = with_shell(|s| {
            s.param(a.as_str()).and_then(|p| {
                let t = p.tie.clone()?;
                (p.attrs & crate::params::SPECIAL != 0).then_some(t.0)
            })
        });
        if let Some(other) = special_pair {
            with_shell(|s| {
                for n in [a.as_str(), other.as_str()] {
                    if let Some(p) = s.param_mut(n) {
                        p.value = match p.value {
                            crate::params::Value::Array(_) => {
                                crate::params::Value::Array(Vec::new())
                            }
                            _ => crate::params::Value::Scalar(String::new()),
                        };
                        p.attrs = (p.attrs | crate::params::UNSET) & !crate::params::DECLARED;
                    }
                }
            });
            continue;
        }
        // A local stays local after unset (zsh): only its value goes.
        // Unsetting either side of a non-special tied pair unsets both.
        with_shell(|s| {
            let partner = s
                .param(a.as_str())
                .and_then(|p| p.tie.as_ref().map(|(o, _)| o.clone()));
            for name in std::iter::once(a.as_str()).chain(partner.as_deref()) {
                let local = s.param(name).is_some_and(|p| p.level > 0);
                if local {
                    if let Some(p) = s.param_mut(name) {
                        // A private keeps its kind, as its own unset function
                        // leaves it declared.
                        let private = p.attrs & crate::params::PRIVATE != 0;
                        p.value = match &p.value {
                            crate::params::Value::Array(_) if private => {
                                crate::params::Value::Array(Vec::new())
                            }
                            crate::params::Value::Assoc(_) if private => {
                                crate::params::Value::Assoc(Default::default())
                            }
                            _ => crate::params::Value::Scalar(String::new()),
                        };
                        p.tie = None;
                        p.attrs = crate::params::UNSET
                            | (p.attrs & (crate::params::PRIVATE | crate::params::HIDE));
                    }
                } else {
                    s.remove_param(name);
                }
            }
        });
        if a == "CURSOR_FORM" {
            crate::zle::termctl::cursor_form_param_changed();
        }
        if matches!(a.as_str(), "LC_ALL" | "LC_CTYPE" | "LANG") {
            crate::shell::sync_lc_ctype();
        }
    }
    crate::shell::sync_lc_ctype();
    status
}

// `set -o` / `set +o` listings.
fn list_options(as_commands: bool) {
    let mut text = String::new();
    // zsh names an option that is on by default in the current
    // emulation by its `no` form, with the state inverted.
    let emu = with_shell(|s| s.emulation);
    for name in crate::options::names() {
        let on = crate::options::is_set(name);
        let def = crate::options::default_in(name, emu);
        if as_commands {
            let sign = if def != on { '-' } else { '+' };
            text.push_str(&format!(
                "set {}o {}{}\n",
                sign,
                if def { "no" } else { "" },
                name
            ));
        } else if def {
            text.push_str(&format!(
                "no{:<19} {}\n",
                name,
                if on { "off" } else { "on" }
            ));
        } else {
            text.push_str(&format!("{:<21} {}\n", name, if on { "on" } else { "off" }));
        }
    }
    out(&text);
}

fn builtin_set(words: &[String]) -> i32 {
    let args = &words[1..];
    if args.is_empty() {
        list_vars("", 0);
        return 0;
    }
    let mut i = 0;
    let mut array: Option<(String, bool)> = None;
    // As in zsh, the words after `-A name` are all values.
    while i < args.len() && array.is_none() {
        let a = args[i].as_str();
        if a == "--" {
            i += 1;
            break;
        }
        if a == "-" {
            crate::options::set("xtrace", false);
            crate::options::set("verbose", false);
            i += 1;
            break;
        }
        if !((a.starts_with('-') || a.starts_with('+')) && a.len() > 1) {
            break;
        }
        let on = a.starts_with('-');
        for c in a[1..].chars() {
            match c {
                'o' => match args.get(i + 1) {
                    Some(name) => {
                        i += 1;
                        if let Some(c) = crate::options::refused(name, on) {
                            builtin_err("set", &format!("can't change option: {}", c));
                            return 1;
                        }
                        if !crate::options::set(name, on) {
                            return usage("set", &format!("no such option: {}", name));
                        }
                    }
                    None => list_options(!on),
                },
                // zsh: set -A name values (replace) / +A (overwrite start).
                'A' => {
                    i += 1;
                    match args.get(i) {
                        Some(name) => array = Some((name.clone(), on)),
                        None => {
                            list_vars("", 0);
                            return 0;
                        }
                    }
                }
                's' if crate::options::letter('s').is_none() => {}
                c => match crate::options::letter(c) {
                    Some((name, v)) => {
                        let state = if on { v } else { !v };
                        if let Some(c) = crate::options::refused(name, state) {
                            builtin_err("set", &format!("can't change option: {}", c));
                            return 1;
                        }
                        crate::options::set(name, state);
                    }
                    None => {
                        return usage(
                            "set",
                            &format!("{}{}: invalid option", if on { '-' } else { '+' }, c),
                        )
                    }
                },
            }
        }
        i += 1;
    }
    let rest = args[i.min(args.len())..].to_vec();
    match array {
        Some((name, replace)) => {
            let items = if replace {
                rest
            } else {
                let mut old = with_shell(|s| s.array(&name).cloned()).unwrap_or_default();
                for (k, v) in rest.into_iter().enumerate() {
                    if k < old.len() {
                        old[k] = v;
                    } else {
                        old.push(v);
                    }
                }
                old
            };
            if !array_set(&name, items) {
                return 1;
            }
        }
        None if i < args.len() || args.last().map(|a| a == "--").unwrap_or(false) => {
            with_shell(|s| s.pos_params = rest);
        }
        None => {}
    }
    0
}

// zsh's shift [-p] [n] [array ...]: arrays named after the count are
// shifted instead of the positional parameters; -p takes from the end.
fn builtin_shift(words: &[String]) -> i32 {
    let mut args = &words[1..];
    let mut pop = false;
    if args.first().is_some_and(|a| a == "-p") {
        pop = true;
        args = &args[1..];
    }
    let is_array = |n: &str| n == "argv" || with_shell(|s| s.array(n).is_some());
    let mut num: i64 = 1;
    if let Some(a) = args.first().filter(|a| !is_array(a)) {
        num = match crate::arith::eval(a) {
            Ok(n) => n,
            Err(e) => {
                shell_err(&e);
                return 1;
            }
        };
        args = &args[1..];
    }
    if num < 0 {
        builtin_err("shift", "argument to shift must be non-negative");
        return 1;
    }
    let num = num as usize;
    let cut = |v: &mut Vec<String>| {
        if pop {
            v.truncate(v.len() - num);
        } else {
            v.drain(..num);
        }
    };
    if args.is_empty() {
        return with_shell(|s| {
            if num > s.pos_params.len() {
                builtin_err("shift", "shift count must be <= $#");
                return 1;
            }
            cut(&mut s.pos_params);
            0
        });
    }
    let mut ret = 0;
    for name in args {
        if name == "argv" {
            let ok = with_shell(|s| {
                if num > s.pos_params.len() {
                    return false;
                }
                cut(&mut s.pos_params);
                true
            });
            if !ok {
                builtin_err("shift", "shift count must be <= ${#argv}");
                ret += 1;
            }
            continue;
        }
        let Some(mut items) = with_shell(|s| s.array(name).cloned()) else {
            continue;
        };
        if num > items.len() {
            builtin_err("shift", &format!("shift count must be <= ${{#{}}}", name));
            ret += 1;
            continue;
        }
        cut(&mut items);
        crate::shell::array_set(name, items);
    }
    ret
}

// Declaration commands called with plain words (through `builtin`,
// `command` or `eval`): assignments become typed arguments.
fn decl_from_words(words: &[String]) -> i32 {
    let mut args = vec![crate::typeset::DeclArg::Word(words[0].clone())];
    let mut options_done = false;
    for w in &words[1..] {
        if !options_done && (w.starts_with('-') || w.starts_with('+')) {
            if w == "--" {
                options_done = true;
            }
            args.push(crate::typeset::DeclArg::Word(w.clone()));
            continue;
        }
        options_done = true;
        let (name, value, append) = split_assignment(w);
        match value {
            Some(v) => args.push(crate::typeset::DeclArg::Assign {
                name: name.to_string(),
                append,
                value: Some(v.to_string()),
            }),
            None => args.push(crate::typeset::DeclArg::Word(w.clone())),
        }
    }
    crate::typeset::typeset(&words[0], &args)
}

/// `functions` and `typeset -f`: print function definitions (all, or the
/// named ones; names only with `+`; patterns with -m; -x N indents with
/// N spaces).
pub fn functions_cmd(
    _cmd: &str,
    minus: &[char],
    plus: &[char],
    rest: &[crate::typeset::DeclArg],
) -> i32 {
    let names: Vec<String> = rest
        .iter()
        .map(|a| match a {
            crate::typeset::DeclArg::Word(w) => w.clone(),
            crate::typeset::DeclArg::Assign { name, .. }
            | crate::typeset::DeclArg::Array { name, .. } => name.clone(),
        })
        .collect();
    // typeset -fu: autoload, as `autoload` does.
    if minus.contains(&'u') && !names.is_empty() {
        let mut words = vec!["autoload".to_string()];
        words.extend(names.iter().cloned());
        return builtin_autoload(&words);
    }
    // typeset -ft / -fT / +ft: the trace attributes.
    let tflags: Vec<(char, bool)> = minus
        .iter()
        .filter(|c| matches!(c, 't' | 'T'))
        .map(|&c| (c, true))
        .chain(
            plus.iter()
                .filter(|c| matches!(c, 't' | 'T'))
                .map(|&c| (c, false)),
        )
        .collect();
    if !tflags.is_empty() {
        for n in &names {
            for &(c, on) in &tflags {
                crate::shell::set_traced(n, on.then_some(c == 'T'));
            }
        }
        return 0;
    }
    // `m` (match patterns) counts whether given with - or +.
    let patterns = minus.contains(&'m') || plus.contains(&'m');
    functions_listing(&names, patterns, !plus.is_empty(), None)
}

fn functions_listing(
    names: &[String],
    patterns: bool,
    names_only: bool,
    spaces: Option<usize>,
) -> i32 {
    let mut all: Vec<(String, Arc<Node>)> = with_shell(|s| {
        s.functions
            .iter()
            .map(|(k, f)| (k.clone(), f.body.clone()))
            .collect()
    });
    all.sort_by(|a, b| a.0.cmp(&b.0));
    let mut status = 0;
    let mut text = String::new();
    let mut emit = |name: &str, body: &Node| {
        if names_only {
            text.push_str(name);
            text.push('\n');
        } else {
            text.push_str(&crate::deparse::function_text(name, body, spaces));
        }
    };
    if names.is_empty() {
        for (n, b) in &all {
            emit(n, b);
        }
    } else {
        for want in names {
            let mut found = false;
            for (n, b) in &all {
                if (patterns && crate::pattern::matches(want, n)) || (!patterns && n == want) {
                    emit(n, b);
                    found = true;
                }
            }
            if !found && !patterns {
                status = 1;
            }
        }
    }
    out(&text);
    status
}

// `functions -M [-s] name [min [max [shellfn]]]` defines a math
// function, `functions +M name` removes one, `functions -M` lists them.
fn math_functions(words: &[String]) -> i32 {
    use crate::arith::{MathFunc, MATH_FUNCS};
    let remove = words[1].starts_with('+');
    let string = words[1].contains('s');
    let args = &words[2..];
    let Ok(mut table) = MATH_FUNCS.write() else {
        return 1;
    };
    if remove {
        table.retain(|m| !args.contains(&m.name));
        return 0;
    }
    let Some(name) = args.first() else {
        let mut text = String::new();
        for m in table.iter() {
            text.push_str(&format!(
                "functions -M{} {} {} {}{}\n",
                if m.string { "s" } else { "" },
                m.name,
                m.min,
                m.max,
                if m.shellfn != m.name {
                    format!(" {}", m.shellfn)
                } else {
                    String::new()
                }
            ));
        }
        out(&text);
        return 0;
    };
    let num = |k: usize, d: i64| args.get(k).and_then(|a| a.parse::<i64>().ok()).unwrap_or(d);
    let min = num(1, 0);
    let max = if args.len() > 2 {
        num(2, -1)
    } else if args.len() > 1 {
        min
    } else {
        -1
    };
    table.retain(|m| m.name != *name);
    table.push(MathFunc {
        name: name.clone(),
        min,
        max,
        shellfn: args.get(3).cloned().unwrap_or_else(|| name.clone()),
        string,
    });
    0
}

// `functions -c old new`: copies a function, loading it first if it is
// autoloaded.
fn copy_function(words: &[String]) -> i32 {
    let (Some(old), Some(new)) = (words.get(2), words.get(3)) else {
        return usage("functions", "-c: requires two arguments");
    };
    if is_autoload_stub(old) && !autoload_now(old, None, true) {
        shell_err(&format!("{}: function definition file not found", old));
        return 1;
    }
    let Some(f) = with_shell(|s| s.functions.get(old).cloned()) else {
        builtin_err("functions", &format!("no such function: {}", old));
        return 1;
    };
    with_shell(|s| s.functions.insert(new.clone(), f));
    0
}

fn builtin_functions(words: &[String]) -> i32 {
    // `functions -t/-T/+t/+T name`: trace those functions.
    if let Some(flag) = words.get(1).filter(|a| {
        a.len() > 1
            && (a.starts_with('-') || a.starts_with('+'))
            && a[1..].chars().all(|c| c == 't' || c == 'T')
    }) {
        for n in &words[2..] {
            if flag.starts_with('+') {
                crate::shell::set_traced(n, None);
            } else {
                for c in flag[1..].chars() {
                    crate::shell::set_traced(n, Some(c == 'T'));
                }
            }
        }
        return 0;
    }
    // `functions -W name` / `+W`: WARN_NESTED_VAR for those functions.
    if let Some(flag) = words.get(1).filter(|a| *a == "-W" || *a == "+W") {
        if let Ok(mut w) = crate::shell::WARN_NESTED_FUNCS.write() {
            for n in &words[2..] {
                w.retain(|x| x != n);
                if flag == "-W" {
                    w.push(n.clone());
                }
            }
        }
        return 0;
    }
    if words.get(1).is_some_and(|a| a == "-c") {
        return copy_function(words);
    }
    if words
        .get(1)
        .is_some_and(|a| a.len() > 1 && (a.starts_with("-M") || a.starts_with("+M")))
    {
        return math_functions(words);
    }
    let (mut patterns, mut names_only, mut spaces) = (false, false, None);
    let mut i = 1;
    while let Some(a) = words.get(i) {
        if a == "+" {
            names_only = true;
        } else if a == "--" {
            i += 1;
            break;
        } else if let Some(f) = a.strip_prefix('-').filter(|f| !f.is_empty()) {
            let mut chars = f.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    'm' => patterns = true,
                    'x' => {
                        let rest: String = chars.by_ref().collect();
                        let n = if rest.is_empty() {
                            i += 1;
                            words.get(i).cloned().unwrap_or_default()
                        } else {
                            rest
                        };
                        match n.parse() {
                            Ok(v) => spaces = Some(v),
                            Err(_) => {
                                return usage("functions", &format!("invalid indentation: {}", n))
                            }
                        }
                    }
                    _ => {}
                }
            }
        } else if a.starts_with('+') && a.len() > 1 {
            names_only = true;
        } else {
            break;
        }
        i += 1;
    }
    functions_listing(&words[i.min(words.len())..], patterns, names_only, spaces)
}

// ---- read -----------------------------------------------------------------

// Splits `chars` (character, escaped) into at most `n` fields by IFS; the
// last field takes the rest of the line. `n == 0` means no limit.
fn read_split(chars: &[(char, bool)], ifs: &str, n: usize) -> Vec<String> {
    let is_ws = |c: char| matches!(c, ' ' | '\t' | '\n') && ifs.contains(c);
    let is_delim = |&(c, esc): &(char, bool)| !esc && ifs.contains(c);
    let len = chars.len();
    let mut i = 0;
    while i < len && is_delim(&chars[i]) && is_ws(chars[i].0) {
        i += 1;
    }
    let mut out = Vec::new();
    while i < len && (n == 0 || out.len() + 1 < n) {
        let mut f = String::new();
        while i < len && !is_delim(&chars[i]) {
            f.push(chars[i].0);
            i += 1;
        }
        out.push(f);
        while i < len && is_delim(&chars[i]) && is_ws(chars[i].0) {
            i += 1;
        }
        if i < len && is_delim(&chars[i]) && !is_ws(chars[i].0) {
            i += 1;
            while i < len && is_delim(&chars[i]) && is_ws(chars[i].0) {
                i += 1;
            }
        }
    }
    if n != 0 && i < len {
        let mut end = len;
        while end > i && is_delim(&chars[end - 1]) && is_ws(chars[end - 1].0) {
            end -= 1;
        }
        out.push(chars[i..end].iter().map(|(c, _)| *c).collect());
    }
    while n != 0 && out.len() < n {
        out.push(String::new());
    }
    out
}

fn builtin_read(words: &[String]) -> i32 {
    let mut fd = 0;
    let mut keys: Option<usize> = None;
    let mut yes_no = false;
    let mut raw = false;
    let mut prompt = None;
    let mut delim: Vec<u8> = vec![b'\n'];
    let mut from_stack = false;
    let mut array: Option<String> = None;
    // -e echoes the input instead of assigning it, -E does both.
    let (mut echo, mut echo_only) = (false, false);
    let mut timeout: Option<f64> = None;
    let mut i = 1;
    while i < words.len() && words[i].starts_with('-') && words[i].len() > 1 {
        let a = &words[i];
        if a == "--" {
            i += 1;
            break;
        }
        let mut chars = a[1..].chars();
        while let Some(c) = chars.next() {
            let mut take_arg = || -> Option<String> {
                let rest: String = chars.by_ref().collect();
                if !rest.is_empty() {
                    Some(rest)
                } else {
                    i += 1;
                    words.get(i).cloned()
                }
            };
            match c {
                'r' => raw = true,
                's' => {}
                // The whole (possibly multibyte) character ends the input.
                'd' => {
                    delim = take_arg()
                        .and_then(|d| d.chars().next())
                        .map(|c| {
                            crate::shell::decode_raw_byte(c)
                                .map(|b| vec![b])
                                .unwrap_or_else(|| c.to_string().into_bytes())
                        })
                        .unwrap_or_else(|| vec![0]);
                }
                // -z: from the buffer stack that `print -z` pushes to.
                'z' => from_stack = true,
                // zsh -A: the first name is an array (default `reply`).
                'A' => array = Some(String::new()),
                'a' => array = take_arg(),
                // zsh: -k [n] reads n characters, -q one y/n answer.
                'k' => {
                    let rest: String = chars.by_ref().collect();
                    keys = Some(if !rest.is_empty() {
                        rest.parse().unwrap_or(1)
                    } else if let Some(n) = words.get(i + 1).and_then(|w| w.parse::<usize>().ok()) {
                        i += 1;
                        n
                    } else {
                        1
                    });
                }
                'q' => {
                    keys = Some(1);
                    yes_no = true;
                }
                'u' => match take_arg().and_then(|n| n.parse().ok()) {
                    Some(n) => fd = n,
                    None => return usage("read", "-u: bad file descriptor"),
                },
                'p' => match with_shell(|s| s.coproc) {
                    Some((_, r)) => fd = r,
                    None => return usage("read", "-p: no coprocess"),
                },
                'E' => echo = true,
                'e' => {
                    echo = true;
                    echo_only = true;
                }
                // -t [secs]: fail if no input arrives in time.
                't' => {
                    let rest: String = chars.by_ref().collect();
                    timeout = Some(if !rest.is_empty() {
                        rest.parse().unwrap_or(0.0)
                    } else if let Some(n) = words.get(i + 1).and_then(|w| w.parse::<f64>().ok()) {
                        i += 1;
                        n
                    } else {
                        0.0
                    });
                }
                _ => return usage("read", &format!("-{}: invalid option", c)),
            }
        }
        i += 1;
    }
    let mut names: Vec<String> = words[i.min(words.len())..].to_vec();
    if array.as_deref() == Some("") {
        array = Some(if names.is_empty() {
            "reply".to_string()
        } else {
            names.remove(0)
        });
    }
    // zsh: `read 'name?prompt'` prompts on a terminal.
    if let Some(first) = names.first_mut() {
        if let Some((n, p)) = first.split_once('?') {
            prompt = Some(p.to_string());
            *first = n.to_string();
        }
    }
    for n in &names {
        if !is_valid_name(n) {
            builtin_err("read", &format!("`{}': not a valid identifier", n));
            return 2;
        }
    }
    if let Some(p) = prompt {
        if unsafe { libc::isatty(0) } == 1 {
            let _ = crate::sys::write_fd(2, p.as_bytes());
        }
    }

    if let Some(t) = timeout {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = (t * 1000.0).round().clamp(0.0, i32::MAX as f64) as i32;
        if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
            return 1;
        }
    }
    if let Some(n) = keys {
        let text = match read_chars(fd, n) {
            Some(t) => t,
            None => return 1,
        };
        let target = names
            .first()
            .cloned()
            .unwrap_or_else(|| "REPLY".to_string());
        if yes_no {
            let yes = matches!(text.as_str(), "y" | "Y");
            var_set(&target, if yes { "y" } else { "n" }, false);
            return if yes { 0 } else { 1 };
        }
        return if var_set(&target, &text, false) { 0 } else { 2 };
    }

    let mut bytes: Vec<u8> = Vec::new();
    let mut escaped: Vec<bool> = Vec::new();
    let mut got_delim = false;
    let mut pending_backslash = false;
    if from_stack {
        match crate::print::pop_buffer_stack() {
            Some(line) => {
                bytes = line.into_bytes();
                escaped = vec![false; bytes.len()];
                got_delim = true;
            }
            None => return 1,
        }
    }
    if !from_stack {
        loop {
            match read_byte(fd) {
                Ok(Some(b)) => {
                    if pending_backslash {
                        pending_backslash = false;
                        if b == b'\n' {
                            continue;
                        }
                        bytes.push(b);
                        escaped.push(true);
                        continue;
                    }
                    if delim.len() == 1 && b == delim[0] {
                        got_delim = true;
                        break;
                    }
                    if delim.len() > 1
                        && b == delim[delim.len() - 1]
                        && bytes.ends_with(&delim[..delim.len() - 1])
                    {
                        let keep = bytes.len() + 1 - delim.len();
                        bytes.truncate(keep);
                        escaped.truncate(keep);
                        got_delim = true;
                        break;
                    }
                    if b == b'\\' && !raw {
                        pending_backslash = true;
                        continue;
                    }
                    bytes.push(b);
                    escaped.push(false);
                }
                Ok(None) => break,
                // Only an interrupt counts as one; a closed or unreadable
                // descriptor is a plain failure.
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => return 128 + libc::SIGINT,
                Err(_) => return 1,
            }
        }
    }

    let chars: Vec<(char, bool)> = {
        let mut out = Vec::new();
        let mut idx = 0;
        while idx < bytes.len() {
            match std::str::from_utf8(&bytes[idx..]) {
                Ok(s) => {
                    for c in s.chars() {
                        out.push((c, escaped[idx]));
                        idx += c.len_utf8();
                    }
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        let s = unsafe { std::str::from_utf8_unchecked(&bytes[idx..idx + valid]) };
                        for c in s.chars() {
                            out.push((c, escaped[idx]));
                            idx += c.len_utf8();
                        }
                    }
                    let bad = e.error_len().unwrap_or(bytes.len() - idx);
                    for _ in 0..bad {
                        out.push((crate::shell::raw_byte_char(bytes[idx]), escaped[idx]));
                        idx += 1;
                    }
                }
            }
        }
        out
    };
    let ifs = var_get_opt("IFS").unwrap_or_else(|| " \t\n".to_string());
    if echo {
        let line: String = chars.iter().map(|(c, _)| *c).collect();
        // With -A each word goes on its own line.
        if array.is_some() {
            let words = read_split(&chars, &ifs, 0);
            out(&words.iter().map(|w| format!("{}\n", w)).collect::<String>());
        } else {
            out(&format!("{}\n", line));
        }
        if echo_only {
            return if got_delim || !bytes.is_empty() { 0 } else { 1 };
        }
    }

    let ok = if let Some(arr) = array {
        array_set(&arr, read_split(&chars, &ifs, 0))
    } else if names.is_empty() {
        var_set(
            "REPLY",
            &chars.iter().map(|(c, _)| *c).collect::<String>(),
            false,
        )
    } else {
        let fields = read_split(&chars, &ifs, names.len());
        names.iter().zip(fields).all(|(n, v)| var_set(n, &v, false))
    };
    if !ok {
        return 1;
    }
    if got_delim {
        0
    } else {
        1
    }
}

// Reads `n` characters from standard input, from a terminal without
// waiting for Enter. None at end of input.
fn read_chars(fd: i32, n: usize) -> Option<String> {
    let tty = unsafe { libc::isatty(fd) } == 1;
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if tty && unsafe { libc::tcgetattr(fd, &mut saved) } == 0 {
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
    }
    let mut bytes = Vec::new();
    let mut count = 0;
    while count < n {
        match read_byte(fd) {
            Ok(Some(b)) => {
                bytes.push(b);
                // Count a character once its UTF-8 sequence is complete.
                if std::str::from_utf8(&bytes).is_ok() {
                    count = crate::shell::bytes_to_zstr(&bytes).chars().count();
                }
            }
            _ => break,
        }
    }
    if tty {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    }
    if bytes.is_empty() {
        return None;
    }
    Some(crate::shell::bytes_to_zstr(&bytes))
}

// ---- getopts --------------------------------------------------------------

// zsh's getopts (bin_getopts): OPTIND stays on an argument until all of
// its option letters are used; `+x` options too unless POSIX_BUILTINS
// (or -p).
fn builtin_getopts(words: &[String]) -> i32 {
    let mut i = 1;
    let mut posix_flag = false;
    while let Some(a) = words.get(i) {
        let Some(body) = a.strip_prefix('-') else {
            break;
        };
        i += 1;
        if body.is_empty() || body == "-" {
            break;
        }
        for c in body.chars() {
            if c == 'p' {
                posix_flag = true;
            } else {
                builtin_err("getopts", &format!("bad option: -{}", c));
                return 1;
            }
        }
    }
    if words.len() < i + 2 {
        builtin_err("getopts", "not enough arguments");
        return 1;
    }
    let optstring = &words[i];
    let var = &words[i + 1];
    let args: Vec<String> = if words.len() > i + 2 {
        words[i + 2..].to_vec()
    } else {
        with_shell(|s| s.pos_params.clone())
    };
    let posix = posix_flag || crate::options::is_set("posixbuiltins");
    let mut optind: i64 = var_get("OPTIND").trim().parse().unwrap_or(1);
    let mut optcind = with_shell(|s| s.getopts_pos);
    let finish = |optind: i64, optcind: usize| {
        crate::shell::without_assign_warnings(|| var_set("OPTIND", &optind.to_string(), false));
        with_shell(|s| s.getopts_pos = optcind);
    };
    if optind < 1 {
        optind = 1;
        optcind = 0;
    }
    if (args.len() as i64) < optind {
        finish(optind, optcind);
        return 1;
    }
    let quiet = optstring.starts_with(':');
    let spec: Vec<char> = optstring.chars().skip(usize::from(quiet)).collect();
    let mut s: Vec<char> = args[optind as usize - 1].chars().collect();
    if s.is_empty() {
        finish(optind, optcind);
        return 1;
    }
    if optcind >= s.len() {
        optcind = 0;
        optind += 1;
        match args.get(optind as usize - 1) {
            Some(a) => s = a.chars().collect(),
            None => {
                finish(optind, optcind);
                return 1;
            }
        }
    }
    if optcind == 0 {
        if s.len() < 2 || (s[0] != '-' && (posix || s[0] != '+')) {
            finish(optind, optcind);
            return 1;
        }
        if s.len() == 2 && s[0] == '-' && s[1] == '-' {
            finish(optind + 1, optcind);
            return 1;
        }
        optcind = 1;
    }
    let opch = s[optcind];
    optcind += 1;
    let optbuf = if s[0] == '+' {
        format!("+{}", opch)
    } else {
        opch.to_string()
    };
    let sign = if s[0] == '+' { '+' } else { '-' };
    let pos = spec.iter().position(|&c| c == opch).filter(|_| opch != ':');
    let Some(p) = pos else {
        if posix {
            optcind = 0;
            optind += 1;
        }
        var_set(var, "?", false);
        if quiet {
            var_set("OPTARG", &optbuf, false);
        } else {
            shell_err(&format!("bad option: {}{}", sign, opch));
            var_set("OPTARG", "", false);
        }
        finish(optind, optcind);
        return 0;
    };
    if spec.get(p + 1) == Some(&':') {
        let optarg = if optcind == s.len() {
            match args.get(optind as usize) {
                None => {
                    if posix {
                        optcind = 0;
                        optind += 1;
                    }
                    if quiet {
                        var_set(var, ":", false);
                        var_set("OPTARG", &optbuf, false);
                    } else {
                        var_set(var, "?", false);
                        var_set("OPTARG", "", false);
                        shell_err(&format!("argument expected after {}{} option", sign, opch));
                    }
                    finish(optind, optcind);
                    return 0;
                }
                Some(a) => {
                    optind += 1;
                    a.clone()
                }
            }
        } else {
            s[optcind..].iter().collect()
        };
        optcind = 0;
        optind += 1;
        var_set("OPTARG", &optarg, false);
    } else {
        var_set("OPTARG", "", false);
    }
    var_set(var, &optbuf, false);
    finish(optind, optcind);
    0
}

// ---- directories ----------------------------------------------------------

fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    format!("/{}", parts.join("/"))
}

fn physical_cwd() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

fn chdir(path: &str) -> Result<(), i32> {
    let c = std::ffi::CString::new(path).map_err(|_| libc::EINVAL)?;
    if unsafe { libc::chdir(c.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum CdKind {
    Cd,
    Pushd,
    Popd,
}

// The directory stack with the current directory pushed in front, as
// zsh's bin_cd works on it: index 0 is the newest entry.
fn dir_list() -> Vec<String> {
    with_shell(|s| s.dir_stack.clone())
}

fn set_dir_list(list: Vec<String>) {
    with_shell(|s| s.dir_stack = list);
}

// A directory as zsh prints it: under $HOME as `~/...`.
pub(crate) fn print_dir(d: &str) -> String {
    let home = var_get("HOME");
    if !home.is_empty() && home != "/" {
        if d == home {
            return "~".to_string();
        }
        if let Some(rest) = d.strip_prefix(&home).filter(|r| r.starts_with('/')) {
            return format!("~{}", rest);
        }
    }
    d.to_string()
}

fn print_dirstack() {
    let mut text = print_dir(&var_get("PWD"));
    for d in dir_list() {
        text.push(' ');
        text.push_str(&print_dir(&d));
    }
    text.push('\n');
    out(&text);
}

// Whether any part of `path` (relative to the current directory) is a
// symbolic link.
fn has_symlink(path: &str) -> bool {
    let mut cur = std::path::PathBuf::new();
    if path.starts_with('/') {
        cur.push("/");
    }
    path.split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .any(|c| {
            cur.push(c);
            std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink())
        })
}

// zsh's cd_try_chdir: the full path (logical unless chasing links),
// changed into.
fn cd_try(prefix: Option<&str>, dest: &str, physical: bool) -> Result<String, i32> {
    let pwd = var_get_opt("PWD")
        .filter(|p| p.starts_with('/'))
        .or_else(physical_cwd)
        .unwrap_or_else(|| "/".to_string());
    let full = match prefix.filter(|p| !p.is_empty()) {
        Some(p) if p.starts_with('/') => format!("{}/{}", p.trim_end_matches('/'), dest),
        Some(p) => format!("{}/{}/{}", pwd.trim_end_matches('/'), p, dest),
        None if dest.starts_with('/') => dest.to_string(),
        None => format!("{}/{}", pwd.trim_end_matches('/'), dest),
    };
    if physical {
        // From where the shell really is: $PWD may name a directory that
        // has since been renamed.
        if prefix.is_none() && !dest.starts_with('/') && chdir(dest).is_ok() {
            return Ok(physical_cwd().unwrap_or(full));
        }
        chdir(&full)?;
        return Ok(physical_cwd().unwrap_or(full));
    }
    let logical = normalize_path(&full);
    if chdir(&logical).is_err() {
        // The logical path may not exist (a renamed parent); try as given.
        if prefix.is_some() || dest.starts_with('/') {
            return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
        }
        chdir(dest)?;
        return Ok(physical_cwd().unwrap_or(logical));
    }
    Ok(logical)
}

// zsh's cd_do_chdir: absolute paths, then the directory as given and
// $cdpath entries, then CDABLE_VARS. Sets `printdir` when the result
// should be shown.
fn cd_do_chdir(cmd: &str, dest: &str, physical: bool, printdir: &mut bool) -> Option<String> {
    let mut eno = libc::ENOENT;
    if dest.starts_with('/') {
        return match cd_try(None, dest, physical) {
            Ok(r) => Some(r),
            Err(e) => {
                builtin_err(cmd, &crate::exec::file_error(dest, e));
                None
            }
        };
    }
    let nocdpath = dest == "." || dest == ".." || dest.starts_with("./") || dest.starts_with("../");
    let posix = crate::options::is_set("posixcd");
    let cdpath: Vec<String> = with_shell(|s| s.array("cdpath").cloned())
        .or_else(|| var_get_opt("CDPATH").map(|c| c.split(':').map(|s| s.to_string()).collect()))
        .unwrap_or_default();
    let hasdot = !nocdpath && !posix && cdpath.iter().any(|p| p.is_empty() || p == ".");
    if !hasdot && !posix {
        match cd_try(None, dest, physical) {
            Ok(r) => return Some(r),
            Err(e) if e != libc::ENOENT => eno = e,
            Err(_) => {}
        }
    }
    if !nocdpath {
        for p in &cdpath {
            match cd_try(Some(p), dest, physical) {
                Ok(r) => {
                    if !p.is_empty() && p != "." {
                        *printdir = true;
                    }
                    return Some(r);
                }
                Err(e) if e != libc::ENOENT => eno = e,
                Err(_) => {}
            }
        }
    }
    if posix {
        match cd_try(None, dest, physical) {
            Ok(r) => return Some(r),
            Err(e) if e != libc::ENOENT => eno = e,
            Err(_) => {}
        }
    }
    // CDABLE_VARS: `cd name/rest` with name a parameter holding a path.
    if crate::options::is_set("cdablevars") {
        let (name, rest) = match dest.find('/') {
            Some(k) => (&dest[..k], &dest[k..]),
            None => (dest, ""),
        };
        if let Some(v) = var_get_opt(name).filter(|v| v.starts_with('/')) {
            match cd_try(None, &format!("{}{}", v, rest), physical) {
                Ok(r) => {
                    *printdir = true;
                    return Some(r);
                }
                Err(e) if e != libc::ENOENT => eno = e,
                Err(_) => {}
            }
        }
    }
    builtin_err(cmd, &crate::exec::file_error(dest, eno));
    None
}

// zsh's bin_cd for cd, chdir, pushd and popd.
fn cd_family(words: &[String], kind: CdKind) -> i32 {
    let cmd = words[0].as_str();
    let mut physical = crate::options::is_set("chaselinks");
    let mut quiet = false;
    let mut nolinks = false;
    let mut i = 1;
    while let Some(a) = words.get(i) {
        let Some(flags) = a.strip_prefix('-').filter(|f| !f.is_empty()) else {
            break;
        };
        if flags.chars().all(|c| c.is_ascii_digit()) {
            break;
        }
        if a == "--" {
            i += 1;
            break;
        }
        for c in flags.chars() {
            match c {
                'P' => physical = true,
                'L' => physical = false,
                'q' => quiet = true,
                's' => nolinks = true,
                _ => return usage(cmd, &format!("bad option: -{}", c)),
            }
        }
        i += 1;
    }
    let args = &words[i..];
    let old_pwd = var_get_opt("PWD")
        .filter(|p| p.starts_with('/'))
        .or_else(physical_cwd)
        .unwrap_or_default();
    let mut list = dir_list();
    list.insert(0, old_pwd.clone());
    let interactive = with_shell(|s| s.interactive);
    let mut printdir = false;

    // cd_get_dest: the entry to change to.
    let mut dir: Option<usize> = None;
    match args {
        [] => {
            if kind == CdKind::Popd && list.len() < 2 {
                builtin_err(cmd, "directory stack empty");
                return 1;
            }
            if kind == CdKind::Pushd && !crate::options::is_set("pushdtohome") && list.len() > 1 {
                // Swap the top two entries.
                let pwd_entry = list.remove(0);
                list.insert(1, pwd_entry);
                dir = Some(0);
            } else if kind != CdKind::Popd {
                match var_get_opt("HOME").filter(|h| !h.is_empty()) {
                    Some(h) => list.insert(0, h),
                    None => {
                        builtin_err(cmd, "HOME not set");
                        return 1;
                    }
                }
            }
        }
        [arg] => {
            printdir = true;
            let digits = arg
                .strip_prefix('+')
                .or_else(|| arg.strip_prefix('-'))
                .filter(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()));
            if let Some(digits) = digits.filter(|_| !crate::options::is_set("posixcd")) {
                let n: usize = digits.parse().unwrap_or(usize::MAX);
                let from_top = arg.starts_with('+') != crate::options::is_set("pushdminus");
                let idx = if from_top {
                    Some(n)
                } else {
                    (list.len() - 1).checked_sub(n)
                };
                match idx.filter(|&k| k < list.len()) {
                    Some(k) => dir = Some(k),
                    None => {
                        builtin_err(cmd, "no such entry in dir stack");
                        return 1;
                    }
                }
            }
            if dir.is_none() {
                if arg == "-" {
                    match var_get_opt("OLDPWD") {
                        Some(o) => list.insert(0, o),
                        None => {
                            builtin_err(cmd, "OLDPWD not set");
                            return 1;
                        }
                    }
                } else {
                    printdir = false;
                    list.insert(0, arg.clone());
                }
            }
        }
        [from, to, ..] => {
            let Some(k) = old_pwd.find(from.as_str()) else {
                builtin_err(cmd, &format!("string not in pwd: {}", from));
                return 1;
            };
            let d = format!("{}{}{}", &old_pwd[..k], to, &old_pwd[k + from.len()..]);
            list.insert(0, d);
            printdir = true;
        }
    }
    let mut target = dir;
    if kind == CdKind::Popd {
        match dir {
            None => {
                target = Some(0);
                dir = Some(0);
            }
            Some(k) if k != 0 => {
                return cd_new_pwd(kind, list, k, quiet, interactive, printdir);
            }
            _ => {}
        }
        dir = dir.map(|k| k + 1);
    }
    let d = dir.unwrap_or(0);
    let Some(dest) = list.get(d).cloned() else {
        return 1;
    };
    // -s: a path through a symbolic link is refused.
    if nolinks && has_symlink(&dest) {
        builtin_err(cmd, &format!("not a directory: {}", dest));
        return 1;
    }
    let Some(newdir) = cd_do_chdir(cmd, &dest, physical, &mut printdir) else {
        return 1;
    };
    list[d] = newdir;
    cd_new_pwd(
        kind,
        list,
        target.unwrap_or(d),
        quiet,
        interactive,
        printdir,
    )
}

// zsh's cd_new_pwd: updates the stack, $PWD and $OLDPWD, prints, and
// runs the chpwd hooks.
fn cd_new_pwd(
    kind: CdKind,
    mut list: Vec<String>,
    dir: usize,
    quiet: bool,
    interactive: bool,
    printdir: bool,
) -> i32 {
    if kind == CdKind::Pushd {
        list.rotate_left(dir);
    }
    let idx = if kind == CdKind::Pushd { 0 } else { dir };
    let mut new_pwd = list.remove(idx);
    if kind == CdKind::Popd && !list.is_empty() {
        new_pwd = list.remove(0);
    } else if kind == CdKind::Cd && !crate::options::is_set("autopushd") && !list.is_empty() {
        list.remove(0);
    }
    if crate::options::is_set("pushdignoredups") {
        if let Some(k) = list.iter().position(|d| *d == new_pwd) {
            list.remove(k);
        }
    }
    if std::path::Path::new(&new_pwd).is_dir() {
        let _ = chdir(&new_pwd);
    }
    let old = var_get("PWD");
    var_set("OLDPWD", &old, false);
    var_set("PWD", &new_pwd, false);
    if let Some(size) = var_get_opt("DIRSTACKSIZE").and_then(|v| v.trim().parse::<usize>().ok()) {
        if size > 0 {
            list.truncate(size.max(2) - 1);
        }
    }
    set_dir_list(list);
    if interactive || crate::options::is_set("posixcd") {
        if kind != CdKind::Cd && interactive {
            if !crate::options::is_set("pushdsilent") && !quiet {
                print_dirstack();
            }
        } else if !crate::options::is_set("cdsilent") && printdir {
            out(&format!("{}\n", print_dir(&new_pwd)));
        }
    }
    if !quiet {
        run_hooks("chpwd", &[]);
    }
    crate::zle::termctl::notify_pwd();
    0
}

fn builtin_cd(words: &[String]) -> i32 {
    cd_family(words, CdKind::Cd)
}

fn builtin_pwd(words: &[String]) -> i32 {
    let physical = words.iter().skip(1).any(|a| a == "-P")
        || (crate::options::is_set("chaselinks") && !words.iter().skip(1).any(|a| a == "-L"));
    let pwd = var_get_opt("PWD").filter(|p| p.starts_with('/'));
    let dir = match (physical, pwd) {
        (false, Some(p)) => p,
        _ => match physical_cwd() {
            Some(p) => p,
            None => {
                builtin_err("pwd", "cannot determine current directory");
                return 1;
            }
        },
    };
    if out(&format!("{}\n", dir)) {
        0
    } else {
        1
    }
}

fn builtin_pushd(words: &[String]) -> i32 {
    cd_family(words, CdKind::Pushd)
}

fn builtin_popd(words: &[String]) -> i32 {
    cd_family(words, CdKind::Popd)
}

// `dirs [-c] [-l] [-p] [-v] [dir ...]`: lists the stack, or replaces it.
fn builtin_dirs(words: &[String]) -> i32 {
    let (mut clear, mut long, mut per_line, mut numbered) = (false, false, false, false);
    let mut i = 1;
    while let Some(a) = words.get(i).filter(|a| a.starts_with('-') && a.len() > 1) {
        for c in a[1..].chars() {
            match c {
                'c' => clear = true,
                'l' => long = true,
                'p' => per_line = true,
                'v' => numbered = true,
                _ => return usage("dirs", &format!("bad option: -{}", c)),
            }
        }
        i += 1;
    }
    let args = &words[i..];
    if (args.is_empty() && !clear) || numbered || per_line {
        let show = |d: &str| if long { d.to_string() } else { print_dir(d) };
        let mut all = vec![var_get("PWD")];
        all.extend(dir_list());
        let text: String = if numbered {
            all.iter()
                .enumerate()
                .map(|(k, d)| format!("{}\t{}\n", k, show(d)))
                .collect()
        } else if per_line {
            all.iter().map(|d| format!("{}\n", show(d))).collect()
        } else {
            format!(
                "{}\n",
                all.iter().map(|d| show(d)).collect::<Vec<_>>().join(" ")
            )
        };
        out(&text);
        return 0;
    }
    set_dir_list(args.to_vec());
    0
}

// ---- output ---------------------------------------------------------------

fn builtin_echo(words: &[String]) -> i32 {
    let mut args = &words[1..];
    let mut newline = true;
    // zsh's echo reads escapes unless BSD_ECHO (set in sh emulation).
    let mut escapes = !crate::options::is_set("bsdecho");
    while let Some(a) = args.first() {
        let flags = a.strip_prefix('-').unwrap_or("");
        if !flags.is_empty() && flags.chars().all(|c| matches!(c, 'n' | 'e' | 'E')) {
            for c in flags.chars() {
                match c {
                    'n' => newline = false,
                    'e' => escapes = true,
                    _ => escapes = false,
                }
            }
            args = &args[1..];
        } else {
            break;
        }
    }
    let mut text = String::new();
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            text.push(' ');
        }
        if escapes {
            if let Some(cut) = a.find("\\c") {
                text.push_str(&crate::expand::decode_escapes(&a[..cut]));
                newline = false;
                break;
            }
            text.push_str(&crate::expand::decode_escapes(a));
        } else {
            text.push_str(a);
        }
    }
    if newline {
        text.push('\n');
    }
    if out(&text) {
        0
    } else {
        1
    }
}

fn builtin_history() -> i32 {
    let lines = with_shell(|s| s.history.lock().unwrap_or_else(|e| e.into_inner()).clone());
    let mut text = String::new();
    for (i, line) in lines.iter().enumerate() {
        text.push_str(&format!("{:5}  {}\n", i + 1, line));
    }
    out(&text);
    0
}

// ---- aliases --------------------------------------------------------------

// alias [+-gmrsL] [name[=value] ...]: -g global, -s suffix, -r regular
// aliases; -L lists as commands, -m treats names as patterns.
fn builtin_alias(words: &[String]) -> i32 {
    let mut args = &words[1..];
    let (mut global, mut suffix, mut list_cmd, mut pattern) = (false, false, false, false);
    while let Some(a) = args.first() {
        if a == "--" {
            args = &args[1..];
            break;
        }
        let Some(flags) = a.strip_prefix(['-', '+']).filter(|f| !f.is_empty()) else {
            break;
        };
        for c in flags.chars() {
            match c {
                'g' => global = true,
                's' => suffix = true,
                'L' => list_cmd = true,
                'm' => pattern = true,
                'r' => {}
                _ => {
                    builtin_err("alias", &format!("bad option: {}{}", &a[..1], c));
                    return 1;
                }
            }
        }
        args = &args[1..];
    }
    let table_name = if suffix {
        crate::types::SALIASES
    } else if global {
        crate::types::GALIASES
    } else {
        crate::types::ALIASES
    };
    let flag = if suffix {
        "-s "
    } else if global {
        "-g "
    } else {
        ""
    };
    let table = || with_shell(|s| s.assoc(table_name).cloned().unwrap_or_default());
    let show = |k: &str, v: &str| -> String {
        if list_cmd {
            format!(
                "alias {}{}={}\n",
                flag,
                crate::builtins::shell_quote(k),
                shell_quote(v)
            )
        } else {
            format!("{}={}\n", k, shell_quote(v))
        }
    };
    if args.is_empty() || pattern {
        let all = table();
        let text: String = all
            .iter()
            .filter(|(k, _)| args.is_empty() || args.iter().any(|p| crate::pattern::matches(p, k)))
            .filter(|(k, _)| {
                // `alias -L` output is read back as commands, which a name
                // containing `=` would not survive.
                let bad = list_cmd && k.contains('=');
                if bad {
                    crate::shell::shell_err(&format!(
                        "invalid alias '{}' encountered while printing aliases",
                        k
                    ));
                }
                !bad
            })
            .map(|(k, v)| show(k, v))
            .collect();
        out(&text);
        return 0;
    }
    let mut status = 0;
    for a in args {
        match a.find('=') {
            Some(i) => {
                let (name, value) = (a[..i].to_string(), a[i + 1..].to_string());
                with_shell(|s| s.assoc_entry(table_name).insert(name, value));
            }
            None => match table().get(a.as_str()).cloned() {
                Some(v) => {
                    out(&show(a, &v));
                }
                None => {
                    builtin_err("alias", &format!("{}: not found", a));
                    status = 1;
                }
            },
        }
    }
    status
}

fn builtin_unalias(words: &[String]) -> i32 {
    let mut status = 0;
    let (mut all, mut suffix, mut global) = (false, false, false);
    let mut i = 1;
    while let Some(a) = words.get(i) {
        let Some(flags) = a.strip_prefix('-').filter(|f| !f.is_empty()) else {
            break;
        };
        i += 1;
        if flags == "-" {
            break;
        }
        for c in flags.chars() {
            match c {
                'a' => all = true,
                's' => suffix = true,
                'g' => global = true,
                'm' => {}
                _ => {
                    builtin_err("unalias", &format!("bad option: -{}", c));
                    return 1;
                }
            }
        }
    }
    if all {
        with_shell(|s| {
            if suffix {
                s.assoc_entry(crate::types::SALIASES).clear();
            } else {
                s.alias_map(false).clear();
                s.alias_map(true).clear();
            }
        });
        return 0;
    }
    for a in &words[i..] {
        let removed = with_shell(|s| {
            if suffix {
                s.assoc_entry(crate::types::SALIASES)
                    .remove(a.as_str())
                    .is_some()
            } else {
                (!global && s.alias_map(false).remove(a.as_str()).is_some())
                    | s.alias_map(true).remove(a.as_str()).is_some()
            }
        });
        if !removed {
            builtin_err("unalias", &format!("no such hash table element: {}", a));
            status = 1;
        }
    }
    status
}

// ---- command lookup -------------------------------------------------------

fn describe_command(name: &str, verbose: bool) -> Option<String> {
    if let Some(v) = with_shell(|s| s.alias(name, false).cloned()) {
        return Some(if verbose {
            format!("{} is an alias for {}", name, v)
        } else {
            format!("alias {}={}", name, shell_quote(&v))
        });
    }
    if KEYWORDS.contains(&name) {
        return Some(if verbose {
            format!("{} is a shell keyword", name)
        } else {
            name.to_string()
        });
    }
    if with_shell(|s| s.functions.contains_key(name)) {
        return Some(if verbose {
            format!("{} is a function", name)
        } else {
            name.to_string()
        });
    }
    if is_special_builtin(name) {
        return Some(if verbose {
            format!("{} is a special shell builtin", name)
        } else {
            name.to_string()
        });
    }
    if is_builtin(name) {
        return Some(if verbose {
            format!("{} is a shell builtin", name)
        } else {
            name.to_string()
        });
    }
    find_command(name).map(|p| {
        if verbose {
            format!("{} is {}", name, p)
        } else {
            p
        }
    })
}

#[derive(Default)]
struct Whence {
    verbose: bool,   // -v
    csh: bool,       // -c
    word: bool,      // -w
    funcs: bool,     // -f
    all: bool,       // -a
    path_only: bool, // -p
    pattern: bool,   // -m
    symlinks: u8,    // -s 1, -S 2
    indent: Option<usize>,
}

// Resolves symlinks for -s (final target) or -S (each step).
fn whence_links(path: &str, steps: bool) -> String {
    let mut out = path.to_string();
    let mut cur = std::path::PathBuf::from(path);
    let mut seen = std::collections::HashSet::new();
    while let Ok(target) = std::fs::read_link(&cur) {
        if !seen.insert(cur.clone()) {
            return path.to_string();
        }
        let next = cur.parent().map(|p| p.join(&target)).unwrap_or(target);
        let next = std::fs::canonicalize(next.parent().unwrap_or(&next))
            .map(|p| p.join(next.file_name().unwrap_or_default()))
            .unwrap_or(next);
        if steps {
            out.push_str(&format!(" -> {}", next.display()));
        }
        cur = next;
    }
    if !steps {
        if let Ok(real) = std::fs::canonicalize(path) {
            if real.to_string_lossy() != path {
                out.push_str(&format!(" -> {}", real.display()));
            }
        }
    }
    out
}

fn path_matches(name: &str, all: bool) -> Vec<String> {
    if name.contains('/') {
        let p = std::path::Path::new(name);
        return if p.is_file() {
            vec![name.to_string()]
        } else {
            Vec::new()
        };
    }
    let mut found = Vec::new();
    for dir in var_get("PATH").split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let p = std::path::Path::new(dir).join(name);
        if p.is_file() && crate::shell::find_command(&p.to_string_lossy()).is_some() {
            found.push(p.to_string_lossy().into_owned());
            if !all {
                break;
            }
        }
    }
    found
}

// One name for whence and its relatives. Returns false if not found.
fn whence_one(name: &str, o: &Whence, out_text: &mut String) -> bool {
    let mut found = false;
    let mut line = |s: String| {
        out_text.push_str(&s);
        out_text.push('\n');
    };
    if !o.path_only {
        if let Some(v) = with_shell(|s| s.alias(name, false).cloned()) {
            line(if o.word {
                format!("{}: alias", name)
            } else if o.csh {
                format!("{}: aliased to {}", name, v)
            } else if o.verbose {
                format!("{} is an alias for {}", name, v)
            } else {
                v
            });
            found = true;
            if !o.all {
                return true;
            }
        }
        if let Some((ext, v)) = name.rsplit_once('.').and_then(|(stem, ext)| {
            (!stem.is_empty())
                .then(|| {
                    with_shell(|s| {
                        s.assoc(crate::types::SALIASES)
                            .and_then(|m| m.get(ext).cloned())
                    })
                })
                .flatten()
                .map(|v| (ext.to_string(), v))
        }) {
            line(if o.word {
                format!("{}: suffix alias", ext)
            } else if o.verbose {
                format!("{} is a suffix alias for {}", ext, v)
            } else {
                v
            });
            found = true;
            if !o.all {
                return true;
            }
        }
        if let Some(v) = with_shell(|s| s.alias(name, true).cloned()) {
            line(if o.word {
                format!("{}: global alias", name)
            } else if o.csh {
                format!("{}: globally aliased to {}", name, v)
            } else if o.verbose {
                format!("{} is a global alias for {}", name, v)
            } else {
                v
            });
            found = true;
            if !o.all {
                return true;
            }
        }
        if KEYWORDS.contains(&name) {
            line(if o.word {
                format!("{}: reserved", name)
            } else if o.csh {
                format!("{}: shell reserved word", name)
            } else if o.verbose {
                format!("{} is a reserved word", name)
            } else {
                name.to_string()
            });
            found = true;
            if !o.all {
                return true;
            }
        }
        if let Some(f) = with_shell(|s| s.functions.get(name).cloned()) {
            let def = || crate::deparse::function_text(name, &f.body, o.indent);
            line(if o.word {
                format!("{}: function", name)
            } else if o.csh || o.funcs {
                def().trim_end().to_string()
            } else if o.verbose {
                format!("{} is a shell function from {}", name, f.origin)
            } else {
                name.to_string()
            });
            found = true;
            if !o.all {
                return true;
            }
        }
        if is_builtin(name) {
            line(if o.word {
                format!("{}: builtin", name)
            } else if o.csh {
                format!("{}: shell built-in command", name)
            } else if o.verbose {
                format!("{} is a shell builtin", name)
            } else {
                name.to_string()
            });
            found = true;
            if !o.all {
                return true;
            }
        }
    }
    for p in path_matches(name, o.all) {
        let shown = match o.symlinks {
            1 => whence_links(&p, false),
            2 => whence_links(&p, true),
            _ => p.clone(),
        };
        line(if o.word {
            format!("{}: command", name)
        } else if o.verbose && !o.csh {
            format!("{} is {}", name, shown)
        } else {
            shown
        });
        found = true;
    }
    if !found {
        if o.word {
            line(format!("{}: none", name));
        } else if o.verbose || o.csh {
            line(format!("{} not found", name));
        }
    }
    found
}

/// whence, and which (-c), where (-ca), type (-v).
fn builtin_whence(words: &[String]) -> i32 {
    let cmd = words[0].as_str();
    let mut o = Whence::default();
    match cmd {
        "which" => o.csh = true,
        "where" => {
            o.csh = true;
            o.all = true;
        }
        "type" => o.verbose = true,
        _ => {}
    }
    let mut i = 1;
    while let Some(a) = words.get(i) {
        if a == "--" {
            i += 1;
            break;
        }
        let Some(flags) = a.strip_prefix('-').filter(|f| !f.is_empty()) else {
            break;
        };
        let mut chars = flags.chars();
        while let Some(c) = chars.next() {
            match c {
                'v' => o.verbose = true,
                'c' => o.csh = true,
                'w' => o.word = true,
                'f' => o.funcs = true,
                'a' => o.all = true,
                'p' => o.path_only = true,
                'm' => o.pattern = true,
                's' => o.symlinks = 1,
                'S' => o.symlinks = 2,
                'x' => {
                    let rest: String = chars.by_ref().collect();
                    let n = if rest.is_empty() {
                        i += 1;
                        words.get(i).cloned().unwrap_or_default()
                    } else {
                        rest
                    };
                    match n.parse() {
                        Ok(v) => o.indent = Some(v),
                        Err(_) => return usage(cmd, &format!("bad argument to -x: {}", n)),
                    }
                }
                _ => return usage(cmd, &format!("bad option: -{}", c)),
            }
        }
        i += 1;
    }
    let mut status = 0;
    let mut text = String::new();
    for name in &words[i.min(words.len())..] {
        if o.pattern {
            let mut names: Vec<String> = crate::shell::all_commands()
                .into_iter()
                .chain(with_shell(|s| {
                    s.functions.keys().cloned().collect::<Vec<_>>()
                }))
                .chain(builtin_names().map(|b| b.to_string()))
                .filter(|n| crate::pattern::matches(name, n))
                .collect();
            names.sort();
            names.dedup();
            if names.is_empty() {
                status = 1;
            }
            for n in names {
                whence_one(&n, &o, &mut text);
            }
            continue;
        }
        if !whence_one(name, &o, &mut text) {
            status = 1;
        }
    }
    out(&text);
    status
}

// Only `command -v` / `-V` reach here; plain `command` is handled by the
// executor.
fn builtin_command(words: &[String]) -> i32 {
    let mut verbose = None;
    let mut i = 1;
    while i < words.len() && words[i].starts_with('-') {
        match words[i].as_str() {
            "-v" => verbose = Some(false),
            "-V" => verbose = Some(true),
            "-p" => {}
            "--" => {
                i += 1;
                break;
            }
            _ => break,
        }
        i += 1;
    }
    let Some(verbose) = verbose else {
        return 0;
    };
    // command -V is whence -v.
    if verbose {
        let mut args = vec!["whence".to_string(), "-v".to_string()];
        args.extend(words[i..].iter().cloned());
        return builtin_whence(&args);
    }
    let mut status = 0;
    for name in &words[i..] {
        match describe_command(name, verbose) {
            Some(d) => {
                out(&format!("{}\n", d));
            }
            None => {
                if verbose {
                    builtin_err("command", &format!("{}: not found", name));
                }
                status = 1;
            }
        }
    }
    status
}

// hash [-dfmrvL] [name[=value]] ...: with -d the named directory table,
// otherwise the command table (looked up through PATH here).
fn builtin_hash(words: &[String]) -> i32 {
    let mut flags = String::new();
    let mut i = 1;
    while let Some(a) = words.get(i) {
        let Some(f) = a.strip_prefix('-').filter(|f| !f.is_empty()) else {
            break;
        };
        i += 1;
        if f == "-" {
            break;
        }
        for c in f.chars() {
            if !"dfmrvL".contains(c) {
                builtin_err("hash", &format!("bad option: -{}", c));
                return 1;
            }
        }
        flags.push_str(f);
    }
    let has = |c| flags.contains(c);
    let args = &words[i..];
    if !has('d') {
        if has('r') || has('f') {
            if has('r') {
                with_shell(|s| s.cmd_hash.clear());
            }
            if args.is_empty() {
                return 0;
            }
        }
        let mut status = 0;
        for name in args {
            if let Some((k, v)) = name.split_once('=') {
                with_shell(|s| {
                    s.cmd_hash.insert(k.to_string(), v.to_string());
                });
            } else if crate::shell::lookup_command(name).is_none() && !is_builtin(name) {
                builtin_err("hash", &format!("{}: not found", name));
                status = 1;
            }
        }
        return status;
    }
    let table = &crate::shell::NAMED_DIRS;
    if has('r') || has('f') {
        if has('r') {
            if let Ok(mut t) = table.lock() {
                t.clear();
            }
        }
        if args.is_empty() {
            return 0;
        }
    }
    let line = |k: &str, v: &str| {
        if has('L') {
            format!("hash -d {}={}\n", k, v)
        } else {
            format!("{}={}\n", k, v)
        }
    };
    let all: Vec<(String, String)> = table
        .lock()
        .map(|t| t.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    if args.is_empty() || has('m') {
        let mut text = String::new();
        for (k, v) in &all {
            if args.is_empty() || args.iter().any(|p| crate::pattern::matches(p, k)) {
                text.push_str(&line(k, v));
            }
        }
        out(&text);
        return 0;
    }
    let mut status = 0;
    for a in args {
        match a.split_once('=') {
            Some((name, dir)) => {
                if let Ok(mut t) = table.lock() {
                    t.insert(name.to_string(), dir.to_string());
                }
                if has('v') {
                    out(&line(name, dir));
                }
            }
            None => match crate::shell::named_dir(a) {
                Some(d) => {
                    if has('v') {
                        out(&line(a, &d));
                    }
                }
                None => {
                    builtin_err("hash", &format!("no such directory name: {}", a));
                    status = 1;
                }
            },
        }
    }
    status
}

// ---- processes and signals ------------------------------------------------

// zsh's kill (bin_kill): one signal option, then pids or %jobs; the
// status counts the failures.
fn builtin_kill(words: &[String]) -> i32 {
    let mut sig = libc::SIGTERM;
    let mut i = 1;
    let mut got_sig = false;
    let upper_name = |s: &str| {
        let u = s.to_uppercase();
        u.strip_prefix("SIG").map(str::to_string).unwrap_or(u)
    };
    let lookup = |name: &str| -> Option<i32> {
        if name == "0" {
            return Some(0);
        }
        SIGNALS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, s)| *s)
            .or(match name {
                "IOT" => Some(libc::SIGABRT),
                "POLL" | "IO" => Some(libc::SIGIO),
                "CLD" => Some(libc::SIGCHLD),
                _ => None,
            })
    };
    while let Some(a) = words.get(i) {
        if !a.starts_with('-') || got_sig {
            break;
        }
        let body = &a[1..];
        if body.starts_with(|c: char| c.is_ascii_digit()) {
            match body.parse::<i32>() {
                Ok(n) => sig = n,
                Err(_) => {
                    builtin_err("kill", &format!("invalid signal number: {}", a));
                    return 1;
                }
            }
        } else if body != "-" {
            if body == "l" {
                if words.len() > i + 1 {
                    let mut ret = 0;
                    for arg in &words[i + 1..] {
                        let digits: String =
                            arg.chars().take_while(|c| c.is_ascii_digit()).collect();
                        if digits.is_empty() {
                            let name = upper_name(arg);
                            match lookup(&name) {
                                Some(n) => {
                                    out(&format!("{}\n", n));
                                }
                                None => {
                                    builtin_err("kill", &format!("unknown signal: SIG{}", name));
                                    ret += 1;
                                }
                            }
                        } else if digits.len() < arg.len() {
                            builtin_err(
                                "kill",
                                &format!("unknown signal: SIG{}", arg.to_uppercase()),
                            );
                            ret += 1;
                        } else {
                            let status: i32 = digits.parse().unwrap_or(0);
                            let n = status & !0o200;
                            match SIGNALS.iter().find(|(_, s)| *s == n) {
                                Some((name, _)) => {
                                    out(&format!("{}\n", name));
                                }
                                None => {
                                    out(&format!("{}\n", status));
                                }
                            }
                        }
                    }
                    return ret;
                }
                let names: Vec<&str> = SIGNALS.iter().map(|(n, _)| *n).collect();
                out(&format!("{}\n", names.join(" ")));
                return 0;
            }
            if body == "L" {
                let cols = var_get("COLUMNS").parse::<usize>().unwrap_or(80);
                let cols = if cols >= 30 {
                    if cols < 90 {
                        cols / 15
                    } else {
                        6
                    }
                } else {
                    1
                };
                let mut text = String::new();
                for (k, (name, n)) in SIGNALS.iter().enumerate() {
                    let sep = if (k + 1) % cols == 0 || k + 1 == SIGNALS.len() {
                        '\n'
                    } else {
                        ' '
                    };
                    text.push_str(&format!("{:2} {:<10}{}", n, name, sep));
                }
                out(&text);
                return 0;
            }
            if body == "n" {
                i += 1;
                let Some(n) = words.get(i) else {
                    builtin_err("kill", "-n: argument expected");
                    return 1;
                };
                match n.parse::<i32>() {
                    Ok(v) => sig = v,
                    Err(_) => {
                        builtin_err("kill", &format!("invalid signal number: {}", n));
                        return 1;
                    }
                }
            } else {
                let name = if body == "s" {
                    i += 1;
                    match words.get(i) {
                        Some(n) => n.clone(),
                        None => {
                            builtin_err("kill", "-s: argument expected");
                            return 1;
                        }
                    }
                } else {
                    body.to_string()
                };
                if name.is_empty() {
                    builtin_err("kill", "-: signal name expected");
                    return 1;
                }
                let name = upper_name(&name);
                match lookup(&name) {
                    Some(n) => sig = n,
                    None => {
                        builtin_err("kill", &format!("unknown signal: SIG{}", name));
                        builtin_err("kill", "type kill -L for a list of signals");
                        return 1;
                    }
                }
            }
        }
        i += 1;
        got_sig = true;
    }
    if words
        .get(i)
        .is_some_and(|a| a == "-" || a.starts_with("--"))
    {
        i += 1;
    }
    if i >= words.len() {
        builtin_err("kill", "not enough arguments");
        return 1;
    }
    let mut ret = 0;
    for spec in &words[i..] {
        if spec.starts_with('%') {
            match kill_targets(spec) {
                Ok(pids) => {
                    for pid in pids {
                        if unsafe { libc::kill(pid, sig) } != 0 {
                            let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                            builtin_err(
                                "kill",
                                &format!("kill {} failed: {}", spec, strerror(e).to_lowercase()),
                            );
                            ret += 1;
                        }
                    }
                }
                Err(e) => {
                    builtin_err("kill", &e.to_string());
                    ret += 1;
                }
            }
            continue;
        }
        let num = spec.strip_prefix('-').unwrap_or(spec);
        if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) {
            builtin_err("kill", &format!("invalid pid: {}", spec));
            ret += 1;
            continue;
        }
        // A pid too big for pid_t can't name a process; reading it as 0
        // would signal the shell's own process group.
        let Ok(pid) = spec.parse::<libc::pid_t>() else {
            builtin_err("kill", &format!("kill {} failed: no such process", spec));
            ret += 1;
            continue;
        };
        if unsafe { libc::kill(pid, sig) } != 0 {
            let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            builtin_err(
                "kill",
                &format!("kill {} failed: {}", spec, strerror(e).to_lowercase()),
            );
            ret += 1;
        }
    }
    if ret < 126 {
        ret
    } else {
        1
    }
}

fn builtin_trap(words: &[String]) -> i32 {
    let mut args = &words[1..];
    if args.first().map(|s| s.as_str()) == Some("--") {
        args = &args[1..];
    }
    if args.is_empty() || args[0] == "-p" {
        let mut traps: Vec<(i32, String)> =
            with_shell(|s| s.traps.iter().map(|(k, v)| (*k, v.clone())).collect());
        traps.sort();
        let mut text = String::new();
        for (sig, a) in &traps {
            // A TRAPxxx function is shown as its definition.
            let fname = format!("TRAP{}", signal_name(*sig));
            let func = (a.split(' ').next() == Some(fname.as_str()))
                .then(|| with_shell(|s| s.functions.get(&fname).map(|f| f.body.clone())))
                .flatten();
            match func {
                Some(body) => text.push_str(&crate::deparse::function_text(&fname, &body, None)),
                None => text.push_str(&format!(
                    "trap -- {} {}\n",
                    shell_quote(a),
                    signal_name(*sig)
                )),
            }
        }
        out(&text);
        return 0;
    }
    // `trap -` alone removes every trap.
    if args.len() == 1 && args[0] == "-" {
        let sigs: Vec<i32> = with_shell(|s| s.traps.keys().copied().collect());
        for sig in sigs {
            set_trap(sig, None);
        }
        return 0;
    }
    if args[0] == "-l" {
        let text: String = SIGNALS
            .iter()
            .map(|(n, s)| format!("{}) SIG{}\n", s, n))
            .collect();
        out(&text);
        return 0;
    }
    let first_is_number = args[0].parse::<u32>().is_ok();
    let (action, conds): (Option<String>, &[String]) = if first_is_number || args.len() == 1 {
        (None, args)
    } else if args[0] == "-" {
        (None, &args[1..])
    } else {
        (Some(args[0].clone()), &args[1..])
    };
    let mut status = 0;
    for c in conds {
        match signal_number(c) {
            Some(sig) if sig == libc::SIGKILL || sig == libc::SIGSTOP => {
                builtin_err("trap", &format!("{}: cannot trap", c));
                status = 1;
            }
            Some(sig) => set_trap(sig, action.clone()),
            None => {
                builtin_err("trap", &format!("undefined signal: {}", c));
                status = 1;
            }
        }
    }
    status
}

fn fmt_time(tv: libc::timeval) -> String {
    let secs = tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    format!("{}m{:.3}s", (secs / 60.0) as i64, secs % 60.0)
}

fn builtin_times() -> i32 {
    let mut me: libc::rusage = unsafe { std::mem::zeroed() };
    let mut kids: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe {
        libc::getrusage(libc::RUSAGE_SELF, &mut me);
        libc::getrusage(libc::RUSAGE_CHILDREN, &mut kids);
    }
    out(&format!(
        "{} {}\n{} {}\n",
        fmt_time(me.ru_utime),
        fmt_time(me.ru_stime),
        fmt_time(kids.ru_utime),
        fmt_time(kids.ru_stime)
    ));
    0
}

fn builtin_umask(words: &[String]) -> i32 {
    let mut symbolic = false;
    let mut args = Vec::new();
    for a in &words[1..] {
        if a == "-S" {
            symbolic = true;
        } else {
            args.push(a.clone());
        }
    }
    let current = unsafe {
        let m = libc::umask(0);
        libc::umask(m);
        m as u32
    };
    let Some(spec) = args.first() else {
        if symbolic {
            let perms = |shift: u32| {
                let bits = !(current >> shift) & 7;
                let mut s = String::new();
                if bits & 4 != 0 {
                    s.push('r');
                }
                if bits & 2 != 0 {
                    s.push('w');
                }
                if bits & 1 != 0 {
                    s.push('x');
                }
                s
            };
            out(&format!("u={},g={},o={}\n", perms(6), perms(3), perms(0)));
        } else {
            out(&format!("{:04o}\n", current));
        }
        return 0;
    };
    if spec.chars().all(|c| c.is_digit(8)) {
        match u32::from_str_radix(spec, 8) {
            Ok(m) if m <= 0o777 => {
                unsafe {
                    libc::umask(m as libc::mode_t);
                }
                return 0;
            }
            _ => return usage("umask", &format!("{}: octal number out of range", spec)),
        }
    }
    // Symbolic mode: [ugoa]*[+-=][rwx]*, comma separated, applied to the
    // permissions the mask allows.
    let mut allowed = !current & 0o777;
    for clause in spec.split(',') {
        let who_end = clause.find(['+', '-', '=']).unwrap_or(clause.len());
        let who = &clause[..who_end];
        if who_end >= clause.len() || !who.chars().all(|c| "ugoa".contains(c)) {
            return usage("umask", &format!("{}: invalid symbolic mode", spec));
        }
        let mut mask_who = 0;
        for c in who.chars() {
            mask_who |= match c {
                'u' => 0o700,
                'g' => 0o070,
                'o' => 0o007,
                _ => 0o777,
            };
        }
        if who.is_empty() {
            mask_who = 0o777;
        }
        let op = clause.as_bytes()[who_end] as char;
        let mut bits = 0;
        for c in clause[who_end + 1..].chars() {
            bits |= match c {
                'r' => 0o444,
                'w' => 0o222,
                'x' => 0o111,
                _ => return usage("umask", &format!("{}: invalid symbolic mode", spec)),
            };
        }
        bits &= mask_who;
        match op {
            '+' => allowed |= bits,
            '-' => allowed &= !bits,
            _ => allowed = (allowed & !mask_who) | bits,
        }
    }
    unsafe {
        libc::umask((!allowed & 0o777) as libc::mode_t);
    }
    0
}

// zsh/rlimits: each resource with its `limit` name, `ulimit` letter,
// unit and description, in RLIMIT_* order.
#[derive(Clone, Copy, PartialEq)]
enum LimKind {
    Time,
    Blocks,
    Kbytes,
    Number,
    #[cfg(target_os = "linux")]
    Micro,
}

const LIMITS: &[(&str, char, LimKind, &str, libc::c_int)] = &[
    (
        "cputime",
        't',
        LimKind::Time,
        "cpu time (seconds)",
        libc::RLIMIT_CPU as _,
    ),
    (
        "filesize",
        'f',
        LimKind::Blocks,
        "file size (blocks)",
        libc::RLIMIT_FSIZE as _,
    ),
    (
        "datasize",
        'd',
        LimKind::Kbytes,
        "data seg size (kbytes)",
        libc::RLIMIT_DATA as _,
    ),
    (
        "stacksize",
        's',
        LimKind::Kbytes,
        "stack size (kbytes)",
        libc::RLIMIT_STACK as _,
    ),
    (
        "coredumpsize",
        'c',
        LimKind::Blocks,
        "core file size (blocks)",
        libc::RLIMIT_CORE as _,
    ),
    (
        "resident",
        'm',
        LimKind::Kbytes,
        "resident set size (kbytes)",
        libc::RLIMIT_RSS as _,
    ),
    (
        "maxproc",
        'u',
        LimKind::Number,
        "processes",
        libc::RLIMIT_NPROC as _,
    ),
    (
        "descriptors",
        'n',
        LimKind::Number,
        "file descriptors",
        libc::RLIMIT_NOFILE as _,
    ),
    (
        "memorylocked",
        'l',
        LimKind::Kbytes,
        "locked-in-memory size (kbytes)",
        libc::RLIMIT_MEMLOCK as _,
    ),
    (
        "addressspace",
        'v',
        LimKind::Kbytes,
        "address space (kbytes)",
        libc::RLIMIT_AS as _,
    ),
    #[cfg(target_os = "linux")]
    (
        "maxfilelocks",
        'x',
        LimKind::Number,
        "file locks",
        libc::RLIMIT_LOCKS as _,
    ),
    #[cfg(target_os = "linux")]
    (
        "sigpending",
        'i',
        LimKind::Number,
        "pending signals",
        libc::RLIMIT_SIGPENDING as _,
    ),
    #[cfg(target_os = "linux")]
    (
        "msgqueue",
        'q',
        LimKind::Number,
        "bytes in POSIX msg queues",
        libc::RLIMIT_MSGQUEUE as _,
    ),
    #[cfg(target_os = "linux")]
    (
        "nice",
        'e',
        LimKind::Number,
        "max nice",
        libc::RLIMIT_NICE as _,
    ),
    #[cfg(target_os = "linux")]
    (
        "rt_priority",
        'r',
        LimKind::Number,
        "max rt priority",
        libc::RLIMIT_RTPRIO as _,
    ),
    #[cfg(target_os = "linux")]
    (
        "rt_time",
        'N',
        LimKind::Micro,
        "rt cpu time (microseconds)",
        libc::RLIMIT_RTTIME as _,
    ),
];

fn get_limit(res: usize, hard: bool) -> Option<u64> {
    let entry = LIMITS.get(res)?;
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(entry.4 as _, &mut lim) } != 0 {
        return None;
    }
    Some(if hard { lim.rlim_max } else { lim.rlim_cur } as u64)
}

fn set_limit(name: &str, res: usize, hard: bool, soft: bool, val: u64) -> bool {
    let Some(entry) = LIMITS.get(res) else {
        builtin_err(name, &format!("invalid resource number: {}", res));
        return false;
    };
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(entry.4 as _, &mut lim) } != 0 {
        return false;
    }
    if hard {
        lim.rlim_max = val as libc::rlim_t;
    }
    if soft {
        lim.rlim_cur = val as libc::rlim_t;
    }
    if unsafe { libc::setrlimit(entry.4 as _, &lim) } != 0 {
        let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        builtin_err(
            name,
            &format!("setrlimit failed: {}", strerror(e).to_lowercase()),
        );
        return false;
    }
    true
}

// ulimit's unit for a resource (what one counted unit is in the raw limit).
fn ulimit_unit(kind: LimKind) -> u64 {
    match kind {
        LimKind::Blocks => 512,
        LimKind::Kbytes => 1024,
        _ => 1,
    }
}

fn ulimit_value(v: u64, kind: LimKind) -> String {
    if v == libc::RLIM_INFINITY {
        "unlimited".to_string()
    } else {
        (v / ulimit_unit(kind)).to_string()
    }
}

// zsh's ulimit: options pick resources (several allowed), -H/-S hard or
// soft, -a all; an argument after a resource sets it.
fn builtin_ulimit(words: &[String]) -> i32 {
    let (mut hard, mut soft, mut all) = (false, false, false);
    let mut chosen: Vec<(usize, Option<String>)> = Vec::new();
    let mut i = 1;
    while let Some(w) = words.get(i) {
        i += 1;
        let Some(body) = w.strip_prefix('-').filter(|b| !b.is_empty()) else {
            // A value for the last resource (or the default -f).
            if chosen.is_empty() {
                chosen.push((1, None));
            }
            chosen.last_mut().unwrap().1 = Some(w.clone());
            continue;
        };
        let chars: Vec<char> = body.chars().collect();
        let mut k = 0;
        while k < chars.len() {
            let c = chars[k];
            k += 1;
            match c {
                'H' => hard = true,
                'S' => soft = true,
                'a' => all = true,
                'N' => {
                    let rest: String = chars[k..].iter().collect();
                    let spec = if !rest.is_empty() {
                        k = chars.len();
                        rest
                    } else if let Some(n) = words.get(i) {
                        i += 1;
                        n.clone()
                    } else {
                        builtin_err("ulimit", "number required after -N");
                        return 1;
                    };
                    match spec.parse::<usize>() {
                        Ok(n) if n < LIMITS.len() => chosen.push((n, None)),
                        Ok(_) => {
                            builtin_err("ulimit", &format!("no such limit: {}", spec));
                            return 1;
                        }
                        Err(_) => {
                            builtin_err("ulimit", &format!("invalid number: {}", spec));
                            return 1;
                        }
                    }
                }
                c => match LIMITS.iter().position(|l| l.1 == c) {
                    Some(r) => chosen.push((r, None)),
                    None => {
                        builtin_err("ulimit", &format!("bad option: -{}", c));
                        return 1;
                    }
                },
            }
        }
    }
    if all {
        if !chosen.is_empty() {
            builtin_err("ulimit", "no limits allowed with -a");
            return 1;
        }
        let mut text = String::new();
        for (r, l) in LIMITS.iter().enumerate() {
            let opt = if l.1 == 'N' {
                format!("-N {}", r)
            } else {
                format!("-{}", l.1)
            };
            let head = format!("{}: {}", opt, l.3);
            let v = get_limit(r, hard)
                .map(|v| ulimit_value(v, l.2))
                .unwrap_or_default();
            text.push_str(&format!("{:<36}{}\n", head, v));
        }
        out(&text);
        return 0;
    }
    if chosen.is_empty() {
        chosen.push((1, None));
    }
    let many = chosen.len() > 1;
    let mut ret = 0;
    for (r, val) in chosen {
        let (kind, desc) = LIMITS
            .get(r)
            .map_or((LimKind::Number, "unknown"), |l| (l.2, l.3));
        match val {
            None => {
                let v = get_limit(r, hard)
                    .map(|v| ulimit_value(v, kind))
                    .unwrap_or_default();
                if many {
                    let opt = LIMITS
                        .get(r)
                        .map_or(format!("-N {}", r), |l| format!("-{}", l.1));
                    out(&format!("{:<36}{}\n", format!("{}: {}", opt, desc), v));
                } else {
                    out(&format!("{}\n", v));
                }
            }
            Some(v) => {
                let new = if v == "unlimited" {
                    libc::RLIM_INFINITY
                } else {
                    match v.parse::<u64>() {
                        Ok(n) => n.saturating_mul(ulimit_unit(kind)),
                        Err(_) => {
                            builtin_err("ulimit", &format!("invalid number: {}", v));
                            return 1;
                        }
                    }
                };
                let (h, s) = if hard || soft {
                    (hard, soft)
                } else {
                    (true, true)
                };
                if !set_limit("ulimit", r, h, s, new) {
                    ret = 1;
                }
            }
        }
    }
    ret
}

// `limit` / `unlimit` value text: times as h:mm:ss, sizes in kB or MB.
fn limit_text(v: u64, kind: LimKind) -> String {
    if v == libc::RLIM_INFINITY {
        return "unlimited".to_string();
    }
    match kind {
        LimKind::Time => format!("{}:{:02}:{:02}", v / 3600, (v / 60) % 60, v % 60),
        LimKind::Blocks | LimKind::Kbytes => {
            if v >= 1024 * 1024 {
                format!("{}MB", v / (1024 * 1024))
            } else {
                format!("{}kB", v / 1024)
            }
        }
        #[cfg(target_os = "linux")]
        LimKind::Micro => format!("{}us", v),
        LimKind::Number => v.to_string(),
    }
}

// limit [-hs] [resource [value]] ...
fn builtin_limit(words: &[String]) -> i32 {
    let mut hard = false;
    let mut args = &words[1..];
    while let Some(a) = args.first().filter(|a| a.starts_with('-') && a.len() > 1) {
        if a.contains('h') {
            hard = true;
        }
        args = &args[1..];
    }
    let lookup = |n: &str| {
        LIMITS
            .iter()
            .position(|l| l.0.starts_with(n) && !n.is_empty())
    };
    if args.is_empty() {
        let mut text = String::new();
        for (r, l) in LIMITS.iter().enumerate() {
            let v = get_limit(r, hard)
                .map(|v| limit_text(v, l.2))
                .unwrap_or_default();
            text.push_str(&format!("{:<16}{}\n", l.0, v));
        }
        out(&text);
        return 0;
    }
    let mut k = 0;
    let mut ret = 0;
    while k < args.len() {
        let Some(r) = lookup(&args[k]) else {
            builtin_err("limit", &format!("no such resource: {}", args[k]));
            return 1;
        };
        let kind = LIMITS[r].2;
        match args.get(k + 1) {
            None => {
                let v = get_limit(r, hard)
                    .map(|v| limit_text(v, kind))
                    .unwrap_or_default();
                out(&format!("{:<16}{}\n", LIMITS[r].0, v));
                k += 1;
            }
            Some(v) => {
                let val = if v == "unlimited" {
                    libc::RLIM_INFINITY
                } else {
                    let digits: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
                    let n: u64 = match digits.parse() {
                        Ok(n) => n,
                        Err(_) => {
                            builtin_err("limit", &format!("limit must be a number: {}", v));
                            return 1;
                        }
                    };
                    let suffix = v[digits.len()..].to_ascii_lowercase();
                    match kind {
                        LimKind::Time => match suffix.as_str() {
                            "h" => n * 3600,
                            "m" => n * 60,
                            _ => n,
                        },
                        LimKind::Blocks | LimKind::Kbytes => match suffix.as_str() {
                            "m" | "mb" => n * 1024 * 1024,
                            "g" | "gb" => n * 1024 * 1024 * 1024,
                            _ => n * 1024,
                        },
                        _ => n,
                    }
                };
                if !set_limit("limit", r, hard, !hard, val) {
                    ret = 1;
                }
                k += 2;
            }
        }
    }
    ret
}

// unlimit [-h] [resource ...]: back to unlimited (as far as allowed).
fn builtin_unlimit(words: &[String]) -> i32 {
    let mut hard = false;
    let mut args = &words[1..];
    while let Some(a) = args.first().filter(|a| a.starts_with('-') && a.len() > 1) {
        if a.contains('h') {
            hard = true;
        }
        args = &args[1..];
    }
    let targets: Vec<usize> = if args.is_empty() {
        (0..LIMITS.len()).collect()
    } else {
        let mut v = Vec::new();
        for a in args {
            match LIMITS.iter().position(|l| l.0.starts_with(a.as_str())) {
                Some(r) => v.push(r),
                None => {
                    builtin_err("unlimit", &format!("no such resource: {}", a));
                    return 1;
                }
            }
        }
        v
    };
    let mut ret = 0;
    for r in targets {
        let to = if hard {
            libc::RLIM_INFINITY
        } else {
            get_limit(r, true).unwrap_or(libc::RLIM_INFINITY)
        };
        if !set_limit("unlimit", r, hard, true, to) {
            ret = 1;
        }
    }
    ret
}

// ---- zsh compatibility ----------------------------------------------------

// Best-effort autoload: for each function name, sources a file of that name
// from $fpath; names that are not found become no-op functions so later
// calls do not fail.
// autoload [-UXzk] [+X] name...: marks functions to be loaded from the
// first file of that name in $fpath when first called. As in zsh, the
// file's contents are the function body. `+X` loads immediately.
// Names that are meowsh builtins are left alone so the builtin runs.
// The stub an autoloaded function has until it is loaded: it loads
// the definition (from `dir` when given, `-d` also from $fpath) and runs
// it, as zsh's `autoload -X` does.
fn autoload_stub(dir: Option<&str>, default_too: bool) -> String {
    let mut stub = String::from("builtin autoload -X");
    if default_too {
        stub.push_str(" -d");
    }
    if let Some(d) = dir {
        stub.push(' ');
        stub.push_str(&shell_quote(d));
    }
    stub
}

pub fn is_autoload_stub(name: &str) -> bool {
    with_shell(|s| {
        s.functions.get(name).is_some_and(|f| {
            crate::deparse::Deparser::new(None)
                .body_text(&f.body)
                .trim_start_matches(['{', ' ', '\n', '\t'])
                .starts_with("builtin autoload -X")
        })
    })
}

fn define_stub(name: &str, dir: Option<&str>, default_too: bool) {
    let stub = autoload_stub(dir, default_too);
    let mut p = crate::parser::Parser::without_aliases(&stub, true);
    if let Ok(Some(body)) = p.parse_complete_command() {
        with_shell(|s| {
            s.functions.insert(
                name.to_string(),
                FuncDef {
                    body: Arc::new(body),
                    // Known only when autoloaded by its path.
                    origin: dir.map(|d| format!("{}/{}", d, name)).unwrap_or_default(),
                    line: 0,
                },
            )
        });
        // Under `emulate -c`, the function loads and runs in that emulation.
        crate::options::set_sticky(name);
        // An autoloaded TRAPxxx sets its trap now, as a defined one does.
        crate::trap::function_defined(name);
    }
}

// compinit and friends: zsh's functions when $fpath has them (then
// they replace these builtins), otherwise nothing, so startup files that
// call them still work without zsh's function library.
fn run_fpath_function(words: &[String]) -> i32 {
    let name = &words[0];
    if fpath_dir(name).is_none() || !autoload_now(name, None, true) {
        return 0;
    }
    let Some((body, line)) =
        with_shell(|s| s.functions.get(name).map(|f| (f.body.clone(), f.line)))
    else {
        return 0;
    };
    call_function(&body, words, line)
}

// The directory of $fpath holding `name`, made absolute.
fn fpath_dir(name: &str) -> Option<String> {
    let fpath = with_shell(|s| s.array("fpath").cloned()).unwrap_or_default();
    fpath.iter().find_map(|d| {
        let p = std::path::Path::new(d).join(name);
        p.is_file().then(|| {
            let dir = std::path::Path::new(d);
            let abs = if dir.is_absolute() {
                dir.to_path_buf()
            } else {
                std::env::current_dir().unwrap_or_default().join(dir)
            };
            abs.to_string_lossy().into_owned()
        })
    })
}

fn builtin_autoload(words: &[String]) -> i32 {
    let (mut load_now, mut exec_now, mut default_too) = (false, false, false);
    let (mut resolve, mut strict) = (false, false);
    let mut i = 1;
    while let Some(a) = words.get(i) {
        if a == "+X" {
            load_now = true;
        } else if let Some(f) = a.strip_prefix('-').filter(|f| !f.is_empty()) {
            for c in f.chars() {
                match c {
                    'X' => exec_now = true,
                    'd' => default_too = true,
                    'r' => resolve = true,
                    'R' => {
                        resolve = true;
                        strict = true;
                    }
                    _ => {}
                }
            }
        } else if !a.starts_with('+') {
            break;
        }
        i += 1;
    }
    let args = &words[i..];
    if exec_now {
        return autoload_exec(args.first().map(|s| s.as_str()), default_too);
    }
    let mut status = 0;
    for arg in args {
        // `autoload /dir/name`: name loads from dir.
        let (name, dir) = match arg.rsplit_once('/') {
            Some((d, n)) if arg.starts_with('/') => (n.to_string(), Some(d.to_string())),
            _ => (arg.clone(), None),
        };
        if is_builtin(&name) {
            continue;
        }
        if load_now {
            if !autoload_now(&name, dir.as_deref(), default_too || dir.is_none()) {
                shell_err(&format!("{}: function definition file not found", name));
                status = 1;
            }
            continue;
        }
        let defined = with_shell(|s| s.functions.contains_key(&name));
        if defined && !is_autoload_stub(&name) {
            continue;
        }
        if dir.is_some() {
            define_stub(&name, dir.as_deref(), default_too);
            continue;
        }
        if defined {
            // A bare autoload keeps a stub with a path.
            continue;
        }
        if resolve {
            match fpath_dir(&name) {
                Some(d) => define_stub(&name, Some(&d), false),
                None if strict => {
                    shell_err(&format!("{}: function definition file not found", name));
                    status = 1;
                }
                None => define_stub(&name, None, false),
            }
            continue;
        }
        define_stub(&name, None, false);
    }
    status
}

// `autoload -X [dir]` in a function: loads the running function's
// definition, replacing it, and runs it with the same arguments.
fn autoload_exec(dir: Option<&str>, default_too: bool) -> i32 {
    let (name, args) = with_shell(|s| (s.argv0.clone(), s.pos_params.clone()));
    if !autoload_now(&name, dir, default_too || dir.is_none()) {
        // A file that did not parse has been reported already.
        if AUTOLOAD_PARSE_FAILED.with(|f| f.replace(false)) {
            return 1;
        }
        // Reported under the caller's name, with the line in the function
        // (zsh's own numbering here).
        // An autoload stub of our own has no lines: the caller's line.
        let stub = is_autoload_stub(&name);
        let top = crate::shell::with_err_ctx(|e| {
            let top = e.stack.pop();
            let caller_base = e.stack.last().map_or(0, |f| f.1);
            let saved = e.lineno;
            let call_line = e
                .calls
                .last()
                .and_then(|c| c.0.rsplit_once(':'))
                .and_then(|(_, l)| l.parse::<usize>().ok());
            match (&top, call_line) {
                (Some(_), Some(l)) if stub => e.lineno = caller_base + l,
                // zsh counts from 1 here, even on the definition's line.
                (Some((_, base)), _) => {
                    e.lineno = caller_base + e.lineno.saturating_sub(*base).max(1)
                }
                _ => {}
            }
            (top, saved)
        });
        shell_err(&format!("{}: function definition file not found", name));
        crate::shell::with_err_ctx(|e| {
            e.lineno = top.1;
            if let Some(t) = top.0 {
                e.stack.push(t);
            }
        });
        return 1;
    }
    let Some(body) = with_shell(|s| s.functions.get(&name).map(|f| f.body.clone())) else {
        return 1;
    };
    // Functions it defines come from its file.
    let origin = with_shell(|s| s.functions.get(&name).map(|f| f.origin.clone()));
    // The stub's call now runs the loaded function: $funcsourcetrace
    // names its file.
    if let Some(o) = &origin {
        crate::shell::with_err_ctx(|e| {
            if let Some(c) = e.calls.last_mut() {
                c.2 = format!("{}:0", o);
            }
        });
    }
    let saved = with_shell(|s| std::mem::replace(&mut s.source_file, origin));
    let mut words = vec![name.clone()];
    words.extend(args);
    let st = if crate::options::is_set("kshautoload") {
        // KSH_AUTOLOAD: the file is run, and should define the function,
        // which is then called.
        crate::exec::exec_node(&body);
        let defined = with_shell(|s| {
            s.functions
                .get(&name)
                .filter(|f| !Arc::ptr_eq(&f.body, &body))
                .map(|f| (f.body.clone(), f.line))
        });
        match defined {
            Some((b, l)) => call_function(&b, &words, l),
            None => last_status(),
        }
    } else {
        // The definition runs in the stub's own call, as in zsh, so it is
        // as deep as later calls (comptags keys its state by depth).
        let inner = match body.as_ref() {
            Node::Brace(b) => b.as_ref(),
            b => b,
        };
        crate::exec::exec_node(inner)
    };
    with_shell(|s| s.source_file = saved);
    st
}

// Loads `name`'s definition from `dir` (or $fpath when `fpath_too`).
// A file holding just a definition of `name` gives that definition's
// body; otherwise the whole file is the body.
fn autoload_now(name: &str, dir: Option<&str>, fpath_too: bool) -> bool {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(d) = dir {
        candidates.push(std::path::Path::new(d).join(name));
    }
    if fpath_too {
        let fpath = with_shell(|s| s.array("fpath").cloned()).unwrap_or_default();
        candidates.extend(fpath.iter().map(|d| std::path::Path::new(d).join(name)));
    }
    let Some((path, content)) = candidates.into_iter().find_map(|p| {
        std::fs::read_to_string(&p)
            .ok()
            .map(|c| (p.to_string_lossy().into_owned(), c))
    }) else {
        return false;
    };
    let mut parser = crate::parser::Parser::new(&content, true);
    let nodes = match parser.parse_program() {
        Ok(nodes) => nodes,
        Err(e) => {
            let msg = match e {
                crate::parser::ParseError::Syntax(m) => m,
                crate::parser::ParseError::Incomplete => "unexpected end of file".to_string(),
            };
            // zsh reports it as the function's, at the line in its file.
            let line = parser.line();
            let saved = crate::shell::with_err_ctx(|e| {
                let base = e.stack.last().map_or(0, |f| f.1);
                std::mem::replace(&mut e.lineno, base + line)
            });
            shell_err(&msg);
            crate::shell::with_err_ctx(|e| e.lineno = saved);
            AUTOLOAD_PARSE_FAILED.with(|f| f.set(true));
            return false;
        }
    };
    let ksh = crate::options::is_set("kshautoload");
    let body = match nodes.as_slice() {
        [Node::FuncDef { name: n, body, .. }] if n == name && !ksh => (**body).clone(),
        _ => Node::List(
            nodes
                .into_iter()
                .map(|node| crate::types::ListItem {
                    node,
                    background: false,
                })
                .collect(),
        ),
    };
    let origin = if path.starts_with('/') {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(path.trim_start_matches("./"))
            .to_string_lossy()
            .into_owned()
    };
    with_shell(|s| {
        s.functions.insert(
            name.to_string(),
            FuncDef {
                body: Arc::new(body),
                origin,
                line: 0,
            },
        )
    });
    true
}

fn builtin_setopt(words: &[String], value: bool) -> i32 {
    let name = &words[0];
    let args = &words[1..];
    if args.is_empty() {
        // Options that differ from the defaults, as zsh lists them: the
        // name for setopt; with a `no` prefix for unsetopt.
        let emu = with_shell(|s| s.emulation);
        let mut text = String::new();
        // KSH_OPTION_PRINT: every option with its state, as zsh's
        // printoptionnodestate shows them.
        if crate::options::is_set("kshoptionprint") {
            for o in crate::options::names() {
                let on = crate::options::is_set(o);
                if crate::options::default_in(o, emu) {
                    text.push_str(&format!("no{:<19} {}\n", o, if on { "off" } else { "on" }));
                } else {
                    text.push_str(&format!("{:<21} {}\n", o, if on { "on" } else { "off" }));
                }
            }
            out(&text);
            return 0;
        }
        // setopt lists options changed from their defaults, unsetopt the
        // rest; either way an option that is off shows as `noname` when it
        // is on by default.
        for o in crate::options::names() {
            let on = crate::options::is_set(o);
            let default = crate::options::default_in(o, emu);
            if (on != default) != value {
                continue;
            }
            if on != default {
                if on {
                    text.push_str(&format!("{}\n", o));
                } else {
                    text.push_str(&format!("no{}\n", o));
                }
            } else if on {
                text.push_str(&format!("no{}\n", o));
            } else {
                text.push_str(&format!("{}\n", o));
            }
        }
        out(&text);
        return 0;
    }
    let mut status = 0;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-o" || arg == "+o" {
            let on = if arg == "-o" { value } else { !value };
            match args.get(i + 1) {
                Some(o) => {
                    if let Some(c) = crate::options::refused(o, on) {
                        builtin_err(name, &format!("can't change option: {}", c));
                        status = 1;
                    } else if !crate::options::set(o, on) {
                        builtin_err(name, &format!("no such option: {}", o));
                        status = 1;
                    }
                }
                None => return usage(name, "-o requires an argument"),
            }
            i += 2;
            continue;
        }
        if (arg.starts_with('-') || arg.starts_with('+')) && arg.len() > 1 {
            // Letters: -X sets the option, +X unsets it (inverted for unsetopt).
            let on = arg.starts_with('-') == value;
            for c in arg[1..].chars() {
                match crate::options::letter(c) {
                    Some((o, v)) => {
                        let state = if on { v } else { !v };
                        if let Some(c) = crate::options::refused(o, state) {
                            builtin_err(name, &format!("can't change option: {}", c));
                            status = 1;
                        } else {
                            crate::options::set(o, state);
                        }
                    }
                    None => {
                        builtin_err(name, &format!("bad option: -{}", c));
                        status = 1;
                    }
                }
            }
        } else if let Some(c) = crate::options::refused(arg, value) {
            builtin_err(name, &format!("can't change option: {}", c));
            status = 1;
        } else if !crate::options::set(arg, value) {
            builtin_err(name, &format!("no such option: {}", arg));
            status = 1;
        }
        i += 1;
    }
    status
}

fn toggle_named_opt(name: &str, value: bool) {
    crate::options::set(name, value);
}

/// Calls a shell function by name if one is defined (used for hooks).
const HOOKS: &[&str] = &[
    "chpwd",
    "precmd",
    "preexec",
    "periodic",
    "zshaddhistory",
    "zshexit",
];

/// Runs zsh hook `hook`: the function of that name, then each function
/// named in `${hook}_functions`. Each sees the `$?` and `$pipestatus`
/// from before the hooks, and both are restored afterwards.
pub fn run_hooks(hook: &str, args: &[String]) {
    let mut names = vec![hook.to_string()];
    let list = format!("{}_functions", hook);
    names.extend(with_shell(|s| s.array(&list).cloned()).unwrap_or_default());
    let saved = with_shell(|s| (s.last_status, s.array("pipestatus").cloned()));
    let restore = || {
        with_shell(|s| {
            s.last_status = saved.0;
            match &saved.1 {
                Some(p) => s.store("pipestatus", crate::params::Value::Array(p.clone())),
                None => {
                    s.remove_param("pipestatus");
                }
            };
        })
    };
    for name in names {
        let Some((body, line)) =
            with_shell(|s| s.functions.get(&name).map(|f| (f.body.clone(), f.line)))
        else {
            continue;
        };
        restore();
        let mut words = vec![name];
        words.extend(args.iter().cloned());
        call_function(&body, &words, line);
        if matches!(crate::exec::flow(), Flow::Exit(_)) {
            return;
        }
        set_flow(Flow::None);
    }
    restore();
}

// fc -l [-n] [-r] [-d|-f|-E|-i|-t fmt] [first [last]]: lists history.
// Other forms (editing, -p/-P/-R/-W/-A history files) are accepted and
// do nothing.
fn builtin_fc(words: &[String]) -> i32 {
    let (mut list, mut numbers, mut reverse) = (false, true, false);
    let (mut push, mut pop) = (false, false);
    let (mut write, mut read, mut append) = (false, false, false);
    let mut range = Vec::new();
    let mut i = 1;
    while let Some(a) = words.get(i) {
        if a == "--" {
            range.extend(words[i + 1..].iter().cloned());
            break;
        }
        match a
            .strip_prefix('-')
            .filter(|f| !f.is_empty() && !f.starts_with(|c: char| c.is_ascii_digit()))
        {
            Some(flags) => {
                for c in flags.chars() {
                    match c {
                        'l' => list = true,
                        'n' => numbers = false,
                        'r' => reverse = true,
                        'p' => push = true,
                        'P' => pop = true,
                        'W' => write = true,
                        'R' => read = true,
                        'A' => append = true,
                        't' => i += 1,
                        _ => {}
                    }
                }
            }
            None if a.is_empty() => {}
            None => range.push(a.clone()),
        }
        i += 1;
    }
    let history = || with_shell(|s| s.history.lock().unwrap_or_else(|e| e.into_inner()).clone());
    let set_history =
        |h: Vec<String>| with_shell(|s| *s.history.lock().unwrap_or_else(|e| e.into_inner()) = h);
    // fc -p [file [histsize [savehist]]]: a new history, the old one kept
    // on a stack for fc -P; the sizes must be integers.
    if push {
        for (arg, var) in range.iter().skip(1).zip(["HISTSIZE", "SAVEHIST"]) {
            if arg.parse::<i64>().is_err() {
                builtin_err("fc", &format!("{} must be an integer", var));
                return 1;
            }
        }
        let file = var_get_opt("HISTFILE");
        if let Ok(mut st) = HIST_STACK.lock() {
            st.push((history(), file));
        }
        // The command running now stays in the old history.
        crate::exec::LINE_IN_HISTORY.store(false, std::sync::atomic::Ordering::Relaxed);
        set_history(Vec::new());
        if let Some(f) = range.first() {
            crate::shell::var_set("HISTFILE", f, false);
            if let Ok(h) = crate::history::read_zsh_file(f) {
                set_history(h);
            }
        }
        if !read && !write && !append {
            return 0;
        }
    }
    if pop {
        let Some((h, file)) = HIST_STACK.lock().ok().and_then(|mut s| s.pop()) else {
            builtin_err("fc", "history stack empty");
            return 1;
        };
        set_history(h);
        match file {
            Some(f) => {
                crate::shell::var_set("HISTFILE", &f, false);
            }
            None => {
                with_shell(|s| s.remove_param("HISTFILE"));
            }
        }
        return 0;
    }
    // fc -W / -A / -R [file]: write, append or read the history file.
    if write || append || read {
        let file = if push { None } else { range.first().cloned() };
        let Some(file) = file
            .or_else(|| var_get_opt("HISTFILE"))
            .filter(|f| !f.is_empty())
        else {
            return 0;
        };
        if read {
            if !push {
                if let Ok(h) = crate::history::read_zsh_file(&file) {
                    let mut cur = history();
                    cur.extend(h);
                    set_history(cur);
                }
            }
            return 0;
        }
        let mut entries = history();
        // The command running now is not saved.
        if crate::shell::with_err_ctx(|e| e.interactive) && current_in_history() {
            entries.pop();
        }
        if crate::history::write_zsh_file(&file, &entries, append).is_err() {
            builtin_err("fc", &format!("can't write history file {}", file));
            return 1;
        }
        return 0;
    }
    if !list {
        return 0;
    }
    let mut lines = with_shell(|s| s.history.lock().unwrap_or_else(|e| e.into_inner()).clone());
    // An interactive shell has already added the line running now; it is
    // the current event, and -1 is the one before it.
    if crate::shell::with_err_ctx(|e| e.interactive) && current_in_history() {
        lines.pop();
    }
    let n = lines.len() as i64;
    // An event by number, or the latest one starting with the text.
    let pos = |spec: &str| -> Option<i64> {
        match spec.parse::<i64>() {
            Ok(k) if k < 0 => Some((n + k + 1).max(1)),
            Ok(k) => Some(k.max(1)),
            Err(_) => lines
                .iter()
                .rposition(|l| l.starts_with(spec))
                .map(|k| k as i64 + 1),
        }
    };
    let mut ends = Vec::new();
    for spec in range.iter().take(2) {
        match pos(spec) {
            Some(k) => ends.push(k),
            None => {
                builtin_err("fc", &format!("event not found: {}", spec));
                return 1;
            }
        }
    }
    let first = ends.first().copied().unwrap_or((n - 15).max(1));
    let last = ends.get(1).copied().unwrap_or(n);
    let mut picked: Vec<(i64, &String)> = (first..=last.min(n))
        .filter_map(|k| lines.get((k - 1) as usize).map(|l| (k, l)))
        .collect();
    if reverse {
        picked.reverse();
    }
    let text: String = picked
        .iter()
        .map(|(k, l)| {
            // A newline in an event shows as \n.
            let l = l.replace('\n', "\\n");
            if numbers {
                format!("{:5}  {}\n", k, l)
            } else {
                format!("{}\n", l)
            }
        })
        .collect();
    out(&text);
    0
}

// zparseopts [-D] [-E] [-F] [-K] [-M] [-a array] [-A assoc] [--] spec...
// Parses options from the positional parameters. A spec is
// name[+][:|::][=array]; matched options are stored as `-name` (and the
// argument) in the spec's array or the -a array, and as keys in -A.
// zsh/zutil's zparseopts (bin_zparseopts): parses options described by
// specs `name[+][:[:][-]][=array]` out of $argv (or -v array).
fn builtin_zparseopts(words: &[String]) -> i32 {
    const ARG: u8 = 1;
    const OPT: u8 = 2;
    const SAME: u8 = 4;
    const MULT: u8 = 8;
    const GNUL: u8 = 16;
    const GNUS: u8 = 32;
    const MAP: u8 = 64;
    struct Desc {
        name: String,
        flags: u8,
        arr: Option<usize>,
        // Indices into `vals` of this option's values.
        vals: Vec<usize>,
    }
    struct Val {
        name: String,
        arg: Option<String>,
        // The single-element form (`-xarg`, `--long=arg`).
        joined: Option<String>,
    }
    struct Arr {
        name: String,
        vals: Vec<usize>,
        real: bool,
    }
    // Builtin option parsing for "a:A:DEFGKMn:v:".
    let mut ops: std::collections::HashMap<char, String> = std::collections::HashMap::new();
    let mut i = 1;
    'outer: while let Some(w) = words.get(i) {
        let Some(body) = w.strip_prefix('-') else {
            break;
        };
        i += 1;
        if body.is_empty() || body == "-" {
            break;
        }
        let chars: Vec<char> = body.chars().collect();
        let mut k = 0;
        while k < chars.len() {
            let c = chars[k];
            match c {
                'a' | 'A' | 'n' | 'v' => {
                    let v = if k + 1 < chars.len() {
                        chars[k + 1..].iter().collect()
                    } else {
                        match words.get(i) {
                            Some(v) => {
                                i += 1;
                                v.clone()
                            }
                            None => {
                                builtin_err("zparseopts", &format!("argument expected: -{}", c));
                                return 1;
                            }
                        }
                    };
                    ops.insert(c, v);
                    continue 'outer;
                }
                'D' | 'E' | 'F' | 'G' | 'K' | 'M' => {
                    ops.insert(c, String::new());
                }
                _ => {
                    builtin_err("zparseopts", &format!("bad option: -{}", c));
                    return 1;
                }
            }
            k += 1;
        }
    }
    let has = |c| ops.contains_key(&c);
    let (del, extract, fail, gnu, keep) = (has('D'), has('E'), has('F'), has('G'), has('K'));
    let map = has('M');
    let mut arrs: Vec<Arr> = Vec::new();
    let mut defarr = None;
    if let Some(n) = ops.get(&'a') {
        if n.is_empty() {
            builtin_err("zparseopts", "missing array name for -a");
            return 1;
        }
        arrs.push(Arr {
            name: n.clone(),
            vals: Vec::new(),
            real: true,
        });
        defarr = Some(0);
    }
    let assoc = ops.get(&'A').cloned();
    if assoc.as_deref() == Some("") {
        builtin_err("zparseopts", "missing array name for -A");
        return 1;
    }
    let progname = match ops.get(&'n') {
        Some(n) if n.is_empty() => {
            builtin_err("zparseopts", "missing program name for -n");
            return 1;
        }
        Some(n) => n.clone(),
        // zsh's scriptname: the function or sourced file running (eval
        // keeps it), else the script.
        None => crate::shell::with_err_ctx(|e| {
            e.stack
                .iter()
                .rev()
                .map(|f| &f.0)
                .find(|n| *n != "(eval)")
                .unwrap_or(&e.name)
                .clone()
        }),
    };
    let paramsname = ops.get(&'v').cloned().unwrap_or_else(|| "argv".to_string());
    if paramsname.is_empty() {
        builtin_err("zparseopts", "missing array name for -v");
        return 1;
    }
    let params: Vec<String> = if paramsname == "argv" {
        with_shell(|s| s.pos_params.clone())
    } else {
        match with_shell(|s| s.array(&paramsname).cloned()) {
            Some(p) => p,
            None => {
                builtin_err("zparseopts", &format!("no such array: {}", paramsname));
                return 1;
            }
        }
    };
    let mut specs: &[String] = &words[i.min(words.len())..];
    if specs.len() == 1 && (specs[0].is_empty() || specs[0] == "-") {
        specs = &[];
    } else if specs.is_empty() {
        builtin_err("zparseopts", "missing option descriptions");
        return 1;
    }
    let mut descs: Vec<Desc> = Vec::new();
    for spec in specs {
        if spec.is_empty() {
            builtin_err(
                "zparseopts",
                &format!("invalid option description: {}", spec),
            );
            return 1;
        }
        let ch: Vec<char> = spec.chars().collect();
        let mut p = 0;
        let mut f = 0u8;
        let mut name_end = ch.len();
        while p < ch.len() {
            if ch[p] == '\\' && p + 1 < ch.len() {
                p += 2;
                continue;
            }
            if p > 0 {
                if ch[p] == '+' {
                    f |= MULT;
                    name_end = p;
                    p += 1;
                    break;
                } else if ch[p] == ':' || ch[p] == '=' {
                    name_end = p;
                    break;
                }
            }
            p += 1;
        }
        if name_end == ch.len() {
            name_end = p.min(ch.len());
        }
        let raw_name: String = ch[..name_end].iter().collect();
        if ch.get(p) == Some(&':') {
            f |= ARG;
            if gnu {
                f |= if raw_name.chars().count() > 1 {
                    GNUL
                } else {
                    GNUS
                };
            }
            p += 1;
            if ch.get(p) == Some(&':') {
                p += 1;
                f |= OPT;
            }
            if ch.get(p) == Some(&'-') {
                p += 1;
                f |= SAME;
            }
        }
        let arr = if ch.get(p) == Some(&'=') {
            let an: String = ch[p + 1..].iter().collect();
            f |= if map { MAP } else { 0 };
            Some(match arrs.iter().position(|x| x.name == an) {
                Some(k) => k,
                None => {
                    arrs.push(Arr {
                        name: an,
                        vals: Vec::new(),
                        real: true,
                    });
                    arrs.len() - 1
                }
            })
        } else if p < ch.len() {
            builtin_err(
                "zparseopts",
                &format!("invalid option description: {}", spec),
            );
            return 1;
        } else if defarr.is_none() && assoc.is_none() {
            builtin_err("zparseopts", &format!("no default array defined: {}", spec));
            return 1;
        } else {
            defarr
        };
        let mut name = String::new();
        let mut it = raw_name.chars();
        while let Some(c) = it.next() {
            if c == '\\' {
                if let Some(n) = it.next() {
                    name.push(n);
                    continue;
                }
            }
            name.push(c);
        }
        if descs.iter().any(|d| d.name == name) {
            builtin_err(
                "zparseopts",
                &format!("option defined more than once: {}", name),
            );
            return 1;
        }
        descs.push(Desc {
            name,
            flags: f,
            arr,
            vals: Vec::new(),
        });
    }
    // -M: an option whose array names another option is that option.
    let resolve = |descs: &Vec<Desc>, arrs: &Vec<Arr>, d: usize| -> usize {
        let mut cur = d;
        for _ in 0..descs.len() + 1 {
            if descs[cur].flags & MAP == 0 {
                return cur;
            }
            let Some(a) = descs[cur].arr else {
                return cur;
            };
            match descs.iter().position(|x| x.name == arrs[a].name) {
                Some(n) if n != cur => cur = n,
                _ => return cur,
            }
        }
        cur
    };
    let mut vals: Vec<Val> = Vec::new();
    let mut add = |descs: &mut Vec<Desc>, arrs: &mut Vec<Arr>, d0: usize, arg: Option<String>| {
        let name = format!("-{}", descs[d0].name);
        let d = resolve(descs, arrs, d0);
        let flags = descs[d].flags;
        let joined = if flags & ARG != 0 && flags & (OPT | SAME) == 0 {
            None
        } else if arg.is_some() || flags & GNUL != 0 {
            let mut s = format!("-{}", descs[d].name);
            if flags & GNUL != 0 {
                s.push('=');
            }
            s.push_str(arg.as_deref().unwrap_or(""));
            Some(s)
        } else {
            None
        };
        let v = Val { name, arg, joined };
        if flags & MULT == 0 {
            if let Some(&k) = descs[d].vals.first() {
                vals[k] = v;
                return;
            }
        }
        vals.push(v);
        let k = vals.len() - 1;
        descs[d].vals.push(k);
        if let Some(a) = descs[d].arr {
            arrs[a].vals.push(k);
        }
    };
    // Latest definitions are found first, as zsh's list is built.
    let lookup = |descs: &Vec<Desc>, s: &str| -> Option<usize> {
        (0..descs.len()).rev().find(|&k| {
            let d = &descs[k];
            if d.flags & GNUL != 0 {
                s == d.name
                    || (s.starts_with(d.name.as_str()) && s[d.name.len()..].starts_with('='))
            } else if d.flags & ARG != 0 {
                s.starts_with(d.name.as_str())
            } else {
                s == d.name
            }
        })
    };
    let short = |descs: &Vec<Desc>, c: char| -> Option<usize> {
        (0..descs.len())
            .rev()
            .find(|&k| descs[k].name.chars().count() == 1 && descs[k].name.starts_with(c))
    };
    let mut kept_extract: Vec<String> = Vec::new();
    let mut pp = 0;
    while pp < params.len() {
        let o = &params[pp];
        if !o.starts_with('-') || (gnu && o == "-") {
            if extract {
                if del {
                    kept_extract.push(o.clone());
                }
                pp += 1;
                continue;
            }
            break;
        }
        if o == "-" || o == "--" {
            if del && extract {
                kept_extract.push(o.clone());
            }
            pp += 1;
            break;
        }
        let body = &o[1..];
        match lookup(&descs, body) {
            Some(d) => {
                if descs[d].flags & ARG != 0 {
                    let e = &body[descs[d].name.len()..];
                    let fl = descs[d].flags;
                    if fl & GNUL != 0 && e.starts_with('=') {
                        add(&mut descs, &mut arrs, d, Some(e[1..].to_string()));
                    } else if !e.is_empty() {
                        add(&mut descs, &mut arrs, d, Some(e.to_string()));
                    } else if fl & OPT == 0
                        || (fl & (GNUL | GNUS) == 0
                            && params.get(pp + 1).is_some_and(|n| !n.starts_with('-')))
                    {
                        match params.get(pp + 1) {
                            Some(n) => {
                                pp += 1;
                                add(&mut descs, &mut arrs, d, Some(n.clone()));
                            }
                            None => {
                                err_line(&format!(
                                    "{}: missing argument for option: -{}",
                                    progname, descs[d].name
                                ));
                                return 1;
                            }
                        }
                    } else {
                        add(&mut descs, &mut arrs, d, None);
                    }
                } else {
                    add(&mut descs, &mut arrs, d, None);
                }
            }
            None => {
                let chars: Vec<char> = body.chars().collect();
                let mut not_opt = false;
                let mut k = 0;
                while k < chars.len() {
                    let c = chars[k];
                    let Some(d) = short(&descs, c) else {
                        if fail {
                            if c != '-' || k > 0 {
                                err_line(&format!("{}: bad option: -{}", progname, c));
                            } else {
                                err_line(&format!("{}: bad option: -{}", progname, body));
                            }
                            return 1;
                        }
                        not_opt = true;
                        break;
                    };
                    let fl = descs[d].flags;
                    if fl & ARG != 0 {
                        if k + 1 < chars.len() {
                            add(
                                &mut descs,
                                &mut arrs,
                                d,
                                Some(chars[k + 1..].iter().collect()),
                            );
                            break;
                        } else if fl & OPT == 0
                            || (fl & (GNUL | GNUS) == 0
                                && params.get(pp + 1).is_some_and(|n| !n.starts_with('-')))
                        {
                            match params.get(pp + 1) {
                                Some(n) => {
                                    pp += 1;
                                    add(&mut descs, &mut arrs, d, Some(n.clone()));
                                }
                                None => {
                                    err_line(&format!(
                                        "{}: missing argument for option: -{}",
                                        progname, descs[d].name
                                    ));
                                    return 1;
                                }
                            }
                        } else {
                            add(&mut descs, &mut arrs, d, None);
                        }
                    } else {
                        add(&mut descs, &mut arrs, d, None);
                    }
                    k += 1;
                }
                if not_opt {
                    if extract {
                        if del {
                            kept_extract.push(o.clone());
                        }
                        pp += 1;
                        continue;
                    }
                    break;
                }
            }
        }
        pp += 1;
    }
    if map {
        for d in &descs {
            if let Some(a) = d.arr {
                if d.vals.is_empty()
                    && d.flags & MAP != 0
                    && arrs[a].vals.is_empty()
                    && descs.iter().any(|x| x.name == arrs[a].name)
                {
                    arrs[a].real = false;
                }
            }
        }
    }
    if extract && del {
        kept_extract.extend(params[pp.min(params.len())..].iter().cloned());
    }
    for a in &arrs {
        if a.real && (!keep || !a.vals.is_empty()) {
            let mut out = Vec::new();
            for &k in &a.vals {
                let v = &vals[k];
                match &v.joined {
                    Some(s) => out.push(s.clone()),
                    None => {
                        out.push(v.name.clone());
                        if let Some(arg) = &v.arg {
                            out.push(arg.clone());
                        }
                    }
                }
            }
            array_set(&a.name, out);
        }
    }
    if let Some(name) = assoc {
        let used: Vec<&Desc> = descs.iter().filter(|d| !d.vals.is_empty()).collect();
        if !keep || !used.is_empty() {
            if !keep {
                with_shell(|s| {
                    s.remove_param(&name);
                });
            }
            assoc_declare(&name);
            for d in used.iter().rev() {
                let text: Vec<String> = d
                    .vals
                    .iter()
                    .map(|&k| vals[k].arg.clone().unwrap_or_default())
                    .collect();
                crate::shell::assoc_set(&name, &format!("-{}", d.name), &text.join(" "));
            }
        }
    }
    if del {
        let rest = if extract {
            kept_extract
        } else {
            params[pp.min(params.len())..].to_vec()
        };
        if paramsname == "argv" {
            with_shell(|s| s.pos_params = rest);
        } else {
            array_set(&paramsname, rest);
        }
    }
    0
}

// is-at-least min [version]: true if version (default $ZSH_VERSION) is
// at least min, comparing the numeric parts of each `.`/`-` component.
fn builtin_is_at_least(words: &[String]) -> i32 {
    let Some(min) = words.get(1) else {
        return usage("is-at-least", "usage: is-at-least min [version]");
    };
    let have = words
        .get(2)
        .cloned()
        .unwrap_or_else(|| var_get("ZSH_VERSION"));
    let parts = |v: &str| -> Vec<u64> {
        v.split(['.', '-'])
            .map(|p| {
                let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
                digits.parse().unwrap_or(0)
            })
            .collect()
    };
    let (a, b) = (parts(&have), parts(min));
    for k in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(k).copied().unwrap_or(0),
            b.get(k).copied().unwrap_or(0),
        );
        if x != y {
            return if x > y { 0 } else { 1 };
        }
    }
    0
}

// zsh's `colors` function: $fg, $bg, $fg_bold, $fg_no_bold, $bg_bold,
// $bg_no_bold and $color tables, and $reset_color.
fn builtin_colors() -> i32 {
    const NAMES: [&str; 8] = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ];
    let mut tables: Vec<(&str, Vec<(String, String)>)> = Vec::new();
    let mk = |f: &dyn Fn(usize) -> String| -> Vec<(String, String)> {
        NAMES
            .iter()
            .enumerate()
            .flat_map(|(k, n)| {
                let v = f(k);
                [(n.to_string(), v.clone()), (n.to_uppercase(), v)]
            })
            .collect()
    };
    tables.push(("fg", mk(&|k| format!("\x1b[{}m", 30 + k))));
    tables.push(("bg", mk(&|k| format!("\x1b[{}m", 40 + k))));
    tables.push(("fg_bold", mk(&|k| format!("\x1b[01;{}m", 30 + k))));
    tables.push(("fg_no_bold", mk(&|k| format!("\x1b[22;{}m", 30 + k))));
    tables.push(("bg_bold", mk(&|k| format!("\x1b[01;{}m", 40 + k))));
    tables.push(("bg_no_bold", mk(&|k| format!("\x1b[22;{}m", 40 + k))));
    let mut color: Vec<(String, String)> = Vec::new();
    for (k, n) in NAMES.iter().enumerate() {
        color.push((n.to_string(), (30 + k).to_string()));
        color.push((format!("bg-{}", n), (40 + k).to_string()));
    }
    for (n, c) in [
        ("reset", 0),
        ("bold", 1),
        ("faint", 2),
        ("italic", 3),
        ("underline", 4),
        ("blink", 5),
        ("reverse", 7),
        ("normal", 22),
    ] {
        color.push((n.to_string(), c.to_string()));
    }
    tables.push(("color", color));
    for (name, entries) in tables {
        assoc_declare(name);
        for (k, v) in entries {
            crate::shell::assoc_set(name, &k, &v);
        }
    }
    var_set("reset_color", "\x1b[00m", false);
    var_set("bold_color", "\x1b[01m", false);
    0
}

// emulate: meowsh always behaves as zsh here. With no arguments it
// reports the mode; `-c cmd` runs cmd; option changes are ignored.
// emulate [-lLR] [zsh|sh|ksh|csh [-c arg]]: with no mode, prints the
// current one. -R resets every option, -L makes the change local to the
// calling function, -c runs `arg` under the emulation and then restores
// the options.
fn builtin_emulate(words: &[String]) -> i32 {
    let (mut reset_all, mut local) = (false, false);
    let mut mode = None;
    let mut cmd = None;
    // `-o name` / `+o name` / `-x` after the emulation: extra options.
    let mut extra: Vec<(String, bool)> = Vec::new();
    let mut i = 1;
    while let Some(a) = words.get(i) {
        match a.as_str() {
            "-o" | "+o" if mode.is_some() => {
                i += 1;
                let Some(name) = words.get(i) else {
                    return usage("emulate", "-o requires an argument");
                };
                if crate::options::resolve(name).is_none() {
                    builtin_err("emulate", &format!("no such option: {}", name));
                    return 1;
                }
                extra.push((name.clone(), a == "-o"));
            }
            a if mode.is_some()
                && a.len() > 1
                && (a.starts_with('-') || a.starts_with('+'))
                && a != "-c" =>
            {
                for c in a[1..].chars() {
                    match crate::options::letter(c) {
                        Some((name, v)) => {
                            extra.push((name.to_string(), if a.starts_with('-') { v } else { !v }))
                        }
                        None => return usage("emulate", &format!("bad option: {}{}", &a[..1], c)),
                    }
                }
            }
            "-c" => {
                i += 1;
                cmd = words.get(i).cloned();
                if cmd.is_none() {
                    return usage("emulate", "-c requires an argument");
                }
            }
            a if a.starts_with('-') && a.len() > 1 => {
                for c in a[1..].chars() {
                    match c {
                        'R' => reset_all = true,
                        'L' => local = true,
                        'l' => {}
                        _ => return usage("emulate", &format!("bad option: -{}", c)),
                    }
                }
            }
            m => match crate::options::Emulation::from_name(m) {
                Some(e) if mode.is_none() => mode = Some(e),
                _ => return usage("emulate", &format!("unknown emulation: {}", m)),
            },
        }
        i += 1;
    }
    let Some(mode) = mode else {
        out(&format!("{}\n", with_shell(|s| s.emulation.name())));
        return 0;
    };
    if let Some(cmd) = cmd {
        let saved = with_shell(|s| {
            (
                s.opts,
                s.named_opts.clone(),
                s.emulation,
                s.assoc("options").cloned(),
            )
        });
        let sticky = (mode, reset_all, extra.clone());
        crate::options::apply_sticky(&sticky);
        let saved_sticky = crate::options::CUR_STICKY
            .lock()
            .ok()
            .and_then(|mut c| c.replace(sticky));
        let st = crate::shell::in_eval_context("eval", || {
            crate::exec::run_string_named(&cmd, Some("(eval)"))
        });
        if let Ok(mut c) = crate::options::CUR_STICKY.lock() {
            *c = saved_sticky;
        }
        with_shell(|s| {
            crate::options::restore_in(s, &saved.1);
            s.opts = saved.0;
            s.emulation = saved.2;
            if let Some(o) = saved.3 {
                s.store("options", crate::params::Value::Assoc(o));
            }
        });
        return st;
    }
    with_shell(|s| crate::options::emulate_in(s, mode, reset_all));
    for (name, on) in &extra {
        if !crate::options::set(name, *on) {
            builtin_err("emulate", &format!("no such option: {}", name));
            return 1;
        }
    }
    if local {
        for o in ["localoptions", "localtraps", "localpatterns"] {
            toggle_named_opt(o, true);
        }
    }
    0
}

// add-zsh-hook [-d] hook function
fn builtin_add_zsh_hook(words: &[String]) -> i32 {
    let mut args: Vec<&str> = words[1..].iter().map(|s| s.as_str()).collect();
    let mut delete = false;
    while let Some(a) = args.first() {
        match *a {
            "-d" => delete = true,
            "-D" | "-U" | "-z" | "-k" => {}
            _ => break,
        }
        args.remove(0);
    }
    let [hook, func] = args[..] else {
        return usage("add-zsh-hook", "usage: add-zsh-hook [-d] hook function");
    };
    if !HOOKS.contains(&hook) {
        return usage("add-zsh-hook", &format!("{}: unknown hook", hook));
    }
    let list = format!("{}_functions", hook);
    with_shell(|s| {
        let mut fns = s.array(&list).cloned().unwrap_or_default();
        fns.retain(|f| f != func);
        if !delete {
            fns.push(func.to_string());
        }
        s.store(&list, crate::params::Value::Array(fns));
    });
    0
}

// zsh `print`: echo-style escapes unless -r; -n no newline, -l one
// argument per line, -N NUL-separated, -P prompt escapes, -u fd.
// `fc -p`: the histories (and $HISTFILE) set aside, for `fc -P`.
static HIST_STACK: std::sync::Mutex<Vec<(Vec<String>, Option<String>)>> =
    std::sync::Mutex::new(Vec::new());

// Whether the command running now was added to the history (it is not
// with HIST_IGNORE_SPACE, nor after `fc -p`).
fn current_in_history() -> bool {
    crate::exec::LINE_IN_HISTORY.load(std::sync::atomic::Ordering::Relaxed)
}

thread_local! {
    // Set when an autoloaded file failed to parse (and was reported).
    static AUTOLOAD_PARSE_FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<(char, bool)> {
        s.chars().map(|c| (c, false)).collect()
    }

    #[test]
    fn read_split_assigns_rest_to_last() {
        assert_eq!(
            read_split(&chars("  a b  c d  "), " \t\n", 2),
            vec!["a", "b  c d"]
        );
        assert_eq!(read_split(&chars("a:b:c"), ":", 2), vec!["a", "b:c"]);
        assert_eq!(read_split(&chars("a"), " ", 3), vec!["a", "", ""]);
        assert_eq!(read_split(&chars("x y z"), " ", 0), vec!["x", "y", "z"]);
    }

    #[test]
    fn quoting_round_trips() {
        assert_eq!(shell_quote("abc"), "abc");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn normalize_resolves_dots() {
        assert_eq!(normalize_path("/a/b/../c/./d"), "/a/c/d");
        assert_eq!(normalize_path("/.."), "/");
    }
}
