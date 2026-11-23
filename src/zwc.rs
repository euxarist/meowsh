//! `.zwc` wordcode dump support (`zcompile` and `source` lookup).
//!
//! Uses zsh's `FDHead` layout (`Src/parse.c`):
//! - 12 words (`FD_PRELEN * 4 = 48` bytes): magic, flags, 40-byte version string.
//! - One or more `FDHead` records (`start, len, npats, strs, hlen, flags`, 24 bytes)
//!   followed by the NUL-terminated 4-byte-aligned entry name, terminated by a 0 word.
//!   Note `FDHead.npats` sits at byte offset 56 (`48 + 8`).
//! - Each entry payload holds compiled `Node` wordcode (`MWC1` binary encoding),
//!   so `source` / `.` executes pre-parsed AST nodes without re-parsing source.

use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use crate::exec::strerror;
use crate::parser::{ParseError, Parser};
use crate::shell::{builtin_err, bytes_to_zstr, shell_err, zstr_to_bytes};
use crate::types::{
    AndOrOp, Assign, AssignValue, CaseItem, HereDoc, ListItem, Node, Redir, RedirOp, SimpleCmd,
};

const FD_PRELEN: usize = 12;
const FD_MAGIC: u32 = 0x0405_0607;
const FD_OMAGIC: u32 = 0x0706_0504;
const VERSION_BYTES: &[u8] = b"meowsh-wc1";
const PAYLOAD_MAGIC: &[u8; 4] = b"MWC1";

// ---- compact binary wordcode encoder / decoder ------------------------------

struct Enc(Vec<u8>);

impl Enc {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn usize(&mut self, v: usize) {
        self.u32(v as u32);
    }
    fn str(&mut self, s: &str) {
        let b = zstr_to_bytes(s);
        self.usize(b.len());
        self.0.extend_from_slice(&b);
    }
    fn opt_str(&mut self, o: &Option<String>) {
        match o {
            Some(s) => {
                self.u8(1);
                self.str(s);
            }
            None => self.u8(0),
        }
    }
    fn str_vec(&mut self, v: &[String]) {
        self.usize(v.len());
        for s in v {
            self.str(s);
        }
    }
    fn opt_str_vec(&mut self, o: &Option<Vec<String>>) {
        match o {
            Some(v) => {
                self.u8(1);
                self.str_vec(v);
            }
            None => self.u8(0),
        }
    }
    fn redir(&mut self, r: &Redir) {
        self.i32(r.fd);
        self.u8(match r.op {
            RedirOp::Read => 0,
            RedirOp::Write => 1,
            RedirOp::Clobber => 2,
            RedirOp::Append => 3,
            RedirOp::AppendClobber => 4,
            RedirOp::ReadWrite => 5,
            RedirOp::DupIn => 6,
            RedirOp::DupOut => 7,
            RedirOp::HereDoc => 8,
            RedirOp::HereStr => 9,
        });
        self.str(&r.target);
        match &r.heredoc {
            Some(h) => {
                self.u8(1);
                self.usize(h.id);
                self.str(&h.body);
                self.bool(h.expand);
            }
            None => self.u8(0),
        }
        self.opt_str(&r.var);
    }
    fn redirs(&mut self, rs: &[Redir]) {
        self.usize(rs.len());
        for r in rs {
            self.redir(r);
        }
    }
    fn assign(&mut self, a: &Assign) {
        self.str(&a.name);
        self.opt_str(&a.subscript);
        self.bool(a.append);
        match &a.value {
            AssignValue::Scalar(s) => {
                self.u8(0);
                self.str(s);
            }
            AssignValue::Array(v) => {
                self.u8(1);
                self.str_vec(v);
            }
        }
    }
    fn node(&mut self, n: &Node) {
        match n {
            Node::Simple(c) => {
                self.u8(0);
                self.usize(c.line);
                self.usize(c.assigns.len());
                for a in &c.assigns {
                    self.assign(a);
                }
                self.str_vec(&c.words);
                self.redirs(&c.redirs);
                self.usize(c.decl_arrays.len());
                for (idx, items) in &c.decl_arrays {
                    self.usize(*idx);
                    self.str_vec(items);
                }
            }
            Node::Pipeline { cmds, bang } => {
                self.u8(1);
                self.bool(*bang);
                self.usize(cmds.len());
                for c in cmds {
                    self.node(c);
                }
            }
            Node::AndOr { first, rest } => {
                self.u8(2);
                self.node(first);
                self.usize(rest.len());
                for (op, node) in rest {
                    self.u8(match op {
                        AndOrOp::And => 0,
                        AndOrOp::Or => 1,
                    });
                    self.node(node);
                }
            }
            Node::List(items) => {
                self.u8(3);
                self.usize(items.len());
                for item in items {
                    self.node(&item.node);
                    self.bool(item.background);
                }
            }
            Node::Subshell(b) => {
                self.u8(4);
                self.node(b);
            }
            Node::Brace(b) => {
                self.u8(5);
                self.node(b);
            }
            Node::If {
                branches,
                else_body,
            } => {
                self.u8(6);
                self.usize(branches.len());
                for (cond, body) in branches {
                    self.node(cond);
                    self.node(body);
                }
                match else_body {
                    Some(eb) => {
                        self.u8(1);
                        self.node(eb);
                    }
                    None => self.u8(0),
                }
            }
            Node::Loop { cond, body, until } => {
                self.u8(7);
                self.node(cond);
                self.node(body);
                self.bool(*until);
            }
            Node::Arith(expr, line) => {
                self.u8(8);
                self.str(expr);
                self.usize(*line);
            }
            Node::Repeat { count, body } => {
                self.u8(9);
                self.str(count);
                self.node(body);
            }
            Node::Select { var, words, body } => {
                self.u8(10);
                self.str(var);
                self.opt_str_vec(words);
                self.node(body);
            }
            Node::Coproc(b) => {
                self.u8(11);
                self.node(b);
            }
            Node::Time(b) => {
                self.u8(12);
                self.node(b);
            }
            Node::Disowned(b) => {
                self.u8(13);
                self.node(b);
            }
            Node::Always { body, always } => {
                self.u8(14);
                self.node(body);
                self.node(always);
            }
            Node::AnonFunc {
                body,
                args,
                line,
                traced,
            } => {
                self.u8(15);
                self.node(body);
                self.str_vec(args);
                self.usize(*line);
                self.bool(*traced);
            }
            Node::ForArith {
                init,
                cond,
                step,
                body,
            } => {
                self.u8(16);
                self.str(init);
                self.str(cond);
                self.str(step);
                self.node(body);
            }
            Node::For { vars, words, body } => {
                self.u8(17);
                self.str_vec(vars);
                self.opt_str_vec(words);
                self.node(body);
            }
            Node::Case { word, items } => {
                self.u8(18);
                self.str(word);
                self.usize(items.len());
                for item in items {
                    self.str_vec(&item.patterns);
                    match &item.body {
                        Some(b) => {
                            self.u8(1);
                            self.node(b);
                        }
                        None => self.u8(0),
                    }
                    self.bool(item.fallthrough);
                    self.bool(item.cont);
                }
            }
            Node::FuncDef {
                name,
                body,
                line,
                traced,
            } => {
                self.u8(19);
                self.str(name);
                self.node(body);
                self.usize(*line);
                self.bool(*traced);
            }
            Node::DBracket(args, line) => {
                self.u8(20);
                self.str_vec(args);
                self.usize(*line);
            }
            Node::Redirected { node, redirs } => {
                self.u8(21);
                self.node(node);
                self.redirs(redirs);
            }
            Node::Empty => self.u8(22),
        }
    }
}

struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
    depth: usize,
}

const MAX_ZWC_DEPTH: usize = 256;

impl<'a> Dec<'a> {
    fn u8(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    fn bool(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    fn u32(&mut self) -> Option<u32> {
        let s: [u8; 4] = self.buf.get(self.pos..self.pos + 4)?.try_into().ok()?;
        self.pos += 4;
        Some(u32::from_le_bytes(s))
    }
    fn i32(&mut self) -> Option<i32> {
        let s: [u8; 4] = self.buf.get(self.pos..self.pos + 4)?.try_into().ok()?;
        self.pos += 4;
        Some(i32::from_le_bytes(s))
    }
    fn usize(&mut self) -> Option<usize> {
        Some(self.u32()? as usize)
    }
    fn str(&mut self) -> Option<String> {
        let len = self.usize()?;
        let end = self.pos.checked_add(len)?;
        let slice = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(bytes_to_zstr(slice))
    }
    fn opt_str(&mut self) -> Option<Option<String>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.str()?)),
            _ => None,
        }
    }
    fn str_vec(&mut self) -> Option<Vec<String>> {
        let len = self.usize()?;
        let mut out = Vec::with_capacity(len.min(1024));
        for _ in 0..len {
            out.push(self.str()?);
        }
        Some(out)
    }
    fn opt_str_vec(&mut self) -> Option<Option<Vec<String>>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.str_vec()?)),
            _ => None,
        }
    }
    fn redir(&mut self) -> Option<Redir> {
        let fd = self.i32()?;
        let op = match self.u8()? {
            0 => RedirOp::Read,
            1 => RedirOp::Write,
            2 => RedirOp::Clobber,
            3 => RedirOp::Append,
            4 => RedirOp::AppendClobber,
            5 => RedirOp::ReadWrite,
            6 => RedirOp::DupIn,
            7 => RedirOp::DupOut,
            8 => RedirOp::HereDoc,
            9 => RedirOp::HereStr,
            _ => return None,
        };
        let target = self.str()?;
        let heredoc = match self.u8()? {
            0 => None,
            1 => Some(HereDoc {
                id: self.usize()?,
                body: self.str()?,
                expand: self.bool()?,
            }),
            _ => return None,
        };
        let var = self.opt_str()?;
        Some(Redir {
            fd,
            op,
            target,
            heredoc,
            var,
        })
    }
    fn redirs(&mut self) -> Option<Vec<Redir>> {
        let len = self.usize()?;
        let mut out = Vec::with_capacity(len.min(64));
        for _ in 0..len {
            out.push(self.redir()?);
        }
        Some(out)
    }
    fn assign(&mut self) -> Option<Assign> {
        let name = self.str()?;
        let subscript = self.opt_str()?;
        let append = self.bool()?;
        let value = match self.u8()? {
            0 => AssignValue::Scalar(self.str()?),
            1 => AssignValue::Array(self.str_vec()?),
            _ => return None,
        };
        Some(Assign {
            name,
            subscript,
            append,
            value,
        })
    }
    fn node(&mut self) -> Option<Node> {
        if self.depth >= MAX_ZWC_DEPTH {
            return None;
        }
        self.depth += 1;
        let res = self.node_inner();
        self.depth -= 1;
        res
    }
    fn node_inner(&mut self) -> Option<Node> {
        match self.u8()? {
            0 => {
                let line = self.usize()?;
                let alen = self.usize()?;
                let mut assigns = Vec::with_capacity(alen.min(64));
                for _ in 0..alen {
                    assigns.push(self.assign()?);
                }
                let words = self.str_vec()?;
                let redirs = self.redirs()?;
                let dlen = self.usize()?;
                let mut decl_arrays = Vec::with_capacity(dlen.min(64));
                for _ in 0..dlen {
                    decl_arrays.push((self.usize()?, self.str_vec()?));
                }
                Some(Node::Simple(SimpleCmd {
                    line,
                    assigns,
                    words,
                    redirs,
                    decl_arrays,
                }))
            }
            1 => {
                let bang = self.bool()?;
                let len = self.usize()?;
                let mut cmds = Vec::with_capacity(len.min(64));
                for _ in 0..len {
                    cmds.push(self.node()?);
                }
                Some(Node::Pipeline { cmds, bang })
            }
            2 => {
                let first = Box::new(self.node()?);
                let len = self.usize()?;
                let mut rest = Vec::with_capacity(len.min(64));
                for _ in 0..len {
                    let op = match self.u8()? {
                        0 => AndOrOp::And,
                        1 => AndOrOp::Or,
                        _ => return None,
                    };
                    rest.push((op, self.node()?));
                }
                Some(Node::AndOr { first, rest })
            }
            3 => {
                let len = self.usize()?;
                let mut items = Vec::with_capacity(len.min(256));
                for _ in 0..len {
                    items.push(ListItem {
                        node: self.node()?,
                        background: self.bool()?,
                    });
                }
                Some(Node::List(items))
            }
            4 => Some(Node::Subshell(Box::new(self.node()?))),
            5 => Some(Node::Brace(Box::new(self.node()?))),
            6 => {
                let len = self.usize()?;
                let mut branches = Vec::with_capacity(len.min(64));
                for _ in 0..len {
                    branches.push((self.node()?, self.node()?));
                }
                let else_body = match self.u8()? {
                    0 => None,
                    1 => Some(Box::new(self.node()?)),
                    _ => return None,
                };
                Some(Node::If {
                    branches,
                    else_body,
                })
            }
            7 => Some(Node::Loop {
                cond: Box::new(self.node()?),
                body: Box::new(self.node()?),
                until: self.bool()?,
            }),
            8 => Some(Node::Arith(self.str()?, self.usize()?)),
            9 => Some(Node::Repeat {
                count: self.str()?,
                body: Box::new(self.node()?),
            }),
            10 => Some(Node::Select {
                var: self.str()?,
                words: self.opt_str_vec()?,
                body: Box::new(self.node()?),
            }),
            11 => Some(Node::Coproc(Box::new(self.node()?))),
            12 => Some(Node::Time(Box::new(self.node()?))),
            13 => Some(Node::Disowned(Box::new(self.node()?))),
            14 => Some(Node::Always {
                body: Box::new(self.node()?),
                always: Box::new(self.node()?),
            }),
            15 => Some(Node::AnonFunc {
                body: Arc::new(self.node()?),
                args: self.str_vec()?,
                line: self.usize()?,
                traced: self.bool()?,
            }),
            16 => Some(Node::ForArith {
                init: self.str()?,
                cond: self.str()?,
                step: self.str()?,
                body: Box::new(self.node()?),
            }),
            17 => Some(Node::For {
                vars: self.str_vec()?,
                words: self.opt_str_vec()?,
                body: Box::new(self.node()?),
            }),
            18 => {
                let word = self.str()?;
                let len = self.usize()?;
                let mut items = Vec::with_capacity(len.min(64));
                for _ in 0..len {
                    let patterns = self.str_vec()?;
                    let body = match self.u8()? {
                        0 => None,
                        1 => Some(self.node()?),
                        _ => return None,
                    };
                    let fallthrough = self.bool()?;
                    let cont = self.bool()?;
                    items.push(CaseItem {
                        patterns,
                        body,
                        fallthrough,
                        cont,
                    });
                }
                Some(Node::Case { word, items })
            }
            19 => Some(Node::FuncDef {
                name: self.str()?,
                body: Box::new(self.node()?),
                line: self.usize()?,
                traced: self.bool()?,
            }),
            20 => Some(Node::DBracket(self.str_vec()?, self.usize()?)),
            21 => Some(Node::Redirected {
                node: Box::new(self.node()?),
                redirs: self.redirs()?,
            }),
            22 => Some(Node::Empty),
            _ => None,
        }
    }
}

fn encode_program(nodes: &[Node]) -> Vec<u8> {
    let mut enc = Enc(Vec::new());
    enc.0.extend_from_slice(PAYLOAD_MAGIC);
    enc.usize(nodes.len());
    for n in nodes {
        enc.node(n);
    }
    enc.0
}

fn decode_program(bytes: &[u8]) -> Option<Vec<Node>> {
    if bytes.get(0..4) != Some(PAYLOAD_MAGIC.as_slice()) {
        return None;
    }
    let mut dec = Dec {
        buf: bytes,
        pos: 4,
        depth: 0,
    };
    let len = dec.usize()?;
    let mut nodes = Vec::with_capacity(len.min(256));
    for _ in 0..len {
        nodes.push(dec.node()?);
    }
    if dec.pos != bytes.len() {
        return None;
    }
    Some(nodes)
}

// ---- FDHead container -------------------------------------------------------

fn read_u32(data: &[u8], off: usize, swap: bool) -> Option<u32> {
    let bytes: [u8; 4] = data.get(off..off + 4)?.try_into().ok()?;
    let v = u32::from_ne_bytes(bytes);
    Some(if swap { v.swap_bytes() } else { v })
}

/// Checks for a compiled `<path>.zwc` newer than (or as new as) `<path>` and
/// returns its decoded compiled AST nodes, emitting zsh's diagnostic if malformed.
pub fn try_load_compiled(source_path: &str) -> Option<Vec<Node>> {
    let zwc_path = format!("{}.zwc", source_path);
    let zwc_meta = std::fs::metadata(&zwc_path).ok()?;
    if !zwc_meta.is_file() {
        return None;
    }
    if let Ok(src_meta) = std::fs::metadata(source_path) {
        let zwc_mt = zwc_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let src_mt = src_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if zwc_mt < src_mt {
            return None;
        }
    }
    let name = Path::new(source_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(source_path);
    let payload = load_entry(&zwc_path, name)?;
    match decode_program(&payload) {
        Some(nodes) => Some(nodes),
        None => {
            shell_err(&format!("{}: invalid description: {}", zwc_path, name));
            None
        }
    }
}

fn load_entry(zwc_path: &str, name: &str) -> Option<Vec<u8>> {
    let data = std::fs::read(zwc_path).ok()?;
    if data.len() < FD_PRELEN * 4 + 4 {
        return None;
    }
    let magic = u32::from_ne_bytes(data[0..4].try_into().ok()?);
    let swap = match magic {
        FD_MAGIC => false,
        FD_OMAGIC => true,
        _ => return None,
    };
    let ver = &data[8..FD_PRELEN * 4];
    let ver_end = ver.iter().position(|&b| b == 0).unwrap_or(ver.len());
    if &ver[..ver_end] != VERSION_BYTES {
        return None;
    }

    let mut pos = FD_PRELEN * 4;
    while pos + 4 <= data.len() {
        let hlen_peek = read_u32(&data, pos + 16, swap).unwrap_or(0);
        if pos + 24 > data.len() {
            break;
        }
        let start = read_u32(&data, pos, swap)?;
        let len = read_u32(&data, pos + 4, swap)?;
        let npats = read_u32(&data, pos + 8, swap)?;
        let _strs = read_u32(&data, pos + 12, swap)?;
        let _flags = read_u32(&data, pos + 20, swap)?;
        if hlen_peek == 0 {
            break;
        }
        let step = (hlen_peek as usize).checked_mul(4)?;
        if step < 24 || pos + step > data.len() {
            break;
        }
        let name_area = &data[pos + 24..pos + step];
        let nul = name_area
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_area.len());
        let entry_name = bytes_to_zstr(&name_area[..nul]);
        if entry_name == name {
            if npats > len / 4 {
                shell_err(&format!("{}: invalid description: {}", zwc_path, name));
                return None;
            }
            let start_byte = (start as usize).checked_mul(4)?;
            let end_byte = start_byte.checked_add(len as usize)?;
            if end_byte > data.len() {
                shell_err(&format!("{}: invalid description: {}", zwc_path, name));
                return None;
            }
            return Some(data[start_byte..end_byte].to_vec());
        }
        pos += step;
    }
    None
}

fn build_zwc(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut header_bytes = FD_PRELEN * 4;
    let mut aligned_names = Vec::with_capacity(entries.len());
    for (name, _) in entries {
        let raw_len = name.len() + 1;
        let padded = (raw_len + 3) & !3;
        aligned_names.push(padded);
        header_bytes += 24 + padded;
    }
    header_bytes += 4;

    let mut out = Vec::new();
    out.extend_from_slice(&FD_MAGIC.to_ne_bytes());
    out.extend_from_slice(&0u32.to_ne_bytes());
    let mut ver = [0u8; (FD_PRELEN - 2) * 4];
    ver[..VERSION_BYTES.len()].copy_from_slice(VERSION_BYTES);
    out.extend_from_slice(&ver);

    let mut cur_offset = header_bytes;
    for ((name, body), &padded_name) in entries.iter().zip(&aligned_names) {
        let start_words = (cur_offset / 4) as u32;
        let len_bytes = body.len() as u32;
        let hlen_words = ((24 + padded_name) / 4) as u32;
        out.extend_from_slice(&start_words.to_ne_bytes());
        out.extend_from_slice(&len_bytes.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes()); // npats at byte offset 56
        out.extend_from_slice(&len_bytes.to_ne_bytes());
        out.extend_from_slice(&hlen_words.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes());
        out.extend_from_slice(name.as_bytes());
        out.resize(out.len() + (padded_name - name.len()), 0);
        let aligned_body = (body.len() + 3) & !3;
        cur_offset += aligned_body;
    }
    out.extend_from_slice(&0u32.to_ne_bytes());

    for (_, body) in entries {
        out.extend_from_slice(body);
        let rem = body.len() % 4;
        if rem != 0 {
            out.extend(std::iter::repeat_n(0u8, 4 - rem));
        }
    }
    out
}

/// Implements `zcompile [-U] [-z|-k] [-R|-M] [-c] [-t] file [name...]`.
pub fn builtin_zcompile(words: &[String]) -> i32 {
    let mut test_only = false;
    let mut i = 1;
    while let Some(a) = words.get(i) {
        if a == "--" {
            i += 1;
            break;
        }
        let Some(opts) = a.strip_prefix('-').filter(|o| !o.is_empty()) else {
            break;
        };
        for c in opts.chars() {
            match c {
                'U' | 'z' | 'k' | 'R' | 'M' | 'c' | 'a' | 'm' => {}
                't' => test_only = true,
                other => {
                    builtin_err("zcompile", &format!("bad option: -{}", other));
                    return 1;
                }
            }
        }
        i += 1;
    }
    let files = &words[i..];
    if files.is_empty() {
        builtin_err("zcompile", "too few arguments");
        return 1;
    }
    if test_only {
        let data = match std::fs::read(&files[0]) {
            Ok(d) => d,
            Err(_) => return 1,
        };
        if data.len() < FD_PRELEN * 4 {
            return 1;
        }
        let magic = u32::from_ne_bytes(data[0..4].try_into().unwrap_or([0; 4]));
        return if magic == FD_MAGIC || magic == FD_OMAGIC {
            0
        } else {
            1
        };
    }

    let target = &files[0];
    let src_files: Vec<String> = if files.len() > 1 {
        files[1..].to_vec()
    } else {
        vec![target.clone()]
    };
    let out_path = if target.ends_with(".zwc") {
        target.clone()
    } else {
        format!("{}.zwc", target)
    };

    let mut entries = Vec::with_capacity(src_files.len());
    for sf in &src_files {
        let bytes = match std::fs::read(sf) {
            Ok(b) => b,
            Err(e) => {
                builtin_err(
                    "zcompile",
                    &format!(
                        "can't open file: {}: {}",
                        sf,
                        strerror(e.raw_os_error().unwrap_or(0))
                    ),
                );
                return 1;
            }
        };
        let text = bytes_to_zstr(&bytes);
        let nodes = match Parser::new(&text, true).parse_program() {
            Ok(n) => n,
            Err(e) => {
                let msg = match e {
                    ParseError::Syntax(m) => m,
                    ParseError::Incomplete => "unexpected end of file".to_string(),
                };
                builtin_err("zcompile", &format!("{}: {}", sf, msg));
                return 1;
            }
        };
        let name = Path::new(sf)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(sf)
            .to_string();
        entries.push((name, encode_program(&nodes)));
    }

    let zwc = build_zwc(&entries);
    let _ = std::fs::remove_file(&out_path);
    match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o444)
        .open(&out_path)
    {
        Ok(mut f) => {
            use std::io::Write;
            if let Err(e) = f.write_all(&zwc) {
                builtin_err("zcompile", &format!("can't write {}: {}", out_path, e));
                return 1;
            }
        }
        Err(e) => {
            builtin_err(
                "zcompile",
                &format!(
                    "can't open zwc file: {}: {}",
                    out_path,
                    strerror(e.raw_os_error().unwrap_or(0))
                ),
            );
            return 1;
        }
    }
    0
}
