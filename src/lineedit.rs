use crate::exec::InputLine;
use crate::shell::build_prompt;
use crate::strip_ansi;
use rustyline::completion::Completer;
use rustyline::config::Configurer;
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::history::FileHistory;
use rustyline::validate::Validator;
use rustyline::{ColorMode, Editor, Helper, KeyCode, KeyEvent, Modifiers};
use std::borrow::Cow;
use std::cell::RefCell;

pub struct MeowshHelper;

impl MeowshHelper {
    // Syntax colors for the input line.
    fn highlight_line(&self, line: &str) -> String {
        let parts = get_char_or_ansi(line);
        let mut colored = String::with_capacity(line.len() * 2);
        let mut i = 0;
        let mut is_start_of_command = true;

        while i < parts.len() {
            match &parts[i] {
                CharOrAnsi::Ansi(ansi) => {
                    colored.push_str(ansi);
                    i += 1;
                }
                CharOrAnsi::Char(c) => match *c {
                    // A newline (pasted text) starts another command.
                    '\n' => {
                        colored.push('\n');
                        i += 1;
                        is_start_of_command = true;
                    }
                    // Every other blank is passed over; the word scan
                    // below stops at blanks without consuming them.
                    c if c.is_whitespace() => {
                        colored.push(c);
                        i += 1;
                    }
                    '#' if is_start_of_command => {
                        colored.push_str("\x1b[90m");
                        while i < parts.len() {
                            match &parts[i] {
                                CharOrAnsi::Char(c) => colored.push(*c),
                                CharOrAnsi::Ansi(ansi) => colored.push_str(ansi),
                            }
                            i += 1;
                        }
                        colored.push_str("\x1b[0m");
                        break;
                    }
                    ';' | '|' | '&' => {
                        colored.push_str("\x1b[36m");
                        colored.push(*c);
                        i += 1;
                        if i < parts.len() {
                            if let CharOrAnsi::Char(next_c) = &parts[i] {
                                if *next_c == *c && (*c == '|' || *c == '&') {
                                    colored.push(*next_c);
                                    i += 1;
                                }
                            }
                        }
                        colored.push_str("\x1b[0m");
                        is_start_of_command = true;
                    }
                    '\'' | '"' => {
                        let quote = *c;
                        colored.push_str("\x1b[33m");
                        colored.push(quote);
                        i += 1;
                        while i < parts.len() {
                            match &parts[i] {
                                CharOrAnsi::Char(c) if *c == quote => break,
                                CharOrAnsi::Char(c) if *c == '\\' => {
                                    colored.push('\\');
                                    i += 1;
                                    if i < parts.len() {
                                        if let CharOrAnsi::Char(c2) = &parts[i] {
                                            colored.push(*c2);
                                            i += 1;
                                        }
                                    }
                                }
                                CharOrAnsi::Char(c) => {
                                    colored.push(*c);
                                    i += 1;
                                }
                                CharOrAnsi::Ansi(ansi) => {
                                    colored.push_str(ansi);
                                    i += 1;
                                }
                            }
                        }
                        if i < parts.len() {
                            colored.push(quote);
                            i += 1;
                        }
                        colored.push_str("\x1b[0m");
                        is_start_of_command = false;
                    }
                    '$' => {
                        colored.push_str("\x1b[35m");
                        colored.push('$');
                        i += 1;
                        while i < parts.len() {
                            if let CharOrAnsi::Char(c) = &parts[i] {
                                if c.is_alphanumeric() || *c == '_' {
                                    colored.push(*c);
                                    i += 1;
                                    continue;
                                }
                            }
                            break;
                        }
                        colored.push_str("\x1b[0m");
                        is_start_of_command = false;
                    }
                    _ => {
                        let mut word_parts = Vec::new();
                        let mut visible_word = String::new();
                        while i < parts.len() {
                            match &parts[i] {
                                CharOrAnsi::Char(c)
                                    if c.is_whitespace() || ";|&'\"$".contains(*c) =>
                                {
                                    break;
                                }
                                CharOrAnsi::Char(c) => {
                                    visible_word.push(*c);
                                    word_parts.push(&parts[i]);
                                }
                                CharOrAnsi::Ansi(_) => {
                                    word_parts.push(&parts[i]);
                                }
                            }
                            i += 1;
                        }

                        if is_start_of_command {
                            if self.command_exists(&visible_word) {
                                colored.push_str("\x1b[32m");
                            } else {
                                colored.push_str("\x1b[31m");
                            }
                            for p in word_parts {
                                match p {
                                    CharOrAnsi::Char(c) => colored.push(*c),
                                    CharOrAnsi::Ansi(ansi) => colored.push_str(ansi),
                                }
                            }
                            colored.push_str("\x1b[0m");
                            is_start_of_command = false;
                        } else {
                            for p in word_parts {
                                match p {
                                    CharOrAnsi::Char(c) => colored.push(*c),
                                    CharOrAnsi::Ansi(ansi) => colored.push_str(ansi),
                                }
                            }
                        }
                    }
                },
            }
        }

        colored
    }

    pub fn command_exists(&self, name: &str) -> bool {
        crate::shell::command_exists(name)
    }

    fn fuzzy_match(&self, query: &str, candidate: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let mut query_chars = query.chars().peekable();
        for c in candidate.chars() {
            if let Some(&q) = query_chars.peek() {
                if c.to_lowercase().next() == q.to_lowercase().next() {
                    query_chars.next();
                }
            } else {
                return true;
            }
        }
        query_chars.peek().is_none()
    }

    fn get_completions(&self, line: &str) -> Vec<rustyline::completion::Pair> {
        let mut completions = Vec::new();

        let chars: Vec<char> = line.chars().collect();
        let (words, open) = split_words(&chars, char::is_whitespace);
        let parts: Vec<&str> = words.iter().map(|(_, w)| w.as_str()).collect();
        // Command position: empty line, single word without '/', or after a pipe/operator
        let trailing_space = !open && !parts.is_empty();
        let is_cmd_position =
            parts.is_empty() || (parts.len() == 1 && !trailing_space && !parts[0].contains('/'));

        if is_cmd_position {
            let current = parts.last().copied().unwrap_or("");
            // Held only here: path completion reads variables, which lock
            // the shell themselves.
            let shell = crate::shell::SHELL.shell.lock().unwrap();

            let aliases: Vec<String> = shell
                .assoc(crate::types::ALIASES)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            for name in &aliases {
                if self.fuzzy_match(current, name) {
                    completions.push(rustyline::completion::Pair {
                        display: name.clone(),
                        replacement: name.clone(),
                    });
                }
            }

            for name in shell.functions.keys() {
                if self.fuzzy_match(current, name) {
                    completions.push(rustyline::completion::Pair {
                        display: name.clone(),
                        replacement: name.clone(),
                    });
                }
            }

            let builtins = crate::builtins::builtin_names();
            for b in builtins {
                if self.fuzzy_match(current, b) {
                    completions.push(rustyline::completion::Pair {
                        display: b.to_string(),
                        replacement: b.to_string(),
                    });
                }
            }

            let path = shell.scalar("PATH").unwrap_or("");
            for dir in path.split(':') {
                if dir.is_empty() {
                    continue;
                }
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        if let Ok(name) = entry.file_name().into_string() {
                            if self.fuzzy_match(current, &name) {
                                completions.push(rustyline::completion::Pair {
                                    display: name.clone(),
                                    replacement: name,
                                });
                            }
                        }
                    }
                }
            }
        } else {
            // Argument position: complete file/directory paths
            let current = if trailing_space {
                ""
            } else {
                parts.last().copied().unwrap_or("")
            };
            let cmd = parts.first().unwrap_or(&"");
            let dirs_only = matches!(*cmd, "cd" | "pushd" | "rmdir");
            self.complete_paths(current, &mut completions, dirs_only);
        }

        completions
    }

    fn complete_paths(
        &self,
        partial: &str,
        completions: &mut Vec<rustyline::completion::Pair>,
        dirs_only: bool,
    ) {
        use std::path::Path;

        // Expand ~ to home directory
        let expanded = if partial.starts_with('~') {
            if let Some(home) = crate::shell::var_get_opt("HOME") {
                if partial == "~" {
                    home.clone()
                } else {
                    partial.replacen('~', &home, 1)
                }
            } else {
                partial.to_string()
            }
        } else {
            partial.to_string()
        };

        let (dir, prefix) = if expanded.ends_with('/') {
            (expanded.as_str(), "")
        } else {
            let path = Path::new(&expanded);
            match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => {
                    let dir_str = if parent.as_os_str().is_empty() {
                        "."
                    } else {
                        parent.to_str().unwrap_or(".")
                    };
                    (dir_str, name.to_str().unwrap_or(""))
                }
                _ => (".", &*expanded),
            }
        };

        // We need dir to live long enough - rebind to owned string for the case where
        // dir was borrowed from a temporary
        let dir_owned;
        let dir = if dir == "." && expanded.ends_with('/') {
            &*expanded
        } else if dir == "." && !partial.is_empty() && !partial.starts_with('.') {
            dir
        } else {
            dir_owned = dir.to_string();
            &dir_owned
        };

        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };

            // Skip hidden files unless the user is explicitly typing a dot prefix
            if name.starts_with('.') && !prefix.starts_with('.') {
                continue;
            }

            if !name.starts_with(prefix) {
                continue;
            }

            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);

            if dirs_only && !is_dir {
                continue;
            }

            // Build the replacement string preserving the user's typed prefix
            let replacement = if partial.is_empty() {
                if is_dir {
                    format!("{}/", name)
                } else {
                    name.clone()
                }
            } else if partial.ends_with('/') {
                if is_dir {
                    format!("{}{}/", partial, name)
                } else {
                    format!("{}{}", partial, name)
                }
            } else {
                // Replace just the filename portion, keeping the directory part
                let dir_prefix = if let Some(slash_pos) = partial.rfind('/') {
                    &partial[..=slash_pos]
                } else {
                    ""
                };
                if is_dir {
                    format!("{}{}/", dir_prefix, name)
                } else {
                    format!("{}{}", dir_prefix, name)
                }
            };

            let display = if is_dir { format!("{}/", name) } else { name };

            completions.push(rustyline::completion::Pair {
                display,
                replacement: escape_word(&replacement),
            });
        }

        completions.sort_by(|a, b| a.display.cmp(&b.display));
    }
}

#[derive(Clone, Debug)]
enum CharOrAnsi {
    Char(char),
    Ansi(String),
}

fn get_char_or_ansi(s: &str) -> Vec<CharOrAnsi> {
    let mut out = Vec::new();
    let mut chars = s.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c == '\x1b' {
            let mut ansi = String::new();
            ansi.push(c);
            if let Some(&(_, next_c)) = chars.peek() {
                match next_c {
                    '[' => {
                        // CSI
                        let (_, c2) = chars.next().unwrap();
                        ansi.push(c2);
                        for (_, c) in chars.by_ref() {
                            ansi.push(c);
                            if ('@'..='~').contains(&c) {
                                break;
                            }
                        }
                    }
                    ']' => {
                        // OSC
                        let (_, c2) = chars.next().unwrap();
                        ansi.push(c2);
                        while let Some((_, c)) = chars.next() {
                            ansi.push(c);
                            if c == '\x07' {
                                // BEL
                                break;
                            }
                            if c == '\x1b' {
                                if let Some(&(_, next_next_c)) = chars.peek() {
                                    if next_next_c == '\\' {
                                        // ST
                                        let (_, c3) = chars.next().unwrap();
                                        ansi.push(c3);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    '(' | ')' | '#' | '*' | '+' | '-' | '.' | '/' => {
                        // G0/G1 charset or other 2-char ESC
                        let (_, c2) = chars.next().unwrap();
                        ansi.push(c2);
                        if let Some((_, c3)) = chars.next() {
                            ansi.push(c3);
                        }
                    }
                    _ => {
                        if let Some((_, c2)) = chars.next() {
                            ansi.push(c2);
                        }
                    }
                }
            }
            out.push(CharOrAnsi::Ansi(ansi));
        } else {
            out.push(CharOrAnsi::Char(c));
        }
    }
    out
}

impl Completer for MeowshHelper {
    type Candidate = rustyline::completion::Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> Result<(usize, Vec<rustyline::completion::Pair>), ReadlineError> {
        Ok(crate::menu::take_pending().unwrap_or_else(|| candidates(line, pos)))
    }
}

/// The matches for the word before `pos` in `line`, and the byte offset
/// where that word starts.
pub(crate) fn candidates(line: &str, pos: usize) -> (usize, Vec<rustyline::completion::Pair>) {
    let (clean_line, mappings) = crate::get_clean_and_mappings(line);
    // `mappings` has one entry per character of `clean_line`, so all
    // positions below are character indices, not byte offsets.
    let chars: Vec<char> = clean_line.chars().collect();
    let clean_pos = mappings
        .iter()
        .position(|&raw| raw >= pos)
        .unwrap_or(chars.len())
        .min(chars.len());
    let (words, open) = split_words(&chars[..clean_pos], |c| {
        c.is_whitespace() || ";|&".contains(c)
    });
    let word_start = match words.last() {
        Some(&(i, _)) if open => i,
        _ => clean_pos,
    };
    let start = mappings.get(word_start).copied().unwrap_or(pos);
    let prefix: String = chars[..clean_pos].iter().collect();
    (start, MeowshHelper.get_completions(&prefix))
}

// Splits `chars` into words at unquoted separators, as the parser would
// read them: each word's start index and its text with quotes and
// backslashes removed. The flag is whether the last word runs to the end,
// that is, is still being typed.
fn split_words(chars: &[char], sep: impl Fn(char) -> bool) -> (Vec<(usize, String)>, bool) {
    let mut words: Vec<(usize, String)> = Vec::new();
    let mut cur: Option<(usize, String)> = None;
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if quote.is_none() && sep(c) {
            words.extend(cur.take());
            i += 1;
            continue;
        }
        let word = &mut cur.get_or_insert_with(|| (i, String::new())).1;
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (None, '\'') | (None, '"') => quote = Some(c),
            (Some('"'), '\\') if matches!(chars.get(i + 1), Some('"' | '\\' | '$' | '`')) => {
                i += 1;
                word.push(chars[i]);
            }
            (None, '\\') => {
                i += 1;
                if let Some(&n) = chars.get(i) {
                    word.push(n);
                }
            }
            _ => word.push(c),
        }
        i += 1;
    }
    let open = cur.is_some();
    words.extend(cur);
    (words, open)
}

// Backslash-escapes what the parser would otherwise read as syntax, so a
// completed name like `foo (1).deb` stays one literal word. A leading `~`
// is the user's tilde and is left for expansion.
fn escape_word(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    for (i, c) in word.chars().enumerate() {
        let special = c.is_whitespace()
            || "\\'\"`$&|;<>()[]{}*?!^".contains(c)
            || (i == 0 && "#=".contains(c));
        if special {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// zsh's RPROMPT for the line being read: its rendered text and width,
// and the width of the last line of the left prompt.
struct RightPrompt {
    text: String,
    width: usize,
    left: usize,
}

static RIGHT_PROMPT: std::sync::Mutex<Option<RightPrompt>> = std::sync::Mutex::new(None);

// Renders $RPROMPT/$RPS1 (or $RPROMPT2/$RPS2 for continuation lines) for
// the next read, beside `prompt`.
fn set_right_prompt(prompt: &str, continuation: bool) {
    let names = if continuation {
        ["RPROMPT2", "RPS2"]
    } else {
        ["RPROMPT", "RPS1"]
    };
    let value = names
        .iter()
        .find_map(|n| crate::shell::var_get_opt(n).filter(|v| !v.is_empty()));
    let rp = value.map(|v| {
        let text = crate::shell::render_prompt(&v);
        let width = crate::display_width(&strip_ansi(&text));
        let last = prompt.rsplit('\n').next().unwrap_or(prompt);
        // highlight_prompt adds a space when the prompt has none.
        let left = crate::display_width(&strip_ansi(last)) + usize::from(!prompt.ends_with(' '));
        RightPrompt { text, width, left }
    });
    if let Ok(mut r) = RIGHT_PROMPT.lock() {
        *r = rp.filter(|r| r.width > 0);
    }
}

// Width of the input line last drawn, for the hint that follows it.
static LINE_WIDTH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

// The column (0-based) where the right prompt starts, and the width of
// the left prompt; None when there is no right prompt.
fn right_prompt_start() -> Option<(usize, usize)> {
    let guard = RIGHT_PROMPT.lock().ok()?;
    let rp = guard.as_ref()?;
    let cols = crate::shell::terminal_size()
        .map(|(c, _)| c as usize)
        .or_else(|| crate::shell::var_get_opt("COLUMNS").and_then(|c| c.parse().ok()))
        .unwrap_or(80);
    let indent = crate::shell::var_get_opt("ZLE_RPROMPT_INDENT")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1);
    Some((cols.checked_sub(indent + rp.width)?, rp.left))
}

// Whether a right prompt starting at column `start` fits beside a left
// prompt and line taking `used` columns: zsh wants a blank between them.
fn right_prompt_fits(start: usize, used: usize) -> bool {
    used < start
}

// The escape sequences that draw the right prompt at the end of the
// row, leaving the cursor where it was. Empty when the line would reach
// it, as zsh then hides it.
fn right_prompt_overlay(line: &str) -> String {
    let width = crate::display_width(&strip_ansi(line));
    LINE_WIDTH.store(width, std::sync::atomic::Ordering::Relaxed);
    if line.contains('\n') {
        return String::new();
    }
    let Some((start, left)) = right_prompt_start() else {
        return String::new();
    };
    if !right_prompt_fits(start, left + width) {
        return String::new();
    }
    let text = RIGHT_PROMPT
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|r| r.text.clone()))
        .unwrap_or_default();
    format!("\x1b7\x1b[{}G{}\x1b[0m\x1b8", start + 1, text)
}

// After a suggestion that reaches the right prompt: clears what is left
// of it on the row.
fn hint_clears_right_prompt(hint: &str) -> bool {
    let Some((start, left)) = right_prompt_start() else {
        return false;
    };
    let line = LINE_WIDTH.load(std::sync::atomic::Ordering::Relaxed);
    !right_prompt_fits(start, left + line + crate::display_width(hint))
}

impl Highlighter for MeowshHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        let line = match visible_line(line) {
            Cow::Borrowed(l) => l,
            Cow::Owned(l) => {
                let mut colored = self.highlight_line(&l);
                colored.push_str(&right_prompt_overlay(&l));
                return Cow::Owned(colored);
            }
        };
        let overlay = right_prompt_overlay(line);
        if line.is_empty() {
            return if overlay.is_empty() {
                Cow::Borrowed(line)
            } else {
                Cow::Owned(overlay)
            };
        }
        let mut colored = self.highlight_line(line);
        colored.push_str(&overlay);
        Cow::Owned(colored)
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        if crate::menu::is_drawn(hint) {
            return Cow::Borrowed(hint);
        }
        let clear = if hint_clears_right_prompt(hint) {
            "\x1b[K"
        } else {
            ""
        };
        Cow::Owned(format!("\x1b[90m{}\x1b[0m{}", hint, clear))
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        // Always re-render the full line so that command validity coloring
        // updates for the entire word as the user types each character.
        true
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        // Ensure trailing space for input separation
        if prompt.ends_with(' ') {
            Cow::Borrowed(prompt)
        } else {
            Cow::Owned(format!("{} ", prompt))
        }
    }
}

#[derive(Hash, Debug, PartialEq, Eq)]
pub struct MeowshHint {
    display: String,
    completion: String,
}

impl rustyline::hint::Hint for MeowshHint {
    fn display(&self) -> &str {
        &self.display
    }

    fn completion(&self) -> Option<&str> {
        // The completion menu has nothing to accept.
        Some(self.completion.as_str()).filter(|c| !c.is_empty())
    }
}

impl Hinter for MeowshHelper {
    type Hint = MeowshHint;

    fn hint(&self, line: &str, pos: usize, _ctx: &rustyline::Context<'_>) -> Option<MeowshHint> {
        if let Some(grid) = crate::menu::hint(line, pos) {
            return Some(MeowshHint {
                display: grid,
                completion: String::new(),
            });
        }
        // Only show hint if cursor is at the end of the line
        if pos < line.len() {
            return None;
        }

        let clean_line = strip_ansi(line);
        let trimmed_input = clean_line.trim_start();
        if trimmed_input.is_empty() {
            return None;
        }

        let shell = crate::shell::SHELL.shell.lock().unwrap();
        let history = shell.history.lock().unwrap();

        let lower_input = trimmed_input.to_lowercase();
        for hist in history.iter().rev() {
            let clean_hist = strip_ansi(hist);
            let lower_hist = clean_hist.to_lowercase();
            if lower_hist.starts_with(&lower_input)
                && clean_hist.chars().count() > trimmed_input.chars().count()
            {
                // Return what's left of the history entry after the input
                // We skip the number of characters in the input to get the hint suffix
                let char_count = trimmed_input.chars().count();
                let hint_text: String = clean_hist.chars().skip(char_count).collect();

                return Some(MeowshHint {
                    display: hint_text.clone(),
                    completion: hint_text, // hist is already clean
                });
            }
        }
        None
    }
}

impl Validator for MeowshHelper {}

impl Helper for MeowshHelper {}

fn continuation_prompt() -> String {
    let ps2 = crate::shell::var_get_opt("PROMPT2")
        .filter(|p| !p.is_empty())
        .or_else(|| crate::shell::var_get_opt("PS2"))
        .unwrap_or_else(|| "> ".to_string());
    crate::shell::render_prompt(&ps2)
}

fn primary_prompt() -> String {
    crate::builtins::run_hooks("precmd", &[]);
    build_prompt()
}

/// NOTIFY: while waiting for input on `fd`, reports jobs as they finish.
fn wait_input_notify(fd: i32) {
    if !crate::options::is_set("notify") {
        return;
    }
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, 50) } != 0 {
            break;
        }
        crate::jobs::reap_jobs(true);
    }
}

/// Reads commands from `fd` until end of input. Prompts go to standard
/// error when `prompts` is set (an interactive shell without a terminal).
pub fn run_fd(fd: i32, prompts: bool) -> i32 {
    let mut reader = crate::sys::LineReader::new(fd);
    crate::exec::run_input(
        |cont| {
            if prompts {
                let p = if cont {
                    continuation_prompt()
                } else {
                    primary_prompt()
                };
                let _ = crate::sys::write_fd(2, p.as_bytes());
            }
            if prompts {
                wait_input_notify(fd);
            }
            match reader.read_line() {
                Some(l) => InputLine::Line(l),
                None => InputLine::Eof,
            }
        },
        |text| {
            crate::exec::history_add(text);
        },
    )
}

// The line without what the editor gives no width: escape sequences as
// rustyline measures them, and other control characters. A paste or
// Ctrl-V can put them in the line; drawn raw, the terminal would act on
// them and show a different command from the one that runs. They are
// neither drawn nor run.
fn visible_line(line: &str) -> Cow<'_, str> {
    let hidden = |c: char| c.is_control() && c != '\n' && c != '\t';
    if !line.chars().any(hidden) {
        return Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut esc = 0;
    for c in line.chars() {
        if !hidden(c) && (esc == 0 || c == '\n' || c == '\t') {
            out.push(c);
            continue;
        }
        esc = match esc {
            // After ESC: `[` starts a CSI sequence, anything else ends it.
            1 if c == '[' => 2,
            // CSI parameters; the first other character is the last.
            2 if c == ';' || c.is_ascii_digit() => 2,
            0 if c == '\x1b' => 1,
            _ => 0,
        };
    }
    Cow::Owned(out)
}

/// The interactive read-eval loop with line editing and history.
pub fn run_interactive() -> i32 {
    // A terminal without cursor control gets plain line reading: the
    // prompt, then the line as the terminal's own line discipline edits it.
    if unsafe { libc::isatty(0) } == 1
        && std::env::var_os("MEOWSH_ZLE").is_some_and(|v| !v.is_empty())
    {
        return run_zle();
    }
    let dumb = std::env::var("TERM").map_or(true, |t| t.is_empty() || t == "dumb");
    if unsafe { libc::isatty(0) } != 1 || dumb {
        return run_fd(0, true);
    }
    let mut rl: Editor<MeowshHelper, FileHistory> = match Editor::new() {
        Ok(rl) => rl,
        Err(_) => return run_fd(0, true),
    };
    rl.set_color_mode(ColorMode::Forced);
    // The menu's replacements arrive as a single completion match, which
    // list completion inserts with the cursor after it.
    rl.set_completion_type(rustyline::CompletionType::List);
    rl.set_helper(Some(MeowshHelper));
    let _ = rl.set_max_history_size(1000);
    // Tab opens zsh's completion menu, which takes the arrows, Enter and
    // Ctrl-G while it is selecting. Otherwise Up/Down search history for
    // lines starting with what is typed, as oh-my-zsh binds them.
    use crate::menu::{Key, MenuKey};
    for (key, menu_key) in [
        (KeyEvent(KeyCode::Tab, Modifiers::NONE), Key::Tab),
        (KeyEvent(KeyCode::BackTab, Modifiers::NONE), Key::BackTab),
        (KeyEvent(KeyCode::Up, Modifiers::NONE), Key::Up),
        (KeyEvent(KeyCode::Down, Modifiers::NONE), Key::Down),
        (KeyEvent(KeyCode::Left, Modifiers::NONE), Key::Left),
        (KeyEvent(KeyCode::Right, Modifiers::NONE), Key::Right),
        (KeyEvent(KeyCode::Enter, Modifiers::NONE), Key::Enter),
        (KeyEvent::ctrl('G'), Key::Cancel),
    ] {
        rl.bind_sequence(
            key,
            rustyline::EventHandler::Conditional(Box::new(MenuKey(menu_key))),
        );
    }

    let history_file = crate::shell::with_shell(|s| s.history_file.clone());
    let entries = if history_file.is_empty() {
        Vec::new()
    } else {
        crate::history::read_own_file(&history_file)
    };
    for e in &entries {
        let _ = rl.add_history_entry(e.as_str());
    }
    crate::shell::with_shell(|s| *s.history.lock().unwrap_or_else(|e| e.into_inner()) = entries);

    let rl = RefCell::new(rl);
    let status = crate::exec::run_input(
        |cont| {
            let prompt = if cont {
                continuation_prompt()
            } else {
                primary_prompt()
            };
            set_right_prompt(&prompt, cont);
            crate::menu::reset();
            finish_history_entry();
            let line = rl.borrow_mut().readline(&prompt);
            match line {
                Ok(line) => InputLine::Line(visible_line(&line).into_owned() + "\n"),
                Err(ReadlineError::Interrupted) => InputLine::Interrupted,
                Err(ReadlineError::Eof) => InputLine::Eof,
                Err(err) => {
                    crate::sys::err_line(&format!("meowsh: {}", err));
                    InputLine::Eof
                }
            }
        },
        |text| {
            let clean = strip_ansi(text);
            if !crate::exec::history_add(&clean) {
                return;
            }
            let clean = clean.trim_start().to_string();
            let mut rl = rl.borrow_mut();
            let _ = rl.add_history_entry(clean.as_str());
            save_history_entry(&history_file, &clean);
        },
    );
    status
}

thread_local! {
    // The history line of the command running now, and when it started.
    static RUNNING: RefCell<Option<(crate::history::Appended, std::time::Instant)>> =
        const { RefCell::new(None) };
}

// Appends an accepted line to the history file with the time it starts.
fn save_history_entry(path: &str, line: &str) {
    if path.is_empty() {
        return;
    }
    let start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Ok(a) = crate::history::append_timed(path, line, start) {
        RUNNING.with(|r| *r.borrow_mut() = Some((a, std::time::Instant::now())));
    }
}

// Back at the prompt: the last command's line gets its elapsed time.
fn finish_history_entry() {
    if let Some((a, began)) = RUNNING.with(|r| r.borrow_mut().take()) {
        let _ = crate::history::finish_timed(&a, began.elapsed().as_secs());
    }
}

/// The interactive loop with meowsh's own line editor (ZLE). With the
/// zle option off, lines are read as the terminal delivers them, after
/// a prompt on standard error, as zsh does.
fn run_zle() -> i32 {
    // A descriptor of its own, as widgets run with standard input on
    // /dev/null.
    let tty = crate::sys::dup_high(0);
    crate::sys::set_cloexec(tty);
    crate::zle::refresh::set_tty_fd(tty);
    let history_file = crate::shell::with_shell(|s| s.history_file.clone());
    if !history_file.is_empty() {
        let entries = crate::history::read_own_file(&history_file);
        crate::shell::with_shell(|s| {
            *s.history.lock().unwrap_or_else(|e| e.into_inner()) = entries
        });
    }
    let mut plain = crate::sys::LineReader::new(0);
    let mut pending = String::new();
    let status = crate::exec::run_input(
        |cont| {
            finish_history_entry();
            let prompt = if cont {
                continuation_prompt()
            } else {
                pending.clear();
                primary_prompt()
            };
            if !crate::options::is_set("zle") {
                let _ = crate::sys::write_fd(2, prompt.as_bytes());
                wait_input_notify(0);
                return match plain.read_line() {
                    Some(l) => {
                        pending.push_str(&l);
                        InputLine::Line(l)
                    }
                    None => InputLine::Eof,
                };
            }
            let rp_names = if cont {
                ["RPROMPT2", "RPS2"]
            } else {
                ["RPROMPT", "RPS1"]
            };
            let rprompt = rp_names
                .iter()
                .find_map(|n| crate::shell::var_get_opt(n).filter(|v| !v.is_empty()))
                .map(|v| crate::shell::render_prompt(&v))
                .unwrap_or_default();
            crate::zle::set_prebuffer(&pending);
            let ctx = if cont {
                crate::zle::ZLCON_LINE_CONT
            } else {
                crate::zle::ZLCON_LINE_START
            };
            match crate::zle::zleread(
                &prompt,
                &rprompt,
                crate::zle::ZLRF_HISTORY,
                ctx,
                "zle-line-init",
                "zle-line-finish",
                None,
            ) {
                crate::zle::ReadResult::Line(l) => {
                    pending.push_str(&l);
                    InputLine::Line(l)
                }
                crate::zle::ReadResult::Interrupted => InputLine::Interrupted,
                crate::zle::ReadResult::Eof => InputLine::Eof,
            }
        },
        |text| {
            if !crate::exec::history_add(text) {
                return;
            }
            save_history_entry(&history_file, text.trim_start());
        },
    );
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::SHELL;
    use crate::TEST_LOCK;

    #[test]
    fn visible_line_drops_what_has_no_width() {
        assert_eq!(visible_line("ls -l\tx\ny"), "ls -l\tx\ny");
        assert_eq!(visible_line("safe\x08\x08evil\x7f"), "safeevil");
        assert_eq!(visible_line("a \x1b[200~b\x1b[201~ c"), "a b c");
        assert_eq!(visible_line("a\x1b[1;31mb\x1bcd"), "abd");
    }

    #[test]
    fn right_prompt_needs_a_blank_before_it() {
        // 60 columns, a 7-column right prompt, indent 1: it starts at 52.
        assert!(right_prompt_fits(52, 51));
        assert!(!right_prompt_fits(52, 52));
    }
    use rustyline::hint::Hinter;

    #[test]
    fn test_hinter_no_input() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;
        let history = FileHistory::new();
        let ctx = rustyline::Context::new(&history);
        assert_eq!(helper.hint("", 0, &ctx), None);
        assert_eq!(helper.hint("   ", 3, &ctx), None);
    }

    #[test]
    fn test_hinter_mid_line() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;
        let history = FileHistory::new();
        let ctx = rustyline::Context::new(&history);
        assert_eq!(helper.hint("echo hello", 4, &ctx), None);
    }

    #[test]
    fn test_hinter_match() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
            let mut history = shell.history.lock().unwrap_or_else(|e| e.into_inner());
            history.clear();
            history.push("ls -l".to_string());
            history.push("echo hello".to_string());
        }

        let helper = MeowshHelper;
        let history = FileHistory::new();
        let ctx = rustyline::Context::new(&history);

        // Case insensitive match and prefix match
        let hint = helper.hint("ec", 2, &ctx);
        assert!(hint.is_some());
        use rustyline::hint::Hint;
        assert_eq!(hint.as_ref().unwrap().display(), "ho hello");
        assert_eq!(hint.as_ref().unwrap().completion(), Some("ho hello"));

        let hint = helper.hint("LS", 2, &ctx);
        assert!(hint.is_some());
        assert_eq!(hint.as_ref().unwrap().display(), " -l");
        assert_eq!(hint.as_ref().unwrap().completion(), Some(" -l"));
    }

    #[test]
    fn test_hinter_no_match() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
            let mut history = shell.history.lock().unwrap_or_else(|e| e.into_inner());
            history.clear();
            history.push("ls -l".to_string());
        }

        let helper = MeowshHelper;
        let history = FileHistory::new();
        let ctx = rustyline::Context::new(&history);
        assert_eq!(helper.hint("grep", 4, &ctx), None);
    }

    #[test]
    fn test_highlight_command() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        // Known builtin
        let h = helper.highlight("echo hello", 0);
        assert!(h.contains("\x1b[32mecho\x1b[0m"));

        // Unknown command
        let h = helper.highlight("nonexistent_command", 0);
        assert!(h.contains("\x1b[31mnonexistent_command\x1b[0m"));
    }

    #[test]
    fn highlighting_a_pasted_multi_line_command_finishes() {
        // A newline (or other blank) as a word's first character used to
        // stop the word scan without moving on: the shell spun forever.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(MeowshHelper.highlight_line("echo a\necho b\r\n\x0bx"));
        });
        let out = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("highlighting hung");
        // Each line's first word is a command.
        assert_eq!(out.matches("\x1b[32mecho").count(), 2, "{:?}", out);
        assert_eq!(strip_ansi(&out), "echo a\necho b\r\n\x0bx");
    }

    #[test]
    fn test_highlight_operators() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        let h = helper.highlight("ls | grep foo && echo bar", 0);
        assert!(h.contains("\x1b[36m|\x1b[0m"));
        assert!(h.contains("\x1b[36m&&\x1b[0m"));
        // Second command 'grep' should be highlighted too
        assert!(h.contains("\x1b[32mgrep\x1b[0m") || h.contains("\x1b[31mgrep\x1b[0m"));
    }

    #[test]
    fn test_highlight_strings() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        let h = helper.highlight("echo \"hello world\"", 0);
        assert!(h.contains("\x1b[33m\"hello world\"\x1b[0m"));
    }

    #[test]
    fn test_highlight_comments() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        let h = helper.highlight("# this is a comment", 0);
        assert!(h.contains("\x1b[90m# this is a comment\x1b[0m"));
    }

    #[test]
    fn test_fuzzy_match() {
        let helper = MeowshHelper;
        assert!(helper.fuzzy_match("msh", "meowsh"));
        assert!(helper.fuzzy_match("echo", "echo"));
        assert!(helper.fuzzy_match("eo", "echo"));
        assert!(helper.fuzzy_match("E", "echo"));
        assert!(!helper.fuzzy_match("abc", "echo"));
        assert!(helper.fuzzy_match("", "anything"));
    }

    #[test]
    fn test_fuzzy_completions() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        // Test builtins fuzzy matching
        let comps = helper.get_completions("eo");
        assert!(comps.iter().any(|c| c.display == "echo"));

        let comps = helper.get_completions("rd");
        assert!(comps.iter().any(|c| c.display == "read"));
    }

    #[test]
    fn test_highlight_char_always_true() {
        let helper = MeowshHelper;
        // highlight_char must return true for all inputs so rustyline
        // re-renders the full line after every keystroke, preventing
        // stale red coloring on partial command words.
        assert!(helper.highlight_char("cargo", 0, CmdKind::MoveCursor));
        assert!(helper.highlight_char("cargo", 3, CmdKind::Other));
        assert!(helper.highlight_char("", 0, CmdKind::ForcedRefresh));
    }

    #[test]
    fn test_highlight_command_with_ansi() {
        let _lock = crate::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        // "ec\x1b[90mho" is seen as "echo", a builtin (green); the escape
        // in the line is not sent to the terminal.
        let h = helper.highlight("ec\x1b[90mho hello", 0);
        assert!(h.contains("\x1b[32mecho\x1b[0m"), "{:?}", h);
        assert!(!h.contains("\x1b[90m"), "{:?}", h);
    }

    #[test]
    fn test_complete_with_ansi() {
        let _lock = crate::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;
        let history = FileHistory::new();
        let ctx = rustyline::Context::new(&history);

        // "\x1b[31mec" -> complete should find "echo" and offset should be at "ec" (index 5)
        use rustyline::completion::Completer;
        let (offset, candidates) = helper.complete("\x1b[31mec", 7, &ctx).unwrap();
        assert_eq!(offset, 5);
        assert!(candidates.iter().any(|c| c.display == "echo"));
    }

    #[test]
    fn test_complete_paths_in_current_dir() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        // Create a temp directory with known files
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("hello.txt");
        std::fs::write(&file_path, "test").unwrap();
        let sub_dir = tmp.path().join("subdir");
        std::fs::create_dir(&sub_dir).unwrap();

        let mut completions = Vec::new();
        let partial = tmp.path().join("hel");
        let partial_str = partial.to_str().unwrap();
        helper.complete_paths(partial_str, &mut completions, false);
        assert!(completions.iter().any(|c| c.display == "hello.txt"));

        // A single matching directory is completed, not entered, as in zsh.
        std::fs::write(sub_dir.join("inner.txt"), "").unwrap();
        let mut completions = Vec::new();
        let partial = tmp.path().join("sub");
        let partial_str = partial.to_str().unwrap();
        helper.complete_paths(partial_str, &mut completions, false);
        let shown: Vec<&str> = completions.iter().map(|c| c.display.as_str()).collect();
        assert_eq!(shown, ["subdir/"]);

        // When multiple dirs/files match the prefix, directories keep their slash
        std::fs::create_dir(tmp.path().join("submarine")).unwrap();
        let mut completions = Vec::new();
        helper.complete_paths(partial_str, &mut completions, false);
        assert!(completions.iter().any(|c| c.display == "subdir/"));
        assert!(completions.iter().any(|c| c.display == "submarine/"));
    }

    #[test]
    fn test_complete_paths_hidden_files() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".hidden"), "").unwrap();
        std::fs::write(tmp.path().join("visible"), "").unwrap();

        // Without dot prefix, hidden files should be excluded
        let mut completions = Vec::new();
        let partial = tmp.path().join("");
        let partial_str = partial.to_str().unwrap();
        helper.complete_paths(partial_str, &mut completions, false);
        assert!(!completions.iter().any(|c| c.display == ".hidden"));
        assert!(completions.iter().any(|c| c.display == "visible"));

        // With dot prefix, hidden files should appear
        let mut completions = Vec::new();
        let partial = tmp.path().join(".hid");
        let partial_str = partial.to_str().unwrap();
        helper.complete_paths(partial_str, &mut completions, false);
        assert!(completions.iter().any(|c| c.display == ".hidden"));
    }

    #[test]
    fn tilde_paths_complete_without_deadlocking() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("Projects")).unwrap();
        std::fs::create_dir(home.path().join("Pictures")).unwrap();
        let old_home = crate::shell::var_get_opt("HOME");
        crate::shell::var_set("HOME", &home.path().to_string_lossy(), false);
        // Expanding `~` reads $HOME, which takes the shell lock; holding it
        // across that call hung the shell.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(MeowshHelper.get_completions("ls ~/P"));
        });
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("completion deadlocked");
        crate::shell::var_set("HOME", old_home.as_deref().unwrap_or(""), false);
        let mut names: Vec<_> = got.into_iter().map(|p| p.replacement).collect();
        names.sort();
        assert_eq!(names, ["~/Pictures/", "~/Projects/"]);
    }

    #[test]
    fn test_complete_paths_tilde_expansion() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        // ~ should expand to HOME and list files there
        let mut completions = Vec::new();
        helper.complete_paths("~/", &mut completions, false);
        // We can't assert specific files in HOME, but we should get some results
        // and no panics. Just verify it doesn't crash.
        drop(completions);
    }

    #[test]
    fn test_complete_paths_nonexistent_dir() {
        let helper = MeowshHelper;

        // Should return empty, not panic
        let mut completions = Vec::new();
        helper.complete_paths("/nonexistent_dir_12345/foo", &mut completions, false);
        assert!(completions.is_empty());
    }

    #[test]
    fn test_complete_file_in_arg_position() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("myfile.rs"), "").unwrap();

        // Simulate "cat <partial_path>" — argument position
        let partial = tmp.path().join("myf");
        let line = format!("cat {}", partial.to_str().unwrap());
        let completions = helper.get_completions(&line);
        assert!(completions.iter().any(|c| c.display == "myfile.rs"));
    }

    #[test]
    fn test_complete_empty_arg_lists_files() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        // "echo " (with trailing space) should be argument position
        // and complete files in current directory
        let completions = helper.get_completions("echo ");
        // Should have at least some files (Cargo.toml, src/, etc.)
        assert!(!completions.is_empty());
    }

    #[test]
    fn test_complete_dir_adds_trailing_slash() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("mydir")).unwrap();
        std::fs::write(tmp.path().join("myfile"), "").unwrap();

        let mut completions = Vec::new();
        let partial = tmp.path().join("my");
        let partial_str = partial.to_str().unwrap();
        helper.complete_paths(partial_str, &mut completions, false);

        let dir_comp = completions.iter().find(|c| c.display == "mydir/").unwrap();
        assert!(dir_comp.replacement.ends_with('/'));

        let file_comp = completions.iter().find(|c| c.display == "myfile").unwrap();
        assert!(!file_comp.replacement.ends_with('/'));
    }

    #[test]
    fn a_lone_directory_match_is_completed_not_entered() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("rust-remake/assets")).unwrap();
        let partial = format!("{}/r", tmp.path().display());
        // As zsh does: `cd dir/r` gives `dir/rust-remake/`, not what is in it.
        for dirs_only in [true, false] {
            let mut completions = Vec::new();
            MeowshHelper.complete_paths(&partial, &mut completions, dirs_only);
            let got: Vec<&str> = completions.iter().map(|c| c.replacement.as_str()).collect();
            assert_eq!(got, [format!("{}/rust-remake/", tmp.path().display())]);
        }
    }

    #[test]
    fn names_with_syntax_complete_escaped_and_read_back() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("chrome (1).deb"), "").unwrap();
        let dir = tmp.path().display();
        let want = format!("{dir}/chrome\\ \\(1\\).deb");
        // Typed plain, with the space escaped, and inside quotes: each is
        // one word and completes to the one name, escaped.
        for (cmd, word) in [
            ("sudo apt install ", format!("{dir}/chr")),
            ("mv ", format!("{dir}/chrome\\ (1")),
            ("mv ", format!("'{dir}/chrome (1")),
        ] {
            let line = format!("{cmd}{word}");
            let (start, got) = candidates(&line, line.len());
            let got: Vec<&str> = got.iter().map(|c| c.replacement.as_str()).collect();
            assert_eq!((start, got), (cmd.len(), vec![want.as_str()]), "{line}");
        }
    }

    #[test]
    fn test_cd_completes_dirs_only() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("alpha")).unwrap();
        std::fs::create_dir(tmp.path().join("bravo")).unwrap();
        std::fs::write(tmp.path().join("afile.txt"), "").unwrap();

        // dirs_only=true should exclude files
        let mut completions = Vec::new();
        let partial = tmp.path().join("a");
        let partial_str = partial.to_str().unwrap();
        helper.complete_paths(partial_str, &mut completions, true);
        assert!(completions.iter().any(|c| c.display == "alpha/"));
        assert!(!completions.iter().any(|c| c.display == "afile.txt"));

        // dirs_only=false should include both
        let mut completions = Vec::new();
        helper.complete_paths(partial_str, &mut completions, false);
        assert!(completions.iter().any(|c| c.display == "alpha/"));
        assert!(completions.iter().any(|c| c.display == "afile.txt"));
    }

    #[test]
    fn test_cd_completion_via_get_completions() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        // "cd " should only show directories from cwd, not files
        let completions = helper.get_completions("cd ");
        for c in &completions {
            assert!(
                c.display.ends_with('/'),
                "cd completion '{}' should be a directory",
                c.display
            );
        }
        // Should have at least src/ and tests/ from this project
        assert!(completions.iter().any(|c| c.display == "src/"));
    }
}
