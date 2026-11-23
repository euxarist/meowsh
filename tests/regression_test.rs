//! Regression tests for the behavior fixed by the core rewrite. Each test
//! runs the shell non-interactively with a deadline, so a regression that
//! hangs fails instead of stalling the suite.

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Out {
    status: Option<i32>,
    signal: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_mode(emulate_sh: bool, args: &[&str], stdin: &str, cwd: Option<&std::path::Path>) -> Out {
    let home = TempDir::new().expect("tempdir for HOME");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_meowsh"));
    // Own process group, so a timeout can kill forked children that would
    // otherwise keep the output pipes open.
    cmd.process_group(0).env("HOME", home.path());
    if emulate_sh {
        // These are POSIX behaviors; zsh mode differs (no word splitting
        // of parameters, no-match errors).
        cmd.args(["--emulate", "sh"]);
    }
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().expect("spawn meowsh");
    let mut input = child.stdin.take().unwrap();
    let data = stdin.as_bytes().to_vec();
    let writer = std::thread::spawn(move || {
        let _ = input.write_all(&data);
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            break;
        }
        if Instant::now() > deadline {
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().expect("wait meowsh");
    let _ = writer.join();
    use std::os::unix::process::ExitStatusExt;
    Out {
        status: output.status.code(),
        signal: output.status.signal(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

fn sh(script: &str) -> Out {
    run(&["-c", script], "", None)
}

fn piped(script: &str) -> Out {
    run(&[], script, None)
}

// Crashes

#[test]
fn non_ascii_input_does_not_crash() {
    let o = sh("echo é 'ü' \"ß\"; echo -é");
    assert_eq!(o.status, Some(0), "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "é ü ß\n-é\n");
}

#[test]
fn arithmetic_overflow_does_not_crash() {
    let o = sh("echo $((9223372036854775807 + 1)) $((-9223372036854775807 - 1))");
    assert_eq!(o.signal, None);
    assert_eq!(o.status, Some(0), "stderr: {}", o.stderr);
}

#[test]
fn arithmetic_division_by_zero_is_an_error() {
    let o = sh("echo $((1 / 0)); echo after");
    assert_eq!(o.signal, None);
    assert_ne!(o.status, Some(0));
    assert!(!o.stdout.contains("after"));
}

// Exit status

#[test]
fn command_string_exit_status_propagates() {
    assert_eq!(sh("false").status, Some(1));
    assert_eq!(sh("exit 7").status, Some(7));
    assert_eq!(sh("false; exit").status, Some(1));
}

#[test]
fn bang_negates_pipeline_status() {
    let o = sh("! true; echo $?; ! false; echo $?");
    assert_eq!(o.stdout, "1\n0\n");
}

#[test]
fn return_stops_the_function() {
    let o = sh("f() { echo a; return 3; echo b; }; f; echo $?");
    assert_eq!(o.stdout, "a\n3\n");
}

#[test]
fn not_found_and_not_executable_statuses() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("plain");
    std::fs::write(&file, "echo hi\n").unwrap();
    let o = sh(&format!(
        "no_such_command_xyz; echo $?; {}; echo $?",
        file.display()
    ));
    assert_eq!(o.stdout, "127\n126\n");
}

#[test]
fn piped_exit_does_not_print_logout() {
    let o = piped("echo hi\nexit 4\n");
    assert_eq!(o.status, Some(4));
    assert_eq!(o.stdout, "hi\n");
}

// Reading scripts

#[test]
fn piped_script_runs_multiline_compound_commands() {
    let o = piped("for i in 1 2; do\n  echo $i\ndone\nif true\nthen\n  echo yes\nfi\n");
    assert_eq!(o.stdout, "1\n2\nyes\n");
}

#[test]
fn piped_script_heredoc_body_is_data() {
    let o = piped("cat <<EOF\necho not run\nEOF\necho after\n");
    assert_eq!(o.stdout, "echo not run\nafter\n");
}

#[test]
fn read_shares_stdin_with_piped_script() {
    let o = piped("read line\nthis is data\necho \"got: $line\"\n");
    assert_eq!(o.stdout, "got: this is data\n");
}

#[test]
fn alias_applies_to_later_commands_in_same_string() {
    let o = sh("alias greet='echo hello'\ngreet world");
    assert_eq!(o.stdout, "hello world\n");
}

#[test]
fn script_file_gets_positional_parameters() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("s.sh");
    std::fs::write(&file, "echo \"$#:$1:$2\"\n").unwrap();
    let o = run(&[file.to_str().unwrap(), "a b", "c"], "", None);
    assert_eq!(o.stdout, "2:a b:c\n");
}

#[test]
fn large_heredoc_does_not_deadlock() {
    let body = "x".repeat(199) + "\n";
    let script = format!("wc -c <<EOF\n{}EOF\n", body.repeat(1000));
    let o = piped(&script);
    assert_eq!(o.stdout.trim(), "200000");
}

// Expansion

#[test]
fn unquoted_expansion_is_field_split() {
    let o = sh("x='a b  c'; for w in $x; do echo \"[$w]\"; done");
    assert_eq!(o.stdout, "[a]\n[b]\n[c]\n");
}

#[test]
fn ifs_non_whitespace_delimiters_keep_empty_fields() {
    let o = sh("IFS=:; x='a::b'; set -- $x; echo $#");
    assert_eq!(o.stdout, "3\n");
}

#[test]
fn quoted_at_keeps_each_parameter() {
    let o = sh("set -- 'a b' '' c; for w in \"$@\"; do echo \"[$w]\"; done; echo \"$*\"");
    assert_eq!(o.stdout, "[a b]\n[]\n[c]\na b  c\n");
}

#[test]
fn quoted_glob_and_tilde_stay_literal() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("foo"), "").unwrap();
    let o = run(
        &["-c", "echo \"f*\" 'f*' f*; echo \"~\" '~'"],
        "",
        Some(dir.path()),
    );
    assert_eq!(o.stdout, "f* f* foo\n~ ~\n");
}

#[test]
fn dash_operator_distinguishes_empty_from_unset() {
    let o = sh("e=; echo \"[${e-def}] [${u-def}] [${e:-def}]\"");
    assert_eq!(o.stdout, "[] [def] [def]\n");
}

#[test]
fn arithmetic_precedence_and_associativity() {
    let o = sh("echo $((10-2-3)) $((2+3*4)) $((2**3**2)) $(( (1+2)*3 ))");
    assert_eq!(o.stdout, "5 14 512 9\n");
}

#[test]
fn command_substitution_strips_trailing_newlines_only() {
    let o = sh("x=$(printf 'a\\n\\nb\\n\\n'); echo \"[$x]\"");
    assert_eq!(o.stdout, "[a\n\nb]\n");
}

// Commands

#[test]
fn and_or_lists_are_left_associative() {
    let o = sh("false && echo a || echo b; true || echo c && echo d");
    assert_eq!(o.stdout, "b\nd\n");
}

#[test]
fn parentheses_run_in_a_subshell() {
    let o = sh("x=1; (x=2; cd /; exit 3); echo $? $x; pwd");
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(o.stdout, format!("3 1\n{}\n", cwd.display()));
}

#[test]
fn redirections_apply_to_functions_and_groups() {
    let dir = TempDir::new().unwrap();
    let o = run(
        &[
            "-c",
            "f() { echo fn; } > f.out; f; { echo grp; } > g.out; cat f.out g.out",
        ],
        "",
        Some(dir.path()),
    );
    assert_eq!(o.stdout, "fn\ngrp\n");
}

#[test]
fn read_returns_failure_at_end_of_input() {
    let o = sh("printf 'a\\nb' | { while read l; do echo \"$l\"; done; echo \"end $l\"; }");
    assert_eq!(o.stdout, "a\nend b\n");
    let o = sh("read x < /dev/null; echo $?");
    assert_eq!(o.stdout, "1\n");
}

#[test]
fn only_exported_variables_reach_children() {
    let o = sh("x=1; export y=2; sh -c 'echo \"${x-unset} ${y-unset}\"'");
    assert_eq!(o.stdout, "unset 2\n");
}

#[test]
fn case_alternatives_and_fallthrough() {
    let o = sh("case b in a|b) echo ab;& c) echo c;; d) echo d;; esac");
    assert_eq!(o.stdout, "ab\nc\n");
}

#[test]
fn errexit_exceptions() {
    let o = sh("set -e; if false; then :; fi; false || true; ! true; false && true; echo alive; false; echo dead");
    assert_eq!(o.stdout, "alive\n");
    assert_eq!(o.status, Some(1));
}

#[test]
fn exec_and_command_builtins() {
    let o = sh("command -v echo; command echo hi; exec echo replaced; echo not reached");
    assert_eq!(o.stdout, "echo\nhi\nreplaced\n");
}

#[test]
fn wait_for_background_pid_returns_its_status() {
    let o = sh("(exit 5) & wait $!; echo $?");
    assert_eq!(o.stdout, "5\n");
}

// Signals

#[test]
fn exit_trap_runs() {
    let o = sh("trap 'echo bye' EXIT; echo hi");
    assert_eq!(o.stdout, "hi\nbye\n");
}

#[test]
fn script_dies_on_sigterm_and_sigint() {
    for sig in ["TERM", "INT"] {
        let o = sh(&format!("kill -{} $$; echo survived", sig));
        assert!(!o.stdout.contains("survived"), "{} was ignored", sig);
        assert!(o.signal.is_some(), "{} did not kill the shell", sig);
    }
}

fn run(args: &[&str], stdin: &str, cwd: Option<&std::path::Path>) -> Out {
    run_mode(true, args, stdin, cwd)
}

fn zsh(script: &str) -> Out {
    run_mode(false, &["-c", script], "", None)
}

#[test]
fn trapped_signal_runs_action() {
    let o = sh("trap 'echo caught' USR1; kill -USR1 $$; echo after");
    assert_eq!(o.stdout, "caught\nafter\n");
}

#[test]
fn subshell_in_trap_can_run_traps() {
    let o = zsh(
        r#"zmodload zsh/system; trap '( trap "echo sub" USR1; kill -USR1 $sysparams[pid] )' USR2; kill -USR2 $$"#,
    );
    assert_eq!(o.stdout, "sub\n");
}

// Critical bug regressions

#[test]
fn invalid_typeset_base_ulimit_and_cd_zero_do_not_panic() {
    let o = zsh("typeset -i 1 x=5; echo $x; typeset -i 37 y=5; echo $y");
    assert!(o.signal.is_none());
    // zsh rejects the base and leaves the parameter unset.
    assert_eq!(o.stdout, "\n\n");

    let o = zsh("ulimit -N 999; echo $?");
    assert!(o.signal.is_none());
    assert_eq!(o.stdout, "1\n");

    let o = zsh("cd +0; cd -0; echo ok");
    assert!(o.signal.is_none());
    assert_eq!(o.stdout, "ok\n");
}

#[test]
fn expansion_and_arith_edge_cases_do_not_panic_or_loop() {
    let o = zsh("s=abc; echo ${s:1:9223372036854775807}");
    assert_eq!(o.status, Some(0));
    assert_eq!(o.stdout, "bc\n");

    let o = zsh("s=abc; echo ${s[(n:-9223372036854775808)r]a}");
    assert!(o.signal.is_none());

    let o = zsh("s=abc; echo ${s[(n:-9223372036854775808)ra]}");
    assert!(o.signal.is_none());

    let o = zsh("x=y; y=1; echo ${${(P)x}[}");
    assert!(o.signal.is_none());
    assert_ne!(o.status, Some(0));

    let o = zsh("a='$a'; echo $(( [$a] 1 ))");
    assert!(o.signal.is_none());

    let o = zsh("echo ${999999999999999:=x}");
    assert!(o.signal.is_none());
    assert_ne!(o.status, Some(0));

    let o = zsh("x=a; : ${x:fs/a/aa/}; echo ok");
    assert_eq!(o.status, Some(0));
    assert_eq!(o.stdout, "ok\n");

    let o = zsh("echo $(( (-9223372036854775807 - 1) ** 65 ))");
    assert_eq!(o.status, Some(0));
    assert_eq!(o.stdout, "0\n");
}

#[test]
fn pattern_repeat_large_counts_do_not_overflow_or_hang() {
    let o = zsh(r#"setopt extendedglob; [[ a == a(#c18446744073709551615) ]]; echo $?"#);
    assert_eq!(o.status, Some(0));
    assert_eq!(o.stdout, "1\n");

    let o = zsh(r#"setopt extendedglob; [[ "" == (a|)(#c1000000000000) ]]; echo $?"#);
    assert_eq!(o.status, Some(0));
    assert_eq!(o.stdout, "0\n");
}

#[test]
fn eval_recursion_is_bounded() {
    let o = zsh(r#"x='eval "$x"'; eval "$x"; echo survived"#);
    assert!(o.signal.is_none());
    assert!(o.stdout.contains("survived"));
}

#[test]
fn closed_stdio_fds_work_in_pipelines_and_both_redir() {
    let o = zsh("exec 0<&-; echo hi | cat");
    assert_eq!(o.status, Some(0));
    assert_eq!(o.stdout, "hi\n");

    let tmp = tempfile::TempDir::new().unwrap();
    let out = tmp.path().join("both.txt");
    let script = format!("exec 2>&-; echo hello >&{}", out.display());
    let o = zsh(&script);
    assert_eq!(o.status, Some(0));
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "hello\n");
}

#[test]
fn multios_on_high_fd_does_not_deadlock() {
    let tmp = tempfile::TempDir::new().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    let script = format!(
        "/usr/bin/true 10>{} 10>{}; echo done",
        a.display(),
        b.display()
    );
    let o = zsh(&script);
    assert_eq!(o.status, Some(0));
    assert_eq!(o.stdout, "done\n");
}

#[test]
fn zpty_child_does_not_run_parent_exit_trap() {
    let tmp = tempfile::TempDir::new().unwrap();
    let log = tmp.path().join("exit.log");
    let script = format!(
        "zmodload zsh/zpty; trap 'echo EXIT >> {}' EXIT; zpty p 'true'; zpty -w p ''; zpty -d p",
        log.display()
    );
    let o = zsh(&script);
    assert_eq!(o.status, Some(0));
    let lines: Vec<_> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(lines, vec!["EXIT"]);
}

#[test]
fn systell_rejects_fd_above_i32_max() {
    let o = zsh("zmodload zsh/system; echo $(( systell(4294967296) ))");
    assert!(o.signal.is_none());
    assert_ne!(o.status, Some(0));
}
