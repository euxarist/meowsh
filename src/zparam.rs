//! zsh/parameter's special parameters ($builtins, $commands, $parameters,
//! ...). Each is computed when read and kept as an ordinary read-only
//! parameter, so subscripts, flags and `$+` see it like any other. A
//! parameter the user made with the same name hides it.

use crate::params::{Param, Value, READONLY, SPECIAL};
use crate::shell::with_shell;
use std::collections::BTreeMap;

const NAMES: &[&str] = &[
    "builtins",
    "commands",
    "parameters",
    "reswords",
    "nameddirs",
    "userdirs",
    "modules",
    "history",
    "historywords",
    "widgets",
    "keymaps",
    "errnos",
    "epochtime",
    "functions_source",
    "patchars",
];

/// Brings special parameter `name` up to date, if it is one.
pub fn ensure(name: &str) {
    if !NAMES.contains(&name) {
        return;
    }
    let shadowed = with_shell(|s| {
        s.param(name)
            .is_some_and(|p| p.attrs & SPECIAL == 0 || p.level > 0)
    });
    if shadowed {
        return;
    }
    // A module feature turned off (zmodload -F mod -p:name) takes it away.
    if !crate::modules::feature_available(&format!("p:{}", name)) {
        with_shell(|s| {
            s.remove_param(name);
        });
        return;
    }
    let value = match name {
        "builtins" => assoc(
            crate::builtins::builtin_names()
                .filter(|b| crate::builtins::is_builtin(b))
                .map(|b| (b.to_string(), "defined".to_string())),
        ),
        "commands" => {
            if crate::options::is_set("hashlistall") {
                for c in crate::shell::all_commands() {
                    crate::shell::lookup_command(&c);
                }
            }
            let map = with_shell(|s| {
                s.cmd_hash
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            });
            Value::Assoc(map)
        }
        "parameters" => {
            let names: Vec<String> = with_shell(|s| s.params.keys().cloned().collect());
            assoc(
                names
                    .into_iter()
                    .filter(|n| n != "parameters")
                    .map(|n| {
                        let is_ref = with_shell(|s| {
                            s.param(&n).is_some_and(|p| {
                                p.attrs & crate::params::NAMEREF != 0
                                    && p.attrs & crate::params::UNSET == 0
                                    && !p.text().is_empty()
                            })
                        });
                        let mut t = crate::expand::param_type(&n);
                        if is_ref && !t.is_empty() {
                            let _swap = crate::shell::RefSwap::enter(&n);
                            let target =
                                crate::shell::split_target_pub(&crate::shell::resolve_ref(&n));
                            let tt = crate::expand::param_type(&target);
                            if !tt.is_empty() {
                                t.push('-');
                                t.push_str(&tt);
                            }
                        }
                        (n, t)
                    })
                    .filter(|(_, t)| !t.is_empty()),
            )
        }
        "reswords" => Value::Array(crate::builtins::reserved_words()),
        "nameddirs" => {
            let m = crate::shell::NAMED_DIRS
                .lock()
                .map(|m| m.clone())
                .unwrap_or_default();
            Value::Assoc(m)
        }
        "userdirs" => assoc(user_dirs().into_iter()),
        "modules" => Value::Assoc(crate::modules::modules_param()),
        "history" => {
            let h = with_shell(|s| s.history.lock().map(|h| h.clone()).unwrap_or_default());
            assoc(
                h.into_iter()
                    .enumerate()
                    .map(|(i, l)| ((i + 1).to_string(), l)),
            )
        }
        "historywords" => {
            let h = with_shell(|s| s.history.lock().map(|h| h.clone()).unwrap_or_default());
            Value::Array(
                h.iter()
                    .rev()
                    .flat_map(|l| crate::expand::split_shell_words(l).into_iter().rev())
                    .collect(),
            )
        }
        "errnos" => Value::Array(crate::zsystem::errnos()),
        // The pattern characters `disable -p` can turn off.
        "patchars" => Value::Array(
            [
                "|", "~", "(", "?", "*", "[", "<", "^", "#", "?(", "*(", "+(", "!(", "!(", "@(",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        ),
        "functions_source" => assoc(
            with_shell(|s| {
                s.functions
                    .iter()
                    .map(|(n, f)| (n.clone(), f.origin.clone()))
                    .collect::<Vec<_>>()
            })
            .into_iter(),
        ),
        "epochtime" => {
            crate::zdatetime::epoch_param("epochtime").unwrap_or(Value::Array(Vec::new()))
        }
        "widgets" => assoc(crate::zle::thingy::names().into_iter().map(|n| {
            let kind = crate::zle::thingy::describe(&n);
            (n, kind)
        })),
        _ => Value::Array(crate::zle::keymap::with_keymaps(|t| t.names())),
    };
    with_shell(|s| {
        let mut p = Param::with(value);
        p.attrs |= READONLY | SPECIAL | crate::params::HIDE | crate::params::HIDEVAL;
        s.params.insert(name.to_string(), p);
    });
}

fn assoc(it: impl Iterator<Item = (String, String)>) -> Value {
    Value::Assoc(it.collect::<BTreeMap<_, _>>())
}

fn user_dirs() -> Vec<(String, String)> {
    let mut out = Vec::new();
    unsafe {
        libc::setpwent();
        loop {
            let pw = libc::getpwent();
            if pw.is_null() {
                break;
            }
            let name = std::ffi::CStr::from_ptr((*pw).pw_name)
                .to_string_lossy()
                .into_owned();
            let dir = std::ffi::CStr::from_ptr((*pw).pw_dir)
                .to_string_lossy()
                .into_owned();
            out.push((name, dir));
        }
        libc::endpwent();
    }
    out
}

/// Whether `$functions` is zsh/parameter's (not a parameter the user
/// made), so assigning to it defines functions.
pub fn functions_special() -> bool {
    with_shell(|s| s.param("functions").is_none())
}

/// `functions[name]=body`: defines function `name` from its body text.
pub fn set_function(name: &str, body: &str) -> bool {
    let quoted = format!("'{}'", name.replace('\'', "'\\''"));
    crate::exec::run_string(&format!("function {} {{\n{}\n}}", quoted, body)) == 0
}
