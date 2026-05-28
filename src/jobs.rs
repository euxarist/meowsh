use crate::shell::SHELL;
use crate::types::{Job, JobState, ProcState};

pub fn jobs_init() {}

fn status_to_exit(status: i32) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status) as i32
    } else if libc::WIFSTOPPED(status) {
        128 + libc::WSTOPSIG(status) as i32
    } else {
        0
    }
}

fn job_exit_status(job_id: i32) -> i32 {
    let shell = SHELL.shell.lock().unwrap();
    let jobs = shell.jobs.lock().unwrap();
    jobs.iter()
        .find(|j| j.id == job_id)
        .and_then(|j| j.procs.last())
        .map(|proc| status_to_exit(proc.status))
        .unwrap_or(0)
}

fn remove_done_job(job_id: i32) {
    let shell = SHELL.shell.lock().unwrap();
    let mut jobs = shell.jobs.lock().unwrap();
    jobs.retain(|j| j.id != job_id || j.state != JobState::Done);
}

fn reap_finished_jobs() {
    loop {
        let mut status: i32 = 0;
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

        job_update_status(pid, status);
    }
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

fn print_no_current_job(command: &str, requested_id: Option<i32>) {
    if let Some(id) = requested_id {
        eprintln!("meowsh: {command}: no such job: %{id}");
    } else {
        eprintln!("meowsh: {command}: no current job");
    }
}

pub fn job_update_status(pid: i32, status: i32) {
    let shell = SHELL.shell.lock().unwrap();
    let mut jobs = shell.jobs.lock().unwrap();

    for job in jobs.iter_mut() {
        for proc in job.procs.iter_mut() {
            if proc.pid == pid {
                proc.status = status;
                let stopped = libc::WIFSTOPPED(status);
                let exited = libc::WIFEXITED(status);
                let signaled = libc::WIFSIGNALED(status);

                if stopped {
                    proc.state = ProcState::Stopped;
                } else if exited || signaled {
                    proc.state = ProcState::Done;
                } else {
                    proc.state = ProcState::Running;
                }
                let all_done = job.procs.iter().all(|p| p.state == ProcState::Done);
                let any_stopped = job.procs.iter().any(|p| p.state == ProcState::Stopped);

                if all_done {
                    job.state = JobState::Done;
                } else if any_stopped {
                    job.state = JobState::Stopped;
                } else {
                    job.state = JobState::Running;
                }
                return;
            }
        }
    }
}

pub fn job_wait_foreground(job: &Job) -> i32 {
    loop {
        let mut status: i32 = 0;
        let pid = unsafe { libc::waitpid(-job.pgid, &mut status, libc::WUNTRACED) };

        if pid < 0 {
            break;
        }

        job_update_status(pid, status);

        if libc::WIFSTOPPED(status) {
            println!("\n[{}]+  Stopped\t{}", job.id, job.cmd_text);
            let shell = SHELL.shell.lock().unwrap();
            let mut jobs = shell.jobs.lock().unwrap();
            if let Some(j) = jobs.iter_mut().find(|j| j.id == job.id) {
                j.state = JobState::Stopped;
                j.notified = true;
            }
            break;
        }

        let job_state = {
            let shell = SHELL.shell.lock().unwrap();
            let jobs = shell.jobs.lock().unwrap();
            jobs.iter()
                .find(|j| j.id == job.id)
                .map(|j| j.state)
                .unwrap_or(JobState::Done)
        };
        if job_state != JobState::Running {
            break;
        }
    }

    let status = job_exit_status(job.id);

    unsafe {
        let shell_pgid = SHELL.shell.lock().unwrap().shell_pgid;
        let _ = libc::tcsetpgrp(0, shell_pgid);
    }

    remove_done_job(job.id);

    status
}

pub fn builtin_jobs(_args: &[String]) -> i32 {
    reap_finished_jobs();

    let shell = SHELL.shell.lock().unwrap();
    let mut jobs = shell.jobs.lock().unwrap();

    for job in jobs.iter() {
        let state_str = match job.state {
            JobState::Running => "Running",
            JobState::Stopped => "Stopped",
            JobState::Done => "Done",
        };
        println!("[{}]+  {}\t\t{}", job.id, state_str, job.cmd_text);
    }
    jobs.retain(|job| job.state != JobState::Done);
    0
}

pub fn builtin_fg(args: &[String]) -> i32 {
    reap_finished_jobs();

    let requested_id = match parse_job_id(args) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("meowsh: fg: {}", e);
            return 1;
        }
    };

    let shell = SHELL.shell.lock().unwrap();
    let mut jobs = shell.jobs.lock().unwrap();

    let Some(job_index) = find_job_index(&jobs, requested_id) else {
        print_no_current_job("fg", requested_id);
        return 1;
    };

    let job = &mut jobs[job_index];
    println!("{}", job.cmd_text);
    job.state = JobState::Running;
    job.foreground = true;

    let job = job.clone();

    drop(jobs);
    drop(shell);

    unsafe {
        let _ = libc::tcsetpgrp(0, job.pgid);
        let _ = libc::kill(-job.pgid, libc::SIGCONT);
    }

    job_wait_foreground(&job)
}

pub fn builtin_bg(args: &[String]) -> i32 {
    reap_finished_jobs();

    let requested_id = match parse_job_id(args) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("meowsh: bg: {}", e);
            return 1;
        }
    };

    let shell = SHELL.shell.lock().unwrap();
    let mut jobs = shell.jobs.lock().unwrap();

    let Some(job_index) = find_job_index(&jobs, requested_id) else {
        print_no_current_job("bg", requested_id);
        return 1;
    };

    let job = &mut jobs[job_index];
    job.state = JobState::Running;
    job.foreground = false;

    let pgid = job.pgid;

    drop(jobs);
    drop(shell);

    unsafe {
        let _ = libc::kill(-pgid, libc::SIGCONT);
    }

    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Process;

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
        let status = libc::W_STOPCODE(libc::SIGTSTP);

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
