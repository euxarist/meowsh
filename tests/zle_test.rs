//! meowsh's own line editor (ZLE), driven through a pseudo-terminal.

use std::process::{Command, Stdio};
use tempfile::TempDir;

// Starts an interactive meowsh using ZLE with `zshrc`, types `input`
// and returns everything it wrote.
fn on_terminal(zshrc: &str, input: &str) -> String {
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    let home = TempDir::new().unwrap();
    std::fs::write(
        home.path().join(".zshrc"),
        format!(".term.extensions=( -query )\n{}", zshrc),
    )
    .unwrap();
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
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(3) {
        drain(&mut m, &mut out);
        if !out.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    drain(&mut m, &mut out);
    m.write_all(input.as_bytes()).unwrap();
    let sent = std::time::Instant::now();
    while sent.elapsed() < std::time::Duration::from_secs(5) {
        drain(&mut m, &mut out);
        if String::from_utf8_lossy(&out).contains("[B=") {
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

const REPORT: &str = "KEYTIMEOUT=1\n\
report() { print -r \"[B=$BUFFER C=$CURSOR]\"; BUFFER=; zle accept-line }\n\
zle -N report\nbindkey '^X' report\nbindkey -a '^X' report\n";

#[test]
fn vi_change_command_edits_the_line() {
    let out = on_terminal(&format!("bindkey -v\n{}", REPORT), "goox\x1bcld\x18");
    assert!(out.contains("[B=good C=4]"), "output: {:?}", out);
}

#[test]
fn widgets_see_and_set_the_buffer() {
    let rc = format!(
        "{}up() {{ LBUFFER=${{LBUFFER:u}} }}\nzle -N up\nbindkey '^U' up\n",
        REPORT
    );
    let out = on_terminal(&rc, "ab cd\x02\x02\x15\x18");
    assert!(out.contains("[B=AB cd C=3]"), "output: {:?}", out);
}

#[test]
fn send_string_bindings_are_read_as_input() {
    let rc = format!("bindkey -s '^Y' 'foo'\n{}", REPORT);
    let out = on_terminal(&rc, "\x19\x18");
    assert!(out.contains("[B=foo C=3]"), "output: {:?}", out);
}

#[test]
fn inherited_fpath_is_preserved() {
    let out = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env("FPATH", "/custom/zsh/funcs")
        .args(["-f", "-c", "print -r -- \"$FPATH\""])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "/custom/zsh/funcs\n");
}

#[test]
fn bracketed_paste_magic_quotes_pasted_url() {
    // Only runs when the host has zsh's contributed Zle functions installed.
    let probe = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env_remove("FPATH")
        .args([
            "-f",
            "-c",
            "for d in $fpath; do [[ -e $d/url-quote-magic ]] && exit 0; done; exit 1",
        ])
        .status()
        .unwrap();
    if !probe.success() {
        return;
    }
    let rc = format!(
        "autoload -Uz bracketed-paste-magic url-quote-magic\n\
         zle -N bracketed-paste bracketed-paste-magic\n\
         zle -N self-insert url-quote-magic\n\
         {}",
        REPORT
    );
    let out = on_terminal(
        &rc,
        "yt-dlp \x1b[200~https://x.com/u/status/1?a=1&s=20\x1b[201~\x18",
    );
    assert!(
        out.contains(r"[B=yt-dlp https://x.com/u/status/1\?a\=1\&s\=20 C=44]"),
        "output: {:?}",
        out
    );
}

#[test]
fn zle_f_fd_handler_runs_while_waiting_for_input() {
    let rc = r#"KEYTIMEOUT=1
on_fd() {
    local sug
    IFS='' read -rd '' -u "$1" sug
    POSTDISPLAY="$sug"
    builtin exec {1}<&-
    zle -F "$1"
    zle -R
    print -r "[B=$BUFFER P=$POSTDISPLAY]"
    BUFFER=
    zle accept-line
}
start_async() {
    local fd
    builtin exec {fd}< <(print -rn -- "suggestion")
    zle -F "$fd" on_fd
}
zle -N start_async
bindkey '^A' start_async
"#;
    let out = on_terminal(rc, "ec\x01");
    assert!(out.contains("[B=ec P=suggestion]"), "output: {:?}", out);
}

#[test]
fn zle_r_draws_postdisplay_set_in_the_same_widget() {
    // zsh-autosuggestions sets POSTDISPLAY from a zle -F handler and calls
    // zle -R; no widget returns before the redraw.
    let rc = r#"
show() {
    POSTDISPLAY="GHOSTTEXT"
    zle -R
    POSTDISPLAY=
    print -r "[B=$BUFFER]"
    BUFFER=
    zle accept-line
}
zle -N show
bindkey '^A' show
"#;
    let out = on_terminal(rc, "ec\x01");
    let marker = out
        .find("[B=ec]")
        .unwrap_or_else(|| panic!("output: {:?}", out));
    assert!(out[..marker].contains("GHOSTTEXT"), "output: {:?}", out);
}
