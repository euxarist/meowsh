//! Command execution.

use crate::expand::{
    expand_assign_value, expand_heredoc, expand_pattern, expand_string, expand_words, ExpError,
    ExpResult,
};
use crate::jobs::{add_job, reap_jobs, status_to_exit, wait_foreground, TTY_FD};
use crate::lexer::assignment_split;
use crate::parser::{ParseError, Parser};
use crate::shell::{
    array_append, array_set_subscript, build_env, find_command, is_array, opt_set, pop_scope,
    shell_err, var_get, var_get_opt, var_set, with_shell,
};
use crate::sys::{close, dup_high, err_line, pipe_cloexec, read_all, temp_file_with};
use crate::types::{
    AndOrOp, Assign, AssignValue, CaseItem, Flow, FuncDef, JobState, ListItem, Node, Redir,
    RedirOp, SimpleCmd, OPT_ERREXIT, OPT_NOCLOBBER, OPT_NOEXEC, OPT_PIPEFAIL, OPT_VERBOSE,
    OPT_XTRACE, VAR_READONLY,
};
use crate::typeset::DeclArg;
use std::ffi::CString;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::Arc;

const MAX_FUNC_DEPTH: usize = 1000;

// ---- shared state helpers -------------------------------------------------

pub fn last_status() -> i32 {
    with_shell(|s| s.last_status)
}

pub fn flow() -> Flow {
    with_shell(|s| s.flow)
}

pub fn set_flow(f: Flow) {
    with_shell(|s| s.flow = f);
}

fn flow_pending() -> bool {
    flow() != Flow::None
}

fn interactive() -> bool {
    with_shell(|s| s.interactive)
}

fn suppress_errexit(delta: isize) {
    with_shell(|s| {
        s.errexit_suppressed = (s.errexit_suppressed as isize + delta).max(0) as usize;
        s.errreturn_suppressed = (s.errreturn_suppressed as isize + delta).max(0) as usize;
    });
}

fn set_error_exit(status: i32) {
    with_shell(|s| {
        s.flow = Flow::Exit(status);
        s.error_exit = true;
    });
}

/// zsh's `donetrap`: set once the ZERR trap (or ERR_EXIT handling) has
/// acted on a failure, so the same failure seen again by an enclosing
/// command (a function call's status) doesn't trigger it twice. Cleared
/// before each command of a list.
static DONE_TRAP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn check_errexit(status: i32) {
    use std::sync::atomic::Ordering;
    if status == 0 || DONE_TRAP.load(Ordering::Relaxed) {
        return;
    }
    // `return 1` counts as a failing command too.
    let (live, idle, trapped) = with_shell(|s| {
        (
            s.errexit_suppressed == 0,
            // A fatal error on the way out still runs the ZERR trap.
            matches!(s.flow, Flow::None | Flow::Return(_))
                || (s.error_exit && matches!(s.flow, Flow::Exit(_))),
            s.traps
                .get(&crate::trap::SIG_ZERR)
                .is_some_and(|a| !a.is_empty()),
        )
    });
    if !idle {
        return;
    }
    // zsh's ZERR trap runs first, then ERR_EXIT or ERR_RETURN act.
    if live && trapped {
        let pending = with_shell(|s| std::mem::replace(&mut s.flow, Flow::None));
        crate::trap::run_pseudo(crate::trap::SIG_ZERR);
        // After the trap: running it (a TRAPZERR function) clears the flag.
        DONE_TRAP.store(true, Ordering::Relaxed);
        with_shell(|s| {
            s.last_status = status;
            if s.flow == Flow::None {
                s.flow = pending;
            }
        });
    }
    with_shell(|s| {
        if !matches!(s.flow, Flow::None | Flow::Return(_)) {
            return;
        }
        if live && s.opts & OPT_ERREXIT != 0 {
            s.flow = Flow::Exit(status);
        } else if s.errreturn_suppressed == 0
            && s.func_depth > 0
            && crate::options::is_set_in(s, "errreturn")
        {
            // ERR_RETURN: leave the function with the failing status.
            if s.flow == Flow::None {
                s.flow = Flow::Return(status);
            }
        }
    });
}

/// A fatal error (zsh's errflag): abort the command line, as an
/// expansion error does.
pub fn abort_command(status: i32) -> i32 {
    expansion_failed(ExpError { status })
}

// An expansion error aborts the current command line in an interactive
// shell and exits a non-interactive one.
fn expansion_failed(e: ExpError) -> i32 {
    with_shell(|s| {
        if s.flow == Flow::None {
            s.error_exit = true;
            s.flow = if s.interactive {
                Flow::Interrupt
            } else {
                Flow::Exit(e.status)
            };
        }
    });
    e.status
}

pub fn strerror(errno: i32) -> String {
    unsafe {
        let p = libc::strerror(errno);
        if p.is_null() {
            return format!("error {}", errno);
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

// ---- process creation -----------------------------------------------------

/// How a forked child relates to job control.
#[derive(Clone, Copy)]
pub enum ForkKind {
    /// A foreground job (or member of one); 0 starts a new process group.
    Foreground(i32),
    /// An asynchronous list.
    Background,
    /// Stays in the shell's process group (command substitution).
    Inline,
}

/// Forks. In the child, signal dispositions and shell state are reset for
/// a subshell; returns 0 there and the pid in the parent (-1 on failure).
pub fn fork_process(kind: ForkKind) -> i32 {
    let _ = std::io::stdout().flush();
    let jc = with_shell(|s| s.job_control);
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        err_line(&format!("meowsh: fork: {}", strerror(last_errno())));
        return -1;
    }
    if pid == 0 {
        if jc {
            match kind {
                ForkKind::Foreground(pgid) => unsafe {
                    libc::setpgid(0, pgid);
                    let fd = TTY_FD.load(Ordering::SeqCst);
                    if fd >= 0 {
                        libc::tcsetpgrp(fd, libc::getpgrp());
                    }
                },
                ForkKind::Background => unsafe {
                    libc::setpgid(0, 0);
                },
                ForkKind::Inline => {}
            }
        }
        crate::shell::SUBSHELL_LEVEL.fetch_add(1, Ordering::Relaxed);
        with_shell(|s| {
            s.subshell_loop_depth += s.loop_depth;
            s.loop_depth = 0;
        });
        crate::trap::reset_in_child(matches!(kind, ForkKind::Background) && !jc);
        with_shell(|s| {
            s.in_subshell = true;
            s.interactive = false;
            s.job_control = false;
            // The parent's jobs stay visible to `jobs` and $jobstates.
            let old = std::mem::take(&mut *s.jobs.lock().unwrap_or_else(|e| e.into_inner()));
            if !old.is_empty() {
                if let Ok(mut o) = crate::jobs::OLD_JOBS.lock() {
                    *o = old;
                }
            }
        });
        TTY_FD.store(-1, Ordering::SeqCst);
    } else if jc {
        match kind {
            ForkKind::Foreground(pgid) => unsafe {
                libc::setpgid(pid, if pgid == 0 { pid } else { pgid });
            },
            ForkKind::Background => unsafe {
                libc::setpgid(pid, pid);
            },
            ForkKind::Inline => {}
        }
    }
    pid
}

/// Leaves a forked child: runs its EXIT trap and exits without running
/// the parent's destructors or flushing its buffers twice.
pub fn exit_child(status: i32) -> ! {
    let status = match flow() {
        Flow::Exit(n) => n,
        _ => status,
    };
    crate::trap::run_exit_trap();
    let status = match flow() {
        Flow::Exit(n) => n,
        _ => status,
    };
    let _ = std::io::stdout().flush();
    unsafe { libc::_exit(status & 0xff) }
}

/// Exits the shell: runs the EXIT trap and terminates the process.
// Files made for `=(cmd)`, removed when the shell exits.
static EQ_FILES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// zsh's `=(cmd)`: the output of cmd in a temporary file, whose name is
/// the result.
pub fn output_file_subst(src: &str) -> Option<String> {
    let (out, _) = crate::shell::in_eval_context("equalsubst", || command_subst_in(src));
    // zsh names it $TMPPREFIX and a unique suffix.
    let prefix = var_get_opt("TMPPREFIX")
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| {
            let dir = std::env::var("TMPDIR")
                .ok()
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| "/tmp".to_string());
            format!("{}/meowsh-eq", dir.trim_end_matches('/'))
        });
    let mut buf = format!("{}XXXXXX", prefix).into_bytes();
    buf.push(0);
    let fd = unsafe { libc::mkstemp(buf.as_mut_ptr() as *mut libc::c_char) };
    if fd < 0 {
        shell_err(&format!(
            "can't create temporary file: {}",
            strerror(last_errno())
        ));
        return None;
    }
    let _ = crate::sys::write_fd(fd, out.as_bytes());
    close(fd);
    buf.pop();
    let path = String::from_utf8_lossy(&buf).into_owned();
    if let Ok(mut f) = EQ_FILES.lock() {
        f.push(path.clone());
    }
    Some(path)
}

fn remove_eq_files() {
    if let Ok(mut f) = EQ_FILES.lock() {
        for p in f.drain(..) {
            let _ = std::fs::remove_file(p);
        }
    }
}

pub fn exit_shell(status: i32) -> ! {
    if with_shell(|s| s.in_subshell) {
        exit_child(status);
    }
    remove_eq_files();
    with_shell(|s| s.flow = Flow::None);
    if interactive() {
        crate::builtins::run_hooks("zshexit", &[]);
        with_shell(|s| s.flow = Flow::None);
        crate::jobs::hup_jobs();
    }
    crate::trap::run_exit_trap();
    let status = match flow() {
        Flow::Exit(n) => n,
        _ => status,
    };
    let _ = std::io::stdout().flush();
    std::process::exit(status & 0xff)
}

/// Replaces the process with an external command. Never returns.
pub fn exec_external(words: &[String], env: &[(String, String)]) -> ! {
    exec_external_as(words, None, env)
}

/// Like `exec_external`, with `argv0` (when given) as the new program's
/// argument zero instead of the command name.
pub fn exec_external_as(words: &[String], argv0: Option<&str>, env: &[(String, String)]) -> ! {
    let name = &words[0];
    // `UID=n cmd` (and EUID, GID, EGID) runs the command with that id; if
    // it can't be changed, the command doesn't run.
    for (k, v) in env {
        if matches!(k.as_str(), "UID" | "EUID" | "GID" | "EGID")
            && !sh_emulation_pub()
            && !crate::shell::var_set(k, v, false)
        {
            exit_child(1);
        }
    }
    // `PATH=dirs cmd` searches the PATH given for the command.
    // PATH_DIRS: `dir/cmd` (not `./`, `../` or absolute) missing here
    // is looked for under each $PATH directory.
    let path_dirs = name.contains('/')
        && !name.starts_with('/')
        && !name.starts_with("./")
        && !name.starts_with("../")
        && !std::path::Path::new(name).exists()
        && crate::options::is_set("pathdirs");
    let path = if path_dirs {
        var_get("PATH")
            .split(':')
            .map(|d| format!("{}/{}", if d.is_empty() { "." } else { d }, name))
            .find(|p| std::path::Path::new(p).is_file())
            .or_else(|| Some(name.clone()))
    } else if name.contains('/') {
        Some(name.clone())
    } else if let Some((_, p)) = env.iter().find(|(k, _)| k == "PATH") {
        crate::shell::find_command_in(name, p)
    } else {
        find_command(name)
    };
    let Some(path) = path else {
        // zsh runs command_not_found_handler with the command and its
        // arguments, in the process that would have run the command.
        let handler = with_shell(|s| {
            s.functions
                .get("command_not_found_handler")
                .map(|f| (f.body.clone(), f.line))
        });
        if let Some((body, line)) = handler {
            let mut args = vec!["command_not_found_handler".to_string()];
            args.extend(words.iter().cloned());
            let st = call_function(&body, &args, line);
            let _ = std::io::stdout().flush();
            unsafe { libc::_exit(st) }
        }
        shell_err(&format!(
            "command not found: {}",
            crate::shell::nice_str(name)
        ));
        unsafe { libc::_exit(127) }
    };
    let env_argv0 = argv0
        .is_none()
        .then(|| {
            env.iter()
                .find(|(k, _)| k == "ARGV0")
                .map(|(_, v)| v.as_str())
        })
        .flatten();
    let eff_argv0 = argv0.or(env_argv0);
    let to_c = |s: &str| CString::new(crate::shell::zstr_to_bytes(s)).ok();
    let (Some(c_path), Some(argv)) = (
        to_c(&path),
        words
            .iter()
            .enumerate()
            .map(|(k, w)| to_c(if k == 0 { eff_argv0.unwrap_or(w) } else { w }))
            .collect::<Option<Vec<_>>>(),
    ) else {
        err_line(&format!("meowsh: {}: invalid argument", name));
        unsafe { libc::_exit(126) }
    };
    let envp: Vec<CString> = env
        .iter()
        .filter(|(k, _)| !(env_argv0.is_some() && k == "ARGV0"))
        .filter_map(|(k, v)| {
            CString::new(crate::shell::zstr_to_bytes(&format!("{}={}", k, v))).ok()
        })
        .collect();
    let mut argv_p: Vec<*const libc::c_char> = argv.iter().map(|c| c.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let mut envp_p: Vec<*const libc::c_char> = envp.iter().map(|c| c.as_ptr()).collect();
    envp_p.push(std::ptr::null());
    let _ = std::io::stdout().flush();
    unsafe {
        libc::execve(c_path.as_ptr(), argv_p.as_ptr(), envp_p.as_ptr());
    }
    let e = last_errno();
    // zsh runs `#!interp [arg]` itself when the system cannot, as for an
    // interpreter given without a directory (searched in $PATH).
    if e == libc::ENOENT {
        if let Some((interp, arg)) = shebang(&path) {
            let interp = if interp.contains('/') {
                Some(interp)
            } else {
                find_command(&interp)
            };
            if let Some(c_interp) = interp.as_deref().and_then(to_c) {
                let c_arg = arg.as_deref().and_then(to_c);
                let mut args: Vec<*const libc::c_char> = vec![c_interp.as_ptr()];
                if let Some(a) = &c_arg {
                    args.push(a.as_ptr());
                }
                args.push(c_path.as_ptr());
                args.extend(argv.iter().skip(1).map(|c| c.as_ptr()));
                args.push(std::ptr::null());
                unsafe {
                    libc::execve(c_interp.as_ptr(), args.as_ptr(), envp_p.as_ptr());
                }
            }
        }
    }
    if e == libc::ENOEXEC {
        // No interpreter line: run the file as a shell script.
        if let Ok(exe) = std::env::current_exe() {
            if let Some(c_exe) = to_c(&exe.to_string_lossy()) {
                // `--` keeps a path starting with - or + from being read
                // as options.
                let dashdash = to_c("--");
                let mut args: Vec<*const libc::c_char> = vec![c_exe.as_ptr()];
                if let Some(d) = dashdash.as_ref().filter(|_| path.starts_with(['-', '+'])) {
                    args.push(d.as_ptr());
                }
                args.push(c_path.as_ptr());
                args.extend(argv.iter().skip(1).map(|c| c.as_ptr()));
                args.push(std::ptr::null());
                unsafe {
                    libc::execve(c_exe.as_ptr(), args.as_ptr(), envp_p.as_ptr());
                }
            }
        }
    }
    shell_err(&file_error(name, e));
    unsafe { libc::_exit(if e == libc::ENOENT { 127 } else { 126 }) }
}

// The interpreter and optional argument of a `#!` line.
fn shebang(path: &str) -> Option<(String, Option<String>)> {
    use std::io::Read;
    let mut buf = [0u8; 256];
    let n = std::fs::File::open(path).ok()?.read(&mut buf).ok()?;
    let head = std::str::from_utf8(&buf[..n]).ok()?;
    let line = head.strip_prefix("#!")?.lines().next()?.trim();
    let mut parts = line.splitn(2, [' ', '\t']);
    let interp = parts.next()?.to_string();
    let arg = parts
        .next()
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty());
    (!interp.is_empty()).then_some((interp, arg))
}

// ---- redirections ---------------------------------------------------------

// zsh gives 1 for a parse error, sh (bash) 2.
fn parse_error_status() -> i32 {
    if sh_emulation() {
        2
    } else {
        1
    }
}

pub fn sh_emulation_pub() -> bool {
    sh_emulation()
}

fn sh_emulation() -> bool {
    with_shell(|s| matches!(s.emulation, crate::options::Emulation::Sh))
}

enum Target {
    Opened(i32),
    // `>& file`: standard output and standard error both go to the file.
    Both(i32),
    Dup(i32),
    Close,
}

// zsh's wording: the system message, lowercased, then the file.
pub fn file_error(path: &str, errno: i32) -> String {
    let msg = strerror(errno);
    let mut c = msg.chars();
    let msg = match c.next() {
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
        None => msg,
    };
    format!("{}: {}", msg, path)
}

fn open_file(path: &str, flags: i32) -> Result<i32, String> {
    let c = CString::new(crate::shell::zstr_to_bytes(path))
        .map_err(|_| format!("invalid file name: {}", path))?;
    let fd = unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC, 0o666 as libc::c_uint) };
    if fd < 0 {
        return Err(file_error(path, last_errno()));
    }
    Ok(fd)
}

// Err(None) means the message was already printed by the expander.
fn open_redir(r: &Redir) -> Result<Target, Option<String>> {
    if r.op == RedirOp::HereDoc {
        let body = match &r.heredoc {
            Some(h) if h.expand => expand_heredoc(&h.body).map_err(|_| None)?,
            Some(h) => h.body.clone(),
            None => String::new(),
        };
        let fd = temp_file_with(&crate::shell::zstr_to_bytes(&body))
            .map_err(|e| Some(format!("here-document: {}", e)))?;
        return Ok(Target::Opened(fd));
    }
    let mut target = expand_string(&r.target).map_err(|_| None)?;
    // A pattern target is globbed: one match names the file, none fails
    // even with (N).
    if r.op != RedirOp::HereStr
        && !crate::options::is_set("noglob")
        && !r.target.starts_with("<(")
        && !r.target.starts_with(">(")
        && !r.target.starts_with("=(")
        && crate::pattern::has_meta(&r.target)
        && r.target.ends_with(')')
    {
        let words =
            crate::expand::expand_words(std::slice::from_ref(&r.target)).map_err(|_| None)?;
        match words.as_slice() {
            [one] => target = one.clone(),
            [] => return Err(Some(format!("redirection failed (no match): {}", r.target))),
            _ => {}
        }
    }
    if r.op == RedirOp::HereStr {
        let fd = temp_file_with(&crate::shell::zstr_to_bytes(&format!("{}\n", target)))
            .map_err(|e| Some(format!("here-string: {}", e)))?;
        return Ok(Target::Opened(fd));
    }
    let file = |flags| open_file(&target, flags).map(Target::Opened).map_err(Some);
    match r.op {
        RedirOp::DupIn | RedirOp::DupOut => {
            if target == "-" {
                return Ok(Target::Close);
            }
            // `>&p` / `<&p`: the coprocess.
            if target == "p" {
                return match with_shell(|s| s.coproc) {
                    Some((w, rd)) => Ok(Target::Dup(if r.op == RedirOp::DupIn { rd } else { w })),
                    None => Err(Some("no coprocess".to_string())),
                };
            }
            match target.parse::<i32>() {
                Ok(n) if n >= 0 => {
                    if unsafe { libc::fcntl(n, libc::F_GETFD) } < 0 {
                        return Err(Some(format!(
                            "{}: {}",
                            n,
                            strerror(libc::EBADF).to_lowercase()
                        )));
                    }
                    Ok(Target::Dup(n))
                }
                _ if r.op == RedirOp::DupOut && !sh_emulation() => {
                    open_file(&target, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC)
                        .map(Target::Both)
                        .map_err(Some)
                }
                _ => Err(Some(format!("{}: ambiguous redirect", target))),
            }
        }
        RedirOp::Read => file(libc::O_RDONLY),
        RedirOp::Write if opt_set(OPT_NOCLOBBER) => match std::fs::metadata(&target) {
            // CLOBBER_EMPTY: an empty file may still be overwritten.
            Ok(m) if m.is_file() && m.len() == 0 && crate::options::is_set("clobberempty") => {
                file(libc::O_WRONLY | libc::O_TRUNC)
            }
            Ok(m) if m.is_file() => Err(Some(format!("file exists: {}", target))),
            Ok(_) => file(libc::O_WRONLY),
            Err(_) => file(libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL),
        },
        RedirOp::Write | RedirOp::Clobber => file(libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC),
        // NO_CLOBBER: `>>` needs the file to exist (unless APPEND_CREATE).
        RedirOp::Append if opt_set(OPT_NOCLOBBER) && !crate::options::is_set("appendcreate") => {
            file(libc::O_WRONLY | libc::O_APPEND)
        }
        RedirOp::Append | RedirOp::AppendClobber => {
            file(libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND)
        }
        RedirOp::ReadWrite => file(libc::O_RDWR | libc::O_CREAT),
        RedirOp::HereDoc | RedirOp::HereStr => unreachable!(),
    }
}

/// Saved descriptors: (fd, copy), where copy is -1 if fd was closed.
pub type SavedFds = Vec<(i32, i32)>;

/// A redirection failed; the message has been printed.
#[derive(Debug)]
pub struct RedirError;

// In a pipeline stage: whether its standard input / output is the
// pipe, which MULTIOS joins with the stage's own redirections.
static PIPE_IN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static PIPE_OUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// A redirection's direction, for MULTIOS.
fn redir_output(op: RedirOp) -> Option<bool> {
    match op {
        RedirOp::Write
        | RedirOp::Clobber
        | RedirOp::Append
        | RedirOp::AppendClobber
        | RedirOp::DupOut => Some(true),
        RedirOp::Read | RedirOp::HereDoc | RedirOp::HereStr | RedirOp::DupIn => Some(false),
        RedirOp::ReadWrite => None,
    }
}

// The targets of a file redirection: with MULTIOS a pattern or a word
// that splits may name several files, each a target of its own.
fn redir_words(r: &Redir) -> Vec<Redir> {
    let plain = matches!(
        r.op,
        RedirOp::Write
            | RedirOp::Clobber
            | RedirOp::Append
            | RedirOp::AppendClobber
            | RedirOp::Read
    );
    if !plain
        || !crate::options::is_set("multios")
        || r.target.starts_with("<(")
        || r.target.starts_with(">(")
    {
        return vec![r.clone()];
    }
    match crate::expand::expand_words(std::slice::from_ref(&r.target)) {
        Ok(words) if words.len() > 1 => words
            .into_iter()
            .map(|w| Redir {
                target: format!("'{}'", w.replace('\'', "'\\''")),
                ..r.clone()
            })
            .collect(),
        _ => vec![r.clone()],
    }
}

// Copies `from` into every one of `to` (an output multio) or each of
// `from` in turn into `to[0]` (an input multio), in a child process.
fn multio_worker(sources: &[i32], sinks: &[i32], keep: &[i32]) -> i32 {
    let pid = unsafe { libc::fork() };
    if pid != 0 {
        return pid;
    }
    // The worker keeps only its own descriptors.
    for fd in 0..256 {
        if !sources.contains(&fd) && !sinks.contains(&fd) && !keep.contains(&fd) {
            unsafe {
                libc::close(fd);
            }
        }
    }
    let mut buf = [0u8; 8192];
    for &src in sources {
        loop {
            let n = unsafe { libc::read(src, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
            for &dst in sinks {
                let _ = crate::sys::write_fd(dst, &buf[..n as usize]);
            }
        }
    }
    unsafe { libc::_exit(0) }
}

fn move_fd(src: i32, dst: i32) -> bool {
    if src == dst {
        unsafe {
            libc::fcntl(dst, libc::F_SETFD, 0);
        }
        true
    } else {
        let ok = unsafe { libc::dup2(src, dst) } >= 0;
        close(src);
        ok
    }
}

/// Applies redirections. With `save`, the previous descriptors are kept
/// so `restore_redirs` can undo them. With MULTIOS, several output (or
/// input) redirections of one descriptor all take effect, through a
/// copying process.
pub fn apply_redirs(redirs: &[Redir], save: bool) -> Result<SavedFds, RedirError> {
    let mut saved = Vec::new();
    let multios = crate::options::is_set("multios");
    // Per descriptor: direction and the targets so far (owned copies).
    let mut multi: Vec<(i32, bool, Vec<i32>)> = Vec::new();
    let expanded: Vec<Redir> = redirs.iter().flat_map(redir_words).collect();
    for r in &expanded {
        if let Some(var) = &r.var {
            if let Err(e) = redir_to_var(r, var) {
                for (_, _, t) in &multi {
                    t.iter().for_each(|&f| close(f));
                }
                restore_redirs(saved);
                return Err(e);
            }
            continue;
        }
        let fd = r.fd;
        // `>&n` must name a descriptor open before this command, so it is
        // looked up before saving: a saved copy may take that number.
        let dup = matches!(r.op, RedirOp::DupIn | RedirOp::DupOut);
        let early =
            dup.then(|| match open_redir(r) {
                Ok(Target::Dup(n)) if saved.iter().any(|&(_, copy)| copy == n) => Err(Some(
                    format!("{}: {}", n, strerror(libc::EBADF).to_lowercase()),
                )),
                t => t,
            });
        if save && !saved.iter().any(|&(f, _)| f == fd) {
            saved.push((fd, dup_high(fd)));
        }
        let target = match early.unwrap_or_else(|| open_redir(r)) {
            Ok(t) => t,
            Err(msg) => {
                if let Some(m) = msg {
                    shell_err(&m);
                }
                for (_, _, t) in &multi {
                    t.iter().for_each(|&f| close(f));
                }
                restore_redirs(saved);
                return Err(RedirError);
            }
        };
        // Track the target for MULTIOS before installing it.
        let dir = redir_output(r.op);
        if multios {
            let owned = match &target {
                Target::Opened(t) | Target::Both(t) => unsafe {
                    libc::fcntl(*t, libc::F_DUPFD_CLOEXEC, 10)
                },
                Target::Dup(src) => unsafe { libc::fcntl(*src, libc::F_DUPFD_CLOEXEC, 10) },
                Target::Close => -1,
            };
            let pos = multi.iter().position(|(f, _, _)| *f == fd);
            match (pos, dir) {
                (Some(k), Some(d)) if multi[k].1 == d && owned >= 0 => multi[k].2.push(owned),
                (_, Some(d)) if owned >= 0 => {
                    if let Some(k) = pos {
                        let (_, _, old) = multi.remove(k);
                        old.iter().for_each(|&f| close(f));
                    }
                    let mut targets = Vec::new();
                    // A pipeline stage's pipe is the first target.
                    let piped = match (fd, d) {
                        (1, true) => PIPE_OUT.swap(false, std::sync::atomic::Ordering::SeqCst),
                        (0, false) => PIPE_IN.swap(false, std::sync::atomic::Ordering::SeqCst),
                        _ => false,
                    };
                    if piped {
                        targets.push(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) });
                    }
                    targets.push(owned);
                    multi.push((fd, d, targets));
                }
                _ => {
                    if owned >= 0 {
                        close(owned);
                    }
                    if let Some(k) = pos {
                        let (_, _, old) = multi.remove(k);
                        old.iter().for_each(|&f| close(f));
                    }
                }
            }
        }
        let ok = match target {
            Target::Opened(newfd) => {
                let ok = newfd == fd || unsafe { libc::dup2(newfd, fd) } >= 0;
                if newfd != fd {
                    close(newfd);
                } else {
                    unsafe {
                        libc::fcntl(fd, libc::F_SETFD, 0);
                    }
                }
                ok
            }
            Target::Both(newfd) => {
                if save {
                    saved.push((2, dup_high(2)));
                }
                let ok1 = if newfd == fd {
                    unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
                    true
                } else {
                    unsafe { libc::dup2(newfd, fd) >= 0 }
                };
                let ok2 = if newfd == 2 {
                    unsafe { libc::fcntl(2, libc::F_SETFD, 0) };
                    true
                } else {
                    unsafe { libc::dup2(newfd, 2) >= 0 }
                };
                if newfd != fd && newfd != 2 {
                    close(newfd);
                }
                ok1 && ok2
            }
            Target::Dup(src) => src == fd || unsafe { libc::dup2(src, fd) } >= 0,
            Target::Close => {
                close(fd);
                if save && fd == 1 {
                    STDOUT_CLOSED_FOR_CMD.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                true
            }
        };
        if !ok {
            err_line(&format!("meowsh: {}: {}", fd, strerror(last_errno())));
            for (_, _, t) in &multi {
                t.iter().for_each(|&f| close(f));
            }
            restore_redirs(saved);
            return Err(RedirError);
        }
    }
    // Descriptors with several targets get a copying process.
    for (fd, output, targets) in multi {
        if targets.len() < 2 {
            targets.iter().for_each(|&f| close(f));
            continue;
        }
        let Ok((pr, pw)) = pipe_cloexec() else {
            targets.iter().for_each(|&f| close(f));
            continue;
        };
        let pid = if output {
            multio_worker(&[pr], &targets, &[])
        } else {
            multio_worker(&targets, &[pw], &[])
        };
        let mine = if output { pw } else { pr };
        unsafe {
            libc::dup2(mine, fd);
        }
        close(pr);
        close(pw);
        targets.iter().for_each(|&f| close(f));
        if pid > 0 {
            if save {
                saved.push((-2, pid));
            } else if let Ok(mut p) = MULTIO_PIDS.lock() {
                p.push((pid, fd));
            }
        }
    }
    // Only the stage's own redirections join its pipes, not those of
    // the commands inside it.
    PIPE_IN.store(false, std::sync::atomic::Ordering::SeqCst);
    PIPE_OUT.store(false, std::sync::atomic::Ordering::SeqCst);
    Ok(saved)
}

// Whether a pipeline stage has redirections of its own (for MULTIOS).
fn own_redirs(cmd: &Node) -> bool {
    match cmd {
        Node::Simple(s) => !s.redirs.is_empty(),
        Node::Redirected { .. } => true,
        _ => false,
    }
}

// Copying processes started without saved descriptors (in a child about
// to run a command); it waits for them before exiting.
static MULTIO_PIDS: std::sync::Mutex<Vec<(i32, i32)>> = std::sync::Mutex::new(Vec::new());

// Runs the command in place, or, when MULTIOS copiers serve it, as a
// child, so the copiers can be waited for before exiting.
fn exec_with_multios(words: &[String], argv0: Option<&str>, env: &[(String, String)]) -> ! {
    let workers = take_multio_pids();
    if workers.is_empty() {
        exec_external_as(words, argv0, env);
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        for fd in 0..10 {
            close(fd);
        }
        for (_, fd) in &workers {
            if *fd >= 10 {
                close(*fd);
            }
        }
        for (w, _) in workers {
            let mut ws = 0;
            unsafe {
                libc::waitpid(w, &mut ws, 0);
            }
        }
        unsafe { libc::_exit(1) }
    }
    if pid == 0 {
        exec_external_as(words, argv0, env);
    }
    let mut st = 0;
    unsafe {
        libc::waitpid(pid, &mut st, 0);
        for fd in 0..10 {
            libc::close(fd);
        }
        for (_, fd) in &workers {
            if *fd >= 10 {
                libc::close(*fd);
            }
        }
    }
    for (w, _) in workers {
        let mut ws = 0;
        unsafe {
            libc::waitpid(w, &mut ws, 0);
        }
    }
    let code = if libc::WIFEXITED(st) {
        libc::WEXITSTATUS(st)
    } else {
        128 + libc::WTERMSIG(st)
    };
    unsafe { libc::_exit(code) }
}

fn take_multio_pids() -> Vec<(i32, i32)> {
    MULTIO_PIDS
        .lock()
        .map(|mut p| std::mem::take(&mut *p))
        .unwrap_or_default()
}

// `{name}>file`: opens on a new descriptor (10 or above, kept open
// after the command) and stores its number in `name`; `{name}>&-`
// closes the descriptor `name` holds.
fn redir_to_var(r: &Redir, var: &str) -> Result<(), RedirError> {
    let readonly = with_shell(|s| {
        s.param(var)
            .is_some_and(|p| p.attrs & crate::params::READONLY != 0)
    });
    let closing = matches!(r.op, RedirOp::DupIn | RedirOp::DupOut) && r.target == "-";
    let hidden = with_shell(|s| {
        s.param(var)
            .is_some_and(|p| p.attrs & crate::params::PRIVATE_HIDDEN != 0)
    });
    if readonly && hidden {
        shell_err(&crate::shell::readonly_message(var));
        abort_command(1);
        return Err(RedirError);
    }
    if readonly {
        shell_err(&if closing {
            format!(
                "can't close file descriptor from readonly parameter {}",
                var
            )
        } else {
            format!(
                "can't allocate file descriptor to readonly parameter {}",
                var
            )
        });
        return Err(RedirError);
    }
    let held = var_get(var).trim().parse::<i32>().ok();
    let is_open = |fd: i32| unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0;
    if closing {
        let fd = held.unwrap_or(-1);
        if fd < 0 || !is_open(fd) {
            shell_err(&format!(
                "failed to close file descriptor {}: bad file descriptor",
                fd
            ));
            return Err(RedirError);
        }
        close(fd);
        return Ok(());
    }
    // NO_CLOBBER: a parameter already holding an open descriptor is not
    // overwritten (any other value is).
    if let Some(fd) = held.filter(|&fd| fd >= 0 && is_open(fd)) {
        if !crate::options::is_set("clobber") {
            shell_err(&format!(
                "can't clobber parameter {} containing file descriptor {}",
                var, fd
            ));
            return Err(RedirError);
        }
    }
    let src = match open_redir(r) {
        Ok(Target::Opened(fd)) | Ok(Target::Both(fd)) => (fd, true),
        Ok(Target::Dup(fd)) => (fd, false),
        Ok(Target::Close) => return Ok(()),
        Err(msg) => {
            if let Some(m) = msg {
                shell_err(&m);
            }
            return Err(RedirError);
        }
    };
    let fd = unsafe { libc::fcntl(src.0, libc::F_DUPFD, 10) };
    if src.1 {
        close(src.0);
    }
    if fd < 0 || !var_set(var, &fd.to_string(), false) {
        if fd >= 0 {
            close(fd);
        }
        return Err(RedirError);
    }
    Ok(())
}

/// Set while standard output is closed by the running command's own
/// `>&-`: writing to it is then not an error (zsh's `print foo >&-`).
pub static STDOUT_CLOSED_FOR_CMD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn restore_redirs(saved: SavedFds) {
    let mut workers = Vec::new();
    for (fd, copy) in saved.into_iter().rev() {
        if fd == -2 {
            workers.push(copy);
            continue;
        }
        if fd == 1 {
            STDOUT_CLOSED_FOR_CMD.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        if copy >= 0 {
            unsafe {
                libc::dup2(copy, fd);
            }
            close(copy);
        } else {
            close(fd);
        }
    }
    // MULTIOS copiers finish once every writer has closed.
    for pid in workers {
        let mut st = 0;
        unsafe {
            libc::waitpid(pid, &mut st, 0);
        }
    }
}

// ---- assignments ----------------------------------------------------------

enum Value {
    Scalar(String),
    Array(Vec<crate::shell::ArrItem>),
}

// Expands the items of an array assignment. zsh's `[key]=value` items
// (recognized before expansion) set one element or key.
fn expand_array_items(items: &[String]) -> ExpResult<Vec<crate::shell::ArrItem>> {
    use crate::shell::ArrItem;
    let mut out = Vec::new();
    for raw in items {
        if let Some(rest) = raw.strip_prefix('[') {
            let chars: Vec<char> = rest.chars().collect();
            let mut depth = 1;
            let mut k = 0;
            while k < chars.len() {
                match chars[k] {
                    '\\' => k += 1,
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
            let after: String = chars
                .get(k + 1..)
                .map(|c| c.iter().collect())
                .unwrap_or_default();
            let (append, value) = match (after.strip_prefix("+="), after.strip_prefix('=')) {
                (Some(v), _) => (true, Some(v.to_string())),
                (_, Some(v)) => (false, Some(v.to_string())),
                _ => (false, None),
            };
            if let (true, Some(value)) = (k < chars.len(), value) {
                let key: String = chars[..k].iter().collect();
                out.push(ArrItem::Key(
                    expand_string(&key)?,
                    expand_assign_value(&value)?,
                    append,
                ));
                continue;
            }
        }
        for w in expand_words(std::slice::from_ref(raw))? {
            out.push(ArrItem::Plain(w));
        }
    }
    Ok(out)
}

struct Expanded {
    name: String,
    subscript: Option<String>,
    append: bool,
    value: Value,
}

fn expand_assigns(assigns: &[Assign]) -> ExpResult<Vec<Expanded>> {
    let mut out = Vec::new();
    for a in assigns {
        let value = match &a.value {
            // GLOB_ASSIGN: `name=pattern` is globbed, and several matches
            // make the parameter an array.
            AssignValue::Scalar(v)
                if a.subscript.is_none() && crate::options::is_set("globassign") =>
            {
                let mut words = expand_words(std::slice::from_ref(v))?;
                if words.len() > 1 {
                    Value::Array(
                        words
                            .into_iter()
                            .map(crate::shell::ArrItem::Plain)
                            .collect(),
                    )
                } else {
                    // A file name from a pattern is text: a numeric
                    // parameter becomes a plain one, as in zsh.
                    if v.contains(['*', '?', '[']) {
                        with_shell(|s| {
                            if let Some(p) = s.param_mut(&a.name) {
                                p.attrs &= !crate::params::NUMERIC;
                            }
                        });
                    }
                    Value::Scalar(words.pop().unwrap_or_default())
                }
            }
            AssignValue::Scalar(v) => Value::Scalar(expand_assign_value(v)?),
            AssignValue::Array(items) => Value::Array(expand_array_items(items)?),
        };
        out.push(Expanded {
            name: a.name.clone(),
            subscript: a.subscript.clone(),
            append: a.append,
            value,
        });
    }
    Ok(out)
}

fn is_readonly(name: &str) -> bool {
    with_shell(|s| s.param(name).is_some_and(|p| p.attrs & VAR_READONLY != 0))
}

/// `name[sub]=value` (`sub` not yet expanded): one element or key, a
/// `[a,b]` range, or characters of a scalar.
// The kind of the parameter `name` (after namerefs) for `+=` rules.
#[derive(PartialEq)]
enum ParamKind {
    Unset,
    Scalar,
    Numeric,
    Array,
    Assoc,
}

fn param_kind(name: &str) -> ParamKind {
    let target = crate::shell::split_target_pub(&crate::shell::resolve_ref(name));
    with_shell(|s| match s.param(&target) {
        None => ParamKind::Unset,
        Some(p) if p.attrs & crate::params::UNSET != 0 => ParamKind::Unset,
        Some(p) if p.is_assoc() => ParamKind::Assoc,
        Some(p) if p.is_array() => ParamKind::Array,
        Some(p) if p.attrs & crate::params::NUMERIC != 0 => ParamKind::Numeric,
        Some(_) => ParamKind::Scalar,
    })
}

// The 1-based index a subscript (or the end of a `[a,b]` range) names,
// counted from the end when negative.
fn subscript_end(name: &str, sub: &str) -> Result<i64, i32> {
    let text = expand_string(sub).map_err(|e| e.status)?;
    let last = text.rsplit(',').next().unwrap_or(&text);
    let mut e = match crate::arith::eval(last) {
        Ok(n) => n,
        Err(err) => {
            shell_err(&err);
            return Err(1);
        }
    };
    if e >= 0 && crate::options::quick("ksharrays") {
        e += 1;
    }
    if e < 0 {
        let len = with_shell(|s| {
            s.array(name)
                .map(|a| a.len())
                .or_else(|| s.scalar(name).map(|v| v.chars().count()))
                .unwrap_or(0)
        }) as i64;
        e = (len + e + 1).max(0);
    }
    Ok(e)
}

// `name[sub]+=value`: after the subscript's end, the text goes into a
// scalar or onto an array element.
fn append_subscripted(name: &str, sub: &str, v: &str) -> Result<bool, i32> {
    match param_kind(name) {
        ParamKind::Numeric => {
            shell_err("attempt to add to slice of a numeric variable");
            Err(1)
        }
        ParamKind::Assoc => {
            if sub.starts_with('(') {
                shell_err(&format!(
                    "{}: attempt to set slice of associative array",
                    name
                ));
                return Err(1);
            }
            let key = crate::expand::assoc_key(sub).map_err(|e| e.status)?;
            Ok(array_set_subscript(name, &key, v.to_string(), true))
        }
        ParamKind::Scalar => {
            let e = subscript_end(name, sub)?;
            Ok(crate::shell::array_splice(
                name,
                &format!("{},{}", e + 1, e),
                vec![v.to_string()],
            ))
        }
        ParamKind::Array | ParamKind::Unset if matches!(sub, "@" | "*") => {
            Ok(array_append(name, vec![v.to_string()]))
        }
        ParamKind::Array | ParamKind::Unset => {
            let mut e = subscript_end(name, sub)?;
            // array_set_subscript counts from 0 itself with KSH_ARRAYS.
            if crate::options::quick("ksharrays") {
                e -= 1;
            }
            Ok(array_set_subscript(
                name,
                &e.to_string(),
                v.to_string(),
                true,
            ))
        }
    }
}

pub fn set_subscripted(name: &str, sub: &str, v: &str, append: bool) -> Result<bool, i32> {
    let _swap = crate::shell::RefSwap::enter(name);
    if append {
        return append_subscripted(name, sub, v);
    }
    // Through a reference, the kind of what it refers to.
    let target = crate::shell::split_target_pub(&crate::shell::resolve_ref(name));
    let is_assoc = with_shell(|s| s.assoc(&target).is_some())
        || (name == "functions" && crate::zparam::functions_special());
    if sub.contains(',') && !is_assoc {
        let sub = expand_string(sub).map_err(|e| e.status)?;
        return Ok(crate::shell::array_splice(name, &sub, vec![v.to_string()]));
    }
    let key = if is_assoc {
        if sub == "@" || sub == "*" {
            shell_err(&format!(
                "{}: attempt to set slice of associative array",
                name
            ));
            return Err(1);
        }
        // (e): the text is the key exactly.
        match sub.strip_prefix("(e)") {
            Some(k) => crate::expand::assoc_key(k).map_err(|e| e.status)?,
            None => crate::expand::assoc_key(sub).map_err(|e| e.status)?,
        }
    } else if let Some(r) = crate::expand::search_subscript_range(name, sub) {
        r.map_err(|e| e.status)?
    } else {
        let idx = expand_string(sub).map_err(|e| e.status)?;
        match crate::arith::eval(&idx) {
            Ok(n) => n.to_string(),
            Err(e) => {
                shell_err(&e);
                return Err(1);
            }
        }
    };
    // A character of a scalar: `s[2]=x` replaces it.
    let target = crate::shell::resolve_ref(name);
    let scalar = with_shell(|s| s.scalar(&target).is_some());
    if scalar && !append && !is_assoc && key != "0" {
        return Ok(crate::shell::array_splice(
            name,
            &format!("{},{}", key, key),
            vec![v.to_string()],
        ));
    }
    Ok(array_set_subscript(name, &key, v.to_string(), append))
}

fn apply_assign(a: &Expanded, export: bool) -> Result<(), i32> {
    if is_readonly(&a.name) {
        shell_err(&crate::shell::readonly_message(&a.name));
        return Err(1);
    }
    let ok = match (&a.subscript, &a.value) {
        (Some(sub), Value::Scalar(v)) => set_subscripted(&a.name, sub, v, a.append)?,
        (Some(sub), Value::Array(items)) => match param_kind(&a.name) {
            ParamKind::Assoc => {
                shell_err(&format!(
                    "{}: attempt to set slice of associative array",
                    a.name
                ));
                return Err(1);
            }
            ParamKind::Scalar | ParamKind::Numeric if a.append => {
                shell_err(&format!(
                    "{}: attempt to assign array value to non-array",
                    a.name
                ));
                return Err(1);
            }
            _ if a.append => {
                // `a[i]+=(x y)` inserts after element i.
                let e = subscript_end(&a.name, sub)?;
                crate::shell::array_splice(
                    &a.name,
                    &format!("{},{}", e + 1, e),
                    crate::shell::plain_items(items),
                )
            }
            _ => {
                let sub = match crate::expand::search_subscript_range(&a.name, sub) {
                    Some(r) => r.map_err(|e| e.status)?,
                    None => expand_string(sub).map_err(|e| e.status)?,
                };
                crate::shell::array_splice(&a.name, &sub, crate::shell::plain_items(items))
            }
        },
        (None, Value::Array(items)) => {
            // A declared but unset parameter (TYPESET_TO_UNSET) has no
            // value to append to.
            // $argv (the positional parameters) has no parameter of its own.
            let append = a.append
                && (param_kind(&a.name) != ParamKind::Unset
                    || (a.name == "argv" && with_shell(|s| s.param("argv").is_none())));
            crate::shell::array_assign_items(&a.name, items.clone(), append)
        }
        (None, Value::Scalar(v)) if a.append && param_kind(&a.name) == ParamKind::Numeric => {
            // Numeric parameters add.
            let cur = var_get(&a.name);
            match crate::arith::eval_num(&format!("({})+({})", cur, v)) {
                Ok(n) => var_set(&a.name, &n.to_string(), export),
                Err(e) => {
                    shell_err(&e);
                    return Err(1);
                }
            }
        }
        (None, Value::Scalar(v)) if a.append && param_kind(&a.name) == ParamKind::Assoc => {
            with_shell(|s| s.remove_param(&a.name));
            var_set(&a.name, v, export)
        }
        (None, Value::Scalar(v))
            if a.append && is_array(&a.name) && crate::options::quick("ksharrays") =>
        {
            // KSH_ARRAYS: `a+=x` appends to the first element.
            array_set_subscript(&a.name, "0", v.clone(), true)
        }
        (None, Value::Scalar(v)) => {
            if a.append && is_array(&a.name) {
                array_append(&a.name, vec![v.clone()])
            } else if a.append {
                let cur = var_get(&a.name);
                var_set(&a.name, &format!("{}{}", cur, v), export)
            } else {
                var_set(&a.name, v, export)
            }
        }
    };
    if ok && !crate::shell::take_ref_type_error() {
        Ok(())
    } else {
        Err(1)
    }
}

fn assign_text(a: &Expanded) -> String {
    let sub = a
        .subscript
        .as_ref()
        .map(|s| format!("[{}]", s))
        .unwrap_or_default();
    let op = if a.append { "+=" } else { "=" };
    match &a.value {
        Value::Scalar(v) => format!("{}{}{}{}", a.name, sub, op, crate::builtins::shell_quote(v)),
        Value::Array(items) => format!(
            "{}{}{}({})",
            a.name,
            sub,
            op,
            crate::shell::plain_items(items).join(" ")
        ),
    }
}

// Variables assigned for the duration of one builtin or function call.
struct TempVars(Vec<(String, Option<crate::params::Param>)>);

impl TempVars {
    fn set(assigns: &[Expanded]) -> Result<TempVars, i32> {
        Self::set_as(assigns, true)
    }

    // zsh exports prefix assignments only for what may run a program;
    // for other builtins they are plain temporary values.
    fn set_as(assigns: &[Expanded], export: bool) -> Result<TempVars, i32> {
        let mut saved = TempVars(Vec::new());
        for a in assigns {
            // The error ends a script, as zsh's does.
            let fail = |saved: TempVars, st: i32| {
                saved.restore();
                if !interactive() && !crate::options::is_set("continueonerror") {
                    set_error_exit(st);
                }
                Err(st)
            };
            // `HELLO[2,4]=oo cmd`: the element changes for the command
            // too, and so does an array.
            let v = match &a.value {
                Value::Scalar(v) if a.subscript.is_none() => v,
                _ => {
                    let prev = with_shell(|s| s.param(&a.name).cloned());
                    saved.0.push((a.name.clone(), prev));
                    if let Err(st) = apply_assign(a, export) {
                        return fail(saved, st);
                    }
                    continue;
                }
            };
            if is_readonly(&a.name) {
                shell_err(&crate::shell::readonly_message(&a.name));
                return fail(saved, 1);
            }
            let prev = with_shell(|s| s.param(&a.name).cloned());
            saved.0.push((a.name.clone(), prev));
            if !crate::shell::without_assign_warnings(|| var_set(&a.name, v, export)) {
                saved.restore();
                return Err(1);
            }
        }
        Ok(saved)
    }

    fn restore(self) {
        for (name, prev) in self.0.into_iter().rev() {
            // The command made the name local to this function (`x=1 eval
            // 'typeset x'`). As in zsh the old value replaces that local;
            // the temporary one hidden under it must not come back when
            // the function returns.
            with_shell(|s| {
                let depth = s.local_scopes.len();
                let local_now = s.params.get(&name).is_some_and(|p| p.level == depth);
                let was_outer = prev.as_ref().is_none_or(|p| p.level < depth);
                if local_now && was_outer {
                    if let Some(scope) = s.local_scopes.last_mut() {
                        scope.remove(&name);
                    }
                }
            });
            match prev {
                Some(p) => {
                    with_shell(|s| {
                        s.params.insert(name.clone(), p);
                        s.sync_tie(&name);
                    });
                }
                None => {
                    with_shell(|s| s.remove_param(&name));
                }
            }
        }
    }
}

// XTRACE: the command as zsh shows it, assignments first (each followed
// by a space) then the words, quoted where needed.
fn xtrace_cmd(assigns: &[Expanded], words: &[String], decl: Option<&[DeclArg]>) {
    if !opt_set(OPT_XTRACE) || (assigns.is_empty() && words.is_empty()) {
        return;
    }
    let mut line: String = assigns
        .iter()
        .map(|a| format!("{} ", assign_text(a)))
        .collect();
    let formatted: Vec<String> = match decl {
        Some(args) => args
            .iter()
            .map(|a| match a {
                DeclArg::Word(w) => crate::builtins::shell_quote(w),
                DeclArg::Assign {
                    name,
                    append,
                    value,
                } => match value {
                    Some(v) => format!(
                        "{}{}{}",
                        name,
                        if *append { "+=" } else { "=" },
                        crate::builtins::shell_quote(v)
                    ),
                    None => crate::builtins::shell_quote(name),
                },
                DeclArg::Array {
                    name,
                    append,
                    items,
                } => format!(
                    "{}{}=({})",
                    name,
                    if *append { "+=" } else { "" },
                    crate::shell::plain_items(items).join(" ")
                ),
            })
            .collect(),
        None => words
            .iter()
            .map(|w| crate::builtins::shell_quote(w))
            .collect(),
    };
    line.push_str(&formatted.join(" "));
    xtrace_line(&line);
}

// `[[ ... ]]` as XTRACE shows it: operands expanded and quoted,
// patterns with their quoted characters backslashed, no parentheses.
fn dbracket_trace(words: &[String]) -> String {
    let mut out = vec!["[[".to_string()];
    let mut i = 0;
    let mut prev = "";
    while i < words.len() {
        let w = &words[i];
        if matches!(prev, "==" | "=" | "!=") && !matches!(w.as_str(), "(" | ")") {
            let pat = crate::expand::pattern_for_trace(w).unwrap_or_else(|_| w.clone());
            out.push(pat);
            prev = "";
            i += 1;
            continue;
        }
        if matches!(prev, "==" | "=" | "!=") && w == "(" {
            let mut rhs = w.clone();
            let mut depth = 1i32;
            i += 1;
            while i < words.len() {
                if depth == 0 && words[i] != "(" {
                    break;
                }
                depth += (words[i] == "(") as i32 - (words[i] == ")") as i32;
                rhs.push_str(&words[i]);
                i += 1;
                if depth == 0 {
                    if let Some(t) = words.get(i) {
                        if !matches!(t.as_str(), "&&" | "||" | "(" | ")" | "]]") {
                            rhs.push_str(t);
                            i += 1;
                        }
                    }
                }
            }
            let pat = crate::expand::pattern_for_trace(&rhs).unwrap_or(rhs);
            out.push(pat);
            prev = "";
            continue;
        }
        let text = match w.as_str() {
            "(" | ")" => {
                prev = w;
                i += 1;
                continue;
            }
            "&&" | "||" | "!" | "==" | "=" | "!=" | "=~" | "<" | ">" => w.clone(),
            op if op.starts_with('-')
                && op.len() == 2
                && op[1..].chars().all(|c| c.is_ascii_alphabetic()) =>
            {
                w.clone()
            }
            op if op.starts_with('-')
                && matches!(
                    op,
                    "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" | "-nt" | "-ot" | "-ef"
                ) =>
            {
                w.clone()
            }
            _ => crate::builtins::shell_quote(&expand_string(w).unwrap_or_else(|_| w.clone())),
        };
        prev = w;
        i += 1;
        out.push(text);
    }
    out.push("]]".to_string());
    out.join(" ")
}

fn xtrace_line(text: &str) {
    if !opt_set(OPT_XTRACE) {
        return;
    }
    let ps4 = var_get_opt("PS4").unwrap_or_else(|| "+ ".to_string());
    let ps4 = crate::shell::render_prompt(&ps4);
    let line = format!("{}{}\n", ps4, text);
    match XTRACE_FD.with(|t| t.get()) {
        Some(fd) => {
            let _ = crate::sys::write_fd(fd, line.as_bytes());
        }
        None => err_line(line.trim_end_matches('\n')),
    }
}

// Runs a builtin whose redirections are in place (`saved` holds what
// they replaced). The code `.` runs traces to the shell's standard error,
// not the one redirected for the `.` command (as zsh's xtrerr).
fn run_builtin_traced(
    name: &str,
    words: &[String],
    decl: &Option<Vec<DeclArg>>,
    saved: &SavedFds,
) -> i32 {
    let trace = matches!(name, "." | "source")
        .then(|| {
            saved
                .iter()
                .find(|&&(f, copy)| f == 2 && copy >= 0)
                .map(|&(_, c)| c)
        })
        .flatten();
    let outer = XTRACE_FD.with(|t| t.replace(trace.or(t.get())));
    let st = run_builtin(name, words, decl);
    XTRACE_FD.with(|t| t.set(outer));
    st
}

thread_local! {
    // Where xtrace goes while a sourced file runs under its own stderr
    // redirection: the standard error from before it.
    static XTRACE_FD: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

// Declaration utilities take assignment arguments, which expand like
// assignments (no field splitting or globbing).
// Expands a command's words. For declaration commands (`typeset`,
// `local`, `export`, ...) arguments that are assignments expand like
// assignment values, without splitting or globbing, and the command
// also gets them as typed arguments, including array values.
// The index of the word that holds `exec -a`'s name: the word after
// `-a` (or `-ca`), or `-aNAME` itself.
fn exec_argv0_index(raw: &[String]) -> Option<usize> {
    for (i, w) in raw.iter().enumerate().skip(1) {
        let letters = w.strip_prefix('-').filter(|l| !l.is_empty() && *l != "-")?;
        if let Some(p) = letters.find('a') {
            let k = if p + 1 < letters.len() { i } else { i + 1 };
            return (k < raw.len()).then_some(k);
        }
    }
    None
}

fn expand_command_words(
    raw: &[String],
    arrays: &[(usize, Vec<String>)],
) -> ExpResult<(Vec<String>, Option<Vec<DeclArg>>)> {
    let Some(first) = raw.first() else {
        return Ok((Vec::new(), None));
    };
    // zsh reads `exec`'s options before filename generation, so the
    // name after `-a` is not globbed.
    if let Some(k) = (first == "exec").then(|| exec_argv0_index(raw)).flatten() {
        let mut out = expand_words(&raw[..k])?;
        let glob = crate::options::is_set("glob");
        crate::options::set("glob", false);
        let name = expand_words(&raw[k..=k]);
        crate::options::set("glob", glob);
        out.extend(name?);
        out.extend(expand_words(&raw[k + 1..])?);
        return Ok((out, None));
    }
    if !crate::parser::is_decl_word(first) {
        return Ok((expand_words(raw)?, None));
    }
    let mut out = expand_words(&raw[..1])?;
    let mut decl: Vec<DeclArg> = out.iter().cloned().map(DeclArg::Word).collect();
    for (k, w) in raw.iter().enumerate().skip(1) {
        if let Some((_, items)) = arrays.iter().find(|(i, _)| *i == k) {
            let (name, sub, append, _) = assignment_split(w).unwrap_or_default();
            let name = match sub {
                Some(s) => format!("{}[{}]", name, expand_string(&s)?),
                None => name,
            };
            let items = expand_array_items(items)?;
            out.push(format!(
                "{}=({})",
                name,
                crate::shell::plain_items(&items).join(" ")
            ));
            decl.push(DeclArg::Array {
                name,
                append,
                items,
            });
            continue;
        }
        match assignment_split(w) {
            Some((name, sub, append, value)) => {
                let name = match sub {
                    Some(s) => format!("{}[{}]", name, expand_string(&s)?),
                    None => name,
                };
                let value = expand_assign_value(&value)?;
                let op = if append { "+=" } else { "=" };
                out.push(format!("{}{}{}", name, op, value));
                decl.push(DeclArg::Assign {
                    name,
                    append,
                    value: Some(value),
                });
            }
            None => {
                for x in expand_words(std::slice::from_ref(w))? {
                    out.push(x.clone());
                    decl.push(DeclArg::Word(x));
                }
            }
        }
    }
    Ok((out, Some(decl)))
}

// ---- node execution -------------------------------------------------------

pub fn exec_node(node: &Node) -> i32 {
    let mark = procsub_mark();
    let st = exec_node_inner(node);
    procsub_close_from(mark);
    // A `break` inside `${| ... }` takes effect once the command that
    // expanded it has run.
    if let Some(f) = DEFERRED_FLOW.lock().ok().and_then(|mut d| d.take()) {
        with_shell(|s| s.flow = f);
    }
    st
}

/// Loop control raised inside a non-forking substitution, applied after
/// the command using it completes.
pub static DEFERRED_FLOW: std::sync::Mutex<Option<Flow>> = std::sync::Mutex::new(None);

/// Set when the current command is the final command of a `-c` invocation
/// (or a subshell that is itself the final command), so zsh's implicit
/// exec optimization decrements `SHLVL` by 1 for an external command.
pub static EXITING_SHELL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn exec_node_inner(node: &Node) -> i32 {
    if flow_pending() {
        return last_status();
    }
    if opt_set(OPT_NOEXEC) && !interactive() {
        if let Node::Simple(cmd) = node {
            for w in &cmd.words {
                if let Err(e) = crate::expand::validate_noexec_word(w) {
                    return e.status;
                }
            }
        }
        return 0;
    }
    if !matches!(
        node,
        Node::Simple(_) | Node::Subshell(_) | Node::List(_) | Node::Brace(_)
    ) {
        EXITING_SHELL.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    let status = match node {
        Node::Simple(cmd) => {
            let st = exec_simple(cmd, false);
            set_pipestatus(&[st]);
            check_errexit(st);
            st
        }
        Node::Pipeline { cmds, bang } => exec_pipeline(cmds, *bang),
        Node::AndOr { first, rest } => exec_and_or(first, rest),
        Node::List(items) => exec_list(items),
        Node::Subshell(body) => {
            let st = exec_subshell(body);
            check_errexit(st);
            st
        }
        Node::Brace(body) => exec_node(body),
        Node::If {
            branches,
            else_body,
        } => exec_if(branches, else_body.as_deref()),
        Node::Loop { cond, body, until } => exec_loop(cond, body, *until),
        Node::For { vars, words, body } => exec_for(vars, words.as_deref(), body),
        Node::Disowned(inner) => exec_node(inner),
        Node::Always { body, always } => {
            let st = exec_node(body);
            // The cleanup runs even after break, return or an error; a
            // pending flow resumes afterwards unless the cleanup sets one.
            let mut pending = flow();
            set_flow(Flow::None);
            // TRY_BLOCK_ERROR: 1 in the cleanup after an error; setting it
            // to 0 there cancels the error.
            let is_error =
                matches!(pending, Flow::Exit(_) | Flow::Interrupt) && with_shell(|s| s.error_exit);
            let saved = crate::shell::var_get_opt("TRY_BLOCK_ERROR");
            crate::shell::without_assign_warnings(|| {
                crate::shell::var_set("TRY_BLOCK_ERROR", if is_error { "1" } else { "0" }, false)
            });
            exec_node(always);
            if is_error && crate::shell::var_get("TRY_BLOCK_ERROR").trim() == "0" {
                pending = Flow::None;
                with_shell(|s| s.error_exit = false);
            }
            crate::shell::without_assign_warnings(|| match &saved {
                Some(v) => {
                    crate::shell::var_set("TRY_BLOCK_ERROR", v, false);
                }
                None => {
                    crate::shell::var_set("TRY_BLOCK_ERROR", "-1", false);
                }
            });
            // A `return` in the cleanup doesn't replace the try block's
            // own break, continue or return: its status is ignored.
            if flow() == Flow::None || (pending != Flow::None && matches!(flow(), Flow::Return(_)))
            {
                set_flow(pending);
            }
            st
        }
        Node::AnonFunc {
            body,
            args,
            line,
            traced,
        } => match expand_words(args) {
            Ok(mut words) => {
                words.insert(0, "(anon)".to_string());
                set_lineno(*line);
                xtrace_cmd(&[], &words, None);
                let saved = traced.then(|| with_shell(|s| s.opts & OPT_XTRACE != 0));
                if *traced {
                    with_shell(|s| crate::options::set_in(s, "xtrace", true));
                }
                let st = call_function(body, &words, *line);
                if let Some(x) = saved {
                    with_shell(|s| crate::options::set_in(s, "xtrace", x));
                }
                set_last_arg(if words.len() > 1 {
                    words.last().map_or("", |w| w.as_str())
                } else {
                    ""
                });
                check_errexit(st);
                st
            }
            Err(e) => expansion_failed(e),
        },
        Node::Arith(expr, line) => {
            set_lineno(*line);
            let st = arith_status(expr);
            check_errexit(st);
            st
        }
        Node::Repeat { count, body } => exec_repeat(count, body),
        Node::Select { var, words, body } => exec_select(var, words.as_deref(), body),
        Node::Coproc(body) => exec_coproc(body),
        Node::Time(body) => exec_time(body),
        Node::ForArith {
            init,
            cond,
            step,
            body,
        } => exec_for_arith(init, cond, step, body),
        Node::Case { word, items } => exec_case(word, items),
        Node::FuncDef {
            name,
            body,
            line,
            traced,
        } => {
            // zsh expands the name: `function f$$ { ... }`.
            let name = &if name.contains(['$', '`', '\'', '"', '\\']) {
                match expand_string(name) {
                    Ok(n) => n,
                    Err(e) => return expansion_failed(e),
                }
            } else {
                name.clone()
            };
            with_shell(|s| {
                s.functions.insert(
                    name.clone(),
                    FuncDef {
                        body: Arc::new((**body).clone()),
                        origin: s.function_origin(),
                        line: *line,
                    },
                )
            });
            // A new definition starts untraced, unless the function is
            // redefining itself as it runs.
            if *traced {
                crate::shell::set_traced(name, Some(true));
            } else if !crate::shell::with_err_ctx(|e| e.stack.iter().any(|c| c.0 == *name)) {
                crate::shell::set_traced(name, None);
            }
            crate::options::set_sticky(name);
            crate::trap::function_defined(name);
            0
        }
        Node::DBracket(words, line) => {
            set_lineno(*line);
            if opt_set(OPT_XTRACE) {
                xtrace_line(&dbracket_trace(words));
            }
            let st = crate::testexpr::eval_dbracket(words);
            check_errexit(st);
            st
        }
        Node::Redirected { node, redirs } => match apply_redirs(redirs, true) {
            Ok(saved) => {
                let st = exec_node(node);
                restore_redirs(saved);
                st
            }
            Err(RedirError) => {
                check_errexit(1);
                1
            }
        },
        Node::Empty => 0,
    };
    with_shell(|s| s.last_status = status);
    status
}

// Runs a node in a forked child that exits afterwards; a simple command
// replaces the child instead of forking again.
fn exec_in_child(node: &Node) -> ! {
    let st = match node {
        Node::Simple(cmd) => exec_simple(cmd, true),
        _ => exec_node(node),
    };
    exit_child(st)
}

// Records the line of the command running (not inside traps or a
// NO_EVAL_LINENO eval).
fn set_lineno(line: usize) {
    crate::shell::with_err_ctx(|e| {
        if e.frozen == 0 && !crate::trap::keeps_trigger_line(e.stack.len()) {
            e.lineno = line;
        }
    });
}

fn exec_simple(cmd: &SimpleCmd, in_child: bool) -> i32 {
    // A trap runs with the line of the command that triggered it.
    set_lineno(cmd.line);
    // A command other than `exit` resets the CHECK_JOBS warning.
    if cmd.words.first().is_some_and(|w| w != "exit") {
        crate::jobs::EXIT_WARNED.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    exec_simple_inner(cmd, in_child)
}

// The line a command starts on, where the parser kept one.
fn node_line(node: &Node) -> Option<usize> {
    match node {
        Node::Simple(c) => Some(c.line),
        Node::Arith(_, l) | Node::DBracket(_, l) => Some(*l),
        Node::FuncDef { line, .. } | Node::AnonFunc { line, .. } => Some(*line),
        Node::AndOr { first, .. } => node_line(first),
        Node::Pipeline { cmds, .. } => cmds.first().and_then(node_line),
        Node::Redirected { node, .. } | Node::Time(node) => node_line(node),
        _ => None,
    }
}

/// Runs one sublist with zsh's DEBUG trap: before it with
/// DEBUG_BEFORE_CMD (`$ZSH_DEBUG_CMD` holding its text; setting ERR_EXIT
/// in the trap skips it), otherwise after it.
pub fn exec_with_debug(node: &Node) -> i32 {
    DONE_TRAP.store(false, std::sync::atomic::Ordering::Relaxed);
    let debug_set =
        || with_shell(|s| s.traps.contains_key(&crate::trap::SIG_DEBUG)) && !crate::trap::in_trap();
    if matches!(node, Node::List(_)) {
        return exec_node(node);
    }
    // Without DEBUG_BEFORE_CMD the trap runs after the command, so one
    // set by the command itself runs too.
    if !crate::options::is_set("debugbeforecmd") {
        let st = exec_node(node);
        if !debug_set() {
            return st;
        }
        if let Some(l) = node_line(node) {
            crate::shell::with_err_ctx(|e| e.lineno = l);
        }
        crate::trap::run_pseudo(crate::trap::SIG_DEBUG);
        with_shell(|s| s.last_status = st);
        return st;
    }
    if !debug_set() {
        return exec_node(node);
    }
    if let Some(l) = node_line(node) {
        crate::shell::with_err_ctx(|e| {
            if e.frozen == 0 {
                e.lineno = l;
            }
        });
    }
    {
        let text = crate::deparse::Deparser::new(None).body_text(node);
        let errexit_before = opt_set(OPT_ERREXIT);
        with_shell(|s| s.store("ZSH_DEBUG_CMD", crate::params::Value::Scalar(text)));
        // $LINENO in the trap is the command's line.
        crate::shell::with_err_ctx(|e| e.frozen += 1);
        crate::trap::run_pseudo(crate::trap::SIG_DEBUG);
        crate::shell::with_err_ctx(|e| e.frozen -= 1);
        with_shell(|s| {
            s.remove_param("ZSH_DEBUG_CMD");
        });
        // `return` in the trap leaves the function; ERR_EXIT set by the
        // trap skips the command.
        if flow_pending() {
            return last_status();
        }
        if !errexit_before && opt_set(OPT_ERREXIT) {
            with_shell(|s| crate::options::set_in(s, "errexit", false));
            return 1;
        }
    }
    exec_node(node)
}

/// RM_STAR_SILENT unset: an interactive `rm *` or `rm dir/*` asks first.
/// False when the answer is no.
fn rm_star_ok(raw: &[String]) -> bool {
    if raw.first().map(String::as_str) != Some("rm")
        || !interactive()
        || crate::options::is_set("rmstarsilent")
        || !crate::options::is_set("shinstdin")
    {
        return true;
    }
    for w in &raw[1..] {
        let dir = if w == "*" {
            crate::shell::var_get("PWD")
        } else if let Some(d) = w
            .strip_suffix("/*")
            .filter(|d| !d.contains(['\'', '"', '\\', '$', '`']))
        {
            if d.is_empty() {
                "/".to_string()
            } else if d.starts_with('/') {
                d.to_string()
            } else {
                format!(
                    "{}/{}",
                    crate::shell::var_get("PWD").trim_end_matches('/'),
                    d
                )
            }
        } else {
            continue;
        };
        if !check_rm_all(&dir) {
            return false;
        }
    }
    true
}

// zsh's checkrmall(): the query for deleting every file in `dir`.
fn check_rm_all(dir: &str) -> bool {
    let dots = crate::options::is_set("globdots");
    let count = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| dots || !e.file_name().to_string_lossy().starts_with('.'))
                .take(101)
                .count()
        })
        .unwrap_or(0);
    let what = match count {
        n if n > 100 => "more than 100 files in ".to_string(),
        1 => "the only file in ".to_string(),
        0 => "all the files in ".to_string(),
        n => format!("all {} files in ", n),
    };
    let mut msg = format!("zsh: sure you want to delete {}{}", what, dir);
    if crate::options::is_set("rmstarwait") {
        msg.push_str("? (waiting ten seconds)\x07");
        let _ = crate::sys::write_fd(2, msg.as_bytes());
        std::thread::sleep(std::time::Duration::from_secs(10));
        msg = "\n".to_string();
    }
    msg.push_str(" [yn]? \x07");
    let _ = crate::sys::write_fd(2, msg.as_bytes());
    get_query()
}

// zsh's getquery("ny", purge): typeahead counts as no; otherwise one
// key, without echo, y or n (newline means n).
fn get_query() -> bool {
    let fd = 0;
    let mut pending: libc::c_int = 0;
    unsafe {
        libc::ioctl(fd, libc::FIONREAD, &mut pending);
    }
    if pending > 0 {
        let mut b = vec![0u8; pending as usize];
        unsafe {
            libc::read(fd, b.as_mut_ptr() as *mut libc::c_void, b.len());
        }
        let _ = crate::sys::write_fd(2, b"n\n");
        return false;
    }
    let mut old: libc::termios = unsafe { std::mem::zeroed() };
    let tty = unsafe { libc::tcgetattr(fd, &mut old) } == 0;
    if tty {
        let mut t = old;
        t.c_lflag &= !(libc::ECHO | libc::ICANON);
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        unsafe {
            libc::tcsetattr(fd, libc::TCSANOW, &t);
        }
    }
    let mut answer = b'n';
    loop {
        let mut c = [0u8; 1];
        if unsafe { libc::read(fd, c.as_mut_ptr() as *mut libc::c_void, 1) } != 1 {
            break;
        }
        match c[0].to_ascii_lowercase() {
            b'\n' => break,
            b @ (b'y' | b'n') => {
                answer = b;
                break;
            }
            _ => {
                let _ = crate::sys::write_fd(2, b"\x07");
            }
        }
    }
    if tty {
        unsafe {
            libc::tcsetattr(fd, libc::TCSANOW, &old);
        }
    }
    let _ = crate::sys::write_fd(2, &[answer, b'\n']);
    answer == b'y'
}

fn exec_simple_inner(cmd: &SimpleCmd, in_child: bool) -> i32 {
    with_shell(|s| s.last_cmdsub_status = None);
    // zsh precommand modifiers that act before expansion: `noglob`
    // (no filename generation), `nocorrect`, and `-` (dashed argv[0]).
    let (mut noglob, mut dash) = (false, false);
    let mut skip = 0;
    while let Some(w) = cmd.words.get(skip) {
        match w.as_str() {
            "noglob" => noglob = true,
            "nocorrect" => {}
            "-" => dash = true,
            _ => break,
        }
        skip += 1;
    }
    let raw = &cmd.words[skip..];
    if !rm_star_ok(raw) {
        return 1;
    }
    let arrays: Vec<(usize, Vec<String>)> = cmd
        .decl_arrays
        .iter()
        .filter(|(i, _)| *i >= skip)
        .map(|(i, a)| (i - skip, a.clone()))
        .collect();
    let saved_glob = noglob.then(|| {
        let on = crate::options::is_set("glob");
        crate::options::set("glob", false);
        on
    });
    let expanded = expand_command_words(raw, &arrays);
    if let Some(on) = saved_glob {
        crate::options::set("glob", on);
    }
    let (words, decl) = match expanded {
        Ok(w) => w,
        Err(mut e) => {
            if e.status == 1 {
                let prev = last_status();
                if prev != 0 {
                    e.status = prev;
                }
            }
            return expansion_failed(e);
        }
    };
    let assigns = match expand_assigns(&cmd.assigns) {
        Ok(a) => a,
        Err(e) => return expansion_failed(e),
    };
    if flow_pending() {
        return last_status();
    }

    // zsh's null command: `>file` runs $NULLCMD (cat), `<file` alone
    // $READNULLCMD (more); SH_NULLCMD makes it `:`, CSH_NULLCMD an error.
    if cmd.words.is_empty() && cmd.assigns.is_empty() && !cmd.redirs.is_empty() {
        if crate::options::is_set("cshnullcmd") {
            shell_err("redirection with no command");
            return 1;
        }
        if !crate::options::is_set("shnullcmd") {
            // Without NULLCMD there is nothing to run, even for a read
            // that READNULLCMD would serve.
            let Some(null) = var_get_opt("NULLCMD") else {
                shell_err("redirection with no command");
                return 1;
            };
            let reading = cmd.redirs.len() == 1 && cmd.redirs[0].op == RedirOp::Read;
            let name = if reading {
                var_get_opt("READNULLCMD").unwrap_or(null)
            } else {
                null
            };
            if !name.is_empty() {
                return run_command(vec![name], None, &[], &cmd.redirs, in_child, dash);
            }
        }
    }
    if words.is_empty() {
        xtrace_cmd(&assigns, &[], None);
        let saved = match apply_redirs(&cmd.redirs, true) {
            Ok(saved) => saved,
            Err(RedirError) => return 1,
        };
        let mut quiet_status = 0;
        for a in &assigns {
            if let Err(st) = apply_assign(a, false) {
                if crate::shell::take_ref_missing_upper() {
                    quiet_status = st;
                    continue;
                }
                restore_redirs(saved);
                if !interactive() {
                    set_error_exit(st);
                }
                return st;
            }
        }
        restore_redirs(saved);
        return with_shell(|s| s.last_cmdsub_status).unwrap_or(quiet_status);
    }

    xtrace_cmd(&assigns, &words, decl.as_deref());
    // $_: the last non-assignment word of the previous command.
    let last = match decl.as_deref() {
        Some(args) => args.iter().rev().find_map(|a| match a {
            crate::typeset::DeclArg::Word(w) => Some(w.clone()),
            _ => None,
        }),
        None => words.last().cloned(),
    };
    let st = run_command(words, decl, &assigns, &cmd.redirs, in_child, dash);
    if let Some(l) = last {
        set_last_arg(&l);
    }
    st
}

fn set_last_arg(v: &str) {
    with_shell(|s| {
        let p = s
            .params
            .entry("_".to_string())
            .or_insert_with(|| crate::params::Param::scalar(""));
        p.value = crate::params::Value::Scalar(v.to_string());
    });
}

// Runs builtin `name`, giving declaration commands their typed arguments.
fn run_builtin(name: &str, words: &[String], decl: &Option<Vec<DeclArg>>) -> i32 {
    match decl {
        Some(args) if crate::parser::is_decl_word(name) => crate::typeset::typeset(name, args),
        _ => crate::builtins::run(name, words),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_command(
    mut words: Vec<String>,
    mut decl: Option<Vec<DeclArg>>,
    assigns: &[Expanded],
    redirs: &[Redir],
    in_child: bool,
    dash: bool,
) -> i32 {
    // `command [-pvV] name ...` skips function lookup; -v/-V describe
    // the command instead; -p searches the default PATH.
    let mut skip_functions = false;
    let mut default_path = false;
    // `command name`: without POSIX_BUILTINS zsh runs only external
    // commands; with it, builtins too, special ones losing their
    // special assignment rules.
    let mut via_command = false;
    // A function named `command` or `builtin` overrides the builtin.
    let shadowed = |w: &str| with_shell(|s| s.functions.contains_key(w));
    if let Some(bad) = bad_namespace_assignment(&words[0]) {
        shell_err(&format!("not an identifier: {}", bad));
        return 1;
    }
    if words[0] == "command" && !shadowed("command") {
        let mut i = 1;
        let mut describe = None;
        while i < words.len() && words[i].starts_with('-') && words[i] != "-" {
            if words[i] == "--" {
                i += 1;
                break;
            }
            let flags = words[i][1..].to_string();
            if !flags.chars().all(|c| matches!(c, 'p' | 'v' | 'V')) {
                break;
            }
            for c in flags.chars() {
                match c {
                    'p' => default_path = true,
                    'v' => describe = Some("-v"),
                    _ => describe = Some("-V"),
                }
            }
            i += 1;
        }
        if i >= words.len() && describe.is_none() {
            return 0;
        }
        if let Some(flag) = describe {
            // Describe through the builtin, so redirections apply.
            let mut args = vec!["command".to_string(), flag.to_string()];
            args.extend(words[i..].iter().cloned());
            words = args;
        } else {
            words.drain(..i);
            skip_functions = true;
            via_command = true;
        }
        decl = None;
    }
    let external_only = via_command && !crate::options::is_set("posixbuiltins");
    // zsh/bash `builtin name ...` runs the builtin even if a function or
    // alias shadows it.
    let mut builtin_only = false;
    if words[0] == "builtin" && !shadowed("builtin") {
        words.remove(0);
        decl = None;
        if words.is_empty() {
            return 0;
        }
        if !crate::builtins::is_builtin(&words[0]) {
            crate::shell::builtin_err("builtin", &format!("no such builtin: {}", words[0]));
            return 1;
        }
        builtin_only = true;
        skip_functions = true;
    }
    let name = words[0].clone();

    if name == "exec" && !external_only {
        return exec_builtin(&words[1..], assigns, redirs);
    }

    // Assignments before a special builtin outlast it only with
    // POSIX_BUILTINS; otherwise they are temporary, as for any builtin.
    if crate::builtins::is_special_builtin(&name)
        && !via_command
        && (assigns.is_empty() || crate::options::is_set("posixbuiltins"))
    {
        for a in assigns {
            if let Err(st) = apply_assign(a, false) {
                if !interactive() && !crate::options::is_set("continueonerror") {
                    set_error_exit(st);
                }
                return st;
            }
        }
        let saved = match apply_redirs(redirs, true) {
            Ok(s) => s,
            // POSIX_BUILTINS: a special builtin's failed redirection ends
            // a script.
            Err(RedirError) => {
                if !interactive() && crate::options::is_set("posixbuiltins") {
                    set_error_exit(1);
                }
                return 1;
            }
        };
        let st = run_builtin_traced(&name, &words, &decl, &saved);
        restore_redirs(saved);
        return st;
    }

    let func = if skip_functions {
        None
    } else {
        with_shell(|s| s.functions.get(&name).map(|f| (f.body.clone(), f.line)))
            .filter(|_| !crate::builtins::is_disabled('f', &name))
    };
    if let Some((body, line)) = func {
        let tmp = match TempVars::set(assigns) {
            Ok(t) => t,
            Err(st) => return st,
        };
        let st = match apply_redirs(redirs, true) {
            Ok(saved) => {
                let st = call_function(&body, &words, line);
                restore_redirs(saved);
                st
            }
            Err(RedirError) => 1,
        };
        tmp.restore();
        return st;
    }

    // A module builtin that is off may have an autoload (zmodload -ab).
    if !external_only && !crate::builtins::is_builtin(&name) {
        if let Some(r) = crate::modules::autoload_feature(&format!("b:{}", name)) {
            crate::builtins::run_module_setup();
            if let Err(msg) = r {
                if !msg.is_empty() {
                    shell_err(&msg);
                    // zsh treats it as an error that stops the command line.
                    return abort_command(1);
                }
                return 1;
            }
        }
    }
    if builtin_only || (crate::builtins::is_builtin(&name) && !external_only) {
        let export = matches!(
            name.as_str(),
            "exec" | "command" | "builtin" | "nocorrect" | "noglob"
        );
        let tmp = match TempVars::set_as(assigns, export) {
            Ok(t) => t,
            Err(st) => return st,
        };
        let st = match apply_redirs(redirs, true) {
            Ok(saved) => {
                let st = run_builtin_traced(&name, &words, &decl, &saved);
                restore_redirs(saved);
                st
            }
            Err(RedirError) => 1,
        };
        tmp.restore();
        return st;
    }

    // zsh AUTO_CD: a directory name typed as a command changes to it.
    if words.len() == 1
        && interactive()
        && with_shell(|s| s.named_opts.get("autocd").copied().unwrap_or(false))
        && crate::shell::find_command(&name).is_none()
        && std::path::Path::new(&name).is_dir()
    {
        return crate::builtins::run("cd", &["cd".to_string(), name]);
    }

    let mut overrides: Vec<(String, String)> = assigns
        .iter()
        .filter(|a| a.subscript.is_none())
        .filter_map(|a| match &a.value {
            Value::Scalar(v) => Some((a.name.clone(), v.clone())),
            Value::Array(_) => None,
        })
        .collect();
    if EXITING_SHELL.load(std::sync::atomic::Ordering::Relaxed)
        && !overrides.iter().any(|(k, _)| k == "SHLVL")
        && with_shell(|s| {
            s.func_depth == 0 && !s.traps.contains_key(&0) && !s.functions.contains_key("TRAPEXIT")
        })
    {
        if let Ok(n) = var_get("SHLVL").trim().parse::<i64>() {
            overrides.push(("SHLVL".to_string(), (n - 1).max(0).to_string()));
        }
    }
    let env = build_env(&overrides);
    // `-cmd` (the `-` precommand) gives the program a dashed argument zero;
    // `command -p` looks only in the default PATH.
    let argv0 = dash.then(|| format!("-{}", words[0]));
    if default_path && !words[0].contains('/') {
        if let Some(p) = ["/bin", "/usr/bin", "/sbin", "/usr/sbin"]
            .iter()
            .map(|d| format!("{}/{}", d, words[0]))
            .find(|p| std::path::Path::new(p).is_file())
        {
            let name = words[0].clone();
            words[0] = p;
            let argv0 = argv0.unwrap_or(name);
            return run_external(&words, Some(&argv0), &env, redirs, in_child);
        }
    }
    run_external(&words, argv0.as_deref(), &env, redirs, in_child)
}

fn run_external(
    words: &[String],
    argv0: Option<&str>,
    env: &[(String, String)],
    redirs: &[Redir],
    in_child: bool,
) -> i32 {
    if in_child {
        if apply_redirs(redirs, false).is_err() {
            exit_child(1);
        }
        exec_with_multios(words, argv0, env);
    }
    let pid = fork_process(ForkKind::Foreground(0));
    if pid == 0 {
        if apply_redirs(redirs, false).is_err() {
            exit_child(1);
        }
        exec_with_multios(words, argv0, env);
    }
    if pid < 0 {
        return 1;
    }
    wait_foreground(&[pid], pid, &words.join(" "))[0]
}

// exec [-cl] [-a name] [cmd args]: replaces the shell. -a sets the
// command's argument zero, -l prefixes it with `-` (a login shell), -c
// clears the environment.
fn exec_builtin(args: &[String], assigns: &[Expanded], redirs: &[Redir]) -> i32 {
    let (mut argv0, mut login, mut clear) = (None::<String>, false, false);
    let mut i = 0;
    while let Some(a) = args.get(i) {
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
                'c' => clear = true,
                'l' => login = true,
                'a' => {
                    let rest: String = chars.by_ref().collect();
                    argv0 = Some(if rest.is_empty() {
                        i += 1;
                        match args.get(i) {
                            Some(n) => n.clone(),
                            None => {
                                err_line("meowsh: exec: argument expected: -a");
                                return 1;
                            }
                        }
                    } else {
                        rest
                    });
                }
                _ => {
                    err_line(&format!("meowsh: exec: bad option: -{}", c));
                    return 1;
                }
            }
        }
        i += 1;
    }
    let args = &args[i.min(args.len())..];
    if args.is_empty() {
        // Without POSIX_BUILTINS, `x=v exec >file` keeps x as it was, and
        // a failed redirection doesn't end the script.
        let posix = crate::options::is_set("posixbuiltins");
        if posix {
            for a in assigns {
                if let Err(st) = apply_assign(a, false) {
                    return st;
                }
            }
        }
        return match apply_redirs(redirs, false) {
            Ok(_) => 0,
            Err(RedirError) => {
                if !interactive() && posix {
                    set_error_exit(1);
                }
                1
            }
        };
    }
    if apply_redirs(redirs, false).is_err() {
        return 1;
    }
    // Without POSIX_BUILTINS, `exec name` runs a function or builtin of
    // that name, then exits.
    let shell_cmd =
        with_shell(|s| s.functions.contains_key(&args[0])) || crate::builtins::is_builtin(&args[0]);
    if shell_cmd && !crate::options::is_set("posixbuiltins") {
        let st = run_command(args.to_vec(), None, assigns, &[], false, false);
        exit_shell(st);
    }
    let mut overrides: Vec<(String, String)> = assigns
        .iter()
        .filter_map(|a| match &a.value {
            Value::Scalar(v) if a.subscript.is_none() => Some((a.name.clone(), v.clone())),
            _ => None,
        })
        .collect();
    if (crate::shell::SUBSHELL_LEVEL.load(std::sync::atomic::Ordering::Relaxed) == 0
        || EXITING_SHELL.load(std::sync::atomic::Ordering::Relaxed))
        && !overrides.iter().any(|(k, _)| k == "SHLVL")
    {
        if let Ok(n) = var_get("SHLVL").trim().parse::<i64>() {
            overrides.push(("SHLVL".to_string(), (n - 1).max(0).to_string()));
        }
    }
    let env = if clear {
        overrides
    } else {
        build_env(&overrides)
    };
    let mut name = argv0.unwrap_or_else(|| args[0].clone());
    if login {
        name = format!("-{}", name);
    }
    exec_external_as(args, Some(&name), &env)
}

type Frame = (
    std::collections::HashMap<i32, String>,
    u32,
    std::collections::HashMap<String, bool>,
    crate::options::Emulation,
    TrapFuncs,
    Vec<String>,
);

// The TRAPxxx functions defined when a function was entered.
type TrapFuncs = Vec<(String, Option<crate::types::FuncDef>)>;

fn trap_funcs() -> TrapFuncs {
    with_shell(|s| {
        s.functions
            .iter()
            .filter(|(n, _)| n.starts_with("TRAP"))
            .map(|(n, f)| (n.clone(), Some(f.clone())))
            .collect()
    })
}

// zsh function scoping: an EXIT trap set inside a function runs when it
// returns; with LOCAL_TRAPS / LOCAL_OPTIONS (also set by `emulate -L`)
// traps / options go back to their values at entry.
/// Returns the status the function ends with: a `return` in its EXIT
/// trap replaces it (and makes the caller return too, as in zsh).
fn restore_function_frame(entry: Frame, status: i32, posix_traps: bool) -> i32 {
    let mut status = status;
    let (traps, opts, named, emulation, funcs, pat_dis) = entry;
    let (local_traps, local_opts, local_patterns, exit_action) = with_shell(|s| {
        let on = |k: &str| s.named_opts.get(k).copied().unwrap_or(false);
        let exit = s.traps.get(&0).cloned();
        // A TRAPEXIT defined in the function is a new EXIT trap even
        // when one of that name was set on entry.
        let before = funcs
            .iter()
            .find(|(n, _)| n == "TRAPEXIT")
            .and_then(|(_, f)| f.as_ref());
        let now = s.functions.get("TRAPEXIT");
        let func_changed = match (before, now) {
            (Some(a), Some(b)) => !Arc::ptr_eq(&a.body, &b.body),
            (None, Some(_)) => true,
            _ => false,
        };
        let changed =
            !posix_traps && (exit != traps.get(&0).cloned() || (func_changed && exit.is_some()));
        (
            on("localtraps"),
            on("localoptions"),
            on("localpatterns"),
            exit.filter(|_| changed),
        )
    });
    if local_patterns {
        crate::builtins::set_disabled_patterns(&pat_dis);
    }
    if exit_action.is_some() || local_traps {
        let current = with_shell(|s| s.traps.clone());
        let mut sigs: Vec<i32> = current.keys().chain(traps.keys()).copied().collect();
        sigs.sort();
        sigs.dedup();
        for sig in sigs {
            let before = traps.get(&sig).cloned();
            if (sig == 0 && posix_traps) || (sig != 0 && !local_traps) {
                continue;
            }
            if current.get(&sig) != before.as_ref() {
                crate::trap::set_trap(sig, before);
            }
        }
    }
    if local_opts {
        with_shell(|s| {
            crate::options::restore_in(s, &named);
            s.opts = opts;
            s.emulation = emulation;
        });
    }
    if let Some(action) = exit_action {
        with_shell(|s| s.last_status = status);
        let pending = flow();
        set_flow(Flow::None);
        crate::trap::run_action(&action);
        if let Flow::Return(n) = flow() {
            status = n;
        } else if flow() == Flow::None {
            set_flow(pending);
        }
    }
    // LOCAL_TRAPS: TRAPxxx functions go back to those at entry.
    if local_traps {
        with_shell(|s| {
            let keep = |n: &str| posix_traps && n == "TRAPEXIT";
            s.functions.retain(|n, _| {
                !n.starts_with("TRAP") || keep(n) || funcs.iter().any(|(m, _)| m == n)
            });
            for (n, f) in &funcs {
                if keep(n) {
                    continue;
                }
                if let Some(f) = f {
                    s.functions.insert(n.clone(), f.clone());
                }
            }
        });
    }
    status
}

thread_local! {
    // For a sourced file: the file it is sourced from, as $funcfiletrace
    // names it (the shell has already switched to the new file).
    static NEXT_FRAME_FILE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Names the file the next frame is entered from.
pub fn set_next_frame_file(file: String) {
    NEXT_FRAME_FILE.with(|f| f.replace(Some(file)));
}

// Enters a function, eval or sourced file `name` whose lines count from
// `base`, recording where it was called from and where it was defined
// (`def`; an eval is defined where it runs). Returns the caller's line.
fn push_frame(name: &str, base: usize, def: Option<String>) -> usize {
    let file = NEXT_FRAME_FILE
        .with(|f| f.take())
        .unwrap_or_else(code_origin);
    crate::shell::with_err_ctx(|e| {
        let (caller, caller_base) = e
            .stack
            .last()
            .map(|(n, b)| (n.clone(), *b))
            .unwrap_or_else(|| (e.name.clone(), 0));
        let here = e.lineno;
        // Lines of an eval count from 1 in its text: in the file they
        // follow on from the line the eval ran at.
        let file_line = match (e.stack.last(), e.calls.last()) {
            (Some((n, 0)), Some((_, at, _))) if n == "(eval)" => at
                .rsplit_once(':')
                .and_then(|(_, l)| l.parse::<usize>().ok())
                .map_or(here, |l| l + here.saturating_sub(1)),
            _ => here,
        };
        let filetrace = format!("{}:{}", file, file_line);
        e.calls.push((
            format!("{}:{}", caller, here.saturating_sub(caller_base)),
            filetrace.clone(),
            def.unwrap_or(filetrace),
        ));
        e.stack.push((name.to_string(), base));
        here
    })
}

// For each running function, the file it was defined in (when known) and
// the source depth it was called at.
static FUNC_ORIGINS: std::sync::Mutex<Vec<(Option<String>, usize)>> =
    std::sync::Mutex::new(Vec::new());

/// The file the code now running came from, as the prompt's %x shows it:
/// the running function's own file, unless a file sourced since then is
/// running.
pub fn code_origin() -> String {
    let depth = with_shell(|s| s.source_depth);
    let top = FUNC_ORIGINS.lock().ok().and_then(|o| o.last().cloned());
    match top {
        Some((Some(origin), d)) if d == depth => origin,
        _ => with_shell(|s| s.function_origin()),
    }
}

// Set while XTRACE comes from a `functions -T` function.
static TRACE_FROM_T: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn call_function(body: &Arc<Node>, words: &[String], line: usize) -> i32 {
    // A pending `break` from `${ ... }` in the arguments skips the call.
    if let Some(f) = DEFERRED_FLOW.lock().ok().and_then(|mut d| d.take()) {
        with_shell(|s| s.flow = f);
        return last_status();
    }
    let depth = with_shell(|s| s.func_depth);
    // $FUNCNEST limits the depth (zsh's default is 500).
    let limit = var_get_opt("FUNCNEST")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(MAX_FUNC_DEPTH);
    if depth >= limit {
        // zsh names the function it could not enter, at the caller's line.
        crate::shell::with_err_ctx(|e| e.stack.push((words[0].clone(), 0)));
        shell_err("maximum nested function level reached; increase FUNCNEST?");
        crate::shell::with_err_ctx(|e| e.stack.pop());
        with_shell(|s| {
            if s.flow == Flow::None && !s.interactive {
                s.flow = Flow::Exit(1);
            }
        });
        return 1;
    }
    // The file the function was defined in, for the prompt's %x.
    let (origin, def) = with_shell(|s| {
        let f = s
            .functions
            .get(&words[0])
            .filter(|f| Arc::ptr_eq(&f.body, body));
        (
            (f.map(|f| f.origin.clone()), s.source_depth),
            f.map(|f| format!("{}:{}", f.origin, f.line)),
        )
    });
    let def = def.unwrap_or_else(|| format!("{}:{}", code_origin(), line));
    // A body that is one simple command (`f() g`) has no lines of its own:
    // zsh keeps the caller's line number while it runs.
    let one_line = matches!(**body, Node::Simple(_))
        && words[0] != "(anon)"
        && !crate::builtins::is_autoload_stub(&words[0]);
    let saved_line = push_frame(&words[0], if one_line { 0 } else { line }, Some(def));
    // A function with lines of its own counts them, even when called from
    // one that keeps its caller's line.
    let outer_frozen = crate::shell::with_err_ctx(|e| {
        let outer = e.frozen;
        e.frozen = if one_line { outer + 1 } else { 0 };
        outer
    });
    if let Ok(mut o) = FUNC_ORIGINS.lock() {
        o.push(origin);
    }
    // Traced functions (functions -t / -T) run with XTRACE; a named
    // function called from a -T one does not.
    use std::sync::atomic::Ordering;
    let kind = if words[0] == "(anon)" {
        None
    } else {
        crate::shell::trace_kind(&words[0])
    };
    let saved_trace = (
        with_shell(|s| s.opts & OPT_XTRACE != 0),
        TRACE_FROM_T.load(Ordering::SeqCst),
    );
    match kind {
        Some(t) => {
            with_shell(|s| crate::options::set_in(s, "xtrace", true));
            TRACE_FROM_T.store(t, Ordering::SeqCst);
        }
        None if saved_trace.1 && words[0] != "(anon)" => {
            with_shell(|s| crate::options::set_in(s, "xtrace", false));
            TRACE_FROM_T.store(false, Ordering::SeqCst);
        }
        None => {}
    }
    // zsh (FUNCTION_ARGZERO): $0 is the function name while it runs.
    let (saved_params, saved_loops, saved_argv0) = with_shell(|s| {
        s.func_depth += 1;
        s.errreturn_stack
            .push(std::mem::take(&mut s.errreturn_suppressed));
        // zsh: `break` in a function can leave the caller's loop, unless
        // the caller has LOCAL_LOOPS.
        let loops = s.loop_depth;
        (
            std::mem::replace(&mut s.pos_params, words[1..].to_vec()),
            loops,
            if crate::options::is_set_in(s, "functionargzero") {
                std::mem::replace(&mut s.argv0, words[0].clone())
            } else {
                s.argv0.clone()
            },
        )
    });
    // Without POSIX_BUILTINS each function call parses options afresh:
    // OPTIND is 1 inside and restored afterwards.
    let saved_getopts = (!crate::options::is_set("posixbuiltins")).then(|| {
        let saved = (
            crate::shell::var_get_opt("OPTIND"),
            with_shell(|s| s.getopts_pos),
        );
        crate::shell::without_assign_warnings(|| var_set("OPTIND", "1", false));
        with_shell(|s| s.getopts_pos = 0);
        saved
    });
    let local_loops = crate::options::is_set("localloops");
    let tf = trap_funcs();
    let pat_dis = crate::builtins::disabled_patterns();
    let entry = with_shell(|s| {
        (
            s.traps.clone(),
            s.opts,
            s.named_opts.clone(),
            s.emulation,
            tf,
            pat_dis,
        )
    });
    // A function with a different sticky emulation runs in it, and its
    // option changes do not leak out.
    let sticky = crate::options::sticky_of(&words[0]);
    let cur_sticky = crate::options::CUR_STICKY
        .lock()
        .ok()
        .and_then(|c| c.clone());
    let switch_sticky = sticky.is_some() && sticky != cur_sticky;
    if let Some(st) = sticky.as_ref().filter(|_| switch_sticky) {
        crate::options::apply_sticky(st);
        if let Ok(mut c) = crate::options::CUR_STICKY.lock() {
            *c = sticky.clone();
        }
    }
    crate::shell::push_func_scope();
    // A `{ cmd }` body: the trap sees cmd, as zsh's body is the list.
    let inner = match body.as_ref() {
        Node::Brace(b) => b.as_ref(),
        b => b,
    };
    let mut st =
        in_cmd_scope(|| crate::shell::in_eval_context("shfunc", || exec_with_debug(inner)));
    with_shell(|s| {
        if let Flow::Return(n) = s.flow {
            st = n;
            s.flow = Flow::None;
        }
    });
    // LOCAL_LOOPS (as the caller had it): a break or continue stops at
    // the end of the function.
    let stray = local_loops
        && with_shell(|s| {
            let f = matches!(s.flow, Flow::Break(_) | Flow::Continue(_));
            if f {
                s.flow = Flow::None;
            }
            f
        });
    // POSIX_TRAPS (as in force when the function ends): EXIT traps
    // belong to the shell, not the function.
    let posix_traps = crate::options::is_set("posixtraps");
    if switch_sticky {
        with_shell(|s| {
            crate::options::restore_in(s, &entry.2);
            s.opts = entry.1;
            s.emulation = entry.3;
        });
        if let Ok(mut c) = crate::options::CUR_STICKY.lock() {
            *c = cur_sticky;
        }
    }
    // The EXIT trap runs with the caller's positional parameters (zgetopt
    // sets the caller's argv from one).
    // Its locals are gone by then too.
    pop_scope();
    let own_params = with_shell(|s| std::mem::replace(&mut s.pos_params, saved_params.clone()));
    st = restore_function_frame(entry, st, posix_traps);
    let trap_params = with_shell(|s| std::mem::replace(&mut s.pos_params, own_params));
    let saved_params = trap_params;
    if let Some((optind, pos)) = saved_getopts {
        match optind {
            Some(v) => {
                crate::shell::without_assign_warnings(|| var_set("OPTIND", &v, false));
            }
            None => with_shell(|s| {
                s.remove_param("OPTIND");
            }),
        }
        with_shell(|s| s.getopts_pos = pos);
    }
    with_shell(|s| {
        s.pos_params = saved_params;
        s.loop_depth = saved_loops;
        s.argv0 = saved_argv0;
        s.func_depth -= 1;
        s.errreturn_suppressed = s.errreturn_stack.pop().unwrap_or(0);
        s.last_status = st;
    });
    crate::shell::with_err_ctx(|e| {
        e.stack.pop();
        e.calls.pop();
        e.lineno = saved_line;
        e.frozen = outer_frozen;
    });
    if let Ok(mut o) = FUNC_ORIGINS.lock() {
        o.pop();
    }
    // zsh reports it from the caller, at the line of the call.
    if stray {
        shell_err("`break' active at end of function scope");
    }
    if kind.is_some() || saved_trace.1 {
        with_shell(|s| crate::options::set_in(s, "xtrace", saved_trace.0));
        TRACE_FROM_T.store(saved_trace.1, Ordering::SeqCst);
    }
    st
}

fn describe(node: &Node) -> String {
    match node {
        Node::Simple(c) => c.words.join(" "),
        Node::Pipeline { cmds, bang } => {
            let s = cmds.iter().map(describe).collect::<Vec<_>>().join(" | ");
            if *bang {
                format!("! {}", s)
            } else {
                s
            }
        }
        Node::AndOr { first, rest } => {
            let mut s = describe(first);
            for (op, n) in rest {
                s.push_str(if *op == AndOrOp::And { " && " } else { " || " });
                s.push_str(&describe(n));
            }
            s
        }
        Node::Subshell(b) => format!("( {} )", describe(b)),
        Node::Brace(b) => format!("{{ {}; }}", describe(b)),
        Node::Redirected { node, .. } => describe(node),
        Node::List(items) => items
            .iter()
            .map(|i| describe(&i.node))
            .collect::<Vec<_>>()
            .join("; "),
        _ => "...".to_string(),
    }
}

fn exec_pipeline(cmds: &[Node], bang: bool) -> i32 {
    if bang {
        suppress_errexit(1);
    }
    let status = match cmds.len() {
        0 => 0,
        1 => exec_node(&cmds[0]),
        _ => run_pipeline(cmds),
    };
    if bang {
        suppress_errexit(-1);
        return if status == 0 { 1 } else { 0 };
    }
    check_errexit(status);
    status
}

/// zsh's `$pipestatus`: the exit status of each command of the last
/// pipeline.
fn set_pipestatus(codes: &[i32]) {
    let items = codes.iter().map(|c| c.to_string()).collect();
    with_shell(|s| {
        if s.param("pipestatus")
            .is_some_and(|p| p.level > 0 && p.attrs & crate::params::HIDE != 0)
        {
            return;
        }
        s.store("pipestatus", crate::params::Value::Array(items));
    });
}

fn run_pipeline(cmds: &[Node]) -> i32 {
    let text = cmds.iter().map(describe).collect::<Vec<_>>().join(" | ");
    let mut pids = Vec::new();
    let mut pgid = 0;
    let mut prev_read = -1;
    // zsh runs the last stage in the shell itself (so `... | read x`
    // sets x), unless it is an external command. Not under job control,
    // where the stages' process group owns the terminal.
    let in_shell = with_shell(|s| {
        !s.job_control
            && matches!(
                s.emulation,
                crate::options::Emulation::Zsh | crate::options::Emulation::Csh
            )
    }) && cmds.len() > 1
        && runs_in_shell(&cmds[cmds.len() - 1]);
    for (k, cmd) in cmds.iter().enumerate() {
        let last = k + 1 == cmds.len();
        if last && in_shell {
            let saved = dup_high(0);
            move_fd(prev_read, 0);
            PIPE_IN.store(own_redirs(cmd), std::sync::atomic::Ordering::SeqCst);
            let st = exec_node(cmd);
            PIPE_IN.store(false, std::sync::atomic::Ordering::SeqCst);
            if saved >= 0 {
                unsafe {
                    libc::dup2(saved, 0);
                    libc::close(saved);
                }
            } else {
                close(0);
            }
            let mut codes = if pids.is_empty() {
                Vec::new()
            } else {
                wait_foreground(&pids, pgid, &text)
            };
            codes.push(st);
            set_pipestatus(&codes);
            return if opt_set(OPT_PIPEFAIL) {
                codes.iter().rev().find(|&&c| c != 0).copied().unwrap_or(0)
            } else {
                st
            };
        }
        let (r, w) = if last {
            (-1, -1)
        } else {
            match pipe_cloexec() {
                Ok(p) => p,
                Err(e) => {
                    err_line(&format!("meowsh: pipe: {}", e));
                    break;
                }
            }
        };
        let pid = fork_process(ForkKind::Foreground(pgid));
        if pid == 0 {
            // MULTIOS joins a stage's redirections with its pipes.
            PIPE_IN.store(
                prev_read >= 0 && own_redirs(cmd),
                std::sync::atomic::Ordering::SeqCst,
            );
            PIPE_OUT.store(
                w >= 0 && own_redirs(cmd),
                std::sync::atomic::Ordering::SeqCst,
            );
            let mut p_out = w;
            if r >= 0 {
                close(r);
            }
            if prev_read >= 0 && p_out == 0 {
                let hi = dup_high(p_out);
                if hi >= 0 {
                    close(p_out);
                    p_out = hi;
                }
            }
            if prev_read >= 0 {
                move_fd(prev_read, 0);
            }
            if p_out >= 0 {
                move_fd(p_out, 1);
            }
            exec_in_child(cmd);
        }
        close(prev_read);
        close(w);
        prev_read = r;
        if pid < 0 {
            close(r);
            prev_read = -1;
            break;
        }
        if pgid == 0 {
            pgid = pid;
        }
        pids.push(pid);
    }
    close(prev_read);
    if pids.is_empty() {
        return 1;
    }
    let codes = wait_foreground(&pids, pgid, &text);
    set_pipestatus(&codes);
    if opt_set(OPT_PIPEFAIL) {
        codes.iter().rev().find(|&&c| c != 0).copied().unwrap_or(0)
    } else {
        *codes.last().unwrap_or(&0)
    }
}

// Whether a pipeline's last stage can run in the shell: anything but a
// simple command naming an external program.
fn runs_in_shell(node: &Node) -> bool {
    match node {
        Node::Simple(c) => match c.words.first() {
            None => true,
            Some(w) if w.contains(['$', '`', '"', '\'', '\\']) => false,
            Some(w) => {
                crate::builtins::is_builtin(w)
                    || with_shell(|s| s.functions.contains_key(w.as_str()))
            }
        },
        Node::Redirected { node, .. } => runs_in_shell(node),
        Node::Subshell(_) => false,
        _ => true,
    }
}

fn rusage(who: i32) -> libc::rusage {
    let mut r: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe {
        libc::getrusage(who, &mut r);
    }
    r
}

fn tv_secs(tv: libc::timeval) -> f64 {
    tv.tv_sec as f64 + tv.tv_usec as f64 / 1_000_000.0
}

// zsh's printtime: one line of TIMEFMT for `desc`.
fn print_time(desc: &str, elapsed: f64, user: f64, sys: f64, maxrss: i64) {
    let fmt = crate::shell::var_get_opt("TIMEFMT")
        .unwrap_or_else(|| "%J  %U user %S system %P cpu %*E total".to_string());
    let hhmmss = |secs: f64| {
        let mins = secs as i64 / 60;
        let hours = mins / 60;
        let secs = secs - 60.0 * mins as f64;
        let mins = mins - 60 * hours;
        if hours > 0 {
            format!("{}:{:02}:{:05.2}", hours, mins, secs)
        } else if mins > 0 {
            format!("{}:{:05.2}", mins, secs)
        } else {
            format!("{:.3}", secs)
        }
    };
    let pick = |c: char| match c {
        'E' => Some(elapsed),
        'U' => Some(user),
        'S' => Some(sys),
        _ => None,
    };
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&c) = chars.get(i) else {
            break;
        };
        i += 1;
        match c {
            'E' | 'U' | 'S' => out.push_str(&format!("{:4.2}s", pick(c).unwrap())),
            'm' | 'u' | 'n' | '*' => match chars.get(i).and_then(|&x| pick(x)) {
                Some(v) => {
                    i += 1;
                    out.push_str(&match c {
                        'm' => format!("{:.0}ms", v * 1e3),
                        'u' => format!("{:.0}us", v * 1e6),
                        'n' => format!("{:.0}ns", v * 1e9),
                        _ => hhmmss(v),
                    });
                }
                None => {
                    out.push('%');
                    out.push(c);
                }
            },
            'P' => {
                let pct = if elapsed > 0.0 {
                    (100.0 * (user + sys) / elapsed) as i64
                } else {
                    0
                };
                out.push_str(&format!("{}%", pct));
            }
            'M' => out.push_str(&maxrss.to_string()),
            'J' => out.push_str(desc),
            '%' => out.push('%'),
            c => {
                out.push('%');
                out.push(c);
            }
        }
    }
    out.push('\n');
    let _ = crate::sys::write_fd(2, out.as_bytes());
}

// `time pipeline`: a forked job reports its children's times under its
// command text; work done in the shell reports `shell` and `children`.
fn exec_time(body: &Node) -> i32 {
    let start = std::time::Instant::now();
    let (self0, kids0) = (rusage(libc::RUSAGE_SELF), rusage(libc::RUSAGE_CHILDREN));
    let st = exec_node(body);
    let elapsed = start.elapsed().as_secs_f64();
    let (self1, kids1) = (rusage(libc::RUSAGE_SELF), rusage(libc::RUSAGE_CHILDREN));
    let d = |a: libc::timeval, b: libc::timeval| (tv_secs(b) - tv_secs(a)).max(0.0);
    let job = match body {
        Node::Pipeline { .. } | Node::Subshell(_) => true,
        Node::Simple(_) => !runs_in_shell(body),
        _ => false,
    };
    if job {
        print_time(
            &describe(body),
            elapsed,
            d(kids0.ru_utime, kids1.ru_utime),
            d(kids0.ru_stime, kids1.ru_stime),
            kids1.ru_maxrss,
        );
    } else {
        print_time(
            "shell",
            elapsed,
            d(self0.ru_utime, self1.ru_utime),
            d(self0.ru_stime, self1.ru_stime),
            self1.ru_maxrss,
        );
        print_time(
            "children",
            elapsed,
            d(kids0.ru_utime, kids1.ru_utime),
            d(kids0.ru_stime, kids1.ru_stime),
            kids1.ru_maxrss,
        );
    }
    st
}

fn exec_subshell(body: &Node) -> i32 {
    let pid = fork_process(ForkKind::Foreground(0));
    if pid == 0 {
        // The subshell keeps ZERR and ERR_EXIT: a lone failing command
        // trips them in it, as in zsh.
        let st = match body {
            Node::Simple(cmd) => {
                let st = exec_simple(cmd, true);
                check_errexit(st);
                st
            }
            _ => exec_node(body),
        };
        exit_child(st);
    }
    if pid < 0 {
        return 1;
    }
    wait_foreground(&[pid], pid, &describe(body))[0]
}

fn exec_and_or(first: &Node, rest: &[(AndOrOp, Node)]) -> i32 {
    suppress_errexit(1);
    let mut status = exec_node(first);
    suppress_errexit(-1);
    for (k, (op, node)) in rest.iter().enumerate() {
        if flow_pending() {
            break;
        }
        let run = match op {
            AndOrOp::And => status == 0,
            AndOrOp::Or => status != 0,
        };
        if !run {
            continue;
        }
        let last = k + 1 == rest.len();
        if !last {
            suppress_errexit(1);
        }
        status = exec_node(node);
        if !last {
            suppress_errexit(-1);
        }
    }
    status
}

fn exec_list(items: &[ListItem]) -> i32 {
    let mut status = last_status();
    let exiting = EXITING_SHELL.load(std::sync::atomic::Ordering::Relaxed);
    for (k, item) in items.iter().enumerate() {
        if flow_pending() {
            break;
        }
        EXITING_SHELL.store(
            exiting && k + 1 == items.len() && !item.background,
            std::sync::atomic::Ordering::Relaxed,
        );
        status = if item.background {
            exec_background(&item.node)
        } else {
            exec_with_debug(&item.node)
        };
        with_shell(|s| s.last_status = status);
        crate::trap::run_pending();
    }
    status
}

fn exec_background(node: &Node) -> i32 {
    let (node, disown) = match node {
        Node::Disowned(inner) => (inner.as_ref(), true),
        n => (n, false),
    };
    let jc = with_shell(|s| s.job_control);
    let pid = fork_process(ForkKind::Background);
    if pid == 0 {
        if !jc {
            if let Ok(fd) = open_file("/dev/null", libc::O_RDONLY) {
                move_fd(fd, 0);
            }
        }
        exec_in_child(node);
    }
    if pid < 0 {
        return 1;
    }
    with_shell(|s| s.last_bg_pid = pid);
    if disown {
        return 0;
    }
    let id = add_job(pid, &[pid], JobState::Running, &describe(node));
    if interactive() {
        err_line(&format!("[{}] {}", id, pid));
    }
    0
}

/// zsh's command stack, which `%_` shows: the constructs being run.
pub static CMD_STACK: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());

fn in_cmd<R>(name: &'static str, f: impl FnOnce() -> R) -> R {
    if let Ok(mut c) = CMD_STACK.lock() {
        c.push(name);
    }
    let r = f();
    if let Ok(mut c) = CMD_STACK.lock() {
        c.pop();
    }
    r
}

fn in_cmd_scope<R>(f: impl FnOnce() -> R) -> R {
    let saved = CMD_STACK
        .lock()
        .map(|mut c| std::mem::take(&mut *c))
        .unwrap_or_default();
    let r = f();
    if let Ok(mut c) = CMD_STACK.lock() {
        *c = saved;
    }
    r
}

fn exec_if(branches: &[(Node, Node)], else_body: Option<&Node>) -> i32 {
    for (k, (cond, body)) in branches.iter().enumerate() {
        suppress_errexit(1);
        let st = in_cmd(if k == 0 { "if" } else { "elif" }, || exec_node(cond));
        suppress_errexit(-1);
        if flow_pending() {
            return st;
        }
        if st == 0 {
            return in_cmd(if k == 0 { "then" } else { "elif-then" }, || {
                exec_node(body)
            });
        }
    }
    match else_body {
        Some(b) => in_cmd("else", || exec_node(b)),
        None => 0,
    }
}

// After a loop body: Some(true) leaves the loop, Some(false) starts the
// next iteration, None continues normally.
fn exec_repeat(count: &str, body: &Node) -> i32 {
    // zsh reads a bare `?` count as $?.
    let n = match expand_string(count).map(|c| {
        if c.trim() == "?" {
            Ok(last_status() as i64)
        } else {
            crate::arith::eval(&c)
        }
    }) {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => {
            err_line(&format!("meowsh: {}", e));
            return 1;
        }
        Err(e) => return expansion_failed(e),
    };
    let mut status = 0;
    with_shell(|s| s.loop_depth += 1);
    for _ in 0..n.max(0) {
        status = exec_node(body);
        if loop_control() == Some(true) {
            break;
        }
        crate::trap::run_pending();
    }
    with_shell(|s| s.loop_depth -= 1);
    status
}

// zsh's select menu (selectlist in Src/loop.c): items in columns down
// the screen, on standard error.
fn select_menu(items: &[String]) {
    let cols: usize = var_get("COLUMNS").parse().unwrap_or(80);
    let lines: usize = var_get("LINES").parse().unwrap_or(24);
    let mut longest = items
        .iter()
        .map(|i| i.chars().count())
        .max()
        .unwrap_or(0)
        .max(1)
        + 1;
    let mut t0 = items.len();
    while t0 > 0 {
        t0 /= 10;
        longest += 1;
    }
    let (fct, fw) = match (cols.saturating_sub(1)) / (longest + 3) {
        0 => (1, 0),
        n => (n, (cols - 1) / n),
    };
    let colsz = items.len().div_ceil(fct);
    let mut text = String::new();
    for row in 0..colsz.min(lines.saturating_sub(2).max(1)) {
        let mut k = row;
        while k < items.len() {
            let entry = format!("{}) {}", k + 1, items[k]);
            let w = entry.chars().count();
            text.push_str(&entry);
            if w < fw {
                text.push_str(&" ".repeat(fw - w));
            }
            k += colsz;
        }
        text.push('\n');
    }
    err_line_raw(&text);
}

fn err_line_raw(s: &str) {
    let _ = crate::sys::write_fd(2, s.as_bytes());
}

fn exec_select(var: &str, words: Option<&[String]>, body: &Node) -> i32 {
    let items = match words {
        Some(ws) => match expand_words(ws) {
            Ok(v) => v,
            Err(e) => return expansion_failed(e),
        },
        None => with_shell(|s| s.pos_params.clone()),
    };
    if items.is_empty() {
        return 0;
    }
    let mut status = 0;
    with_shell(|s| s.loop_depth += 1);
    select_menu(&items);
    let mut reader = crate::sys::LineReader::new(0);
    loop {
        let ps3 = var_get_opt("PS3").unwrap_or_else(|| "?# ".to_string());
        err_line_raw(&crate::shell::expand_prompt(&ps3));
        let Some(line) = reader.read_line() else {
            var_set("REPLY", "", false);
            err_line_raw("\n");
            break;
        };
        let line = line.trim_end_matches('\n').to_string();
        if line.is_empty() {
            select_menu(&items);
            continue;
        }
        var_set("REPLY", &line, false);
        let choice = line
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|k| items.get(k).cloned())
            .unwrap_or_default();
        var_set(var, &choice, false);
        status = exec_node(body);
        if loop_control() == Some(true) {
            break;
        }
    }
    with_shell(|s| s.loop_depth -= 1);
    status
}

// coproc cmd: runs cmd in the background connected to two pipes, which
// `print -p`, `read -p` and the `>&p`/`<&p` redirections use.
fn exec_coproc(body: &Node) -> i32 {
    let Ok((in_r, in_w)) = pipe_cloexec() else {
        err_line("meowsh: coproc: cannot create pipes");
        return 1;
    };
    let Ok((out_r, out_w)) = pipe_cloexec() else {
        close(in_r);
        close(in_w);
        err_line("meowsh: coproc: cannot create pipes");
        return 1;
    };
    if let Some((a, b)) = with_shell(|s| s.coproc.take()) {
        close(a);
        close(b);
    }
    let pid = fork_process(ForkKind::Background);
    if pid == 0 {
        close(in_w);
        close(out_r);
        let mut out_w = out_w;
        if out_w == 0 {
            let hi = dup_high(out_w);
            if hi >= 0 {
                close(out_w);
                out_w = hi;
            }
        }
        move_fd(in_r, 0);
        move_fd(out_w, 1);
        exec_in_child(body);
    }
    close(in_r);
    close(out_w);
    if pid < 0 {
        close(in_w);
        close(out_r);
        return 1;
    }
    let (w, r) = (dup_high(in_w), dup_high(out_r));
    close(in_w);
    close(out_r);
    with_shell(|s| {
        s.coproc = Some((w, r));
        s.last_bg_pid = pid;
    });
    add_job(pid, &[pid], JobState::Running, &describe(body));
    0
}

fn loop_control() -> Option<bool> {
    with_shell(|s| match s.flow {
        Flow::None => None,
        Flow::Break(n) => {
            s.flow = if n > 1 {
                Flow::Break(n - 1)
            } else {
                Flow::None
            };
            Some(true)
        }
        Flow::Continue(n) => {
            if n > 1 {
                s.flow = Flow::Continue(n - 1);
                Some(true)
            } else {
                s.flow = Flow::None;
                Some(false)
            }
        }
        _ => Some(true),
    })
}

fn exec_loop(cond: &Node, body: &Node, until: bool) -> i32 {
    let mut status = 0;
    with_shell(|s| s.loop_depth += 1);
    loop {
        suppress_errexit(1);
        let c = exec_node(cond);
        suppress_errexit(-1);
        match loop_control() {
            Some(true) => break,
            Some(false) => continue,
            None => {}
        }
        if (c == 0) == until {
            break;
        }
        status = exec_node(body);
        if loop_control() == Some(true) {
            break;
        }
        crate::trap::run_pending();
    }
    with_shell(|s| s.loop_depth -= 1);
    status
}

fn exec_for(vars: &[String], words: Option<&[String]>, body: &Node) -> i32 {
    with_shell(|s| s.last_cmdsub_status = None);
    let items = match words {
        Some(ws) => match expand_words(ws) {
            Ok(v) => v,
            Err(e) => return expansion_failed(e),
        },
        None => with_shell(|s| s.pos_params.clone()),
    };
    // A command substitution in the words sets $? for the first pass.
    with_shell(|s| {
        if let Some(st) = s.last_cmdsub_status.take() {
            s.last_status = st;
        }
    });
    let mut status = 0;
    with_shell(|s| s.loop_depth += 1);
    for chunk in items.chunks(vars.len().max(1)) {
        // A reference as the loop variable is pointed at each word.
        let ok = vars.iter().enumerate().all(|(k, v)| {
            let word = chunk.get(k).map(|s| s.as_str()).unwrap_or("");
            let is_ref = with_shell(|s| {
                s.param(v).map(|p| {
                    (
                        p.attrs & crate::params::NAMEREF != 0,
                        p.attrs & crate::params::READONLY != 0,
                    )
                })
            });
            match is_ref {
                Some((true, true)) => {
                    shell_err(&format!("read-only reference: {}", v));
                    false
                }
                Some((true, false)) => crate::shell::set_ref_target(v, word),
                _ => var_set(v, word, false),
            }
        });
        if !ok {
            status = abort_command(1);
            break;
        }
        status = exec_node(body);
        if loop_control() == Some(true) {
            break;
        }
        crate::trap::run_pending();
    }
    with_shell(|s| s.loop_depth -= 1);
    status
}

// 0 if the expression is non-zero, 1 if zero, 2 on error.
fn arith_status(expr: &str) -> i32 {
    match crate::expand::eval_arith(expr) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(_) => 2,
    }
}

fn exec_for_arith(init: &str, cond: &str, step: &str, body: &Node) -> i32 {
    if !init.is_empty() && crate::expand::eval_arith(init).is_err() {
        return 1;
    }
    let mut status = 0;
    with_shell(|s| s.loop_depth += 1);
    loop {
        if !cond.is_empty() {
            match crate::expand::eval_arith(cond) {
                Ok(false) => break,
                Ok(true) => {}
                Err(_) => {
                    status = 1;
                    break;
                }
            }
        }
        status = exec_node(body);
        if loop_control() == Some(true) {
            break;
        }
        crate::trap::run_pending();
        if !step.is_empty() && crate::expand::eval_arith(step).is_err() {
            status = 1;
            break;
        }
    }
    with_shell(|s| s.loop_depth -= 1);
    status
}

fn exec_case(word: &str, items: &[CaseItem]) -> i32 {
    let w = match expand_string(word) {
        Ok(w) => w,
        Err(e) => return expansion_failed(e),
    };
    let mut status = 0;
    let mut falling = false;
    for item in items {
        if !falling {
            let mut matched = false;
            for p in &item.patterns {
                let pat = match expand_pattern(p) {
                    Ok(p) => p,
                    Err(e) => return expansion_failed(e),
                };
                if crate::pattern::matches(&pat, &w) {
                    matched = true;
                    break;
                }
            }
            if !matched {
                continue;
            }
        }
        status = match &item.body {
            Some(b) => exec_node(b),
            None => 0,
        };
        if flow_pending() {
            break;
        }
        if item.cont {
            falling = false;
            continue;
        }
        if !item.fallthrough {
            break;
        }
        falling = true;
    }
    status
}

// ---- running source text --------------------------------------------------

/// Parses and runs `src` one complete command at a time, so aliases
/// defined by earlier commands apply to later ones. Used for `-c`,
/// `eval`, `.`, and trap actions.
pub fn run_string(src: &str) -> i32 {
    run_string_named(src, None)
}

const MAX_EVAL_DEPTH: usize = 500;

thread_local! {
    static EVAL_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct EvalDepthGuard;

impl Drop for EvalDepthGuard {
    fn drop(&mut self) {
        EVAL_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Like `run_string`, with errors naming `ctx` (`(eval)`, a sourced
/// file) and counting lines from the start of `src`.
pub fn run_string_named(src: &str, ctx: Option<&str>) -> i32 {
    let depth = EVAL_DEPTH.with(|d| d.get());
    if depth >= MAX_EVAL_DEPTH {
        err_line(&format!(
            "meowsh: maximum nested evaluation depth reached: {}",
            MAX_EVAL_DEPTH
        ));
        return 1;
    }
    EVAL_DEPTH.with(|d| d.set(depth + 1));
    let _depth_guard = EvalDepthGuard;
    // NO_EVAL_LINENO: an eval keeps its caller's name and line.
    let frozen = ctx == Some("(eval)") && !crate::options::is_set("evallineno");
    let ctx = ctx.filter(|_| !frozen);
    let saved_line = match ctx {
        Some(c) => push_frame(c, 0, (c != "(eval)").then(|| format!("{}:0", c))),
        None => crate::shell::with_err_ctx(|e| e.lineno),
    };
    if frozen {
        crate::shell::with_err_ctx(|e| e.frozen += 1);
    }
    // zsh parses an `eval` or `-c` string whole before running it (so
    // aliases it defines apply only later); sourced files, and sh mode as
    // in bash, go command by command.
    let whole = ctx.is_none_or(|c| c == "(eval)") && !sh_emulation();
    let run = || {
        if whole {
            run_whole(src)
        } else {
            run_parsed(src)
        }
    };
    let st = if ctx.is_some() {
        in_cmd_scope(run)
    } else {
        run()
    };
    crate::shell::with_err_ctx(|e| {
        if ctx.is_some() {
            e.stack.pop();
            e.calls.pop();
        }
        if frozen {
            e.frozen -= 1;
        }
        e.lineno = saved_line;
    });
    st
}

/// Runs pre-compiled AST nodes (e.g. from `.zwc`) in a named frame without re-parsing.
pub fn run_nodes_named(nodes: &[Node], ctx: Option<&str>) -> i32 {
    let depth = EVAL_DEPTH.with(|d| d.get());
    if depth >= MAX_EVAL_DEPTH {
        err_line(&format!(
            "meowsh: maximum nested evaluation depth reached: {}",
            MAX_EVAL_DEPTH
        ));
        return 1;
    }
    EVAL_DEPTH.with(|d| d.set(depth + 1));
    let _depth_guard = EvalDepthGuard;
    let saved_line = match ctx {
        Some(c) => push_frame(c, 0, (c != "(eval)").then(|| format!("{}:0", c))),
        None => crate::shell::with_err_ctx(|e| e.lineno),
    };
    let run = || {
        let mut status = 0;
        let exiting = EXITING_SHELL.swap(false, std::sync::atomic::Ordering::Relaxed);
        for (k, node) in nodes.iter().enumerate() {
            if flow_pending() {
                break;
            }
            EXITING_SHELL.store(
                exiting && k + 1 == nodes.len(),
                std::sync::atomic::Ordering::Relaxed,
            );
            status = exec_with_debug(node);
            crate::trap::run_pending();
        }
        EXITING_SHELL.store(false, std::sync::atomic::Ordering::Relaxed);
        status
    };
    let st = if ctx.is_some() {
        in_cmd_scope(run)
    } else {
        run()
    };
    crate::shell::with_err_ctx(|e| {
        if ctx.is_some() {
            e.stack.pop();
            e.calls.pop();
        }
        e.lineno = saved_line;
    });
    st
}

fn run_whole(src: &str) -> i32 {
    let mut parser = Parser::new(src, true);
    let mut nodes = Vec::new();
    loop {
        match parser.parse_complete_command() {
            Ok(Some(node)) => nodes.push(node),
            Ok(None) => break,
            Err(e) => {
                let msg = match e {
                    ParseError::Syntax(m) => m,
                    ParseError::Incomplete => "syntax error: unexpected end of file".to_string(),
                };
                // An unclosed quote is reported where the input ends.
                let line =
                    if msg.starts_with("unmatched") && !crate::options::is_set("cshjunkiequotes") {
                        src.matches('\n').count() + 1
                    } else {
                        parser.line()
                    };
                crate::shell::with_err_ctx(|e| e.lineno = line);
                shell_err(&msg);
                let st = parse_error_status();
                with_shell(|s| s.last_status = st);
                return st;
            }
        }
    }
    let mut status = 0;
    let exiting = EXITING_SHELL.swap(false, std::sync::atomic::Ordering::Relaxed);
    for (k, node) in nodes.iter().enumerate() {
        if flow_pending() {
            break;
        }
        EXITING_SHELL.store(
            exiting && k + 1 == nodes.len(),
            std::sync::atomic::Ordering::Relaxed,
        );
        status = exec_with_debug(node);
        crate::trap::run_pending();
    }
    EXITING_SHELL.store(false, std::sync::atomic::Ordering::Relaxed);
    status
}

fn run_parsed(src: &str) -> i32 {
    let mut parser = Parser::new(src, true);
    let mut status = 0;
    loop {
        if flow_pending() {
            break;
        }
        match parser.parse_complete_command() {
            Ok(Some(node)) => {
                // A sourced file's commands get the DEBUG trap too.
                status = exec_with_debug(&node);
                crate::trap::run_pending();
            }
            Ok(None) => break,
            Err(e) => {
                let msg = match e {
                    ParseError::Syntax(m) => m,
                    ParseError::Incomplete => "syntax error: unexpected end of file".to_string(),
                };
                // An unclosed quote is reported where the input ends.
                let line =
                    if msg.starts_with("unmatched") && !crate::options::is_set("cshjunkiequotes") {
                        src.matches('\n').count() + 1
                    } else {
                        parser.line()
                    };
                crate::shell::with_err_ctx(|e| e.lineno = line);
                shell_err(&msg);
                // zsh's `.` gives 126 for a file it can't parse.
                let sourced = crate::shell::EVAL_CONTEXT
                    .lock()
                    .is_ok_and(|c| c.last() == Some(&"file"));
                status = if sourced && !sh_emulation() {
                    126
                } else {
                    parse_error_status()
                };
                with_shell(|s| s.last_status = status);
                break;
            }
        }
    }
    status
}

/// Runs a command line as typed interactively (used by tests).
pub fn execute_line(line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    if interactive() {
        history_add(line);
    }
    run_string(line);
}

/// Adds a typed command line to the history; false when HIST_IGNORE_SPACE
/// drops it (a leading space). The text kept has no leading blanks.
pub fn history_add(line: &str) -> bool {
    let kept = !(line.starts_with(' ') && crate::options::is_set("histignorespace"));
    LINE_IN_HISTORY.store(kept, std::sync::atomic::Ordering::Relaxed);
    if kept {
        history_push(line.trim_start());
    }
    kept
}

// Whether the command line running now is the last history entry.
pub static LINE_IN_HISTORY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Adds `line` to the history as it is (`print -s`).
pub fn history_push(line: &str) {
    with_shell(|s| {
        let mut h = s.history.lock().unwrap_or_else(|e| e.into_inner());
        h.push(line.to_string());
        let excess = h.len().saturating_sub(1000);
        if excess > 0 {
            h.drain(..excess);
        }
    });
}

/// Runs the output of `src` in a subshell and returns its standard output
/// and exit status.
pub fn command_subst(src: &str) -> (String, i32) {
    crate::shell::in_eval_context("cmdsubst", || command_subst_in(src))
}

fn command_subst_in(src: &str) -> (String, i32) {
    let (r, w) = match pipe_cloexec() {
        Ok(p) => p,
        Err(e) => {
            err_line(&format!("meowsh: pipe: {}", e));
            return (String::new(), 1);
        }
    };
    let pid = fork_process(ForkKind::Inline);
    if pid == 0 {
        if r != 1 {
            close(r);
        }
        move_fd(w, 1);
        EXITING_SHELL.store(false, std::sync::atomic::Ordering::Relaxed);
        let base = crate::shell::with_err_ctx(|e| e.lineno.saturating_sub(1));
        let prefixed = format!("{}{}", "\n".repeat(base), src);
        let st = run_string(&prefixed);
        exit_child(st);
    }
    close(w);
    if pid < 0 {
        close(r);
        return (String::new(), 1);
    }
    let bytes = read_all(r);
    close(r);
    let status = loop {
        let mut st = 0;
        let rc = unsafe { libc::waitpid(pid, &mut st, 0) };
        if rc == pid {
            break status_to_exit(st);
        }
        if rc < 0 && last_errno() == libc::EINTR {
            continue;
        }
        break 127;
    };
    (crate::shell::bytes_to_zstr(&bytes), status)
}

// Descriptors handed out by process substitution, closed once the
// command that expanded them finishes.
static PROCSUB_FDS: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());

fn procsub_mark() -> usize {
    PROCSUB_FDS.lock().unwrap_or_else(|e| e.into_inner()).len()
}

fn procsub_close_from(mark: usize) {
    let fds: Vec<i32> = {
        let mut v = PROCSUB_FDS.lock().unwrap_or_else(|e| e.into_inner());
        if v.len() <= mark {
            return;
        }
        v.drain(mark..).collect()
    };
    for fd in fds {
        close(fd);
    }
}

/// `<(src)` (`read` true) or `>(src)`: runs `src` in the background
/// connected to a pipe and returns a `/dev/fd` path for the other end.
pub fn process_subst(src: &str, read: bool) -> Option<String> {
    crate::shell::in_eval_context(if read { "outsubst" } else { "insubst" }, || {
        process_subst_in(src, read)
    })
}

fn process_subst_in(src: &str, read: bool) -> Option<String> {
    let (r, w) = match pipe_cloexec() {
        Ok(p) => p,
        Err(e) => {
            err_line(&format!("meowsh: pipe: {}", e));
            return None;
        }
    };
    let (mine, theirs) = if read { (r, w) } else { (w, r) };
    let pid = fork_process(ForkKind::Inline);
    if pid == 0 {
        let dst = if read { 1 } else { 0 };
        if mine != dst {
            close(mine);
        }
        move_fd(theirs, dst);
        EXITING_SHELL.store(false, std::sync::atomic::Ordering::Relaxed);
        let base = crate::shell::with_err_ctx(|e| e.lineno.saturating_sub(1));
        let prefixed = format!("{}{}", "\n".repeat(base), src);
        let st = run_string(&prefixed);
        exit_child(st);
    }
    close(theirs);
    if pid < 0 {
        close(mine);
        return None;
    }
    // The path must stay valid across exec, so the descriptor is
    // inheritable; it is closed in the shell when the command ends.
    let fd = dup_high(mine);
    close(mine);
    if fd < 0 {
        return None;
    }
    unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
    PROCSUB_FDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(fd);
    Some(format!("/dev/fd/{}", fd))
}

/// One line of input for `run_input`.
pub enum InputLine {
    Line(String),
    Eof,
    Interrupted,
}

/// Reads and runs commands until end of input. `next` supplies lines
/// (its argument is true when a command is incomplete and a continuation
/// prompt is due); `on_command` receives each complete command's text in
/// an interactive shell. Returns the shell's exit status.
pub fn run_input(mut next: impl FnMut(bool) -> InputLine, mut on_command: impl FnMut(&str)) -> i32 {
    let mut buf = String::new();
    // Lines of input already run, so line numbers count from the start.
    let mut lines_done = 0;
    loop {
        let is_interactive = interactive();
        if is_interactive && buf.is_empty() {
            reap_jobs(true);
        }
        let eof = match next(!buf.is_empty()) {
            InputLine::Line(mut l) => {
                // BANG_HIST: `!` history references, in interactive input.
                if is_interactive && l.contains(['!', '^']) && crate::options::is_set("banghist") {
                    let hist =
                        with_shell(|s| s.history.lock().map(|h| h.clone()).unwrap_or_default());
                    match crate::history::expand(&l, &hist) {
                        Ok(new) => {
                            if new != l && unsafe { libc::isatty(0) } == 1 {
                                let _ = crate::sys::write_fd(2, &crate::shell::zstr_to_bytes(&new));
                            }
                            l = new;
                        }
                        Err(e) => {
                            shell_err(&e);
                            buf.clear();
                            with_shell(|s| s.last_status = 1);
                            continue;
                        }
                    }
                }
                if opt_set(OPT_VERBOSE) {
                    let _ = crate::sys::write_fd(2, l.as_bytes());
                }
                buf.push_str(&l);
                false
            }
            InputLine::Eof => true,
            InputLine::Interrupted => {
                buf.clear();
                with_shell(|s| s.last_status = 130);
                continue;
            }
        };
        loop {
            if buf.trim().is_empty() {
                buf.clear();
                break;
            }
            let mut parser = Parser::new(&buf, eof);
            parser.set_line_offset(lines_done);
            match parser.parse_complete_command() {
                Ok(Some(node)) => {
                    let rest = parser.rest();
                    let used = if buf.ends_with(&rest) {
                        &buf[..buf.len() - rest.len()]
                    } else {
                        &buf[..]
                    };
                    lines_done += used.matches('\n').count();
                    let text = used.trim().to_string();
                    // The leading space HIST_IGNORE_SPACE looks for.
                    let spaced = used.trim_start_matches('\n').starts_with([' ', '\t']);
                    buf = rest;
                    if is_interactive {
                        if spaced {
                            on_command(&format!(" {}", text));
                        } else {
                            on_command(&text);
                        }
                        crate::builtins::run_hooks("preexec", &[text.clone(), text.clone(), text]);
                        crate::zle::termctl::mark_output(true);
                    }
                    let st = exec_with_debug(&node);
                    if is_interactive {
                        crate::zle::termctl::mark_output(false);
                    }
                    // PRINT_EXIT_VALUE: report a command line's failure.
                    if st != 0 && crate::options::is_set("printexitvalue") {
                        let name = crate::shell::with_err_ctx(|e| e.name.clone());
                        err_line(&format!("{}: exit {}", name, st));
                    }
                    crate::trap::run_pending();
                    match flow() {
                        Flow::Exit(n) => return n,
                        Flow::None => {}
                        Flow::Interrupt => {
                            with_shell(|s| {
                                s.flow = Flow::None;
                                s.error_exit = false;
                            });
                            buf.clear();
                            break;
                        }
                        _ => set_flow(Flow::None),
                    }
                    if !is_interactive {
                        reap_jobs(false);
                    }
                }
                Ok(None) => {
                    buf.clear();
                    break;
                }
                Err(ParseError::Incomplete) => break,
                Err(ParseError::Syntax(msg)) => {
                    let line = if msg.starts_with("unmatched") {
                        lines_done + buf.matches('\n').count() + 1
                    } else {
                        parser.line()
                    };
                    crate::shell::with_err_ctx(|e| e.lineno = line);
                    shell_err(&msg);
                    let st = parse_error_status();
                    with_shell(|s| s.last_status = st);
                    buf.clear();
                    if !is_interactive {
                        return st;
                    }
                    break;
                }
            }
        }
        if eof {
            break;
        }
    }
    last_status()
}

// `.ns.name=value` that zsh's lexer takes as an assignment but whose name
// is invalid (`.k.`, `.2.b`, `.not.2b`): the name, for the error.
fn bad_namespace_assignment(word: &str) -> Option<&str> {
    let name = word.split_once('=')?.0;
    let name = name.strip_suffix('+').unwrap_or(name);
    let (ns, id) = name.strip_prefix('.')?.split_once('.')?;
    let word_chars = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ns.is_empty() || id.contains('.') || !word_chars(ns) || !word_chars(id) {
        return None;
    }
    (!crate::lexer::is_valid_name(name)).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::SHELL;
    use crate::TEST_LOCK;

    #[test]
    fn test_execute_line_history() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
            shell.interactive = true;
            let mut history = shell.history.lock().unwrap_or_else(|e| e.into_inner());
            history.clear();
        }

        execute_line(": ls -l");
        execute_line(": echo hello");

        {
            let mut shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
            shell.interactive = false;
            let history = shell.history.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(history.len(), 2);
            assert_eq!(history[0], ": ls -l");
            assert_eq!(history[1], ": echo hello");
        }
    }

    #[test]
    fn test_execute_line_history_non_interactive() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
            shell.interactive = false;
            let mut history = shell.history.lock().unwrap_or_else(|e| e.into_inner());
            history.clear();
        }

        execute_line(": ls -l");

        {
            let shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
            let history = shell.history.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(history.len(), 0);
        }
    }

    fn reset_shell() {
        crate::zstyle::clear();
        let mut shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
        shell.params.clear();
        shell.functions.clear();
        shell.local_scopes.clear();
        shell.alias_map(false).clear();
        shell.pos_params.clear();
        shell.last_status = 0;
        shell.loop_depth = 0;
        shell.func_depth = 0;
        shell.flow = Flow::None;
        shell.interactive = true;
        shell.opts = 0;
    }

    fn var(name: &str) -> Option<String> {
        let shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
        shell.scalar(name).map(|v| v.to_string())
    }

    #[test]
    fn test_local_outside_function_acts_like_typeset() {
        // zsh accepts `local` at the top level of a sourced file.
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("local x=1");
        assert_eq!(last_status(), 0);
        assert_eq!(var("x").as_deref(), Some("1"));
    }

    #[test]
    fn test_local_does_not_leak_after_return() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("function f { local x=inner; }");
        execute_line("f");
        assert!(
            var("x").is_none(),
            "x should not exist after function returns"
        );
    }

    #[test]
    fn test_local_restores_prior_global() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("x=outer");
        execute_line("function f { local x=inner; }");
        execute_line("f");
        assert_eq!(var("x").as_deref(), Some("outer"));
    }

    #[test]
    fn test_bare_local_masks_with_empty_then_restores() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("x=outer");
        execute_line("function f { local x; x=inside; }");
        execute_line("f");
        assert_eq!(var("x").as_deref(), Some("outer"));
    }

    #[test]
    fn test_local_repeated_in_same_scope_keeps_original_snapshot() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("x=outer");
        execute_line("function f { local x=first; local x=second; }");
        execute_line("f");
        assert_eq!(var("x").as_deref(), Some("outer"));
    }

    #[test]
    fn test_nested_functions_have_independent_scopes() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("x=outer");
        execute_line("function inner { local x=inner_val; }");
        execute_line("function outer { local x=outer_val; inner; }");
        execute_line("outer");
        assert_eq!(var("x").as_deref(), Some("outer"));
    }

    #[test]
    fn test_local_creates_var_that_did_not_exist() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("function f { local newvar=hi; }");
        execute_line("f");
        assert!(var("newvar").is_none());
    }

    #[test]
    fn test_assignment_without_local_is_global() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("function f { y=set_inside; }");
        execute_line("f");
        assert_eq!(var("y").as_deref(), Some("set_inside"));
    }

    #[test]
    fn test_return_sets_function_status() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("f() { return 3; echo unreachable; }; f; r=$?");
        assert_eq!(var("r").as_deref(), Some("3"));
    }

    #[test]
    fn test_and_or_is_left_associative_at_runtime() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("false && x=a || x=b");
        assert_eq!(var("x").as_deref(), Some("b"));
    }

    #[test]
    fn test_bang_negates_status() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("! false; a=$?; ! true; b=$?");
        assert_eq!(var("a").as_deref(), Some("0"));
        assert_eq!(var("b").as_deref(), Some("1"));
    }

    #[test]
    fn test_execute_line_history_limit() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
            shell.interactive = true;
            let mut history = shell.history.lock().unwrap_or_else(|e| e.into_inner());
            history.clear();
            for i in 0..1000 {
                history.push(format!("cmd{}", i));
            }
        }

        execute_line(": new_cmd");

        {
            let mut shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
            shell.interactive = false;
            let history = shell.history.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(history.len(), 1000);
            assert_eq!(history[999], ": new_cmd");
            assert_eq!(history[0], "cmd1");
        }
    }
}
