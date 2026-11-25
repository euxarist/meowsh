use crate::exec::InputLine;
use crate::shell::build_prompt;
use std::cell::RefCell;

fn continuation_prompt() -> String {
    let ps2 = crate::shell::var_get_opt("PROMPT2")
        .filter(|p| !p.is_empty())
        .or_else(|| crate::shell::var_get_opt("PS2"))
        .unwrap_or_else(|| "> ".to_string());
    crate::shell::render_prompt(&ps2)
}

fn primary_prompt() -> String {
    crate::builtins::run_hooks("precmd", &[]);
    build_prompt()
}

/// NOTIFY: while waiting for input on `fd`, reports jobs as they finish.
fn wait_input_notify(fd: i32) {
    if !crate::options::is_set("notify") {
        return;
    }
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, 50) } != 0 {
            break;
        }
        crate::jobs::reap_jobs(true);
    }
}

/// Reads commands from `fd` until end of input. Prompts go to standard
/// error when `prompts` is set (an interactive shell without a terminal).
pub fn run_fd(fd: i32, prompts: bool) -> i32 {
    let mut reader = crate::sys::LineReader::new(fd);
    crate::exec::run_input(
        |cont| {
            if prompts {
                let p = if cont {
                    continuation_prompt()
                } else {
                    primary_prompt()
                };
                let _ = crate::sys::write_fd(2, p.as_bytes());
            }
            if prompts {
                wait_input_notify(fd);
            }
            match reader.read_line() {
                Some(l) => InputLine::Line(l),
                None => InputLine::Eof,
            }
        },
        |text| {
            crate::exec::history_add(text);
        },
    )
}

/// The interactive read-eval loop with line editing and history.
pub fn run_interactive() -> i32 {
    if unsafe { libc::isatty(0) } == 1 {
        return run_zle();
    }
    run_fd(0, true)
}

thread_local! {
    // The history line of the command running now, and when it started.
    static RUNNING: RefCell<Option<(crate::history::Appended, std::time::Instant)>> =
        const { RefCell::new(None) };
}

// Appends an accepted line to the history file with the time it starts.
fn save_history_entry(path: &str, line: &str) {
    if path.is_empty() {
        return;
    }
    let start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Ok(a) = crate::history::append_timed(path, line, start) {
        RUNNING.with(|r| *r.borrow_mut() = Some((a, std::time::Instant::now())));
    }
}

// Back at the prompt: the last command's line gets its elapsed time.
fn finish_history_entry() {
    if let Some((a, began)) = RUNNING.with(|r| r.borrow_mut().take()) {
        let _ = crate::history::finish_timed(&a, began.elapsed().as_secs());
    }
}

/// The interactive loop with meowsh's own line editor (ZLE). With the
/// zle option off, lines are read as the terminal delivers them, after
/// a prompt on standard error, as zsh does.
fn run_zle() -> i32 {
    // A descriptor of its own, as widgets run with standard input on
    // /dev/null.
    let tty = crate::sys::dup_high(0);
    crate::sys::set_cloexec(tty);
    crate::zle::refresh::set_tty_fd(tty);
    let history_file = crate::shell::with_shell(|s| s.history_file.clone());
    if !history_file.is_empty() {
        let entries = crate::history::read_own_file(&history_file);
        crate::shell::with_shell(|s| {
            *s.history.lock().unwrap_or_else(|e| e.into_inner()) = entries
        });
    }
    let mut plain = crate::sys::LineReader::new(0);
    let mut pending = String::new();
    let status = crate::exec::run_input(
        |cont| {
            finish_history_entry();
            let prompt = if cont {
                continuation_prompt()
            } else {
                pending.clear();
                primary_prompt()
            };
            if !crate::options::is_set("zle") {
                let _ = crate::sys::write_fd(2, prompt.as_bytes());
                wait_input_notify(0);
                return match plain.read_line() {
                    Some(l) => {
                        pending.push_str(&l);
                        InputLine::Line(l)
                    }
                    None => InputLine::Eof,
                };
            }
            let rp_names = if cont {
                ["RPROMPT2", "RPS2"]
            } else {
                ["RPROMPT", "RPS1"]
            };
            let rprompt = rp_names
                .iter()
                .find_map(|n| crate::shell::var_get_opt(n).filter(|v| !v.is_empty()))
                .map(|v| crate::shell::render_prompt(&v))
                .unwrap_or_default();
            crate::zle::set_prebuffer(&pending);
            let ctx = if cont {
                crate::zle::ZLCON_LINE_CONT
            } else {
                crate::zle::ZLCON_LINE_START
            };
            match crate::zle::zleread(
                &prompt,
                &rprompt,
                crate::zle::ZLRF_HISTORY,
                ctx,
                "zle-line-init",
                "zle-line-finish",
                None,
            ) {
                crate::zle::ReadResult::Line(l) => {
                    pending.push_str(&l);
                    InputLine::Line(l)
                }
                crate::zle::ReadResult::Interrupted => InputLine::Interrupted,
                crate::zle::ReadResult::Eof => InputLine::Eof,
            }
        },
        |text| {
            if !crate::exec::history_add(text) {
                return;
            }
            save_history_entry(&history_file, text.trim_start());
        },
    );
    status
}
