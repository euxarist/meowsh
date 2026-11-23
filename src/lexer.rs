use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Word(String),
    IoNumber(i32),
    // `{name}` right before a redirection operator.
    IoVar(String),
    Op(&'static str),
    // `(( expr ))`, holding the expression text.
    Arith(String),
    Newline,
    Eof,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LexError {
    // The input ended inside a construct; more input may complete it.
    Incomplete,
    Syntax(String),
}

const OPERATORS: &[&str] = &[
    // zsh's clobbering and both-streams forms, longest first.
    "&>>|", "&>>!", ">>&|", ">>&!", ">&|", ">&!", "&>|", "&>!", ">>|", ">>!", ">>&", ">!", "&>>",
    "<<<", "<<-", "&&", "||", "|&", ";;", ";&", ";|", "<<", ">>", "<&", ">&", "<>", ">|", "&>",
    "&|", "&!", "|", "&", ";", "<", ">", "(", ")",
];

fn is_operator_start(c: char) -> bool {
    matches!(c, '|' | '&' | ';' | '<' | '>' | '(' | ')')
}

pub fn is_name_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic() || (!c.is_ascii() && c.is_alphabetic() && wide_names())
}

pub fn is_name_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric() || (!c.is_ascii() && c.is_alphanumeric() && wide_names())
}

// zsh accepts non-ASCII letters in names unless POSIX_IDENTIFIERS.
fn wide_names() -> bool {
    !crate::options::quick("posixidentifiers")
}

pub fn is_valid_name(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    name_end(&chars, 0) == Some(chars.len())
}

/// End of a parameter name at `i`, with zsh's namespaces: `name`,
/// `.ns.name`, `.ns`, `ns.name`, `ns.` and `ns.2` (a part that starts
/// with a digit is all digits). Namespaces do not nest.
pub fn name_end(c: &[char], i: usize) -> Option<usize> {
    let mut j = i;
    let dotted = c.get(j) == Some(&'.');
    if dotted {
        j += 1;
    }
    if !c.get(j).is_some_and(|&x| is_name_start(x)) {
        return None;
    }
    j += 1;
    while c.get(j).is_some_and(|&x| is_name_char(x)) {
        j += 1;
    }
    if c.get(j) == Some(&'.') {
        let k = j + 1;
        let mut e = k;
        if dotted && c.get(e).is_some_and(char::is_ascii_digit) {
            while c.get(e).is_some_and(char::is_ascii_digit) {
                e += 1;
            }
            if c.get(e).is_some_and(|&x| is_name_char(x)) {
                return None;
            }
        } else if (!dotted && c.get(e).is_some_and(|&x| is_name_char(x)))
            || (dotted && c.get(e).is_some_and(|&x| is_name_start(x)))
        {
            while c.get(e).is_some_and(|&x| is_name_char(x)) {
                e += 1;
            }
        }
        if e == k && dotted {
            return None;
        }
        if c.get(e) == Some(&'.') {
            return None;
        }
        j = e;
    }
    Some(j)
}

/// True if `s` has the form `name=...`, `name+=...`, or `name[sub]=...`.
pub fn is_assignment(s: &str) -> bool {
    assignment_split(s).is_some()
}

/// Splits an assignment word into (name, optional subscript, append, value).
pub fn assignment_split(s: &str) -> Option<(String, Option<String>, bool, String)> {
    let chars: Vec<char> = s.chars().collect();
    // zsh: `2=x` / `2+=x` assign a positional parameter.
    let digits = chars.iter().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0
        && wide_names()
        && matches!(chars.get(digits), Some('=') | Some('+'))
        && chars[0] != '0'
    {
        let append = chars[digits] == '+';
        let eq = digits + usize::from(append);
        if chars.get(eq) == Some(&'=') {
            return Some((
                chars[..digits].iter().collect(),
                None,
                append,
                chars[eq + 1..].iter().collect(),
            ));
        }
    }
    let mut i = name_end(&chars, 0)?;
    // A plain name stops at a dot that does not make a namespace.
    if chars.get(i) == Some(&'.') {
        return None;
    }
    if i == 0 {
        return None;
    }
    let name: String = chars[..i].iter().collect();
    let mut subscript = None;
    if i < chars.len() && chars[i] == '[' {
        let mut depth = 0;
        let start = i + 1;
        let mut j = i;
        while j < chars.len() {
            match chars[j] {
                // A backslash-escaped bracket does not count.
                '\\' => j += 1,
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        if j >= chars.len() || j == start {
            return None;
        }
        subscript = Some(chars[start..j].iter().collect());
        i = j + 1;
    }
    let append = i < chars.len() && chars[i] == '+';
    if append {
        i += 1;
    }
    if i >= chars.len() || chars[i] != '=' {
        return None;
    }
    let value: String = chars[i + 1..].iter().collect();
    Some((name, subscript, append, value))
}

// ---- Scanners shared with the expander --------------------------------
//
// Each takes the index of the opening character and returns the index just
// past the closing one, or None if the input ends first.

pub fn scan_squote(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < chars.len() {
        if chars[j] == '\'' {
            return Some(j + 1);
        }
        j += 1;
    }
    None
}

// `$'...'`: `i` indexes the quote after `$`.
pub fn scan_dollar_squote(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 2,
            '\'' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

pub fn scan_dquote(chars: &[char], i: usize) -> Option<usize> {
    scan_dquote_res(chars, i).ok()
}

fn scan_dquote_res(chars: &[char], i: usize) -> Result<usize, &'static str> {
    let mut j = i + 1;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 2,
            '"' => return Ok(j + 1),
            '`' => j = scan_bquote(chars, j).ok_or("unmatched \"")?,
            '$' if j + 1 < chars.len() && chars[j + 1] == '(' => {
                j = scan_paren(chars, j + 1).ok_or("unmatched \"")?
            }
            '$' if j + 1 < chars.len() && chars[j + 1] == '{' => {
                j = scan_brace_res(chars, j + 1, true)?
            }
            _ => j += 1,
        }
    }
    Err("unmatched \"")
}

pub fn scan_bquote(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 2,
            '`' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

// `${...}`: `i` indexes the `{`.
pub fn scan_brace(chars: &[char], i: usize, in_dq: bool) -> Option<usize> {
    scan_brace_res(chars, i, in_dq).ok()
}

pub fn scan_brace_res(chars: &[char], i: usize, in_dq: bool) -> Result<usize, &'static str> {
    let mut j = i + 1;
    let mut depth = 1;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 2,
            '\'' if !in_dq => j = scan_squote(chars, j).ok_or("unmatched '")?,
            '"' => j = scan_dquote_res(chars, j)?,
            '`' => j = scan_bquote(chars, j).ok_or("closing brace expected")?,
            '$' if j + 1 < chars.len() && chars[j + 1] == '(' => {
                j = scan_paren(chars, j + 1).ok_or("closing brace expected")?
            }
            '$' if j + 1 < chars.len() && chars[j + 1] == '{' => {
                j = scan_brace_res(chars, j + 1, in_dq)?
            }
            '{' => {
                depth += 1;
                j += 1;
            }
            '}' => {
                depth -= 1;
                j += 1;
                if depth == 0 {
                    return Ok(j);
                }
            }
            _ => j += 1,
        }
    }
    Err("closing brace expected")
}

// `$(...)` and `$((...))`: `i` indexes the first `(`. Unbalanced `)` that
// end case patterns are skipped while inside `case ... esac`.
pub fn scan_paren(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    let mut depth = 1;
    // The paren depth of each open `case`: its patterns' `)` close
    // nothing.
    let mut cases: Vec<usize> = Vec::new();
    while j < chars.len() {
        let c = chars[j];
        let at_word_start = j == i + 1
            || matches!(
                chars[j - 1],
                ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')'
            );
        match c {
            '\\' => j += 2,
            '\'' => j = scan_squote(chars, j)?,
            '"' => j = scan_dquote(chars, j)?,
            '`' => j = scan_bquote(chars, j)?,
            '$' if j + 1 < chars.len() && chars[j + 1] == '(' => j = scan_paren(chars, j + 1)?,
            '$' if j + 1 < chars.len() && chars[j + 1] == '{' => {
                j = scan_brace(chars, j + 1, false)?
            }
            // As in the lexer: `#` right after a parenthesis is glob
            // syntax such as `(#e)`, not a comment.
            // ... unless the `(` opens a command substitution: `$(# x`.
            '#' if at_word_start
                && (!matches!(chars[j - 1], '(' | ')')
                    || (j == i + 1 && i > 0 && chars[i - 1] == '$')) =>
            {
                while j < chars.len() && chars[j] != '\n' {
                    j += 1;
                }
            }
            '(' => {
                depth += 1;
                j += 1;
            }
            ')' => {
                j += 1;
                if cases.last() == Some(&depth) {
                    continue;
                }
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            c if at_word_start && is_name_start(c) => {
                let start = j;
                while j < chars.len() && is_name_char(chars[j]) {
                    j += 1;
                }
                let word: String = chars[start..j].iter().collect();
                let ends_word = j >= chars.len()
                    || matches!(chars[j], ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')');
                if ends_word && command_position(&chars[i + 1..start]) {
                    if word == "case" {
                        cases.push(depth);
                    } else if word == "esac" {
                        cases.pop();
                    }
                }
            }
            _ => j += 1,
        }
    }
    None
}

// Whether a word after `before` is in command position: at the start,
// after a separator, or after a reserved word that takes a command.
fn command_position(before: &[char]) -> bool {
    let text: String = before.iter().collect();
    let mut t = text.trim_end_matches([' ', '\t']);
    // A backslash-newline continues the line.
    while let Some(r) = t.strip_suffix("\\\n") {
        t = r.trim_end_matches([' ', '\t']);
    }
    // `name=(`: array elements follow, not a command.
    if t.ends_with("=(") {
        return false;
    }
    if t.is_empty() || t.ends_with(['(', ';', '&', '|', '\n', '{', '!']) {
        return true;
    }
    let last = t.rsplit([' ', '\t', '\n', ';', '(']).next().unwrap_or("");
    matches!(
        last,
        "do" | "then" | "else" | "elif" | "time" | "if" | "while" | "until" | "always"
    )
}

/// `$((...))` with `i` at the first `(`: returns the index after `))`, or
/// None if the parentheses do not close that way (then it is a command
/// substitution starting with a subshell). Unlike `scan_paren`, `#` is
/// not a comment here: `$(( #c ))` is a character code.
/// zsh's old `$[expr]` arithmetic: `i` indexes the `[`. Returns the
/// index after the matching `]`.
pub fn scan_dollar_bracket(chars: &[char], i: usize) -> Option<usize> {
    let mut depth = 0;
    let mut j = i;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 1,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

pub fn scan_arith(chars: &[char], i: usize) -> Option<usize> {
    if chars.get(i + 1) != Some(&'(') {
        return None;
    }
    let mut depth = 0usize;
    let mut j = i;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 1,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 1 {
                    return (chars.get(j + 1) == Some(&')')).then_some(j + 2);
                }
                if depth == 0 {
                    return None;
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// Length of a zsh numeric range glob `<->`, `<1-10>`, `<5->` at `i`.
pub fn numeric_range_len(chars: &[char], i: usize) -> Option<usize> {
    if chars.get(i) != Some(&'<') {
        return None;
    }
    let mut j = i + 1;
    while chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
        j += 1;
    }
    if chars.get(j) != Some(&'-') {
        return None;
    }
    j += 1;
    while chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
        j += 1;
    }
    (chars.get(j) == Some(&'>')).then_some(j + 1 - i)
}

// True if `word` is `name=`, `name+=` or `name[sub]=`: a following `(`
// starts an array value rather than continuing the word.
fn is_assign_prefix(word: &str) -> bool {
    let w = word.strip_suffix('=').unwrap_or("");
    let w = w.strip_suffix('+').unwrap_or(w);
    let name = match w.find('[') {
        Some(k) if w.ends_with(']') => &w[..k],
        _ => w,
    };
    word.ends_with('=') && is_valid_name(name)
}

/// Removes quoting from a here-document delimiter word. Returns the
/// delimiter and whether any part of it was quoted.
pub fn unquote_delimiter(raw: &str) -> (String, bool) {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::new();
    let mut quoted = false;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                quoted = true;
                if i + 1 < chars.len() {
                    out.push(chars[i + 1]);
                }
                i += 2;
            }
            // `$'...'` is decoded, the one expansion a delimiter gets.
            '$' if chars.get(i + 1) == Some(&'\'') => {
                quoted = true;
                let start = i + 2;
                let mut j = start;
                while j < chars.len() && chars[j] != '\'' {
                    if chars[j] == '\\' {
                        j += 1;
                    }
                    j += 1;
                }
                let inner: String = chars[start..j.min(chars.len())].iter().collect();
                out.push_str(&crate::expand::decode_escapes(&inner));
                i = j + 1;
            }
            '\'' => {
                quoted = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    out.push(chars[i]);
                    i += 1;
                }
                i += 1;
            }
            // In double quotes a backslash quotes only $ ` " \.
            '"' => {
                quoted = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && matches!(chars.get(i + 1), Some('$' | '`' | '"' | '\\'))
                    {
                        i += 1;
                    }
                    out.push(chars[i]);
                    i += 1;
                }
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    (out, quoted)
}

struct PendingHeredoc {
    id: usize,
    delim: String,
    strip_tabs: bool,
    // Unquoted delimiter: a backslash-newline joins lines.
    expands: bool,
}

pub struct Lexer {
    chars: Vec<char>,
    pos: usize,
    // When false, reaching the end of the input inside a construct reports
    // Incomplete so the caller can read another line.
    final_input: bool,
    pending: Vec<PendingHeredoc>,
    pub heredoc_bodies: HashMap<usize, String>,
    next_heredoc_id: usize,
    // (position, line) of the last `line()` answer, to count on from.
    line_cache: std::cell::Cell<(usize, usize)>,
    // Lines of the same input read before this text.
    pub line_offset: usize,
    // Where the last token started, for its line number.
    tok_start: usize,
    // Whether `#` at the start of a word starts a comment.
    pub comments: bool,
    // Inside `{ ... }` command groups: trailing `}` splits off words.
    pub brace_depth: usize,
}

impl Lexer {
    pub fn new(input: &str, final_input: bool) -> Self {
        Lexer {
            chars: input.chars().collect(),
            pos: 0,
            final_input,
            pending: Vec::new(),
            heredoc_bodies: HashMap::new(),
            next_heredoc_id: 0,
            line_cache: std::cell::Cell::new((0, 1)),
            line_offset: 0,
            tok_start: 0,
            comments: true,
            brace_depth: 0,
        }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    /// After a `(` token: reads the rest of a word that starts with a
    /// glob group, such as `(y|z)*`, and returns it whole.
    pub fn glob_word_after_paren(&mut self) -> Option<String> {
        let start = self.pos.checked_sub(1)?;
        if self.chars.get(start) != Some(&'(') {
            return None;
        }
        let mut depth = 0;
        let mut quote: Option<char> = None;
        let mut i = start;
        while i < self.chars.len() {
            let c = self.chars[i];
            if let Some(q) = quote {
                if c == q {
                    quote = None;
                } else if q == '"' && c == '\\' {
                    i += 1;
                }
                i += 1;
                continue;
            }
            match c {
                '\\' => i += 1,
                '\'' | '"' => quote = Some(c),
                '(' => depth += 1,
                ')' if depth == 0 => break,
                ')' => depth -= 1,
                ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' if depth == 0 => break,
                '\n' => return None,
                _ => {}
            }
            i += 1;
        }
        if depth != 0 || quote.is_some() {
            return None;
        }
        let word: String = self.chars[start..i.min(self.chars.len())].iter().collect();
        self.pos = i.min(self.chars.len());
        Some(word)
    }

    /// The next input character.
    pub fn next_char(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    /// Whether the input continues with `text` after blanks.
    pub fn next_after_blanks_is(&self, text: &str) -> bool {
        let mut i = self.pos;
        while matches!(self.chars.get(i), Some(' ' | '\t')) {
            i += 1;
        }
        text.chars()
            .enumerate()
            .all(|(k, c)| self.chars.get(i + k) == Some(&c))
    }

    /// Moves back to the start of the token last read, so it can be read
    /// again another way.
    pub fn unread_token(&mut self) {
        self.pos = self.tok_start;
    }

    /// Reads a case item's patterns up to the `)` that ends them, as zsh's
    /// lexer does in a case pattern: parentheses nest, so `|` and blanks
    /// inside a group belong to the pattern, and the alternatives are the
    /// parts between top-level `|`, blanks around them dropped. A leading
    /// `(` is POSIX's optional one when a blank follows its `)`, otherwise
    /// it starts a group (`(a|b)*)`).
    pub fn case_patterns(&mut self) -> Result<Vec<String>, LexError> {
        let c = &self.chars;
        let mut i = self.pos;
        while c.get(i).is_some_and(|&ch| ch == ' ' || ch == '\t') {
            i += 1;
        }
        // The index of the `)` that brings the depth below zero, from `i`.
        let close = |mut j: usize| -> Option<usize> {
            let mut depth = 0usize;
            while j < c.len() {
                match c[j] {
                    '\\' => j += 1,
                    '\'' => j = scan_squote(c, j)? - 1,
                    '"' => j = scan_dquote(c, j)? - 1,
                    '`' => j = scan_bquote(c, j)? - 1,
                    '$' if c.get(j + 1) == Some(&'\'') => j = scan_dollar_squote(c, j + 1)? - 1,
                    '$' if c.get(j + 1) == Some(&'{') => j = scan_brace(c, j + 1, false)? - 1,
                    '(' => depth += 1,
                    ')' if depth == 0 => return Some(j),
                    ')' => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            None
        };
        let (start, end) = match c.get(i) {
            Some('(') => {
                let Some(j) = close(i + 1) else {
                    return Err(self.incomplete("unmatched ("));
                };
                if c.get(j + 1).is_none_or(|ch| ch.is_whitespace()) {
                    (i + 1, j)
                } else {
                    match close(i) {
                        Some(k) => (i, k),
                        None => return Err(self.incomplete("unmatched (")),
                    }
                }
            }
            _ => match close(i) {
                Some(k) => (i, k),
                None => return Err(self.incomplete("unmatched (")),
            },
        };
        // Split at the top-level `|`s.
        let mut alts = Vec::new();
        let mut from = start;
        let mut j = start;
        let mut depth = 0usize;
        while j < end {
            match c[j] {
                '\\' => j += 1,
                '\'' => j = scan_squote(c, j).map_or(end, |e| e - 1),
                '"' => j = scan_dquote(c, j).map_or(end, |e| e - 1),
                '`' => j = scan_bquote(c, j).map_or(end, |e| e - 1),
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                '|' if depth == 0 => {
                    alts.push(c[from..j].iter().collect::<String>());
                    from = j + 1;
                }
                _ => {}
            }
            j += 1;
        }
        alts.push(c[from..end].iter().collect::<String>());
        self.pos = end + 1;
        Ok(alts
            .into_iter()
            .map(|a| a.trim_matches([' ', '\t', '\n']).to_string())
            .collect())
    }

    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos.min(self.chars.len());
    }

    /// 1-based line number of the current position.
    pub fn line(&self) -> usize {
        let pos = self.pos.min(self.chars.len());
        let (mut from, mut line) = self.line_cache.get();
        if pos < from {
            (from, line) = (0, 1);
        }
        line += self.chars[from..pos].iter().filter(|&&c| c == '\n').count();
        self.line_cache.set((pos, line));
        line + self.line_offset
    }

    pub fn rest(&self) -> String {
        self.chars[self.pos.min(self.chars.len())..]
            .iter()
            .collect()
    }

    /// Splices alias text into the input at the current position.
    pub fn insert_text(&mut self, text: &str) {
        let at = self.pos.min(self.chars.len());
        self.chars.splice(at..at, text.chars());
    }

    fn incomplete(&self, msg: &str) -> LexError {
        if self.final_input {
            LexError::Syntax(msg.to_string())
        } else {
            LexError::Incomplete
        }
    }

    /// Registers a here-document whose body starts after the next newline.
    pub fn add_heredoc(&mut self, raw_delim: &str, strip_tabs: bool) -> (usize, bool) {
        let (delim, quoted) = unquote_delimiter(raw_delim);
        let id = self.next_heredoc_id;
        self.next_heredoc_id += 1;
        self.pending.push(PendingHeredoc {
            id,
            delim,
            strip_tabs,
            expands: !quoted,
        });
        (id, !quoted)
    }

    pub fn has_pending_heredocs(&self) -> bool {
        !self.pending.is_empty()
    }

    fn read_heredocs(&mut self) -> Result<(), LexError> {
        let pending = std::mem::take(&mut self.pending);
        for hd in pending {
            let mut body = String::new();
            // The previous line ended in a backslash that joins this one:
            // no tab stripping and no delimiter here.
            let mut continued = false;
            loop {
                if self.pos >= self.chars.len() {
                    if !self.final_input {
                        return Err(LexError::Incomplete);
                    }
                    // zsh takes the end of input as the end silently; the
                    // warning is bash's, for sh emulation.
                    if crate::exec::sh_emulation_pub() {
                        eprintln!(
                            "meowsh: warning: here-document delimited by end-of-file (wanted `{}')",
                            hd.delim
                        );
                    }
                    break;
                }
                let start = self.pos;
                let mut end = start;
                while end < self.chars.len() && self.chars[end] != '\n' {
                    end += 1;
                }
                if end >= self.chars.len() && !self.final_input {
                    return Err(LexError::Incomplete);
                }
                let mut line: String = self.chars[start..end].iter().collect();
                if hd.strip_tabs && !continued {
                    line = line.trim_start_matches('\t').to_string();
                }
                self.pos = (end + 1).min(self.chars.len());
                if !continued && line == hd.delim {
                    break;
                }
                let trailing = line.chars().rev().take_while(|&c| c == '\\').count();
                continued = hd.expands && trailing % 2 == 1;
                body.push_str(&line);
                body.push('\n');
            }
            self.heredoc_bodies.insert(hd.id, body);
        }
        Ok(())
    }

    /// The line the last token read started on.
    pub fn tok_line(&self) -> usize {
        let pos = self.tok_start.min(self.chars.len());
        self.chars[..pos].iter().filter(|&&c| c == '\n').count() + 1 + self.line_offset
    }

    pub fn next_token(&mut self) -> Result<Tok, LexError> {
        while self.pos < self.chars.len() && matches!(self.chars[self.pos], ' ' | '\t') {
            self.pos += 1;
        }
        self.tok_start = self.pos;
        self.next_token_inner()
    }

    fn next_token_inner(&mut self) -> Result<Tok, LexError> {
        loop {
            while self.pos < self.chars.len() && matches!(self.chars[self.pos], ' ' | '\t') {
                self.pos += 1;
            }
            if self.pos + 1 < self.chars.len()
                && self.chars[self.pos] == '\\'
                && self.chars[self.pos + 1] == '\n'
            {
                self.pos += 2;
                if self.pos >= self.chars.len() && !self.final_input {
                    return Err(LexError::Incomplete);
                }
                continue;
            }
            // `#` starts a comment at the start of a word, but not right
            // after `(` or `)`, where zsh reads glob syntax such as `(#i)`
            // and `(ab)##`.
            if self.comments
                && self.pos < self.chars.len()
                && self.chars[self.pos] == '#'
                && !(self.pos > 0 && matches!(self.chars[self.pos - 1], '(' | ')'))
            {
                while self.pos < self.chars.len() && self.chars[self.pos] != '\n' {
                    self.pos += 1;
                }
                continue;
            }
            break;
        }

        if self.pos >= self.chars.len() {
            if !self.pending.is_empty() {
                // `cat <<EOF` with no newline after it.
                if !self.final_input {
                    return Err(LexError::Incomplete);
                }
                self.read_heredocs()?;
            }
            return Ok(Tok::Eof);
        }

        let c = self.chars[self.pos];
        if c == '\n' {
            self.pos += 1;
            if !self.pending.is_empty() {
                self.read_heredocs()?;
            }
            return Ok(Tok::Newline);
        }

        if c.is_ascii_digit() {
            let mut j = self.pos;
            while j < self.chars.len() && self.chars[j].is_ascii_digit() {
                j += 1;
            }
            // `2<->`: the digits belong to a numeric range pattern, not to
            // a redirection.
            if j < self.chars.len()
                && matches!(self.chars[j], '<' | '>')
                && numeric_range_len(&self.chars, j).is_none()
            {
                let digits: String = self.chars[self.pos..j].iter().collect();
                if let Ok(n) = digits.parse::<i32>() {
                    self.pos = j;
                    return Ok(Tok::IoNumber(n));
                }
            }
        }

        if c == '{' {
            let mut j = self.pos + 1;
            while j < self.chars.len() && is_name_char(self.chars[j]) {
                j += 1;
            }
            if j > self.pos + 1
                && is_name_start(self.chars[self.pos + 1])
                && self.chars.get(j) == Some(&'}')
                && matches!(self.chars.get(j + 1), Some('<' | '>'))
                && self.chars.get(j + 2) != Some(&'(')
            {
                let name: String = self.chars[self.pos + 1..j].iter().collect();
                self.pos = j + 1;
                return Ok(Tok::IoVar(name));
            }
        }

        if c == '('
            && self.chars.get(self.pos + 1) == Some(&'(')
            && self.pos.checked_sub(1).and_then(|p| self.chars.get(p)) != Some(&'=')
        {
            if let Some(t) = self.try_arith()? {
                return Ok(t);
            }
        }

        let procsub = matches!(c, '<' | '>') && self.chars.get(self.pos + 1) == Some(&'(');
        let range = numeric_range_len(&self.chars, self.pos).is_some();
        if is_operator_start(c) && !procsub && !range {
            for op in OPERATORS {
                let len = op.chars().count();
                if self.pos + len <= self.chars.len()
                    && self.chars[self.pos..self.pos + len]
                        .iter()
                        .copied()
                        .eq(op.chars())
                {
                    self.pos += len;
                    return Ok(Tok::Op(op));
                }
            }
        }

        // zsh: `{` starting a word in command position is a word of its
        // own, so `{cmd}` is a brace group.
        if c == '{'
            && self
                .chars
                .get(self.pos + 1)
                .is_some_and(|n| !matches!(n, ' ' | '\t' | '\n'))
            && !crate::options::is_set("ignorebraces")
            && command_position(&self.chars[..self.pos])
        {
            self.pos += 1;
            return Ok(Tok::Word("{".to_string()));
        }
        self.read_word().map(Tok::Word)
    }

    // `((` starts an arithmetic command when its parentheses close with
    // `))`; otherwise it is two subshell openers, as in `((cmd); cmd)`.
    fn try_arith(&mut self) -> Result<Option<Tok>, LexError> {
        if let Some(end) = scan_arith(&self.chars, self.pos) {
            let expr: String = self.chars[self.pos + 2..end - 2].iter().collect();
            self.pos = end;
            return Ok(Some(Tok::Arith(expr)));
        }
        if scan_paren(&self.chars, self.pos).is_none() && !self.final_input {
            return Err(LexError::Incomplete);
        }
        Ok(None)
    }

    fn read_word(&mut self) -> Result<String, LexError> {
        let mut out = String::new();
        let mut brace_depth = 0usize;
        while self.pos < self.chars.len() {
            let c = self.chars[self.pos];
            match c {
                ' ' | '\t' | '\n' => break,
                // zsh splits an unpaired trailing `}` off a word inside `{ ... }`
                // groups so `{cmd}` and `ALIAS}` work cleanly before aliases.
                '}' if !out.is_empty()
                    && self.brace_depth > 0
                    && brace_depth == 0
                    && !crate::options::is_set("ignorebraces")
                    && !crate::exec::sh_emulation_pub()
                    && self.chars.get(self.pos + 1).is_none_or(|&n| {
                        matches!(n, ' ' | '\t' | '\n') || is_operator_start(n)
                    }) =>
                {
                    break;
                }
                // Process substitution `<(...)` / `>(...)` at word start.
                // Process substitution, also inside a word as in zsh.
                '<' | '>'
                    if self.chars.get(self.pos + 1) == Some(&'(')
                        && (out.is_empty() || !out.chars().all(|c| c.is_ascii_digit())) =>
                {
                    let end = scan_paren(&self.chars, self.pos + 1).ok_or_else(|| {
                        self.incomplete("unexpected end of file while looking for matching `)'")
                    })?;
                    out.extend(&self.chars[self.pos..end]);
                    self.pos = end;
                }
                '<' if numeric_range_len(&self.chars, self.pos).is_some() => {
                    let len = numeric_range_len(&self.chars, self.pos).unwrap();
                    out.extend(&self.chars[self.pos..self.pos + len]);
                    self.pos += len;
                }
                // zsh: `(` inside a word continues it, as in the glob
                // qualifier `*(N)` or the pattern `x_(a|b)_*`. Not after
                // `name=` (array value) and not `name()` (function).
                // SH_GLOB: parentheses aren't glob syntax, so `(` ends the
                // word (`while(cmd)`), except in a KSH_GLOB `!(pat)`.
                '(' if !out.is_empty()
                    && !is_assign_prefix(&out)
                    && self.chars.get(self.pos + 1) != Some(&')')
                    && (!crate::options::is_set("shglob")
                        || (crate::options::is_set("kshglob")
                            && out.ends_with(['@', '*', '+', '?', '!']))) =>
                {
                    let end = scan_paren(&self.chars, self.pos).ok_or_else(|| {
                        self.incomplete("unexpected end of file while looking for matching `)'")
                    })?;
                    out.extend(&self.chars[self.pos..end]);
                    self.pos = end;
                }
                c if is_operator_start(c) => break,
                '\\' => {
                    if self.pos + 1 >= self.chars.len() {
                        out.push('\\');
                        self.pos += 1;
                    } else if self.chars[self.pos + 1] == '\n' {
                        self.pos += 2;
                        if self.pos >= self.chars.len() && !self.final_input {
                            return Err(LexError::Incomplete);
                        }
                    } else {
                        out.push('\\');
                        out.push(self.chars[self.pos + 1]);
                        self.pos += 2;
                    }
                }
                '\'' if crate::options::is_set("cshjunkiequotes") => {
                    // CSH_JUNKIE_QUOTES: a quote may not span lines, except
                    // after a backslash, which is then dropped.
                    out.push('\'');
                    self.pos += 1;
                    loop {
                        match self.chars.get(self.pos) {
                            None => return Err(self.incomplete("unmatched '")),
                            Some('\'') => break,
                            Some('\n') => return Err(LexError::Syntax("unmatched '".into())),
                            Some('\\') if self.chars.get(self.pos + 1) == Some(&'\n') => {
                                out.push('\n');
                                self.pos += 2;
                            }
                            Some(&c) => {
                                out.push(c);
                                self.pos += 1;
                            }
                        }
                    }
                    out.push('\'');
                    self.pos += 1;
                }
                '\'' => {
                    let end = scan_squote(&self.chars, self.pos)
                        .ok_or_else(|| self.incomplete("unmatched '"))?;
                    out.extend(&self.chars[self.pos..end]);
                    self.pos = end;
                }
                '"' => {
                    let end = scan_dquote(&self.chars, self.pos)
                        .ok_or_else(|| self.incomplete("unmatched \""))?;
                    if crate::options::is_set("cshjunkiequotes") {
                        let body = &self.chars[self.pos..end];
                        if body
                            .iter()
                            .enumerate()
                            .any(|(k, &c)| c == '\n' && (k == 0 || body[k - 1] != '\\'))
                        {
                            return Err(LexError::Syntax("unmatched \"".into()));
                        }
                    }
                    out.extend(&self.chars[self.pos..end]);
                    self.pos = end;
                }
                '`' => {
                    let end = scan_bquote(&self.chars, self.pos)
                        .ok_or_else(|| self.incomplete("unmatched `"))?;
                    out.extend(&self.chars[self.pos..end]);
                    self.pos = end;
                }
                '$' if self.chars.get(self.pos + 1) == Some(&'\\')
                    && self.chars.get(self.pos + 2) == Some(&'\n')
                    && self.chars.get(self.pos + 3) == Some(&'(') =>
                {
                    let open = self.pos + 3;
                    let end = match scan_arith(&self.chars, open) {
                        Some(e) => Some(e),
                        None => scan_paren(&self.chars, open),
                    };
                    match end {
                        Some(end) => {
                            out.push('$');
                            out.extend(&self.chars[open..end]);
                            self.pos = end;
                        }
                        None => {
                            self.pos = open;
                            let rest: String = self.chars[open + 1..].iter().collect();
                            return Err(self.incomplete(&format!(
                                "parse error near `$({}'",
                                rest.trim_end_matches('\n')
                            )));
                        }
                    }
                }
                '$' if self.pos + 1 < self.chars.len() => {
                    let next = self.chars[self.pos + 1];
                    let end = match next {
                        '[' => scan_dollar_bracket(&self.chars, self.pos + 1),
                        '(' => Some(
                            match scan_arith(&self.chars, self.pos + 1) {
                                Some(e) => Some(e),
                                None => scan_paren(&self.chars, self.pos + 1),
                            }
                            .ok_or_else(|| {
                                let rest: String = self.chars[self.pos..].iter().collect();
                                self.incomplete(&format!(
                                    "parse error near `{}'",
                                    rest.trim_end_matches('\n')
                                ))
                            })?,
                        ),
                        '{' => Some(
                            scan_brace_res(&self.chars, self.pos + 1, false)
                                .map_err(|msg| self.incomplete(msg))?,
                        ),
                        '\'' => Some(
                            scan_dollar_squote(&self.chars, self.pos + 1)
                                .ok_or_else(|| self.incomplete("unmatched '"))?,
                        ),
                        _ => None,
                    };
                    match end {
                        Some(end) => {
                            out.extend(&self.chars[self.pos..end]);
                            self.pos = end;
                        }
                        None => {
                            out.push('$');
                            self.pos += 1;
                        }
                    }
                }
                '{' => {
                    brace_depth += 1;
                    out.push('{');
                    self.pos += 1;
                }
                '}' => {
                    brace_depth = brace_depth.saturating_sub(1);
                    out.push('}');
                    self.pos += 1;
                }
                c => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(s: &str) -> Vec<Tok> {
        let mut lx = Lexer::new(s, true);
        let mut out = Vec::new();
        loop {
            let t = lx.next_token().unwrap();
            if t == Tok::Eof {
                break;
            }
            out.push(t);
        }
        out
    }

    #[test]
    fn non_ascii_words_do_not_panic() {
        assert_eq!(
            tokens("echo é 'ü' \"ß\" # ç"),
            vec![
                Tok::Word("echo".into()),
                Tok::Word("é".into()),
                Tok::Word("'ü'".into()),
                Tok::Word("\"ß\"".into()),
            ]
        );
    }

    #[test]
    fn nested_substitutions_stay_one_word() {
        assert_eq!(
            tokens("echo ${x:-$(echo \"}\")}y z"),
            vec![
                Tok::Word("echo".into()),
                Tok::Word("${x:-$(echo \"}\")}y".into()),
                Tok::Word("z".into())
            ]
        );
    }

    #[test]
    fn case_inside_command_substitution() {
        assert_eq!(
            tokens("echo $(case a in a) echo y;; esac)"),
            vec![
                Tok::Word("echo".into()),
                Tok::Word("$(case a in a) echo y;; esac)".into())
            ]
        );
    }

    #[test]
    fn unterminated_quote_is_incomplete_when_streaming() {
        let mut lx = Lexer::new("echo 'abc\n", false);
        assert_eq!(lx.next_token(), Ok(Tok::Word("echo".into())));
        assert_eq!(lx.next_token(), Err(LexError::Incomplete));
    }

    #[test]
    fn assignment_detection() {
        assert!(is_assignment("a=1"));
        assert!(is_assignment("a+=1"));
        assert!(is_assignment("a[1]=x"));
        assert!(!is_assignment("1a=b"));
        assert!(!is_assignment("=b"));
        assert!(!is_assignment("a"));
    }
}
