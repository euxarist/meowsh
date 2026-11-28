//! Shell pattern matching (`*`, `?`, `[...]`, zsh `(a|b)` groups,
//! EXTENDED_GLOB `#`, `##`, `^`, `~` and `(#...)` flags, KSH_GLOB
//! `*(...)` forms, backslash escapes) used by `case`, `[[ == ]]`, globbing
//! and the `${x#pat}` family. Patterns come from the expander with quoted
//! characters already backslash-escaped.
//!
//! A pattern compiles to a tree of nodes. Matching follows every way a
//! node can consume the text: from a set of (position, errors) states it
//! yields the states after it, so repetition, negation and exclusion need
//! no backtracking. Errors count approximate-matching (`(#aN)`) edits.

use std::collections::BTreeMap;

// How a literal compares: exactly, ignoring case (`(#i)`), or with
// lower-case pattern letters matching either case (`(#l)`).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Fold {
    Exact,
    Icase,
    Lower,
}

#[derive(Debug, Clone)]
enum Node {
    Lit(char, Fold, u32),
    Any(u32),
    Class {
        negated: bool,
        set: Vec<ClassItem>,
        fold: Fold,
        approx: u32,
    },
    Star,
    // zsh `<lo-hi>`: a run of digits whose value is in range.
    Range(Option<u64>, Option<u64>),
    Seq(Vec<Node>),
    // `(a|b)`; with (#b), the capture slot.
    Alt(Vec<Node>, Option<usize>),
    // `x#`, `x##`, `x(#cN,M)`, KSH_GLOB `*(x)`, `+(x)`, `?(x)`.
    Repeat(Box<Node>, usize, Option<usize>),
    // `^x`, KSH_GLOB `!(x)`: any text x does not match.
    Not(Box<Node>),
    // `x~y~z`: what x matches unless y or z matches the same text.
    Exclude(Box<Node>, Vec<Node>),
    // `(#s)` and `(#e)`: the start and end of the text.
    Start,
    End,
}

// Compile-time state: EXTENDED_GLOB, KSH_GLOB, SH_GLOB, the case and
// approximation flags in effect, and (#b) / (#m).
#[derive(Clone)]
struct Ctx {
    ext: bool,
    // A malformed pattern, such as (#c...) with nothing before it.
    bad: bool,
    ksh: bool,
    shglob: bool,
    fold: Fold,
    approx: u32,
    backref: bool,
    groups: usize,
    matchref: bool,
}

#[derive(Debug, Clone)]
enum ClassItem {
    Char(char),
    Range(char, char),
    Named(String),
}

/// With EXTENDED_GLOB, a glob `a~b`: (a, b), b being matched against
/// whole paths.
pub fn glob_exclusion(pattern: &str) -> Option<(String, String)> {
    if !crate::options::quick("extendedglob") || !pat_on("~") {
        return None;
    }
    let p: Vec<char> = pattern.chars().collect();
    let k = split_exclusion(&p)?;
    Some((p[..k].iter().collect(), p[k + 1..].iter().collect()))
}

fn pat_on(op: &str) -> bool {
    !crate::builtins::is_disabled('p', op)
}

// Splits an EXTENDED_GLOB `a~b` at the top-level `~` (not a final one).
fn split_exclusion(p: &[char]) -> Option<usize> {
    let mut depth = 0;
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            '\\' => i += 1,
            '(' => depth += 1,
            ')' => depth -= 1,
            '[' => {
                if let Some((_, _, next)) = parse_class(p, i) {
                    i = next;
                    continue;
                }
            }
            '~' if depth == 0 && i > 0 && i + 1 < p.len() => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

// Index just past the `)` closing the group opened at `p[open]`.
fn group_end(p: &[char], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < p.len() {
        match p[i] {
            '\\' => i += 1,
            '[' if i > open => {
                if let Some((_, _, next)) = parse_class(p, i) {
                    i = next;
                    continue;
                }
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// Parses a bracket expression starting at `p[start] == '['`: (negated,
// set, index after it).
fn parse_class(p: &[char], start: usize) -> Option<(bool, Vec<ClassItem>, usize)> {
    let mut i = start + 1;
    let mut negated = false;
    if i < p.len() && (p[i] == '!' || p[i] == '^') {
        negated = true;
        i += 1;
    }
    let mut set = Vec::new();
    let mut first = true;
    while i < p.len() {
        let c = p[i];
        if c == ']' && !first {
            return Some((negated, set, i + 1));
        }
        first = false;
        if c == '[' && i + 1 < p.len() && p[i + 1] == ':' {
            if let Some(end) =
                (i + 2..p.len().saturating_sub(1)).find(|&k| p[k] == ':' && p[k + 1] == ']')
            {
                let name: String = p[i + 2..end].iter().collect();
                set.push(ClassItem::Named(name));
                i = end + 2;
                continue;
            }
        }
        let lo = if c == '\\' && i + 1 < p.len() {
            i += 1;
            p[i]
        } else {
            c
        };
        // A quoted `-` is not a range.
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            let mut hi = p[i + 2];
            let mut adv = 3;
            if hi == '\\' && i + 3 < p.len() {
                hi = p[i + 3];
                adv = 4;
            }
            set.push(ClassItem::Range(lo, hi));
            i += adv;
        } else {
            set.push(ClassItem::Char(lo));
            i += 1;
        }
    }
    None
}

fn raw_byte_state(s: &[char], pos: usize) -> Option<bool> {
    if crate::shell::is_c_locale() {
        return None;
    }
    crate::shell::decode_raw_byte(s[pos])?;
    let mut run = Vec::new();
    for &c in &s[pos..] {
        if let Some(b) = crate::shell::decode_raw_byte(c) {
            run.push(b);
        } else {
            let mut buf = [0u8; 4];
            run.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            break;
        }
    }
    match std::str::from_utf8(&run) {
        Err(e) => Some(e.error_len().is_none()),
        Ok(_) => None,
    }
}

fn class_matches(set: &[ClassItem], c: char, raw_state: Option<bool>) -> bool {
    set.iter().any(|item| match item {
        ClassItem::Char(x) => *x == c,
        ClassItem::Range(lo, hi) => *lo <= c && c <= *hi,
        ClassItem::Named(name) => match name.as_str() {
            "alpha" => c.is_alphabetic(),
            "digit" => c.is_ascii_digit(),
            "alnum" => c.is_alphanumeric(),
            "upper" => c.is_uppercase(),
            "lower" => c.is_lowercase(),
            "space" => c.is_whitespace(),
            "blank" => c == ' ' || c == '\t',
            "punct" => c.is_ascii_punctuation(),
            "print" => !c.is_control() && raw_state.is_none(),
            "graph" => !c.is_control() && !c.is_whitespace() && raw_state.is_none(),
            "cntrl" => c.is_control(),
            "xdigit" => c.is_ascii_hexdigit(),
            // zsh's own classes.
            "IDENT" => crate::lexer::is_name_char(c),
            "IFS" => crate::shell::var_get_opt("IFS")
                .unwrap_or_else(|| " \t\n\0".to_string())
                .contains(c),
            "IFSSPACE" => {
                crate::shell::var_get_opt("IFS")
                    .unwrap_or_else(|| " \t\n".to_string())
                    .contains(c)
                    && " \t\n".contains(c)
            }
            "WORD" => {
                crate::lexer::is_name_char(c)
                    || crate::shell::var_get_opt("WORDCHARS")
                        .unwrap_or_default()
                        .contains(c)
            }
            "INCOMPLETE" => raw_state == Some(true),
            "INVALID" => raw_state == Some(false),
            _ => false,
        },
    })
}

fn fold_eq(pat: char, c: char, fold: Fold) -> bool {
    match fold {
        Fold::Exact => pat == c,
        Fold::Icase => pat == c || lower(pat) == lower(c),
        Fold::Lower => {
            if pat.is_lowercase() {
                lower(c) == pat
            } else {
                pat == c
            }
        }
    }
}

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn class_fold(set: &[ClassItem], c: char, fold: Fold, raw_state: Option<bool>) -> bool {
    if class_matches(set, c, raw_state) {
        return true;
    }
    if fold == Fold::Exact || raw_state.is_some() {
        return false;
    }
    let l = lower(c);
    let u = c.to_uppercase().next().unwrap_or(c);
    class_matches(set, l, None) || class_matches(set, u, None)
}

// ---- compiling ------------------------------------------------------------

fn compile(p: &[char], cx: &mut Ctx) -> Node {
    let mut i = 0;
    let node = parse_alts(p, &mut i, false, cx);
    // A stray `)` at the top: the rest is literal text.
    if i < p.len() {
        let mut items = vec![node];
        for &c in &p[i..] {
            items.push(Node::Lit(c, cx.fold, 0));
        }
        return Node::Seq(items);
    }
    node
}

// Alternatives up to `)` (inside a group, which is consumed) or the end.
// `|` only separates alternatives inside a group.
fn parse_alts(p: &[char], i: &mut usize, nested: bool, cx: &mut Ctx) -> Node {
    let saved = (cx.fold, cx.approx);
    let mut alts = vec![parse_branch(p, i, nested, cx)];
    // zsh also alternates at the top level: `[[ x = a|b ]]`.
    while (nested || !cx.shglob) && pat_on("|") && *i < p.len() && p[*i] == '|' {
        *i += 1;
        // Each alternative starts with the group's flags.
        cx.fold = saved.0;
        cx.approx = saved.1;
        alts.push(parse_branch(p, i, nested, cx));
    }
    if nested && *i < p.len() && p[*i] == ')' {
        *i += 1;
    }
    // Flags last only to the end of their group.
    cx.fold = saved.0;
    cx.approx = saved.1;
    if alts.len() == 1 {
        alts.pop().unwrap()
    } else {
        Node::Alt(alts, None)
    }
}

// One alternative: a sequence, `^` negating the rest, `~` exclusions.
fn parse_branch(p: &[char], i: &mut usize, nested: bool, cx: &mut Ctx) -> Node {
    let first = parse_neg_seq(p, i, nested, cx);
    let mut excl = Vec::new();
    while cx.ext && pat_on("~") && *i < p.len() && p[*i] == '~' {
        *i += 1;
        // Exclusions keep their own approximation.
        let saved = cx.approx;
        cx.approx = 0;
        excl.push(parse_neg_seq(p, i, nested, cx));
        cx.approx = saved;
    }
    if excl.is_empty() {
        first
    } else {
        Node::Exclude(Box::new(first), excl)
    }
}

// A sequence, negated by a leading `^`.
fn parse_neg_seq(p: &[char], i: &mut usize, nested: bool, cx: &mut Ctx) -> Node {
    if cx.ext && pat_on("^") && *i + 1 < p.len() && p[*i] == '^' {
        *i += 1;
        Node::Not(Box::new(parse_seq(p, i, nested, cx)))
    } else {
        parse_seq(p, i, nested, cx)
    }
}

fn at_branch_end(p: &[char], i: usize, nested: bool) -> bool {
    i >= p.len() || (p[i] == '|' && pat_on("|")) || (nested && p[i] == ')')
}

fn parse_seq(p: &[char], i: &mut usize, nested: bool, cx: &mut Ctx) -> Node {
    let mut items: Vec<Node> = Vec::new();
    while *i < p.len() {
        let c = p[*i];
        if (nested && c == ')') || (c == '|' && (nested || !cx.shglob) && pat_on("|")) {
            break;
        }
        // `~` before more pattern text is an exclusion; a final one is
        // literal, as is one at the very start.
        if c == '~'
            && cx.ext
            && pat_on("~")
            && !items.is_empty()
            && !at_branch_end(p, *i + 1, nested)
        {
            break;
        }
        let i0 = *i;
        // `@(#I)`: under KSH_GLOB a flag group, the `@` dropped.
        if cx.ksh
            && pat_on("@(")
            && c == '@'
            && cx.ext
            && p.get(i0 + 1) == Some(&'(')
            && p.get(i0 + 2) == Some(&'#')
        {
            *i += 1;
            continue;
        }
        // KSH_GLOB `*(..)`, `+(..)`, `?(..)`, `@(..)`, `!(..)`.
        let ksh_op = match c {
            '*' => Some("*("),
            '+' => Some("+("),
            '?' => Some("?("),
            '@' => Some("@("),
            '!' => Some("!("),
            _ => None,
        };
        if cx.ksh
            && ksh_op.is_some_and(pat_on)
            && p.get(i0 + 1) == Some(&'(')
            && group_end(p, i0 + 1).is_some()
        {
            *i += 2;
            let inner = parse_alts(p, i, true, cx);
            items.push(match c {
                '*' => Node::Repeat(Box::new(inner), 0, None),
                '+' => Node::Repeat(Box::new(inner), 1, None),
                '?' => Node::Repeat(Box::new(inner), 0, Some(1)),
                '!' => Node::Not(Box::new(inner)),
                _ => inner,
            });
            continue;
        }
        match c {
            '\\' if i0 + 1 < p.len() => {
                items.push(Node::Lit(p[i0 + 1], cx.fold, cx.approx));
                *i += 2;
            }
            '*' if pat_on("*") => {
                *i += 1;
                if !matches!(items.last(), Some(Node::Star)) {
                    items.push(Node::Star);
                }
            }
            '?' if pat_on("?") => {
                *i += 1;
                items.push(Node::Any(cx.approx));
            }
            '[' if pat_on("[") => match parse_class(p, i0) {
                Some((negated, set, next)) => {
                    *i = next;
                    items.push(Node::Class {
                        negated,
                        set,
                        fold: cx.fold,
                        approx: cx.approx,
                    });
                }
                None => {
                    *i += 1;
                    items.push(Node::Lit('[', cx.fold, cx.approx));
                }
            },
            '#' if cx.ext && pat_on("#") && !items.is_empty() => {
                let last = items.pop().unwrap();
                let min = if p.get(i0 + 1) == Some(&'#') {
                    *i += 1;
                    1
                } else {
                    0
                };
                *i += 1;
                items.push(Node::Repeat(Box::new(last), min, None));
            }
            // (#i), (#b), (#a2), (#s), (#cN,M) and the other flags.
            '(' if cx.ext && pat_on("(") && p.get(i0 + 1) == Some(&'#') => {
                let end = (i0 + 2..p.len()).find(|&k| p[k] == ')').unwrap_or(p.len());
                let flags: Vec<char> = p[i0 + 2..end].to_vec();
                *i = (end + 1).min(p.len());
                let mut j = 0;
                while j < flags.len() {
                    match flags[j] {
                        'i' => cx.fold = Fold::Icase,
                        'l' => cx.fold = Fold::Lower,
                        'I' => cx.fold = Fold::Exact,
                        'b' => cx.backref = true,
                        'B' => cx.backref = false,
                        'm' => cx.matchref = true,
                        'M' => cx.matchref = false,
                        's' => items.push(Node::Start),
                        'e' => items.push(Node::End),
                        // (#q...): glob qualifiers, nothing to match.
                        'q' => break,
                        'a' => {
                            let digits: String = flags[j + 1..]
                                .iter()
                                .take_while(|c| c.is_ascii_digit())
                                .collect();
                            j += digits.len();
                            cx.approx = digits.parse().unwrap_or(1);
                        }
                        'c' => {
                            let spec: String = flags[j + 1..].iter().collect();
                            let (lo, hi) = match spec.split_once(',') {
                                Some((a, b)) => (a.parse().unwrap_or(0), b.parse().ok()),
                                None => {
                                    let n = spec.parse().unwrap_or(0);
                                    (n, Some(n))
                                }
                            };
                            match items.pop() {
                                Some(last) => items.push(Node::Repeat(Box::new(last), lo, hi)),
                                None => cx.bad = true,
                            }
                            break;
                        }
                        _ => {}
                    }
                    j += 1;
                }
            }
            // SH_GLOB: `<n-m>` and `(a|b)` are plain characters.
            '<' if !cx.shglob
                && pat_on("<")
                && crate::lexer::numeric_range_len(p, i0).is_some() =>
            {
                let len = crate::lexer::numeric_range_len(p, i0).unwrap();
                let inner: String = p[i0 + 1..i0 + len - 1].iter().collect();
                let (lo, hi) = inner.split_once('-').unwrap_or(("", ""));
                items.push(Node::Range(lo.parse().ok(), hi.parse().ok()));
                *i += len;
            }
            '(' if !cx.shglob && pat_on("(") && group_end(p, i0).is_some() => {
                *i += 1;
                let slot = cx.backref.then(|| {
                    cx.groups += 1;
                    cx.groups - 1
                });
                let inner = parse_alts(p, i, true, cx);
                items.push(match (inner, slot) {
                    // This group's own alternatives take its slot.
                    (Node::Alt(alts, None), s) => Node::Alt(alts, s),
                    (n, Some(s)) => Node::Alt(vec![n], Some(s)),
                    (n, None) => n,
                });
            }
            _ => {
                *i += 1;
                items.push(Node::Lit(c, cx.fold, cx.approx));
            }
        }
    }
    if items.len() == 1 {
        items.pop().unwrap()
    } else {
        Node::Seq(items)
    }
}

// ---- matching -------------------------------------------------------------

// Reachable positions, each with the fewest approximation errors used.
type States = BTreeMap<usize, u32>;

fn add(st: &mut States, p: usize, e: u32) {
    let v = st.entry(p).or_insert(e);
    if e < *v {
        *v = e;
    }
}

struct Matcher<'a> {
    s: &'a [char],
}

impl Matcher<'_> {
    fn one_char(&self, node: &Node, p: usize) -> Option<bool> {
        let c = self.s[p];
        match node {
            Node::Lit(x, f, _) => Some(fold_eq(*x, c, *f)),
            Node::Any(_) => Some(true),
            Node::Class {
                negated, set, fold, ..
            } => Some(class_fold(set, c, *fold, raw_byte_state(self.s, p)) != *negated),
            _ => None,
        }
    }

    fn approx_of(node: &Node) -> u32 {
        match node {
            Node::Lit(_, _, a) | Node::Any(a) => *a,
            Node::Class { approx, .. } => *approx,
            _ => 0,
        }
    }

    fn step(&self, node: &Node, from: &States) -> States {
        let s = self.s;
        let len = s.len();
        let mut out = States::new();
        match node {
            Node::Lit(..) | Node::Any(_) | Node::Class { .. } => {
                for (&p, &e) in from {
                    if p < len && self.one_char(node, p) == Some(true) {
                        add(&mut out, p + 1, e);
                    }
                }
            }
            Node::Star => {
                if let Some((&p, _)) = from.iter().next() {
                    // Every position from the first state on, with the
                    // fewest errors of any state at or before it.
                    let mut best = u32::MAX;
                    for q in p..=len {
                        if let Some(&e) = from.get(&q) {
                            best = best.min(e);
                        }
                        add(&mut out, q, best);
                    }
                }
            }
            Node::Range(lo, hi) => {
                for (&p, &e) in from {
                    let mut q = p;
                    while q < len && s[q].is_ascii_digit() {
                        q += 1;
                        let text: String = s[p..q].iter().collect();
                        let v = text.parse::<u128>().unwrap_or(u128::MAX);
                        if lo.is_none_or(|l| v >= l as u128) && hi.is_none_or(|h| v <= h as u128) {
                            add(&mut out, q, e);
                        }
                    }
                }
            }
            Node::Start => {
                if let Some(&e) = from.get(&0) {
                    add(&mut out, 0, e);
                }
            }
            Node::End => {
                if let Some(&e) = from.get(&len) {
                    add(&mut out, len, e);
                }
            }
            Node::Seq(items) => return self.seq(items, from),
            Node::Alt(alts, _) => {
                for a in alts {
                    for (q, e) in self.step(a, from) {
                        add(&mut out, q, e);
                    }
                }
            }
            Node::Repeat(inner, min, max) => return self.repeat(inner, *min, *max, from),
            Node::Not(inner) => {
                for (&p, &e) in from {
                    let hit = self.step(inner, &States::from([(p, 0)]));
                    for q in p..=len {
                        if !hit.contains_key(&q) {
                            add(&mut out, q, e);
                        }
                    }
                }
            }
            Node::Exclude(first, excl) => {
                for (&p, &e) in from {
                    let hits = self.step(first, &States::from([(p, e)]));
                    let bad: Vec<States> = excl
                        .iter()
                        .map(|x| self.step(x, &States::from([(p, 0)])))
                        .collect();
                    for (q, e2) in hits {
                        if !bad.iter().any(|b| b.contains_key(&q)) {
                            add(&mut out, q, e2);
                        }
                    }
                }
            }
        }
        out
    }

    // A sequence, with approximate matching for single characters under
    // (#aN): substitution, a missing or extra character, or two swapped.
    fn seq(&self, items: &[Node], from: &States) -> States {
        let s = self.s;
        let len = s.len();
        let mut cur = from.clone();
        let mut swapped: Option<States> = None;
        for (k, item) in items.iter().enumerate() {
            let budget = Self::approx_of(item);
            if budget > 0 {
                // Extra characters in the text before this one.
                let mut grown = cur.clone();
                let mut frontier = cur.clone();
                while !frontier.is_empty() {
                    let mut next = States::new();
                    for (&p, &e) in &frontier {
                        if p < len && e < budget && grown.get(&(p + 1)).is_none_or(|&g| g > e + 1) {
                            add(&mut next, p + 1, e + 1);
                        }
                    }
                    for (&q, &e) in &next {
                        add(&mut grown, q, e);
                    }
                    frontier = next;
                }
                cur = grown;
            }
            let mut next = self.step(item, &cur);
            if budget > 0 {
                for (&p, &e) in &cur {
                    if e < budget {
                        // Missing from the text.
                        add(&mut next, p, e + 1);
                        // A different character (not for a `[...]` set).
                        if p < len && matches!(item, Node::Lit(..)) {
                            add(&mut next, p + 1, e + 1);
                        }
                    }
                }
                // Swapped with the next single character.
                let next_swap =
                    if let Some(after) = items.get(k + 1).filter(|n| Self::approx_of(n) > 0) {
                        let mut t = States::new();
                        for (&p, &e) in &cur {
                            if e < budget
                                && p + 1 < len
                                && self.one_char(after, p) == Some(true)
                                && self.one_char(item, p + 1) == Some(true)
                            {
                                add(&mut t, p + 2, e + 1);
                            }
                        }
                        (!t.is_empty()).then_some(t)
                    } else {
                        None
                    };
                if let Some(t) = swapped.take() {
                    for (q, e) in t {
                        add(&mut next, q, e);
                    }
                }
                swapped = next_swap;
                cur = next;
                if cur.is_empty() && swapped.is_none() {
                    break;
                }
                continue;
            }
            if let Some(t) = swapped.take() {
                for (q, e) in t {
                    add(&mut next, q, e);
                }
            }
            cur = next;
            if cur.is_empty() {
                break;
            }
        }
        // Extra characters after the last approximate one.
        if let Some(budget) = items.last().map(Self::approx_of).filter(|&b| b > 0) {
            let mut extra = cur.clone();
            for (&p, &e) in &cur {
                for q in p + 1..=len {
                    let e2 = e + (q - p) as u32;
                    if e2 <= budget {
                        add(&mut extra, q, e2);
                    }
                }
            }
            cur = extra;
        }
        cur
    }

    fn step_rep(&self, inner: &Node, cur: &States) -> States {
        let mut next = self.step(inner, cur);
        let budget = Self::approx_of(inner);
        if budget > 0 {
            let len = self.s.len();
            for (&p, &e) in cur {
                if e < budget {
                    add(&mut next, p, e + 1);
                    if p < len && matches!(inner, Node::Lit(..)) {
                        add(&mut next, p + 1, e + 1);
                    }
                }
            }
        }
        next
    }

    fn repeat(&self, inner: &Node, min: usize, max: Option<usize>, from: &States) -> States {
        let mut out = States::new();
        let mut seen = States::new();
        if min == 0 {
            out = from.clone();
            seen = from.clone();
        }
        let mut cur = from.clone();
        let limit = self.s.len().saturating_add(min).saturating_add(1);
        for n in 1..=limit {
            if max.is_some_and(|m| n > m) {
                break;
            }
            let mut next = self.step_rep(inner, &cur);
            if n > min {
                // Past the minimum, only new progress matters.
                next.retain(|q, e| seen.get(q).is_none_or(|g| *g > *e));
            }
            if next.is_empty() {
                break;
            }
            if next == cur {
                // Fixed point: repeating further yields identical states.
                if max.is_none_or(|m| min <= m) {
                    for (&q, &e) in &next {
                        add(&mut out, q, e);
                    }
                }
                break;
            }
            for (&q, &e) in &next {
                add(&mut seen, q, e);
                if n >= min {
                    add(&mut out, q, e);
                }
            }
            cur = next;
        }
        out
    }

    fn reaches(&self, node: &Node, p: usize, e: usize) -> bool {
        self.step(node, &States::from([(p, 0)])).contains_key(&e)
    }

    // After a match, finds how `node` covers s[p..e], greedily, and
    // records (#b) group spans.
    fn capture(&self, node: &Node, p: usize, e: usize, caps: &mut Caps) -> bool {
        match node {
            Node::Seq(items) => self.capture_seq(items, p, e, caps),
            Node::Alt(alts, slot) => {
                for a in alts {
                    if self.reaches(a, p, e) {
                        self.capture(a, p, e, caps);
                        if let Some(n) = slot {
                            caps[*n] = Some((p, e));
                        }
                        return true;
                    }
                }
                false
            }
            Node::Repeat(inner, min, max) => {
                if p == e && *min == 0 {
                    return true;
                }
                if *max == Some(0) {
                    return false;
                }
                let mids: Vec<usize> = self
                    .step(inner, &States::from([(p, 0)]))
                    .keys()
                    .copied()
                    .collect();
                let rest = Node::Repeat(inner.clone(), min.saturating_sub(1), max.map(|m| m - 1));
                for &mid in mids.iter().rev() {
                    if mid > p && mid <= e && self.reaches(&rest, mid, e) {
                        self.capture(inner, p, mid, caps);
                        return self.capture(&rest, mid, e, caps);
                    }
                }
                p == e && *min == 0
            }
            _ => true,
        }
    }

    fn capture_seq(&self, items: &[Node], p: usize, e: usize, caps: &mut Caps) -> bool {
        let Some((first, rest)) = items.split_first() else {
            return p == e;
        };
        let mids: Vec<usize> = self
            .step(first, &States::from([(p, 0)]))
            .keys()
            .copied()
            .collect();
        let rest_node = Node::Seq(rest.to_vec());
        for &mid in mids.iter().rev() {
            if mid <= e && self.reaches(&rest_node, mid, e) {
                self.capture(first, p, mid, caps);
                return self.capture_seq(rest, mid, e, caps);
            }
        }
        false
    }
}

type Caps = Vec<Option<(usize, usize)>>;

pub struct Pattern {
    root: Node,
    // (#b) capturing groups; (#m) sets MATCH, MBEGIN and MEND.
    groups: usize,
    matchref: bool,
    bad: bool,
}

impl Pattern {
    /// Whether the pattern is malformed (zsh's "bad pattern").
    pub fn is_bad(&self) -> bool {
        self.bad
    }

    pub fn new(pattern: &str) -> Self {
        let norm = crate::shell::normalize_zstr(pattern);
        let p: Vec<char> = norm.chars().collect();
        let mut cx = Ctx {
            ext: crate::options::quick("extendedglob"),
            ksh: crate::options::quick("kshglob"),
            shglob: crate::options::quick("shglob"),
            fold: Fold::Exact,
            approx: 0,
            backref: false,
            groups: 0,
            matchref: false,
            bad: false,
        };
        let root = compile(&p, &mut cx);
        Pattern {
            root,
            groups: cx.groups,
            matchref: cx.matchref,
            bad: cx.bad,
        }
    }

    /// After a match of `s[b..e]`, sets zsh's `$match`, `$mbegin` and
    /// `$mend` for (#b) groups and `$MATCH`, `$MBEGIN`, `$MEND` for (#m).
    /// Positions count from 1 in the whole of `s`.
    pub fn record(&self, s: &[char], b: usize, e: usize) {
        if self.groups > 0 {
            let sub = &s[b..e];
            let m = Matcher { s: sub };
            let mut caps = vec![None; self.groups];
            m.capture(&self.root, 0, sub.len(), &mut caps);
            let (mut mt, mut mb, mut me) = (Vec::new(), Vec::new(), Vec::new());
            for c in caps {
                match c {
                    Some((x, y)) => {
                        mt.push(sub[x..y].iter().collect());
                        mb.push((b + x + 1).to_string());
                        me.push((b + y).to_string());
                    }
                    None => {
                        mt.push(String::new());
                        mb.push("-1".to_string());
                        me.push("-1".to_string());
                    }
                }
            }
            crate::shell::array_set("match", mt);
            crate::shell::array_set("mbegin", mb);
            crate::shell::array_set("mend", me);
        }
        if self.matchref {
            let text: String = s[b..e].iter().collect();
            crate::shell::var_set("MATCH", &text, false);
            crate::shell::var_set("MBEGIN", &(b + 1).to_string(), false);
            crate::shell::var_set("MEND", &e.to_string(), false);
        }
    }

    fn records(&self) -> bool {
        self.groups > 0 || self.matchref
    }

    pub fn matches_chars(&self, s: &[char]) -> bool {
        let m = Matcher { s };
        m.step(&self.root, &States::from([(0, 0)]))
            .contains_key(&s.len())
    }

    pub fn matches(&self, s: &str) -> bool {
        let norm = crate::shell::normalize_zstr(s);
        let chars: Vec<char> = norm.chars().collect();
        self.matches_chars(&chars)
    }
}

pub fn matches(pattern: &str, s: &str) -> bool {
    let p = Pattern::new(pattern);
    let norm = crate::shell::normalize_zstr(s);
    let chars: Vec<char> = norm.chars().collect();
    let hit = p.matches_chars(&chars);
    if hit && p.records() {
        p.record(&chars, 0, chars.len());
    }
    hit
}

/// True if the pattern contains an unescaped glob metacharacter: `*`,
/// `?`, `[`, a `(...)` group, or a `<n-m>` range.
pub fn has_meta(pattern: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let ksh = crate::options::quick("kshglob");
    let shglob = crate::options::quick("shglob");
    let ext = crate::options::quick("extendedglob");
    let mut i = 0;
    while i < p.len() {
        let ksh_op = match p[i] {
            '*' => Some("*("),
            '+' => Some("+("),
            '?' => Some("?("),
            '@' => Some("@("),
            '!' => Some("!("),
            _ => None,
        };
        if ksh
            && ksh_op.is_some_and(pat_on)
            && p.get(i + 1) == Some(&'(')
            && group_end(&p, i + 1).is_some()
        {
            return true;
        }
        match p[i] {
            '\\' => i += 1,
            '*' if pat_on("*") => return true,
            '?' if pat_on("?") => return true,
            '[' if pat_on("[") && parse_class(&p, i).is_some() => return true,
            '(' if !shglob && pat_on("(") && group_end(&p, i).is_some() => return true,
            '<' if !shglob && pat_on("<") && crate::lexer::numeric_range_len(&p, i).is_some() => {
                return true
            }
            '#' if ext && pat_on("#") => return true,
            '^' if ext && pat_on("^") => return true,
            // An exclusion needs text on both sides: a final `~` is literal.
            '~' if ext && pat_on("~") && i > 0 && i + 1 < p.len() => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Backslash-escapes pattern characters so `s` matches only itself.
pub fn escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "*?[]()|<>^#~\\!-".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Removes backslash escapes from a pattern, yielding its literal text.
pub fn unescape(pattern: &str) -> String {
    let mut out = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Flags for `Pattern::getmatch`, as zsh's SUB_* (Src/glob.c).
#[derive(Default, Clone, Copy, Debug)]
pub struct SubFlags {
    pub end: bool,     // match at the end (`%`)
    pub long: bool,    // longest match (`##`, `%%`, `/`)
    pub substr: bool,  // match substrings, not just at an end ((S), `/`)
    pub start: bool,   // anchored at the start (`/#`)
    pub global: bool,  // all matches (`//`)
    pub all: bool,     // the whole string must match (`:/`)
    pub matched: bool, // (M): the matched part
    pub rest: bool,    // (R): the rest (the default)
    pub bind: bool,    // (B): start index
    pub eind: bool,    // (E): end index
    pub len: bool,     // (N): match length
}

impl Pattern {
    // Where a match starting at `t` can end, matching against the whole
    // text so `(#s)` and `(#e)` mean its start and end.
    fn ends_at(&self, s: &[char], t: usize) -> States {
        Matcher { s }.step(&self.root, &States::from([(t, 0)]))
    }

    fn longest_at(&self, s: &[char], t: usize) -> Option<usize> {
        self.ends_at(s, t).keys().next_back().copied()
    }

    fn shortest_at(&self, s: &[char], t: usize) -> Option<usize> {
        self.ends_at(s, t).keys().next().copied()
    }

    fn matches_to_end(&self, s: &[char], t: usize) -> bool {
        self.ends_at(s, t).contains_key(&s.len())
    }

    // Builds the result for a match of s[b..e] (zsh's get_match_ret).
    fn match_ret(s: &[char], b: usize, e: usize, fl: &SubFlags, repl: Option<&str>) -> String {
        let mut out = String::new();
        if fl.matched {
            out.extend(&s[b..e]);
        }
        if fl.rest {
            if fl.matched {
                out.push(' ');
            }
            out.extend(&s[..b]);
            if let Some(r) = repl {
                out.push_str(r);
            }
            out.extend(&s[e..]);
        }
        let mut nums = Vec::new();
        if fl.bind {
            nums.push((b + 1).to_string());
        }
        if fl.eind {
            nums.push((e + 1).to_string());
        }
        if fl.len {
            nums.push((e - b).to_string());
        }
        if !nums.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&nums.join(" "));
        }
        out
    }

    /// zsh's igetmatch: finds the `n`th match (1-based) of the pattern in
    /// `value` as `fl` describes and returns the requested parts, with
    /// `repl` substituted for the match in the rest. No match gives the
    /// whole value (or the empty match's parts).
    /// `repl` is called for each replacement after the match parameters
    /// are set, so it may refer to `$MATCH` or `$match`.
    pub fn getmatch(
        &self,
        value: &str,
        mut fl: SubFlags,
        n: usize,
        repl: Option<&dyn Fn() -> String>,
    ) -> String {
        let s: Vec<char> = value.chars().collect();
        let len = s.len();
        let mut n = n.max(1) as isize;
        if !(fl.matched || fl.rest || fl.bind || fl.eind || fl.len) {
            fl.rest = true;
        }
        if fl.start && fl.end {
            fl.long = true;
        }
        let ret = |b, e| {
            if self.records() {
                self.record(&s, b, e);
            }
            let r = repl.map(|f| f());
            Self::match_ret(&s, b, e, &fl, r.as_deref())
        };
        if fl.all {
            return if self.matches_chars(&s) {
                ret(0, len)
            } else {
                Self::match_ret(&s, 0, 0, &fl, None)
            };
        }
        let empty_ok = || self.matches_chars(&[]);
        match (fl.end, fl.long, fl.substr) {
            (false, _, false) => {
                if let Some(mut e) = self.longest_at(&s, 0) {
                    if !fl.long {
                        e = self.shortest_at(&s, 0).unwrap_or(e);
                    }
                    return ret(0, e);
                }
            }
            (true, false, false) => {
                for t in (0..=len).rev() {
                    if self.matches_to_end(&s, t) {
                        return ret(t, len);
                    }
                }
            }
            (true, true, false) => {
                for t in 0..=len {
                    if self.matches_to_end(&s, t) {
                        return ret(t, len);
                    }
                    if fl.start {
                        break;
                    }
                }
            }
            (false, long, true) => {
                if !long && !fl.global && empty_ok() {
                    n -= 1;
                    if n == 0 {
                        return ret(0, 0);
                    }
                }
                let mut chunks: Vec<(usize, usize)> = Vec::new();
                let mut t = 0;
                while t <= len {
                    if let Some(mut mpos) = self.longest_at(&s, t) {
                        if !long {
                            mpos = self.shortest_at(&s, t).unwrap_or(mpos);
                        }
                        n -= 1;
                        if fl.global {
                            // zsh stops before the end: no empty match there.
                            if mpos == t && t == len && len > 0 {
                                break;
                            }
                            if mpos > t || chunks.last().is_none_or(|c| c.1 != t) {
                                chunks.push((t, mpos));
                            }
                            t = if mpos > t { mpos } else { t + 1 };
                            continue;
                        }
                        if n == 0 {
                            return ret(t, mpos);
                        }
                    }
                    t += 1;
                }
                if fl.global && !chunks.is_empty() {
                    let mut out = String::new();
                    let mut i = 0;
                    for (b, e) in chunks {
                        out.extend(&s[i..b]);
                        if self.records() {
                            self.record(&s, b, e);
                        }
                        out.push_str(&repl.map(|f| f()).unwrap_or_default());
                        i = e;
                    }
                    out.extend(&s[i..]);
                    return out;
                }
                if long && !fl.global && empty_ok() {
                    n -= 1;
                    if n == 0 {
                        return ret(0, 0);
                    }
                }
            }
            (true, long, true) => {
                if empty_ok() {
                    n -= 1;
                    if n == 0 {
                        return ret(len, len);
                    }
                }
                for t in (0..len).rev() {
                    if let Some(mut mpos) = self.longest_at(&s, t) {
                        n -= 1;
                        if n == 0 {
                            if !long {
                                mpos = self.shortest_at(&s, t).unwrap_or(mpos);
                            }
                            return ret(t, mpos);
                        }
                    }
                }
                if long && empty_ok() {
                    n -= 1;
                    if n == 0 {
                        return ret(len, len);
                    }
                }
            }
        }
        Self::match_ret(&s, 0, 0, &fl, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_matching() {
        let _g = crate::zsh_test_guard();
        assert!(matches("f*", "foo"));
        assert!(!matches("f*", "bar"));
        assert!(matches("?a?", "bar"));
        assert!(matches("[a-c]x", "bx"));
        assert!(matches("[!a-c]x", "dx"));
        assert!(matches("[[:digit:]]*", "1abc"));
        assert!(matches("\\*", "*"));
        assert!(!matches("\\*", "a"));
        assert!(matches("*.rs", "main.rs"));
        assert!(matches("é*", "école"));
    }

    #[test]
    fn groups_and_alternation() {
        let _g = crate::zsh_test_guard();
        assert!(matches("([1-4]*)", "4.0"));
        assert!(!matches("([1-4]*)", "5.9"));
        assert!(!matches("([1-4]*)", ""));
        assert!(matches("*.(rs|toml)", "Cargo.toml"));
        assert!(matches("(a|b)(c|d)", "bd"));
        assert!(!matches("(a|b)(c|d)", "ab"));
        assert!(matches("a(", "a("));
        assert!(matches("<->", "123"));
        assert!(!matches("<->", "12a"));
        assert!(matches("file<1-3>.txt", "file2.txt"));
        assert!(!matches("file<1-3>.txt", "file4.txt"));
        assert!(matches("\\(x\\)", "(x)"));
    }

    fn gm(v: &str, p: &str, fl: SubFlags, repl: Option<&str>) -> String {
        let f = repl.map(|r| move || r.to_string());
        Pattern::new(p).getmatch(v, fl, 1, f.as_ref().map(|f| f as &dyn Fn() -> String))
    }

    #[test]
    fn prefix_suffix() {
        let _g = crate::zsh_test_guard();
        let long = SubFlags {
            long: true,
            ..Default::default()
        };
        let end = SubFlags {
            end: true,
            ..Default::default()
        };
        let end_long = SubFlags {
            end: true,
            long: true,
            ..Default::default()
        };
        assert_eq!(gm("a/b/c", "*/", SubFlags::default(), None), "b/c");
        assert_eq!(gm("a/b/c", "*/", long, None), "c");
        assert_eq!(gm("a.tar.gz", ".*", end, None), "a.tar");
        assert_eq!(gm("a.tar.gz", ".*", end_long, None), "a");
        assert_eq!(gm("héllo", "l*", end_long, None), "hé");
    }

    #[test]
    fn substitution() {
        let _g = crate::zsh_test_guard();
        let sub = SubFlags {
            long: true,
            substr: true,
            ..Default::default()
        };
        let all = SubFlags {
            global: true,
            ..sub
        };
        let start = SubFlags {
            long: true,
            start: true,
            ..Default::default()
        };
        let end = SubFlags {
            long: true,
            end: true,
            ..Default::default()
        };
        assert_eq!(gm("a-b-c", "-", sub, Some("+")), "a+b-c");
        assert_eq!(gm("a-b-c", "-", all, Some("+")), "a+b+c");
        assert_eq!(gm("abc", "a", start, Some("X")), "Xbc");
        assert_eq!(gm("abc", "c", end, Some("X")), "abX");
        assert_eq!(gm("aXbXc", "X*", sub, Some("_")), "a_");
    }
}
