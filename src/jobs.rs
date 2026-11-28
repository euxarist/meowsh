use crate::shell::{builtin_err, with_shell};
use crate::sys::{err_line, out};
use crate::types::{Job, JobState, ProcState, Process};
use std::sync::atomic::{AtomicI32, Ordering};

/// Descriptor for the controlling terminal while job control is on.
pub static TTY_FD: AtomicI32 = AtomicI32::new(-1);

pub fn status_to_exit(status: i32) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else if libc::WIFSTOPPED(status) {
        128 + libc::WSTOPSIG(status)
    } else {
        0
    }
}

fn job_control() -> bool {
    with_shell(|s| s.job_control)
}

fn give_terminal_to(pgid: i32) {
    let fd = TTY_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        unsafe {
            libc::tcsetpgrp(fd, pgid);
        }
    }
}

fn reclaim_terminal() {
    let pgid = with_shell(|s| s.shell_pgid);
    give_terminal_to(pgid);
}

/// Adds a job and returns its number.
pub fn add_job(pgid: i32, pids: &[i32], state: JobState, text: &str) -> i32 {
    with_shell(|s| {
        let mut next = s.next_job_id.lock().unwrap_or_else(|e| e.into_inner());
        let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if jobs.is_empty() {
            // A subshell's own jobs start at 2: zsh keeps job 1 for the
            // subshell itself.
            *next = if s.in_subshell { 2 } else { 1 };
            if s.in_subshell {
                if let Ok(mut o) = OLD_JOBS.lock() {
                    o.clear();
                }
            }
        }
        let id = *next;
        *next += 1;
        let proc_state = match state {
            JobState::Stopped => ProcState::Stopped,
            JobState::Running => ProcState::Running,
            JobState::Done => ProcState::Done,
        };
        jobs.push(Job {
            id,
            pgid,
            procs: pids
                .iter()
                .map(|&pid| Process {
                    pid,
                    status: 0,
                    state: proc_state,
                    cmd: text.to_string(),
                })
                .collect(),
            state,
            notified: false,
            foreground: false,
            cmd_text: text.to_string(),
        });
        id
    })
}

pub fn job_update_status(pid: i32, status: i32) {
    with_shell(|s| {
        let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        for job in jobs.iter_mut() {
            if let Some(proc) = job.procs.iter_mut().find(|p| p.pid == pid) {
                if libc::WIFSTOPPED(status) {
                    proc.state = ProcState::Stopped;
                    proc.status = status;
                } else if libc::WIFCONTINUED(status) {
                    proc.state = ProcState::Running;
                } else {
                    proc.state = ProcState::Done;
                    proc.status = status;
                }
                job.state = if job.procs.iter().all(|p| p.state == ProcState::Done) {
                    JobState::Done
                } else if job.procs.iter().any(|p| p.state == ProcState::Stopped) {
                    JobState::Stopped
                } else {
                    JobState::Running
                };
                job.notified = false;
                return;
            }
        }
    })
}

/// Collects status changes of background children without blocking.
/// With `notify`, finished jobs are reported and forgotten.
pub fn reap_jobs(notify: bool) {
    loop {
        let mut status = 0;
        let pid = unsafe {
            libc::waitpid(
                -1,
                &mut status,
                libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED,
            )
        };
        if pid <= 0 {
            break;
        }
        let known = with_shell(|s| {
            s.jobs
                .lock()
                .map(|j| j.iter().any(|x| x.procs.iter().any(|p| p.pid == pid)))
                .unwrap_or(false)
        });
        if known {
            job_update_status(pid, status);
        } else {
            remember_bg_status(pid, status);
        }
    }
    // zsh: without an interactive job-control shell to report them,
    // finished jobs are forgotten (their statuses stay for `wait`).
    if zsh_mode() && !with_shell(|s| s.interactive && s.job_control) {
        let done: Vec<Job> = with_shell(|s| {
            let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
            let done = jobs
                .iter()
                .filter(|j| j.state == JobState::Done)
                .cloned()
                .collect();
            jobs.retain(|j| j.state != JobState::Done);
            done
        });
        for j in done {
            for p in j.procs {
                remember_bg_status(p.pid, p.status);
            }
        }
    }
    if notify && zsh_mode() {
        // zsh: `[1]  + done       cmd`, with LONG_LIST_JOBS the pids.
        let long = crate::options::is_set("longlistjobs");
        let text = with_shell(|s| {
            let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
            // A finished job keeps the mark it had while running.
            let mut ids: Vec<i32> = jobs.iter().map(|j| j.id).collect();
            ids.sort();
            let cur = ids.pop();
            let prev = ids.pop();
            let mut text = String::new();
            for j in jobs
                .iter_mut()
                .filter(|j| j.state == JobState::Stopped && !j.notified)
            {
                j.notified = true;
                text.push_str(&zsh_job_text(j, cur, prev, long, false));
            }
            for j in jobs.iter().filter(|j| j.state == JobState::Done) {
                text.push_str(&zsh_job_text(j, cur, prev, long, false));
            }
            jobs.retain(|j| j.state != JobState::Done);
            text
        });
        if !text.is_empty() {
            let _ = crate::sys::write_fd(2, text.as_bytes());
        }
    } else if notify {
        let done: Vec<(i32, String)> = with_shell(|s| {
            let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
            let done = jobs
                .iter()
                .filter(|j| j.state == JobState::Done)
                .map(|j| (j.id, j.cmd_text.clone()))
                .collect();
            jobs.retain(|j| j.state != JobState::Done);
            done
        });
        for (id, text) in done {
            err_line(&format!("[{}]+  Done\t\t{}", id, text));
        }
    }
}

// `wait`: a trapped signal interrupts it; its trap runs and the status
// is 128 plus the signal (Err).
fn wait_interruptible(pid: i32) -> Result<i32, i32> {
    loop {
        let mut status = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            return Ok(status);
        }
        if r < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return Ok(127 << 8);
        }
        if let Some(sig) = crate::trap::pending_trapped() {
            crate::trap::run_pending();
            return Err(sig);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn waitpid_retry(pid: i32, flags: i32) -> Option<i32> {
    loop {
        let mut status = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, flags) };
        if r == pid {
            return Some(status);
        }
        if r < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return None;
    }
}

/// Waits for a foreground pipeline. Returns one exit code per process.
/// A stopped pipeline becomes a background job.
pub fn wait_foreground(pids: &[i32], pgid: i32, text: &str) -> Vec<i32> {
    let jc = job_control();
    let flags = if jc { libc::WUNTRACED } else { 0 };
    let mut codes = vec![0; pids.len()];
    let mut stopped_at = None;
    let mut stop_status = 0;
    for (k, &pid) in pids.iter().enumerate() {
        match waitpid_retry(pid, flags) {
            Some(st) => {
                codes[k] = status_to_exit(st);
                if libc::WIFSTOPPED(st) {
                    stopped_at = Some(k);
                    stop_status = st;
                    break;
                }
                if libc::WIFSIGNALED(st) && libc::WTERMSIG(st) == libc::SIGINT {
                    with_shell(|s| {
                        if s.interactive {
                            s.flow = crate::types::Flow::Interrupt;
                        }
                    });
                }
            }
            None => codes[k] = 127,
        }
    }
    if jc {
        reclaim_terminal();
    }
    if let Some(k) = stopped_at {
        let id = add_job(pgid, pids, JobState::Stopped, text);
        with_shell(|s| {
            let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(job) = jobs.iter_mut().find(|j| j.id == id) {
                for (i, p) in job.procs.iter_mut().enumerate() {
                    if i < k {
                        p.state = ProcState::Done;
                        p.status = codes[i] << 8;
                    } else {
                        p.status = stop_status;
                    }
                }
                job.notified = true;
            }
        });
        if zsh_mode() {
            // zsh reports the job it was running as `zsh: suspended  cmd`.
            let name = crate::shell::with_err_ctx(|e| e.name.clone());
            let msg = sigmsg(libc::WSTOPSIG(stop_status));
            err_line(&format!(
                "{}: {:<w$}{}",
                name,
                msg,
                text,
                w = msg.len().max(9) + 2
            ));
        } else {
            err_line(&format!("\n[{}]+  Stopped\t\t{}", id, text));
        }
        let stopped_code = codes[k];
        for c in codes.iter_mut().skip(k + 1) {
            *c = stopped_code;
        }
    }
    codes
}

// Waits for every unfinished process of a job brought to the foreground.
fn wait_job_foreground(id: i32) -> i32 {
    let pids: Vec<i32> = with_shell(|s| {
        let jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.iter()
            .find(|j| j.id == id)
            .map(|j| {
                j.procs
                    .iter()
                    .filter(|p| p.state != ProcState::Done)
                    .map(|p| p.pid)
                    .collect()
            })
            .unwrap_or_default()
    });
    let mut stopped = false;
    for pid in pids {
        if let Some(st) = waitpid_retry(pid, libc::WUNTRACED) {
            job_update_status(pid, st);
            if libc::WIFSTOPPED(st) {
                stopped = true;
                break;
            }
        }
    }
    reclaim_terminal();
    let (status, text) = job_result(id);
    if stopped && zsh_mode() {
        let name = crate::shell::with_err_ctx(|e| e.name.clone());
        err_line(&format!("{}: {:<11}{}", name, "suspended", text));
    } else if stopped {
        err_line(&format!("\n[{}]+  Stopped\t\t{}", id, text));
    } else {
        remove_job(id);
    }
    status
}

fn job_result(id: i32) -> (i32, String) {
    with_shell(|s| {
        let jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.iter()
            .find(|j| j.id == id)
            .map(|j| {
                let st = j
                    .procs
                    .last()
                    .map(|p| status_to_exit(p.status))
                    .unwrap_or(0);
                (st, j.cmd_text.clone())
            })
            .unwrap_or((0, String::new()))
    })
}

fn remove_job(id: i32) {
    with_shell(|s| {
        let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.retain(|j| j.id != id);
    })
}

fn parse_job_id(args: &[String]) -> Result<Option<i32>, String> {
    if args.is_empty() {
        return Ok(None);
    }
    if args.len() > 1 {
        return Err("too many arguments".to_string());
    }

    let spec = args[0].strip_prefix('%').unwrap_or(&args[0]);
    if spec.is_empty() || spec == "+" || spec == "%" {
        return Ok(None);
    }

    spec.parse::<i32>()
        .map(Some)
        .map_err(|_| format!("no such job: {}", args[0]))
}

fn find_job_index(jobs: &[Job], requested_id: Option<i32>) -> Option<usize> {
    match requested_id {
        Some(id) => jobs
            .iter()
            .position(|job| job.id == id && job.state != JobState::Done),
        None => jobs.iter().rposition(|job| job.state != JobState::Done),
    }
}

fn no_current_job(command: &str, requested_id: Option<i32>) -> i32 {
    match requested_id {
        Some(id) => err_line(&format!("meowsh: {}: no such job: %{}", command, id)),
        None => err_line(&format!("meowsh: {}: no current job", command)),
    }
    1
}

// Resolves a job spec to (id, pgid, text).
fn select_job(command: &str, args: &[String]) -> Result<(i32, i32, String), i32> {
    reap_jobs(false);
    if zsh_mode() {
        let jobs: Vec<Job> = with_shell(|s| s.jobs.lock().map(|j| j.clone()).unwrap_or_default());
        let spec = args.first().map(String::as_str).unwrap_or("%+");
        return match resolve_spec(&jobs, spec) {
            Ok(id) => {
                let j = jobs.iter().find(|j| j.id == id).expect("resolved job");
                Ok((j.id, j.pgid, j.cmd_text.clone()))
            }
            Err(e) => {
                builtin_err(
                    command,
                    if args.is_empty() {
                        "no current job"
                    } else {
                        &e
                    },
                );
                Err(1)
            }
        };
    }
    let requested = match parse_job_id(args) {
        Ok(id) => id,
        Err(e) => {
            builtin_err(command, &e.to_string());
            return Err(1);
        }
    };
    let found = with_shell(|s| {
        let jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        find_job_index(&jobs, requested)
            .map(|i| (jobs[i].id, jobs[i].pgid, jobs[i].cmd_text.clone()))
    });
    found.ok_or_else(|| no_current_job(command, requested))
}

fn set_job_running(id: i32) {
    with_shell(|s| {
        let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = jobs.iter_mut().find(|j| j.id == id) {
            job.state = JobState::Running;
            for p in job.procs.iter_mut() {
                if p.state == ProcState::Stopped {
                    p.state = ProcState::Running;
                }
            }
        }
    })
}

// zsh's names for how a process ended (sigmsg()).
fn sigmsg(sig: i32) -> String {
    match sig {
        libc::SIGHUP => "hangup",
        libc::SIGINT => "interrupt",
        libc::SIGQUIT => "quit",
        libc::SIGILL => "illegal hardware instruction",
        libc::SIGTRAP => "trace trap",
        libc::SIGABRT => "abort",
        libc::SIGBUS => "bus error",
        libc::SIGFPE => "floating point exception",
        libc::SIGKILL => "killed",
        libc::SIGUSR1 => "user-defined signal 1",
        libc::SIGSEGV => "segmentation fault",
        libc::SIGUSR2 => "user-defined signal 2",
        libc::SIGPIPE => "broken pipe",
        libc::SIGALRM => "alarm",
        libc::SIGTERM => "terminated",
        libc::SIGSTOP => "suspended (signal)",
        libc::SIGTSTP => "suspended",
        libc::SIGTTIN => "suspended (tty input)",
        libc::SIGTTOU => "suspended (tty output)",
        _ => return format!("signal {}", sig),
    }
    .to_string()
}

// A process's status as zsh's job listings show it.
fn proc_status(p: &Process) -> String {
    match p.state {
        ProcState::Running => "running".to_string(),
        ProcState::Stopped => sigmsg(libc::WSTOPSIG(p.status)),
        ProcState::Done if libc::WIFSIGNALED(p.status) => sigmsg(libc::WTERMSIG(p.status)),
        ProcState::Done if libc::WEXITSTATUS(p.status) != 0 => {
            format!("exit {:<4}", libc::WEXITSTATUS(p.status))
        }
        ProcState::Done => "done".to_string(),
    }
}

// The current (`+`) and previous (`-`) jobs: the newest two.
fn job_marks(jobs: &[Job]) -> (Option<i32>, Option<i32>) {
    let mut ids: Vec<i32> = jobs
        .iter()
        .filter(|j| j.state != JobState::Done)
        .map(|j| j.id)
        .collect();
    ids.sort();
    let cur = ids.pop();
    (cur, ids.pop())
}

/// zsh's printjob: `[1]  + running    sleep 3`; `long` adds each
/// process ID, `pgid` the process group.
fn zsh_job_text(job: &Job, cur: Option<i32>, prev: Option<i32>, long: bool, pgid: bool) -> String {
    zsh_job_text_as(job, cur, prev, long, pgid, false)
}

// With `conted`, stopped processes show as `continued` (fg and bg).
fn zsh_job_text_as(
    job: &Job,
    cur: Option<i32>,
    prev: Option<i32>,
    long: bool,
    pgid: bool,
    conted: bool,
) -> String {
    let mark = if Some(job.id) == cur {
        '+'
    } else if Some(job.id) == prev {
        '-'
    } else {
        ' '
    };
    let status = |p: &Process| {
        if conted && p.state != ProcState::Done {
            "continued".to_string()
        } else {
            proc_status(p)
        }
    };
    let width = job
        .procs
        .iter()
        .map(|p| status(p).len())
        .max()
        .unwrap_or(0)
        .max(9);
    let mut out = String::new();
    for (k, p) in job.procs.iter().enumerate() {
        if k == 0 {
            out.push_str(&format!("[{}]  {} ", job.id, mark));
        } else {
            out.push_str(if job.id > 9 { "        " } else { "       " });
        }
        if long {
            out.push_str(&format!("{} ", p.pid));
        } else if pgid && k == 0 {
            out.push_str(&format!("{} ", job.pgid));
        }
        out.push_str(&format!("{:<w$}", status(p), w = width + 2));
        out.push_str(if job.procs.len() > 1 {
            &p.cmd
        } else {
            &job.cmd_text
        });
        out.push('\n');
    }
    out
}

/// zsh's $jobstates value for a job: `running:+:pid=running`.
pub fn job_state_text(id: i32) -> Option<String> {
    reap_jobs(false);
    let jobs = visible_jobs();
    {
        let (cur, prev) = job_marks(&jobs);
        let job = jobs.iter().find(|j| j.id == id)?;
        let state = match job.state {
            JobState::Running => "running",
            JobState::Stopped => "suspended",
            JobState::Done => "done",
        };
        let mark = if Some(id) == cur {
            "+"
        } else if Some(id) == prev {
            "-"
        } else {
            ""
        };
        let mut t = format!("{}:{}:", state, mark);
        let procs: Vec<String> = job
            .procs
            .iter()
            .map(|p| format!("{}={}", p.pid, proc_status(p)))
            .collect();
        t.push_str(&procs.join(":"));
        Some(t)
    }
}

/// The numbers of the jobs, for $jobstates' keys.
pub fn job_ids() -> Vec<i32> {
    reap_jobs(false);
    visible_jobs().iter().map(|x| x.id).collect()
}

/// In a subshell: the parent's jobs, shown but not waited for.
pub static OLD_JOBS: std::sync::Mutex<Vec<Job>> = std::sync::Mutex::new(Vec::new());

fn visible_jobs() -> Vec<Job> {
    let own: Vec<Job> = with_shell(|s| s.jobs.lock().map(|j| j.clone()).unwrap_or_default());
    if own.is_empty() {
        OLD_JOBS.lock().map(|o| o.clone()).unwrap_or_default()
    } else {
        own
    }
}

/// Set after `exit` warned about jobs; a second `exit` straight after
/// leaves anyway.
pub static EXIT_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// CHECK_JOBS: true (and a warning printed) if an interactive `exit`
/// should not leave yet because of jobs.
pub fn exit_blocked() -> bool {
    use std::sync::atomic::Ordering;
    if !with_shell(|s| s.interactive) || !zsh_mode() || !crate::options::is_set("checkjobs") {
        return false;
    }
    if EXIT_WARNED.swap(false, Ordering::SeqCst) {
        return false;
    }
    reap_jobs(false);
    let (running, stopped) = with_shell(|s| {
        let j = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        (
            j.iter().filter(|x| x.state == JobState::Running).count(),
            j.iter().filter(|x| x.state == JobState::Stopped).count(),
        )
    });
    let name = crate::shell::with_err_ctx(|e| e.name.clone());
    let msg = if stopped > 0 {
        "you have suspended jobs."
    } else if running > 0 && crate::options::is_set("checkrunningjobs") {
        "you have running jobs."
    } else {
        return false;
    };
    err_line(&format!("{}: {}", name, msg));
    EXIT_WARNED.store(true, Ordering::SeqCst);
    true
}

/// HUP: on leaving an interactive shell, running jobs get SIGHUP.
pub fn hup_jobs() {
    if !zsh_mode() || !crate::options::is_set("hup") {
        return;
    }
    reap_jobs(false);
    let groups: Vec<(i32, bool)> = with_shell(|s| {
        let j = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        j.iter()
            .filter(|x| x.state != JobState::Done)
            .map(|x| (x.pgid, x.state == JobState::Stopped))
            .collect()
    });
    let mut n = 0;
    for (pgid, stopped) in &groups {
        unsafe {
            // Stopped jobs are hung up too, but zsh counts only running ones.
            if libc::killpg(*pgid, libc::SIGHUP) == 0 && !*stopped {
                n += 1;
            }
            if *stopped {
                libc::killpg(*pgid, libc::SIGCONT);
            }
        }
    }
    if n > 0 {
        let name = crate::shell::with_err_ctx(|e| e.name.clone());
        err_line(&format!("{}: warning: {} jobs SIGHUPed", name, n));
    }
}

fn zsh_mode() -> bool {
    !crate::exec::sh_emulation_pub()
}

pub fn builtin_jobs(args: &[String]) -> i32 {
    if zsh_mode() {
        let mut flags = String::new();
        for a in args.iter().filter(|a| a.starts_with('-')) {
            flags.push_str(&a[1..]);
        }
        reap_jobs(false);
        // Having seen the jobs, a following `exit` does not warn.
        EXIT_WARNED.store(true, std::sync::atomic::Ordering::SeqCst);
        let jobs = visible_jobs();
        let (cur, prev) = job_marks(&jobs);
        let mut text = String::new();
        for job in &jobs {
            if flags.contains('r') && job.state != JobState::Running
                || flags.contains('s') && job.state != JobState::Stopped
            {
                continue;
            }
            text.push_str(&zsh_job_text(
                job,
                cur,
                prev,
                flags.contains('l'),
                flags.contains('p'),
            ));
            if flags.contains('d') {
                text.push_str(&format!("(pwd : {})\n", crate::shell::var_get("PWD")));
            }
        }
        // Finished jobs are reported once.
        with_shell(|s| {
            if let Ok(mut j) = s.jobs.lock() {
                j.retain(|x| x.state != JobState::Done);
            }
        });
        out(&text);
        return 0;
    }
    reap_jobs(false);
    let pids_only = args.iter().any(|a| a == "-p");
    let long = args.iter().any(|a| a == "-l");
    let jobs: Vec<Job> = with_shell(|s| {
        let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = jobs.clone();
        jobs.retain(|j| j.state != JobState::Done);
        snapshot
    });
    for job in jobs {
        if pids_only {
            out(&format!("{}\n", job.pgid));
            continue;
        }
        let state = match job.state {
            JobState::Running => "Running",
            JobState::Stopped => "Stopped",
            JobState::Done => "Done",
        };
        if long {
            out(&format!(
                "[{}]+ {} {}\t\t{}\n",
                job.id, job.pgid, state, job.cmd_text
            ));
        } else {
            out(&format!("[{}]+  {}\t\t{}\n", job.id, state, job.cmd_text));
        }
    }
    0
}

/// zsh's getjob(): `%`, `%+`, `%%`, `%-`, `%N`, `%?text`, `%text` or
/// `text`. Errors carry zsh's message.
fn resolve_spec(jobs: &[Job], spec: &str) -> Result<i32, String> {
    // A script never reports finished jobs, and zsh drops them there, so
    // only an interactive shell keeps them until they are reported.
    // (Callers may hold the shell lock, so the flag comes from the error
    // context, not the options.)
    let kept: Vec<Job>;
    let jobs = if crate::shell::with_err_ctx(|e| e.interactive) {
        jobs
    } else {
        kept = jobs
            .iter()
            .filter(|j| j.state != JobState::Done)
            .cloned()
            .collect();
        &kept
    };
    let live: Vec<&Job> = jobs
        .iter()
        .filter(|j| j.state != JobState::Done || !j.notified)
        .collect();
    let (cur, prev) = job_marks(jobs);
    let Some(s) = spec.strip_prefix('%') else {
        return find_by_name(&live, spec).ok_or_else(|| format!("job not found: {}", spec));
    };
    match s {
        "" | "%" | "+" => {
            cur.ok_or_else(|| format!("%{}: no such job", s.chars().next().unwrap_or('%')))
        }
        "-" => prev.ok_or_else(|| "%-: no such job".to_string()),
        _ if s.starts_with(|c: char| c.is_ascii_digit()) => {
            let n: i32 = s
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            live.iter()
                .find(|j| j.id == n)
                .map(|j| j.id)
                .ok_or_else(|| format!("%{}: no such job", s))
        }
        _ if s.starts_with('?') => live
            .iter()
            .rev()
            .find(|j| j.cmd_text.contains(&s[1..]))
            .map(|j| j.id)
            .ok_or_else(|| format!("job not found: {}", s)),
        _ => find_by_name(&live, s).ok_or_else(|| format!("job not found: {}", s)),
    }
}

fn find_by_name(jobs: &[&Job], name: &str) -> Option<i32> {
    jobs.iter()
        .rev()
        .find(|j| j.cmd_text.starts_with(name))
        .map(|j| j.id)
}

/// `disown [-a] [job ...]`: forgets jobs so they are not sent SIGHUP.
pub fn builtin_disown(args: &[String]) -> i32 {
    reap_jobs(false);
    let own_empty = with_shell(|s| s.jobs.lock().map(|j| j.is_empty()).unwrap_or(true));
    if own_empty && OLD_JOBS.lock().map(|o| !o.is_empty()).unwrap_or(false) {
        builtin_err("disown", "can't manipulate jobs in subshell");
        return 1;
    }
    let all = args.first().is_some_and(|a| a == "-a");
    let specs: Vec<&String> = args.iter().filter(|a| *a != "-a").collect();
    let jobs: Vec<Job> = with_shell(|s| s.jobs.lock().map(|j| j.clone()).unwrap_or_default());
    let ids: Vec<i32> = if all {
        jobs.iter().map(|j| j.id).collect()
    } else if specs.is_empty() {
        match job_marks(&jobs).0 {
            Some(id) => vec![id],
            None => {
                builtin_err("disown", "no current job");
                return 1;
            }
        }
    } else {
        let mut v = Vec::new();
        for sp in specs {
            match resolve_spec(&jobs, sp) {
                Ok(id) => v.push(id),
                Err(e) => {
                    builtin_err("disown", &e);
                    return 127;
                }
            }
        }
        v
    };
    for id in ids {
        if let Some(j) = jobs.iter().find(|j| j.id == id) {
            if j.state == JobState::Stopped {
                builtin_err(
                    "disown",
                    &format!(
                        "warning: job is suspended, use `kill -CONT -{}' to resume",
                        j.pgid
                    ),
                );
            }
        }
        remove_job(id);
    }
    0
}

pub fn builtin_fg(args: &[String]) -> i32 {
    if !job_control() {
        builtin_err(
            "fg",
            if zsh_mode() {
                "no job control in this shell"
            } else {
                "no job control"
            },
        );
        return 1;
    }
    let (id, pgid, text) = match select_job("fg", args) {
        Ok(j) => j,
        Err(st) => return st,
    };
    if zsh_mode() {
        let jobs: Vec<Job> = with_shell(|s| s.jobs.lock().map(|j| j.clone()).unwrap_or_default());
        let (cur, prev) = job_marks(&jobs);
        // fg makes the job current: the marks shift before it is shown.
        let (cur, prev) = if Some(id) == cur {
            (prev, Some(id))
        } else {
            (cur, prev.filter(|&p| p != id))
        };
        if let Some(j) = jobs.iter().find(|j| j.id == id) {
            let conted = j.state == JobState::Stopped;
            out(&zsh_job_text_as(j, cur, prev, false, false, conted));
        }
    } else {
        out(&format!("{}\n", text));
    }
    set_job_running(id);
    give_terminal_to(pgid);
    unsafe {
        libc::kill(-pgid, libc::SIGCONT);
    }
    wait_job_foreground(id)
}

pub fn builtin_bg(args: &[String]) -> i32 {
    if !job_control() {
        builtin_err(
            "bg",
            if zsh_mode() {
                "no job control in this shell"
            } else {
                "no job control"
            },
        );
        return 1;
    }
    let (id, pgid, text) = match select_job("bg", args) {
        Ok(j) => j,
        Err(st) => return st,
    };
    set_job_running(id);
    unsafe {
        libc::kill(-pgid, libc::SIGCONT);
    }
    out(&format!("[{}]+ {} &\n", id, text));
    0
}

// Waits for one process, using a status already collected by `reap_jobs`
// when there is one.
fn wait_pid(pid: i32) -> i32 {
    let known = with_shell(|s| {
        let jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.iter()
            .flat_map(|j| j.procs.iter())
            .find(|p| p.pid == pid)
            .map(|p| (p.state, p.status))
    });
    match known {
        Some((ProcState::Done, st)) => status_to_exit(st),
        Some(_) => match wait_interruptible(pid) {
            Err(sig) => 128 + sig,
            Ok(st) => {
                job_update_status(pid, st);
                status_to_exit(st)
            }
        },
        None => {
            // A background job reaped already keeps its status for `wait`.
            if let Some(st) = take_bg_status(pid) {
                return status_to_exit(st);
            }
            match waitpid_retry(pid, 0) {
                Some(st) => status_to_exit(st),
                None => {
                    if zsh_mode() && !crate::options::is_set("posixbuiltins") {
                        builtin_err("wait", &format!("pid {} is not a child of this shell", pid));
                    }
                    127
                }
            }
        }
    }
}

/// Statuses of background processes reaped while nothing would report
/// them (zsh's bgstatus list), for a later `wait pid`.
static BG_STATUS: std::sync::Mutex<Vec<(i32, i32)>> = std::sync::Mutex::new(Vec::new());

fn remember_bg_status(pid: i32, status: i32) {
    if let Ok(mut b) = BG_STATUS.lock() {
        b.retain(|&(p, _)| p != pid);
        b.push((pid, status));
        let excess = b.len().saturating_sub(256);
        b.drain(..excess);
    }
}

fn take_bg_status(pid: i32) -> Option<i32> {
    let mut b = BG_STATUS.lock().ok()?;
    let k = b.iter().position(|&(p, _)| p == pid)?;
    Some(b.remove(k).1)
}

fn forget_done_jobs() {
    with_shell(|s| {
        let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.retain(|j| j.state != JobState::Done);
    })
}

pub fn builtin_wait(args: &[String]) -> i32 {
    if args.is_empty() {
        let pids: Vec<i32> = with_shell(|s| {
            let jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
            jobs.iter()
                .flat_map(|j| j.procs.iter())
                .filter(|p| p.state != ProcState::Done)
                .map(|p| p.pid)
                .collect()
        });
        for &pid in &pids {
            wait_pid(pid);
        }
        // Each status stays available to a later `wait pid`.
        let done: Vec<(i32, i32)> = with_shell(|s| {
            let jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
            jobs.iter()
                .flat_map(|j| j.procs.iter())
                .filter(|p| p.state == ProcState::Done && pids.contains(&p.pid))
                .map(|p| (p.pid, p.status))
                .collect()
        });
        for (pid, status) in done {
            remember_bg_status(pid, status);
        }
        finish_wait();
        return 0;
    }
    let mut status = 0;
    let posix = crate::options::is_set("posixbuiltins");
    for arg in args {
        if arg.starts_with('%') && zsh_mode() {
            // Jobs that ended since they were last looked at count as done.
            reap_jobs(false);
            let jobs: Vec<Job> =
                with_shell(|s| s.jobs.lock().map(|j| j.clone()).unwrap_or_default());
            match resolve_spec(&jobs, arg) {
                Ok(id) => {
                    let pids: Vec<i32> = jobs
                        .iter()
                        .find(|j| j.id == id)
                        .map(|j| j.procs.iter().map(|p| p.pid).collect())
                        .unwrap_or_default();
                    for pid in pids {
                        status = wait_pid(pid);
                    }
                }
                Err(e) => {
                    if !posix {
                        builtin_err("wait", &e);
                    }
                    status = 127;
                }
            }
            continue;
        }
        if let Some(spec) = arg.strip_prefix('%') {
            let pids: Option<Vec<i32>> = spec.parse::<i32>().ok().and_then(|id| {
                with_shell(|s| {
                    let jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
                    jobs.iter()
                        .find(|j| j.id == id)
                        .map(|j| j.procs.iter().map(|p| p.pid).collect())
                })
            });
            match pids {
                Some(pids) => {
                    for pid in pids {
                        status = wait_pid(pid);
                    }
                }
                None => {
                    builtin_err("wait", &format!("{}: no such job", arg));
                    status = 127;
                }
            }
        } else {
            match arg.parse::<i32>() {
                Ok(pid) if pid > 0 => status = wait_pid(pid),
                _ => {
                    err_line(&format!(
                        "meowsh: wait: `{}': not a pid or valid job spec",
                        arg
                    ));
                    status = 2;
                }
            }
        }
    }
    finish_wait();
    status
}

// After `wait`: an interactive zsh reports the jobs that finished.
fn finish_wait() {
    if zsh_mode() && with_shell(|s| s.interactive) {
        reap_jobs(true);
    } else {
        forget_done_jobs();
    }
}

/// Target process(es) for `kill`: a pid, or a job spec.
pub fn kill_targets(spec: &str) -> Result<Vec<i32>, String> {
    if spec.starts_with('%') && zsh_mode() {
        let jc = job_control();
        return with_shell(|s| {
            let jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
            let id = resolve_spec(&jobs, spec)?;
            let job = jobs.iter().find(|j| j.id == id).expect("resolved job");
            Ok(if jc {
                vec![-job.pgid]
            } else {
                job.procs.iter().map(|p| p.pid).collect()
            })
        });
    }
    if let Some(job) = spec.strip_prefix('%') {
        let id = if job.is_empty() || job == "+" || job == "%" {
            None
        } else {
            Some(
                job.parse::<i32>()
                    .map_err(|_| format!("{}: no such job", spec))?,
            )
        };
        let jc = job_control();
        return with_shell(|s| {
            let jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
            let idx = find_job_index(&jobs, id).ok_or_else(|| format!("{}: no such job", spec))?;
            let job = &jobs[idx];
            Ok(if jc {
                vec![-job.pgid]
            } else {
                job.procs.iter().map(|p| p.pid).collect()
            })
        });
    }
    spec.parse::<i32>()
        .map(|p| vec![p])
        .map_err(|_| format!("{}: arguments must be process or job IDs", spec))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_job(id: i32, state: JobState) -> Job {
        Job {
            id,
            pgid: id,
            procs: vec![Process {
                pid: id,
                status: 0,
                state: ProcState::Running,
                cmd: format!("cmd{id}"),
            }],
            state,
            notified: false,
            foreground: false,
            cmd_text: format!("cmd{id}"),
        }
    }

    #[test]
    fn parse_job_id_accepts_default_and_percent_specs() {
        assert_eq!(parse_job_id(&[]).unwrap(), None);
        assert_eq!(parse_job_id(&["%".to_string()]).unwrap(), None);
        assert_eq!(parse_job_id(&["%+".to_string()]).unwrap(), None);
        assert_eq!(parse_job_id(&["%2".to_string()]).unwrap(), Some(2));
        assert_eq!(parse_job_id(&["3".to_string()]).unwrap(), Some(3));
        assert!(parse_job_id(&["%abc".to_string()]).is_err());
        assert!(parse_job_id(&["%1".to_string(), "%2".to_string()]).is_err());
    }

    #[test]
    fn status_to_exit_reports_stopped_signal_status() {
        let status = (libc::SIGTSTP << 8) | 0x7f;

        assert_eq!(status_to_exit(status), 128 + libc::SIGTSTP);
    }

    #[test]
    fn find_job_index_defaults_to_latest_non_done_job() {
        let jobs = vec![
            test_job(1, JobState::Stopped),
            test_job(2, JobState::Done),
            test_job(3, JobState::Running),
        ];

        assert_eq!(find_job_index(&jobs, None), Some(2));
        assert_eq!(find_job_index(&jobs, Some(1)), Some(0));
        assert_eq!(find_job_index(&jobs, Some(2)), None);
        assert_eq!(find_job_index(&jobs, Some(99)), None);
    }
}
