//! zsh-style history expansion (BANG_HIST): `!!`, `!n`, `!-n`, `!str`,
//! `!?str?`, word designators (`:0`, `:$`, `:2-4`, `!$`, `!*`, ...) and
//! modifiers (`:h`, `:s/a/b/`, ...), plus `^old^new` at the start.

/// Expands history references in an input line. `history` holds the
/// earlier lines, oldest first; event numbers count from 1. Returns the
/// new line, or an error message.
pub fn expand(line: &str, history: &[String]) -> Result<String, String> {
    let c: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    // `^old^new^` is `!!:s/old/new/`.
    if c.first() == Some(&'^') {
        let rest: String = c[1..].iter().collect();
        let mut parts = rest.splitn(3, '^');
        let old = parts.next().unwrap_or("").to_string();
        let new = parts
            .next()
            .unwrap_or("")
            .trim_end_matches('\n')
            .to_string();
        let tail = parts.next().unwrap_or("").to_string();
        let last = history.last().ok_or("no previous command")?;
        let replaced = last.replacen(&old, &new, 1);
        return Ok(format!(
            "{}{}",
            replaced.trim_end_matches('\n'),
            if tail.is_empty() {
                "\n".to_string()
            } else {
                tail
            }
        ));
    }
    let mut in_squote = false;
    while i < c.len() {
        let ch = c[i];
        if ch == '\\' && c.get(i + 1) == Some(&'!') {
            out.push('!');
            i += 2;
            continue;
        }
        if ch == '\'' {
            in_squote = !in_squote;
        }
        if ch != '!' || in_squote {
            out.push(ch);
            i += 1;
            continue;
        }
        // `!` before a blank, `=` or `(` is literal, and so is one with
        // neither event nor word designator (`&!;`, `!}`, `!'`).
        let Some(&n) = c.get(i + 1) else {
            out.push('!');
            i += 1;
            continue;
        };
        if matches!(
            n,
            ' ' | '\t' | '\n' | '=' | '(' | ';' | '}' | '\'' | '"' | '`'
        ) {
            out.push('!');
            i += 1;
            continue;
        }
        i += 1;
        // The event.
        let braced = c.get(i) == Some(&'{');
        if braced {
            i += 1;
        }
        let Some(&first) = c.get(i).filter(|&&ch| ch != '\n') else {
            return Err("'}' expected".to_string());
        };
        let event: String = match first {
            '!' => {
                i += 1;
                last(history)?
            }
            '#' => {
                i += 1;
                out.clone()
            }
            '-' | '0'..='9' => {
                let start = i;
                i += 1;
                while c.get(i).is_some_and(char::is_ascii_digit) {
                    i += 1;
                }
                let n: i64 = c[start..i]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .map_err(|_| "bad history event".to_string())?;
                let idx = if n < 0 {
                    history.len() as i64 + n
                } else {
                    n - 1
                };
                history
                    .get(usize::try_from(idx).map_err(|_| no_event(n))?)
                    .cloned()
                    .ok_or_else(|| no_event(n))?
            }
            '?' => {
                i += 1;
                let start = i;
                while i < c.len() && c[i] != '?' && c[i] != '\n' {
                    i += 1;
                }
                let pat: String = c[start..i].iter().collect();
                if c.get(i) == Some(&'?') {
                    i += 1;
                }
                history
                    .iter()
                    .rev()
                    .find(|h| h.contains(&pat))
                    .cloned()
                    .ok_or_else(|| format!("no such event: {}", pat))?
            }
            // `!$`, `!^`, `!*`, `!:...`: words of the last command.
            '$' | '^' | '*' | ':' | '%' => last(history)?,
            _ => {
                let start = i;
                while i < c.len()
                    && !matches!(
                        c[i],
                        ' ' | '\t'
                            | '\n'
                            | ':'
                            | '^'
                            | '$'
                            | '*'
                            | '%'
                            | '}'
                            | ';'
                            | '&'
                            | '|'
                            | '"'
                            | '\''
                    )
                {
                    i += 1;
                }
                let pfx: String = c[start..i].iter().collect();
                history
                    .iter()
                    .rev()
                    .find(|h| h.starts_with(&pfx))
                    .cloned()
                    .ok_or_else(|| format!("event not found: {}", pfx))?
            }
        };
        let event = event.trim_end_matches('\n').to_string();
        // Word designators.
        let words = crate::expand::split_shell_words(&event);
        let mut text = event.clone();
        let designator = match c.get(i) {
            Some(':')
                if c.get(i + 1)
                    .is_some_and(|d| d.is_ascii_digit() || matches!(d, '$' | '^' | '*' | '-')) =>
            {
                i += 1;
                true
            }
            Some('$' | '^' | '*') => true,
            _ => false,
        };
        if designator {
            let pick = |k: &str, i: &mut usize| -> Option<usize> {
                let _ = k;
                match c.get(*i) {
                    Some('$') => {
                        *i += 1;
                        Some(words.len().saturating_sub(1))
                    }
                    Some('^') => {
                        *i += 1;
                        Some(1)
                    }
                    Some(d) if d.is_ascii_digit() => {
                        let start = *i;
                        while c.get(*i).is_some_and(char::is_ascii_digit) {
                            *i += 1;
                        }
                        c[start..*i].iter().collect::<String>().parse().ok()
                    }
                    _ => None,
                }
            };
            let (from, to);
            if c.get(i) == Some(&'*') {
                i += 1;
                from = 1;
                to = words.len().saturating_sub(1);
            } else {
                from = if c.get(i) == Some(&'-') {
                    0
                } else {
                    pick("from", &mut i).unwrap_or(0)
                };
                if c.get(i) == Some(&'-') {
                    i += 1;
                    to = match pick("to", &mut i) {
                        Some(t) => t,
                        None if c.get(i) == Some(&'*') || c.get(i) == Some(&'$') => {
                            i += 1;
                            words.len().saturating_sub(1)
                        }
                        // `x-`: up to but not including the last word.
                        None => words.len().saturating_sub(2),
                    };
                } else if c.get(i) == Some(&'*') {
                    i += 1;
                    to = words.len().saturating_sub(1);
                } else {
                    to = from;
                }
            }
            if from >= words.len() || to >= words.len() {
                return Err("no such word in event".to_string());
            }
            text = words[from..=to.max(from)].join(" ");
        }
        // Modifiers apply to each word chosen.
        if c.get(i) == Some(&':') {
            // As zsh reads history modifiers, `\\` is one backslash.
            let raw: String = c[i..].iter().collect();
            let mods: Vec<char> = raw.replace("\\\\", "\\").chars().collect();
            let (res, used_m) = crate::expand::history_modifiers(&text, &mods);
            // Map the length used back onto the original text.
            let consumed: String = mods[..used_m].iter().collect();
            let used = if used_m == 0 {
                0
            } else {
                let mut k = 0;
                let mut seen = String::new();
                while k < c.len() - i && seen.replace("\\\\", "\\") != consumed {
                    seen.push(c[i + k]);
                    k += 1;
                }
                k
            };
            if used > 0 {
                text = res;
                i += used;
            }
        }
        if braced {
            // zsh wants the brace closed: `!{!` alone is an error.
            if c.get(i) != Some(&'}') {
                return Err("'}' expected".to_string());
            }
            i += 1;
        }
        out.push_str(&text);
    }
    Ok(out)
}

fn last(history: &[String]) -> Result<String, String> {
    history
        .last()
        .cloned()
        .ok_or_else(|| "no previous command".to_string())
}

fn no_event(n: i64) -> String {
    format!("no such event: {}", n)
}

/// Writes `entries` in zsh's history file format (savehistfile): a
/// newline inside an entry is backslash-newline, an entry ending in a
/// backslash gets a space so it does not continue, and a leading `:` is
/// escaped.
pub fn write_zsh_file(path: &str, entries: &[String], append: bool) -> std::io::Result<()> {
    use std::io::Write;
    let mut text = String::new();
    for e in entries {
        if e.starts_with(':') {
            text.push('\\');
        }
        push_entry(&mut text, e);
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(append)
        .truncate(!append)
        .open(path)?;
    f.write_all(text.as_bytes())
}

// One entry and its newline, escaped as savehistfile does.
fn push_entry(text: &mut String, e: &str) {
    let mut end_backslashes = false;
    for c in e.chars() {
        if c == '\n' {
            text.push('\\');
        }
        end_backslashes = c == '\\' || (end_backslashes && c == ' ');
        text.push(c);
    }
    if end_backslashes {
        text.push(' ');
    }
    text.push('\n');
}

/// A line of zsh's EXTENDED_HISTORY format: `: start:elapsed;command`,
/// both times in seconds.
pub fn extended_line(start: u64, elapsed: u64, entry: &str) -> String {
    let mut text = format!(": {}:{};", start, elapsed);
    push_entry(&mut text, entry);
    text
}

/// An entry appended to meowsh's history file whose command is still
/// running: where its line is, so the elapsed time can be filled in.
pub struct Appended {
    path: String,
    offset: u64,
    len: u64,
    start: u64,
    entry: String,
}

// The history file, locked against other shells until it is dropped.
fn open_locked(path: &str) -> std::io::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    // SAFETY: the descriptor is open; the lock goes with it when it closes.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(f)
}

/// Appends `entry` to meowsh's history file with the time it started and
/// an elapsed time of 0, so the line is on disk before the command runs.
pub fn append_timed(path: &str, entry: &str, start: u64) -> std::io::Result<Appended> {
    use std::io::{Seek, SeekFrom, Write};
    let line = extended_line(start, 0, entry);
    let mut f = open_locked(path)?;
    let offset = f.seek(SeekFrom::End(0))?;
    f.write_all(line.as_bytes())?;
    Ok(Appended {
        path: path.to_string(),
        offset,
        len: line.len() as u64,
        start,
        entry: entry.to_string(),
    })
}

/// Fills in how long the command took. Only when its line is still the
/// last one: another shell may have appended since, and that is left
/// alone.
pub fn finish_timed(a: &Appended, elapsed: u64) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    if elapsed == 0 {
        return Ok(());
    }
    let mut f = open_locked(&a.path)?;
    if f.metadata()?.len() != a.offset + a.len {
        return Ok(());
    }
    f.set_len(a.offset)?;
    f.seek(SeekFrom::Start(a.offset))?;
    f.write_all(extended_line(a.start, elapsed, &a.entry).as_bytes())
}

// rustyline's history format, which meowsh used before: a "#V2" line, then
// one entry per line with backslashes and newlines escaped.
fn read_rustyline_text(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| *l != "#V2")
        .map(|l| {
            let mut out = String::new();
            let mut it = l.chars();
            while let Some(c) = it.next() {
                if c == '\\' {
                    match it.next() {
                        Some('n') => out.push('\n'),
                        Some(o) => out.push(o),
                        None => out.push('\\'),
                    }
                } else {
                    out.push(c);
                }
            }
            out
        })
        .collect()
}

/// Reads meowsh's history file. One still in the old rustyline format is
/// first rewritten in zsh's (its entries have no times), and the old file
/// kept beside it as `<path>.rustyline`.
pub fn read_own_file(path: &str) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let old = std::fs::read(path)
        .ok()
        .filter(|d| d.starts_with(b"#V2\n"))
        .map(|d| read_rustyline_text(&String::from_utf8_lossy(&d)));
    if let Some(entries) = old {
        let tmp = format!("{}.new", path);
        let converted = write_zsh_file(&tmp, &entries, false)
            .and_then(|_| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)))
            .and_then(|_| std::fs::rename(path, format!("{}.rustyline", path)))
            .and_then(|_| std::fs::rename(&tmp, path));
        if converted.is_err() {
            // Left as it was; nothing is appended in the other format.
            let _ = std::fs::remove_file(&tmp);
            return entries;
        }
    }
    read_zsh_file(path).unwrap_or_default()
}

/// Reads a zsh history file (readhistline): backslash-newline continues
/// an entry, and the space after a final backslash goes.
pub fn read_zsh_file(path: &str) -> std::io::Result<Vec<String>> {
    let data = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&data);
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.split_inclusive('\n') {
        let line = line.strip_suffix('\n').unwrap_or(line);
        cur.push_str(line);
        if line.ends_with('\\') {
            cur.pop();
            cur.push('\n');
            continue;
        }
        let trimmed = cur.trim_end_matches(' ');
        if trimmed.len() != cur.len() && trimmed.ends_with('\\') {
            cur.pop();
        }
        // Extended history: `: start:elapsed;command`.
        let entry = match cur.strip_prefix(": ") {
            Some(rest) if rest.contains(';') => rest.split_once(';').unwrap().1.to_string(),
            _ => cur
                .strip_prefix('\\')
                .filter(|r| r.starts_with(':'))
                .unwrap_or(&cur)
                .to_string(),
        };
        out.push(entry);
        cur.clear();
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_history_file_records_start_and_elapsed_times() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("h").to_string_lossy().to_string();
        let a = append_timed(&path, "sleep 3", 100).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ": 100:0;sleep 3\n");
        finish_timed(&a, 3).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ": 100:3;sleep 3\n");
        // Another shell's line after it: the earlier one is left alone.
        let b = append_timed(&path, "print 'a\nb'", 104).unwrap();
        append_timed(&path, ": other; x\\", 105).unwrap();
        finish_timed(&b, 9).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            ": 100:3;sleep 3\n: 104:0;print 'a\\\nb'\n: 105:0;: other; x\\ \n"
        );
        assert_eq!(
            read_own_file(&path),
            ["sleep 3", "print 'a\nb'", ": other; x\\"]
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn old_rustyline_history_file_is_converted_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("h").to_string_lossy().to_string();
        let old = "#V2\nls -l\nprint 'a\\nb' \\\\x\n: colon\n";
        std::fs::write(&path, old).unwrap();
        let want = ["ls -l", "print 'a\nb' \\x", ": colon"];
        assert_eq!(read_own_file(&path), want);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "ls -l\nprint 'a\\\nb' \\x\n\\: colon\n"
        );
        assert_eq!(
            std::fs::read_to_string(format!("{}.rustyline", path)).unwrap(),
            old
        );
        assert_eq!(read_own_file(&path), want);
    }

    #[test]
    fn events_and_words() {
        let _g = crate::zsh_test_guard();
        let h = vec!["print one two three".to_string()];
        assert_eq!(
            expand("echo !!\n", &h).unwrap(),
            "echo print one two three\n"
        );
        assert_eq!(expand("echo !:$ !:1\n", &h).unwrap(), "echo three one\n");
        assert_eq!(expand("echo !1:2-3\n", &h).unwrap(), "echo two three\n");
        assert_eq!(expand("echo '!!' a\\!b\n", &h).unwrap(), "echo '!!' a!b\n");
    }

    #[test]
    fn an_unclosed_brace_is_an_error_not_a_panic() {
        let _g = crate::zsh_test_guard();
        let h = vec!["echo hi".to_string()];
        assert_eq!(expand("echo !{", &h), Err("'}' expected".to_string()));
        assert_eq!(expand("echo !{\n", &h), Err("'}' expected".to_string()));
    }
}
