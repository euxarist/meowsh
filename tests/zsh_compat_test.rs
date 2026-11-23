//! zsh features that common startup files rely on: what `starship init
//! zsh`, `direnv hook zsh` and oh-my-zsh style configs emit.

use std::process::{Command, Stdio};
use tempfile::TempDir;

fn sh(script: &str) -> (i32, String, String) {
    let home = TempDir::new().unwrap();
    sh_home(script, &home)
}

fn sh_home(script: &str, home: &TempDir) -> (i32, String, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env("HOME", home.path())
        .args(["-c", script])
        .stdin(Stdio::null())
        .output()
        .expect("run meowsh");
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

// Runs `input` through an interactive shell reading a pipe, so prompts
// and hooks run, with `zshrc` as the startup file.
fn interactive(zshrc: &str, input: &str) -> (i32, String) {
    use std::io::Write;
    let home = TempDir::new().unwrap();
    std::fs::write(home.path().join(".zshrc"), zshrc).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env("HOME", home.path())
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned(),
    )
}

#[test]
fn process_substitution() {
    let (_, out, err) = sh("source <(echo 'echo sourced'); cat <(echo a) <(echo b); \
         while read l; do echo \"got $l\"; done < <(printf 'x\\ny\\n'); \
         echo z > >(cat); wait");
    assert_eq!(out, "sourced\na\nb\ngot x\ngot y\nz\n", "stderr: {}", err);
}

#[test]
fn process_substitution_does_not_leak_descriptors() {
    let (_, out, _) = sh(
        "before=$(ls /proc/$$/fd | wc -l); cat <(echo a) <(echo b) >/dev/null; \
         after=$(ls /proc/$$/fd | wc -l); [ \"$before\" = \"$after\" ] && echo same",
    );
    assert_eq!(out, "same\n");
}

#[test]
fn brace_group_closes_without_semicolon() {
    let (_, out, _) = sh("f() { echo in f }; f; { echo grp }; echo }");
    assert_eq!(out, "in f\ngrp\n}\n");
}

#[test]
fn arithmetic_command_and_c_style_for() {
    let (_, out, _) = sh(
        "(( 2 > 1 )); echo $?; (( 0 )); echo $?; x=0; (( x++ )); echo $? $x; \
         for ((i = 0; i < 3; i++)); do echo $i; done; ((echo sub); echo b)",
    );
    assert_eq!(out, "0\n1\n1 1\n0\n1\n2\nsub\nb\n");
}

#[test]
fn float_arithmetic_and_math_functions() {
    let (_, out, _) = sh("echo $((1.5 * 2)) $((10 / 4.)) $((7 / 2)); \
         t=$(( int(rint(EPOCHREALTIME * 1000)) )); [ ${#t} -ge 13 ] && echo ms");
    assert_eq!(out, "3. 2.5 3\nms\n");
}

#[test]
fn subscript_search_flags() {
    let (_, out, _) = sh(
        "a=(x foo bar foo); echo ${a[(i)foo]} ${a[(I)foo]} ${a[(I)nope]} ${a[(i)nope]} \
         ${a[(r)b*]}; typeset -ag hooks; \
         if (( ! ${hooks[(I)h]} )); then hooks=(h $hooks); fi; echo ${hooks[@]}",
    );
    assert_eq!(out, "2 4 0 5 bar\nh\n");
}

#[test]
fn dbracket_grouped_patterns_and_subscript_v() {
    let (_, out, _) = sh("v=5.9; [[ $v == ([1-4]*) ]] && echo old || echo new; \
         [[ main.rs == *.(rs|toml) ]] && echo grp; \
         typeset -A w; [[ -v w[k] ]] || echo unset; w[k]=1; [[ -v w[k] ]] && echo set");
    assert_eq!(out, "new\ngrp\nunset\nset\n");
}

#[test]
fn dynamic_parameters() {
    let (_, out, _) = sh(
        "RANDOM=7; a=$RANDOM; RANDOM=7; [ $a = $RANDOM ] && echo seeded; \
         [ $EPOCHSECONDS -gt 0 ] && echo epoch; true | false | (exit 3); \
         echo ${pipestatus[@]}",
    );
    assert_eq!(out, "seeded\nepoch\n0 1 3\n");
}

#[test]
fn print_builtin() {
    let (_, out, _) = sh("print -n 'a\\tb'; print; print -l x y; print -r 'raw\\n'");
    assert_eq!(out, "a\tb\nx\ny\nraw\\n\n");
}

#[test]
fn autoload_reads_body_from_fpath_and_keeps_builtins() {
    let home = TempDir::new().unwrap();
    let dir = home.path().join("fn");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("greet"), "echo \"hello $1\"\n").unwrap();
    let script = format!(
        "fpath=({} $fpath); autoload -Uz greet add-zsh-hook; greet world; greet again; \
         type add-zsh-hook",
        dir.display()
    );
    let (_, out, _) = sh_home(&script, &home);
    assert_eq!(
        out,
        "hello world\nhello again\nadd-zsh-hook is a shell builtin\n"
    );
}

#[test]
fn hooks_run_with_saved_status() {
    let zshrc = r#"
p1() { echo "p1 st=$? ps=${pipestatus[*]}"; false; }
p2() { echo "p2 st=$?"; }
precmd_functions+=(p1)
autoload -Uz add-zsh-hook
add-zsh-hook precmd p2
add-zsh-hook preexec px
px() { echo "preexec [$1]"; }
add-zsh-hook chpwd cw
cw() { echo "chpwd $PWD"; }
zshexit() { echo bye; }
"#;
    let (status, out) = interactive(zshrc, "false | true\ncd /\nexit 3\n");
    assert_eq!(status, 3);
    assert_eq!(
        out,
        "p1 st=0 ps=0\np2 st=0\npreexec [false | true]\n\
         p1 st=0 ps=1 0\np2 st=0\npreexec [cd /]\nchpwd /\n\
         p1 st=0 ps=0\np2 st=0\npreexec [exit 3]\nbye\n"
    );
}

#[test]
fn prompt_substitutes_before_percent_escapes() {
    let zshrc = "setopt promptsubst\nPROMPT='$(printf \"%%{<%%}x\")%# '\n";
    let home = TempDir::new().unwrap();
    std::fs::write(home.path().join(".zshrc"), zshrc).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env("HOME", home.path())
        .arg("-i")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    let expected = if unsafe { libc::geteuid() } == 0 {
        "<x# "
    } else {
        "<x% "
    };
    assert!(err.starts_with(expected), "prompt was {:?}", err);
}

// ---- oh-my-zsh ------------------------------------------------------------

#[test]
fn zsh_version_and_emulate() {
    let (_, out, _) = sh("[ -n \"$ZSH_VERSION\" ] && echo set; emulate; is-at-least 5.0 && echo ok; is-at-least 9.0 || echo old");
    assert_eq!(out, "set\nzsh\nok\nold\n");
}

#[test]
fn nested_expansion_flags_and_modifiers() {
    let (_, out, err) = sh("v='git version 2.43.0'; echo ${${(As: :)v}[3]}; \
         p=/a/b/c.tar.gz; echo ${p:t} ${p:h} ${p:r} ${p:e} ${p:h:t}; \
         a=(b a c a); echo ${(j:,:)a} ${(o)a} ${(u)a} ${#a} ${a[2,-1]}; \
         codes=(1 2); x=(\"<\"${^codes}\">\"); echo ${x[@]}; \
         s=hello; echo ${s[2]} ${s[2,3]} $#s; \
         typeset -A m; m=(k1 v1 k2 v2); echo ${(k)m} ${(kv)m} ${(k)m[(r)v2]}; \
         l='l1\nl2'; arr=(\"${(@f)$(printf \"$l\")}\"); echo ${#arr} $arr[2]; \
         echo $+commands[sh] $+commands[no-such-cmd-xyz]; \
         o='a b'; set -- ${=o}; echo $#; \
         h=(x.zsh y.sh); echo ${h:#*.sh} ${(M)h:#*.sh}; \
         k=(1 2); w=(a b); echo ${k:^w}");
    assert_eq!(
        out,
        "2.43.0\nc.tar.gz /a/b /a/b/c.tar gz b\nb,a,c,a a a b c b a c 4 a c a\n<1> <2>\n\
         e el 5\nk1 k2 k1 v1 k2 v2 k2\n2 l2\n1 0\n2\nx.zsh y.sh\n1 a 2 b\n",
        "stderr: {}",
        err
    );
}

#[test]
fn global_aliases_and_the_aliases_array() {
    let (_, out, _) = sh(
        // Aliases apply to text parsed after they are defined, so the
        // uses go through eval (zsh parses a -c string whole).
        "alias -g G='| grep b'; eval \"printf 'a\\nb\\n' G\"; alias ll='ls -l'; \
         echo ${aliases[ll]} ${#galiases}; aliases[hi]='echo hello'; eval hi",
    );
    assert_eq!(out, "b\nls -l 1\nhello\n");
}

#[test]
fn short_loops_multiple_names_and_anonymous_functions() {
    let (_, out, _) = sh(
        "for k v (a 1 b 2); do echo $k=$v; done; for x (p q) echo $x; \
         () { echo anon $1 } arg; function f1 f2 { echo $0 }; f1; f2",
    );
    assert_eq!(out, "a=1\nb=2\np\nq\nanon arg\nf1\nf2\n");
}

#[test]
fn always_here_string_and_redirect_both() {
    let home = TempDir::new().unwrap();
    let (_, out, _) = sh_home(
        "cd; f() { { return 3 } always { echo cleanup } }; f; echo $?; \
         read x <<< 'from string'; echo $x; \
         { echo out; echo err >&2 } &> both; cat both; \
         (echo bg > bgout) &|; while [ ! -s bgout ]; do :; done; cat bgout; jobs | wc -l | tr -d ' '",
        &home,
    );
    assert_eq!(out, "cleanup\n3\nfrom string\nout\nerr\nbg\n0\n");
}

#[test]
fn case_patterns_with_groups() {
    let (_, out, _) = sh(
        "for v in freebsd13 plugin::load x; do case $v in \
         (darwin|freebsd)*) echo bsd;; plugin::(disable|load)) echo plug;; (x) echo posix;; esac; done",
    );
    assert_eq!(out, "bsd\nplug\nposix\n");
}

#[test]
fn glob_groups_ranges_and_qualifiers() {
    let home = TempDir::new().unwrap();
    for f in ["a.rs", "b.toml", "c.txt", "f1", "f2", "f10"] {
        std::fs::write(home.path().join(f), "").unwrap();
    }
    std::fs::create_dir(home.path().join("sub")).unwrap();
    let script = format!(
        "cd {}; echo *.(rs|toml); echo f<1-2>; echo *(/); echo none*(N) end; echo f<->(.)",
        home.path().display()
    );
    let (_, out, _) = sh_home(&script, &home);
    assert_eq!(out, "a.rs b.toml\nf1 f2\nsub\nend\nf1 f10 f2\n");
}

#[test]
fn zparseopts_and_fc() {
    let (_, out, _) = sh(
        "f() { local -a opts; local clear stamp; zparseopts -D -E -a opts c=clear t:=stamp P; \
         echo \"c=$clear t=${stamp[*]} o=${opts[*]} rest=$*\"; }; f -c -t fmt x -P y",
    );
    assert_eq!(out, "c=-c t=-t fmt o=-P rest=x y\n");
    // The running fc is the current event, not listed (as zsh does).
    let (_, out) = interactive("", "echo a\nfc -ln -2\n");
    assert_eq!(out, "a\necho a\n");
}

#[test]
fn function_exit_trap_and_local_traps() {
    let (_, out, _) = sh("f() { trap 'echo fexit' EXIT; echo in; }; f; echo after; \
         g() { emulate -L zsh; trap 'echo int' INT; setopt autocd; }; g; trap; echo end");
    assert_eq!(out, "in\nfexit\nafter\nend\n");
}

#[test]
fn auto_cd_in_interactive_shell() {
    let (_, out) = interactive("setopt auto_cd\n", "/\npwd\n");
    assert_eq!(out, "/\n");
}

#[test]
fn read_keys_and_yes_no() {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .args([
            "-c",
            "read -q && echo yes; read -k 1 c; echo \"[$c]\"; read -k 2; echo \"[$REPLY]\"",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"yZab").unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout), "yes\n[Z]\n[ab]\n");
}

#[test]
fn colors_and_prompt_truncation() {
    let (_, out, _) =
        sh("colors; print -rn -- \"$fg[red]x$reset_color\"; print; print -P '%5<..<abcdefgh%<<'");
    assert_eq!(out, "\x1b[31mx\x1b[00m\n..fgh\n");
}

#[test]
fn eval_catches_expansion_errors_but_not_exit() {
    let (status, out, _) = sh(
        "eval 'echo ${(Z)x}; echo same-eval'; echo \"st=$? after\"; \
         echo ${unset_v?gone}; echo not-reached",
    );
    assert_eq!(out, "st=1 after\n");
    assert_eq!(status, 1);
    let (status, out, _) = sh("eval 'exit 4'; echo not-reached");
    assert_eq!((status, out.as_str()), (4, ""));
}

// ---- zsh defaults, options, typeset, grammar ------------------------------

#[test]
fn zsh_defaults_and_sh_emulation() {
    let (_, out, _) = sh("x='a b'; for w in $x; do echo \"[$w]\"; done; for w in $(echo c d); do echo \"[$w]\"; done");
    assert_eq!(out, "[a b]\n[c]\n[d]\n");
    let (status, out, err) = sh("echo nomatch-xyz-*; echo after");
    assert_eq!((status, out.as_str()), (1, ""));
    assert!(err.contains("no matches found: nomatch-xyz-*"));
    let (_, out, _) = sh("emulate sh; x='a b'; set -- $x; echo $#; echo nomatch-xyz-*");
    assert_eq!(out, "2\nnomatch-xyz-*\n");
    let (_, out, _) = sh("setopt shwordsplit; x='a b'; set -- $x; echo $#; unsetopt sh_word_split; set -- $x; echo $#");
    assert_eq!(out, "2\n1\n");
    let (_, out, _) =
        sh("echo ${options[nomatch]} ${options[shwordsplit]}; [[ $- == *f* ]] || echo rcs");
    assert_eq!(out, "on off\nrcs\n");
}

#[test]
fn typeset_attributes_and_printing() {
    let (_, out, err) = sh(
        "f() { local s=x; local -a a=(1 2); typeset -p s a; typeset +m s a; \
         integer i=3.7; integer -i2 i; echo $i; typeset -p i; \
         float -F2 g=3.14159; echo $g; typeset -R5 -u v=foo; echo \"[$v]\"; typeset -p v; \
         typeset -Z4 z=7; echo $z; export e=1; typeset -p e; }; f; \
         typeset -T TIED tied=(a b); echo $TIED; tied+=(c); echo $TIED; typeset -p tied",
    );
    assert_eq!(
        out,
        "typeset s=x\ntypeset -a a=( 1 2 )\nlocal s\narray local a\n2#11\ntypeset -i2 i=3\n\
         3.14\n[  FOO]\ntypeset -R5 -u v=foo\n0007\nexport e=1\na:b\na:b:c\n\
         typeset -aT TIED tied=( a b c )\n",
        "stderr: {}",
        err
    );
}

#[test]
fn functions_prints_definitions() {
    let (_, out, _) =
        sh("f() { if a; then b; fi; for x in 1 2; do echo $x; done; c && d | e; }; functions f");
    assert_eq!(
        out,
        "f () {\n\tif a\n\tthen\n\t\tb\n\tfi\n\tfor x in 1 2\n\tdo\n\t\techo $x\n\tdone\n\tc && d | e\n}\n"
    );
}

#[test]
fn zsh_loop_forms_and_coprocess() {
    let (_, out, err) = sh(
        "repeat 2 echo r; for x in a b; print $x; if [[ 1 == 1 ]] print short; \
         if (false) { print no } else { print yes }; while (( n++ < 2 )) { print w$n }; \
         case b { (a) echo A;; (b) echo B;; }; \
         case x in (x) echo 1 ;| (*) echo 2 ;; esac; \
         foreach v (p q)\nprint $v\nend\n\
         coproc { read l; print got $l; }; print -p hi; read -p r; echo $r",
    );
    assert_eq!(
        out, "r\nr\na\nb\nshort\nyes\nw1\nw2\nB\n1\n2\np\nq\ngot hi\n",
        "stderr: {}",
        err
    );
}

#[test]
fn precommand_modifiers_and_exec_options() {
    let (_, out, _) = sh(
        "noglob echo *; command -v sh >/dev/null && echo quiet; exec -a renamed sh -c 'echo $0'",
    );
    assert_eq!(out, "*\nquiet\nrenamed\n");
}

// ---- whence, match flags, extended glob, namerefs, array items ------------

#[test]
fn whence_family_formats() {
    let (_, out, _) = sh(
        "alias ll='ls -l'; f() { :; }; whence ll; whence -v ll f echo if; whence -c echo if; \
         whence -w ll f echo if sh nope-xyz; type -w echo",
    );
    assert_eq!(
        out,
        "ls -l\nll is an alias for ls -l\nf is a shell function from meowsh\necho is a shell builtin\n\
         if is a reserved word\necho: shell built-in command\nif: shell reserved word\n\
         ll: alias\nf: function\necho: builtin\nif: reserved\nsh: command\nnope-xyz: none\necho: builtin\n"
    );
}

#[test]
fn substitution_match_flags() {
    let (_, out, _) = sh(
        "s='where is the white windmill'; print ${(S)s#h*e}; print ${(S)s%%h*e}; \
         print ${(BES)s##white}; print ${(NS)s##white}; print ${(MSI:2:)s#w*e}; \
         a=(arthur boldly claws); print ${a:/[aeiou]*/X}; print ${(S)a/[aeiou]*/Y}; \
         x=abcabc; print ${x//b/X} ${x#*b} ${x%%b*}; set2=old; print ${set2::=new} $set2; \
         m=(one two three); n=(two); print ${m:|n} -- ${m:*n}",
    );
    assert_eq!(
        out,
        "wre is the white windmill\nwhere is the w windmill\n14 19\n5\nwhite\nX boldly claws\n\
         Yrthur bYldly clYws\naXcaXc cabc a\nnew new\none three -- two\n"
    );
}

#[test]
fn extended_glob_operators() {
    let (_, out, _) = sh(
        "setopt extendedglob; [[ aaab == a#b ]] && echo 1; [[ b == a##b ]] || echo 2; \
         [[ foo == ^bar ]] && echo 3; [[ foo.c == *.c~f* ]] || echo 4; [[ FOO == (#i)foo ]] && echo 5; \
         [[ abab == (ab)## ]] && echo 6; print \"q~^#\"",
    );
    assert_eq!(out, "1\n2\n3\n4\n5\n6\nq~^#\n");
}

#[test]
fn padding_and_display_flags() {
    let (_, out, _) = sh(
        "foo=(resulting words uproariously); print ${(l.5..X.r.5..Y.)foo}; \
         print ${(l.6..X..A.r.6..Y..B.)foo}; w=(why in); print ${(r.5..!..?.)w}; \
         e='$(print a b)'; print -l ${(e)e}; psvar=(dog); print ${(%):-%1v}; \
         arr=(ab cd e); print ${(c)#arr} ${(w)#arr}; v=$'a\\x7fb'; print ${(V)v}; \
         q=\"a b'c\"; print -r ${(qqqq)q}",
    );
    assert_eq!(
        out,
        "Xresulting XXXwordsYY proariousl\nXAresultingB XXXAwordsBYY uproariously\n\
         why?! in?!!\na\nb\ndog\n7 3\na^?b\n$'a b\\'c'\n"
    );
}

#[test]
fn namerefs() {
    let (status, out, err) = sh(
        "var=value; typeset -n ptr=var; echo $ptr ${ptr[2,3]} ${!ptr}; ptr=new; echo $var; \
         arr=(a b); typeset -n ar=arr; ar+=(c); echo ${ar[@]} $#ar; typeset -p ptr; \
         typeset -n p2; p2=var; echo $p2; unset ptr; echo ${+var}; \
         typeset -n l1=l2; typeset -n l2=l1",
    );
    assert_eq!(
        out,
        "value al var\nnew\na b c 3\ntypeset -n ptr=var\nnew\n0\n"
    );
    assert!(err.contains("invalid self reference"), "stderr: {}", err);
    assert_eq!(status, 1);
}

#[test]
fn key_value_array_items_and_slices() {
    let (_, out, _) = sh(
        "typeset -A h=([one]=eins [two]=zwei); h+=([three]=drei); for k in ${(ok)h}; print $k $h[$k]; \
         a=(first [3]=third fourth); print -l \"${a[@]}\"; \
         c=(one two three four five); c[2,4]=(umm er); print ${#c} $c; c[2,3]=(); print $c; \
         s=abcdef; s[2,3]=XY; print $s",
    );
    assert_eq!(
        out,
        "one eins\nthree drei\ntwo zwei\nfirst\n\nthird\nfourth\n4 one umm er five\none five\naXYdef\n"
    );
}

// ---- glob/split toggles, KSH_ARRAYS, captures, assignment forms -----------

#[test]
fn glob_and_split_toggles_and_brace_order() {
    let (_, out, err) = sh("cd $HOME && touch b1 b2; s='b*'; print $s ${~s} $~s; \
         setopt shwordsplit; w='x y'; print -l ${==w}; unsetopt shwordsplit; \
         a=(p q); print IF{${^a},T}E; print X{\"${a}\",T}Y");
    assert_eq!(
        out, "b* b1 b2 b1 b2\nx y\nIFpE IFTE IFqE IFTE\nXp qY XTY\n",
        "stderr: {}",
        err
    );
}

#[test]
fn ksh_arrays_and_zero_subscript() {
    let (_, out, _) = sh(
        "setopt ksharrays; k=(x y z); print $k ${k[0]} ${k[1]} \"${k[0,1]}\" ${k[(i)y]}; \
         k[0]=X; print ${k[@]}; unsetopt ksharrays; \
         z=(one two); print X${z[0]}X; setopt kshzerosubscript; print ${z[0]}",
    );
    assert_eq!(out, "x x y x y 1\nX y z\nXX\none\n");
}

#[test]
fn backreferences_and_match_counts() {
    let (_, out, err) = sh("setopt extendedglob; s='look for a match'; \
         [[ $s == (#b)(*)(match) ]] && print $match[1]x $match[2] $mbegin[2] $mend[2]; \
         t='this is'; print ${t//(#m)s/<$MATCH$MBEGIN>}; \
         f=aleftkept; print ${f//(#b)(*)left/${match/a/A}}; \
         n=1_2_3_4; print ${n#(*_)(#c2)}; \
         fn() { setopt localoptions noextendedglob; }; fn; [[ -o extendedglob ]] && print still");
    assert_eq!(
        out, "look for a x match 12 16\nthi<s4> i<s7>\nAkept\n3_4\nstill\n",
        "stderr: {}",
        err
    );
}

#[test]
fn subscripted_and_positional_assignment_forms() {
    let (_, out, err) = sh(
        "typeset -Z3 z; print ${z::=15}; a=(a b c d); print ${a[2]::=X} $a; \
         () { print ${2::=two} $2 } one; s=abc; s[2]=Z; print $s; \
         x=sprodj; x[-10]=scrumf; print $x; typeset -i i=1; i=(3 4); typeset -p i; \
         set a b c d e f g h i j k; print $11 $10; v=abc; print $(( #v )) $(( ##a )); \
         set 1 2 3 4 5 6 7 8 9 10; print ${##} ${##1}; echo $(( 010 ))",
    );
    assert_eq!(
        out,
        "015\nX a X c d\ntwo two\naZc\nscrumfsprodj\ntypeset -a i=( 3 4 )\nk j\n97 97\n2 0\n10\n",
        "stderr: {}",
        err
    );
}

#[test]
fn flag_order_and_double_quoted_joining() {
    let (_, out, _) = sh("arr=(she sells z shells); (IFS=+; print ${(s.s.)arr}); \
         x=b:a:b; print ${(us.:.)x} ${(os.:.j.-.)x}; \
         foo=(a6 a117 a17 b-2 b-11); print ${(n)foo}; print ${(-)foo}; \
         a=(et Swann avec); print \"${a#?}\" \"${${a}[2,4]}\" \"${${(@)a}[1,2]#?}\" \"${(o)a}\"");
    assert_eq!(
        out,
        "he+ ell +z+ hell\nb a a b b\na6 a17 a117 b-2 b-11\na6 a17 a117 b-11 b-2\n\
         t Swann avec t S t Swann et Swann avec\n"
    );
}

#[test]
fn errors_name_their_context_and_line() {
    let (_, out, err) = sh(
        "eval 'nosuchcmd-xyz'\nf() {\n  :\n  nosuchcmd-abc\n}\nf\n() { cd /nonexistent-dir-xyz }\n\
         print $(( 3,,4 ))\nprint after",
    );
    assert_eq!(out, "");
    assert_eq!(
        err,
        "(eval):1: command not found: nosuchcmd-xyz\nf:2: command not found: nosuchcmd-abc\n\
         (anon):cd: no such file or directory: /nonexistent-dir-xyz\n\
         meowsh:8: bad math expression: operand expected at `,4 '\n"
    );
}

#[test]
fn redirect_both_streams_to_a_file() {
    let (_, out, _) = sh("cd $HOME && { echo out; echo err >&2 } >& f.txt; cat f.txt");
    assert_eq!(out, "out\nerr\n");
}

// ---- (z) splitting, subscript searches, enable/disable, traps ------------

#[test]
fn z_flag_splits_like_the_lexer() {
    let (_, out, _) = sh("foo='<five> {six} )ten( more'; print -rl -- ${(z)foo}; \
         s='for (( i = 1 ; i < 3 ; i++ ))'; print -rl -- ${(z)s}; \
         l=$'a # c\\nb'; print -rl -- ${(Z+c+)l} -- ${(Z+C+)l}");
    assert_eq!(
        out,
        "<\nfive\n>\n{six}\n)\nten( more\nfor\n((\ni = 1 ;\ni < 3 ;\ni++ \n))\n\
         a\n# c\n;\nb\n--\na\n;\nb\n"
    );
}

#[test]
fn subscript_search_flags_and_multibyte() {
    let (_, out, err) = sh("s='Twinkle, twinkle, little *'; x=','; \
         print ${s[(r)$x,(R)$x]} $s[(i)winkle] $s[(I)winkle]; \
         a=ténébreux; print ${a[(r)én,(r)éb]} ${a[(rb:3:)é,-1]} ${a[(rn:2:)é,-1]}; \
         b=(sping spang spong); print ${b[(ib.2.)sp?ng]} ${b[(n:2:i)sp*]}; \
         w='foo bar baz'; print ${w[(w)2]} ${w[(wi)baz]}; hähä=3; print $hähä");
    assert_eq!(
        out, ", twinkle, 2 11\nénéb ébreux ébreux\n2 2\nbar 9\n3\n",
        "stderr: {}",
        err
    );
}

#[test]
fn enable_disable_and_closing_braces() {
    let (_, out, _) = sh(
        "f() { print f }; disable -f f; f 2>/dev/null || print disabled; enable -f f; f; \
         disable -r typeset; eval 'g() { typeset v=`echo a b=c`; print $v $b }'; g; \
         { A=0 }; print $A",
    );
    assert_eq!(out, "disabled\nf\na c\n0\n");
}

#[test]
fn readonly_listing_in_sh_mode_does_not_hang() {
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .args([
            "--emulate",
            "sh",
            "-c",
            "PATH=/bin; export PATH; readonly PATH; export -p PATH; typeset -p PATH; \
             readonly -p; echo ${path-unset}",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&o.stdout),
        "export -r PATH=/bin\nreadonly PATH=/bin\nunset\n"
    );
}

#[test]
fn fd_variables_clobber_forms_and_read_options() {
    let (_, out, _) = sh(
        "cd $HOME && exec {fd}>out; print to fd >&$fd; exec {fd}>&-; cat out; \
         set -o noclobber; print a >!o2; print b >>|o2; cat o2; \
         print -n x | read -e; print; print -n y | { read -t 1 v; print $v }",
    );
    assert_eq!(out, "to fd\na\nb\nx\n\ny\n");
}

#[test]
fn shebang_fallback_and_prefix_path() {
    let (_, out, err) = sh(
        "cd $HOME && print -n '#!sh\\necho slashless' >t1 && chmod +x t1 && \
         PATH=$PATH:$HOME t1",
    );
    assert_eq!(out, "slashless\n", "stderr: {}", err);
}

#[test]
fn zerr_and_debug_traps() {
    let (_, out, _) = sh(
        "TRAPZERR() { print zerr }; false; unfunction TRAPZERR; false; print ok\n\
         trap 'print dbg $LINENO' DEBUG; :; trap - DEBUG\nprint $LINENO",
    );
    assert_eq!(out, "zerr\nok\ndbg 2\ndbg 2\n3\n");
}

// ---- functions: anonymous, math, autoload, handlers ----------------------

#[test]
fn anonymous_functions_with_glob_args_and_redirections() {
    let (_, out, err) = sh(
        "cd $HOME && v=outside; function { local v=inside; print $v $* } a b; print $v; \
         touch yes no; () { echo $1 } (y|z)*; function { sed s/a/b/ } <<<abc",
    );
    assert_eq!(out, "inside a b\noutside\nyes\nbbc\n", "stderr: {}", err);
}

#[test]
fn function_keyword_with_parens_is_anonymous() {
    let (_, out, err) = sh("function () { print a $1 } x; function() { print b }; \
         for i in 1 2; do function () { print $i; break }; done");
    assert_eq!(out, "a x\nb\n1\n", "stderr: {}", err);
}

#[test]
fn assignments_before_special_builtins_are_temporary() {
    let (_, out, err) = sh(
        "x=0; x=1 :; x=2 eval 'print in=$x'; x=3 source /dev/null; x=4 export y=5; \
         print $x $y; IFS=: eval true; v='a:b c'; print ${#${=v}}; \
         a=(1 2); f() { print $a }; a=(3 4) f; a=(5) eval 'print $a'; print $a; \
         setopt posixbuiltins; x=6 :; print $x",
    );
    assert_eq!(out, "in=2\n0 5\n2\n3 4\n5\n1 2\n6\n", "stderr: {}", err);
    let (st, out, _) = sh("readonly r=1; r=2 :; print after");
    assert_eq!((st, out.as_str()), (1, ""));
}

#[test]
fn prefix_assignment_errors_and_scopes_compat() {
    // A read-only array ends the script like a read-only scalar.
    let (st, out, _) = sh("readonly r=1; r=(1 2) :; print after");
    assert_eq!((st, out.as_str()), (1, ""));
    // A local declared by the command does not bring the temporary value
    // back when the function returns.
    let (_, out, err) = sh(
        "x=0; f() { x=1 eval 'typeset x=7; print $x'; print $x }; f; print $x; \
         unset y; g() { y=1 eval 'typeset y=7' }; g; print ${+y}",
    );
    assert_eq!(out, "7\n0\n0\n0\n", "stderr: {}", err);
}

#[test]
fn digits_before_a_numeric_range_are_not_a_redirection() {
    let dir = TempDir::new().unwrap();
    for f in ["21", "25", "x7"] {
        std::fs::write(dir.path().join(f), "").unwrap();
    }
    let (_, out, err) = sh(&format!(
        "cd {}; [[ 2026 = 2<-> ]] && print year; case 215 in 2<->5) print m;; esac; \
         print 2<->; print 2<4-9>; print hi 2>/dev/null",
        dir.path().display()
    ));
    assert_eq!(out, "year\nm\n21 25\n25\nhi\n", "stderr: {}", err);
}

#[test]
fn q_flag_quotes_each_unprintable_character_on_its_own() {
    let (_, out, err) = sh(
        "x=$'a\\tb\\n\\nc'; print -r -- ${(q)x}; x=$'\\e[1m\\x01\\x7f'; print -r -- ${(q)x} ${(q+)x}",
    );
    assert_eq!(
        out, "a$'\\t'b$'\\n'$'\\n'c\n$'\\033'\\[1m$'\\001'$'\\177' $'\\C-[[1m\\C-A\\C-?'\n",
        "stderr: {}",
        err
    );
}

#[test]
fn break_and_continue_outside_a_loop_are_fatal() {
    for cmd in [
        "break",
        "continue",
        "f() { break }; f",
        "if break; then :; fi",
    ] {
        let (st, out, err) = sh(&format!("{}; print after", cmd));
        assert_eq!((st, out.as_str()), (1, ""), "{}: {}", cmd, err);
        assert!(err.contains("not in"), "{}: {}", cmd, err);
    }
    // The error ends an eval, not the script.
    let (_, out, _) = sh("eval break; print after");
    assert_eq!(out, "after\n");
}

#[test]
fn not_found_handler_math_functions_and_argv() {
    let (_, out, err) = sh(
        "command_not_found_handler() { print handled $1 ${argv[2,-1]}; return 3 }; \
         nosuchcmd-q x y; print $?; \
         functions -M m1; m1() { (( $# * 10 )) }; print $(( m1(1,2) )); \
         f() { (( $#1 )) }; functions -Ms slen 1 1 f; print $(( slen(a, b) ))",
    );
    assert_eq!(
        out, "handled nosuchcmd-q x y\n3\n20\n4\n",
        "stderr: {}",
        err
    );
}

#[test]
fn eval_parses_whole_string_and_function_names_expand() {
    let (_, out, _) = sh(
        "eval 'alias zz=\"print aliased\"; zz' 2>/dev/null; eval zz; \
         function g$$ () { print named }; g$$; d() { a b() { print $0 } }; functions d; \
         e(){}; e; print empty ok",
    );
    assert_eq!(
        out,
        "aliased\nnamed\nd () {\n\tfunction a b {\n\t\tprint $0\n\t}\n}\nempty ok\n"
    );
}

#[test]
fn autoload_paths_and_function_copies() {
    let (_, out, err) = sh(
        "cd $HOME && mkdir -p ad && print 'print loaded from dir' >ad/af; \
         autoload $PWD/ad/af; af; print 'print via X' >ad/ax; ax() { autoload -X $PWD/ad }; ax; \
         h() { print h }; functions -c h h2; unfunction h; h2",
    );
    assert_eq!(out, "loaded from dir\nvia X\nh\n", "stderr: {}", err);
}

#[test]
fn output_files_equals_and_glob_exclusions() {
    let (_, out, err) = sh("cd $HOME && cat =(print in a file); print =cat(:t); \
         mkdir -p g/d1 g/d2 g/d3; touch g/f; setopt extendedglob; \
         print g/*~*/d2; print g/*~*/d3(/*|(#e))(/); print g/*(/:t); \
         (FUNCNEST=1; r() { r }; r) 2>/dev/null; print $?");
    assert_eq!(
        out, "in a file\ncat\ng/d1 g/d3 g/f\ng/d1 g/d2\nd1 d2 d3\n1\n",
        "stderr: {}",
        err
    );
}

// ---- right prompt ---------------------------------------------------------

// Runs an interactive meowsh on a 60-column pseudo-terminal, types
// `input`, and returns everything it drew.
fn on_terminal(zshrc: &str, input: &str) -> String {
    on_terminal_with(&[], zshrc, input)
}

fn on_terminal_with(args: &[&str], zshrc: &str, input: &str) -> String {
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    let home = TempDir::new().unwrap();
    std::fs::write(home.path().join(".zshrc"), zshrc).unwrap();
    let (mut master, mut slave) = (0, 0);
    let mut ws = libc::winsize {
        ws_row: 10,
        ws_col: 60,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ok = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws as *mut _ as _,
        )
    };
    assert_eq!(ok, 0, "openpty");
    let tty = |fd: i32| unsafe { Stdio::from_raw_fd(libc::dup(fd)) };
    let mut child = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env("HOME", home.path())
        .env("TERM", "xterm")
        .args(args)
        .arg("-i")
        .stdin(tty(slave))
        .stdout(tty(slave))
        .stderr(tty(slave))
        .spawn()
        .unwrap();
    unsafe { libc::close(slave) };
    unsafe {
        let flags = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let mut m = unsafe { std::fs::File::from_raw_fd(master) };
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    let mut drain = |m: &mut std::fs::File, out: &mut Vec<u8>| {
        while let Ok(n) = m.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
    };
    std::thread::sleep(std::time::Duration::from_millis(300));
    drain(&mut m, &mut out);
    m.write_all(input.as_bytes()).unwrap();
    let sent = std::time::Instant::now();
    while sent.elapsed() < std::time::Duration::from_secs(2) {
        drain(&mut m, &mut out);
        if String::from_utf8_lossy(&out).contains("[right]") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    drain(&mut m, &mut out);
    let _ = child.kill();
    let _ = child.wait();
    drain(&mut m, &mut out);
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn rprompt_is_drawn_at_the_right_edge() {
    let out = on_terminal("PROMPT='> '\nRPROMPT='[right]'\n", "echo hi");
    // 60 columns, 7 wide, one column of indent: column 53 (1-based).
    assert!(out.contains("\x1b[53G[right]"), "output: {:?}", out);
}

#[test]
fn paste_after_quoted_insert_runs_the_line_as_drawn() {
    // Ctrl-V, then a bracketed paste: the start marker used to stay in
    // the word and fail as a bad pattern.
    let out = on_terminal(
        "PROMPT='> '\n",
        "print -r -- A \x16\x1b[200~bio\x1b[201~ \"[ri\"\"ght]\"\r",
    );
    assert!(out.contains("A bio [right]"), "output: {:?}", out);
}

#[test]
fn pasted_control_characters_are_neither_drawn_nor_run() {
    // Backspaces in a paste used to be drawn raw, so the terminal showed
    // `evil` while `safe^H^H^H^Hevil` ran.
    let out = on_terminal(
        "PROMPT='> '\n",
        "print -r -- \x1b[200~safe\x08\x08\x08\x08evil\x1b[201~; print \"[ri\"\"ght]\"\r",
    );
    assert!(out.contains("\r\nsafeevil\r\n"), "output: {:?}", out);
    assert!(!out.contains("safe\x08"), "output: {:?}", out);
}

#[test]
fn input_typed_ahead_of_the_prompt_is_kept() {
    // Both lines arrive in one read; the second used to be dropped.
    let out = on_terminal(
        "PROMPT='> '\n",
        "print one\rprint two\rprint \"[ri\"\"ght]\"\r",
    );
    assert!(out.contains("\r\ntwo\r\n"), "output: {:?}", out);
}

#[test]
fn input_typed_ahead_of_a_command_goes_to_that_command() {
    // All three lines arrive in one read. The second belongs to `read`;
    // the editor used to take it and run it as a command afterwards.
    let out = on_terminal(
        "PROMPT='> '\n",
        "read -r v\rtyped\rprint \"v=$v [ri\"\"ght]\"\r",
    );
    assert!(out.contains("v=typed [right]"), "output: {:?}", out);
    assert!(!out.contains("command not found"), "output: {:?}", out);
}

#[test]
fn history_file_lines_carry_their_start_time() {
    let out = on_terminal(
        "PROMPT='> '\n",
        "print one\rprint -r -- \"n=$(grep -c '^: [0-9]*:0;print one$' ~/.meowsh_history) [ri\"\"ght]\"\r",
    );
    assert!(out.contains("n=1 [right]"), "output: {:?}", out);
}

#[test]
fn no_history_file_without_startup_files() {
    let count = "print one\rprint -r -- \"own=$(ls -a ~ | grep -c meowsh_history) [ri\"\"ght]\"\r";
    // -f: nothing is saved, as in zsh.
    let out = on_terminal_with(&["-f"], "", count);
    assert!(out.contains("own=0 [right]"), "output: {:?}", out);
    // Otherwise the history goes to meowsh's own file, whatever HISTFILE
    // says.
    let out = on_terminal("HISTFILE=~/h\nPROMPT='> '\n", count);
    assert!(out.contains("own=1 [right]"), "output: {:?}", out);
}

#[test]
fn read_k_honours_its_timeout() {
    // The harness for zsh's ZLE tests waits this way between keys; it
    // used to block until the writer went away.
    let (_, out, _) = sh("typeset -F SECONDS; s=$SECONDS; \
         { sleep 2 >/dev/null 2>&1 & } | read -t 0.05 -u 0 -k 1; r=$?; \
         print $r $(( SECONDS - s < 1 ))");
    assert_eq!(out, "1 1\n");
}

#[test]
fn rc_expand_and_glob_subst_flags_combine() {
    let (_, out, err) = sh("a=(x .); print -r ${^~a:/.}");
    assert_eq!((out.as_str(), err.as_str()), ("x\n", ""));
}

#[test]
fn quoted_nested_split_keeps_empty_fields() {
    let (_, out, _) = sh("x=:a:b; print -l \"${(@)${(@s.:.)x}[1,2]}\"");
    assert_eq!(out, "\na\n");
}

#[test]
fn arithmetic_output_base_may_come_from_a_parameter() {
    let (_, out, _) = sh("f='##8'; print $(( [$f] 8#7 + 1 ))");
    assert_eq!(out, "10\n");
}

#[test]
fn rc_expand_param_drops_words_with_empty_arrays() {
    let (_, out, _) = sh("setopt rcexpandparam; a=(); f=( \"$a[@]\" x$a ); b=(1 2); \
         g=( \"x$b[@]y\" ); print ${#f} $g");
    assert_eq!(out, "0 x1y x2y\n");
}

#[test]
fn process_substitution_is_literal_inside_braces() {
    let (_, out, err) = sh("setopt extendedglob; s='<LC><DI><RC>'; \
         print ${(N)s#*(#b)<LC><(??)><RC>} $match[1]; cat <(print ok)");
    assert_eq!((out.as_str(), err.as_str()), ("12 DI\nok\n", ""));
}

#[test]
fn zsh_parameter_module_specials() {
    let (_, out, _) = sh(
        "x=1; print $+builtins[:] $+builtins[nosuch] ${parameters[x]} \
         $+commands[sh]",
    );
    assert_eq!(out, "1 0 scalar 1\n");
}

#[test]
fn zmodload_e_reports_missing_modules() {
    let (_, out, _) = sh("zmodload -e zsh/compctl; a=$?; zmodload -e zsh/main; print $a $?");
    assert_eq!(out, "1 0\n");
}

#[test]
fn argv_assignments_edit_the_positional_parameters() {
    let (_, out, _) = sh("f() { argv+=(h:$1); argv[1]=x; print -r -- $# $@ }; f a b");
    assert_eq!(out, "3 x b h:a\n");
}

#[test]
fn set_a_takes_every_later_word_as_a_value() {
    let (_, out, _) = sh("o=(); set -A a \"$o[@]\" -J y; print -r -- $#a $a");
    assert_eq!(out, "2 -J y\n");
}

#[test]
fn typeset_plus_fm_lists_matching_functions() {
    let (_, out, _) = sh("_q() {}; _x() {}; y() {}; print -r -- $(typeset +fm '_*')");
    assert_eq!(out, "_q _x\n");
}

#[test]
fn indirect_keys_and_values_of_an_association() {
    let (_, out, _) = sh("typeset -A h; h=(a 1 b 2); n=h; print -r -- ${(@Pkv)n} / ${(@Pk)n}");
    assert_eq!(out, "a 1 b 2 / a b\n");
}

#[test]
fn tilde_join_separator_is_pattern_text() {
    let (_, out, _) = sh("f=(/a/_x /a/_y /b/_z); a=(_x _z); print -r -- ${(M)f:#*/(${(~j.|.)a})}");
    assert_eq!(out, "/a/_x /b/_z\n");
}

#[test]
fn quoted_outer_expansion_joins_a_nested_at_list() {
    let (_, out, _) = sh("f() { print -rl -- \"x ${${(@q+)@}} y\" }; f a 'b c'");
    assert_eq!(out, "x a 'b c' y\n");
}

#[test]
fn zformat_formats_and_aligns() {
    let (_, out, _) = sh(
        "zformat -f r '<%d:%-5n>' d:file n:ab; zformat -a l . aa:x b:y; \
         print -r -- $r $l",
    );
    assert_eq!(out, "<file:   ab> aa.x b .y\n");
}

#[test]
fn close_brace_may_follow_a_quoted_word() {
    let (_, out, _) = sh("f () { echo -n 'a'}; f; { echo \"b\"}; { echo '}'}");
    assert_eq!(out, "ab\n}\n");
}

#[test]
fn a_lone_semicolon_is_an_empty_command() {
    let (st, out, _) = sh("; echo a; ; echo b; f () { echo c; ; }; f");
    assert_eq!((st, out.as_str()), (0, "a\nb\nc\n"));
}

#[test]
fn braces_expand_as_the_first_array_element() {
    let (_, out, _) = sh("a=({a,b} x); local -a c; c=( {1..2} ); print -r -- $a $c");
    assert_eq!(out, "a b x 1 2\n");
}

#[test]
fn dollar_bracket_arithmetic() {
    let (_, out, _) = sh("a=3; e=$[ $a - 1 ]; print -r -- $e $[2*3] \"$[4]\"");
    assert_eq!(out, "2 6 4\n");
}

#[test]
fn quoted_subscripts_may_contain_blanks() {
    let (_, out, _) = sh("p=1; x=abcd; print -r -- \"[$x[p + 1, -2]]\"");
    assert_eq!(out, "[bc]\n");
}

#[test]
fn glob_group_words_after_a_command_word() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("foo"), "").unwrap();
    let (_, out, _) = sh(&format!(
        "cd {}; print (foo|bar) x; x=(''); setopt nomatch; print -r -- $^x(N) end",
        dir.path().display()
    ));
    assert_eq!(out, "foo x\nend\n");
}

#[test]
fn loading_an_unavailable_module_fails() {
    let (st, _, err) = sh("zmodload zsh/zftp");
    assert_eq!(st, 1);
    assert!(err.contains("failed to load module `zsh/zftp'"), "{}", err);
}

#[test]
fn subscript_flag_p_splits_on_nul() {
    let (_, out, _) = sh("s=$'foo\\0bar'; print -r -- ${s[(pws:\\0:)2]}");
    assert_eq!(out, "bar\n");
}

#[test]
fn search_flags_as_assignment_range() {
    let (_, out, _) = sh("a=(x y z); a[(R)none,(r)none]=(n m); print -r -- $a");
    assert_eq!(out, "n m\n");
}

#[test]
fn module_builtins_work_until_zmodload_manages_the_module() {
    let (_, out, _) = sh("TZ=UTC0; strftime -s t %Y 0; print -r -- $t; \
         zmodload -F zsh/datetime -b:strftime; strftime %Y 0 2>/dev/null || print gone");
    assert_eq!(out, "1970\ngone\n");
}

#[test]
fn private_parameters_are_hidden_from_called_functions() {
    let (_, out, _) = sh("a=top; f() { private a=mine; g; print -r -- $a }; \
         g() { print -r -- ${a:-unset} }; f; print -r -- $a");
    assert_eq!(out, "top\nmine\ntop\n");
}

#[test]
fn local_dash_capital_p_is_private() {
    let (_, out, _) = sh("a=top; f() { local -P a=mine; g }; g() { print -r -- $a }; f");
    assert_eq!(out, "top\n");
}

#[test]
fn nested_p_flag_keeps_the_parameter_for_outer_flags() {
    let (_, out, _) = sh(
        "typeset -A h=(k v); f() { print -r -- ${(k)${(P)1}} ${${(P)1}[k]} \
         ${(t)${(P)1}} }; f h",
    );
    assert_eq!(out, "k v association\n");
}

#[test]
fn zparseopts_errors_name_the_running_function() {
    let (_, _, err) = sh("f() { local -a o; zparseopts -F -a o - a }; f -x");
    assert_eq!(err, "f: bad option: -x\n");
}

#[test]
fn assigning_to_functions_defines_them() {
    let (_, out, _) = sh("functions[a]='echo foo'; a; functions+=(a 'echo bar'); a");
    assert_eq!(out, "foo\nbar\n");
}

#[test]
fn rehash_is_a_builtin() {
    let (st, _, _) = sh("rehash");
    assert_eq!(st, 0);
}

#[test]
fn funcsourcetrace_names_definitions() {
    let dir = TempDir::new().unwrap();
    let script = dir.path().join("s.zsh");
    std::fs::write(
        &script,
        "f() {\n eval 'print -r -- $funcsourcetrace $funcfiletrace'\n}\nf\n",
    )
    .unwrap();
    let (_, out, _) = sh(&format!("cd {} && . ./s.zsh", dir.path().display()));
    // The sourced file's own frame: defined at line 0, run from the
    // -c string (not from itself).
    let rest = out
        .strip_prefix("./s.zsh:2 ./s.zsh:1 ./s.zsh:0 ./s.zsh:2 ./s.zsh:4 ")
        .unwrap_or_else(|| panic!("{}", out));
    assert!(!rest.starts_with("./s.zsh"), "{}", out);
}

#[test]
fn equals_expansion_in_assignments_and_after_braces() {
    let (_, out, _) = sh(
        "x==sh; [[ $x == /* ]] && print a; print ={sh,} | grep -c '^/'; \
         foo='=sh'; print -r -- ${~foo} | grep -c '^/'",
    );
    assert_eq!(out, "a\n1\n1\n");
}

#[test]
fn second_rc_expand_combines_with_every_field() {
    let (_, out, _) = sh("a=(1 2); b=(x y); print -r -- ${^a}-${^b}");
    assert_eq!(out, "1-x 1-y 2-x 2-y\n");
}

#[test]
fn dollar_quote_drops_backslash_of_unknown_escape() {
    let (_, out, _) = sh("print -r -- $'\\]\\q'; echo '\\q'");
    assert_eq!(out, "]q\n\\q\n");
}

#[test]
fn glob_subst_trailing_backslash_stays_in_its_value() {
    let (_, out, _) = sh("p=$'a\\\\'; v='a\\b'; print -r -- ${v##($~p|x)}");
    assert_eq!(out, "b\n");
}

#[test]
fn typeset_m_lists_in_zsh_hash_order() {
    let (_, out, _) = sh("typeset tqa=1 tqz=2; typeset -m 'tq*'");
    assert_eq!(out, "tqz=2\ntqa=1\n");
}

#[test]
fn private_hidden_from_a_function_rejects_assignment() {
    let (_, out, err) = sh("() { private q=outer; () { q=new; print reached }; print -r -- $q }");
    assert_eq!(out, "");
    assert!(
        err.contains("q: can't modify read-only parameter"),
        "{}",
        err
    );
}

#[test]
fn nofork_sees_its_functions_private() {
    let (_, out, _) = sh("() { private z=outer; print -r -- ${(t)z} ${| REPLY=$z } }");
    assert_eq!(out, "scalar-local-hide-special outer\n");
}

#[test]
fn private_errors_for_specials_locals_and_type_changes() {
    let (_, _, err) = sh("() { private path }");
    assert!(
        err.contains("can't change scope of existing param: path"),
        "{}",
        err
    );
    let (_, _, err) = sh("() { local h=s; private -A h }");
    assert!(
        err.contains("can't change scope of existing param: h"),
        "{}",
        err
    );
    let (_, _, err) = sh("() { private x=1; private -a x }");
    assert!(
        err.contains("can't change type of private param: x"),
        "{}",
        err
    );
    let (_, _, err) = sh("() { private x; x=(a b) }");
    assert!(
        err.contains("x: attempt to assign array value to non-array"),
        "{}",
        err
    );
}

#[test]
fn private_survives_unset_and_redeclaration() {
    let (_, out, _) = sh("() { private x=1; unset x; private x=2; print -r -- $x }");
    assert_eq!(out, "2\n");
}

#[test]
fn exported_private_reaches_commands_run_by_called_functions() {
    let (_, out, _) = sh("inner() { print $+v; sh -c 'echo X $v' }; () { private -x v=w; inner }");
    assert_eq!(out, "0\nX w\n");
}

#[test]
fn unset_local_has_no_type() {
    let (_, out, _) = sh("f() { local x; unset x; print -r -- A${(t)x} }; f");
    assert_eq!(out, "A\n");
}

#[test]
fn tied_partner_shadowed_by_a_local_stays_tied() {
    let (_, out, _) = sh("typeset -T VAR var=(a b c b a); \
         () { typeset -i var=1; typeset -g -U VAR; typeset -p VAR var }; typeset -p VAR var");
    assert_eq!(
        out,
        "typeset -g -UT VAR var=1\ntypeset -i var=1\n\
         typeset -UT VAR var=( a b c )\ntypeset -aT VAR var=( a b c )\n"
    );
}

#[test]
fn special_tied_pairs_exist_and_localize_together() {
    let (_, out, _) = sh("print -r -- ${(t)fignore} ${(t)FIGNORE}; \
         f() { typeset MANPATH; manpath=(/ /); typeset -UT MANPATH manpath; print -r -- $manpath }; f");
    assert_eq!(out, "array-tied-special scalar-tied-special\n/\n");
}

#[test]
fn p_and_t_flags_give_the_named_parameters_type() {
    let (_, out, _) = sh("typeset -a arr; v=arr; print -r -- ${(Pt)v} ${(t)v}");
    assert_eq!(out, "array scalar\n");
}

#[test]
fn reference_cannot_change_a_hidden_parameters_type() {
    let (_, out, err) = sh("typeset -a arr=(a b); () { typeset -n R=arr; typeset arr; \
         { R=foo } always { TRY_BLOCK_ERROR=0 }; echo $? }; typeset -p arr");
    assert_eq!(out, "1\ntypeset -a arr=( a b )\n");
    assert!(
        err.contains("can't change type of hidden variable: arr"),
        "{}",
        err
    );
}

#[test]
fn subscript_through_a_reference_uses_the_targets_kind() {
    let (_, out, _) = sh("typeset -A var=(k1 v1); typeset -n ptr=var; ptr[k2]=v2; typeset -p var");
    assert_eq!(out, "typeset -A var=( [k1]=v1 [k2]=v2 )\n");
}

#[test]
fn nameref_errors_name_what_zsh_names() {
    let (_, _, err) = sh("typeset -n ptr[1]=var");
    assert!(
        err.contains("reference variable cannot be an array"),
        "{}",
        err
    );
    let (_, _, err) = sh("typeset -n p; typeset -t p");
    assert!(
        err.contains("p: can't change type of a named reference"),
        "{}",
        err
    );
    let (_, _, err) = sh("typeset -A ass=(a b); typeset -n Ass=ass; typeset Ass=foo");
    assert!(
        err.contains("ass: inconsistent type for assignment"),
        "{}",
        err
    );
    let (_, _, err) =
        sh("typeset ptr1=foo; () { typeset -n ptr1; typeset -n ptr2=ptr1; ptr1=ptr2 }");
    assert!(err.contains("ptr2: invalid self reference"), "{}", err);
}

#[test]
fn array_assignment_sets_a_declared_unset_array() {
    let (_, out, _) = sh("setopt typesettounset; typeset -a x; x=(c d); typeset -p x");
    assert_eq!(out, "typeset -a x=( c d )\n");
}

#[test]
fn pad_width_is_expanded_before_evaluation() {
    let (_, out, _) = sh("w=6; x=ab; print -r -- \"[${(r:$w::-:)x}]\"");
    assert_eq!(out, "[ab----]\n");
}

#[test]
fn nested_a_flag_keeps_an_array_in_double_quotes() {
    let (_, out, _) = sh("o=Option1; print -r -- \"[${${(As:;:)o}[1]}]\"");
    assert_eq!(out, "[Option1]\n");
}

#[test]
fn quoted_array_assignment_expansion_joins() {
    let (_, out, _) = sh("set a 'b c' d; print -l \"${(A)foo::=$@}\"; print -l $foo");
    assert_eq!(out, "a b c d\na\nb c\nd\n");
}

#[test]
fn escaped_newline_after_dollar_paren_can_still_be_arithmetic() {
    let (_, out, _) = sh("eval $'echo $(\\\\\\n( 15 / 3 ))'");
    assert_eq!(out, "5\n");
}

#[test]
fn zle_rprompt_indent_is_an_integer() {
    let (_, out, _) =
        sh("print ${ZLE_RPROMPT_INDENT-unset}; ZLE_RPROMPT_INDENT=; print $ZLE_RPROMPT_INDENT");
    assert_eq!(out, "unset\n0\n");
}

#[test]
fn doubled_tilde_flag_and_doubled_equals_toggle_off() {
    let (_, out, _) = sh(
        "foo=('|' '?'); [[ '|' = ${(~~j.|.)foo} ]] && print yes || print no; \
         emulate sh; a='1 2' b='3 4'; print -l ${==1:-$a $b}",
    );
    assert_eq!(out, "no\n1 2 3 4\n");
}

#[test]
fn typeset_t_ties_an_existing_scalar() {
    let (_, out, _) = sh("S=a:b; typeset -T S s; print -r -- $S $s");
    assert_eq!(out, "a:b a b\n");
    let (st, _, err) = sh("typeset -T t1 t2 +; typeset -T t2 t1 +");
    assert_eq!(st, 1);
    assert!(err.contains("already tied as non-scalar: t2"), "{}", err);
}

#[test]
fn unset_special_tied_pair_stays_tied() {
    let (_, out, _) = sh(
        "unset fignore; print $+FIGNORE; fignore=(a b); print $FIGNORE; \
         unset FIGNORE; FIGNORE=c:d; print $fignore",
    );
    assert_eq!(out, "0\na:b\nc d\n");
}

#[test]
fn negated_y_qualifier_removes_the_limit() {
    let dir = TempDir::new().unwrap();
    for n in ["d1", "d2", "d3"] {
        std::fs::create_dir(dir.path().join(n)).unwrap();
    }
    let (_, out, _) = sh(&format!("cd {} && print d*(Y1^Y)", dir.path().display()));
    assert_eq!(out, "d1 d2 d3\n");
}

#[test]
fn misplaced_count_flag_is_a_bad_pattern() {
    let (st, _, err) = sh("setopt extendedglob; [[ foo = (#c0)foo ]]");
    assert_eq!(st, 2);
    assert!(err.contains("bad pattern: (#c0)foo"), "{}", err);
}

#[test]
fn redirect_to_unmatched_pattern_fails_even_with_n() {
    let dir = TempDir::new().unwrap();
    let (st, _, err) = sh(&format!("cd {} && echo > none(N)", dir.path().display()));
    assert_eq!(st, 1);
    assert!(
        err.contains("redirection failed (no match): none(N)"),
        "{}",
        err
    );
}

#[test]
fn star_flag_enables_extended_glob_for_one_expansion() {
    let (_, out, _) = sh("unsetopt extendedglob; \
         print -r -- ${(*)=${(@s.+.):-A+B}/(#b)(?)/-${(L)match[1]} ${match[1]}}; \
         [[ -o extendedglob ]] && print on || print off");
    assert_eq!(out, "-a A -b B\noff\n");
}

#[test]
fn zsh_namespace_parameters_are_read_only() {
    let (st, _, err) = sh(".zsh.cmdsubst=x");
    assert_eq!(st, 1);
    assert!(
        err.contains(".zsh.cmdsubst: can't modify read-only parameter"),
        "{}",
        err
    );
}

#[test]
fn print_tab_expansion_counts_wide_characters_as_two_columns() {
    let (_, out, _) = sh("print -X8 -r -- $'one\\tＺＳＨ\\tthree'");
    assert_eq!(out, "one     ＺＳＨ  three\n");
}

#[test]
fn one_command_function_keeps_the_callers_line() {
    let (_, _, err) = sh("f() g\ng() :\nfunctions -t f\nf");
    assert_eq!(err, "+f:4> g\n+g:4> :\n");
}

#[test]
fn invalid_namespace_assignment_is_not_an_identifier() {
    for (word, name) in [
        (".k.=x", ".k."),
        (".2.b=x", ".2.b"),
        (".not.2b=x", ".not.2b"),
    ] {
        let (st, _, err) = sh(word);
        assert_eq!(st, 1, "{}", word);
        assert!(
            err.contains(&format!("not an identifier: {}", name)),
            "{}",
            err
        );
    }
    let (st, _, _) = sh(".n.s.k=x");
    assert_eq!(st, 127);
}

#[test]
fn local_hides_epochseconds_unless_unhidden() {
    let (_, out, _) =
        sh("fn() { local EPOCHSECONDS=s; print $EPOCHSECONDS ${(t)EPOCHSECONDS} }; fn");
    assert_eq!(out, "s scalar-local\n");
    let (st, _, err) = sh("typeset +h EPOCHSECONDS; fn() { local EPOCHSECONDS=s }; fn");
    assert_eq!(st, 1);
    assert!(err.contains("read-only variable: EPOCHSECONDS"), "{}", err);
}

#[test]
fn nested_lookup_in_an_association_subscript_keeps_its_own_text() {
    let (_, out, _) = sh(
        "typeset -A A; A[\\\\\\]]=bcb; print -R $A[$A[(i)\\\\\\\\\\]]] ${A[${A[(i)\\\\\\\\\\]]}]}",
    );
    assert_eq!(out, "bcb bcb\n");
}

#[test]
fn dynamic_named_directories() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("foo/bar/rod")).unwrap();
    let (_, out, err) = sh(&format!(
        "cd {} && zsh_directory_name() {{ \
           if [[ $1 = d ]]; then [[ $2 = */bar/rod ]] || return 1; reply=(barmy $(( ${{#2}} - 4 ))); \
           else [[ $2 = barmy ]] || return 1; reply=($PWD/foo/bar); fi }}; \
         print ~[barmy]/x; cd foo/bar/rod; print -P %~; cd ../../..; print ~[nope]/y",
        dir.path().display()
    ));
    assert_eq!(
        out,
        format!("{}/foo/bar/x\n~[barmy]/rod\n", dir.path().display())
    );
    assert!(err.contains("no directory expansion: ~[nope]"), "{}", err);
}

#[test]
fn final_tilde_is_not_an_exclusion() {
    let (_, out, _) = sh("setopt extendedglob; print %~ a~");
    assert_eq!(out, "%~ a~\n");
}

#[test]
fn quoted_string_as_expansion_value() {
    let (_, out, _) = sh("print -l ${=\"$(echo a b)\"}");
    assert_eq!(out, "a\nb\n");
}

#[test]
fn prompt_dates_have_zsh_fraction_escapes() {
    let (_, out, _) = sh("print -P '%D{%9.} %D{%N}'");
    let parts: Vec<&str> = out.trim().split(' ').collect();
    assert_eq!(parts.len(), 2, "{}", out);
    assert!(
        parts
            .iter()
            .all(|p| p.len() == 9 && p.chars().all(|c| c.is_ascii_digit())),
        "{}",
        out
    );
}

#[test]
fn unknown_prompt_colours_reset_to_the_default() {
    let (_, out, _) = sh("print -r -- ${(V)${(%):-'%F{foo}'}} ${(V)${(%):-'%K{reset}'}}");
    assert_eq!(out, "^[[39m ^[[49m\n");
}

#[test]
fn cat_in_a_pipeline_brace_group_reads_its_own_redirection() {
    let (_, out, _) = sh("echo 1 | { cat </dev/null; read b; print B$b }");
    assert_eq!(out, "B1\n");
}

#[test]
fn equals_subst_files_use_tmpprefix() {
    let dir = TempDir::new().unwrap();
    let (_, out, _) = sh(&format!(
        "TMPPREFIX={}/eq; () {{ print -r -- ${{1:h}} }} =(true)",
        dir.path().display()
    ));
    assert_eq!(out, format!("{}\n", dir.path().display()));
}

#[test]
fn history_file_round_trip_with_backslashes_and_newlines() {
    let dir = TempDir::new().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .args(["-fgis"])
        .env("PS1", "")
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(b"  print -rs 'one\\'\n  print -s 'a\\nb'\n  fc -W hist\n  fc -p -R hist\n  fc -l\n")
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "    1  one\\\n    2  a\\nb\n"
    );
}

#[test]
fn funcnest_error_names_the_function() {
    let (_, _, err) = sh("FUNCNEST=0; fn() { true; }; fn");
    assert!(
        err.starts_with("fn:1: maximum nested function level reached"),
        "{}",
        err
    );
}

#[test]
fn ignore_braces_needs_closing_brace_in_command_position() {
    let (st, _, err) = sh("setopt ignorebraces; eval '{ echo OK }'");
    assert_eq!(st, 1);
    assert!(err.contains("parse error near `}'"), "{}", err);
    let (_, out, _) = sh("setopt ignorebraces; eval '{ echo OK; }'");
    assert_eq!(out, "OK\n");
}

#[test]
fn autoload_in_emulate_c_is_sticky_and_parse_errors_name_the_function() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("emu"),
        "{ echo OK }\n[[ -o ignorebraces ]] || print off\n",
    )
    .unwrap();
    let (_, out, err) = sh(&format!(
        "cd {} && setopt ignorebraces; fpath=(.); (autoload -z emu; emu); \
         emulate zsh -c 'autoload -Uz emu'; emu; [[ -o ignorebraces ]] && print on",
        dir.path().display()
    ));
    assert_eq!(out, "OK\noff\non\n");
    assert_eq!(err, "emu:3: parse error near `\\n'\n");
}

#[test]
fn hist_subst_pattern_uses_patterns_anchors_and_match() {
    let (_, out, _) = sh("setopt histsubstpattern extendedglob; foo=(one.c two.c three.c); \
         print ${foo:s/#%(#b)t(*).c/T${match[1]}.X/}; x='a{}b[]c'; print ${x:fs/(\\\\{\\\\}|\\\\[\\\\])//}");
    assert_eq!(out, "one.c Two.X Three.X\nabc\n");
}

#[test]
fn glob_qualifier_substitution_sorts_the_modified_names() {
    let dir = TempDir::new().unwrap();
    for f in ["tmpcd", "tmpfile1", "tmpfile2", "homedir"] {
        std::fs::write(dir.path().join(f), "").unwrap();
    }
    let (_, out, _) = sh(&format!(
        "cd {} && setopt histsubstpattern extendedglob; \
         print *(#q:s/#(#b)tmp(*e)/'scrunchy${{match[1]}}'/)",
        dir.path().display()
    ));
    assert_eq!(out, "homedir scrunchyfile1 scrunchyfile2 tmpcd\n");
}

#[test]
fn glob_subst_backslash_quotes_the_next_character() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join("A(B)")).unwrap();
    let (_, out, _) = sh(&format!(
        "cd {} && x='A(B)'; y=${{(q)x}}; print -l $~y*",
        dir.path().display()
    ));
    assert_eq!(out, "A(B)\n");
}

#[test]
fn keys_flag_with_search_subscripts() {
    let (_, out, _) =
        sh("typeset -A h=(path 1 paths 2 x 3 y 3); print ${(k)h[(I)path*]} / ${(k)h[(R)3]} / ${(k)h[(r)3]}");
    assert_eq!(out, "path paths / x y / x\n");
}

#[test]
fn status_is_a_read_only_special() {
    let (_, out, _) = sh("false; print $status; typeset -p status");
    assert_eq!(out, "1\ntypeset -i10 -r status=0\n");
    let (st, out, err) = sh("f() { local status; typeset -p status; status=3 }; f; print after");
    assert_eq!(st, 1);
    assert_eq!(out, "typeset -i10 -r status=0\n");
    assert!(err.contains("read-only variable: status"), "{}", err);
    let (_, out, _) = sh("f() { local -h status; typeset -p status }; f");
    assert_eq!(out, "typeset -h status=''\n");
}

#[test]
fn failed_module_autoloads_stop_the_command_line() {
    for (code, st) in [
        ("zmodload -u zsh/parameter; zmodload -aF zsh/parameter b:fail; fail; print reached", 1),
        ("zmodload -u zsh/parameter; zmodload -aF zsh/parameter p:fail; print $fail; print reached", 1),
        ("zmodload -u zsh/parameter; zmodload -aF zsh/parameter c:fail; [[ -fail x ]]; print reached", 2),
    ] {
        let (s, out, err) = sh(code);
        assert_eq!(s, st, "{}", code);
        assert_eq!(out, "", "{}", code);
        assert!(err.contains("autoload cancelled"), "{}", err);
    }
}

#[test]
fn positional_arrays_take_a_subscript_without_braces() {
    let (_, out, _) = sh("set a b c; print $@[2,-1] / $*[2] / \"$@[1,2]\"");
    assert_eq!(out, "b c / b / a b\n");
}

#[test]
fn disabling_a_parameter_feature_removes_it() {
    let (_, out, _) = sh(
        "zmodload zsh/system; print ${+errnos}; zmodload -F zsh/system -p:errnos; \
         print ${+errnos}; zmodload -F zsh/system +p:errnos; print ${+errnos}",
    );
    assert_eq!(out, "1\n0\n1\n");
}

#[test]
fn zsh_parameter_reloads_over_alias_and_option_tables() {
    let (st, _, err) = sh("alias x=y; zmodload -u zsh/parameter; zmodload zsh/parameter");
    assert_eq!(st, 0, "{}", err);
}

#[test]
fn sourced_code_traces_to_the_shells_stderr() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("xt.in"), "print \"$*\"\n").unwrap();
    let (_, out, err) = sh(&format!(
        "cd {} && PS4='+%N:%i> '; set -x; . ./xt.in hi 2>>e; set +x; cat e",
        dir.path().display()
    ));
    assert_eq!(out, "hi\n");
    assert!(err.contains("+./xt.in:1> print hi"), "{}", err);
}

#[test]
fn functions_called_from_one_command_functions_count_their_own_lines() {
    let (_, _, err) = sh("f() g\ng() { () : }\nfunctions -t f\nf");
    assert_eq!(err, "+f:4> g\n+g:0> '(anon)'\n+(anon):0> :\n");
}

#[test]
fn alias_expanding_to_a_closing_brace_ends_the_group() {
    let (_, out, _) = sh("alias OPEN='{' CLOSE='};'; eval 'OPEN print hi; CLOSE'");
    assert_eq!(out, "hi\n");
}

#[test]
fn epochseconds_goes_with_its_module() {
    let (_, out, _) = sh("zmodload zsh/datetime; fn() { local EPOCHSECONDS=s; zmodload -u zsh/datetime; \
         print $EPOCHSECONDS }; fn; print ${+EPOCHSECONDS}; EPOCHSECONDS=(a b); print -l $EPOCHSECONDS");
    assert_eq!(out, "s\n0\na\nb\n");
}

#[test]
fn computed_specials_report_zsh_types() {
    let (_, out, _) = sh(
        "zmodload zsh/datetime; for v in RANDOM EPOCHSECONDS EPOCHREALTIME LINENO USERNAME; \
         do print ${(tP)v}; done",
    );
    assert_eq!(
        out,
        "integer-special\ninteger-readonly-hide-hideval-special\n\
         float-readonly-hide-hideval-special\ninteger-readonly-special\nscalar-special\n"
    );
}

#[test]
fn nofork_parse_errors_stop_the_command() {
    let (st, out, err) = sh("print ${| REPLY=x}}");
    assert_eq!((st, out.as_str()), (1, ""));
    assert!(err.contains("parse error near `}'"), "{}", err);
    let (_, out, _) = sh("setopt ignorebraces; print ${| REPLY=x}}");
    assert_eq!(out, "x}\n");
    let (st, out, _) = sh("setopt ignorebraces; print ${ { echo nested } } DONE");
    assert_eq!((st, out.as_str()), (1, ""));
}

#[test]
fn csh_junkie_quotes_end_at_a_newline() {
    let (_, out, err) =
        sh("setopt cshjunkiequotes; eval \"print 'a\nb'\"; eval \"print 'c\\\\\nd'\"");
    assert_eq!(out, "c\nd\n");
    assert_eq!(err.matches("unmatched '").count(), 1, "{}", err);
}

#[test]
fn subshell_keeps_zerr_trap_functions() {
    let (_, out, _) = sh("TRAPZERR() { print trapped; }; (false; true); (false)");
    assert_eq!(out, "trapped\ntrapped\ntrapped\n");
    let (_, out, _) = sh("trap 'print no' USR1; (trap)");
    assert_eq!(out, "");
}

#[test]
fn local_loops_warning_comes_from_the_caller() {
    let (_, _, err) =
        sh("g() {\n setopt localloops\n for i in 1; do\n  () {\n   break\n  }\n done\n}\ng");
    assert!(
        err.contains("g:3: `break' active at end of function scope"),
        "{}",
        err
    );
}

#[test]
fn parameter_flags_q_g_qx_and_seconds() {
    let (_, out, _) = sh("foo='\\u65\\0123^X\\C-x'; print -r -- ${(g::)foo}; \
         bar='\\u65\\123'; print -r -- ${(g:o:)bar}; \
         q='playing '\\''stupid'\\'' \"games\"'; print -r -- ${(q-)q}; \
         s=\"three so-called 'words'\"; print -r -- ${(q+)s}; \
         SECONDS=8888; print $(( SECONDS >= 8888 ))");
    assert_eq!(
        out,
        "eS^X\\C-x\neS\n'playing '\\'stupid\\'' \"games\"'\n'three so-called '\\''words'\\'\n1\n"
    );
    let (st, _, err) = sh("u='unmatched \"'; print ${(QX)u}");
    assert_eq!(st, 1);
    assert!(err.contains("unmatched \""), "{}", err);
}

#[test]
fn tied_unset_and_shlvl_implicit_exec_and_noexec() {
    let (_, out, _) = sh(
        "f() { local S=a:b; typeset -T S s; unset S; set -A s x y; print $S $s; unset s; S=m:n; print $S $s; }; f",
    );
    assert_eq!(out, "x y\nm:n\n");
    let exe = env!("CARGO_BIN_EXE_meowsh");
    let (_, out, _) = sh(&format!("SHLVL=1 {} -fc 'echo $SHLVL'", exe));
    assert_eq!(out, "2\n");
    let (st, _, err) = sh("(setopt noexec\n: ${(el.20..X.)$bar})");
    assert_eq!(st, 1);
    assert!(err.contains("bad substitution"), "{}", err);
}

#[test]
fn aliases_short_loops_and_ignoreclosebraces() {
    let (_, out, _) = sh("setopt ignoreclosebraces; alias OPEN='{' CLOSE='};'; \
         eval '{ OPEN print hi; CLOSE }\nvar=$({ OPEN print bye; CLOSE}) && print $var'; \
         alias WI='while {false}'; eval 'echo $(WI blah)'");
    assert_eq!(out, "hi\nbye\n\n");
    let (st, out, err) = sh("print something=${:-=(echo 'C,D),(F,G)'}");
    assert_eq!((st, out.as_str()), (1, ""));
    assert!(err.contains("unterminated `=(...)'"), "{}", err);
}

#[test]
fn glob_approx_pattern_disables_realpath_and_qualifiers() {
    let tmp = tempfile::TempDir::new().unwrap();
    // macOS temp dirs live behind the /var -> /private/var symlink.
    let real = std::fs::canonicalize(tmp.path()).unwrap();
    let d = real.to_str().unwrap();
    std::fs::create_dir_all(format!("{}/dir3/subdir", d)).unwrap();
    std::fs::create_dir_all(format!("{}/hello/world", d)).unwrap();
    std::os::unix::fs::symlink("dir3/subdir", format!("{}/link", d)).unwrap();
    std::fs::write(format!("{}/a", d), "").unwrap();
    let (_, out, _) = sh(&format!(
        "setopt extendedglob; \
         [[ dcba = (#a3)abcd && aabaXaaabY = (#a1)(a#b)(a#b)Y ]] && echo ok; \
         f() {{ emulate -L zsh; disable -p '*'; [[ 'f*g' = $~1 ]] && echo off; disable -p; }}; \
         f 'f*g'; disable -p; \
         p='{}/link/../../hello/world'; print -r -- ${{p:P}}; \
         for 1 2 in a b c d; do print -r -- \"$1:$2\"; done; \
         cd {}; print -r -- [a](#qe\"|reply=( \\\\* )|\"[1]); \
         print -r -- [a](#qe*reply=( \\\\\\* )*)",
        d, d
    ));
    assert_eq!(
        out,
        format!("ok\noff\n'*'\n{}/hello/world\na:b\nc:d\n*\n*\n", d)
    );
    let (st, _, err) = sh(&format!(
        "setopt extendedglob; cd {}; print -r -- [a](#qe\"*reply=( \\* )*\")",
        d
    ));
    assert_eq!(st, 1);
    assert!(err.contains("unknown file attribute:"), "err={:?}", err);
}

#[test]
fn prompt_xtrace_brace_and_func_quoting() {
    let (_, out, err) = sh("A1=${(%):-%s}; A2=${(%):-%u}; A3=${(%):-%s%u%s}; \
         [[ $A3 = $A1$A2 ]] && echo optok; \
         () { setopt no_multibyte; echo -E {$'\\x80'..$'\\x81'}; }; \
         setopt extendedglob xtrace; \
         f() { local d='~[x:l]'; [[ abc = (#b)(*)/z ]] || true; }; f; unsetopt xtrace; \
         eval \"$'ba\\0z'() { echo inner; }\"; LC_ALL=C which $'ba\\0z'");
    assert!(out.contains("optok\n\\M-^@ \\M-^A\n"), "out={:?}", out);
    assert!(
        out.contains("$'ba\\C-@z' () {\n\techo inner\n}\n"),
        "out={:?}",
        out
    );
    assert!(err.contains("local d='~[x:l]'"), "err={:?}", err);
    assert!(err.contains("= (#b)(*)/z ]]"), "err={:?}", err);
    let (st, _, err) = sh("print ${:-\"${| REPLY=${REPLY:-x}\"}");
    assert_eq!(st, 1);
    assert!(err.contains("unmatched \""), "err={:?}", err);
    let (st2, _, err2) = sh("print ${:-\"${| REPLY=${REPLY:-x}\"}\"}");
    assert_eq!(st2, 1);
    assert!(err2.contains("unmatched \""), "err2={:?}", err2);
}

#[test]
fn modules_commands_quote_subscript_and_eval_error_compat() {
    // V01zmodload: autoloaded readonly module params report read-only on unset.
    let (st, out, err) = sh("unset builtins 2>&1; print \"st=$?\"; \
         zmodload -F zsh/parameter -p:builtins; unset builtins; print \"u=$?\"");
    assert_eq!(st, 0, "err={:?}", err);
    assert!(
        out.contains("read-only") && out.ends_with("st=1\nu=0\n"),
        "out={:?} err={:?}",
        out,
        err
    );

    // V06parameter: $commands respects hashcmds and rehash.
    let (_, out, _) = sh(
        "print ${+commands[sh]}; unsetopt hashcmds; PATH=/nonexistent; \
         rehash; print \"${+commands[sh]} ${#commands}\"",
    );
    assert_eq!(out, "1\n0 0\n");

    // V12zparseopts & Y01completion: (q+) quotes `=` anywhere, not `%`, and `quote_value` wraps `'`.
    let (_, out, _) = sh(
        "a=('--foo=' '%pct' \"a'b\"); print -rl -- ${(q+)a}; \
         b=(\"a'b\"); typeset -p b; \
         f() { :; }; print \"<${(k)functions[f]}>|<${(k)functions[missing]}>\"; \
         argv=(a - b); (( minus = argv[(ib:2:)-] )); print \"m=$minus\"; \
         setopt bareglobqual nullglob; g() { eval local \"x;x=(( a b ))\"; print \"after=$? +x=$+x t=${(t)x}\"; }; g",
    );
    assert_eq!(
        out,
        "'--foo='\n%pct\n'a'\\''b'\ntypeset -a b=( 'a'\\''b' )\n<f>|<>\nm=2\nafter=1 +x=1 t=scalar-local\n"
    );
}

#[test]
fn private_and_nameref_scope_compat() {
    // V10private: `private +`, private-hidden readonly via `exec {fd}` and nameref,
    // and bare integer/float default values + typeset listing of stashed flags.
    let (st, out, err) = sh("zmodload zsh/param/private; \
         x=ok; () { private + x; } 2>/dev/null; \
         () { private -hr r=init; () { r=XX; print AFTER; } 2>&1; }; \
         () { private -hr fd=1; exec {fd}>&2; print AFTER_FD; } 2>/dev/null");
    assert_eq!(st, 1, "out={:?} err={:?}", out, err);
    assert!(out.contains("read-only"), "out={:?}", out);
    assert!(!out.contains("AFTER"), "out={:?}", out);

    // K01nameref: self-preferring `local -n ref=ref`, `-nu` missing-upper non-abort,
    // invalid variable name / identifier aborts, `$_` on `local -n`, bang quoting,
    // `foo.4ar` namespace, and `emulate ksh -c` frozen LINENO.
    let (st, out, err) = sh("ref=enclosing; \
         () { local -n ref=ref; print \"self=$ref\"; }; \
         () { local v=inner; local -nu p=v; p=changed; print \"st=$? p=$p v=$v\"; }; \
         () { local -n ref=$1; print \"under=$_\" } _; \
         () { local -n x='!'; typeset -p x }; \
         () { local -n -- okref=foo.4ar; print \"ns=${(P)+okref}:${!okref}\" }; \
         { local -n -- badref=. } always { e1=$TRY_BLOCK_ERROR; TRY_BLOCK_ERROR=0 }; \
         { local -n -- . } always { e2=$TRY_BLOCK_ERROR; TRY_BLOCK_ERROR=0 }; \
         print \"errs=$e1:$e2\"; \
         emulate ksh -c 'print \"ln1=$LINENO\"; print \"ln2=$LINENO\"' 2>/dev/null");
    assert_eq!(st, 0, "out={:?} err={:?}", out, err);
    assert_eq!(
        out,
        "self=enclosing\nst=1 p= v=inner\nunder=-n\ntypeset -n x=!\nns=0:foo.4ar\nerrs=1:1\nln1=1\nln2=1\n"
    );
}

#[test]
fn raw_bytes_multibyte_zstat_and_zformat_compat() {
    let dir = std::env::temp_dir().join(format!("meowsh_zstat_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_str().unwrap().to_string();

    let (st, out, err) = sh(&format!(
        "export LC_ALL=en_US.UTF-8; \
         setopt multibyte; \
         read -ed $'\\xc2' <<<$'one\\xc2\\xa3two'; \
         foo=$'X\\xc0Y\\x07Z\\x7fT'; print -r -- \"q=${{(q)foo}}\"; \
         printf 'px=%x\\n' $(printf '\"\\xf0'); \
         [[ $'\\xcc' != $'\\ucc' && $'\\xcc' = [$'\\xcc'] && $'\\xcc' != [$'\\ucc'] ]] && print raw_cmp; \
         [[ $'\\xe3' == [[:INCOMPLETE:]] && $'\\xe3\\x83' == [[:INCOMPLETE:]][[:INVALID:]] && $'\\xe3\\x83\\x9b' = ? ]] && print utf_class; \
         [[ $'\\ua0' =~ '^.$' ]] && print regex_mb; \
         (LC_ALL=C; print $'\\u00e9') 2>&1 | grep -q 'character not in range' && print c_range; \
         (LC_ALL=C; [[ ${{(#):-0x80}} = $'\\x80' ]] && print c_hash); \
         zmodload zsh/stat zsh/zutil; \
         x=$'\\xff'; zformat -F REPLY \"<%$x>\" \"$x:$x\" && print -r -- \"zf=${{(V)REPLY}}\"; \
         zformat -F REPLY '' 'é:' 2>&1 | sed 's/.*: //'; \
         cd '{d}' && : > 'f-é' && typeset -A sizes && zstat +size -A sizes -nor -- 'f-é' && \
         [[ -N 'f-é' ]] && print \"zstat=${{sizes[f-é]}}\"",
    ));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(st, 0, "out={:?} err={:?}", out, err);
    assert_eq!(
        out,
        "one\nq=X$'\\300'Y$'\\a'Z$'\\177'T\npx=f0\nraw_cmp\nutf_class\nregex_mb\nc_range\nc_hash\nzf=<\\M-^?>\n\\M-C\\M-):\nzstat=0\n"
    );
}

#[test]
fn zstat_type_name_flags_compat() {
    // oh-my-zsh does `mtime=$(zstat +mtime f); (( mtime + 1 ))`, which recursed
    // when a picked element printed as `mtime   <value>`.
    let (st, out, err) = sh("zmodload -F zsh/stat b:zstat; f=/; \
         m=$(zstat +mtime $f); (( m + 1 > 1 )) && print bare; \
         zstat -t +nlink $f | sed 's/[0-9]*$/N/'; \
         zstat +nlink $f $f | sed 's/[0-9]*$/N/'; \
         zstat -T $f | head -1 | sed 's/[0-9]*$/N/'; \
         zstat $f $f | sed -n '1p;2s/[0-9]*$/N/p'; \
         zstat -t -A a +nlink $f; print -r -- \"${a%% *}\"; \
         zstat -A a +nlink $f; [[ $a = <-> ]] && print num");
    assert_eq!(st, 0, "out={:?} err={:?}", out, err);
    assert_eq!(
        out,
        "bare\nnlink N\n/ N\n/ N\nN\n/:\ndevice  N\nnlink\nnum\n"
    );
}

#[test]
fn zstat_raw_and_string_forms_compat() {
    let (st, out, err) = sh("zmodload -F zsh/stat b:zstat; \
         zstat -r +mode /; zstat -s +mode /; zstat -or +mode /; zstat -o +mode /; \
         zstat -s +uid /; zstat -r +uid /; \
         [[ $(zstat -F %Y +mtime /) = <2000-9999> ]] && print year; \
         [[ $(zstat -rF %Y +mtime /) = <->' ('<2000-9999>')' ]] && print both; \
         [[ $(zstat -g +mtime /) = *' '* ]] && print gmt; \
         zstat / /dev/null | grep -c '^$'; \
         zstat +nlink /nonexistent / 2>/dev/null | sed 's/[0-9]*$/N/'; print $pipestatus[1]");
    assert_eq!(st, 0, "out={:?} err={:?}", out, err);
    assert_eq!(
        out,
        "16877 (drwxr-xr-x)\ndrwxr-xr-x\n040755 (drwxr-xr-x)\n040755\nroot\n0 (root)\n\
         year\nboth\ngmt\n1\n/ N\n1\n"
    );
}

#[test]
fn zwc_compile_and_invalid_description_compat() {
    use std::time::{Duration, UNIX_EPOCH};
    let dir = TempDir::new().unwrap();
    let script = dir.path().join("zmod.zsh");
    let zwc = dir.path().join("zmod.zsh.zwc");
    std::fs::write(&script, "zwc_fn() { echo from_zwc:$1; }\n").unwrap();

    let (st, out, err) = sh(&format!(
        "zcompile {zwc} {script}\nzcompile -t {zwc}\necho ok\n",
        zwc = zwc.display(),
        script = script.display(),
    ));
    assert_eq!(st, 0, "out={:?} err={:?}", out, err);
    assert_eq!(out, "ok\n", "err={:?}", err);

    let zwc_bytes = std::fs::read(&zwc).unwrap();
    assert!(zwc_bytes.len() > 60 && zwc_bytes[0..4] == 0x0405_0607u32.to_ne_bytes());
    assert!(
        zwc_bytes.windows(4).any(|w| w == b"MWC1"),
        "expected compiled MWC1 wordcode payload in .zwc"
    );

    // Keep source mtime older than .zwc so source loads from the compiled file.
    std::fs::write(&script, "CORRUPTED\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&script)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000_000))
        .unwrap();
    let (st_zwc, out_zwc, err_zwc) = sh(&format!(
        "source {script}\nzwc_fn ok\n",
        script = script.display(),
    ));
    assert_eq!(st_zwc, 0, "out={:?} err={:?}", out_zwc, err_zwc);
    assert_eq!(out_zwc, "from_zwc:ok\n", "err={:?}", err_zwc);

    // Corrupt FDHead.npats (offset 56); .zwc is 0444 so recreate it.
    let mut bad_bytes = zwc_bytes;
    bad_bytes[56..60].copy_from_slice(&0x7fff_ffff_u32.to_ne_bytes());
    let _ = std::fs::remove_file(&zwc);
    std::fs::write(&zwc, bad_bytes).unwrap();
    std::fs::write(&script, "zwc_fn() { echo from_src:$1; }\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&script)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000_000))
        .unwrap();
    let (st2, out2, err2) = sh(&format!(
        "source {script}\nzwc_fn fallback\n",
        script = script.display(),
    ));
    assert_eq!(st2, 0, "out={:?} err={:?}", out2, err2);
    assert_eq!(out2, "from_src:fallback\n", "err={:?}", err2);
    assert!(err2.contains("invalid description:"), "err={:?}", err2);
}

#[test]
fn nofork_stdout_parse_wrapper_lineno_and_comment_compat() {
    let (st, out, err) = sh(
        "setopt ignorebraces interactivecomments\neval 'echo ${ echo hi # comment\n}'\neval 'echo ${ { echo nested } } DONE'\n",
    );
    assert_eq!(st, 1, "out={:?} err={:?}", out, err);
    assert_eq!(out, "hi\n");
    assert_eq!(err, "(eval):3: parse error near `}'\n");
}

#[test]
fn terminfo_nearcolor_watch_pcre_and_gdbm_modules_compat() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("t.gdbm");
    let (st, out, err) = sh(&format!(
        r#"
TERM=xterm-256color
print "ti=$terminfo[colors] tc=$termcap[Co] fg=${{(V)${{(%):-%F{{#123456}}}}}}"
WATCH=a:b
print "watch=${{watch[1]}},${{watch[2]}}"
if zmodload zsh/pcre 2>/dev/null; then
  setopt rematch_pcre
  [[ "foo/bar-42" =~ '(?<k>[^/]+)/(?<v>\w+)-(\d+)' ]]
  print "pcre=${{.pcre.match[k]}}:${{.pcre.match[v]}}:${{match[3]}}"
fi
if zmodload zsh/db/gdbm 2>/dev/null; then
  ztie -d db/gdbm -f {db} dbase
  dbase[k1]=v1
  zuntie dbase
  ztie -r -d db/gdbm -f {db} dbase
  print "gdbm=$dbase[k1]"
  zuntie -u dbase
fi
"#,
        db = db.display()
    ));
    assert_eq!(st, 0, "out={:?} err={:?}", out, err);
    assert!(
        out.contains("ti=256 tc=256 fg=^[[38;5;24m\n"),
        "out={:?}",
        out
    );
    assert!(out.contains("watch=a,b\n"), "out={:?}", out);
    if meowsh::zpcre::is_available() {
        assert!(out.contains("pcre=foo:bar:42\n"), "out={:?}", out);
    }
    if meowsh::zgdbm::is_available() {
        assert!(out.contains("gdbm=v1\n"), "out={:?}", out);
    }
}

#[test]
fn ostype_exepath_and_options_assoc_sync_compat() {
    let (st, out, err) = sh(r#"
print "os=$OSTYPE"
[[ -n $ZSH_EXEPATH && -x $ZSH_EXEPATH ]] && print "exe=ok"
setopt extendedglob
options[extendedglob]=off
[[ -o extendedglob ]] && print "eg=on" || print "eg=off"
"#);
    assert_eq!(st, 0, "out={:?} err={:?}", out, err);
    assert!(
        out.contains("os=linux-gnu\n") || !out.contains("os=\n"),
        "out={:?}",
        out
    );
    assert!(out.contains("exe=ok\n"), "out={:?}", out);
    assert!(out.contains("eg=off\n"), "out={:?}", out);
}
