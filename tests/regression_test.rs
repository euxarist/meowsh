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

// Differences from zsh found by running the same scripts through both.

fn zsh_in(dir: &std::path::Path, script: &str) -> Out {
    run_mode(false, &["-c", script], "", Some(dir))
}

#[test]
fn startup_parameters_have_zsh_defaults() {
    let o = zsh(
        "print $signals[1,3] $TMPPREFIX $KEYTIMEOUT $LISTMAX $HISTCHARS $OPTIND $FUNCNEST \
         $TRY_BLOCK_ERROR $#patchars; print -r -- $WORDCHARS; print ${+ZSH_NAME}",
    );
    assert_eq!(
        o.stdout,
        "EXIT HUP INT /tmp/zsh 40 100 !^# 1 500 -1 15\n*?_-.[]~=/&;!#$%^(){}<>\n1\n"
    );
}

#[test]
fn type_flag_describes_special_parameters() {
    let o = zsh(
        "print ${(t)?} ${(t)argv} ${(t)IFS} ${(t)options} ${(t)OPTIND} ${(t)funcstack}; \
         f() { local IFS=x; print ${(t)IFS} }; f",
    );
    assert_eq!(
        o.stdout,
        "integer-readonly-special array-special scalar-special \
         association-hide-hideval-special integer-special \
         array-readonly-hide-hideval-special\nscalar-local-special\n"
    );
}

#[test]
fn lineno_is_read_only() {
    let o = zsh("LINENO=5; print reached");
    assert_eq!(o.stdout, "");
    assert!(
        o.stderr.contains("read-only variable: LINENO"),
        "{}",
        o.stderr
    );
}

#[test]
fn nested_command_substitution_splits_into_words() {
    let o = zsh(
        "print ${$(print a b c)[2]} ${#$(print a b c)} ${(j:-:)$(print a b c)}; \
         print \"[${$(print a b c)[2]}]\"; x=${$(print 'a   b')}; print -r -- $x",
    );
    assert_eq!(o.stdout, "b 3 a-b-c\n[ ]\na b\n");
    // A non-blank separator keeps the empty words, as plain $(...) does.
    let o = zsh(
        "IFS=:; print ${#$(print a::b)} ${#$(print a:b:)} ${#$(print :a)}; \
         IFS=' :'; print ${#$(print 'a : b::c')}",
    );
    assert_eq!(o.stdout, "3 3 2\n4\n");
}

#[test]
fn zerr_trap_sees_the_failing_status() {
    let o = zsh("trap 'print err $?' ZERR; false; (exit 3); f() { return 4 }; f");
    assert_eq!(o.stdout, "err 1\nerr 3\nerr 4\n");
}

#[test]
fn err_return_ends_a_script_at_top_level() {
    let o = zsh("setopt errreturn; false; print after");
    assert_eq!(o.stdout, "");
    assert_eq!(o.status, Some(1));
}

#[test]
fn return_outside_a_function_leaves_the_shell() {
    let o = zsh("(return 3); print $?; return 7; print notreached");
    assert_eq!(o.stdout, "3\n");
    assert_eq!(o.status, Some(7));
    assert_eq!(o.stderr, "");
}

#[test]
fn exit_trap_context_names_where_the_shell_ended() {
    let trap = "trap 'print $ZSH_EVAL_CONTEXT' EXIT";
    // -c keeps its frame whether the string runs out or calls exit.
    assert_eq!(zsh(trap).stdout, "cmdarg:trap\n");
    assert_eq!(zsh(&format!("{}; exit", trap)).stdout, "cmdarg:trap\n");
    let tmp = TempDir::new().unwrap();
    let script = |body: &str| {
        let path = tmp.path().join("s");
        std::fs::write(&path, body).unwrap();
        run_mode(false, &[path.to_str().unwrap()], "", None).stdout
    };
    // A script that runs out has left the top level; one that exits has not.
    assert_eq!(script(&format!("{}\n", trap)), "trap\n");
    assert_eq!(script(&format!("{}\nexit\n", trap)), "toplevel:trap\n");
    // Standard input behaves like a script.
    assert_eq!(run_mode(false, &[], trap, None).stdout, "trap\n");
}

#[test]
fn integer_to_negative_power_is_real() {
    let o = zsh("print $(( 2 ** -2 )) $(( 2 ** 3 ))");
    assert_eq!(o.stdout, "0.25 8\n");
}

#[test]
fn floats_print_seventeen_significant_digits() {
    let o = zsh("print $(( 3.7 )) $(( 0.1 )) $(( 2.5 )) $(( 1e21 )) $(( 1.5e300 ))");
    assert_eq!(
        o.stdout,
        "3.7000000000000002 0.10000000000000001 2.5 1e+21 1.5000000000000001e+300\n"
    );
}

#[test]
fn oversized_integer_is_truncated_with_a_warning() {
    let o = zsh("print $(( 9223372036854775808 )) $(( 0xffffffffffffffff ))");
    assert_eq!(o.stdout, "922337203685477580 1152921504606846975\n");
    assert!(
        o.stderr.contains("number truncated after 18 digits"),
        "{}",
        o.stderr
    );
    assert!(
        o.stderr.contains("number truncated after 15 digits"),
        "{}",
        o.stderr
    );
}

#[test]
fn sort_flags_follow_zsh() {
    let o = zsh(
        "LC_ALL=C; a=(c B a 10 9 A b); print ${(o)a}; print ${(i)a}; print ${(Oi)a}; \
         print ${(n)a}; print ${(a)a}; print ${(Oa)a}",
    );
    assert_eq!(
        o.stdout,
        "10 9 A B a b c\n10 9 a A B b c\nc B b a A 9 10\n9 10 A B a b c\n\
         c B a 10 9 A b\nb A 9 10 a B c\n"
    );
}

#[test]
fn wait_for_a_pid_after_a_bare_wait() {
    let o = zsh("{ sleep 0.05; exit 2 } & pid=$!; wait; wait $pid; print $?");
    assert_eq!(o.stdout, "2\n");
    assert_eq!(o.stderr, "");
}

#[test]
fn key_and_value_flags_on_searched_hash_subscripts() {
    let o = zsh(
        "typeset -A h=(a 1 b 2 c 3); print ${(k)h[(I)[ab]]} / ${(v)h[(I)[ab]]} / \
         ${(kv)h[(I)[ab]]} / ${(kv)h[(R)[12]]} / ${(kv)h[(i)a]} / ${(k)h[(r)2]} / \
         ${(v)h[(i)a]}; arr=(apple banana cherry); print ${(k)arr[(r)b*]} ${arr[(r)b*]}",
    );
    assert_eq!(
        o.stdout,
        "a b / 1 2 / a 1 b 2 / a 1 b 2 / a 1 / b / 1\n2 banana\n"
    );
}

#[test]
fn length_of_a_default_word() {
    let o = zsh("set -- a b; print ${#:-abc} ${#:-} ${#} ${##}");
    assert_eq!(o.stdout, "3 0 2 1\n");
}

#[test]
fn options_hash_takes_any_spelling() {
    let o = zsh(
        "setopt NO_GLOB; print $options[glob] $options[noglob] $options[NO_GLOB]; \
         setopt glob; setopt -m 'hist*dups'; print $options[histignoredups] $options[histsavenodups]",
    );
    assert_eq!(o.stdout, "off on on\non on\n");
    assert_eq!(o.stderr, "");
}

#[test]
fn alias_listing_covers_global_aliases() {
    let o = zsh(
        "unalias -a; alias -g G=global; alias r=regular; alias -s x=suf; alias; alias -L; \
         alias +; alias -g; alias -s",
    );
    assert_eq!(
        o.stdout,
        "G=global\nr=regular\nalias -g G=global\nalias r=regular\nG\nr\nG=global\nx=suf\n"
    );
    assert_eq!(o.stderr, "");
}

#[test]
fn glob_qualifiers_see_links_unless_followed() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let tmp = TempDir::new().unwrap();
    let d = tmp.path();
    for (name, mode) in [("a", 0o600), ("b", 0o755)] {
        std::fs::write(d.join(name), "x").unwrap();
        std::fs::set_permissions(d.join(name), std::fs::Permissions::from_mode(mode)).unwrap();
    }
    symlink("a", d.join("l")).unwrap();
    symlink("missing", d.join("bl")).unwrap();
    let o = zsh_in(
        d,
        "print *(.); print *(-.); print *(-@); print *(@x); print *(.L1) / *(l1)",
    );
    assert_eq!(o.stdout, "a b\na b l\nbl\nbl l\na b / a b bl l\n");
}

#[test]
fn word_split_flag_keeps_empty_fields() {
    let o = zsh(
        "IFS=:; x='a::b:'; set -- ${=x}; print $#; y=':a'; set -- ${=y}; print $#; \
         IFS=' :'; z='a : b::c'; set -- ${=z}; print $#",
    );
    assert_eq!(o.stdout, "4\n2\n4\n");
}

#[test]
fn printf_assigns_to_an_element() {
    let o = zsh("printf -v 'arr[2]' x; print $#arr; typeset -A h; printf -v 'h[k]' z; print $h[k]");
    assert_eq!(o.stdout, "2\nz\n");
}

#[test]
fn array_value_for_a_scalar_element_is_an_error() {
    let o = zsh("s=abc; s[1]=(); print $s");
    assert_eq!(o.stdout, "");
    assert!(
        o.stderr
            .contains("attempt to assign array value to non-array"),
        "{}",
        o.stderr
    );
}

#[test]
fn autoload_x_outside_a_function_is_refused() {
    let tmp = TempDir::new().unwrap();
    let script = tmp.path().join("s");
    std::fs::write(&script, "autoload -X\n").unwrap();
    let o = run_mode(false, &[script.to_str().unwrap()], "", None);
    assert_eq!(o.signal, None);
    assert_eq!(o.status, Some(1));
    assert!(o.stderr.contains("bad option(s)"), "{}", o.stderr);
}

#[test]
fn autoload_k_runs_the_file_then_the_function() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("af"),
        "af() { print inner } ; print outer\n",
    )
    .unwrap();
    let o = zsh_in(
        tmp.path(),
        "fpath=(.); autoload -Uk af; functions af; af; af",
    );
    assert_eq!(
        o.stdout,
        "af () {\n\t# undefined\n\tbuiltin autoload -XUk\n}\nouter\ninner\ninner\n"
    );
}

#[test]
fn read_keys_need_a_terminal_or_a_descriptor() {
    let o = zsh("read -u0 -k 3 k <<<abcdef; print \"[$k]\"");
    assert_eq!(o.stdout, "[abc]\n");
}

#[test]
fn cd_rejects_three_arguments() {
    let o = zsh("cd /etc /usr /x");
    assert_eq!(o.status, Some(1));
    assert!(o.stderr.contains("too many arguments"), "{}", o.stderr);
}

#[test]
fn empty_pattern_replaces_before_each_character_only() {
    let o = zsh("x=abc; print ${x///X}");
    assert_eq!(o.stdout, "XaXbXc\n");
}

#[test]
fn brace_range_of_non_ascii_letters() {
    let o = zsh("LC_ALL=en_US.UTF-8; print {é..ë}");
    assert_eq!(o.stdout, "é ê ë\n");
}

#[test]
fn tilde_number_names_a_directory_stack_entry() {
    // With single-byte characters it is not a range of two characters.
    let o = zsh("LC_ALL=C; print {é..ë} {a..c}");
    assert_eq!(o.stdout, "{é..ë} a b c\n");
    let o = zsh("cd /tmp; pushd /usr >/dev/null; pushd /etc >/dev/null; \
         print ~0 ~1 ~2 ~+1 ~-0 ~-1 ~1/x; print ~9");
    assert_eq!(o.stdout, "/etc /usr /tmp /usr /tmp /usr /usr/x\n");
    assert!(
        o.stderr.contains("not enough directory stack entries."),
        "{}",
        o.stderr
    );
}

#[test]
fn print_columns_without_a_terminal_is_one_column() {
    let o = zsh("unset COLUMNS; print -c a b c");
    assert_eq!(o.stdout, "a\nb\nc\n");
}
