//! zsh/zutil's `zregexparse` (Src/Modules/zutil.c): parses words with a
//! regex-like language of `/pattern/` elements, `( | )` grouping and `#`
//! closure, running guards and actions as it goes. Used by
//! `_regex_arguments`.

use crate::pattern::Pattern;
use crate::shell::{array_get, array_set, builtin_err, var_set, with_shell};

struct RParseState {
    // None for `/[]/`, which never matches.
    pattern: Option<String>,
    // `/`, `+` or `-`: the character that ended the element.
    cutoff: char,
    guard: Option<String>,
    action: Option<String>,
    branches: Vec<RParseBranch>,
}

struct RParseBranch {
    // Index into the state arena.
    state: usize,
    actions: Vec<String>,
}

#[derive(Default)]
struct RParseResult {
    nullacts: Option<Vec<String>>,
    in_: Vec<RParseBranch>,
    out: Vec<RParseBranch>,
}

// Parse failure that zsh signals with longjmp.
struct RParseErr;

struct RParse<'a> {
    args: &'a [String],
    pos: usize,
    states: Vec<RParseState>,
}

impl<'a> RParse<'a> {
    fn cur(&self) -> Option<&'a str> {
        self.args.get(self.pos).map(|s| s.as_str())
    }
}

fn connectstates(states: &mut [RParseState], out: &[RParseBranch], in_: &[RParseBranch]) {
    for outbranch in out {
        for inbranch in in_ {
            let mut actions = outbranch.actions.clone();
            actions.extend(inbranch.actions.iter().cloned());
            states[outbranch.state].branches.push(RParseBranch {
                state: inbranch.state,
                actions,
            });
        }
    }
}

fn rparseelt(rp: &mut RParse, result: &mut RParseResult) -> Result<i32, RParseErr> {
    let Some(s) = rp.cur() else {
        return Ok(1);
    };
    let c: Vec<char> = s.chars().collect();
    match c.first() {
        Some(&'/') => {
            let l = c.len();
            if !((l >= 2 && c[l - 1] == '/')
                || (l >= 3 && c[l - 2] == '/' && (c[l - 1] == '+' || c[l - 1] == '-')))
            {
                return Ok(1);
            }
            let cutoff = c[l - 1];
            let pattern: String = if cutoff == '/' {
                c[1..l - 1].iter().collect()
            } else {
                c[1..l - 2].iter().collect()
            };
            rp.pos += 1;
            let lookahead = match rp.cur() {
                Some(s) if s.starts_with('%') && s.chars().count() >= 2 && s.ends_with('%') => {
                    rp.pos += 1;
                    Some(s[1..s.len() - 1].to_string())
                }
                _ => None,
            };
            let pattern = if pattern == "[]" {
                None
            } else {
                // Group 1 is what the element consumes; the lookahead
                // and the rest of the word only have to match.
                let mut cp = format!("(#b)((#B){})", pattern);
                if let Some(la) = &lookahead {
                    cp.push_str("(#B)");
                    cp.push_str(la);
                }
                cp.push('*');
                Some(cp)
            };
            let guard = match rp.cur() {
                Some(s) if s.starts_with('-') => {
                    rp.pos += 1;
                    Some(s[1..].to_string())
                }
                _ => None,
            };
            let action = match rp.cur() {
                Some(s) if s.starts_with(':') => {
                    rp.pos += 1;
                    Some(s[1..].to_string())
                }
                _ => None,
            };
            let st = rp.states.len();
            rp.states.push(RParseState {
                pattern,
                cutoff,
                guard,
                action,
                branches: Vec::new(),
            });
            result.nullacts = None;
            result.in_ = vec![RParseBranch {
                state: st,
                actions: Vec::new(),
            }];
            result.out = vec![RParseBranch {
                state: st,
                actions: Vec::new(),
            }];
        }
        Some(&'(') => {
            if c.len() > 1 {
                return Ok(1);
            }
            rp.pos += 1;
            if rparsealt(rp, result)? != 0 {
                return Err(RParseErr);
            }
            if rp.cur() != Some(")") {
                return Err(RParseErr);
            }
            rp.pos += 1;
        }
        _ => return Ok(1),
    }
    Ok(0)
}

fn rparseclo(rp: &mut RParse, result: &mut RParseResult) -> Result<i32, RParseErr> {
    if rparseelt(rp, result)? != 0 {
        return Ok(1);
    }
    if rp.cur() == Some("#") {
        rp.pos += 1;
        while rp.cur() == Some("#") {
            rp.pos += 1;
        }
        connectstates(&mut rp.states, &result.out, &result.in_);
        result.nullacts = Some(Vec::new());
    }
    Ok(0)
}

fn prependactions(acts: &[String], branches: &mut [RParseBranch]) {
    for br in branches {
        br.actions.splice(0..0, acts.iter().cloned());
    }
}

fn appendactions(acts: &[String], branches: &mut [RParseBranch]) {
    for br in branches {
        br.actions.extend(acts.iter().cloned());
    }
}

fn rparseseq(rp: &mut RParse, result: &mut RParseResult) -> Result<i32, RParseErr> {
    result.nullacts = Some(Vec::new());
    result.in_ = Vec::new();
    result.out = Vec::new();

    loop {
        let brace = rp
            .cur()
            .filter(|s| s.starts_with('{') && s.ends_with('}') && s.len() >= 2);
        if let Some(s) = brace {
            rp.pos += 1;
            let action = s[1..s.len() - 1].to_string();
            if let Some(n) = &mut result.nullacts {
                n.push(action.clone());
            }
            for br in &mut result.out {
                br.actions.push(action.clone());
            }
            continue;
        }
        let mut sub = RParseResult::default();
        if rparseclo(rp, &mut sub)? != 0 {
            break;
        }
        let RParseResult {
            nullacts: sub_nullacts,
            in_: mut sub_in,
            out: mut sub_out,
        } = sub;
        connectstates(&mut rp.states, &result.out, &sub_in);

        if let Some(n) = &result.nullacts {
            prependactions(n, &mut sub_in);
            result.in_.append(&mut sub_in);
        }
        match &sub_nullacts {
            Some(sn) => {
                appendactions(sn, &mut result.out);
                result.out.append(&mut sub_out);
            }
            None => result.out = sub_out,
        }

        result.nullacts = match (result.nullacts.take(), sub_nullacts) {
            (Some(mut n), Some(sn)) => {
                n.extend(sn);
                Some(n)
            }
            _ => None,
        };
    }
    Ok(0)
}

fn rparsealt(rp: &mut RParse, result: &mut RParseResult) -> Result<i32, RParseErr> {
    if rparseseq(rp, result)? != 0 {
        return Ok(1);
    }
    while rp.cur() == Some("|") {
        rp.pos += 1;
        let mut sub = RParseResult::default();
        if rparseseq(rp, &mut sub)? != 0 {
            return Err(RParseErr);
        }
        if result.nullacts.is_none() && sub.nullacts.is_some() {
            result.nullacts = sub.nullacts;
        }
        result.in_.append(&mut sub.in_);
        result.out.append(&mut sub.out);
    }
    Ok(0)
}

struct MatchData {
    match_: Option<Vec<String>>,
    mbegin: Option<Vec<String>>,
    mend: Option<Vec<String>>,
}

fn savematch() -> MatchData {
    MatchData {
        match_: array_get("match"),
        mbegin: array_get("mbegin"),
        mend: array_get("mend"),
    }
}

fn restorematch(m: &MatchData) {
    for (name, v) in [
        ("match", &m.match_),
        ("mbegin", &m.mbegin),
        ("mend", &m.mend),
    ] {
        match v {
            Some(a) => {
                array_set(name, a.clone());
            }
            None => unsetparam(name),
        }
    }
}

// As `unset name`: a local keeps its slot and only loses its value.
fn unsetparam(name: &str) {
    with_shell(|s| {
        let local = s
            .param(name)
            .is_some_and(|p| p.level > 0 && p.tie.is_none());
        if local {
            if let Some(p) = s.param_mut(name) {
                p.value = crate::params::Value::Scalar(String::new());
                p.attrs = crate::params::UNSET;
            }
        } else {
            s.remove_param(name);
        }
    });
}

fn setiparam(name: &str, val: usize) {
    var_set(name, &val.to_string(), false);
}

fn execstring(code: &str) -> i32 {
    crate::exec::run_string(code)
}

// Which branch list the matcher is looking at: the start states or a
// state's successors.
#[derive(Clone, Copy)]
enum Nexts {
    In,
    State(usize),
}

fn branches_of<'a>(
    sm: &'a RParseResult,
    states: &'a [RParseState],
    n: Nexts,
) -> &'a [RParseBranch] {
    match n {
        Nexts::In => &sm.in_,
        Nexts::State(i) => &states[i].branches,
    }
}

fn rmatch(
    sm: &RParseResult,
    states: &[RParseState],
    subj: &str,
    var1: &str,
    var2: &str,
    comp: bool,
) -> i32 {
    let subj: Vec<char> = subj.chars().collect();
    let mut at = 0usize;
    let mut st: Option<usize> = None;
    let (mut point1, mut point2) = (0usize, 0usize);
    // Compiled lazily, as zsh does, so only patterns reached are built.
    let mut patprogs: Vec<Option<Pattern>> = states.iter().map(|_| None).collect();

    setiparam(var1, point1);
    setiparam(var2, point2);

    if !comp && subj.is_empty() {
        if let Some(n) = &sm.nullacts {
            for action in n {
                execstring(action);
            }
            return 0;
        }
    }

    let mut nexts = Nexts::In;
    let mut nextslist = vec![nexts];
    loop {
        let match1 = savematch();
        let mut found = false;

        for br in branches_of(sm, states, nexts) {
            let next = &states[br.state];
            let Some(pat) = &next.pattern else {
                continue;
            };
            let prog = patprogs[br.state].get_or_insert_with(|| Pattern::new(pat));
            let rest = &subj[at..];
            if !prog.matches_chars(rest) {
                continue;
            }
            prog.record(rest, 0, rest.len());
            if let Some(guard) = &next.guard {
                if execstring(guard) != 0 {
                    continue;
                }
            }
            let len = array_get("mend")
                .and_then(|m| m.first().and_then(|v| v.parse::<usize>().ok()))
                .unwrap_or(0);
            at = (at + len).min(subj.len());

            let match2 = savematch();
            restorematch(&match1);
            for action in &br.actions {
                execstring(action);
            }
            restorematch(&match2);

            point2 += len;
            setiparam(var2, point2);
            st = Some(br.state);
            nexts = Nexts::State(br.state);
            if next.cutoff == '-' || (next.cutoff == '/' && len != 0) {
                nextslist.clear();
                point1 = point2;
                setiparam(var1, point1);
            }
            nextslist.push(nexts);
            found = true;
            break;
        }
        if !found {
            break;
        }
    }

    if !comp && at == subj.len() {
        if let Some(br) = sm.out.iter().find(|br| Some(br.state) == st) {
            for action in &br.actions {
                execstring(action);
            }
            return 0;
        }
    }

    for &n in &nextslist {
        for br in branches_of(sm, states, n) {
            if let Some(action) = &states[br.state].action {
                execstring(action);
            }
        }
    }
    if branches_of(sm, states, nexts).is_empty() {
        2
    } else {
        1
    }
}

/*
  usage: zregexparse [-c] var1 var2 string regex...
  status:
    0: matched
    1: unmatched (all next state candidates are failed)
    2: unmatched (there is no next state candidates)
    3: regex parse error
*/
fn bin_zregexparse(nam: &str, args: &[String], comp: bool) -> i32 {
    let oldextendedglob = crate::options::is_set("extendedglob");
    let var1 = &args[0];
    let var2 = &args[1];
    let subj = &args[2];

    crate::options::set("extendedglob", true);

    let mut rp = RParse {
        args: &args[3..],
        pos: 0,
        states: Vec::new(),
    };
    let mut result = RParseResult::default();
    let ok = matches!(rparsealt(&mut rp, &mut result), Ok(0)) && rp.cur().is_none();
    let ret = if ok {
        rmatch(&result, &rp.states, subj, var1, var2, comp)
    } else {
        match rp.cur() {
            Some(a) => builtin_err(nam, &format!("invalid regex : {}", a)),
            None => builtin_err(nam, "not enough regex arguments"),
        }
        3
    };

    crate::options::set("extendedglob", oldextendedglob);
    ret
}

/// `zregexparse [-c] var1 var2 string regex...`, with zsh's generic
/// builtin option handling for the `c` option.
pub fn builtin_zregexparse(words: &[String]) -> i32 {
    let nam = "zregexparse";
    let mut comp = false;
    let mut i = 1;
    while let Some(w) = words.get(i) {
        let b = w.as_bytes();
        if b.first() != Some(&b'-') || b.get(1).is_some_and(|c| c.is_ascii_digit()) {
            break;
        }
        // `--` and `-` end the options.
        let opts = if b.get(1) == Some(&b'-') {
            &w[2..]
        } else {
            &w[1..]
        };
        i += 1;
        if opts.is_empty() {
            break;
        }
        for c in opts.chars() {
            if c != 'c' {
                builtin_err(nam, &format!("bad option: -{}", c));
                return 1;
            }
            comp = true;
        }
    }
    let args = &words[i..];
    if args.len() < 3 {
        builtin_err(nam, "not enough arguments");
        return 1;
    }
    bin_zregexparse(nam, args, comp)
}
