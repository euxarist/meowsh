use crate::params::{
    Param, Value, DEFAULT_TIES, EXPORT, FLOAT_E, FLOAT_F, INTEGER, LEFT, PRIVATE_HIDDEN, READONLY,
    RIGHT_B, RIGHT_Z, UNIQUE, UNSET,
};
use crate::types::{Shell, OPT_ALLEXPORT};
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::path::PathBuf;
use std::sync::Mutex;

pub struct ShellState {
    pub shell: Mutex<Shell>,
}

impl Default for ShellState {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellState {
    pub fn new() -> Self {
        ShellState {
            shell: Mutex::new(Shell::new()),
        }
    }
}

use std::sync::LazyLock;

pub static SHELL: LazyLock<ShellState> = LazyLock::new(ShellState::new);

/// Runs `f` with the global shell state locked. Recovers from a poisoned
/// lock so a panic in one builtin does not wedge the shell.
pub fn with_shell<R>(f: impl FnOnce(&mut Shell) -> R) -> R {
    let mut guard = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

pub fn shell_init() {
    let mut shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
    shell.shell_pid = std::process::id() as i32;
    shell.shell_pgid = unsafe { libc::getpgrp() };
    shell.last_status = 0;
    *shell.next_job_id.lock().unwrap() = 1;

    // Every environment variable becomes an exported shell variable.
    {
        use std::os::unix::ffi::OsStrExt;
        for (key, value) in env::vars_os() {
            let Some(key) = key.to_str() else {
                continue;
            };
            if crate::lexer::is_valid_name(key) {
                let mut p = Param::scalar(bytes_to_zstr(value.as_bytes()));
                p.attrs = EXPORT;
                shell.params.insert(key.to_string(), p);
            }
        }
    }

    let set_default = |shell: &mut Shell, name: &str, value: String, attrs: u32| {
        shell.params.entry(name.to_string()).or_insert_with(|| {
            let mut p = Param::scalar(value);
            p.attrs = attrs;
            p
        });
    };
    let set = |shell: &mut Shell, name: &str, value: String, attrs: u32| {
        let mut p = Param::scalar(value);
        p.attrs = attrs;
        shell.params.insert(name.to_string(), p);
    };
    set_default(
        &mut shell,
        "PATH",
        "/usr/local/bin:/usr/bin:/bin".to_string(),
        EXPORT,
    );
    set_default(&mut shell, "PS2", "> ".to_string(), 0);
    set_default(&mut shell, "HISTSIZE", "1000".to_string(), 0);
    set_default(&mut shell, "SAVEHIST", "1000".to_string(), 0);
    set_default(&mut shell, "PS4", "+ ".to_string(), 0);
    // What a redirection with no command runs; unsetting NULLCMD makes
    // that an error.
    set_default(&mut shell, "NULLCMD", "cat".to_string(), 0);
    set_default(&mut shell, "READNULLCMD", "more".to_string(), 0);
    // zsh frameworks such as oh-my-zsh refuse to load without
    // ZSH_VERSION. The suffix keeps per-version files (for example
    // ~/.zcompdump-host-$ZSH_VERSION) apart from real zsh's.
    set(&mut shell, "ZSH_VERSION", "5.9-meowsh".to_string(), 0);
    let ostype = if cfg!(target_os = "linux") {
        "linux-gnu"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "freebsd") {
        "freebsd"
    } else if cfg!(target_os = "netbsd") {
        "netbsd"
    } else if cfg!(target_os = "openbsd") {
        "openbsd"
    } else {
        std::env::consts::OS
    };
    set_default(&mut shell, "OSTYPE", ostype.to_string(), 0);
    set_default(&mut shell, "CPUTYPE", std::env::consts::ARCH.to_string(), 0);
    set_default(
        &mut shell,
        "MACHTYPE",
        std::env::consts::ARCH.to_string(),
        0,
    );
    let vendor = if cfg!(target_os = "macos") {
        "apple"
    } else if cfg!(any(target_arch = "x86_64", target_arch = "x86")) {
        "pc"
    } else {
        "unknown"
    };
    set_default(&mut shell, "VENDOR", vendor.to_string(), 0);
    set(
        &mut shell,
        "ZSH_PATCHLEVEL",
        format!("zsh-5.9-meowsh-0-meowsh-{}", env!("CARGO_PKG_VERSION")),
        0,
    );
    set(
        &mut shell,
        "MEOWSH_VERSION",
        env!("CARGO_PKG_VERSION").to_string(),
        0,
    );
    set(&mut shell, "HOST", hostname_full(), 0);
    // POSIX: IFS is reset at startup rather than inherited (zsh's default
    // includes a trailing NUL).
    set(&mut shell, "IFS", " \t\n\0".to_string(), 0);
    let ppid = unsafe { libc::getppid() };
    set(&mut shell, "PPID", ppid.to_string(), 0);
    let shlvl = shell
        .scalar("SHLVL")
        .map(|v| {
            // Like zsh's strtol: the leading number, never an expression.
            let t = v.trim_start();
            let end = t
                .char_indices()
                .find(|&(k, c)| !(c.is_ascii_digit() || (k == 0 && c == '-')))
                .map_or(t.len(), |(k, _)| k);
            t[..end].parse::<i64>().unwrap_or(0)
        })
        .unwrap_or(0)
        .max(0)
        + 1;
    set(&mut shell, "SHLVL", shlvl.to_string(), EXPORT);
    if let Some(pwd) = initial_pwd(shell.scalar("PWD")) {
        set(&mut shell, "PWD", pwd, EXPORT);
    }

    if let Some(home) = shell.scalar("HOME").filter(|h| !h.is_empty()) {
        shell.history_file = PathBuf::from(home)
            .join(".meowsh_history")
            .to_string_lossy()
            .to_string();
    }

    START.get_or_init(std::time::Instant::now);
    if let Ok(mut s) = SECONDS_START.lock() {
        *s = Some((std::time::Instant::now(), 0.0));
    }
    crate::options::init(&mut shell, crate::options::Emulation::Zsh);
    // zsh/langinfo: the character set of the locale.
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|k| env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default()
        .to_lowercase();
    let codeset = if locale.contains("utf-8") || locale.contains("utf8") {
        "UTF-8"
    } else {
        "US-ASCII"
    };
    shell
        .assoc_entry("langinfo")
        .insert("CODESET".to_string(), codeset.to_string());
    // zsh/langinfo's parameter: zmodload must not see it as the user's.
    if let Some(p) = shell.param_mut("langinfo") {
        p.attrs |= crate::params::SPECIAL;
    }
    // Populate FPATH from system zsh function directories when not inherited,
    // matching zsh's default fpath so autoloadable functions (url-quote-magic,
    // bracketed-paste-magic, compinit, ...) are found out of the box.
    let fpath = default_fpath();
    if !fpath.is_empty() {
        set_default(&mut shell, "FPATH", fpath.join(":"), 0);
    }
    // zsh has each pair from the start, empty unless inherited.
    for (scalar, array) in DEFAULT_TIES {
        shell.tie(scalar, array, ":");
        for n in [scalar, array] {
            if let Some(p) = shell.param_mut(n) {
                p.attrs |= crate::params::SPECIAL;
            }
        }
    }
    drop(shell);
    sync_lc_ctype();
}

/// Discovers standard zsh function directories on the host so `$fpath`
/// matches a system zsh installation when `FPATH` is not inherited.
fn default_fpath() -> Vec<String> {
    use std::path::Path;

    let mut dirs = Vec::new();
    let mut push_if_dir = |p: &Path| {
        if p.is_dir() {
            if let Some(s) = p.to_str() {
                if !dirs.iter().any(|d| d == s) {
                    dirs.push(s.to_string());
                }
            }
        }
    };

    for site in [
        "/usr/local/share/zsh/site-functions",
        "/opt/homebrew/share/zsh/site-functions",
        "/usr/share/zsh/site-functions",
        "/usr/share/zsh/vendor-functions",
        "/usr/share/zsh/vendor-completions",
    ] {
        push_if_dir(Path::new(site));
    }

    // Collect a zsh functions tree (either flat or --enable-function-subdirs).
    fn collect_func_tree(root: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        let mut subdirs = Vec::new();
        let mut has_files = false;
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                subdirs.push(p);
            } else {
                has_files = true;
            }
        }
        subdirs.sort();
        if has_files || subdirs.is_empty() {
            if let Some(s) = root.to_str() {
                if !out.iter().any(|d| d == s) {
                    out.push(s.to_string());
                }
            }
        }
        for sub in subdirs {
            collect_func_tree(&sub, out);
        }
    }

    // Prefer an unversioned /usr/share/zsh/functions (Debian/Ubuntu/Arch),
    // otherwise pick the newest versioned share/zsh/<ver>/functions tree.
    let unversioned = Path::new("/usr/share/zsh/functions");
    if unversioned.is_dir() {
        collect_func_tree(unversioned, &mut dirs);
    } else {
        for base in [
            "/opt/homebrew/share/zsh",
            "/usr/local/share/zsh",
            "/usr/share/zsh",
        ] {
            let Ok(entries) = std::fs::read_dir(base) else {
                continue;
            };
            let mut versioned: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path().join("functions"))
                .filter(|p| p.is_dir())
                .collect();
            versioned.sort();
            if let Some(latest) = versioned.pop() {
                collect_func_tree(&latest, &mut dirs);
                break;
            }
        }
    }

    dirs
}

// Keeps an inherited $PWD only if it is absolute and names the current
// directory; otherwise uses the physical working directory.
fn initial_pwd(inherited: Option<&str>) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let cwd = env::current_dir().ok()?;
    if let Some(p) = inherited {
        if p.starts_with('/') {
            if let (Ok(a), Ok(b)) = (std::fs::metadata(p), std::fs::metadata(&cwd)) {
                if a.dev() == b.dev() && a.ino() == b.ino() {
                    return Some(p.to_string());
                }
            }
        }
    }
    Some(cwd.to_string_lossy().into_owned())
}

pub fn opt_set(flag: u32) -> bool {
    with_shell(|s| s.opts & flag != 0)
}

// With ALIASES off nothing is alias-expanded.
pub fn alias_get(name: &str) -> Option<String> {
    with_shell(|s| {
        crate::options::is_set_in(s, "aliases")
            .then(|| s.alias(name, false).cloned())
            .flatten()
    })
}

/// A suffix alias (`alias -s ext=cmd`) for a command word `file.ext`.
pub fn salias_for(word: &str) -> Option<String> {
    let (stem, ext) = word.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() {
        return None;
    }
    with_shell(|s| {
        crate::options::is_set_in(s, "aliases")
            .then(|| {
                s.assoc(crate::types::SALIASES)
                    .and_then(|m| m.get(ext).cloned())
            })
            .flatten()
    })
}

pub fn galias_get(name: &str) -> Option<String> {
    with_shell(|s| {
        crate::options::is_set_in(s, "aliases")
            .then(|| s.alias(name, true).cloned())
            .flatten()
    })
}

/// Value of a parameter, or None if it is unset.
pub fn var_get_opt(name: &str) -> Option<String> {
    let _swap = crate::shell::RefSwap::enter(name);
    if var_exists(name) {
        Some(var_get(name))
    } else {
        None
    }
}

/// Exported variables for a child process, with `overrides` applied.
pub fn build_env(overrides: &[(String, String)]) -> Vec<(String, String)> {
    let mut map: BTreeMap<String, String> = with_shell(|s| {
        s.params
            .iter()
            // SHLVL is always exported in zsh, even when recreated.
            .filter(|(k, p)| p.attrs & EXPORT != 0 || k.as_str() == "SHLVL")
            .filter_map(|(k, p)| match &p.value {
                Value::Scalar(v) => Some((k.clone(), v.clone())),
                _ => None,
            })
            // A hidden exported private stays in the environment, as
            // zsh does not take it out.
            .chain(
                s.private_stash
                    .iter()
                    .filter_map(|(_, k, p)| match &p.value {
                        Value::Scalar(v) if p.attrs & EXPORT != 0 => Some((k.clone(), v.clone())),
                        _ => None,
                    }),
            )
            .collect()
    });
    for (k, v) in overrides {
        map.insert(k.clone(), v.clone());
    }
    map.into_iter().collect()
}

static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
static SECONDS_START: std::sync::Mutex<Option<(std::time::Instant, f64)>> =
    std::sync::Mutex::new(None);
static RANDOM_STATE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn terminal_size() -> Option<(u16, u16)> {
    let tty = crate::jobs::TTY_FD.load(std::sync::atomic::Ordering::SeqCst);
    for fd in [tty, 2, 0, 1] {
        if fd < 0 {
            continue;
        }
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
            return Some((ws.ws_col, ws.ws_row));
        }
    }
    None
}

/// zsh's named directories (`hash -d name=dir`, `~name`).
pub static NAMED_DIRS: std::sync::Mutex<BTreeMap<String, String>> =
    std::sync::Mutex::new(BTreeMap::new());

pub fn named_dir(name: &str) -> Option<String> {
    if let Some(d) = NAMED_DIRS.lock().ok()?.get(name).cloned() {
        return Some(d);
    }
    let val = with_shell(|s| {
        let p = s.param(name)?;
        if p.level == 0
            && p.attrs & (INTEGER | FLOAT_E | FLOAT_F | crate::params::NAMEREF | UNSET) == 0
            && !p.is_array()
            && !p.is_assoc()
        {
            let v = p.text();
            v.starts_with('/').then_some(v)
        } else {
            None
        }
    })?;
    if let Ok(mut t) = NAMED_DIRS.lock() {
        t.insert(name.to_string(), val.clone());
    }
    Some(val)
}

/// zsh's $zsh_eval_context: how the code now running was reached
/// (toplevel, shfunc, eval, cmdsubst, trap, file, math, ...).
pub static EVAL_CONTEXT: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());

/// Runs `f` in eval context `ctx`.
pub fn in_eval_context<R>(ctx: &'static str, f: impl FnOnce() -> R) -> R {
    if let Ok(mut c) = EVAL_CONTEXT.lock() {
        c.push(ctx);
    }
    let r = f();
    if let Ok(mut c) = EVAL_CONTEXT.lock() {
        c.pop();
    }
    r
}

pub fn eval_context() -> Vec<String> {
    let mut v: Vec<String> = EVAL_CONTEXT
        .lock()
        .map(|c| c.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    if v.is_empty() {
        v.push("toplevel".to_string());
    }
    v
}

/// `typeset -F SECONDS`: $SECONDS counts fractions of a second.
pub static SECONDS_FLOAT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How many subshells deep this process is ($ZSH_SUBSHELL).
pub static SUBSHELL_LEVEL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Parameters computed on each read, as in zsh. A user assignment to
/// RANDOM reseeds it; the others ignore assignments.
fn dynamic_var(name: &str) -> Option<String> {
    use std::sync::atomic::Ordering;
    // A parameter of the same name shadows the special: a hidden (-h)
    // local, or one that replaced the not-yet-loaded SRANDOM.
    if DYNAMIC_NAMES.contains(&name) && with_shell(|s| s.param(name).is_some()) {
        return None;
    }
    match name {
        "SRANDOM" if crate::modules::feature_available("p:SRANDOM") => {
            crate::typeset::SRANDOM_LOADED.store(true, Ordering::Relaxed);
            let mut b = [0u8; 4];
            #[cfg(target_os = "linux")]
            unsafe {
                libc::getrandom(b.as_mut_ptr() as *mut libc::c_void, 4, 0);
            }
            #[cfg(not(target_os = "linux"))]
            unsafe {
                libc::arc4random_buf(b.as_mut_ptr() as *mut libc::c_void, 4);
            }
            Some(u32::from_ne_bytes(b).to_string())
        }
        "RANDOM" => {
            let mut x = RANDOM_STATE.load(Ordering::Relaxed);
            if x == 0 {
                let t = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(1);
                x = t ^ ((std::process::id() as u64) << 32) | 1;
            }
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            RANDOM_STATE.store(x, Ordering::Relaxed);
            Some(((x >> 33) % 32768).to_string())
        }
        "SECONDS" => {
            let secs = SECONDS_START
                .lock()
                .ok()
                .and_then(|g| *g)
                .map(|(t, base)| base + t.elapsed().as_secs_f64())
                .unwrap_or_else(|| {
                    START
                        .get_or_init(std::time::Instant::now)
                        .elapsed()
                        .as_secs_f64()
                });
            Some(if SECONDS_FLOAT.load(Ordering::Relaxed) {
                format!("{:.10}", secs)
            } else {
                (secs as i64).to_string()
            })
        }
        "EPOCHSECONDS" | "EPOCHREALTIME"
            if crate::modules::feature_available(&format!("p:{}", name)) =>
        {
            let d = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            Some(if name == "EPOCHSECONDS" {
                d.as_secs().to_string()
            } else {
                format!("{}.{:09}", d.as_secs(), d.subsec_nanos())
            })
        }
        "ZSH_SUBSHELL" => Some(SUBSHELL_LEVEL.load(Ordering::Relaxed).to_string()),
        "ZSH_EXEPATH" => std::env::current_exe()
            .ok()
            .map(|p| p.to_string_lossy().into_owned()),
        "LINENO" => Some(lineno().to_string()),
        // zsh's user and group ids.
        "UID" => Some(unsafe { libc::getuid() }.to_string()),
        "EUID" => Some(unsafe { libc::geteuid() }.to_string()),
        "GID" => Some(unsafe { libc::getgid() }.to_string()),
        "EGID" => Some(unsafe { libc::getegid() }.to_string()),
        "USERNAME" => unsafe {
            let pw = libc::getpwuid(libc::getuid());
            (!pw.is_null() && !(*pw).pw_name.is_null()).then(|| {
                std::ffi::CStr::from_ptr((*pw).pw_name)
                    .to_string_lossy()
                    .into_owned()
            })
        },
        "COLUMNS" => terminal_size().map(|(c, _)| c.to_string()),
        "LINES" => terminal_size().map(|(_, r)| r.to_string()),
        _ => None,
    }
}

/// Specials computed on read; a parameter of the same name hides them.
pub const DYNAMIC_NAMES: &[&str] = &[
    "RANDOM",
    "SRANDOM",
    "SECONDS",
    "EPOCHSECONDS",
    "EPOCHREALTIME",
    "ZSH_SUBSHELL",
    "ZSH_EXEPATH",
    "UID",
    "EUID",
    "GID",
    "EGID",
    "USERNAME",
    "LINENO",
    "COLUMNS",
    "LINES",
];

/// Reseeds `$RANDOM` after `RANDOM=n`.
pub fn seed_random(seed: u64) {
    RANDOM_STATE.store(
        seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// The parameter `name` refers to, following namerefs.
pub fn resolve_ref(name: &str) -> String {
    with_shell(|s| s.resolve_name(name))
}

/// The parameter name in a nameref target (without its subscript).
pub fn split_target_pub(t: &str) -> String {
    split_target(t).0
}

// Resolves a nameref target that carries a subscript: (name, sub).
fn split_target(t: &str) -> (String, Option<String>) {
    match t.find('[') {
        Some(k) if t.ends_with(']') => {
            (t[..k].to_string(), Some(t[k + 1..t.len() - 1].to_string()))
        }
        _ => (t.to_string(), None),
    }
}

/// True when `name` is, or leads through references to, a nameref that
/// refers to nothing yet (a placeholder).
pub fn placeholder_ref(name: &str) -> bool {
    with_shell(|s| {
        let t = s.resolve_name(name);
        s.param(&t)
            .is_some_and(|p| p.attrs & crate::params::NAMEREF != 0)
    })
}

pub fn var_get(name: &str) -> String {
    let r = with_shell(|s| s.resolve_ref_target(name));
    if r.missing_upper {
        return String::new();
    }
    let _swap = crate::shell::RefSwap::enter(name);
    if let Some(v) = dynamic_var(name) {
        return v;
    }
    let resolved = r.name;
    if resolved != name {
        let (base, sub) = split_target(&resolved);
        return match sub {
            Some(sub) => with_shell(|s| match s.param(&base).map(|p| p.value.clone()) {
                Some(Value::Assoc(m)) => m.get(&sub).cloned().unwrap_or_default(),
                Some(Value::Array(a)) => sub
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| a.get(i).cloned())
                    .unwrap_or_default(),
                Some(Value::Scalar(v)) => v,
                None => String::new(),
            }),
            None => var_get(&base),
        };
    }
    let shell = SHELL.shell.lock().unwrap();
    match name {
        "$" => return format!("{}", shell.shell_pid),
        "?" => return format!("{}", shell.last_status),
        "!" => return format!("{}", shell.last_bg_pid),
        "0" => return shell.argv0.clone(),
        "#" => return format!("{}", shell.pos_params.len()),
        // zsh's ARGC: the number of positional parameters.
        "ARGC"
            if shell.param("ARGC").is_none()
                && matches!(shell.emulation, crate::options::Emulation::Zsh) =>
        {
            return format!("{}", shell.pos_params.len())
        }
        "@" | "*" => return shell.pos_params.join(" "),
        "-" => return crate::options::flags_string(&shell),
        _ => {}
    }

    if name.len() > 1 && name.starts_with('-') {
        drop(shell);
        return var_get(&name[1..]);
    }

    // Numeric positional parameter lookup ($1, $2, ...).
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(idx) = name.parse::<usize>() {
            if idx == 0 {
                return shell.argv0.clone();
            }
            if let Some(p) = shell.pos_params.get(idx - 1) {
                return p.clone();
            }
            return String::new();
        }
    }
    shell
        .param(name)
        .filter(|p| p.attrs & crate::params::NAMEREF == 0)
        .map(|p| p.text())
        .unwrap_or_default()
}

pub fn var_exists(name: &str) -> bool {
    let r = with_shell(|s| s.resolve_ref_target(name));
    if r.missing_upper {
        return false;
    }
    let _swap = crate::shell::RefSwap::enter(name);
    let resolved = r.name;
    if resolved != name {
        return var_exists(&split_target(&resolved).0);
    }
    let shadowed = DYNAMIC_NAMES.contains(&name) && with_shell(|s| s.param(name).is_some());
    // zsh/datetime and zsh/random parameters go with their modules.
    let unloaded = matches!(name, "EPOCHSECONDS" | "EPOCHREALTIME" | "SRANDOM")
        && !crate::modules::feature_available(&format!("p:{}", name));
    if !shadowed
        && !unloaded
        && matches!(
            name,
            "RANDOM"
                | "SRANDOM"
                | "SECONDS"
                | "EPOCHSECONDS"
                | "EPOCHREALTIME"
                | "ZSH_SUBSHELL"
                | "ZSH_EXEPATH"
                | "UID"
                | "EUID"
                | "GID"
                | "EGID"
                | "USERNAME"
                | "LINENO"
        )
        || (matches!(name, "terminfo" | "termcap")
            && crate::modules::feature_available(&format!("p:{}", name)))
        || (name == "ARGC" && !crate::exec::sh_emulation_pub())
        || (matches!(name, "COLUMNS" | "LINES") && terminal_size().is_some())
    {
        return true;
    }
    let shell = SHELL.shell.lock().unwrap();
    if matches!(name, "?" | "$" | "!" | "0" | "#" | "@" | "*" | "-") {
        return true;
    }
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(idx) = name.parse::<usize>() {
            return idx == 0 || idx <= shell.pos_params.len();
        }
    }
    shell.param(name).is_some_and(|p| {
        p.is_set()
            && !(p.attrs & crate::params::NAMEREF != 0
                && p.attrs & crate::params::DECLARED != 0
                && p.text().is_empty())
    })
}

pub fn var_get_if_exists(name: &str) -> Option<i64> {
    let shell = SHELL.shell.lock().unwrap();
    shell.scalar(name).and_then(|v| v.parse::<i64>().ok())
}

/// The error for assigning to read-only `name`: a private parameter
/// hidden from this scope has its own (call without the shell locked).
pub fn readonly_message(name: &str) -> String {
    let attrs = with_shell(|s| s.param(name).map_or(0, |p| p.attrs));
    readonly_text(name, attrs)
}

fn readonly_text(name: &str, attrs: u32) -> String {
    if attrs & PRIVATE_HIDDEN != 0 {
        format!("{}: can't modify read-only parameter", name)
    } else {
        format!("read-only variable: {}", name)
    }
}

fn readonly_error(name: &str, attrs: u32) {
    shell_err(&readonly_text(name, attrs));
}

/// Converts a value for a numeric parameter at assignment: integers
/// and floats evaluate it arithmetically and keep the number. Other
/// attributes (case, justification, base) apply when the value is read.
/// Done without the shell lock held, since arithmetic reads variables.
fn convert_for(attrs: u32, value: &str) -> Result<String, String> {
    if attrs & INTEGER != 0 {
        return Ok(crate::arith::eval(value)?.to_string());
    }
    if attrs & (FLOAT_E | FLOAT_F) != 0 {
        let f = crate::arith::eval_num(value)?.as_float_pub();
        return Ok(format!("{:?}", f));
    }
    Ok(value.to_string())
}

pub fn format_int(n: i64, base: u32) -> String {
    if base == 0 || base == 10 || !(2..=36).contains(&base) {
        return n.to_string();
    }
    if crate::options::quick("cbases")
        && (base == 16 || (base == 8 && crate::options::quick("octalzeroes")))
    {
        let digits = if base == 16 {
            format!("0x{:X}", n.unsigned_abs())
        } else {
            format!("0{:o}", n.unsigned_abs())
        };
        return format!("{}{}", if n < 0 { "-" } else { "" }, digits);
    }
    let neg = n < 0;
    let mut u = n.unsigned_abs();
    let mut digits = Vec::new();
    loop {
        digits.push(
            std::char::from_digit((u % base as u64) as u32, base)
                .unwrap()
                .to_ascii_uppercase(),
        );
        u /= base as u64;
        if u == 0 {
            break;
        }
    }
    digits.reverse();
    format!(
        "{}{}#{}",
        if neg { "-" } else { "" },
        base,
        digits.into_iter().collect::<String>()
    )
}

pub fn format_float(f: f64, attrs: u32, digits: u32) -> String {
    if !f.is_finite() {
        return crate::arith::Num::F(f).to_string();
    }
    let d = if digits == 0 { 10 } else { digits as usize };
    if attrs & FLOAT_F != 0 {
        format!("{:.*}", d, f)
    } else {
        // C's %.*e: two-digit exponent with a sign.
        let s = format!("{:.*e}", d.saturating_sub(1), f);
        match s.split_once('e') {
            Some((m, e)) => {
                let (sign, digits) = match e.strip_prefix('-') {
                    Some(d) => ('-', d),
                    None => ('+', e),
                };
                format!("{}e{}{:0>2}", m, sign, digits)
            }
            None => s,
        }
    }
}

/// zsh's -L/-R/-Z display (getstrvalue in Src/params.c). `width` is the
/// field width; numeric parameters keep a leading `-`, `0x` or `base#`
/// before zero padding.
pub fn justify(v: &str, attrs: u32, width: usize) -> String {
    let chars: Vec<char> = v.chars().collect();
    let fwidth = if width > 0 { width } else { chars.len() };
    if attrs & LEFT != 0 {
        let mut t = 0;
        if attrs & RIGHT_Z != 0 {
            while chars.get(t) == Some(&'0') {
                t += 1;
            }
        } else {
            while chars.get(t).is_some_and(|c| *c == ' ' || *c == '\t') {
                t += 1;
            }
        }
        let mut out: String = chars[t..].iter().take(fwidth).collect();
        let n = out.chars().count();
        out.extend(std::iter::repeat_n(' ', fwidth - n));
        return out;
    }
    if chars.len() >= fwidth {
        return chars[chars.len() - fwidth..].iter().collect();
    }
    let numeric = attrs & (INTEGER | FLOAT_E | FLOAT_F) != 0;
    let mut zero = true;
    let mut pref = 0;
    if attrs & RIGHT_Z != 0 {
        let mut t = chars
            .iter()
            .take_while(|c| **c == ' ' || **c == '\t')
            .count();
        if numeric && chars.get(t) == Some(&'-') {
            t += 1;
        }
        if attrs & INTEGER != 0 {
            if crate::options::quick("cbases")
                && chars.get(t) == Some(&'0')
                && chars.get(t + 1) == Some(&'x')
            {
                t += 2;
            } else if let Some(k) = chars[t..].iter().position(|c| *c == '#') {
                t += k + 1;
            }
        }
        pref = t;
        if t >= chars.len() || (!numeric && !chars[t].is_ascii_digit()) {
            zero = false;
        }
    }
    let fill = if attrs & RIGHT_B != 0 || !zero {
        ' '
    } else {
        '0'
    };
    let mut out: String = chars[..pref].iter().collect();
    out.extend(std::iter::repeat_n(fill, fwidth - chars.len()));
    out.extend(&chars[pref..]);
    out
}

/// Points nameref `name` at `target`, rejecting bad names and loops.
pub fn set_ref_target(name: &str, target: &str) -> bool {
    let (lvl, upper) = with_shell(|s| {
        s.param(name).map_or((0, false), |p| {
            (p.level, p.attrs & crate::params::UPPER != 0)
        })
    });
    let upper_disallowed = upper
        && (target.starts_with(|c: char| c.is_ascii_digit()) || matches!(target, "argv" | "ARGC"));
    let valid = !target.is_empty()
        && !upper_disallowed
        && (matches!(target, "!" | "?" | "$" | "-")
            || crate::lexer::is_valid_name(target)
            || target.chars().all(|c| c.is_ascii_digit()));
    if !target.is_empty() && !valid {
        shell_err(&format!("invalid variable name: {}", target));
        crate::exec::abort_command(1);
        return false;
    }
    if !target.is_empty() {
        let local_shadow = with_shell(|s| s.param(target).is_some_and(|p| p.level > 0));
        if !local_shadow {
            crate::zparam::ensure(target);
            if crate::modules::has_autoparam(target) {
                if let Some(Err(msg)) = crate::modules::autoload_feature(&format!("p:{}", target)) {
                    if !msg.is_empty() {
                        shell_err(&msg);
                        crate::exec::abort_command(1);
                    }
                    return false;
                }
                crate::zparam::ensure(target);
            }
        }
    }
    let (bound, looping) = with_shell(|s| {
        let bound = if upper {
            None
        } else if target == name {
            s.bind_level(target, lvl, true).or(Some(0))
        } else {
            s.params.get(target).map(|p| p.level).or(Some(0))
        };
        let looping = !target.is_empty() && s.is_ref_cycle_from(name, lvl, target, bound, upper);
        (bound, looping)
    });
    if looping {
        shell_err(&format!("{}: invalid self reference", target));
        with_shell(|s| {
            if let Some(p) = s.param_mut(name) {
                p.value = Value::Scalar(String::new());
                p.attrs |= UNSET;
                p.attrs &= !crate::params::DECLARED;
                p.ref_level = None;
            }
        });
        return false;
    }
    if !upper && bound.is_some_and(|b| b > lvl) && crate::options::is_set("warnnestedvar") {
        shell_err(&format!(
            "reference {} in enclosing scope set to local variable {}",
            name, target
        ));
    }
    with_shell(|s| {
        if let Some(p) = s.param_mut(name) {
            p.value = Value::Scalar(target.to_string());
            p.attrs &= !(UNSET | crate::params::DECLARED);
            p.ref_level = bound;
        }
    });
    true
}

thread_local! {
    static REF_TYPE_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static REF_MISSING_UPPER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether an assignment through a reference just tried to change a hidden
/// parameter's type (and was undone).
pub fn take_ref_type_error() -> bool {
    REF_TYPE_ERROR.with(|e| e.replace(false))
}

/// Whether an assignment through an upper reference had no enclosing target.
pub fn take_ref_missing_upper() -> bool {
    REF_MISSING_UPPER.with(|e| e.replace(false))
}

/// While alive, the parameter a nameref was bound to sits in place of
/// the local that hides it, so reads and writes through the reference
/// reach it (zsh binds references to a scope, not just a name).
pub struct RefSwap {
    target: String,
    scope: Option<usize>,
    visible: Option<Param>,
    // The hidden parameter as it was: its type may not change.
    hidden: Option<Param>,
}

impl RefSwap {
    pub fn enter(name: &str) -> Option<RefSwap> {
        with_shell(|s| {
            let p = s.param(name)?;
            if p.attrs & crate::params::NAMEREF == 0 {
                return None;
            }
            let r = s.resolve_ref_target(name);
            let target = split_target(&r.name).0;
            if target.is_empty() {
                return None;
            }
            match r.scope? {
                Some(scope) => {
                    let hidden = s.local_scopes[scope].get(&target).cloned().flatten()?;
                    let visible = s.params.insert(target.clone(), hidden.clone());
                    Some(RefSwap {
                        target,
                        scope: Some(scope),
                        visible,
                        hidden: Some(hidden),
                    })
                }
                // Nothing at the bound scope: the reference sees nothing.
                None => {
                    let visible = s.params.remove(&target);
                    Some(RefSwap {
                        target,
                        scope: None,
                        visible,
                        hidden: None,
                    })
                }
            }
        })
    }
}

impl Drop for RefSwap {
    fn drop(&mut self) {
        let kind = |p: &Param| (p.is_array(), p.is_assoc());
        let changed = with_shell(|s| {
            let mut cur = s.params.remove(&self.target);
            // zsh refuses to change a hidden parameter's type through a
            // reference: put it back as it was.
            let changed = match (&self.hidden, &cur) {
                (Some(h), Some(c)) => kind(h) != kind(c),
                _ => false,
            };
            if changed {
                cur = self.hidden.clone();
            }
            if let Some(slot) = self.scope.and_then(|j| s.local_scopes.get_mut(j)) {
                slot.insert(self.target.clone(), cur);
            }
            if let Some(v) = self.visible.take() {
                s.params.insert(self.target.clone(), v);
            }
            changed
        });
        if changed {
            shell_err(&format!(
                "can't change type of hidden variable: {}",
                self.target
            ));
            REF_TYPE_ERROR.with(|e| e.set(true));
            crate::exec::abort_command(1);
        }
    }
}

// Assignments that never warn (typeset, temporary assignments, element
// updates), and whether the current one comes from arithmetic.
static WARN_OFF: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub static ARITH_ASSIGN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Runs `f` with WARN_CREATE_GLOBAL / WARN_NESTED_VAR warnings off.
pub fn without_assign_warnings<R>(f: impl FnOnce() -> R) -> R {
    use std::sync::atomic::Ordering;
    WARN_OFF.fetch_add(1, Ordering::SeqCst);
    let r = f();
    WARN_OFF.fetch_sub(1, Ordering::SeqCst);
    r
}

/// Traced functions: `functions -t` (trace it and what it calls) or
/// `-T` (it and anonymous functions inside), as (name, is_T).
pub static TRACED_FUNCS: std::sync::RwLock<Vec<(String, bool)>> =
    std::sync::RwLock::new(Vec::new());

pub fn trace_kind(name: &str) -> Option<bool> {
    TRACED_FUNCS
        .read()
        .ok()?
        .iter()
        .rev()
        .find(|(n, _)| n == name)
        .map(|(_, t)| *t)
}

pub fn set_traced(name: &str, kind: Option<bool>) {
    if let Ok(mut t) = TRACED_FUNCS.write() {
        match kind {
            // -T takes precedence over -t.
            Some(false) if t.iter().any(|(n, k)| n == name && *k) => {}
            Some(k) => {
                t.retain(|(n, _)| n != name);
                t.push((name.to_string(), k));
            }
            None => t.retain(|(n, _)| n != name),
        }
    }
}

/// Functions marked with `functions -W` (WARN_NESTED_VAR for them).
pub static WARN_NESTED_FUNCS: std::sync::RwLock<Vec<String>> = std::sync::RwLock::new(Vec::new());

// zsh's WARN_CREATE_GLOBAL and WARN_NESTED_VAR: an assignment in a
// function that creates a global, or changes a parameter of an
// enclosing scope. `kind` is the new value's type.
fn assign_warning(name: &str, kind: &str) {
    use std::sync::atomic::Ordering;
    if WARN_OFF.load(Ordering::SeqCst) > 0 {
        return;
    }
    let fname = with_err_ctx(|e| e.stack.last().map(|c| c.0.clone()));
    let Some(fname) = fname else {
        return;
    };
    let attr = WARN_NESTED_FUNCS
        .read()
        .map(|w| w.contains(&fname))
        .unwrap_or(false);
    let numeric = ARITH_ASSIGN.load(Ordering::SeqCst);
    let msg = with_shell(|s| {
        if s.func_depth == 0 {
            return None;
        }
        let depth = s.local_scopes.len();
        if s.param(name).is_some_and(|p| p.level >= depth) {
            return None;
        }
        let p = s.param(name).filter(|p| p.attrs & UNSET == 0);
        let kind = match p {
            Some(p) if numeric || p.attrs & crate::params::NUMERIC != 0 && kind == "scalar" => {
                "numeric"
            }
            _ if numeric => "numeric",
            _ => kind,
        };
        match p {
            None if crate::options::is_set_in(s, "warncreateglobal") => Some(format!(
                "{} parameter {} created globally in function {}",
                kind, name, fname
            )),
            Some(p)
                if p.level < depth
                    && p.attrs & crate::params::SPECIAL == 0
                    && (attr || crate::options::is_set_in(s, "warnnestedvar")) =>
            {
                Some(format!(
                    "{} parameter {} set in enclosing scope in function {}",
                    kind, name, fname
                ))
            }
            _ => None,
        }
    });
    if let Some(m) = msg {
        shell_err(&m);
    }
}

pub fn var_set(name: &str, value: &str, export: bool) -> bool {
    if name == "status"
        && !crate::exec::sh_emulation_pub()
        && with_shell(|s| s.param("status").is_none())
    {
        shell_err("read-only variable: status");
        return false;
    }
    // zsh reserves the .zsh namespace for its own read-only parameters.
    if name.starts_with(".zsh.") {
        shell_err(&format!("{}: can't modify read-only parameter", name));
        return false;
    }
    let was_placeholder = with_shell(|s| {
        s.param(name).is_some_and(|p| {
            p.attrs & crate::params::NAMEREF != 0
                && matches!(&p.value, Value::Scalar(t) if t.is_empty())
        })
    });
    let ok = var_set_inner(name, value, export);
    // AUTO_NAME_DIRS & named directory updates for global scalar parameters.
    if ok && !was_placeholder {
        let _swap = crate::shell::RefSwap::enter(name);
        let target = split_target(&resolve_ref(name)).0;
        if crate::lexer::is_valid_name(&target)
            && with_shell(|s| {
                s.param(&target).is_some_and(|p| {
                    p.level == 0
                        && p.attrs & (INTEGER | FLOAT_E | FLOAT_F | crate::params::NAMEREF | UNSET)
                            == 0
                        && !p.is_array()
                        && !p.is_assoc()
                })
            })
        {
            if let Ok(mut t) = NAMED_DIRS.lock() {
                if value.starts_with('/') {
                    if t.contains_key(&target) || crate::options::is_set("autonamedirs") {
                        t.insert(target, value.to_string());
                    }
                } else {
                    t.remove(&target);
                }
            }
        }
    }
    ok
}

fn var_set_inner(name: &str, value: &str, export: bool) -> bool {
    // A nameref without a target yet: the value becomes its target.
    let placeholder = with_shell(|s| {
        s.param(name).is_some_and(|p| {
            p.attrs & crate::params::NAMEREF != 0
                && matches!(&p.value, Value::Scalar(t) if t.is_empty())
        })
    });
    if placeholder {
        return set_ref_target(name, value);
    }
    if !with_shell(|s| s.param(name).is_some_and(|p| p.level > 0))
        && crate::modules::has_autoparam(name)
    {
        let _ = crate::modules::autoload_feature(&format!("p:{}", name));
    }
    let r = with_shell(|s| s.resolve_ref_target(name));
    if r.missing_upper {
        REF_MISSING_UPPER.with(|e| e.set(true));
        return false;
    }
    let _swap = crate::shell::RefSwap::enter(name);
    let resolved = r.name;
    if resolved != name {
        let (base, sub) = split_target(&resolved);
        if sub.is_none() {
            let stashed_ref = with_shell(|s| {
                let p = s.param(&base)?;
                if p.attrs & (READONLY | PRIVATE_HIDDEN) == (READONLY | PRIVATE_HIDDEN) {
                    return s
                        .private_stash
                        .iter()
                        .rev()
                        .find(|(_, k, _)| k == &base)
                        .and_then(|(_, _, sp)| {
                            (sp.attrs & crate::params::NAMEREF != 0)
                                .then_some(sp.attrs & UNSET != 0)
                        });
                }
                None
            });
            if let Some(was_unset) = stashed_ref {
                if was_unset {
                    readonly_error(&base, 0);
                } else {
                    readonly_error(name, PRIVATE_HIDDEN);
                }
                return false;
            }
        }
        return match sub {
            Some(sub) => array_set_subscript(&base, &sub, value.to_string(), false),
            None => var_set(&base, value, export),
        };
    }
    // `0=x`, `1=x` (and `for 1 in ...`) set positional parameters / $0.
    if let Some(n) = name
        .parse::<usize>()
        .ok()
        .filter(|_| !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()))
    {
        with_shell(|s| {
            if n == 0 {
                s.argv0 = value.to_string();
            } else {
                if s.pos_params.len() < n {
                    s.pos_params.resize(n, String::new());
                }
                s.pos_params[n - 1] = value.to_string();
            }
        });
        return true;
    }
    if name == "RANDOM" {
        seed_random(value.trim().parse().unwrap_or(0));
    }
    if name == "SECONDS"
        && with_shell(|s| s.param("SECONDS").is_none())
        && !crate::exec::sh_emulation_pub()
    {
        let v = match crate::arith::eval_num(value) {
            Ok(n) => n.as_float_pub(),
            Err(e) => {
                shell_err(&format!("SECONDS: {}", e));
                return false;
            }
        };
        if let Ok(mut g) = SECONDS_START.lock() {
            *g = Some((std::time::Instant::now(), v));
        }
        return true;
    }
    // zsh: assigning UID, EUID, GID or EGID changes the process's ids.
    if matches!(name, "UID" | "EUID" | "GID" | "EGID")
        && with_shell(|s| s.param(name).is_none())
        && !crate::exec::sh_emulation_pub()
    {
        let id = crate::arith::eval(value).unwrap_or(-1) as libc::uid_t;
        let (r, what) = unsafe {
            match name {
                "UID" => (libc::setuid(id), "user ID"),
                "EUID" => (libc::seteuid(id), "effective user ID"),
                "GID" => (libc::setgid(id), "group ID"),
                _ => (libc::setegid(id), "effective group ID"),
            }
        };
        if r != 0 {
            let e =
                crate::exec::strerror(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
            let mut c = e.chars();
            let e = c
                .next()
                .map(|f| f.to_lowercase().chain(c).collect::<String>())
                .unwrap_or_default();
            shell_err(&format!("failed to change {}: {}", what, e));
            return false;
        }
        return true;
    }
    assign_warning(name, "scalar");
    // zsh's integer specials that start unset take their type when set.
    let fresh = if name == "ZLE_RPROMPT_INDENT" {
        INTEGER
    } else {
        0
    };
    let attrs = with_shell(|s| s.param(name).map(|p| p.attrs).unwrap_or(fresh));
    if attrs & READONLY != 0 {
        readonly_error(name, attrs);
        return false;
    }
    let value = match convert_for(attrs, value) {
        Ok(v) => v,
        Err(e) => {
            shell_err(&format!("{}: {}", name, e));
            return false;
        }
    };
    let mut shell = SHELL.shell.lock().unwrap();
    let allexport = shell.opts & OPT_ALLEXPORT != 0;
    let p = shell.params.entry(name.to_string()).or_insert_with(|| {
        let mut p = Param::scalar("");
        p.attrs = fresh;
        p
    });
    if export || allexport {
        p.attrs |= EXPORT;
    }
    p.attrs &= !UNSET;
    if p.width == 0 && p.attrs & (LEFT | RIGHT_B | RIGHT_Z) != 0 {
        p.width = value.chars().count();
    }
    p.value = Value::Scalar(value);
    shell.sync_tie(name);
    drop(shell);
    if name == "CURSOR_FORM" {
        crate::zle::termctl::cursor_form_param_changed();
    }
    if matches!(name, "LC_ALL" | "LC_CTYPE" | "LANG") {
        sync_lc_ctype();
    }
    true
}

/// Stores items into array `name`, honoring -U and readonly.
pub fn array_set(name: &str, mut items: Vec<String>) -> bool {
    let _swap = crate::shell::RefSwap::enter(name);
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    if name == "region_highlight" {
        items = crate::zle::refresh::normalize_region_highlight(items);
    }
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()) {
        shell_err(&format!(
            "{}: attempt to assign array value to non-array",
            name
        ));
        return false;
    }
    let kind = if with_shell(|s| s.assoc(name).is_some()) {
        "association"
    } else {
        "array"
    };
    assign_warning(name, kind);
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    // A value makes a declared-but-unset parameter (TYPESET_TO_UNSET) set.
    if let Some(p) = shell.param_mut(name) {
        p.attrs &= !UNSET;
    }
    if shell.assoc(name).is_some() {
        if !items.len().is_multiple_of(2) {
            drop(shell);
            shell_err("bad set of key/value pairs for associative array");
            return false;
        }
        let lvl = shell.param(name).map(|p| p.level).unwrap_or(0);
        let is_special_opts =
            name == "options" && shell.param("options").is_some_and(|p| p.level == 0);
        let assoc = assoc_from_items(items);
        let copy = assoc.clone();
        shell.store(name, Value::Assoc(assoc));
        drop(shell);
        if is_special_opts {
            for (k, v) in &copy {
                if v == "on" || v == "off" {
                    crate::options::set(k, v == "on");
                }
            }
        }
        crate::zgdbm::on_replace_all(name, lvl, &copy);
        return true;
    }
    // A scalar given an array value becomes a plain array: its scalar
    // attributes (-i, -Z3, -u, -x...) go.
    if let Some(p) = shell.param_mut(name) {
        if matches!(p.value, Value::Scalar(_)) && p.tie.is_none() {
            p.attrs &= crate::params::SPECIAL | crate::params::HIDE | crate::params::DECLARED;
            p.width = 0;
            p.base = 0;
        }
    }
    let items = maybe_unique_items(&shell, name, items);
    shell.store(name, Value::Array(items));
    true
}

/// An item of an array assignment: a plain value or zsh's `[key]=value`
/// (the bool: `[key]+=value`).
#[derive(Clone, Debug)]
pub enum ArrItem {
    Plain(String),
    Key(String, String, bool),
}

pub fn plain_items(items: &[ArrItem]) -> Vec<String> {
    items
        .iter()
        .map(|i| match i {
            ArrItem::Plain(v) => v.clone(),
            ArrItem::Key(k, v, _) => format!("[{}]={}", k, v),
        })
        .collect()
}

/// `name=(items)` / `name+=(items)` with `[key]=value` items: for an
/// associative array each sets a key (plain items pair up); for a normal
/// array `[n]=v` sets element n and later plain items follow it.
pub fn array_assign_items(name: &str, items: Vec<ArrItem>, append: bool) -> bool {
    let _swap = crate::shell::RefSwap::enter(name);
    if placeholder_ref(name) {
        shell_err(&format!("{}: can't change type of a named reference", name));
        return false;
    }
    // A private parameter keeps its type.
    let resolved = resolve_ref(name);
    if !resolved.is_empty() && resolved.chars().all(|c| c.is_ascii_digit()) {
        shell_err(&format!(
            "{}: attempt to assign array value to non-array",
            resolved
        ));
        return false;
    }
    if with_shell(|s| {
        s.param(&resolved).is_some_and(|p| {
            p.attrs & crate::params::PRIVATE != 0 && !p.is_array() && !p.is_assoc()
        })
    }) {
        shell_err(&format!(
            "{}: attempt to assign array value to non-array",
            name
        ));
        return false;
    }
    // zsh/parameter's $functions: each name and body defines a function.
    if name == "functions" && crate::zparam::functions_special() {
        let plain = plain_items(&items);
        if !plain.len().is_multiple_of(2) {
            shell_err("bad set of key/value pairs for associative array");
            return false;
        }
        return plain
            .chunks(2)
            .all(|kv| crate::zparam::set_function(&kv[0], &kv[1]));
    }
    if !items.iter().any(|i| matches!(i, ArrItem::Key(..))) {
        let plain = plain_items(&items);
        // zsh: $argv is the positional parameters.
        if name == "argv"
            && !crate::exec::sh_emulation_pub()
            && with_shell(|s| s.param("argv").is_none())
        {
            with_shell(|s| {
                if append {
                    s.pos_params.extend(plain);
                } else {
                    s.pos_params = plain;
                }
            });
            return true;
        }
        return if append {
            array_append(name, plain)
        } else {
            array_set(name, plain)
        };
    }
    let target = split_target(&resolve_ref(name)).0;
    let is_assoc = with_shell(|s| s.assoc(&target).is_some());
    if is_assoc {
        if items.iter().any(|i| matches!(i, ArrItem::Plain(_))) {
            shell_err("bad [key]=value syntax for associative array");
            return false;
        }
        let mut map = if append {
            with_shell(|s| s.assoc(&target).cloned()).unwrap_or_default()
        } else {
            BTreeMap::new()
        };
        for i in items {
            if let ArrItem::Key(k, v, add) = i {
                let v = if add {
                    format!("{}{}", map.get(&k).cloned().unwrap_or_default(), v)
                } else {
                    v
                };
                map.insert(k, v);
            }
        }
        with_shell(|s| s.store(&target, Value::Assoc(map)));
        return true;
    }
    let mut arr = if append {
        array_get(&target).unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut idx = arr.len();
    for i in items {
        match i {
            ArrItem::Plain(v) => {
                if arr.len() <= idx {
                    arr.resize(idx + 1, String::new());
                }
                arr[idx] = v;
                idx += 1;
            }
            ArrItem::Key(k, v, add) => {
                let mut n = crate::arith::eval(&k).unwrap_or(0);
                if n >= 0 && crate::options::quick("ksharrays") {
                    n += 1;
                }
                if n < 1 {
                    shell_err(&format!("{}: assignment to invalid subscript range", name));
                    return false;
                }
                let k = (n - 1) as usize;
                if arr.len() <= k {
                    arr.resize(k + 1, String::new());
                }
                if add {
                    arr[k].push_str(&v);
                } else {
                    arr[k] = v;
                }
                idx = k + 1;
            }
        }
    }
    array_set(&target, arr)
}

/// `name[a,b]=(items)`: replaces elements a through b (1-based,
/// negatives from the end) with `items`. `name[n]=(items)` replaces one.
pub fn array_splice(name: &str, sub: &str, items: Vec<String>) -> bool {
    if let Some(ok) = on_argv(name, || array_splice(name, sub, items.clone())) {
        return ok;
    }
    let _swap = crate::shell::RefSwap::enter(name);
    without_assign_warnings(|| array_splice_inner(name, sub, items))
}

fn array_splice_inner(name: &str, sub: &str, items: Vec<String>) -> bool {
    let target = split_target(&resolve_ref(name)).0;
    let (a, b) = match sub.split_once(',') {
        Some((a, b)) => (crate::arith::eval(a), crate::arith::eval(b)),
        None => (crate::arith::eval(sub), crate::arith::eval(sub)),
    };
    let (Ok(mut a), Ok(mut b)) = (a, b) else {
        shell_err(&format!("{}: bad subscript", name));
        return false;
    };
    if crate::options::quick("ksharrays") {
        a += i64::from(a >= 0);
        b += i64::from(b >= 0);
    }
    let scalar = with_shell(|s| s.scalar(&target).map(|v| v.to_string()));
    let mut arr: Vec<String> = match &scalar {
        Some(v) => v.chars().map(|c| c.to_string()).collect(),
        None => array_get(&target).unwrap_or_default(),
    };
    let len = arr.len() as i64;
    let norm = |x: i64| if x < 0 { (len + x + 1).max(0) } else { x };
    let start = (norm(a).max(1) - 1) as usize;
    let end = (norm(b).max(start as i64)) as usize;
    if arr.len() < start {
        arr.resize(start, String::new());
    }
    let end = end.min(arr.len());
    arr.splice(start..end, items);
    match scalar {
        Some(_) => var_set(&target, &arr.concat(), false),
        None => array_set(&target, arr),
    }
}

pub fn array_append(name: &str, mut items: Vec<String>) -> bool {
    let _swap = crate::shell::RefSwap::enter(name);
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    if name == "region_highlight" {
        items = crate::zle::refresh::normalize_region_highlight(items);
    }
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    if let Some(existing) = shell.assoc(name).cloned() {
        let lvl = shell.param(name).map(|p| p.level).unwrap_or(0);
        let added = assoc_from_items(items);
        let mut assoc = existing;
        assoc.extend(added.clone());
        shell.store(name, Value::Assoc(assoc));
        drop(shell);
        crate::zgdbm::on_append_all(name, lvl, &added);
        return true;
    }
    let mut combined = match shell.param(name).map(|p| &p.value) {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::Scalar(s)) => vec![s.clone()],
        _ => Vec::new(),
    };
    combined.extend(items);
    let combined = maybe_unique_items(&shell, name, combined);
    shell.store(name, Value::Array(combined));
    true
}

// zsh: $argv is the positional parameters. Edits to it as an array run
// `f` on a parameter holding them, then move the result back. None when
// `name` is not that $argv.
fn on_argv(name: &str, f: impl FnOnce() -> bool) -> Option<bool> {
    if name != "argv"
        || crate::exec::sh_emulation_pub()
        || with_shell(|s| s.param("argv").is_some())
    {
        return None;
    }
    let pos = with_shell(|s| s.pos_params.clone());
    with_shell(|s| s.store("argv", Value::Array(pos)));
    let ok = f();
    with_shell(|s| {
        if let Some(Value::Array(a)) = s.remove_param("argv").map(|p| p.value) {
            s.pos_params = a;
        }
    });
    Some(ok)
}

pub fn array_set_subscript(name: &str, subscript: &str, value: String, append: bool) -> bool {
    if let Some(ok) = on_argv(name, || {
        array_set_subscript(name, subscript, value.clone(), append)
    }) {
        return ok;
    }
    if name == "functions" && crate::zparam::functions_special() {
        return crate::zparam::set_function(subscript, &value);
    }
    let _swap = crate::shell::RefSwap::enter(name);
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    if shell.assoc(name).is_some() {
        let lvl = shell.param(name).map(|p| p.level).unwrap_or(0);
        let is_special_opts =
            name == "options" && shell.param("options").is_some_and(|p| p.level == 0);
        let new_value = {
            let assoc = shell.assoc_mut(name).unwrap();
            let nv = if append {
                let mut current = assoc.get(subscript).cloned().unwrap_or_default();
                current.push_str(&value);
                current
            } else {
                value
            };
            assoc.insert(subscript.to_string(), nv.clone());
            nv
        };
        drop(shell);
        if is_special_opts && (new_value == "on" || new_value == "off") {
            crate::options::set(subscript, new_value == "on");
        }
        crate::zgdbm::on_set_key(name, lvl, subscript, &new_value);
        return true;
    }

    let Ok(mut index) = subscript.parse::<isize>() else {
        return false;
    };
    if index >= 0 && crate::options::quick("ksharrays") {
        index += 1;
    }
    if index == 0 && crate::options::quick("kshzerosubscript") {
        index = 1;
    }
    if index == 0 {
        drop(shell);
        shell_err(&format!("{}: assignment to invalid subscript range", name));
        return false;
    }
    let mut items = shell.array(name).cloned().unwrap_or_default();
    let idx = if index > 0 {
        (index - 1) as usize
    } else {
        let len = items.len() as isize;
        let resolved = len + index;
        if resolved < 0 {
            // Before the first element: the value goes in front.
            items.insert(0, value);
            let items = maybe_unique_items(&shell, name, items);
            shell.store(name, Value::Array(items));
            return true;
        }
        resolved as usize
    };
    if items.len() <= idx {
        items.resize(idx + 1, String::new());
    }
    if append {
        items[idx].push_str(&value);
    } else {
        items[idx] = value;
    }
    let items = maybe_unique_items(&shell, name, items);
    shell.store(name, Value::Array(items));
    true
}

pub fn array_make_unique(name: &str) -> bool {
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    let p = shell
        .params
        .entry(name.to_string())
        .or_insert_with(|| Param::with(Value::Array(Vec::new())));
    p.attrs |= UNIQUE;
    if let Some(items) = shell.array(name).cloned() {
        shell.store(name, Value::Array(unique_items(items)));
    }
    true
}

pub fn array_declare(name: &str) -> bool {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    let items = match shell.param(name).map(|p| &p.value) {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::Scalar(s)) if !s.is_empty() => vec![s.clone()],
        _ => Vec::new(),
    };
    shell.store(name, Value::Array(items));
    true
}

pub fn assoc_declare(name: &str) -> bool {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    if shell.assoc(name).is_none() {
        shell.store(name, Value::Assoc(BTreeMap::new()));
    }
    true
}

pub fn assoc_set(name: &str, key: &str, value: &str) {
    let _swap = crate::shell::RefSwap::enter(name);
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let mut shell = SHELL.shell.lock().unwrap();
    shell
        .assoc_entry(name)
        .insert(key.to_string(), value.to_string());
}

pub fn assoc_get(name: &str, key: &str) -> Option<String> {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let param_lvl = {
        let shell = SHELL.shell.lock().unwrap();
        shell.param(name).filter(|p| p.is_assoc()).map(|p| p.level)
    };
    if let Some(lvl) = param_lvl {
        crate::zgdbm::on_read_refresh(name, lvl);
    } else {
        if name == "terminfo" && crate::modules::feature_available("p:terminfo") {
            return crate::zle::refresh::refresh_term::terminfo_lookup(key);
        }
        if name == "termcap" && crate::modules::feature_available("p:termcap") {
            return crate::zle::refresh::refresh_term::termcap_lookup(key);
        }
    }
    let shell = SHELL.shell.lock().unwrap();
    shell.assoc(name).and_then(|m| m.get(key).cloned())
}

pub fn assoc_values(name: &str) -> Option<Vec<String>> {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let has_param = {
        let shell = SHELL.shell.lock().unwrap();
        shell.assoc(name).is_some()
    };
    if !has_param {
        if name == "terminfo" && crate::modules::feature_available("p:terminfo") {
            return Some(
                crate::zle::refresh::refresh_term::terminfo_entries()
                    .into_iter()
                    .map(|(_, v)| v)
                    .collect(),
            );
        }
        if name == "termcap" && crate::modules::feature_available("p:termcap") {
            return Some(
                crate::zle::refresh::refresh_term::termcap_entries()
                    .into_iter()
                    .map(|(_, v)| v)
                    .collect(),
            );
        }
    }
    let keys = assoc_keys(name)?;
    let shell = SHELL.shell.lock().unwrap();
    let m = shell.assoc(name)?;
    Some(
        keys.into_iter()
            .filter_map(|k| m.get(&k).cloned())
            .collect(),
    )
}

pub fn assoc_keys(name: &str) -> Option<Vec<String>> {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let param_lvl = {
        let shell = SHELL.shell.lock().unwrap();
        shell.param(name).filter(|p| p.is_assoc()).map(|p| p.level)
    };
    if let Some(lvl) = param_lvl {
        crate::zgdbm::on_read_refresh(name, lvl);
    } else {
        if name == "terminfo" && crate::modules::feature_available("p:terminfo") {
            return Some(
                crate::zle::refresh::refresh_term::terminfo_entries()
                    .into_iter()
                    .map(|(k, _)| k)
                    .collect(),
            );
        }
        if name == "termcap" && crate::modules::feature_available("p:termcap") {
            return Some(
                crate::zle::refresh::refresh_term::termcap_entries()
                    .into_iter()
                    .map(|(k, _)| k)
                    .collect(),
            );
        }
    }
    let shell = SHELL.shell.lock().unwrap();
    shell.assoc(name).map(|m| {
        let mut keys: Vec<String> = m.keys().cloned().collect();
        if name == "nameddirs"
            && shell
                .param("nameddirs")
                .is_some_and(|p| p.attrs & crate::params::SPECIAL != 0 && p.level == 0)
        {
            // zsh's nameddirtab has 201 buckets (hashnameddir.c:createnameddirtable);
            // ${(k)nameddirs} and ${(kv)nameddirs} iterate in bucket order.
            keys.sort_by_key(|k| {
                let h = k
                    .bytes()
                    .fold(0u32, |h, c| h.wrapping_add(h << 5).wrapping_add(c as u32));
                (h % 201, k.clone())
            });
        }
        keys
    })
}

fn prepare_array_mutation(shell: &mut Shell, name: &str) -> bool {
    if let Some(attrs) = shell
        .param(name)
        .map(|p| p.attrs)
        .filter(|a| a & READONLY != 0)
    {
        readonly_error(name, attrs);
        return false;
    }
    true
}

fn maybe_unique_items(shell: &Shell, name: &str, items: Vec<String>) -> Vec<String> {
    if shell.param(name).is_some_and(|p| p.attrs & UNIQUE != 0) {
        unique_items(items)
    } else {
        items
    }
}

fn unique_items(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}

fn assoc_from_items(items: Vec<String>) -> BTreeMap<String, String> {
    let mut assoc = BTreeMap::new();
    let mut iter = items.into_iter();
    while let Some(key) = iter.next() {
        let value = iter.next().unwrap_or_default();
        assoc.insert(key, value);
    }
    assoc
}

pub fn array_get(name: &str) -> Option<Vec<String>> {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    SHELL.shell.lock().unwrap().array(name).cloned()
}

pub fn is_array(name: &str) -> bool {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let shell = SHELL.shell.lock().unwrap();
    shell
        .param(name)
        .is_some_and(|p| p.is_array() || p.is_assoc())
}

pub fn push_scope() {
    SHELL.shell.lock().unwrap().push_scope();
}

pub fn pop_scope() {
    let popped_level = SHELL.shell.lock().unwrap().local_scopes.len();
    crate::zgdbm::on_scope_pop(popped_level);
    let (changed, loop_errs) = SHELL.shell.lock().unwrap().pop_scope();
    if changed {
        crate::zle::termctl::cursor_form_param_changed();
    }
    sync_lc_ctype();
    if let Some(target) = loop_errs.first() {
        shell_err(&format!("{}: invalid self reference", target));
        crate::exec::abort_command(1);
    }
}

/// Enters a function call's scope (which hides private parameters).
pub fn push_func_scope() {
    SHELL.shell.lock().unwrap().push_scope_for(true);
}

pub fn in_function_scope() -> bool {
    !SHELL.shell.lock().unwrap().local_scopes.is_empty()
}

/// Saves `name` in the innermost scope so it is restored when the
/// function returns. False outside a function. Only the first call per
/// scope records the prior state.
pub fn declare_local(name: &str) -> bool {
    SHELL.shell.lock().unwrap().make_local(name)
}

pub fn shorten_path(p: &str) -> String {
    let home = var_get("HOME");
    let home = home.trim_end_matches('/');
    if !home.is_empty() {
        if let Some(rest) = p.strip_prefix(home) {
            if rest.is_empty() || rest.starts_with('/') {
                return format!("~{}", rest);
            }
        }
    }
    p.to_string()
}

pub fn build_prompt() -> String {
    let prompt_str = {
        let p = var_get("PROMPT");
        if !p.is_empty() {
            p
        } else {
            var_get("PS1")
        }
    };
    // A PS1 set to nothing means no prompt; only an unset one gets the
    // default.
    if prompt_str.is_empty() && (var_exists("PS1") || var_exists("PROMPT")) {
        return String::new();
    }
    if prompt_str.is_empty() {
        let user = var_get("USER");
        let user = if user.is_empty() { "meow" } else { &user };
        let pwd = var_get("PWD");
        let pwd = if pwd.is_empty() { "?" } else { &pwd };
        let short_pwd = shorten_path(pwd);
        return format!("\x1b[32m{}\x1b[0m \x1b[34m{}\x1b[0m 𓃠  ", user, short_pwd);
    }
    render_prompt(&prompt_str)
}

/// Parameter, command and arithmetic expansion, then zsh `%` escapes, the
/// order zsh uses with PROMPT_SUBST. Text produced by a substitution may
/// itself contain `%` escapes, as starship's output does.
pub fn render_prompt(s: &str) -> String {
    // PROMPT_SUBST: parameter, command and arithmetic substitution.
    let mut t = if crate::options::is_set("promptsubst") {
        crate::expand::expand_prompt_string(s).unwrap_or_else(|_| s.to_string())
    } else {
        s.to_string()
    };
    // PROMPT_BANG: `!` is the history event number, `!!` a `!`.
    if crate::options::is_set("promptbang") {
        let n = history_count();
        let mut out = String::new();
        let mut chars = t.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '!' {
                out.push(c);
            } else if chars.peek() == Some(&'!') {
                chars.next();
                out.push('!');
            } else {
                out.push_str(&n);
            }
        }
        t = out;
    }
    if crate::options::is_set("promptpercent") {
        t = expand_prompt(&t);
    }
    t
}

/// zsh prompt `%` escapes (prompt.c's putpromptchar): paths, times,
/// conditionals `%(x.true.false)`, truncation `%N<..<` / `%N>..>`,
/// colours and attributes.
pub fn expand_prompt(s: &str) -> String {
    let mut p = Prompt {
        f: s.chars().collect(),
        pos: 0,
        trunc: false,
        active_attrs: 0,
        unknown_attrs: 0b1_1111,
    };
    let mut out = String::new();
    while p.pos < p.f.len() {
        p.put(&mut out, true, None);
        // A stray end character at the top level is plain text.
        if p.pos < p.f.len() {
            out.push(p.f[p.pos]);
            p.pos += 1;
        }
    }
    out
}

struct Prompt {
    f: Vec<char>,
    pos: usize,
    // Inside a truncation region: the next one ends it.
    trunc: bool,
    // Tracked text attribute bits (1=bold, 2=underline, 4=standout, 8=fg, 16=bg)
    // and which bits are still unknown since the start of prompt expansion.
    active_attrs: u8,
    unknown_attrs: u8,
}

// The longest named directory (HOME as ``) that starts `p`, for `~`.
fn find_named_dir(p: &str) -> Option<(String, String)> {
    let mut best: Option<(String, String)> = None;
    let mut consider = |name: String, dir: String| {
        if dir.is_empty() || dir == "/" {
            return;
        }
        let hit = p == dir || p.strip_prefix(&dir).is_some_and(|r| r.starts_with('/'));
        if hit && best.as_ref().is_none_or(|(_, d)| dir.len() > d.len()) {
            best = Some((name, dir));
        }
    };
    consider(String::new(), var_get("HOME"));
    if let Ok(t) = NAMED_DIRS.lock() {
        for (k, v) in t.iter() {
            consider(k.clone(), v.clone());
        }
    }
    // zsh_directory_name d: (name, length of the path it names).
    if let Some(r) = crate::expand::dynamic_dir('d', p) {
        if let (Some(name), Some(len)) = (r.first(), r.get(1).and_then(|l| l.parse::<usize>().ok()))
        {
            if let Some(dir) = p.get(..len) {
                if best.as_ref().is_none_or(|(_, d)| len > d.len()) {
                    best = Some((format!("[{}]", name), dir.to_string()));
                }
            }
        }
    }
    best
}

// zsh's promptpath: `p` with ~names if `tilde`, cut to the last `n`
// components (or, negative, the first ones).
fn prompt_path(p: &str, n: i64, tilde: bool) -> String {
    let modp = match find_named_dir(p).filter(|_| tilde) {
        Some((name, dir)) => format!("~{}{}", name, &p[dir.len()..]),
        None => p.to_string(),
    };
    if n == 0 {
        return modp;
    }
    let c: Vec<char> = modp.chars().collect();
    if n > 0 {
        let mut left = n;
        let mut k = c.len();
        while k > 0 {
            k -= 1;
            if c[k] == '/' {
                left -= 1;
                if left == 0 {
                    k += 1;
                    break;
                }
            }
        }
        if k < c.len() && c[k] == '/' && k + 1 < c.len() && k != 0 {
            k += 1;
        }
        c[k..].iter().collect()
    } else {
        let mut left = n;
        let mut k = 1;
        while k < c.len() {
            if c[k] == '/' {
                left += 1;
                if left == 0 {
                    break;
                }
            }
            k += 1;
        }
        c[..k.min(c.len())].iter().collect()
    }
}

// Visible width, not counting `\x1b[...m` sequences.
fn visible_width(s: &str) -> usize {
    let mut n = 0;
    let mut esc = false;
    for c in s.chars() {
        if esc {
            if c.is_ascii_alphabetic() {
                esc = false;
            }
        } else if c == '\x1b' {
            esc = true;
        } else {
            n += 1;
        }
    }
    n
}

impl Prompt {
    fn at(&self, k: usize) -> Option<char> {
        self.f.get(k).copied()
    }

    fn number(&mut self) -> Option<i64> {
        let start = self.pos;
        while self.at(self.pos).is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        (self.pos > start).then(|| {
            self.f[start..self.pos]
                .iter()
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
    }

    // Text up to `end`, with backslash escapes; pos after `end`.
    fn upto(&mut self, end: char) -> String {
        let mut s = String::new();
        while let Some(c) = self.at(self.pos) {
            self.pos += 1;
            if c == end {
                break;
            }
            if c == '\\' {
                if let Some(n) = self.at(self.pos) {
                    s.push(n);
                    self.pos += 1;
                    continue;
                }
            }
            s.push(c);
        }
        s
    }

    fn test(&self, cond: char, arg: i64) -> i32 {
        let pwd = var_get("PWD");
        match cond {
            'c' | '.' | '~' | '/' | 'C' => {
                let mut arg = arg;
                let mut ss: &str = &pwd;
                if matches!(cond, 'c' | '.' | '~') {
                    if let Some((_, dir)) = find_named_dir(&pwd) {
                        arg -= 1;
                        ss = &pwd[dir.len()..];
                    }
                }
                let mut it = ss.chars();
                if it.next() == Some('/') && !it.as_str().is_empty() {
                    arg -= 1;
                }
                arg -= it.filter(|&c| c == '/').count() as i64;
                i32::from(arg <= 0)
            }
            't' | 'T' | 'd' | 'D' | 'w' => {
                let fmt = match cond {
                    't' => "%M",
                    'T' => "%H",
                    'd' => "%d",
                    'D' => "%m",
                    _ => "%w",
                };
                let mut v: i64 = time_fmt(fmt).trim().parse().unwrap_or(-1);
                if cond == 'D' {
                    v -= 1;
                }
                i32::from(v == arg)
            }
            '?' => i32::from(with_shell(|s| s.last_status) as i64 == arg),
            '#' => i32::from(unsafe { libc::geteuid() } as i64 == arg),
            'g' => i32::from(unsafe { libc::getegid() } as i64 == arg),
            'j' => i32::from(jobs_count().parse::<i64>().unwrap_or(0) >= arg),
            'L' => i32::from(var_get("SHLVL").parse::<i64>().unwrap_or(0) >= arg),
            'S' => {
                let secs = START
                    .get_or_init(std::time::Instant::now)
                    .elapsed()
                    .as_secs() as i64;
                i32::from(secs >= arg)
            }
            'v' => i32::from(with_shell(|s| s.array("psvar").map_or(0, |a| a.len())) as i64 >= arg),
            'V' => {
                let ps = with_shell(|s| s.array("psvar").cloned()).unwrap_or_default();
                let k = if arg == 0 { 1 } else { arg } as usize;
                i32::from(ps.len() >= k && !ps[k - 1].is_empty())
            }
            'e' => {
                let depth = with_err_ctx(|e| e.stack.len()) as i64;
                i32::from(depth >= arg)
            }
            '_' => i32::from(crate::exec::CMD_STACK.lock().map_or(0, |c| c.len()) as i64 >= arg),
            '!' => i32::from(unsafe { libc::geteuid() } == 0),
            'l' => 0,
            _ => -1,
        }
    }

    // Processes the format up to `end` (or the end). False if the
    // format ended early.
    fn put(&mut self, out: &mut String, doprint: bool, end: Option<char>) -> bool {
        while let Some(c) = self.at(self.pos) {
            if Some(c) == end {
                return true;
            }
            if c != '%' {
                if doprint {
                    out.push(c);
                }
                self.pos += 1;
                continue;
            }
            let pct = self.pos;
            self.pos += 1;
            let mut minus = false;
            if self.at(self.pos) == Some('-') {
                minus = true;
                self.pos += 1;
            }
            let mut arg = match self.number() {
                Some(n) => n,
                None if minus => 1,
                None => 0,
            };
            if minus {
                arg = -arg;
            }
            let Some(e) = self.at(self.pos) else {
                if doprint {
                    out.push('%');
                }
                return false;
            };
            if e == '(' {
                self.pos += 1;
                if let Some(n) = self.number() {
                    arg = n;
                } else if arg < 0 {
                    arg = -arg;
                }
                let Some(cond) = self.at(self.pos) else {
                    return false;
                };
                let test = self.test(cond, arg);
                self.pos += 1;
                let Some(sep) = self.at(self.pos) else {
                    return false;
                };
                self.pos += 1;
                let saved = self.trunc;
                self.trunc = false;
                if !self.put(out, test == 1 && doprint, Some(sep)) {
                    self.trunc = saved;
                    return false;
                }
                self.pos += 1;
                if self.pos > self.f.len() || !self.put(out, test == 0 && doprint, Some(')')) {
                    self.trunc = saved;
                    return false;
                }
                self.trunc = saved;
                self.pos += 1;
                continue;
            }
            self.pos += 1;
            // Truncation: `%N<str<` ... (to the next truncation or end).
            if matches!(e, '<' | '>' | '[') {
                let close = if e == '[' { ']' } else { e };
                if e == '[' {
                    if let Some(n) = self.number() {
                        arg = n;
                    }
                }
                if arg <= 0 {
                    // `%<<`: ends a truncation region.
                    let _ = self.upto(close);
                    if self.trunc {
                        return true;
                    }
                    continue;
                }
                if self.trunc {
                    self.pos = pct;
                    return true;
                }
                let marker = self.upto(close);
                let mut region = String::new();
                self.trunc = true;
                let ok = self.put(&mut region, doprint, end);
                self.trunc = false;
                if doprint {
                    let width = arg as usize;
                    let w = visible_width(&region);
                    if w <= width {
                        out.push_str(&region);
                    } else {
                        let chars: Vec<char> = region.chars().collect();
                        let mlen = marker.chars().count();
                        if mlen >= width {
                            out.push_str(&marker);
                        } else {
                            let keep = width - mlen;
                            if e == '<' {
                                out.push_str(&marker);
                                out.extend(&chars[chars.len() - keep..]);
                            } else {
                                out.extend(&chars[..keep]);
                                out.push_str(&marker);
                            }
                        }
                    }
                }
                if !ok {
                    return false;
                }
                continue;
            }
            if !doprint {
                if matches!(e, 'D' | 'F' | 'K' | 'H') && self.at(self.pos) == Some('{') {
                    self.pos += 1;
                    let _ = self.upto('}');
                }
                continue;
            }
            let pwd = var_get("PWD");
            match e {
                '~' => out.push_str(&prompt_path(&pwd, arg, true)),
                'd' | '/' => out.push_str(&prompt_path(&pwd, arg, false)),
                'c' | '.' => out.push_str(&prompt_path(&pwd, if arg == 0 { 1 } else { arg }, true)),
                'C' => out.push_str(&prompt_path(&pwd, if arg == 0 { 1 } else { arg }, false)),
                // %N: the running function, eval or script.
                'N' => out.push_str(&with_err_ctx(|e| {
                    e.stack
                        .last()
                        .map(|c| c.0.clone())
                        .unwrap_or_else(|| e.name.clone())
                })),
                'h' | '!' => out.push_str(&history_count()),
                'j' => out.push_str(&jobs_count()),
                'M' => out.push_str(&var_get("HOST")),
                'm' => {
                    let host = var_get("HOST");
                    let arg = if arg == 0 { 1 } else { arg };
                    let parts: Vec<&str> = host.split('.').collect();
                    let text = if arg > 0 {
                        parts[..(arg as usize).min(parts.len())].join(".")
                    } else {
                        parts[parts.len().saturating_sub((-arg) as usize)..].join(".")
                    };
                    out.push_str(&text);
                }
                'n' => out.push_str(&var_get("USERNAME")),
                'l' | 'y' => {
                    let tty = unsafe {
                        let p = libc::ttyname(0);
                        if p.is_null() {
                            String::new()
                        } else {
                            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
                        }
                    };
                    let shown = if tty.is_empty() {
                        "()".to_string()
                    } else if e == 'l' {
                        tty.strip_prefix("/dev/tty")
                            .or_else(|| tty.strip_prefix("/dev/"))
                            .unwrap_or(&tty)
                            .to_string()
                    } else {
                        tty.strip_prefix("/dev/").unwrap_or(&tty).to_string()
                    };
                    out.push_str(&shown);
                }
                'L' => out.push_str(&var_get("SHLVL")),
                '?' => out.push_str(&with_shell(|s| s.last_status).to_string()),
                '%' | ')' => out.push(e),
                '#' => out.push(if is_root() { '#' } else { '%' }),
                'v' => {
                    let ps = with_shell(|s| s.array("psvar").cloned()).unwrap_or_default();
                    let k = if arg == 0 {
                        1
                    } else if arg < 0 {
                        ps.len() as i64 + arg + 1
                    } else {
                        arg
                    };
                    if k > 0 && (k as usize) <= ps.len() {
                        out.push_str(&ps[k as usize - 1]);
                    }
                }
                'e' => out.push_str(&with_err_ctx(|e| e.stack.len()).to_string()),
                'i' => out.push_str(&lineno().to_string()),
                'I' => out.push_str(&with_err_ctx(|e| e.lineno).to_string()),
                'x' => {
                    let file = crate::exec::code_origin();
                    out.push_str(&prompt_path(&file, arg, false));
                }
                'T' => out.push_str(time_fmt("%k:%M").trim_start()),
                '*' => out.push_str(time_fmt("%k:%M:%S").trim_start()),
                't' | '@' => out.push_str(time_fmt("%l:%M%p").trim_start()),
                'w' => out.push_str(&time_fmt("%a %e").replace("  ", " ")),
                'W' => out.push_str(&time_fmt("%m/%d/%y")),
                'D' => {
                    if self.at(self.pos) == Some('{') {
                        self.pos += 1;
                        let fmt = self.upto('}');
                        if !fmt.is_empty() {
                            out.push_str(&time_fmt(&fmt));
                        }
                    } else {
                        out.push_str(&time_fmt("%y-%m-%d"));
                    }
                }
                'B' => self.set_attr(out, 1, true, "\x1b[1m"),
                'b' => self.set_attr(out, 1, false, "\x1b[22m"),
                'U' => self.set_attr(out, 2, true, "\x1b[4m"),
                'u' => self.set_attr(out, 2, false, "\x1b[24m"),
                'S' => self.set_attr(out, 4, true, "\x1b[7m"),
                's' => self.set_attr(out, 4, false, "\x1b[27m"),
                'E' => out.push_str("\x1b[K"),
                'F' | 'K' => {
                    let spec = if self.at(self.pos) == Some('{') {
                        self.pos += 1;
                        Some(self.upto('}'))
                    } else if arg > 0 {
                        Some(arg.to_string())
                    } else {
                        None
                    };
                    match (spec, e) {
                        (Some(n), 'F') => {
                            self.active_attrs |= 8;
                            self.unknown_attrs &= !8;
                            out.push_str(&color_fg(&n));
                        }
                        (Some(n), _) => {
                            self.active_attrs |= 16;
                            self.unknown_attrs &= !16;
                            out.push_str(&color_bg(&n));
                        }
                        (None, 'F') => self.set_attr(out, 8, false, "\x1b[39m"),
                        (None, _) => self.set_attr(out, 16, false, "\x1b[49m"),
                    }
                }
                'f' => self.set_attr(out, 8, false, "\x1b[39m"),
                'k' => self.set_attr(out, 16, false, "\x1b[49m"),
                'H' => {
                    if self.at(self.pos) == Some('{') {
                        self.pos += 1;
                        let _ = self.upto('}');
                    }
                }
                // %{ ... %}: text that takes no space, kept as is.
                '{' | '}' | 'G' => {}
                '_' | '^' => {
                    let stack = crate::exec::CMD_STACK
                        .lock()
                        .map(|c| c.clone())
                        .unwrap_or_default();
                    let n = stack.len() as i64;
                    if n > 0 {
                        let k = if arg == 0 || arg.abs() > n {
                            n
                        } else {
                            arg.abs()
                        };
                        let part: Vec<&str> = if (arg >= 0) == (e == '_') {
                            stack[(n - k) as usize..].to_vec()
                        } else {
                            stack[..k as usize].to_vec()
                        };
                        let part: Vec<&str> = if e == '^' {
                            part.into_iter().rev().collect()
                        } else {
                            part
                        };
                        out.push_str(&part.join(" "));
                    }
                }
                'r' | 'R' => {}
                other => {
                    out.push('%');
                    out.push(other);
                }
            }
        }
        end.is_none()
    }

    fn set_attr(&mut self, out: &mut String, bit: u8, on: bool, seq: &str) {
        let was_on = (self.active_attrs & bit) != 0;
        if (self.unknown_attrs & bit) != 0 || was_on != on {
            out.push_str(seq);
        }
        if on {
            self.active_attrs |= bit;
        } else {
            self.active_attrs &= !bit;
        }
        self.unknown_attrs &= !bit;
    }
}

fn hostname_full() -> String {
    let mut buf = vec![0u8; 256];
    unsafe {
        if libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) == 0 {
            let cstr = std::ffi::CStr::from_ptr(buf.as_ptr() as *const libc::c_char);
            return cstr.to_string_lossy().into_owned();
        }
    }
    String::new()
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn time_fmt(fmt: &str) -> String {
    // zsh's ztrftime, with its %N and %. (fractions of a second).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    // The platform's time_t, which libc no longer lets 32-bit musl name.
    let t = now.as_secs() as _;
    // SAFETY: localtime_r fills a zeroed tm from a valid time_t.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        tm
    };
    crate::zdatetime::format_time(fmt.as_bytes(), &tm, i64::from(now.subsec_nanos()))
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

fn history_count() -> String {
    let shell = SHELL.shell.lock().unwrap();
    let n = shell.history.lock().unwrap().len();
    n.to_string()
}

fn jobs_count() -> String {
    let shell = SHELL.shell.lock().unwrap();
    let n = shell.jobs.lock().unwrap().len();
    n.to_string()
}

fn color_code(name: &str) -> Option<u16> {
    match name.to_ascii_lowercase().as_str() {
        "black" => Some(0),
        "red" => Some(1),
        "green" => Some(2),
        "yellow" => Some(3),
        "blue" => Some(4),
        "magenta" => Some(5),
        "cyan" => Some(6),
        "white" => Some(7),
        "default" => Some(9),
        _ => name.parse::<u16>().ok(),
    }
}

fn color_fg(name: &str) -> String {
    if let Some((r, g, b)) = hex_color(name) {
        return crate::zle::refresh::refresh_attr::prompt_hex_color(r, g, b, true);
    }
    match color_code(name) {
        Some(n) if n < 8 => format!("\x1b[{}m", 30 + n),
        Some(n) if n != 9 => format!("\x1b[38;5;{}m", n),
        // default, and names zsh does not know, give the default colour.
        _ => "\x1b[39m".to_string(),
    }
}

fn color_bg(name: &str) -> String {
    if let Some((r, g, b)) = hex_color(name) {
        return crate::zle::refresh::refresh_attr::prompt_hex_color(r, g, b, false);
    }
    match color_code(name) {
        Some(n) if n < 8 => format!("\x1b[{}m", 40 + n),
        Some(n) if n != 9 => format!("\x1b[48;5;{}m", n),
        _ => "\x1b[49m".to_string(),
    }
}

// `#rrggbb` or `#rgb`.
fn hex_color(name: &str) -> Option<(u8, u8, u8)> {
    let h = name.strip_prefix('#')?;
    let v = u32::from_str_radix(h, 16).ok()?;
    match h.len() {
        6 => Some(((v >> 16) as u8, (v >> 8) as u8, v as u8)),
        3 => {
            let d = |x: u32| ((x & 0xf) * 17) as u8;
            Some((d(v >> 8), d(v >> 4), d(v)))
        }
        _ => None,
    }
}

/// Names of all executables in $PATH, for zsh's `${(k)commands}`.
pub fn all_commands() -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for dir in var_get("PATH").split(':').filter(|d| !d.is_empty()) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                if is_executable(&e.path()) {
                    names.insert(e.file_name().to_string_lossy().into_owned());
                }
            }
        }
    }
    names.into_iter().collect()
}

pub fn find_command(name: &str) -> Option<String> {
    if name.contains('/') {
        let p = std::path::Path::new(name);
        if p.is_file() && is_executable(p) {
            return Some(name.to_string());
        }
        return None;
    }

    find_command_in(name, &var_get("PATH"))
}

/// Looks up `name` in the command hash table, searching `$PATH` and
/// caching when `HASH_CMDS` is set.
pub fn lookup_command(name: &str) -> Option<String> {
    if name.contains('/') {
        return find_command(name);
    }
    if let Some(p) = with_shell(|s| s.cmd_hash.get(name).cloned()) {
        return Some(p);
    }
    let found = find_command(name)?;
    if crate::options::is_set("hashcmds") {
        with_shell(|s| {
            s.cmd_hash.insert(name.to_string(), found.clone());
        });
    }
    Some(found)
}

/// `find_command` searching the directories of `path`.
pub fn find_command_in(name: &str, path: &str) -> Option<String> {
    if name.contains('/') {
        return find_command(name);
    }
    for dir in path.split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let p = std::path::Path::new(dir).join(name);
        if p.is_file() && is_executable(&p) {
            return Some(p.to_string_lossy().to_string());
        }
    }
    None
}

fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = path.metadata() {
            let mode = metadata.permissions().mode();
            return metadata.is_file() && (mode & 0o111 != 0);
        }
    }
    path.is_file()
}

pub fn command_exists(name: &str) -> bool {
    let name = crate::strip_ansi(name);
    let name = name.as_str();
    {
        let shell = SHELL.shell.lock().unwrap();
        if shell.alias(name, false).is_some() || shell.functions.contains_key(name) {
            return true;
        }
    }

    if crate::builtins::is_builtin(name) {
        return true;
    }

    find_command(name).is_some()
}

/// Where errors say they happened, kept apart from the shell state so an
/// error can be reported while the shell is locked.
pub struct ErrCtx {
    // (name, line the numbering starts after) for each function,
    // `(eval)` or sourced file being run.
    pub stack: Vec<(String, usize)>,
    // For each entry of `stack`, where it was called from: zsh's
    // $functrace entry (caller:line in the caller), $funcfiletrace entry
    // (file:line in the file) and $funcsourcetrace entry (file:line of
    // its definition).
    pub calls: Vec<(String, String, String)>,
    // Line of the command running, in its input.
    pub lineno: usize,
    // The script, or the shell's name for `-c` and standard input.
    pub name: String,
    pub interactive: bool,
    // Commands come from standard input (SHIN_STDIN).
    pub stdin: bool,
    // Inside an eval with NO_EVAL_LINENO: line numbers stay the caller's.
    pub frozen: usize,
}

static ERR_CTX: std::sync::Mutex<ErrCtx> = std::sync::Mutex::new(ErrCtx {
    stack: Vec::new(),
    calls: Vec::new(),
    lineno: 0,
    name: String::new(),
    interactive: false,
    stdin: false,
    frozen: 0,
});

pub fn with_err_ctx<R>(f: impl FnOnce(&mut ErrCtx) -> R) -> R {
    let mut guard = ERR_CTX.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// zsh's prefix for an error message: `name:line: ` (or
/// `name:builtin:line: `), where name is the function, `(eval)`, sourced
/// file or script running. Interactive commands typed at the prompt get
/// just `name: ` / `builtin: `.
pub fn err_prefix(builtin: Option<&str>) -> String {
    // Commands read from standard input are reported as typed ones are,
    // without a line number. (No option lookup here: callers may hold the
    // shell lock.)
    with_err_ctx(|e| {
        if (e.interactive || e.stdin) && e.stack.is_empty() {
            return match builtin {
                Some(b) => format!("{}: ", b),
                None => format!("{}: ", e.name),
            };
        }
        let (name, base) = e
            .stack
            .last()
            .cloned()
            .unwrap_or_else(|| (e.name.clone(), 0));
        let line = e.lineno.saturating_sub(base);
        let line = if line > 0 {
            format!(":{}", line)
        } else {
            String::new()
        };
        match builtin {
            Some(b) => format!("{}:{}{}: ", name, b, line),
            None => format!("{}{}: ", name, line),
        }
    })
}

/// `$LINENO`: the line of the running command, counted within the
/// function, `eval` or file it is in.
pub fn lineno() -> usize {
    with_err_ctx(|e| {
        let base = e.stack.last().map(|c| c.1).unwrap_or(0);
        e.lineno.saturating_sub(base)
    })
}

/// Prints a shell error in zsh's format.
pub fn shell_err(msg: &str) {
    crate::sys::err_line(&format!("{}{}", err_prefix(None), msg));
}

/// Prints an error from builtin `name` in zsh's format.
pub fn builtin_err(name: &str, msg: &str) {
    crate::sys::err_line(&format!("{}{}", err_prefix(Some(name)), msg));
}

const RAW_BYTE_BASE: u32 = 0x10_FF00;

/// Encodes a byte as a char, mapping raw high bytes (`0x80..=0xFF`) into
/// `U+10FF80..=U+10FFFF` so they never collide with `U+0080..=U+00FF`.
#[inline]
pub fn raw_byte_char(b: u8) -> char {
    if b < 0x80 {
        b as char
    } else {
        char::from_u32(RAW_BYTE_BASE + b as u32).unwrap()
    }
}

/// Extracts the raw byte if `c` is an escaped raw high byte (`U+10FF80..=U+10FFFF`).
#[inline]
pub fn decode_raw_byte(c: char) -> Option<u8> {
    let u = c as u32;
    (RAW_BYTE_BASE + 0x80..=RAW_BYTE_BASE + 0xFF)
        .contains(&u)
        .then(|| (u - RAW_BYTE_BASE) as u8)
}

/// Losslessly decodes bytes into a zstr, keeping valid UTF-8 intact and escaping
/// invalid bytes (`0x80..=0xFF`) into `U+10FF80..=U+10FFFF`.
pub fn bytes_to_zstr(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match std::str::from_utf8(&bytes[i..]) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                if valid > 0 {
                    out.push_str(unsafe { std::str::from_utf8_unchecked(&bytes[i..i + valid]) });
                    i += valid;
                }
                let bad = e.error_len().unwrap_or(bytes.len() - i);
                for &b in &bytes[i..i + bad] {
                    out.push(raw_byte_char(b));
                }
                i += bad;
            }
        }
    }
    out
}

/// Encodes a zstr back to raw bytes.
pub fn zstr_to_bytes(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0u8; 4];
    for c in s.chars() {
        if let Some(b) = decode_raw_byte(c) {
            out.push(b);
        } else {
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    out
}

/// Re-parses a zstr as UTF-8 bytes + remaining raw bytes when in a UTF-8 multibyte locale.
pub fn normalize_zstr(s: &str) -> String {
    if is_c_locale() {
        s.to_string()
    } else {
        bytes_to_zstr(&zstr_to_bytes(s))
    }
}

/// Renders a single byte using zsh's `nicechar` (`\M-`, `^X`, `^?`).
pub fn nice_byte_char(mut b: u8) -> String {
    let mut out = String::new();
    if b & 0x80 != 0 && !crate::options::is_set("printeightbit") {
        out.push_str("\\M-");
        b &= 0x7f;
    }
    if b == 0x7f {
        out.push_str("^?");
    } else if b == b'\n' {
        out.push_str("\\n");
    } else if b == b'\t' {
        out.push_str("\\t");
    } else if b < 0x20 {
        out.push('^');
        out.push((b + 0x40) as char);
    } else {
        out.push(b as char);
    }
    out
}

/// Renders raw/control bytes via `nice_byte_char`, keeping valid UTF-8 printable chars intact.
pub fn nice_str(s: &str) -> String {
    nice_str_inner(s, false)
}

/// Renders every raw/non-ASCII/control byte via `nice_byte_char`.
pub fn nice_bytes_str(s: &str) -> String {
    nice_str_inner(s, true)
}

fn nice_str_inner(s: &str, all_non_ascii: bool) -> String {
    let mut out = String::new();
    let mut buf = [0u8; 4];
    for c in s.chars() {
        if let Some(b) = decode_raw_byte(c) {
            out.push_str(&nice_byte_char(b));
        } else if (c as u32) < 0x20 || c == '\x7f' {
            out.push_str(&nice_byte_char(c as u8));
        } else if all_non_ascii && !c.is_ascii() {
            for &b in c.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&nice_byte_char(b));
            }
        } else {
            out.push(c);
        }
    }
    out
}

static LAST_LC_CTYPE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn effective_lc_ctype() -> String {
    with_shell(|s| {
        for k in ["LC_ALL", "LC_CTYPE", "LANG"] {
            if let Some(p) = s.param(k) {
                let v = p.text();
                if p.attrs & crate::params::UNSET == 0 && !v.is_empty() {
                    return v;
                }
            }
        }
        String::new()
    })
}

/// Synchronizes libc `LC_CTYPE` with the shell's `LC_ALL` / `LC_CTYPE` / `LANG`.
pub fn sync_lc_ctype() {
    let want = effective_lc_ctype();
    let target = if want.is_empty() { "C" } else { &want };
    let mut last = LAST_LC_CTYPE.lock().unwrap_or_else(|e| e.into_inner());
    if last.as_deref() == Some(target) {
        return;
    }
    if let Ok(c) = std::ffi::CString::new(zstr_to_bytes(target)) {
        let res = unsafe { libc::setlocale(libc::LC_CTYPE, c.as_ptr()) };
        if res.is_null() && target.to_ascii_lowercase().contains("utf") {
            let _ = unsafe { libc::setlocale(libc::LC_CTYPE, c"C.UTF-8".as_ptr()) };
        }
    }
    *last = Some(target.to_string());
}

/// True when the active `LC_CTYPE` is single-byte / non-UTF-8 or `NO_MULTIBYTE` is set.
pub fn is_c_locale() -> bool {
    sync_lc_ctype();
    if !crate::options::is_set("multibyte") {
        return true;
    }
    let cs = unsafe {
        let p = libc::nl_langinfo(libc::CODESET);
        if p.is_null() {
            ""
        } else {
            std::ffi::CStr::from_ptr(p).to_str().unwrap_or("")
        }
    };
    !cs.eq_ignore_ascii_case("UTF-8") && !cs.eq_ignore_ascii_case("UTF8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_LOCK;

    #[test]
    fn test_expand_prompt_literal_percent() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        assert_eq!(expand_prompt("%%"), "%");
    }

    #[test]
    fn test_expand_prompt_color_named() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        let s = expand_prompt("%F{red}x%f");
        assert!(s.contains("\x1b[31m"), "missing red SGR in {:?}", s);
        assert!(s.contains('x'));
        assert!(
            s.contains("\x1b[39m"),
            "missing default fg reset in {:?}",
            s
        );
    }

    #[test]
    fn test_expand_prompt_color_numeric() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        let s = expand_prompt("%F{208}orange%f");
        assert!(s.contains("\x1b[38;5;208m"));
    }

    #[test]
    fn test_expand_prompt_bold_underline() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        let s = expand_prompt("%B%Utext%u%b");
        assert!(s.contains("\x1b[1m"));
        assert!(s.contains("\x1b[4m"));
        assert!(s.contains("\x1b[24m"));
        assert!(s.contains("\x1b[22m"));
    }

    #[test]
    fn test_expand_prompt_user_and_pwd() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        // %n is $USERNAME, the login name, as in zsh.
        var_set("HOME", "/home/alice", false);
        var_set("PWD", "/home/alice/projects", false);
        let s = expand_prompt("%n at %~");
        assert_eq!(s, format!("{} at ~/projects", var_get("USERNAME")));
    }

    #[test]
    fn test_expand_prompt_passes_through_unknown() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        let s = expand_prompt("%Z");
        assert_eq!(s, "%Z");
    }

    #[test]
    fn test_expand_prompt_raw_braces() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        let s = expand_prompt("%{\x1b[31m%}red%{\x1b[0m%}");
        assert_eq!(s, "\x1b[31mred\x1b[0m");
    }

    #[test]
    fn test_prompt_truncation() {
        assert_eq!(expand_prompt("%5<..<abcdefgh%<<!"), "..fgh!");
        assert_eq!(expand_prompt("%5>..>abcdefgh%>>!"), "abc..!");
        assert_eq!(expand_prompt("%9<..<short%<<"), "short");
    }

    #[test]
    fn test_prompt_expands_substitutions_before_percent_escapes() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        // Substitution needs PROMPT_SUBST, which starship and
        // oh-my-zsh turn on.
        with_shell(|s| {
            crate::options::set_in(s, "promptsubst", true);
            crate::options::set_in(s, "promptpercent", true);
        });
        var_set("PS1", "", false);
        var_set("PROMPT", "$(printf '%%{X%%}a')%%", false);
        assert_eq!(build_prompt(), "Xa%");
    }

    #[test]
    fn test_build_prompt_uses_cat_fallback_without_ps1() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        // Unset, not empty: an empty PS1 means no prompt, as in zsh.
        with_shell(|s| {
            s.remove_param("PS1");
            s.remove_param("PROMPT");
        });
        var_set("USER", "alice", false);
        var_set("HOME", "/home/alice", false);
        var_set("PWD", "/home/alice/projects", false);

        let s = build_prompt();
        assert!(s.contains("alice"));
        assert!(s.contains("~/projects"));
        assert!(s.contains("𓃠"));
        var_set("PS1", "", false);
        assert_eq!(build_prompt(), "");
    }

    #[test]
    fn test_diagnostic_commands() {
        let _lock = TEST_LOCK.lock().unwrap();
        shell_init();
        let path = var_get("PATH");
        println!("PATH in meowsh: {}", path);

        let ls_exists = command_exists("ls");
        println!("ls exists: {}", ls_exists);
        assert!(ls_exists, "ls should exist in PATH");

        // Try cargo if it exists in the outer environment
        if let Ok(cargo_path) = std::process::Command::new("which").arg("cargo").output() {
            if cargo_path.status.success() {
                let cargo_exists = command_exists("cargo");
                println!("cargo exists: {}", cargo_exists);
                assert!(
                    cargo_exists,
                    "cargo should exist in PATH if it exists in environment"
                );
            }
        }
    }
}
