//! Signal dispositions and `trap` actions.
//!
//! Handlers only record the signal; trap actions run between commands
//! from `run_pending`, where it is safe to execute shell code.

use crate::shell::with_shell;
use crate::types::Flow;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static PENDING: AtomicU64 = AtomicU64::new(0);
static IN_TRAP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(sig: libc::c_int) {
    if (0..64).contains(&sig) {
        PENDING.fetch_or(1u64 << sig, Ordering::SeqCst);
    }
}

pub const SIGNALS: &[(&str, i32)] = &[
    ("HUP", libc::SIGHUP),
    ("INT", libc::SIGINT),
    ("QUIT", libc::SIGQUIT),
    ("ILL", libc::SIGILL),
    ("TRAP", libc::SIGTRAP),
    ("ABRT", libc::SIGABRT),
    ("BUS", libc::SIGBUS),
    ("FPE", libc::SIGFPE),
    ("KILL", libc::SIGKILL),
    ("USR1", libc::SIGUSR1),
    ("SEGV", libc::SIGSEGV),
    ("USR2", libc::SIGUSR2),
    ("PIPE", libc::SIGPIPE),
    ("ALRM", libc::SIGALRM),
    ("TERM", libc::SIGTERM),
    ("CHLD", libc::SIGCHLD),
    ("CONT", libc::SIGCONT),
    ("STOP", libc::SIGSTOP),
    ("TSTP", libc::SIGTSTP),
    ("TTIN", libc::SIGTTIN),
    ("TTOU", libc::SIGTTOU),
    ("URG", libc::SIGURG),
    ("XCPU", libc::SIGXCPU),
    ("XFSZ", libc::SIGXFSZ),
    ("VTALRM", libc::SIGVTALRM),
    ("PROF", libc::SIGPROF),
    ("WINCH", libc::SIGWINCH),
    ("SYS", libc::SIGSYS),
];

/// zsh's pseudo-signals: ZERR after a command fails, DEBUG around
/// each command. They have no system handler.
pub const SIG_ZERR: i32 = 1001;
pub const SIG_DEBUG: i32 = 1002;

/// Parses a condition for `trap`/`kill`: a name with or without `SIG`, a
/// number, or `EXIT` (0).
pub fn signal_number(spec: &str) -> Option<i32> {
    if let Ok(n) = spec.parse::<i32>() {
        return if n == 0 || SIGNALS.iter().any(|(_, s)| *s == n) {
            Some(n)
        } else {
            None
        };
    }
    let upper = spec.to_ascii_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    match name {
        "EXIT" => return Some(0),
        "ZERR" | "ERR" => return Some(SIG_ZERR),
        "DEBUG" => return Some(SIG_DEBUG),
        _ => {}
    }
    SIGNALS.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
}

/// `$signals`: EXIT, the system's signals by number, then zsh's two
/// pseudo-signals.
pub fn signal_list() -> Vec<String> {
    let mut sigs: Vec<(&str, i32)> = SIGNALS.to_vec();
    sigs.sort_by_key(|(_, n)| *n);
    sigs.dedup_by_key(|(_, n)| *n);
    std::iter::once("EXIT")
        .chain(sigs.iter().map(|(n, _)| *n))
        .chain(["ZERR", "DEBUG"])
        .map(|s| s.to_string())
        .collect()
}

pub fn signal_name(sig: i32) -> String {
    match sig {
        0 => return "EXIT".to_string(),
        SIG_ZERR => return "ZERR".to_string(),
        SIG_DEBUG => return "DEBUG".to_string(),
        _ => {}
    }
    SIGNALS
        .iter()
        .find(|(_, s)| *s == sig)
        .map(|(n, _)| n.to_string())
        .unwrap_or_else(|| sig.to_string())
}

fn set_handler(sig: i32, handler: libc::sighandler_t) {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler;
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut());
    }
}

fn catch(sig: i32) {
    set_handler(
        sig,
        on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
    );
}

/// The disposition the shell itself uses for `sig` when no trap is set.
fn shell_default(sig: i32) {
    let (interactive, job_control) = with_shell(|s| (s.interactive, s.job_control));
    if interactive && sig == libc::SIGINT {
        catch(sig);
    } else if (interactive && (sig == libc::SIGQUIT || sig == libc::SIGTERM))
        || (job_control && matches!(sig, libc::SIGTSTP | libc::SIGTTIN | libc::SIGTTOU))
    {
        set_handler(sig, libc::SIG_IGN);
    } else {
        set_handler(sig, libc::SIG_DFL);
    }
}

/// Sets up the shell's own signal dispositions at startup.
pub fn init_shell_signals() {
    set_handler(libc::SIGPIPE, libc::SIG_DFL);
    for sig in [
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGTERM,
        libc::SIGTSTP,
        libc::SIGTTIN,
        libc::SIGTTOU,
    ] {
        shell_default(sig);
    }
}

/// Installs (`Some`) or resets (`None`) the trap for `sig`.
pub fn set_trap(sig: i32, action: Option<String>) {
    match &action {
        Some(a) => {
            with_shell(|s| s.traps.insert(sig, a.clone()));
            if (1..64).contains(&sig) {
                if a.is_empty() {
                    set_handler(sig, libc::SIG_IGN);
                } else {
                    catch(sig);
                }
            }
        }
        None => {
            with_shell(|s| s.traps.remove(&sig));
            if (1..64).contains(&sig) {
                shell_default(sig);
            }
        }
    }
}

/// Resets dispositions in a freshly forked child. Ignored signals stay
/// ignored; caught ones return to their defaults.
pub fn reset_in_child(background_without_job_control: bool) {
    // zsh's entersubsh() unsets every trap except TRAPxxx functions and the
    // ZERR and DEBUG pseudo-signals, which a subshell keeps.
    let (ignored, kept): (Vec<i32>, Vec<i32>) = with_shell(|s| {
        let is_func = |sig: i32, a: &str| {
            !a.is_empty()
                && a.split(' ').next().is_some_and(|w| {
                    w.strip_prefix("TRAP")
                        .and_then(signal_number)
                        .is_some_and(|n| n == sig)
                })
        };
        s.traps
            .retain(|&sig, a| a.is_empty() || sig >= SIG_ZERR || is_func(sig, a));
        let ignored = s
            .traps
            .iter()
            .filter(|(_, a)| a.is_empty())
            .map(|(&k, _)| k)
            .collect();
        let kept = s
            .traps
            .iter()
            .filter(|(_, a)| !a.is_empty())
            .map(|(&k, _)| k)
            .collect();
        (ignored, kept)
    });
    for &(_, sig) in SIGNALS {
        if sig == libc::SIGKILL || sig == libc::SIGSTOP || kept.contains(&sig) {
            continue;
        }
        if ignored.contains(&sig) {
            set_handler(sig, libc::SIG_IGN);
        } else {
            set_handler(sig, libc::SIG_DFL);
        }
    }
    if background_without_job_control {
        set_handler(libc::SIGINT, libc::SIG_IGN);
        set_handler(libc::SIGQUIT, libc::SIG_IGN);
    }
    PENDING.store(0, Ordering::SeqCst);
    IN_TRAP.store(false, Ordering::SeqCst);
}

/// True while a trap action runs.
pub fn in_trap() -> bool {
    IN_TRAP.load(Ordering::SeqCst)
}

/// A signal that arrived and has a trap set, if any.
pub fn pending_trapped() -> Option<i32> {
    let bits = PENDING.load(Ordering::SeqCst);
    (1..64).find(|&sig| {
        bits & (1u64 << sig) != 0
            && with_shell(|s| s.traps.get(&sig).is_some_and(|a| !a.is_empty()))
    })
}

/// True if a signal arrived that has not been handled yet.
pub fn has_pending() -> bool {
    PENDING.load(Ordering::SeqCst) != 0
}

/// Runs trap actions for signals received since the last call. An
/// untrapped SIGINT in an interactive shell abandons the current command.
pub fn run_pending() {
    if IN_TRAP.load(Ordering::SeqCst) {
        return;
    }
    let bits = PENDING.swap(0, Ordering::SeqCst);
    if bits == 0 {
        return;
    }
    for sig in 1..64 {
        if bits & (1u64 << sig) == 0 {
            continue;
        }
        let action = with_shell(|s| s.traps.get(&sig).cloned());
        match action {
            Some(a) if !a.is_empty() => run_action(&a),
            Some(_) => {}
            None => {
                if sig == libc::SIGINT {
                    with_shell(|s| {
                        if s.flow == Flow::None {
                            s.flow = Flow::Interrupt;
                        }
                    });
                }
            }
        }
    }
}

// How many functions deep the running trap started.
static TRAP_DEPTH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Whether the line number stays that of the command that triggered the
/// running trap: true in the trap's own code, false inside a function it
/// calls (a TRAPxxx function counts its own lines, as in zsh).
pub fn keeps_trigger_line(depth: usize) -> bool {
    IN_TRAP.load(Ordering::SeqCst) && depth <= TRAP_DEPTH.load(Ordering::SeqCst)
}

/// Runs a trap action with `$?` preserved.
pub fn run_action(action: &str) {
    IN_TRAP.store(true, Ordering::SeqCst);
    TRAP_DEPTH.store(
        crate::shell::with_err_ctx(|e| e.stack.len()),
        Ordering::SeqCst,
    );
    let (saved, error_before) = with_shell(|s| (s.last_status, s.error_exit));
    let st = crate::shell::in_eval_context("trap", || crate::exec::run_string(action));
    // An error inside the trap ends the trap, not the code it interrupted.
    with_shell(|s| {
        if s.error_exit && !error_before && matches!(s.flow, Flow::Exit(_)) {
            s.error_exit = false;
            s.flow = Flow::None;
        }
    });
    // A TRAPxxx function returning non-zero makes the function it
    // interrupted return with that status.
    let trap_func = action.starts_with("TRAP") && !action.contains(';');
    with_shell(|s| {
        if trap_func && st != 0 && s.flow == Flow::None && s.func_depth > 0 {
            s.flow = Flow::Return(st);
            s.last_status = st;
        } else if !matches!(s.flow, Flow::Exit(_) | Flow::Return(_)) {
            s.last_status = saved;
        }
    });
    IN_TRAP.store(false, Ordering::SeqCst);
}

/// Runs the trap for pseudo-signal `sig` (ZERR, DEBUG), if one is set and
/// no trap is already running.
pub fn run_pseudo(sig: i32) {
    if IN_TRAP.load(Ordering::SeqCst) {
        return;
    }
    let action = with_shell(|s| s.traps.get(&sig).cloned());
    if let Some(a) = action.filter(|a| !a.is_empty()) {
        run_action(&a);
    }
}

/// zsh: a function named TRAPxxx is the trap for signal xxx. Defining
/// it sets the trap; the function gets the signal number.
pub fn function_defined(name: &str) {
    if let Some(sig) = name.strip_prefix("TRAP").and_then(signal_number) {
        let action = if (1..64).contains(&sig) {
            format!("{} {}", name, sig)
        } else {
            name.to_string()
        };
        set_trap(sig, Some(action));
    }
}

/// Removing a TRAPxxx function removes its trap.
pub fn function_removed(name: &str) {
    if let Some(sig) = name.strip_prefix("TRAP").and_then(signal_number) {
        let is_ours = with_shell(|s| {
            s.traps
                .get(&sig)
                .is_some_and(|a| a.split(' ').next() == Some(name))
        });
        if is_ours {
            set_trap(sig, None);
        }
    }
}

/// Runs the EXIT trap once, if set.
pub fn run_exit_trap() {
    let action = with_shell(|s| s.traps.remove(&0));
    if let Some(a) = action {
        if !a.is_empty() {
            with_shell(|s| s.flow = Flow::None);
            run_action(&a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_names_round_trip() {
        assert_eq!(signal_number("INT"), Some(libc::SIGINT));
        assert_eq!(signal_number("sigterm"), Some(libc::SIGTERM));
        assert_eq!(signal_number("EXIT"), Some(0));
        assert_eq!(signal_number("0"), Some(0));
        assert_eq!(signal_number("NOPE"), None);
        assert_eq!(signal_name(libc::SIGHUP), "HUP");
    }
}
