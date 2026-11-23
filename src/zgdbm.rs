//! `zsh/db/gdbm` module backed by `libgdbm.so.6` via `dlopen`.
//!
//! Provides `ztie`, `zuntie`, `zgdbmpath`, `zgdbmclear`, and the read-only
//! `zgdbm_tied` array parameter, with full local-scope auto-untie, nameref
//! resolution, embedded NUL/UTF-8 round-tripping, and cross-fork sync.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, c_void, CString};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::SystemTime;

use crate::params::{Param, Value, READONLY, SPECIAL, UNSET};
use crate::shell::{builtin_err, bytes_to_zstr, var_set, with_shell, zstr_to_bytes};

const GDBM_READER: c_int = 0;
const GDBM_WRCREAT: c_int = 2;
const GDBM_SYNC: c_int = 0x20;
const GDBM_REPLACE: c_int = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct Datum {
    dptr: *mut c_char,
    dsize: c_int,
}

type OpenFn = unsafe extern "C" fn(
    name: *const c_char,
    block_size: c_int,
    read_write: c_int,
    mode: c_int,
    fatal_func: *const c_void,
) -> *mut c_void;
type CloseFn = unsafe extern "C" fn(dbf: *mut c_void);
type FetchFn = unsafe extern "C" fn(dbf: *mut c_void, key: Datum) -> Datum;
type StoreFn =
    unsafe extern "C" fn(dbf: *mut c_void, key: Datum, content: Datum, flag: c_int) -> c_int;
type DeleteFn = unsafe extern "C" fn(dbf: *mut c_void, key: Datum) -> c_int;
type FirstKeyFn = unsafe extern "C" fn(dbf: *mut c_void) -> Datum;
type NextKeyFn = unsafe extern "C" fn(dbf: *mut c_void, key: Datum) -> Datum;
type SyncFn = unsafe extern "C" fn(dbf: *mut c_void);

struct GdbmApi {
    open: OpenFn,
    close: CloseFn,
    fetch: FetchFn,
    store: StoreFn,
    delete: DeleteFn,
    firstkey: FirstKeyFn,
    nextkey: NextKeyFn,
    sync: SyncFn,
}

unsafe impl Send for GdbmApi {}
unsafe impl Sync for GdbmApi {}

fn api() -> Option<&'static GdbmApi> {
    static API: OnceLock<Option<GdbmApi>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let mut handle = std::ptr::null_mut();
        #[cfg(target_os = "macos")]
        let names: &[&[u8]] = &[
            b"libgdbm.6.dylib\0",
            b"libgdbm.dylib\0",
            b"/opt/homebrew/lib/libgdbm.6.dylib\0",
            b"/usr/local/lib/libgdbm.6.dylib\0",
        ];
        #[cfg(not(target_os = "macos"))]
        let names: &[&[u8]] = &[b"libgdbm.so.6\0", b"libgdbm.so\0"];
        for name in names {
            handle = libc::dlopen(name.as_ptr() as *const c_char, libc::RTLD_LAZY);
            if !handle.is_null() {
                break;
            }
        }
        if handle.is_null() {
            return None;
        }
        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                let p = libc::dlsym(handle, concat!($name, "\0").as_ptr() as *const c_char);
                if p.is_null() {
                    return None;
                }
                std::mem::transmute::<*mut c_void, $ty>(p)
            }};
        }
        Some(GdbmApi {
            open: sym!("gdbm_open", OpenFn),
            close: sym!("gdbm_close", CloseFn),
            fetch: sym!("gdbm_fetch", FetchFn),
            store: sym!("gdbm_store", StoreFn),
            delete: sym!("gdbm_delete", DeleteFn),
            firstkey: sym!("gdbm_firstkey", FirstKeyFn),
            nextkey: sym!("gdbm_nextkey", NextKeyFn),
            sync: sym!("gdbm_sync", SyncFn),
        })
    })
    .as_ref()
}

pub fn is_available() -> bool {
    api().is_some()
}

struct TiedDb {
    name: String,
    level: usize,
    path: String,
    readonly: bool,
    dbf: *mut c_void,
    open_pid: u32,
    last_mtime: Option<(SystemTime, u64)>,
}

impl Drop for TiedDb {
    fn drop(&mut self) {
        if !self.dbf.is_null() && self.open_pid == std::process::id() {
            if let Some(a) = api() {
                unsafe { (a.close)(self.dbf) };
            }
            self.dbf = std::ptr::null_mut();
        }
    }
}

thread_local! {
    static TIED: RefCell<Vec<TiedDb>> = const { RefCell::new(Vec::new()) };
}

fn file_stamp(path: &str) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

fn open_handle(path: &str, readonly: bool) -> Option<*mut c_void> {
    let a = api()?;
    let cpath = CString::new(zstr_to_bytes(path)).ok()?;
    let flags = if readonly {
        GDBM_READER
    } else {
        GDBM_WRCREAT | GDBM_SYNC
    };
    let dbf = unsafe { (a.open)(cpath.as_ptr(), 0, flags, 0o666, std::ptr::null()) };
    if dbf.is_null() {
        None
    } else {
        Some(dbf)
    }
}

fn read_all_from_dbf(a: &GdbmApi, dbf: *mut c_void) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut key = unsafe { (a.firstkey)(dbf) };
    while !key.dptr.is_null() {
        let kbytes =
            unsafe { std::slice::from_raw_parts(key.dptr as *const u8, key.dsize as usize) };
        let kstr = bytes_to_zstr(kbytes);
        let val = unsafe { (a.fetch)(dbf, key) };
        if !val.dptr.is_null() {
            let vbytes =
                unsafe { std::slice::from_raw_parts(val.dptr as *const u8, val.dsize as usize) };
            map.insert(kstr, bytes_to_zstr(vbytes));
            unsafe { libc::free(val.dptr as *mut c_void) };
        }
        let next = unsafe { (a.nextkey)(dbf, key) };
        unsafe { libc::free(key.dptr as *mut c_void) };
        key = next;
    }
    map
}

fn refresh_tied_param() {
    let names: Vec<String> = TIED.with(|t| t.borrow().iter().map(|d| d.name.clone()).collect());
    with_shell(|sh| {
        let mut p = Param::with(Value::Array(names));
        p.attrs = READONLY | SPECIAL;
        p.level = 0;
        sh.params.insert("zgdbm_tied".to_string(), p);
    });
}

pub fn ensure_tied_param() {
    let exists = with_shell(|sh| sh.params.contains_key("zgdbm_tied"));
    if !exists {
        refresh_tied_param();
    }
}

/// Resolves a variable name (following namerefs) to `(target_name, target_level, slot)`
/// where `slot` is `None` for `sh.params` or `Some(idx)` for `sh.local_scopes[idx]`.
fn resolve_target_binding(name: &str) -> (String, usize, Option<usize>) {
    with_shell(|sh| {
        let r = sh.resolve_ref_target(name);
        let tname = crate::shell::split_target_pub(&r.name);
        if let Some(Some(j)) = r.scope {
            let lvl = sh
                .local_scopes
                .get(j)
                .and_then(|s| s.get(&tname))
                .and_then(|o| o.as_ref())
                .map(|p| p.level)
                .unwrap_or(j);
            return (tname, lvl, Some(j));
        }
        let lvl = sh.params.get(&tname).map(|p| p.level).unwrap_or(0);
        (tname, lvl, None)
    })
}

fn store_param_at(tname: &str, level: usize, slot: Option<usize>, p: Param) {
    with_shell(|sh| {
        if let Some(j) = slot {
            if let Some(scope) = sh.local_scopes.get_mut(j) {
                scope.insert(tname.to_string(), Some(p));
                return;
            }
        }
        let mut p = p;
        p.level = level;
        sh.params.insert(tname.to_string(), p);
    });
}

fn remove_param_at(tname: &str, level: usize, slot: Option<usize>) {
    with_shell(|sh| {
        if let Some(j) = slot {
            if let Some(scope) = sh.local_scopes.get_mut(j) {
                let mut u = Param::with(Value::Scalar(String::new()));
                u.level = level;
                u.attrs = UNSET;
                scope.insert(tname.to_string(), Some(u));
                return;
            }
        }
        sh.params.remove(tname);
    });
}

fn close_tied_entry(tname: &str, level: usize) -> Option<TiedDb> {
    TIED.with(|t| {
        let mut list = t.borrow_mut();
        let pos = list
            .iter()
            .position(|d| d.name == tname && d.level == level)?;
        Some(list.remove(pos))
    })
}

pub fn on_scope_pop(level: usize) {
    let mut removed = false;
    TIED.with(|t| {
        let mut list = t.borrow_mut();
        let before = list.len();
        list.retain(|d| d.level != level);
        removed = list.len() != before;
    });
    if removed {
        refresh_tied_param();
    }
}

pub fn on_unload_module() {
    TIED.with(|t| t.borrow_mut().clear());
    with_shell(|sh| {
        sh.params.remove("zgdbm_tied");
    });
}

fn with_tied_mut<R>(
    name: &str,
    level: usize,
    f: impl FnOnce(&GdbmApi, &mut TiedDb) -> R,
) -> Option<R> {
    let a = api()?;
    TIED.with(|t| {
        let mut list = t.borrow_mut();
        let db = list
            .iter_mut()
            .find(|d| d.name == name && d.level == level)?;
        Some(f(a, db))
    })
}

pub fn on_read_refresh(name: &str, level: usize) {
    let updated = with_tied_mut(name, level, |a, db| {
        let pid = std::process::id();
        if db.open_pid == pid && !db.dbf.is_null() {
            unsafe { (a.close)(db.dbf) };
            db.dbf = std::ptr::null_mut();
        }
        if let Some(new_dbf) = open_handle(&db.path, db.readonly) {
            db.dbf = new_dbf;
            db.open_pid = pid;
            db.last_mtime = file_stamp(&db.path);
            return Some(read_all_from_dbf(a, db.dbf));
        }
        None
    })
    .flatten();
    if let Some(map) = updated {
        with_shell(|sh| {
            if let Some(p) = sh.params.get_mut(name) {
                if p.level == level {
                    p.value = Value::Assoc(map);
                }
            }
        });
    }
}

fn ensure_open_for_write(db: &mut TiedDb) -> bool {
    let pid = std::process::id();
    if db.open_pid != pid {
        // After fork, reopen rather than sharing the parent's GDBM_FILE
        // handle and open file description.
        db.dbf = std::ptr::null_mut();
        if let Some(new_dbf) = open_handle(&db.path, db.readonly) {
            db.dbf = new_dbf;
            db.open_pid = pid;
        }
    }
    !db.dbf.is_null()
}

pub fn on_set_key(name: &str, level: usize, key: &str, val: &str) {
    with_tied_mut(name, level, |a, db| {
        if db.readonly || !ensure_open_for_write(db) {
            return;
        }
        let kb = zstr_to_bytes(key);
        let vb = zstr_to_bytes(val);
        let kd = Datum {
            dptr: kb.as_ptr() as *mut c_char,
            dsize: kb.len() as c_int,
        };
        let vd = Datum {
            dptr: vb.as_ptr() as *mut c_char,
            dsize: vb.len() as c_int,
        };
        unsafe {
            (a.store)(db.dbf, kd, vd, GDBM_REPLACE);
            (a.sync)(db.dbf);
        }
        db.last_mtime = file_stamp(&db.path);
    });
}

pub fn on_del_key(name: &str, level: usize, key: &str) {
    with_tied_mut(name, level, |a, db| {
        if db.readonly || !ensure_open_for_write(db) {
            return;
        }
        let kb = zstr_to_bytes(key);
        let kd = Datum {
            dptr: kb.as_ptr() as *mut c_char,
            dsize: kb.len() as c_int,
        };
        unsafe {
            (a.delete)(db.dbf, kd);
            (a.sync)(db.dbf);
        }
        db.last_mtime = file_stamp(&db.path);
    });
}

pub fn on_replace_all(name: &str, level: usize, map: &BTreeMap<String, String>) {
    with_tied_mut(name, level, |a, db| {
        if db.readonly || !ensure_open_for_write(db) {
            return;
        }
        // Delete all existing keys first.
        let mut keys = Vec::new();
        let mut k = unsafe { (a.firstkey)(db.dbf) };
        while !k.dptr.is_null() {
            let kb = unsafe { std::slice::from_raw_parts(k.dptr as *const u8, k.dsize as usize) }
                .to_vec();
            keys.push(kb);
            let next = unsafe { (a.nextkey)(db.dbf, k) };
            unsafe { libc::free(k.dptr as *mut c_void) };
            k = next;
        }
        for kb in keys {
            let kd = Datum {
                dptr: kb.as_ptr() as *mut c_char,
                dsize: kb.len() as c_int,
            };
            unsafe { (a.delete)(db.dbf, kd) };
        }
        for (ks, vs) in map {
            let kb = zstr_to_bytes(ks);
            let vb = zstr_to_bytes(vs);
            let kd = Datum {
                dptr: kb.as_ptr() as *mut c_char,
                dsize: kb.len() as c_int,
            };
            let vd = Datum {
                dptr: vb.as_ptr() as *mut c_char,
                dsize: vb.len() as c_int,
            };
            unsafe { (a.store)(db.dbf, kd, vd, GDBM_REPLACE) };
        }
        unsafe { (a.sync)(db.dbf) };
        db.last_mtime = file_stamp(&db.path);
    });
}

pub fn on_append_all(name: &str, level: usize, map: &BTreeMap<String, String>) {
    with_tied_mut(name, level, |a, db| {
        if db.readonly || !ensure_open_for_write(db) {
            return;
        }
        for (ks, vs) in map {
            let kb = zstr_to_bytes(ks);
            let vb = zstr_to_bytes(vs);
            let kd = Datum {
                dptr: kb.as_ptr() as *mut c_char,
                dsize: kb.len() as c_int,
            };
            let vd = Datum {
                dptr: vb.as_ptr() as *mut c_char,
                dsize: vb.len() as c_int,
            };
            unsafe { (a.store)(db.dbf, kd, vd, GDBM_REPLACE) };
        }
        unsafe { (a.sync)(db.dbf) };
        db.last_mtime = file_stamp(&db.path);
    });
}

pub fn builtin_ztie(words: &[String]) -> i32 {
    let Some(a) = api() else {
        builtin_err("ztie", "gdbm library not available");
        return 1;
    };
    let mut readonly = false;
    let mut backend = String::new();
    let mut file = String::new();
    let mut i = 1;
    while let Some(arg) = words.get(i) {
        if arg == "--" {
            i += 1;
            break;
        }
        let Some(flags) = arg.strip_prefix('-').filter(|s| !s.is_empty()) else {
            break;
        };
        let mut chars = flags.chars();
        while let Some(c) = chars.next() {
            match c {
                'r' => readonly = true,
                'd' | 'f' => {
                    let rest: String = chars.collect();
                    let val = if !rest.is_empty() {
                        rest
                    } else {
                        i += 1;
                        match words.get(i) {
                            Some(v) => v.clone(),
                            None => {
                                builtin_err("ztie", &format!("argument expected after -{}", c));
                                return 1;
                            }
                        }
                    };
                    if c == 'd' {
                        backend = val;
                    } else {
                        file = val;
                    }
                    break;
                }
                other => {
                    builtin_err("ztie", &format!("bad option: -{}", other));
                    return 1;
                }
            }
        }
        i += 1;
    }
    if backend != "db/gdbm" {
        builtin_err("ztie", &format!("unknown database type: {}", backend));
        return 1;
    }
    if file.is_empty() {
        builtin_err("ztie", "missing -f filename");
        return 1;
    }
    let Some(raw_name) = words.get(i) else {
        builtin_err("ztie", "not enough arguments");
        return 1;
    };

    let (tname, level, slot) = resolve_target_binding(raw_name);
    // Untie existing tie on same target if any.
    let _ = close_tied_entry(&tname, level);

    let abs_path = if PathBuf::from(&file).is_absolute() {
        file.clone()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&file).to_string_lossy().into_owned())
            .unwrap_or_else(|_| file.clone())
    };
    let Some(dbf) = open_handle(&abs_path, readonly) else {
        builtin_err("ztie", &format!("error opening database file {}", file));
        return 1;
    };
    let map = read_all_from_dbf(a, dbf);
    let stamp = file_stamp(&abs_path);

    let mut p = Param::with(Value::Assoc(map));
    p.level = level;
    p.attrs = SPECIAL | if readonly { READONLY } else { 0 };
    p.attrs &= !UNSET;
    store_param_at(&tname, level, slot, p);

    TIED.with(|t| {
        t.borrow_mut().push(TiedDb {
            name: tname,
            level,
            path: abs_path,
            readonly,
            dbf,
            open_pid: std::process::id(),
            last_mtime: stamp,
        });
    });
    refresh_tied_param();
    0
}

pub fn builtin_zuntie(words: &[String]) -> i32 {
    let mut force_ro = false;
    let mut i = 1;
    while let Some(arg) = words.get(i) {
        if arg == "--" {
            i += 1;
            break;
        }
        let Some(flags) = arg.strip_prefix('-').filter(|s| !s.is_empty()) else {
            break;
        };
        for c in flags.chars() {
            match c {
                'u' => force_ro = true,
                other => {
                    builtin_err("zuntie", &format!("bad option: -{}", other));
                    return 1;
                }
            }
        }
        i += 1;
    }
    if i >= words.len() {
        builtin_err("zuntie", "not enough arguments");
        return 1;
    }
    let mut status = 0;
    for raw_name in &words[i..] {
        let (tname, level, slot) = resolve_target_binding(raw_name);
        let is_ro = TIED.with(|t| {
            t.borrow()
                .iter()
                .find(|d| d.name == tname && d.level == level)
                .map(|d| d.readonly)
        });
        match is_ro {
            None => {
                builtin_err(
                    "zuntie",
                    &format!("not a tied gdbm parameter: {}", raw_name),
                );
                status = 1;
            }
            Some(true) if !force_ro => {
                builtin_err("zuntie", &format!("read-only parameter: {}", raw_name));
                status = 1;
            }
            Some(_) => {
                let _ = close_tied_entry(&tname, level);
                remove_param_at(&tname, level, slot);
                refresh_tied_param();
            }
        }
    }
    status
}

pub fn builtin_zgdbmpath(words: &[String]) -> i32 {
    let Some(raw_name) = words.get(1) else {
        builtin_err("zgdbmpath", "not enough arguments");
        return 1;
    };
    let (tname, level, _) = resolve_target_binding(raw_name);
    let path = TIED.with(|t| {
        t.borrow()
            .iter()
            .find(|d| d.name == tname && d.level == level)
            .map(|d| d.path.clone())
    });
    match path {
        Some(p) => {
            var_set("REPLY", &p, false);
            0
        }
        None => {
            builtin_err(
                "zgdbmpath",
                &format!("not a tied gdbm parameter: {}", raw_name),
            );
            1
        }
    }
}

pub fn builtin_zgdbmclear(_words: &[String]) -> i32 {
    0
}
