use meowsh::builtins::source_file;
use meowsh::exec::{exit_shell, flow, run_string, set_flow, strerror};
use meowsh::jobs::TTY_FD;
use meowsh::lineedit::{run_fd, run_interactive};
use meowsh::options::{self, Emulation};
use meowsh::shell::{shell_init, with_shell};
use meowsh::types::Flow;
use std::env;
use std::path::Path;
use std::sync::atomic::Ordering;

#[derive(Debug, Default, PartialEq)]
struct Args {
    command: Option<String>,
    script: Option<String>,
    arg0: Option<String>,
    positional: Vec<String>,
    interactive: Option<bool>,
    login: bool,
    // (option, value) pairs from -e, +x, -o name, ...
    options: Vec<(&'static str, bool)>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut out = Args {
        login: args.first().map(|a| a.starts_with('-')).unwrap_or(false),
        ..Args::default()
    };
    let mut command_mode = false;
    let mut stdin_mode = false;
    let mut i = 1;
    // `--emulate mode` is handled before parsing (it decides the letters).
    if args.get(1).map(|a| a.as_str()) == Some("--emulate") {
        if args.len() < 3 {
            eprintln!("{}: --emulate: argument required", args[0]);
            std::process::exit(1);
        }
        i = 3;
    } else if args
        .iter()
        .skip(2)
        .position(|a| a == "--emulate")
        .is_some_and(|p| {
            // Only among the leading options, not in a command or script args.
            args[1..p + 2].iter().all(|a| {
                (a.starts_with('-') || a.starts_with('+')) && a != "--" && !a.contains('c')
            })
        })
    {
        eprintln!("{}: --emulate: must precede other options", args[0]);
        std::process::exit(1);
    }
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            i += 1;
            break;
        }
        if a == "--login" {
            out.login = true;
            i += 1;
            continue;
        }
        // zsh: --version, --help, and --option-name for any option.
        if a == "--version" {
            let name = args
                .first()
                .map(|a0| a0.rsplit('/').next().unwrap_or(a0).trim_start_matches('-'))
                .unwrap_or("meowsh");
            println!(
                "{} 5.9-meowsh ({}-unknown-{})",
                name,
                std::env::consts::ARCH,
                std::env::consts::OS
            );
            std::process::exit(0);
        }
        if a == "--help" {
            println!("Usage: meowsh [<options>] [<argument> ...]");
            std::process::exit(0);
        }
        if let Some(long) = a.strip_prefix("--").filter(|l| !l.is_empty()) {
            let (opt, v) =
                options::resolve(long).ok_or_else(|| format!("no such option: {}", long))?;
            out.options.push((opt, v));
            i += 1;
            continue;
        }
        if !(a.starts_with('-') || a.starts_with('+')) || a.len() < 2 {
            break;
        }
        let on = a.starts_with('-');
        for c in a[1..].chars() {
            match c {
                'c' => command_mode = true,
                'i' => out.interactive = Some(on),
                'l' => out.login = true,
                's' => stdin_mode = true,
                'o' => {
                    i += 1;
                    let name = args.get(i).ok_or("-o: option name required")?;
                    let (opt, v) = options::resolve(name)
                        .ok_or_else(|| format!("no such option: {}", name))?;
                    out.options.push((opt, if on { v } else { !v }));
                }
                _ => match options::letter(c) {
                    Some((opt, v)) => out.options.push((opt, if on { v } else { !v })),
                    None => return Err(format!("-{}: invalid option", c)),
                },
            }
        }
        i += 1;
    }
    if command_mode {
        let cmd = args.get(i).ok_or("-c: option requires an argument")?;
        out.command = Some(cmd.clone());
        i += 1;
        if let Some(a0) = args.get(i) {
            out.arg0 = Some(a0.clone());
            i += 1;
        }
    } else if !stdin_mode && i < args.len() {
        out.script = Some(args[i].clone());
        out.arg0 = Some(args[i].clone());
        i += 1;
    }
    out.positional = args[i.min(args.len())..].to_vec();
    Ok(out)
}

fn main() {
    shell_init();
    let argv: Vec<String> = {
        use std::os::unix::ffi::OsStrExt;
        env::args_os()
            .map(|a| meowsh::shell::bytes_to_zstr(a.as_bytes()))
            .collect()
    };
    // As zsh does: emulate the shell named by argv[0] (sh, ksh, ...) or
    // by `--emulate mode`; otherwise zsh.
    let emu = match (argv.get(1).map(|a| a.as_str()), argv.get(2)) {
        (Some("--emulate"), Some(m)) => Emulation::from_name(m).unwrap_or(Emulation::Zsh),
        _ => argv
            .first()
            .and_then(|a0| Emulation::from_name(a0))
            .filter(|e| *e != Emulation::Csh)
            .unwrap_or(Emulation::Zsh),
    };
    with_shell(|s| {
        options::init(s, emu);
        if matches!(emu, Emulation::Sh | Emulation::Ksh) {
            s.drop_default_ties();
        }
        // zsh's default PS4 names the function and line.
        if !matches!(emu, Emulation::Sh | Emulation::Ksh) && std::env::var_os("PS4").is_none() {
            s.store("PS4", meowsh::params::Value::Scalar("+%N:%i> ".to_string()));
        }
        // zsh's two built-in aliases.
        if !matches!(emu, Emulation::Sh | Emulation::Ksh) {
            s.alias_map(false)
                .insert("run-help".to_string(), "man".to_string());
            s.alias_map(false)
                .insert("which-command".to_string(), "whence".to_string());
        }
    });
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("meowsh: {}", e);
            std::process::exit(2);
        }
    };

    let tty_in = unsafe { libc::isatty(0) == 1 };
    let tty_err = unsafe { libc::isatty(2) == 1 };
    let interactive = args
        .interactive
        .unwrap_or(args.command.is_none() && args.script.is_none() && tty_in && tty_err);

    with_shell(|s| {
        s.interactive = interactive;
        s.login_shell = args.login;
        s.argv0 = args.arg0.clone().unwrap_or_else(|| {
            argv.first()
                .cloned()
                .unwrap_or_else(|| "meowsh".to_string())
        });
        s.pos_params = args.positional.clone();
        s.script_name = match &args.script {
            Some(script) => script.clone(),
            // `-c` and standard input: errors name the shell, as zsh's do.
            None => {
                let a0 = argv.first().map(|a| a.as_str()).unwrap_or("meowsh");
                let base = a0.rsplit('/').next().unwrap_or(a0).trim_start_matches('-');
                if args.command.is_some() && !base.contains("meowsh") {
                    "zsh".to_string()
                } else {
                    base.to_string()
                }
            }
        };
        // As in zsh, an interactive shell on a terminal uses the line
        // editor unless `+Z` (or `-o nozle`) says otherwise.
        options::set_in(s, "zle", interactive && tty_in);
        // HASH_DIRS starts in the same state as INTERACTIVE.
        options::set_in(s, "hashdirs", interactive);
        for (opt, on) in &args.options {
            options::set_in(s, opt, *on);
        }
        options::set_in(s, "interactive", interactive);
        // $ZSH_ARGZERO: $0 as the shell started.
        let a0 = args.script.clone().unwrap_or_else(|| s.argv0.clone());
        s.store("ZSH_ARGZERO", meowsh::params::Value::Scalar(a0));
        // $ZSH_SCRIPT: the script being run, unset for -c and stdin.
        if let Some(script) = &args.script {
            s.store("ZSH_SCRIPT", meowsh::params::Value::Scalar(script.clone()));
        }
        // $ZSH_NAME: what the shell was invoked as.
        let invoked = argv.first().map(|a| a.as_str()).unwrap_or("meowsh");
        let invoked = invoked.rsplit('/').next().unwrap_or(invoked);
        s.store(
            "ZSH_NAME",
            meowsh::params::Value::Scalar(invoked.trim_start_matches('-').to_string()),
        );
        if let Ok(mut c) = meowsh::shell::EVAL_CONTEXT.lock() {
            c.push(if args.command.is_some() {
                "cmdarg"
            } else {
                "toplevel"
            });
        }
        let name = s.script_name.clone();
        let stdin = args.command.is_none() && args.script.is_none();
        meowsh::shell::with_err_ctx(|e| {
            e.name = name;
            e.interactive = interactive;
            e.stdin = stdin;
        });
        options::set_in(s, "login", args.login);
        options::set_in(
            s,
            "shinstdin",
            args.command.is_none() && args.script.is_none(),
        );
    });

    if interactive && tty_in {
        init_job_control();
    }
    meowsh::trap::init_shell_signals();

    if options::is_set("rcs") {
        source_startup_files(args.login, interactive);
    } else {
        // -f: zsh keeps no history file without its startup files, and
        // neither does meowsh.
        with_shell(|s| s.history_file.clear());
    }

    let status = if let Some(cmd) = &args.command {
        meowsh::exec::EXITING_SHELL.store(true, std::sync::atomic::Ordering::Relaxed);
        run_string(cmd)
    } else if let Some(path) = &args.script {
        // PATH_SCRIPT: a script name without a slash not found here is
        // looked for in $PATH.
        let opened = open_script(path).or_else(|e| {
            if e != libc::ENOENT || path.contains('/') || !options::is_set("pathscript") {
                return Err(e);
            }
            meowsh::shell::var_get_opt("PATH")
                .unwrap_or_default()
                .split(':')
                .filter(|d| !d.is_empty())
                .map(|d| format!("{}/{}", d, path))
                .find(|p| std::path::Path::new(p).is_file())
                .map_or(Err(e), |p| open_script(&p))
        });
        match opened {
            Ok(fd) => run_fd(fd, false),
            Err(e) => {
                // zsh's wording, naming the shell as it was invoked.
                let argv0 = env::args().next().unwrap_or_else(|| "meowsh".to_string());
                if e == libc::ENOENT {
                    eprintln!("{}: can't open input file: {}", argv0, path);
                } else {
                    eprintln!("{}: {}: {}", argv0, path, strerror(e).to_lowercase());
                }
                exit_shell(if e == libc::ENOENT { 127 } else { 126 });
            }
        }
    } else if interactive {
        run_interactive()
    } else {
        run_fd(0, false)
    };
    let status = match flow() {
        Flow::Exit(n) => n,
        Flow::None => status,
        _ => with_shell(|s| s.last_status),
    };
    exit_shell(status);
}

// Opens a script on a descriptor >= 10 so the script's own redirections
// of low descriptors cannot disturb reading it.
fn open_script(path: &str) -> Result<i32, i32> {
    let c = std::ffi::CString::new(path).map_err(|_| libc::EINVAL)?;
    let errno = || std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(errno());
    }
    let high = meowsh::sys::dup_high(fd);
    meowsh::sys::close(fd);
    if high < 0 {
        return Err(errno());
    }
    Ok(high)
}

fn init_job_control() {
    unsafe {
        let fd = libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 10);
        if fd < 0 {
            return;
        }
        // Wait until the shell is in the foreground before taking over.
        loop {
            let fg = libc::tcgetpgrp(fd);
            let me = libc::getpgrp();
            if fg < 0 || fg == me {
                break;
            }
            libc::kill(-me, libc::SIGTTIN);
        }
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        let pid = libc::getpid();
        libc::setpgid(pid, pid);
        let pgid = libc::getpgrp();
        if libc::tcsetpgrp(fd, pgid) != 0 {
            libc::close(fd);
            return;
        }
        TTY_FD.store(fd, Ordering::SeqCst);
        with_shell(|s| {
            s.job_control = true;
            s.shell_pgid = pgid;
            options::set_in(s, "monitor", true);
        });
    }
}

fn source_startup_files(login: bool, interactive: bool) {
    // The files are in $ZDOTDIR, or $HOME without it; .zshenv may set
    // ZDOTDIR for the others.
    let dir = || {
        meowsh::shell::var_get_opt("ZDOTDIR")
            .filter(|d| !d.is_empty())
            .or_else(|| meowsh::shell::var_get_opt("HOME"))
            .filter(|d| !d.is_empty())
    };
    let Some(env_dir) = dir() else {
        return;
    };
    // zsh order: .zshenv always, .zprofile for login shells, .zshrc for
    // interactive shells, .zlogin for interactive login shells.
    source_startup(&Path::new(&env_dir).join(".zshenv").to_string_lossy());
    // NO_RCS set by .zshenv skips the rest.
    if !options::is_set("rcs") {
        return;
    }
    let Some(d) = dir() else {
        return;
    };
    let home_path = Path::new(&d);
    if login {
        source_startup(&home_path.join(".zprofile").to_string_lossy());
    }
    if interactive {
        source_startup(&home_path.join(".zshrc").to_string_lossy());
    }
    if login && interactive {
        source_startup(&home_path.join(".zlogin").to_string_lossy());
    }
}

fn source_startup(path: &str) {
    if !Path::new(path).is_file() {
        return;
    }
    source_file(path, None);
    if !matches!(flow(), Flow::Exit(_)) {
        set_flow(Flow::None);
    }
    if let Flow::Exit(n) = flow() {
        exit_shell(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_parse_args_interactive() {
        let a = parse_args(&v(&["meowsh", "-i"])).unwrap();
        assert_eq!(a.interactive, Some(true));
        assert_eq!(a.command, None);
    }

    #[test]
    fn test_parse_args_command_with_arg0_and_params() {
        let a = parse_args(&v(&["meowsh", "-c", "echo $0 $1", "name", "one"])).unwrap();
        assert_eq!(a.command.as_deref(), Some("echo $0 $1"));
        assert_eq!(a.arg0.as_deref(), Some("name"));
        assert_eq!(a.positional, v(&["one"]));
    }

    #[test]
    fn test_parse_args_both() {
        let a = parse_args(&v(&["meowsh", "-i", "-c", "ls"])).unwrap();
        assert_eq!(a.interactive, Some(true));
        assert_eq!(a.command.as_deref(), Some("ls"));
    }

    #[test]
    fn test_parse_args_script_file() {
        let a = parse_args(&v(&["meowsh", "-e", "script.sh", "x", "y"])).unwrap();
        assert_eq!(a.script.as_deref(), Some("script.sh"));
        assert_eq!(a.positional, v(&["x", "y"]));
        assert_eq!(a.options, vec![("errexit", true)]);
    }

    #[test]
    fn test_parse_args_stdin_mode() {
        let a = parse_args(&v(&["meowsh", "-s", "a", "b"])).unwrap();
        assert_eq!(a.script, None);
        assert_eq!(a.positional, v(&["a", "b"]));
    }

    #[test]
    fn test_parse_args_none() {
        assert_eq!(parse_args(&v(&["meowsh"])).unwrap(), Args::default());
    }

    #[test]
    fn test_parse_args_rejects_unknown_option() {
        assert!(parse_args(&v(&["meowsh", "-j"])).is_err());
        assert!(parse_args(&v(&["meowsh", "-c"])).is_err());
    }
}
