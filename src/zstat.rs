//! zsh/stat (`stat` / `zstat`): file status queries (`Src/Modules/stat.c`).

use std::collections::BTreeMap;
use std::ffi::CString;

use crate::params::{Param, Value};
use crate::shell::{array_set, builtin_err, bytes_to_zstr, var_set, with_shell, zstr_to_bytes};
use crate::sys::out;

const FIELDS: &[&str] = &[
    "device", "inode", "mode", "nlink", "uid", "gid", "rdev", "size", "atime", "mtime", "ctime",
    "blksize", "blocks", "link",
];

fn match_field(arg: &str) -> Result<usize, String> {
    let matches: Vec<usize> = FIELDS
        .iter()
        .enumerate()
        .filter(|(_, f)| f.starts_with(arg))
        .map(|(i, _)| i)
        .collect();
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(format!("{}: no such stat element", arg)),
        _ => Err(format!("{}: ambiguous stat element", arg)),
    }
}

fn format_mode(mode: libc::mode_t, octal: bool, raw: bool, sym: bool) -> String {
    if raw || !sym {
        if octal {
            return format!("{:06o}", mode);
        }
        return mode.to_string();
    }
    let kind = match mode & libc::S_IFMT {
        libc::S_IFDIR => 'd',
        libc::S_IFLNK => 'l',
        libc::S_IFCHR => 'c',
        libc::S_IFBLK => 'b',
        libc::S_IFIFO => 'p',
        libc::S_IFSOCK => 's',
        _ => '-',
    };
    let mut s = String::with_capacity(10);
    s.push(kind);
    let tri = |r, w, x, spec, flag| -> [char; 3] {
        [
            if mode & r != 0 { 'r' } else { '-' },
            if mode & w != 0 { 'w' } else { '-' },
            match (mode & x != 0, mode & spec != 0) {
                (true, true) => flag,
                (false, true) => flag.to_ascii_uppercase(),
                (true, false) => 'x',
                (false, false) => '-',
            },
        ]
    };
    s.extend(tri(
        libc::S_IRUSR,
        libc::S_IWUSR,
        libc::S_IXUSR,
        libc::S_ISUID,
        's',
    ));
    s.extend(tri(
        libc::S_IRGRP,
        libc::S_IWGRP,
        libc::S_IXGRP,
        libc::S_ISGID,
        's',
    ));
    s.extend(tri(
        libc::S_IROTH,
        libc::S_IWOTH,
        libc::S_IXOTH,
        libc::S_ISVTX,
        't',
    ));
    s
}

fn format_time(sec: i64, raw: bool, sym: bool, gmt: bool, fmt: &str) -> String {
    if raw || !sym {
        return sec.to_string();
    }
    let t = sec;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        if gmt {
            !libc::gmtime_r(&t, &mut tm).is_null()
        } else {
            !libc::localtime_r(&t, &mut tm).is_null()
        }
    };
    if !ok {
        return sec.to_string();
    }
    let Ok(cfmt) = CString::new(zstr_to_bytes(fmt)) else {
        return sec.to_string();
    };
    let mut buf = [0u8; 256];
    let n = unsafe {
        libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            cfmt.as_ptr(),
            &tm,
        )
    };
    if n == 0 {
        return String::new();
    }
    bytes_to_zstr(&buf[..n])
}

struct StatFmt<'a> {
    octal: bool,
    raw: bool,
    sym: bool,
    gmt: bool,
    fmt: &'a str,
}

fn field_value(idx: usize, st: &libc::stat, link_target: &str, f: &StatFmt<'_>) -> String {
    match idx {
        0 => st.st_dev.to_string(),
        1 => st.st_ino.to_string(),
        2 => format_mode(st.st_mode, f.octal, f.raw, f.sym),
        3 => st.st_nlink.to_string(),
        4 => st.st_uid.to_string(),
        5 => st.st_gid.to_string(),
        6 => st.st_rdev.to_string(),
        7 => st.st_size.to_string(),
        8 => format_time(st.st_atime, f.raw, f.sym, f.gmt, f.fmt),
        9 => format_time(st.st_mtime, f.raw, f.sym, f.gmt, f.fmt),
        10 => format_time(st.st_ctime, f.raw, f.sym, f.gmt, f.fmt),
        11 => st.st_blksize.to_string(),
        12 => st.st_blocks.to_string(),
        13 => link_target.to_string(),
        _ => String::new(),
    }
}

/// Implements `zstat` / `stat` (`zsh/stat`).
pub fn builtin_zstat(words: &[String]) -> i32 {
    let cmd = words.first().map(|s| s.as_str()).unwrap_or("zstat");
    let mut pick: Option<usize> = None;
    let mut arr_name: Option<String> = None;
    let mut hash_name: Option<String> = None;
    let mut use_fd: Option<i32> = None;
    let mut timefmt = "%a %b %e %k:%M:%S %Z %Y".to_string();
    let (mut gmt, mut lstat_flag, mut show_name) = (false, false, false);
    let (mut show_type, mut hide_type) = (false, false);
    let (mut suppress_name, mut octal, mut raw, mut sym) = (false, false, false, false);

    let mut i = 1;
    while let Some(a) = words.get(i) {
        if a == "--" {
            i += 1;
            break;
        }
        if let Some(el) = a.strip_prefix('+') {
            match match_field(el) {
                Ok(idx) => {
                    if idx == 13 {
                        lstat_flag = true;
                    }
                    pick = Some(idx);
                    i += 1;
                    continue;
                }
                Err(msg) => {
                    builtin_err(cmd, &msg);
                    return 1;
                }
            }
        }
        let Some(opts) = a.strip_prefix('-').filter(|o| !o.is_empty()) else {
            break;
        };
        let mut chars = opts.chars().peekable();
        while let Some(c) = chars.next() {
            let take = |i: &mut usize, chars: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                let rest: String = chars.collect();
                if !rest.is_empty() {
                    Some(rest)
                } else {
                    *i += 1;
                    words.get(*i).cloned()
                }
            };
            match c {
                'g' => gmt = true,
                'l' => {
                    out(&format!("{}\n", FIELDS.join(" ")));
                    return 0;
                }
                'L' => lstat_flag = true,
                'n' => show_name = true,
                'N' => suppress_name = true,
                'o' => octal = true,
                'r' => raw = true,
                's' => sym = true,
                't' => show_type = true,
                'T' => hide_type = true,
                'A' => match take(&mut i, &mut chars) {
                    Some(v) => {
                        arr_name = Some(v);
                        break;
                    }
                    None => {
                        builtin_err(cmd, "missing parameter name");
                        return 1;
                    }
                },
                'H' => match take(&mut i, &mut chars) {
                    Some(v) => {
                        hash_name = Some(v);
                        break;
                    }
                    None => {
                        builtin_err(cmd, "missing parameter name");
                        return 1;
                    }
                },
                'f' => match take(&mut i, &mut chars).and_then(|s| s.parse::<i32>().ok()) {
                    Some(fd) => {
                        use_fd = Some(fd);
                        break;
                    }
                    None => {
                        builtin_err(cmd, "missing file descriptor");
                        return 1;
                    }
                },
                'F' => match take(&mut i, &mut chars) {
                    Some(f) => {
                        timefmt = f;
                        break;
                    }
                    None => {
                        builtin_err(cmd, "missing time format");
                        return 1;
                    }
                },
                other => {
                    builtin_err(cmd, &format!("bad option: -{}", other));
                    return 1;
                }
            }
        }
        i += 1;
    }

    let targets: Vec<String> = if let Some(fd) = use_fd {
        vec![fd.to_string()]
    } else {
        words[i..].to_vec()
    };
    if targets.is_empty() {
        builtin_err(cmd, "no files given");
        return 1;
    }

    let mut collected: Vec<String> = Vec::new();
    let mut hash_map: BTreeMap<String, String> = BTreeMap::new();
    let multi = targets.len() > 1;
    let print_stdout = arr_name.is_none() && hash_name.is_none();

    for target in &targets {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let mut link_target = String::new();
        let rc = if let Some(fd) = use_fd {
            unsafe { libc::fstat(fd, &mut st) }
        } else {
            let Ok(cpath) = CString::new(zstr_to_bytes(target)) else {
                builtin_err(cmd, &format!("{}: invalid file name", target));
                return 1;
            };
            if lstat_flag {
                let r = unsafe { libc::lstat(cpath.as_ptr(), &mut st) };
                if r == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFLNK {
                    let mut buf = vec![0u8; 4096];
                    let n = unsafe {
                        libc::readlink(
                            cpath.as_ptr(),
                            buf.as_mut_ptr() as *mut libc::c_char,
                            buf.len(),
                        )
                    };
                    if n > 0 {
                        link_target = bytes_to_zstr(&buf[..n as usize]);
                    }
                }
                r
            } else {
                unsafe { libc::stat(cpath.as_ptr(), &mut st) }
            }
        };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            builtin_err(cmd, &format!("{}: {}", target, e));
            return 1;
        }
        let range = match pick {
            Some(idx) => idx..=idx,
            None => 0..=FIELDS.len() - 1,
        };
        if print_stdout && ((multi && !suppress_name) || show_name) && pick.is_none() {
            out(&format!("{}:\n", target));
        }
        if !print_stdout && arr_name.is_some() && show_name && !suppress_name {
            collected.push(target.clone());
        }
        for idx in range {
            let fmt = StatFmt {
                octal,
                raw,
                sym,
                gmt,
                fmt: &timefmt,
            };
            let val = field_value(idx, &st, &link_target, &fmt);
            if hash_name.is_some() {
                hash_map.insert(FIELDS[idx].to_string(), val);
            } else if arr_name.is_some() {
                // Array elements carry the type name only on request (-t).
                if show_type && !hide_type {
                    collected.push(format!("{} {}", FIELDS[idx], val));
                } else {
                    collected.push(val);
                }
            } else {
                // A single picked element prints bare so `x=$(zstat +mtime f)`
                // yields a number; the full listing aligns type names instead.
                let prefix = if hide_type || (pick.is_some() && !show_type) {
                    String::new()
                } else if pick.is_some() {
                    format!("{} ", FIELDS[idx])
                } else {
                    format!("{:<8}", FIELDS[idx])
                };
                if pick.is_some() && ((multi && !suppress_name) || show_name) {
                    out(&format!("{} {}{}\n", target, prefix, val));
                } else {
                    out(&format!("{}{}\n", prefix, val));
                }
            }
        }
    }

    if let Some(arr) = arr_name {
        let is_assoc = with_shell(|s| s.assoc(&arr).is_some());
        if is_assoc {
            let mut m = BTreeMap::new();
            for [k, v] in collected.as_chunks::<2>().0 {
                m.insert(k.clone(), v.clone());
            }
            with_shell(|s| match s.param_mut(&arr) {
                Some(p) => {
                    p.value = Value::Assoc(m);
                    p.attrs &= !crate::params::UNSET;
                }
                None => {
                    s.params.insert(arr, Param::with(Value::Assoc(m)));
                }
            });
        } else {
            array_set(&arr, collected);
        }
    } else if let Some(h) = hash_name {
        with_shell(|s| s.params.insert(h, Param::with(Value::Assoc(hash_map))));
    } else {
        let _ = var_set("REPLY", "", false);
    }
    0
}
