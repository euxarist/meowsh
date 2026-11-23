//! zsh/system: `sysread`, `syswrite`, `sysopen`, `sysseek`, `syserror`
//! and `zsystem` (`flock`, `supports`), plus the data behind `$errnos`,
//! `$sysparams` and the `systell()` math function.

use crate::arith::Num;
use crate::shell::{builtin_err, shell_err};
use crate::types::Flow;
use std::ffi::CString;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const SYSREAD_BUFSIZE: usize = 8192;

/// zsh's `sys_errnames` for Linux: entry `n` names errno `n + 1`; gaps in
/// the system's numbering are filled with `E<n>`.
const SYS_ERRNAMES: [&str; 134] = [
    "EPERM",
    "ENOENT",
    "ESRCH",
    "EINTR",
    "EIO",
    "ENXIO",
    "E2BIG",
    "ENOEXEC",
    "EBADF",
    "ECHILD",
    "EAGAIN",
    "ENOMEM",
    "EACCES",
    "EFAULT",
    "ENOTBLK",
    "EBUSY",
    "EEXIST",
    "EXDEV",
    "ENODEV",
    "ENOTDIR",
    "EISDIR",
    "EINVAL",
    "ENFILE",
    "EMFILE",
    "ENOTTY",
    "ETXTBSY",
    "EFBIG",
    "ENOSPC",
    "ESPIPE",
    "EROFS",
    "EMLINK",
    "EPIPE",
    "EDOM",
    "ERANGE",
    "EDEADLK",
    "ENAMETOOLONG",
    "ENOLCK",
    "ENOSYS",
    "ENOTEMPTY",
    "ELOOP",
    "E41",
    "ENOMSG",
    "EIDRM",
    "ECHRNG",
    "EL2NSYNC",
    "EL3HLT",
    "EL3RST",
    "ELNRNG",
    "EUNATCH",
    "ENOCSI",
    "EL2HLT",
    "EBADE",
    "EBADR",
    "EXFULL",
    "ENOANO",
    "EBADRQC",
    "EBADSLT",
    "E58",
    "EBFONT",
    "ENOSTR",
    "ENODATA",
    "ETIME",
    "ENOSR",
    "ENONET",
    "ENOPKG",
    "EREMOTE",
    "ENOLINK",
    "EADV",
    "ESRMNT",
    "ECOMM",
    "EPROTO",
    "EMULTIHOP",
    "EDOTDOT",
    "EBADMSG",
    "EOVERFLOW",
    "ENOTUNIQ",
    "EBADFD",
    "EREMCHG",
    "ELIBACC",
    "ELIBBAD",
    "ELIBSCN",
    "ELIBMAX",
    "ELIBEXEC",
    "EILSEQ",
    "ERESTART",
    "ESTRPIPE",
    "EUSERS",
    "ENOTSOCK",
    "EDESTADDRREQ",
    "EMSGSIZE",
    "EPROTOTYPE",
    "ENOPROTOOPT",
    "EPROTONOSUPPORT",
    "ESOCKTNOSUPPORT",
    "EOPNOTSUPP",
    "EPFNOSUPPORT",
    "EAFNOSUPPORT",
    "EADDRINUSE",
    "EADDRNOTAVAIL",
    "ENETDOWN",
    "ENETUNREACH",
    "ENETRESET",
    "ECONNABORTED",
    "ECONNRESET",
    "ENOBUFS",
    "EISCONN",
    "ENOTCONN",
    "ESHUTDOWN",
    "ETOOMANYREFS",
    "ETIMEDOUT",
    "ECONNREFUSED",
    "EHOSTDOWN",
    "EHOSTUNREACH",
    "EALREADY",
    "EINPROGRESS",
    "ESTALE",
    "EUCLEAN",
    "ENOTNAM",
    "ENAVAIL",
    "EISNAM",
    "EREMOTEIO",
    "EDQUOT",
    "ENOMEDIUM",
    "EMEDIUMTYPE",
    "ECANCELED",
    "ENOKEY",
    "EKEYEXPIRED",
    "EKEYREVOKED",
    "EKEYREJECTED",
    "EOWNERDEAD",
    "ENOTRECOVERABLE",
    "ERFKILL",
    "EHWPOISON",
    "EFTYPE",
];

/// The keys of `$sysparams`, in zsh's scan order.
pub const SYSPARAM_KEYS: [&str; 3] = ["pid", "ppid", "procsubstpid"];

/// zsh's errno as left by these builtins: cleared on entry so that
/// `syserror` with no argument reports only a system error.
static ERRNO: AtomicI32 = AtomicI32::new(0);

/// The pid of the last process substitution (`$sysparams[procsubstpid]`).
static PROCSUBSTPID: AtomicI32 = AtomicI32::new(0);

/// Descriptors opened by `zsystem flock` (zsh's FDT_FLOCK entries).
static LOCK_FDS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// `$errnos`: zsh's errno names, in errno order.
pub fn errnos() -> Vec<String> {
    SYS_ERRNAMES.iter().map(|s| s.to_string()).collect()
}

/// Records the pid of a process substitution for `$sysparams`.
pub fn set_procsubstpid(pid: i32) {
    PROCSUBSTPID.store(pid, Ordering::SeqCst);
}

/// `$sysparams[key]`; None for an unknown key.
pub fn sysparam(key: &str) -> Option<String> {
    let num = match key {
        "pid" => std::process::id() as i32,
        // SAFETY: getppid takes no arguments and cannot fail.
        "ppid" => unsafe { libc::getppid() },
        "procsubstpid" => PROCSUBSTPID.load(Ordering::SeqCst),
        _ => return None,
    };
    Some(num.to_string())
}

/// The `systell()` math function: the current offset of `fd`.
pub fn systell(fd: i64) -> Result<i64, String> {
    if fd < 0 || fd > libc::c_int::MAX as i64 {
        return Err("file descriptor out of range".to_string());
    }
    // SAFETY: lseek takes no pointers; a bad fd just fails with -1.
    Ok(unsafe { libc::lseek(fd as libc::c_int, 0, libc::SEEK_CUR) })
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Saves the current errno for `syserror` and returns it.
fn save_errno() -> i32 {
    let e = errno();
    ERRNO.store(e, Ordering::SeqCst);
    e
}

// zsh's %e: the system message with its first letter lowercased.
fn errmsg(e: i32) -> String {
    if e == libc::EINTR {
        return "interrupt".to_string();
    }
    let msg = crate::exec::strerror(e);
    if e == libc::EIO {
        return msg;
    }
    let mut c = msg.chars();
    match c.next() {
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
        None => msg,
    }
}

// zsh retries EINTR unless errflag, retflag, breaks or contflag is set;
// here a trap that ran may have changed the flow of control.
fn interrupted() -> bool {
    if crate::trap::has_pending() {
        crate::trap::run_pending();
    }
    !matches!(crate::exec::flow(), Flow::None)
}

/// zsh's isident(): a parameter name, optionally with a subscript.
fn isident(s: &str) -> bool {
    match s.find('[') {
        Some(i) => i > 0 && s.ends_with(']') && crate::lexer::is_valid_name(&s[..i]),
        None => crate::lexer::is_valid_name(s),
    }
}

fn setsparam(name: &str, value: &str) -> bool {
    match name.find('[') {
        Some(i) => matches!(
            crate::exec::set_subscripted(&name[..i], &name[i + 1..name.len() - 1], value, false),
            Ok(true)
        ),
        None => crate::shell::var_set(name, value, false),
    }
}

fn setiparam(name: &str, value: i64) -> bool {
    setsparam(name, &value.to_string())
}

fn matheval(expr: &str) -> Option<Num> {
    match crate::arith::eval_num(expr) {
        Ok(v) => Some(v),
        Err(e) => {
            if !e.is_empty() {
                shell_err(&e);
            }
            None
        }
    }
}

/// A non-negative decimal integer (zstrtol: leading blanks and a sign
/// allowed, nothing after the digits).
fn getposint(instr: &str, nam: &str) -> Option<i32> {
    let s = instr.trim_start_matches([' ', '\t', '\n']);
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let ret = if digits.bytes().all(|b| b.is_ascii_digit()) {
        let n = digits.bytes().fold(0i64, |n, b| {
            n.wrapping_mul(10).wrapping_add(i64::from(b - b'0'))
        });
        let n = if neg { n.wrapping_neg() } else { n };
        Some(n as i32)
    } else {
        None
    };
    match ret {
        Some(n) if n >= 0 => Some(n),
        _ => {
            builtin_err(nam, &format!("integer expected: {}", instr));
            None
        }
    }
}

/// Options given to a builtin, with their arguments.
struct Ops(Vec<(char, Option<String>)>);

impl Ops {
    fn isset(&self, c: char) -> bool {
        self.0.iter().any(|o| o.0 == c)
    }

    fn arg(&self, c: char) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|o| o.0 == c)
            .and_then(|o| o.1.as_deref())
    }
}

/// zsh's execbuiltin() option parsing for an option string such as
/// `"c:i:"` (a `:` marks an option taking an argument), then the
/// argument count check. None after an error was reported.
fn parse_ops<'a>(
    words: &'a [String],
    spec: &str,
    min: usize,
    max: usize,
) -> Option<(Ops, &'a [String])> {
    let nam = words[0].as_str();
    let mut ops = Vec::new();
    let mut i = 1;
    while let Some(a) = words.get(i) {
        let Some(cluster) = a.strip_prefix('-') else {
            break;
        };
        i += 1;
        if cluster.is_empty() || cluster == "-" {
            break;
        }
        for (k, c) in cluster.char_indices() {
            let pos = spec.find(c).filter(|_| c != ':');
            let Some(pos) = pos else {
                builtin_err(nam, &format!("bad option: -{}", c));
                return None;
            };
            if !spec[pos + c.len_utf8()..].starts_with(':') {
                ops.push((c, None));
                continue;
            }
            let rest = &cluster[k + c.len_utf8()..];
            let val = if !rest.is_empty() {
                rest.to_string()
            } else if let Some(v) = words.get(i) {
                i += 1;
                v.clone()
            } else {
                builtin_err(nam, &format!("argument expected: -{}", c));
                return None;
            };
            ops.push((c, Some(val)));
            break;
        }
    }
    let args = &words[i..];
    if args.len() < min {
        builtin_err(nam, "not enough arguments");
        return None;
    }
    if args.len() > max {
        builtin_err(nam, "too many arguments");
        return None;
    }
    Some((Ops(ops), args))
}

/// Moves `fd` to the lowest free descriptor >= 10, closing the original.
fn movefd(fd: i32) -> i32 {
    if fd == -1 || fd >= 10 {
        return fd;
    }
    // SAFETY: F_DUPFD takes an integer argument; failure returns -1.
    let fe = unsafe { libc::fcntl(fd, libc::F_DUPFD, 10) };
    crate::sys::close(fd);
    fe
}

/// Moves descriptor `x` to `y`; -1 on failure.
fn redup(x: i32, y: i32) -> i32 {
    if x == y {
        return y;
    }
    // SAFETY: dup2 takes plain descriptors; failure returns -1.
    let ret = if unsafe { libc::dup2(x, y) } == -1 {
        -1
    } else {
        y
    };
    crate::sys::close(x);
    ret
}

/*
 * Return values of sysread:
 *	0	Successfully read (and written if appropriate)
 *	1	Error in parameters to command
 *	2	Error on read, or polling read fd
 *	3	Error on write
 *	4	Timeout on read
 *	5	Zero bytes read, end of file
 */
pub fn builtin_sysread(words: &[String]) -> i32 {
    let nam = words[0].as_str();
    ERRNO.store(0, Ordering::SeqCst);
    let Some((ops, args)) = parse_ops(words, "c:i:o:s:t:", 0, 1) else {
        return 1;
    };
    let mut infd = 0;
    let mut outfd = -1;
    let mut bufsize = SYSREAD_BUFSIZE;
    if let Some(a) = ops.arg('i') {
        match getposint(a, nam) {
            Some(n) => infd = n,
            None => return 1,
        }
    }
    if let Some(a) = ops.arg('o') {
        match getposint(a, nam) {
            Some(n) => outfd = n,
            None => return 1,
        }
    }
    if let Some(a) = ops.arg('s') {
        match getposint(a, nam) {
            Some(n) => bufsize = n as usize,
            None => return 1,
        }
    }
    let countvar = ops.arg('c');
    // With -o, only what could not be written is stored, and only in a
    // parameter that was named.
    let outvar = args.first().map(String::as_str);
    for v in countvar.iter().chain(outvar.iter()) {
        if !isident(v) {
            builtin_err(nam, &format!("not an identifier: {}", v));
            return 1;
        }
    }

    if let Some(t) = ops.arg('t') {
        let Some(to) = matheval(t) else {
            return 1;
        };
        let to_int = match to {
            Num::F(d) => (1000.0 * d) as i32,
            Num::I(l) => (l as i32).wrapping_mul(1000),
        };
        let mut poll_fd = libc::pollfd {
            fd: infd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = loop {
            // SAFETY: poll_fd is one valid pollfd for the call's duration.
            let r = unsafe { libc::poll(&mut poll_fd, 1, to_int) };
            if r < 0 && save_errno() == libc::EINTR && !interrupted() {
                continue;
            }
            break r;
        };
        if ret <= 0 {
            // A non-timeout error counts as an error on read.
            return if ret != 0 { 2 } else { 4 };
        }
    }

    let mut inbuf = vec![0u8; bufsize];
    let count = loop {
        // SAFETY: inbuf holds bufsize writable bytes.
        let n = unsafe { libc::read(infd, inbuf.as_mut_ptr().cast(), bufsize) };
        if n < 0 && save_errno() == libc::EINTR && !interrupted() {
            continue;
        }
        break n;
    };
    if let Some(c) = countvar {
        setiparam(c, count as i64);
    }
    if count < 0 {
        return 2;
    }
    let mut count = count as usize;

    if outfd >= 0 {
        if count == 0 {
            return 5;
        }
        let mut off = 0;
        while count > 0 {
            // SAFETY: off + count <= inbuf.len(), so the range is in bounds.
            let ret = unsafe { libc::write(outfd, inbuf[off..].as_ptr().cast(), count) };
            if ret < 0 {
                if save_errno() == libc::EINTR && !interrupted() {
                    continue;
                }
                if let Some(v) = outvar {
                    setsparam(v, &String::from_utf8_lossy(&inbuf[off..off + count]));
                }
                if let Some(c) = countvar {
                    setiparam(c, count as i64);
                }
                return 3;
            }
            off += ret as usize;
            count -= ret as usize;
        }
        return 0;
    }

    // Set even when nothing was read.
    setsparam(
        outvar.unwrap_or("REPLY"),
        &String::from_utf8_lossy(&inbuf[..count]),
    );
    if count > 0 {
        0
    } else {
        5
    }
}

/*
 * Return values of syswrite:
 *	0	Successfully written
 *	1	Error in parameters to command
 *	2	Error on write
 */
pub fn builtin_syswrite(words: &[String]) -> i32 {
    let nam = words[0].as_str();
    ERRNO.store(0, Ordering::SeqCst);
    let Some((ops, args)) = parse_ops(words, "c:o:", 1, 1) else {
        return 1;
    };
    let mut outfd = 1;
    if let Some(a) = ops.arg('o') {
        match getposint(a, nam) {
            Some(n) => outfd = n,
            None => return 1,
        }
    }
    let countvar = ops.arg('c');
    if let Some(c) = countvar.filter(|c| !isident(c)) {
        builtin_err(nam, &format!("not an identifier: {}", c));
        return 1;
    }

    let data = args[0].as_bytes();
    let mut totcount = 0;
    while totcount < data.len() {
        let rest = &data[totcount..];
        // SAFETY: rest is a live slice of rest.len() bytes.
        let count = unsafe { libc::write(outfd, rest.as_ptr().cast(), rest.len()) };
        if count < 0 {
            if save_errno() == libc::EINTR && !interrupted() {
                continue;
            }
            if let Some(c) = countvar {
                setiparam(c, totcount as i64);
            }
            return 2;
        }
        totcount += count as usize;
    }
    if let Some(c) = countvar {
        setiparam(c, totcount as i64);
    }
    0
}

/// `sysopen -o` names and their open(2) flags.
const OPENOPTS: &[(&str, libc::c_int)] = &[
    ("cloexec", libc::O_CLOEXEC),
    ("nofollow", libc::O_NOFOLLOW),
    ("sync", libc::O_SYNC),
    #[cfg(target_os = "linux")]
    ("noatime", libc::O_NOATIME),
    ("nonblock", libc::O_NONBLOCK),
    ("excl", libc::O_EXCL | libc::O_CREAT),
    ("creat", libc::O_CREAT),
    ("create", libc::O_CREAT),
    ("truncate", libc::O_TRUNC),
    ("trunc", libc::O_TRUNC),
];

/*
 * Return values of sysopen:
 *	0	Success
 *	1	Error in parameters to command
 *	2	Error on open
 */
pub fn builtin_sysopen(words: &[String]) -> i32 {
    let nam = words[0].as_str();
    ERRNO.store(0, Ordering::SeqCst);
    let Some((ops, args)) = parse_ops(words, "rwau:o:m:", 1, 1) else {
        return 1;
    };
    let read = ops.isset('r');
    let write = ops.isset('w');
    let append = if ops.isset('a') { libc::O_APPEND } else { 0 };
    let mut flags = libc::O_NOCTTY
        | append
        | if append != 0 || write {
            if read {
                libc::O_RDWR
            } else {
                libc::O_WRONLY
            }
        } else {
            libc::O_RDONLY
        };

    let Some(fdvar) = ops.arg('u') else {
        builtin_err(nam, "file descriptor not specified");
        return 1;
    };
    // Either a single digit 0-9 or a parameter to receive the new fd.
    let explicit = match fdvar.as_bytes() {
        [d] if d.is_ascii_digit() => i32::from(d - b'0'),
        _ if !isident(fdvar) => {
            builtin_err(nam, &format!("not an identifier: {}", fdvar));
            return 1;
        }
        _ => -1,
    };

    if let Some(opts) = ops.arg('o') {
        for opt in opts.split(',') {
            let opt = match opt.get(..2) {
                Some(p) if p.eq_ignore_ascii_case("O_") => &opt[2..],
                _ => opt,
            };
            match OPENOPTS.iter().find(|o| o.0.eq_ignore_ascii_case(opt)) {
                Some(o) => flags |= o.1,
                None => {
                    builtin_err(nam, &format!("unsupported option: {}", opt));
                    return 1;
                }
            }
        }
    }

    let mut perms: libc::c_uint = 0o666;
    if let Some(m) = ops.arg('m') {
        if m.len() < 3 || !m.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
            builtin_err(nam, &format!("invalid mode {}", m));
            return 1;
        }
        perms = m.bytes().fold(0, |p: libc::c_uint, b| {
            p.wrapping_mul(8).wrapping_add(libc::c_uint::from(b - b'0'))
        });
    }

    let path = &args[0];
    let fd = match CString::new(path.as_str()) {
        // SAFETY: c is NUL-terminated; the mode is only read with O_CREAT.
        Ok(c) => unsafe { libc::open(c.as_ptr(), flags, perms) },
        Err(_) => {
            ERRNO.store(libc::ENOENT, Ordering::SeqCst);
            builtin_err(
                nam,
                &format!("can't open file {}: {}", path, errmsg(libc::ENOENT)),
            );
            return 2;
        }
    };
    if fd == -1 {
        let e = save_errno();
        builtin_err(nam, &format!("can't open file {}: {}", path, errmsg(e)));
        return 2;
    }
    let moved_fd = if explicit > -1 {
        redup(fd, explicit)
    } else {
        movefd(fd)
    };
    if moved_fd == -1 {
        save_errno();
        builtin_err(nam, &format!("can't open file {}", path));
        return 2;
    }
    // O_CLOEXEC belongs to the descriptor, not the open file, so a dup
    // loses it.
    if flags & libc::O_CLOEXEC != 0 && fd != moved_fd {
        crate::sys::set_cloexec(moved_fd);
    }
    if explicit == -1 && !setiparam(fdvar, i64::from(moved_fd)) {
        crate::sys::close(moved_fd);
    }
    0
}

/*
 * Return values of sysseek:
 *	0	Success
 *	1	Error in parameters to command
 *	2	Error on seek
 */
pub fn builtin_sysseek(words: &[String]) -> i32 {
    let nam = words[0].as_str();
    ERRNO.store(0, Ordering::SeqCst);
    let Some((ops, args)) = parse_ops(words, "u:w:", 1, 1) else {
        return 1;
    };
    let mut fd = 0;
    if let Some(a) = ops.arg('u') {
        match getposint(a, nam) {
            Some(n) => fd = n,
            None => return 1,
        }
    }
    let w = match ops.arg('w') {
        None => libc::SEEK_SET,
        Some(s) if s.eq_ignore_ascii_case("current") || s == "1" => libc::SEEK_CUR,
        Some(s) if s.eq_ignore_ascii_case("end") || s == "2" => libc::SEEK_END,
        Some(s) if s.eq_ignore_ascii_case("start") || s == "0" => libc::SEEK_SET,
        Some(s) => {
            builtin_err(nam, &format!("unknown argument to -w: {}", s));
            return 1;
        }
    };
    let Some(pos) = matheval(&args[0]) else {
        return 1;
    };
    // SAFETY: lseek takes no pointers; a bad fd just fails with -1.
    if unsafe { libc::lseek(fd, pos.as_int(), w) } == -1 {
        save_errno();
        2
    } else {
        0
    }
}

/*
 * Return values of syserror:
 *	0	Successfully processed error
 *		(although if the number was invalid the string
 *		may not be useful)
 *	1	Error in parameters
 *	2	Name of error not recognised.
 */
pub fn builtin_syserror(words: &[String]) -> i32 {
    let nam = words[0].as_str();
    let Some((ops, args)) = parse_ops(words, "e:p:", 0, 1) else {
        return 1;
    };
    let errvar = ops.arg('e');
    if let Some(v) = errvar.filter(|v| !isident(v)) {
        builtin_err(nam, &format!("not an identifier: {}", v));
        return 1;
    }
    let pfx = ops.arg('p').unwrap_or("");

    let num = match args.first() {
        None => ERRNO.load(Ordering::SeqCst),
        Some(a) if !a.is_empty() && a.bytes().all(|b| b.is_ascii_digit()) => {
            a.parse().unwrap_or(i32::MAX)
        }
        Some(a) => match SYS_ERRNAMES.iter().position(|n| n == a) {
            Some(i) => i as i32 + 1,
            None => return 2,
        },
    };

    let msg = format!("{}{}", pfx, crate::exec::strerror(num));
    match errvar {
        Some(v) => {
            setsparam(v, &msg);
        }
        None => crate::sys::err_line(&msg),
    }
    0
}

/// The argument of a `zsystem flock` option: the rest of the cluster, or
/// the next word.
fn flock_optarg<'a>(rest: &'a str, args: &'a [String], i: &mut usize) -> Option<&'a str> {
    if !rest.is_empty() {
        Some(rest)
    } else {
        let a = args.get(*i)?;
        *i += 1;
        Some(a.as_str())
    }
}

fn bin_zsystem_flock(nam: &str, args: &[String]) -> i32 {
    let mut cloexec = true;
    let mut unlock = false;
    let mut readlock = false;
    let mut timeout: f64 = -1.0;
    let mut timeout_interval: i64 = 1_000_000;
    let mut fdvar: Option<&str> = None;

    let mut i = 0;
    while let Some(opts) = args.get(i).and_then(|a| a.strip_prefix('-')) {
        i += 1;
        if opts.is_empty() || opts == "-" {
            break;
        }
        let mut k = 0;
        while let Some(opt) = opts[k..].chars().next() {
            let rest = &opts[k + opt.len_utf8()..];
            k += opt.len_utf8();
            match opt {
                // Keep the lock across exec.
                'e' => cloexec = false,
                'f' => {
                    match flock_optarg(rest, args, &mut i).filter(|v| isident(v)) {
                        Some(v) => fdvar = Some(v),
                        None => {
                            builtin_err(nam, "flock: option f requires a variable name");
                            return 1;
                        }
                    }
                    if !rest.is_empty() {
                        break;
                    }
                }
                'r' => readlock = true,
                't' => {
                    let Some(optarg) = flock_optarg(rest, args, &mut i) else {
                        builtin_err(nam, "flock: option t requires a numeric timeout");
                        return 1;
                    };
                    let Some(v) = matheval(optarg) else {
                        return 1;
                    };
                    timeout = v.as_float_pub();
                    // Conservatively below 2^30 seconds so a deadline can
                    // never overflow time_t.
                    if timeout > 1073741823.0 {
                        builtin_err(nam, &format!("flock: invalid timeout value: '{}'", optarg));
                        return 1;
                    }
                    if !rest.is_empty() {
                        break;
                    }
                }
                'i' => {
                    let Some(optarg) = flock_optarg(rest, args, &mut i) else {
                        builtin_err(nam, "flock: option i requires a numeric retry interval");
                        return 1;
                    };
                    let Some(v) = matheval(optarg) else {
                        return 1;
                    };
                    let us = (v.as_float_pub() * 1e6).ceil();
                    // Not NaN, at least 1us, and fits a long.
                    if !(1.0..=-(0.999 * i64::MIN as f64)).contains(&us) {
                        builtin_err(nam, &format!("flock: invalid interval value: '{}'", optarg));
                        return 1;
                    }
                    timeout_interval = us as i64;
                    if !rest.is_empty() {
                        break;
                    }
                }
                'u' => unlock = true,
                _ => {
                    builtin_err(nam, &format!("flock: bad option: {}", opt));
                    return 1;
                }
            }
        }
    }

    let args = &args[i..];
    if args.is_empty() {
        builtin_err(nam, "flock: not enough arguments");
        return 1;
    }
    if args.len() > 1 {
        builtin_err(nam, "flock: too many arguments");
        return 1;
    }
    let file = args[0].as_str();

    if unlock {
        let Some(fd) = matheval(file) else {
            return 1;
        };
        let fd = fd.as_int() as i32;
        if zcloselockfd(fd) < 0 {
            builtin_err(
                nam,
                &format!("flock: file descriptor {} not in use for locking", fd),
            );
            return 1;
        }
        return 0;
    }

    let flags = libc::O_NOCTTY
        | if readlock {
            libc::O_RDONLY
        } else {
            libc::O_RDWR
        };
    // A name with a NUL cannot exist.
    let opened = match CString::new(file) {
        // SAFETY: c is a NUL-terminated path.
        Ok(c) => match unsafe { libc::open(c.as_ptr(), flags) } {
            -1 => Err(errno()),
            fd => Ok(fd),
        },
        Err(_) => Err(libc::ENOENT),
    };
    let fd = match opened {
        Ok(fd) => fd,
        Err(e) => {
            builtin_err(
                nam,
                &format!(
                    "failed to open {} for {}ing: {}",
                    file,
                    if readlock { "read" } else { "writ" },
                    errmsg(e)
                ),
            );
            return 1;
        }
    };
    let flock_fd = movefd(fd);
    if flock_fd == -1 {
        return 1;
    }
    if cloexec {
        crate::sys::set_cloexec(flock_fd);
    }
    LOCK_FDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(flock_fd);

    // SAFETY: flock is plain data; all-zero is a valid value.
    let mut lck: libc::flock = unsafe { std::mem::zeroed() };
    let l_type = if readlock {
        libc::F_RDLCK
    } else {
        libc::F_WRLCK
    };
    lck.l_type = l_type as libc::c_short;
    lck.l_whence = libc::SEEK_SET as libc::c_short;
    // l_start and l_len zero: lock the whole file.

    let fail = |e: Option<i32>| {
        zcloselockfd(flock_fd);
        if let Some(e) = e {
            builtin_err(nam, &format!("failed to lock file {}: {}", file, errmsg(e)));
        }
        1
    };

    if timeout > 0.0 {
        let end = Instant::now() + Duration::from_secs_f64(timeout);
        loop {
            // SAFETY: lck is a valid struct flock for the call's duration.
            if unsafe { libc::fcntl(flock_fd, libc::F_SETLK, std::ptr::addr_of!(lck)) } >= 0 {
                break;
            }
            let e = errno();
            if interrupted() {
                return fail(None);
            }
            if e != libc::EINTR && e != libc::EACCES && e != libc::EAGAIN {
                return fail(Some(e));
            }
            let remaining = end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                zcloselockfd(flock_fd);
                return 2;
            }
            let interval = Duration::from_micros(timeout_interval as u64);
            std::thread::sleep(interval.min(remaining));
        }
    } else {
        let cmd = if timeout == 0.0 {
            libc::F_SETLK
        } else {
            libc::F_SETLKW
        };
        loop {
            // SAFETY: lck is a valid struct flock for the call's duration.
            if unsafe { libc::fcntl(flock_fd, cmd, std::ptr::addr_of!(lck)) } >= 0 {
                break;
            }
            let e = errno();
            if interrupted() {
                return fail(None);
            }
            if e != libc::EINTR {
                return fail(Some(e));
            }
        }
    }

    if let Some(v) = fdvar {
        setiparam(v, i64::from(flock_fd));
    }
    0
}

/// Closes `fd` if `zsystem flock` opened it; -1 if it did not.
fn zcloselockfd(fd: i32) -> i32 {
    let mut fds = LOCK_FDS.lock().unwrap_or_else(|e| e.into_inner());
    match fds.iter().position(|&f| f == fd) {
        Some(k) => {
            fds.remove(k);
            crate::sys::close(fd);
            0
        }
        None => -1,
    }
}

/// Status 0 if the zsystem feature is supported, else 1, silently so
/// scripts can probe for future features.
fn bin_zsystem_supports(nam: &str, args: &[String]) -> i32 {
    match args {
        [] => {
            builtin_err(nam, "supports: not enough arguments");
            255
        }
        [a] => i32::from(!matches!(a.as_str(), "supports" | "flock")),
        _ => {
            builtin_err(nam, "supports: too many arguments");
            255
        }
    }
}

pub fn builtin_zsystem(words: &[String]) -> i32 {
    let nam = words[0].as_str();
    let Some(sub) = words.get(1) else {
        builtin_err(nam, "not enough arguments");
        return 1;
    };
    match sub.as_str() {
        "flock" => bin_zsystem_flock(nam, &words[2..]),
        "supports" => bin_zsystem_supports(nam, &words[2..]),
        _ => {
            builtin_err(nam, &format!("unknown subcommand: {}", sub));
            1
        }
    }
}
