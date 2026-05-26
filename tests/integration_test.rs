use std::env;
use std::fs;
use std::io::{Read, Write};
use std::process::Command;
use tempfile::TempDir;

fn run_script(script: &str) -> (i32, String, String) {
    // Point HOME at a fresh empty tempdir so the user's real ~/.zshrc /
    // ~/.zshenv don't get sourced into the test environment.
    let home = TempDir::new().expect("tempdir for HOME");
    let child = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env("HOME", home.path())
        .arg("-i")
        .arg("-c")
        .arg(script)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn meowsh");

    let output = child.wait_with_output().expect("Failed to read output");
    (
        output.status.code().unwrap_or(127),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
fn test_native_pipes() {
    let (status, stdout, _) = run_script("echo 'hello world' | grep 'hello' | wc -w | tr -d ' '");
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "2"); // 'hello world' has 2 words
}

#[test]
fn test_file_operators() {
    let temp_dir = TempDir::new().unwrap();
    let dir_path = temp_dir.path().join("mydir");
    let file_path = temp_dir.path().join("myfile.txt");

    fs::create_dir(&dir_path).unwrap();
    fs::File::create(&file_path).unwrap();

    let script = format!(
        r#"
        if [ -d "{}" ]; then echo "is_dir"; fi
        if [ -f "{}" ]; then echo "is_file"; fi
        if [ -e "{}" ]; then echo "exists"; fi
        if [ -s "{}" ]; then echo "not_empty"; else echo "empty"; fi
        "#,
        dir_path.display(),
        file_path.display(),
        file_path.display(),
        file_path.display()
    );

    let (status, stdout, _) = run_script(&script);
    assert_eq!(status, 0);
    assert!(stdout.contains("is_dir"));
    assert!(stdout.contains("is_file"));
    assert!(stdout.contains("exists"));
    assert!(stdout.contains("empty"));
}

#[test]
fn test_shell_option_errexit() {
    let script = r#"
        set -e
        echo "start"
        false
        echo "end"
    "#;
    let (status, stdout, _) = run_script(script);
    assert_ne!(status, 0);
    assert!(stdout.contains("start"));
    assert!(!stdout.contains("end"));
}

#[test]
fn test_shell_option_nounset() {
    let script = r#"
        set -u
        echo "value: $UNDEFINED_VAR"
        echo "end"
    "#;
    let (status, stdout, stderr) = run_script(script);
    assert_ne!(status, 0);
    assert!(!stdout.contains("end"));
    assert!(stderr.contains("unbound variable"));
}

#[test]
fn test_shell_option_xtrace() {
    let script = r#"
        set -x
        echo "trace this"
    "#;
    let (status, stdout, stderr) = run_script(script);
    assert_eq!(status, 0);
    assert!(stdout.contains("trace this"));
    assert!(stderr.contains("+ echo trace this"));
}

#[test]
fn test_here_documents() {
    let script = r#"
cat << EOF
line1
line2
EOF
"#;
    let (status, stdout, _) = run_script(script);
    assert_eq!(status, 0);
    assert!(stdout.contains("line1"));
    assert!(stdout.contains("line2"));
}

#[test]
fn test_redirection_restores_stdout() {
    let temp_dir = TempDir::new().unwrap();
    let out_path = temp_dir.path().join("out.txt");
    let script = format!(
        r#"
        echo redirected > "{}"
        echo visible
        "#,
        out_path.display()
    );

    let (status, stdout, _) = run_script(&script);
    assert_eq!(status, 0);
    assert_eq!(fs::read_to_string(out_path).unwrap().trim(), "redirected");
    assert_eq!(stdout.trim(), "visible");
}

#[test]
fn test_comments_are_ignored_after_whitespace() {
    let script = r#"
        # startup-style comment
        echo before # trailing comment
        echo foo#bar
    "#;

    let (status, stdout, stderr) = run_script(script);
    assert_eq!(status, 0);
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        vec!["before", "foo#bar"]
    );
    assert!(!stderr.contains("command not found"));
}

#[test]
fn test_stderr_redirection_uses_requested_fd() {
    let temp_dir = TempDir::new().unwrap();
    let err_path = temp_dir.path().join("err.txt");
    let script = format!(r#"/bin/sh -c 'echo err >&2' 2> "{}""#, err_path.display());
    let (status, stdout, stderr) = run_script(&script);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "");
    assert_eq!(stderr.trim(), "");
    assert_eq!(fs::read_to_string(err_path).unwrap().trim(), "err");
}

#[test]
fn test_fd_duplication_redirection() {
    let (status, stdout, stderr) = run_script(r#"/bin/sh -c 'echo err >&2' 2>&1"#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "err");
    assert_eq!(stderr.trim(), "");
}

#[test]
fn test_parameter_expansion_remove_suffix() {
    let script = r#"
        set -f
        file="example.tar.gz"
        echo "${file%.*}"
        echo "${file%%.*}"
    "#;
    let (status, stdout, _) = run_script(script);
    assert_eq!(status, 0);
    let mut lines = stdout.lines();
    assert_eq!(lines.next().unwrap(), "example.tar");
    assert_eq!(lines.next().unwrap(), "example");
}

#[test]
fn test_parameter_expansion_remove_prefix() {
    let script = r#"
        set -f
        path="/usr/local/bin"
        echo "${path#*/}"
        echo "${path##*/}"
    "#;
    let (status, stdout, _) = run_script(script);
    assert_eq!(status, 0);
    let mut lines = stdout.lines();
    assert_eq!(lines.next().unwrap(), "usr/local/bin");
    assert_eq!(lines.next().unwrap(), "bin");
}

#[test]
fn test_history_builtin() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .arg("-i")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn meowsh");

    let mut stdin = child.stdin.take().expect("Failed to open stdin");
    writeln!(stdin, "echo first").unwrap();
    writeln!(stdin, "echo second").unwrap();
    writeln!(stdin, "history").unwrap();
    writeln!(stdin, "exit").unwrap();
    drop(stdin);

    let mut stdout = String::new();
    child.stdout.unwrap().read_to_string(&mut stdout).unwrap();

    assert!(stdout.contains("first"));
    assert!(stdout.contains("second"));
    assert!(stdout.contains("history"));
}

#[test]
fn test_recursive_globbing() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn meowsh");

    let mut stdin = child.stdin.take().expect("Failed to open stdin");
    writeln!(stdin, "echo src/**/*.rs").unwrap();
    writeln!(stdin, "exit").unwrap();
    drop(stdin);

    let mut stdout = String::new();
    child.stdout.unwrap().read_to_string(&mut stdout).unwrap();

    assert!(stdout.contains("src/main.rs"));
    assert!(stdout.contains("src/lib.rs"));
    assert!(stdout.contains("src/expand.rs"));
}

#[test]
fn test_brace_expansion() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn meowsh");

    let mut stdin = child.stdin.take().expect("Failed to open stdin");
    writeln!(stdin, "echo {{a,b,c}}").unwrap();
    writeln!(stdin, "echo file_{{1,2}}.txt").unwrap();
    writeln!(stdin, "exit").unwrap();
    drop(stdin);

    let mut stdout = String::new();
    child.stdout.unwrap().read_to_string(&mut stdout).unwrap();

    assert!(stdout.contains("a b c"));
    assert!(stdout.contains("file_1.txt file_2.txt"));
}

#[test]
fn test_brace_expansion_advanced() {
    let (status, stdout, _) = run_script("echo {a,b}_{1,2}");
    assert_eq!(status, 0);
    assert!(stdout.contains("a_1 a_2 b_1 b_2"));

    let (status, stdout, _) = run_script("echo {a,{b,c}}");
    assert_eq!(status, 0);
    assert!(stdout.contains("a b c"));

    let (status, stdout, _) = run_script("echo {a}");
    assert_eq!(status, 0);
    assert!(stdout.trim() == "{a}");
}

#[test]
fn test_glob_no_match() {
    let (status, stdout, _) = run_script("echo non_existent_file_*.txt");
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "non_existent_file_*.txt");
}

#[test]
fn test_history_limit() {
    let mut script = String::new();
    for i in 0..2000 {
        script.push_str(&format!("echo cmd{}\n", i));
    }
    script.push_str("history\n");

    let (status, stdout, _) = run_script(&script);
    assert_eq!(status, 0);
    // cmd0 should have been rotated out as we have 2000 commands and limit is 1000
    assert!(!stdout.contains("cmd0 "));
    // cmd1999 should be there
    assert!(stdout.contains("cmd1999"));
}

// ---- Phase 1: typical .zshrc compatibility -------------------------------

#[test]
fn test_dbracket_string_equal() {
    let (status, stdout, _) = run_script(r#"[[ "abc" = "abc" ]] && echo yes"#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "yes");
}

#[test]
fn test_dbracket_string_not_equal() {
    let (status, stdout, _) = run_script(r#"[[ "abc" != "xyz" ]] && echo yes"#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "yes");
}

#[test]
fn test_dbracket_unary_n() {
    let (status, stdout, _) = run_script(r#"x=hello; [[ -n "$x" ]] && echo nonempty"#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "nonempty");
}

#[test]
fn test_dbracket_unary_z() {
    let (status, stdout, _) = run_script(r#"x=; [[ -z "$x" ]] && echo empty"#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "empty");
}

#[test]
fn test_dbracket_arith_gt() {
    let (status, stdout, _) = run_script(r#"[[ 5 -gt 3 ]] && echo yes"#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "yes");
}

#[test]
fn test_dbracket_logical_and() {
    let (status, stdout, _) = run_script(r#"x=1; [[ -n "$x" && 1 -lt 5 ]] && echo yes"#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "yes");
}

#[test]
fn test_dbracket_logical_or() {
    let (status, stdout, _) = run_script(r#"[[ -z foo || -n bar ]] && echo yes"#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "yes");
}

#[test]
fn test_dbracket_glob_pattern() {
    let (status, stdout, _) = run_script(r#"v=screen.xterm; [[ "$v" == screen* ]] && echo match"#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "match");
}

#[test]
fn test_posix_function_def() {
    let (status, stdout, _) = run_script(r#"greet() { echo hi $1; }; greet world"#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "hi world");
}

#[test]
fn test_posix_function_multiline() {
    let (status, stdout, _) = run_script("greet() {\n  echo line1\n  echo line2\n}\ngreet");
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "line1\nline2");
}

#[test]
fn test_setopt_errexit_named() {
    let (status, stdout, _) = run_script("setopt errexit; false; echo unreached");
    assert_ne!(status, 0);
    assert!(!stdout.contains("unreached"));
}

#[test]
fn test_setopt_unknown_does_not_error() {
    let (status, stdout, _) = run_script("setopt extendedglob nomatch hist_ignore_dups; echo done");
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "done");
}

#[test]
fn test_unsetopt_clears_legacy_flag() {
    // setopt errexit then unsetopt — `false` should NOT cause exit
    let (status, stdout, _) = run_script("setopt errexit; unsetopt errexit; false; echo reached");
    assert_eq!(status, 0);
    assert!(stdout.contains("reached"));
}

#[test]
fn test_zshenv_sourced_always() {
    let temp_dir = TempDir::new().unwrap();
    fs::write(temp_dir.path().join(".zshenv"), "FOO=zshenv_value\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env("HOME", temp_dir.path())
        .arg("-c")
        .arg("echo $FOO")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("Failed to spawn meowsh");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_eq!(stdout.trim(), "zshenv_value");
}

#[test]
fn test_zshrc_sourced_when_interactive() {
    let temp_dir = TempDir::new().unwrap();
    fs::write(temp_dir.path().join(".zshrc"), "BAR=zshrc_value\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env("HOME", temp_dir.path())
        .arg("-i")
        .arg("-c")
        .arg("echo $BAR")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("Failed to spawn meowsh");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_eq!(stdout.trim(), "zshrc_value");
}

#[test]
fn test_zshrc_skips_oh_my_zsh_bootstrap() {
    let temp_dir = TempDir::new().unwrap();
    let omz_dir = temp_dir.path().join(".oh-my-zsh");
    fs::create_dir(&omz_dir).unwrap();
    fs::write(
        omz_dir.join("oh-my-zsh.sh"),
        "echo should_not_source_oh_my_zsh\n",
    )
    .unwrap();
    fs::write(
        temp_dir.path().join(".zshrc"),
        r#"
        export ZSH="$HOME/.oh-my-zsh"
        source $ZSH/oh-my-zsh.sh
        echo startup_continued
        "#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env("HOME", temp_dir.path())
        .arg("-i")
        .arg("-c")
        .arg("echo command_ran")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("Failed to spawn meowsh");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(stdout.contains("startup_continued"));
    assert!(stdout.contains("command_ran"));
    assert!(!stdout.contains("should_not_source_oh_my_zsh"));
    assert!(!stderr.contains("command not found"));
}

#[test]
fn test_zshrc_skips_zsh_completion_scripts() {
    let temp_dir = TempDir::new().unwrap();
    let completion_path = temp_dir.path().join("_tool");
    fs::write(
        &completion_path,
        "#compdef tool\necho should_not_source_completion\n",
    )
    .unwrap();
    fs::write(
        temp_dir.path().join(".zshrc"),
        format!(
            r#"
            [ -s "{}" ] && source "{}"
            echo startup_continued
            "#,
            completion_path.display(),
            completion_path.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env("HOME", temp_dir.path())
        .arg("-i")
        .arg("-c")
        .arg("echo command_ran")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("Failed to spawn meowsh");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(stdout.contains("startup_continued"));
    assert!(stdout.contains("command_ran"));
    assert!(!stdout.contains("should_not_source_completion"));
    assert!(!stderr.contains("command not found"));
}

// ---- Phase 2A: arrays + parameter substitution + stubs ------------------

#[test]
fn test_array_assign_and_join() {
    let (status, stdout, _) = run_script(r#"arr=(foo bar baz); echo "$arr""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "foo bar baz");
}

#[test]
fn test_array_index_one_based() {
    let (status, stdout, _) = run_script(r#"arr=(alpha beta gamma); echo "${arr[1]} ${arr[3]}""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "alpha gamma");
}

#[test]
fn test_array_at_splat() {
    let (status, stdout, _) = run_script(r#"arr=(a b c); echo "${arr[@]}"; echo "${arr[*]}""#);
    assert_eq!(status, 0);
    let lines: Vec<&str> = stdout.trim().split('\n').collect();
    assert_eq!(lines, vec!["a b c", "a b c"]);
}

#[test]
fn test_array_length() {
    let (status, stdout, _) = run_script(r#"arr=(one two three four); echo "${#arr[@]}""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "4");
}

#[test]
fn test_array_append() {
    let (status, stdout, _) = run_script(r#"arr=(a b); arr+=(c d); echo "${arr[@]}""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "a b c d");
}

#[test]
fn test_array_with_var_expansion_in_literal() {
    let (status, stdout, _) = run_script(r#"x=hello; arr=($x world); echo "${arr[1]}-${arr[2]}""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "hello-world");
}

#[test]
fn test_array_negative_index() {
    let (status, stdout, _) = run_script(r#"arr=(red green blue); echo "${arr[-1]} ${arr[-2]}""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "blue green");
}

#[test]
fn test_subst_first_match() {
    let (status, stdout, _) = run_script(r#"v=aXbXc; echo "${v/X/_}""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "a_bXc");
}

#[test]
fn test_subst_global() {
    let (status, stdout, _) = run_script(r#"v=aXbXc; echo "${v//X/_}""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "a_b_c");
}

#[test]
fn test_subst_path_separator() {
    let (status, stdout, _) = run_script(r#"p=/usr/local/bin; echo "${p//\//.}""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), ".usr.local.bin");
}

#[test]
fn test_subst_anchor_start() {
    let (status, stdout, _) = run_script(r#"v=foofoo; echo "${v/#foo/bar}""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "barfoo");
}

#[test]
fn test_subst_anchor_end() {
    let (status, stdout, _) = run_script(r#"v=foofoo; echo "${v/%foo/bar}""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "foobar");
}

#[test]
fn test_scalar_append() {
    let (status, stdout, _) = run_script(r#"x=hello; x+=" world"; echo "$x""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "hello world");
}

#[test]
fn test_zstyle_stores_and_reads_scalar() {
    let (status, stdout, _) = run_script(
        r#"zstyle ':completion:*' menu select; zstyle -s ':completion::complete:' menu result; echo "$result""#,
    );
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "select");
}

#[test]
fn test_zstyle_reads_array_values() {
    let (status, stdout, _) = run_script(
        r#"zstyle ':completion:*' matcher-list 'm:{a-z}={A-Za-z}' '+r:|[._-]=** r:|=**'; zstyle -a ':completion:*' matcher-list matchers; echo "${#matchers[@]}"; echo "${matchers[1]}""#,
    );
    assert_eq!(status, 0);
    let lines: Vec<&str> = stdout.trim().split('\n').collect();
    assert_eq!(lines, vec!["2", "m:{a-z}={A-Za-z}"]);
}

#[test]
fn test_zstyle_boolean_query() {
    let (status, stdout, _) = run_script(
        r#"zstyle ':completion:*' auto-description yes; zstyle -b ':completion:*' auto-description enabled; echo "$enabled""#,
    );
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "yes");
}

#[test]
fn test_zstyle_later_match_overrides_earlier() {
    let (status, stdout, _) = run_script(
        r#"zstyle ':completion:*' menu select; zstyle ':completion::complete:*' menu no; zstyle -s ':completion::complete:foo' menu result; echo "$result""#,
    );
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "no");
}

#[test]
fn test_zstyle_list_styles() {
    let (status, stdout, _) = run_script(r#"zstyle ':completion:*' menu select; zstyle -L"#);
    assert_eq!(status, 0);
    assert!(stdout.contains("zstyle :completion:* menu select"));
}

#[test]
fn test_compsys_stubs_no_error() {
    let (status, stdout, _) = run_script(
        r#"autoload -Uz compinit; compinit; compdef _gnu_generic ls; bindkey -e; echo ok"#,
    );
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "ok");
}

#[test]
fn test_add_zsh_hook_stub_no_error() {
    let (status, stdout, _) = run_script(r#"add-zsh-hook precmd my_hook; echo done"#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "done");
}

#[test]
fn test_for_loop_over_array() {
    let (status, stdout, _) =
        run_script(r#"arr=(a b c); for x in "${arr[@]}"; do echo "got $x"; done"#);
    assert_eq!(status, 0);
    let lines: Vec<&str> = stdout.trim().split('\n').collect();
    assert_eq!(lines, vec!["got a", "got b", "got c"]);
}

#[test]
fn test_posix_null_command() {
    let (status, stdout, _) = run_script(r#": ignored args; echo ok"#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "ok");
}

#[test]
fn test_posix_adjacent_parameter_expansion() {
    let (status, stdout, _) = run_script(r#"left=meow; right=sh; echo "$left$right""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "meowsh");
}

#[test]
fn test_posix_break_exits_loop() {
    let (status, stdout, _) =
        run_script(r#"for x in 1 2 3 4; do if [ "$x" -eq 3 ]; then break; fi; echo "$x"; done"#);
    assert_eq!(status, 0);
    let lines: Vec<&str> = stdout.trim().split('\n').collect();
    assert_eq!(lines, vec!["1", "2"]);
}

#[test]
fn test_posix_continue_skips_loop_body() {
    let (status, stdout, _) =
        run_script(r#"for x in 1 2 3; do if [ "$x" -eq 2 ]; then continue; fi; echo "$x"; done"#);
    assert_eq!(status, 0);
    let lines: Vec<&str> = stdout.trim().split('\n').collect();
    assert_eq!(lines, vec!["1", "3"]);
}

#[test]
fn test_posix_break_nested_count() {
    let (status, stdout, _) = run_script(
        r#"for outer in a b; do for inner in 1 2; do echo "$outer$inner"; break 2; done; echo after; done"#,
    );
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "a1");
}

#[test]
fn test_posix_continue_nested_count() {
    let (status, stdout, _) = run_script(
        r#"for outer in a b; do echo "outer-$outer"; for inner in 1 2; do continue 2; done; echo never; done"#,
    );
    assert_eq!(status, 0);
    let lines: Vec<&str> = stdout.trim().split('\n').collect();
    assert_eq!(lines, vec!["outer-a", "outer-b"]);
}

#[test]
fn test_posix_set_sets_positional_parameters() {
    let (status, stdout, _) = run_script(r#"set -- alpha beta gamma; echo "$#:$1:$2:$3""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "3:alpha:beta:gamma");
}

#[test]
fn test_posix_set_options_then_positional_parameters() {
    let (status, stdout, stderr) = run_script(r#"set -x alpha beta; echo "$1:$2" 2>/dev/null"#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "alpha:beta");
    assert!(stderr.contains("+ echo alpha:beta"));
}

#[test]
fn test_posix_shift_count() {
    let (status, stdout, _) = run_script(r#"set -- a b c d; shift 2; echo "$#:$1:$2""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "2:c:d");
}

#[test]
fn test_posix_shift_too_many_fails() {
    let (status, stdout, stderr) = run_script(r#"set -- a; shift 2; echo "status=$? count=$#""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "status=1 count=1");
    assert!(stderr.contains("can't shift that many"));
}

#[test]
fn test_posix_readonly_rejects_reassignment() {
    let (status, stdout, stderr) =
        run_script(r#"readonly answer=42; answer=99; echo "status=$? answer=$answer""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "status=1 answer=42");
    assert!(stderr.contains("readonly variable"));
}

#[test]
fn test_posix_readonly_rejects_second_readonly_assignment() {
    let (status, stdout, stderr) =
        run_script(r#"readonly answer=42; readonly answer=99; echo "status=$? answer=$answer""#);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "status=1 answer=42");
    assert!(stderr.contains("readonly variable"));
}

#[test]
fn test_typeset_export_assignment() {
    let (status, stdout, _) =
        run_script(r#"typeset -gx MEOWSH_TYPED=works; sh -c 'printf "%s" "$MEOWSH_TYPED"'"#);
    assert_eq!(status, 0);
    assert_eq!(stdout, "works");
}

#[test]
fn test_path_array_tracks_path_scalar() {
    let (status, stdout, _) =
        run_script(r#"PATH=/bin; path=(/tmp $path); echo "${path[@]}"; echo "$PATH""#);
    assert_eq!(status, 0);
    let lines: Vec<&str> = stdout.trim().split('\n').collect();
    assert_eq!(lines, vec!["/tmp /bin", "/tmp:/bin"]);
}

#[test]
fn test_typeset_unique_array_deduplicates_path() {
    let (status, stdout, _) = run_script(
        r#"typeset -U path; path=(/bin /usr/bin /bin /sbin /usr/bin); echo "${path[@]}"; echo "$PATH""#,
    );
    assert_eq!(status, 0);
    let lines: Vec<&str> = stdout.trim().split('\n').collect();
    assert_eq!(lines, vec!["/bin /usr/bin /sbin", "/bin:/usr/bin:/sbin"]);
}

#[test]
fn test_typeset_associative_array_key_lookup() {
    let (status, stdout, _) = run_script(
        r#"typeset -A colors; colors=(red 31 green 32); echo "${colors[red]} ${colors[green]}""#,
    );
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "31 32");
}

#[test]
fn test_associative_array_key_and_value_expansion() {
    let (status, stdout, _) = run_script(
        r#"typeset -A colors; colors=(red 31 green 32); echo "${(k)colors}"; echo "${(v)colors}"; echo "${#colors[@]}""#,
    );
    assert_eq!(status, 0);
    let lines: Vec<&str> = stdout.trim().split('\n').collect();
    assert_eq!(lines, vec!["green red", "32 31", "2"]);
}

#[test]
fn test_associative_array_subscript_assignment() {
    let (status, stdout, _) = run_script(
        r#"typeset -A colors; colors[blue]=34; colors[green]=32; echo "${colors[blue]} ${colors[green]}"; echo "${#colors[@]}""#,
    );
    assert_eq!(status, 0);
    let lines: Vec<&str> = stdout.trim().split('\n').collect();
    assert_eq!(lines, vec!["34 32", "2"]);
}

#[test]
fn test_associative_array_subscript_append() {
    let (status, stdout, _) = run_script(
        r#"typeset -A names; names[prefix]=meow; names[prefix]+=sh; echo "${names[prefix]}""#,
    );
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "meowsh");
}

#[test]
fn test_indexed_array_subscript_assignment_updates_path() {
    let (status, stdout, _) = run_script(
        r#"PATH=/bin:/usr/bin; path[1]=/tmp; path[3]=/sbin; echo "${path[@]}"; echo "$PATH""#,
    );
    assert_eq!(status, 0);
    let lines: Vec<&str> = stdout.trim().split('\n').collect();
    assert_eq!(lines, vec!["/tmp /usr/bin /sbin", "/tmp:/usr/bin:/sbin"]);
}

#[test]
fn test_zshrc_skipped_when_noninteractive() {
    let temp_dir = TempDir::new().unwrap();
    // .zshrc would set BAZ to "should_not_appear" — but in a non-interactive
    // shell .zshrc must not be sourced.
    fs::write(temp_dir.path().join(".zshrc"), "BAZ=should_not_appear\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env("HOME", temp_dir.path())
        .arg("-c")
        .arg("echo result=$BAZ")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("Failed to spawn meowsh");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_eq!(stdout.trim(), "result=");
}
