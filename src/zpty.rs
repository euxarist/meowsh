//! zsh/zpty: commands run on a pseudo-terminal, written to and read
//! from by name (`zpty name cmd`, `zpty -w`, `zpty -r`, `zpty -d`).

use crate::shell::builtin_err;
use std::sync::Mutex;

struct PtyCmd {
    name: String,
    args: Vec<String>,
    fd: i32,
    pid: i32,
    fin: bool,
    echo: bool,
    nblock: bool,
    // Bytes read past the last pattern match, kept for the next read.
    old: Vec<u8>,
}

static PTYCMDS: Mutex<Vec<PtyCmd>> = Mutex::new(Vec::new());

fn with_cmds<R>(f: impl FnOnce(&mut Vec<PtyCmd>) -> R) -> R {
    let mut g = PTYCMDS.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut g)
}

// A master/slave pseudo-terminal pair: (master fd, slave path).
fn open_pty() -> Option<(i32, std::ffi::CString)> {
    unsafe {
        let mut m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if m < 0 {
            return None;
        }
        // Keep the master out of 0..=2 and closed on exec so children do
        // not clobber stdio or inherit it.
        let hi = libc::fcntl(m, libc::F_DUPFD_CLOEXEC, 10);
        if hi >= 0 {
            libc::close(m);
            m = hi;
        } else {
            libc::fcntl(m, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        if libc::grantpt(m) != 0 || libc::unlockpt(m) != 0 {
            libc::close(m);
            return None;
        }
        #[cfg(target_os = "linux")]
        let name = {
            let mut buf = [0 as libc::c_char; 128];
            if libc::ptsname_r(m, buf.as_mut_ptr(), buf.len()) != 0 {
                libc::close(m);
                return None;
            }
            std::ffi::CStr::from_ptr(buf.as_ptr()).to_owned()
        };
        #[cfg(not(target_os = "linux"))]
        let name = {
            let ptr = libc::ptsname(m);
            if ptr.is_null() {
                libc::close(m);
                return None;
            }
            std::ffi::CStr::from_ptr(ptr).to_owned()
        };
        Some((m, name))
    }
}

fn new_cmd(name: &str, args: &[String], echo: bool, nblock: bool) -> i32 {
    let src = args.join(" ");
    let Some((master, slave_name)) = open_pty() else {
        builtin_err(
            "zpty",
            &format!(
                "can't open pseudo terminal: {}",
                std::io::Error::last_os_error()
            ),
        );
        return 1;
    };
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        builtin_err("zpty", &format!("can't create pty command {}", name));
        unsafe {
            libc::close(master);
        }
        return 1;
    }
    if pid == 0 {
        unsafe {
            libc::setsid();
            let slave = libc::open(slave_name.as_ptr(), libc::O_RDWR);
            if slave < 0 {
                libc::_exit(1);
            }
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            if !echo {
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(slave, &mut t) == 0 {
                    t.c_lflag &= !libc::ECHO;
                    libc::tcsetattr(slave, libc::TCSANOW, &t);
                }
            }
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            if slave > 2 {
                libc::close(slave);
            }
            if master > 2 {
                libc::close(master);
            }
        }
        // Drop inherited pty masters so the child cannot close or signal them.
        with_cmds(|c| {
            for p in c.drain(..) {
                if p.fd > 2 {
                    unsafe {
                        libc::close(p.fd);
                    }
                }
            }
        });
        // Tell the parent the terminal is ready, then run the command.
        let _ = crate::sys::write_fd(1, &[0u8]);
        crate::shell::with_shell(|s| {
            crate::options::set_in(s, "interactive", false);
            s.interactive = false;
            s.in_subshell = true;
        });
        crate::trap::reset_in_child(false);
        let st = crate::exec::run_string(&src);
        crate::exec::exit_shell(st);
    }
    // Wait for the child's sync byte.
    let mut b = [0u8; 1];
    loop {
        let r = unsafe { libc::read(master, b.as_mut_ptr() as *mut libc::c_void, 1) };
        if r == 1
            || (r < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted)
            || r == 0
        {
            break;
        }
    }
    if nblock {
        unsafe {
            let fl = libc::fcntl(master, libc::F_GETFL);
            libc::fcntl(master, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
    with_cmds(|c| {
        c.insert(
            0,
            PtyCmd {
                name: name.to_string(),
                args: args.to_vec(),
                fd: master,
                pid,
                fin: false,
                echo,
                nblock,
                old: Vec::new(),
            },
        )
    });
    crate::shell::var_set("REPLY", &master.to_string(), false);
    0
}

fn check(cmd: &mut PtyCmd) {
    if cmd.fin {
        return;
    }
    let mut pfd = libc::pollfd {
        fd: cmd.fd,
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut pfd, 1, 0) } <= 0 {
        return;
    }
    if pfd.revents & libc::POLLIN != 0 {
        return;
    }
    cmd.fin = true;
}

fn delete(cmd: PtyCmd) {
    unsafe {
        libc::close(cmd.fd);
        libc::kill(-cmd.pid, libc::SIGHUP);
        let mut st = 0;
        libc::waitpid(cmd.pid, &mut st, libc::WNOHANG);
    }
}

/// Removes every pty command (on shell exit).
pub fn delete_all() {
    let all: Vec<PtyCmd> = with_cmds(std::mem::take);
    for c in all {
        delete(c);
    }
}

// zpty -r: one line into `param`, or until `pattern` matches, or (with
// neither) everything until the command ends.
fn read(name: &str, args: &[String], noblock: bool, mustmatch: bool) -> i32 {
    if args.len() > 2 {
        builtin_err("zpty", "too many arguments");
        return 1;
    }
    let pat = args.get(1).map(|p| {
        crate::pattern::Pattern::new(
            &crate::expand::expand_pattern(p).unwrap_or_else(|_| p.clone()),
        )
    });
    let (fd, mut buf) = match with_cmds(|c| {
        c.iter_mut()
            .find(|p| p.name == name)
            .map(|p| (p.fd, std::mem::take(&mut p.old)))
    }) {
        Some(x) => x,
        None => return 1,
    };
    let mut seen = !buf.is_empty();
    let mut matched = false;
    let mut last_ret: isize = 0;
    loop {
        if noblock {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut pfd, 1, 0) } <= 0 || pfd.revents & libc::POLLIN == 0 {
                break;
            }
        }
        if last_ret == 0 {
            let fin = with_cmds(|c| {
                c.iter_mut().find(|p| p.name == name).map(|p| {
                    check(p);
                    p.fin
                })
            });
            if fin == Some(true) {
                break;
            }
        }
        let mut b = [0u8; 1];
        let r = unsafe { libc::read(fd, b.as_mut_ptr() as *mut libc::c_void, 1) };
        last_ret = r;
        if r == 1 {
            buf.push(b[0]);
            seen = true;
            // Without a parameter, output goes straight out in chunks.
            if args.is_empty() && buf.len() >= 255 {
                let _ = crate::sys::write_fd(1, &buf);
                buf.clear();
            }
        }
        match &pat {
            None => {
                if r <= 0 || (!args.is_empty() && buf.last() == Some(&b'\n')) {
                    break;
                }
            }
            Some(p) => {
                if r < 0 {
                    let e = std::io::Error::last_os_error();
                    if e.kind() != std::io::ErrorKind::WouldBlock {
                        break;
                    }
                    // Non-blocking pty without `-t`: wait for input instead of
                    // spinning at 100% CPU.
                    let mut pfd = libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let pr = unsafe { libc::poll(&mut pfd, 1, -1) };
                    if pr < 0 {
                        let pe = std::io::Error::last_os_error();
                        if pe.kind() != std::io::ErrorKind::Interrupted {
                            break;
                        }
                    } else if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
                        && pfd.revents & libc::POLLIN == 0
                    {
                        break;
                    }
                }
                if r == 0 {
                    break;
                }
                if r == 1 && p.matches(&String::from_utf8_lossy(&buf)) {
                    matched = true;
                    break;
                }
            }
        }
        if crate::exec::flow() != crate::types::Flow::None {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    match args.first() {
        Some(param) => {
            crate::shell::var_set(param, &text, false);
        }
        None => {
            if !buf.is_empty() {
                let _ = crate::sys::write_fd(1, &buf);
            }
        }
    }
    let fin = with_cmds(|c| c.iter().find(|p| p.name == name).is_some_and(|p| p.fin));
    if seen && (pat.is_none() || matched || !mustmatch) {
        0
    } else {
        i32::from(fin) + 1
    }
}

fn write_cmd(name: &str, args: &[String], nonl: bool) -> i32 {
    let Some(fd) = with_cmds(|c| c.iter().find(|p| p.name == name).map(|p| p.fd)) else {
        return 1;
    };
    let data = if args.is_empty() {
        let mut v = Vec::new();
        let mut b = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(0, b.as_mut_ptr() as *mut libc::c_void, b.len()) };
            if n <= 0 {
                break;
            }
            v.extend_from_slice(&b[..n as usize]);
        }
        v
    } else {
        let mut s = args.join(" ");
        if !nonl {
            s.push('\n');
        }
        s.into_bytes()
    };
    match crate::sys::write_fd(fd, &data) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

pub fn builtin_zpty(words: &[String]) -> i32 {
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
            if !"ebdmrwLnt".contains(c) {
                builtin_err("zpty", &format!("bad option: -{}", c));
                return 1;
            }
        }
        flags.push_str(f);
    }
    let has = |c| flags.contains(c);
    let args = &words[i..];
    if (has('r') && has('w'))
        || ((has('r') || has('w')) && (has('d') || has('e') || has('b') || has('L')))
        || (has('w') && (has('t') || has('m')))
        || (has('n')
            && (has('b') || has('e') || has('r') || has('t') || has('d') || has('L') || has('m')))
        || (has('d') && (has('b') || has('e') || has('L') || has('t') || has('m')))
        || (has('L') && (has('b') || has('e') || has('m')))
    {
        builtin_err("zpty", "invalid option combination");
        return 1;
    }
    if has('r') || has('w') {
        let Some(name) = args.first() else {
            builtin_err("zpty", "missing pty command name");
            return 1;
        };
        let state = with_cmds(|c| c.iter().find(|p| &p.name == name).map(|p| p.fin));
        match state {
            None => {
                builtin_err("zpty", &format!("no such pty command: {}", name));
                return 1;
            }
            Some(true) => return 2,
            Some(false) => {}
        }
        return if has('r') {
            read(name, &args[1..], has('t'), has('m'))
        } else {
            write_cmd(name, &args[1..], has('n'))
        };
    }
    if has('d') {
        if args.is_empty() {
            delete_all();
            return 0;
        }
        let mut ret = 0;
        for a in args {
            let found = with_cmds(|c| c.iter().position(|p| &p.name == a).map(|k| c.remove(k)));
            match found {
                Some(p) => delete(p),
                None => {
                    builtin_err("zpty", &format!("no such pty command: {}", a));
                    ret = 1;
                }
            }
        }
        return ret;
    }
    if has('t') {
        let Some(name) = args.first() else {
            builtin_err("zpty", "missing pty command name");
            return 1;
        };
        return match with_cmds(|c| {
            c.iter_mut().find(|p| &p.name == name).map(|p| {
                check(p);
                p.fin
            })
        }) {
            Some(fin) => i32::from(fin),
            None => {
                builtin_err("zpty", &format!("no such pty command: {}", name));
                1
            }
        };
    }
    if let Some(name) = args.first() {
        if args.len() < 2 {
            builtin_err("zpty", "missing command");
            return 1;
        }
        if with_cmds(|c| c.iter().any(|p| &p.name == name)) {
            builtin_err("zpty", &format!("pty command name already used: {}", name));
            return 1;
        }
        return new_cmd(name, &args[1..], has('e'), has('b'));
    }
    let text = with_cmds(|c| {
        let mut out = String::new();
        for p in c.iter_mut() {
            check(p);
            if has('L') {
                out.push_str(&format!(
                    "zpty {}{}{} ",
                    if p.echo { "-e " } else { "" },
                    if p.nblock { "-b " } else { "" },
                    p.name
                ));
            } else if p.fin {
                out.push_str(&format!("(finished) {}: ", p.name));
            } else {
                out.push_str(&format!("({}) {}: ", p.pid, p.name));
            }
            out.push_str(
                &p.args
                    .iter()
                    .map(|a| crate::builtins::shell_quote(a))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            out.push('\n');
        }
        out
    });
    let _ = crate::sys::write_fd(1, text.as_bytes());
    0
}
