//! In-process C preprocessor for devicetree sources.
//!
//! Devicetree builds run `cpp` before dtc, so anything that looks at the
//! merged tree (duplicate labels across includes, `#ifdef` branches, macros)
//! needs the same expansion. Spawning a process per edit is far too slow, so
//! this walks the include graph in memory instead: object- and function-like
//! macros, `#if`/`#ifdef`/`#elif`/`#else`, `#include` (quoted and angle),
//! `#pragma once`, comments, and line splicing.
//!
//! Every output line keeps the file and line it came from, so diagnostics can
//! be mapped back to the original sources (clangd's SourceMapper equivalent).
//! Open editor buffers are read from `overlays` instead of disk; files are
//! read at most once per expansion.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct Input<'a> {
    pub root: &'a Path,
    pub include_dirs: &'a [PathBuf],
    /// unsaved editor buffers, keyed by canonical absolute path
    pub overlays: &'a BTreeMap<PathBuf, String>,
}

#[derive(Clone, Debug)]
pub struct Define {
    pub name: String,
    /// replacement text (empty for a flag macro)
    pub value: String,
    pub file: u32,
}

#[derive(Clone, Debug)]
pub struct Error {
    pub file: u32,
    /// 0-based
    pub line: u32,
    pub message: String,
}

pub struct Expanded {
    pub files: Vec<PathBuf>,
    /// preprocessed source, one entry per emitted line
    pub text: String,
    /// per output line: (file index, 0-based source line)
    pub lines: Vec<(u32, u32)>,
    pub defines: Vec<Define>,
    pub errors: Vec<Error>,
}

#[derive(Clone)]
struct Macro {
    params: Option<Vec<String>>,
    body: String,
}

struct Frame {
    parent_active: bool,
    active: bool,
    taken: bool,
}

const MAX_INCLUDE_DEPTH: usize = 64;
const MAX_EXPANSION_DEPTH: usize = 32;

struct Expander<'a> {
    input: &'a Input<'a>,
    texts: HashMap<PathBuf, Option<String>>,
    files: Vec<PathBuf>,
    file_ids: HashMap<PathBuf, u32>,
    once: HashSet<PathBuf>,
    macros: HashMap<String, Macro>,
    frames: Vec<Frame>,
    out: Expanded,
    stack: Vec<PathBuf>,
}

pub fn expand(input: &Input<'_>) -> Expanded {
    let root = canonical(input.root);
    let mut expander = Expander {
        input,
        texts: HashMap::new(),
        files: Vec::new(),
        file_ids: HashMap::new(),
        once: HashSet::new(),
        macros: HashMap::new(),
        frames: Vec::new(),
        out: Expanded {
            files: Vec::new(),
            text: String::new(),
            lines: Vec::new(),
            defines: Vec::new(),
            errors: Vec::new(),
        },
        stack: Vec::new(),
    };
    expander.define_builtin("__DTS__", "1");
    expander.process_file(&root, None);
    expander.out.files = expander.files;
    expander.out
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

impl<'a> Expander<'a> {
    fn define_builtin(&mut self, name: &str, value: &str) {
        self.macros.insert(
            name.to_string(),
            Macro {
                params: None,
                body: value.to_string(),
            },
        );
    }

    fn file_id(&mut self, path: &Path) -> u32 {
        if let Some(id) = self.file_ids.get(path) {
            return *id;
        }
        let id = self.files.len() as u32;
        self.files.push(path.to_path_buf());
        self.file_ids.insert(path.to_path_buf(), id);
        id
    }

    fn text_of(&mut self, path: &Path) -> Option<String> {
        if let Some(text) = self.texts.get(path) {
            return text.clone();
        }
        let text = match self.input.overlays.get(path) {
            Some(overlay) => Some(overlay.clone()),
            None => std::fs::read_to_string(path).ok(),
        };
        self.texts.insert(path.to_path_buf(), text.clone());
        text
    }

    fn active(&self) -> bool {
        self.frames.last().map(|frame| frame.active).unwrap_or(true)
    }

    fn process_file(&mut self, path: &Path, from: Option<(u32, u32)>) {
        let path = canonical(path);
        if self.once.contains(&path) {
            return;
        }
        if self.stack.len() >= MAX_INCLUDE_DEPTH || self.stack.contains(&path) {
            if let Some((file, line)) = from {
                self.out.errors.push(Error {
                    file,
                    line,
                    message: format!("recursive #include of {}", path.display()),
                });
            }
            return;
        }
        let Some(text) = self.text_of(&path) else {
            if let Some((file, line)) = from {
                self.out.errors.push(Error {
                    file,
                    line,
                    message: format!("cannot read {}", path.display()),
                });
            }
            return;
        };
        let Some(text) = text.strip_prefix('\u{feff}').map(str::to_string).or(Some(text)) else {
            return;
        };
        self.stack.push(path.clone());
        let file = self.file_id(&path);

        let mut line_number = 0u32;
        let mut in_block_comment = false;
        let mut physical = text.lines();
        while let Some(first) = physical.next() {
            let mut line = first.to_string();
            let mut consumed = 1u32;
            while line.ends_with('\\') {
                line.pop();
                let Some(next) = physical.next() else { break };
                line.push_str(next);
                consumed += 1;
            }
            let code = strip_comments(&line, &mut in_block_comment);
            self.process_line(&path, file, line_number, &code);
            line_number += consumed;
        }
        self.stack.pop();
    }

    fn process_line(&mut self, path: &Path, file: u32, line: u32, code: &str) {
        let trimmed = code.trim_start();
        if let Some(directive) = trimmed.strip_prefix('#') {
            let directive = directive.trim_start();
            let (name, rest) = split_ident(directive);
            let is_directive = matches!(
                name,
                "include"
                    | "define"
                    | "undef"
                    | "if"
                    | "ifdef"
                    | "ifndef"
                    | "elif"
                    | "else"
                    | "endif"
                    | "error"
                    | "pragma"
                    | "warning"
                    | "line"
            );
            // `# 1 "file"` linemarkers, and devicetree properties such as
            // `#address-cells = <2>;` are not directives: let the latter fall
            // through as ordinary source.
            if !is_directive {
                let linemarker = directive
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit() || c == '"');
                if linemarker {
                    return;
                }
            } else {
                match name {
                "include" => {
                    if !self.active() {
                        return;
                    }
                    let rest = rest.trim();
                    match include_target(rest) {
                        Some((target, quoted)) => match self.resolve(path, target, quoted) {
                            Some(child) => self.process_file(&child, Some((file, line))),
                            None => self.out.errors.push(Error {
                                file,
                                line,
                                message: format!("cannot find include \"{target}\""),
                            }),
                        },
                        None => self.out.errors.push(Error {
                            file,
                            line,
                            message: format!("malformed #include: {rest}"),
                        }),
                    }
                }
                "define" => {
                    if self.active() {
                        self.define(file, rest);
                    }
                }
                "undef" => {
                    if self.active() {
                        let (name, _) = split_ident(rest.trim_start());
                        self.macros.remove(name);
                    }
                }
                "ifdef" | "ifndef" | "if" | "elif" | "else" | "endif" => {
                    self.conditional(name, rest.trim(), file, line);
                }
                "error" => {
                    if self.active() {
                        self.out.errors.push(Error {
                            file,
                            line,
                            message: rest.trim().to_string(),
                        });
                    }
                }
                "pragma" => {
                    if self.active() && rest.trim() == "once" {
                        self.once.insert(path.to_path_buf());
                    }
                }
                _ => {}
                }
                return;
            }
        }
        if !self.active() {
            return;
        }
        let expanded = self.expand_text(code, 0, &mut HashSet::new());
        self.out.text.push_str(&expanded);
        self.out.text.push('\n');
        self.out.lines.push((file, line));
    }

    fn define(&mut self, file: u32, rest: &str) {
        let rest = rest.trim_start();
        let (name, after_name) = split_ident(rest);
        if name.is_empty() {
            return;
        }
        let mut params = None;
        let mut body = after_name;
        if let Some(rest) = after_name.strip_prefix('(')
            && let Some((param_list, remaining)) = rest.split_once(')')
        {
            params = Some(
                param_list
                    .split(',')
                    .map(|param| param.trim().to_string())
                    .filter(|param| !param.is_empty())
                    .collect(),
            );
            body = remaining;
        }
        let body = body.trim().to_string();
        self.macros.insert(
            name.to_string(),
            Macro { params, body: body.clone() },
        );
        self.out.defines.push(Define {
            name: name.to_string(),
            value: body,
            file,
        });
    }

    fn conditional(&mut self, kind: &str, rest: &str, file: u32, line: u32) {
        match kind {
            "if" | "ifdef" | "ifndef" => {
                let parent_active = self.active();
                let condition = if kind == "if" {
                    self.eval_condition(rest, file, line)
                } else {
                    let (name, _) = split_ident(rest);
                    let defined = self.macros.contains_key(name);
                    if kind == "ifdef" { defined } else { !defined }
                };
                let active = parent_active && condition;
                self.frames.push(Frame {
                    parent_active,
                    active,
                    taken: active,
                });
            }
            "elif" => {
                let Some(frame) = self.frames.last() else {
                    return;
                };
                if frame.taken || !frame.parent_active {
                    if let Some(frame) = self.frames.last_mut() {
                        frame.active = false;
                    }
                    return;
                }
                let condition = self.eval_condition(rest, file, line);
                if let Some(frame) = self.frames.last_mut() {
                    frame.active = condition;
                    frame.taken = condition;
                }
            }
            "else" => {
                if let Some(frame) = self.frames.last_mut() {
                    frame.active = frame.parent_active && !frame.taken;
                    frame.taken = true;
                }
            }
            "endif" => {
                self.frames.pop();
            }
            _ => {}
        }
    }

    fn resolve(&mut self, from: &Path, target: &str, quoted: bool) -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if quoted && let Some(parent) = from.parent() {
            candidates.push(parent.join(target));
        }
        for dir in self.input.include_dirs {
            candidates.push(dir.join(target));
        }
        for candidate in candidates {
            let candidate = canonical(&candidate);
            if self.text_of(&candidate).is_some() {
                return Some(candidate);
            }
        }
        None
    }

    fn eval_condition(&mut self, expression: &str, file: u32, line: u32) -> bool {
        let replaced = replace_defined(expression, &self.macros);
        let expanded = self.expand_text(&replaced, 0, &mut HashSet::new());
        let tokens = tokenize_condition(&expanded);
        let mut parser = ExprParser {
            tokens: &tokens,
            pos: 0,
        };
        match parser.parse_or() {
            Some(value) => value != 0,
            // an unparseable condition means the preprocessor cannot tell
            // which branch is live: say so instead of silently taking #else
            None => {
                self.out.errors.push(Error {
                    file,
                    line,
                    message: format!("cannot evaluate #if expression: {expression}"),
                });
                false
            }
        }
    }

    fn expand_text(
        &self,
        text: &str,
        depth: usize,
        disabled: &mut HashSet<String>,
    ) -> String {
        if depth > MAX_EXPANSION_DEPTH {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'"' || b == b'\'' {
                let quote = b;
                out.push(b as char);
                i += 1;
                while i < bytes.len() {
                    let current = bytes[i];
                    push_byte_or_char(text, &mut i, &mut out);
                    if current == b'\\' && i < bytes.len() {
                        push_byte_or_char(text, &mut i, &mut out);
                        continue;
                    }
                    if current == quote {
                        break;
                    }
                }
                continue;
            }
            if is_ident_start(b) {
                let start = i;
                while i < bytes.len() && is_ident_char(bytes[i]) {
                    i += 1;
                }
                let name = &text[start..i];
                let Some(mac) = self.macros.get(name) else {
                    out.push_str(name);
                    continue;
                };
                if disabled.contains(name) {
                    out.push_str(name);
                    continue;
                }
                match &mac.params {
                    None => {
                        disabled.insert(name.to_string());
                        let body = self.expand_text(&mac.body, depth + 1, disabled);
                        disabled.remove(name);
                        out.push_str(&body);
                    }
                    Some(params) => {
                        let mut j = i;
                        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                            j += 1;
                        }
                        if j >= bytes.len() || bytes[j] != b'(' {
                            out.push_str(name);
                            continue;
                        }
                        let Some((args, end)) = call_arguments(text, j) else {
                            out.push_str(name);
                            continue;
                        };
                        let mut body = mac.body.clone();
                        for (param, arg) in params.iter().zip(args.iter()) {
                            body = replace_ident(&body, param, arg.trim());
                        }
                        disabled.insert(name.to_string());
                        let expanded = self.expand_text(&body, depth + 1, disabled);
                        disabled.remove(name);
                        out.push_str(&expanded);
                        i = end;
                    }
                }
                continue;
            }
            push_byte_or_char(text, &mut i, &mut out);
        }
        out
    }
}

/// Copy one character (UTF-8 aware) from `text` at `*i` into `out`.
fn push_byte_or_char(text: &str, i: &mut usize, out: &mut String) {
    let ch = text[*i..].chars().next().unwrap_or('\u{fffd}');
    out.push(ch);
    *i += ch.len_utf8();
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn split_ident(text: &str) -> (&str, &str) {
    let end = text
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    (&text[..end], &text[end..])
}

/// Remove `//` and `/* */` comments, keeping strings intact.
fn strip_comments(line: &str, in_block: &mut bool) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if *in_block {
            if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                *in_block = false;
                i += 2;
                out.push(' ');
                continue;
            }
            i += 1;
            continue;
        }
        if b == b'"' || b == b'\'' {
            let quote = b;
            out.push(b as char);
            i += 1;
            while i < bytes.len() {
                let current = bytes[i];
                push_byte_or_char(line, &mut i, &mut out);
                if current == b'\\' && i < bytes.len() {
                    push_byte_or_char(line, &mut i, &mut out);
                    continue;
                }
                if current == quote {
                    break;
                }
            }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            break;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            *in_block = true;
            i += 2;
            out.push(' ');
            continue;
        }
        push_byte_or_char(line, &mut i, &mut out);
    }
    out
}

fn include_target(rest: &str) -> Option<(&str, bool)> {
    if let Some(rest) = rest.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some((&rest[..end], true));
    }
    if let Some(rest) = rest.strip_prefix('<') {
        let end = rest.find('>')?;
        return Some((&rest[..end], false));
    }
    None
}

fn call_arguments(text: &str, start: usize) -> Option<(Vec<String>, usize)> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut args = Vec::new();
    let mut current = String::new();
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                if depth > 1 {
                    current.push('(');
                }
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    args.push(current.trim().to_string());
                    return Some((args, i + 1));
                }
                current.push(')');
            }
            b',' if depth == 1 => {
                args.push(current.trim().to_string());
                current.clear();
            }
            b => current.push(b as char),
        }
        i += 1;
    }
    None
}

fn replace_ident(text: &str, name: &str, value: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if is_ident_start(bytes[i]) {
            let start = i;
            while i < bytes.len() && is_ident_char(bytes[i]) {
                i += 1;
            }
            let ident = &text[start..i];
            if ident == name {
                out.push('(');
                out.push_str(value);
                out.push(')');
            } else {
                out.push_str(ident);
            }
        } else {
            push_byte_or_char(text, &mut i, &mut out);
        }
    }
    out
}

fn replace_defined(expression: &str, macros: &HashMap<String, Macro>) -> String {
    let bytes = expression.as_bytes();
    let mut out = String::with_capacity(expression.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"defined") {
            let mut j = i + "defined".len();
            let mut has_paren = false;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'(' {
                has_paren = true;
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
            }
            let start = j;
            while j < bytes.len() && is_ident_char(bytes[j]) {
                j += 1;
            }
            let name = &expression[start..j];
            if !name.is_empty() {
                out.push_str(if macros.contains_key(name) { "1" } else { "0" });
                if has_paren {
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b')' {
                        j += 1;
                    }
                }
                i = j;
                continue;
            }
        }
        push_byte_or_char(expression, &mut i, &mut out);
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(i64),
    Op(String),
    LParen,
    RParen,
}

fn tokenize_condition(text: &str) -> Vec<Tok> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b.is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
                i += 1;
            }
            let raw = &text[start..i];
            let value = if let Some(hex) = raw
                .strip_prefix("0x")
                .or_else(|| raw.strip_prefix("0X"))
            {
                i64::from_str_radix(hex, 16).unwrap_or(0)
            } else {
                raw.trim_end_matches(['u', 'U', 'l', 'L'])
                    .parse::<i64>()
                    .unwrap_or(0)
            };
            tokens.push(Tok::Num(value));
            continue;
        }
        if is_ident_start(b) {
            // macros were expanded already; bare identifiers are 0 in cpp
            while i < bytes.len() && is_ident_char(bytes[i]) {
                i += 1;
            }
            tokens.push(Tok::Num(0));
            continue;
        }
        if b == b'(' {
            tokens.push(Tok::LParen);
            i += 1;
            continue;
        }
        if b == b')' {
            tokens.push(Tok::RParen);
            i += 1;
            continue;
        }
        let mut matched = false;
        for op in ["&&", "||", "==", "!=", "<=", ">=", "<<", ">>"] {
            if bytes[i..].starts_with(op.as_bytes()) {
                tokens.push(Tok::Op(op.to_string()));
                i += 2;
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        if matches!(
            b,
            b'-' | b'+' | b'*' | b'/' | b'%' | b'!' | b'~' | b'<' | b'>' | b'&' | b'|' | b'^'
        ) {
            tokens.push(Tok::Op((b as char).to_string()));
            i += 1;
            continue;
        }
        i += 1;
    }
    tokens
}

struct ExprParser<'a> {
    tokens: &'a [Tok],
    pos: usize,
}

impl ExprParser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if let Some(Tok::Op(current)) = self.peek()
            && current == op
        {
            self.pos += 1;
            return true;
        }
        false
    }

    fn parse_or(&mut self) -> Option<i64> {
        let mut value = self.parse_and()?;
        while self.eat_op("||") {
            let right = self.parse_and()?;
            value = (value != 0 || right != 0) as i64;
        }
        Some(value)
    }

    fn parse_and(&mut self) -> Option<i64> {
        let mut value = self.parse_bitor()?;
        while self.eat_op("&&") {
            let right = self.parse_bitor()?;
            value = (value != 0 && right != 0) as i64;
        }
        Some(value)
    }

    fn parse_bitor(&mut self) -> Option<i64> {
        let mut value = self.parse_bitxor()?;
        while self.eat_op("|") {
            value |= self.parse_bitxor()?;
        }
        Some(value)
    }

    fn parse_bitxor(&mut self) -> Option<i64> {
        let mut value = self.parse_bitand()?;
        while self.eat_op("^") {
            value ^= self.parse_bitand()?;
        }
        Some(value)
    }

    fn parse_bitand(&mut self) -> Option<i64> {
        let mut value = self.parse_equality()?;
        while self.eat_op("&") {
            value &= self.parse_equality()?;
        }
        Some(value)
    }

    fn parse_equality(&mut self) -> Option<i64> {
        let mut value = self.parse_relational()?;
        loop {
            if self.eat_op("==") {
                value = (value == self.parse_relational()?) as i64;
            } else if self.eat_op("!=") {
                value = (value != self.parse_relational()?) as i64;
            } else {
                return Some(value);
            }
        }
    }

    fn parse_relational(&mut self) -> Option<i64> {
        let mut value = self.parse_shift()?;
        loop {
            if self.eat_op("<=") {
                value = (value <= self.parse_shift()?) as i64;
            } else if self.eat_op(">=") {
                value = (value >= self.parse_shift()?) as i64;
            } else if self.eat_op("<") {
                value = (value < self.parse_shift()?) as i64;
            } else if self.eat_op(">") {
                value = (value > self.parse_shift()?) as i64;
            } else {
                return Some(value);
            }
        }
    }

    fn parse_shift(&mut self) -> Option<i64> {
        let mut value = self.parse_add()?;
        loop {
            if self.eat_op("<<") {
                value <<= self.parse_add()?;
            } else if self.eat_op(">>") {
                value >>= self.parse_add()?;
            } else {
                return Some(value);
            }
        }
    }

    fn parse_add(&mut self) -> Option<i64> {
        let mut value = self.parse_mul()?;
        loop {
            if self.eat_op("+") {
                value += self.parse_mul()?;
            } else if self.eat_op("-") {
                value -= self.parse_mul()?;
            } else {
                return Some(value);
            }
        }
    }

    fn parse_mul(&mut self) -> Option<i64> {
        let mut value = self.parse_unary()?;
        loop {
            if self.eat_op("*") {
                value *= self.parse_unary()?;
            } else if self.eat_op("/") {
                let right = self.parse_unary()?;
                if right != 0 {
                    value /= right;
                }
            } else if self.eat_op("%") {
                let right = self.parse_unary()?;
                if right != 0 {
                    value %= right;
                }
            } else {
                return Some(value);
            }
        }
    }

    fn parse_unary(&mut self) -> Option<i64> {
        if self.eat_op("!") {
            return Some((self.parse_unary()? == 0) as i64);
        }
        if self.eat_op("~") {
            return Some(!self.parse_unary()?);
        }
        if self.eat_op("-") {
            return Some(-self.parse_unary()?);
        }
        if self.eat_op("+") {
            return self.parse_unary();
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Option<i64> {
        let token = self.peek()?.clone();
        match token {
            Tok::Num(value) => {
                self.pos += 1;
                Some(value)
            }
            Tok::LParen => {
                self.pos += 1;
                let value = self.parse_or()?;
                if matches!(self.peek(), Some(Tok::RParen)) {
                    self.pos += 1;
                }
                Some(value)
            }
            _ => {
                self.pos += 1;
                Some(0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    fn work(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dtsls-preproc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run(dir: &Path, root: &Path, overlays: &BTreeMap<PathBuf, String>) -> Expanded {
        expand(&Input {
            root,
            include_dirs: &[dir.to_path_buf()],
            overlays,
        })
    }

    #[test]
    fn expands_includes_and_maps_lines() {
        let dir = work("include");
        let root = dir.join("board.dts");
        let sub = dir.join("sub.dtsi");
        write(&sub, "/ { l: node {}; };\n");
        write(&root, "/dts-v1/;\n#include \"sub.dtsi\"\n/ {\n};\n");
        let expanded = run(&dir, &root, &BTreeMap::new());
        assert!(expanded.errors.is_empty(), "{:?}", expanded.errors);
        assert!(expanded.text.contains("l: node"));
        let line = expanded
            .text
            .lines()
            .position(|line| line.contains("l: node"))
            .unwrap();
        let (file, source_line) = expanded.lines[line];
        assert_eq!(expanded.files[file as usize], canonical(&sub));
        assert_eq!(source_line, 0);
    }

    #[test]
    fn expands_macros_in_nodes_and_labels() {
        let dir = work("macros");
        let root = dir.join("board.dts");
        write(
            &root,
            "#define LBL mylabel\n#define NODE node@1\n/ { LBL: NODE { }; };\n",
        );
        let expanded = run(&dir, &root, &BTreeMap::new());
        assert!(expanded.text.contains("mylabel: node@1"), "{}", expanded.text);
        assert!(expanded.defines.iter().any(|d| d.name == "LBL"));
    }

    #[test]
    fn selects_conditional_branches() {
        let dir = work("cond");
        let root = dir.join("board.dts");
        write(
            &root,
            "#define USE_A\n#if defined(USE_A) && (1 << 3) == 8\n/ { a {}; };\n#else\n/ { b {}; };\n#endif\n#if 0\n/ { c {}; };\n#endif\n",
        );
        let expanded = run(&dir, &root, &BTreeMap::new());
        assert!(expanded.text.contains("a {"), "{}", expanded.text);
        assert!(!expanded.text.contains("b {"), "{}", expanded.text);
        assert!(!expanded.text.contains("c {"), "{}", expanded.text);
    }

    #[test]
    fn unsaved_buffers_override_disk() {
        let dir = work("overlay");
        let root = dir.join("board.dts");
        let sub = dir.join("sub.dtsi");
        write(&sub, "/ { from_disk {}; };\n");
        write(&root, "#include \"sub.dtsi\"\n");
        let mut overlays = BTreeMap::new();
        overlays.insert(canonical(&sub), "/ { from_editor {}; };\n".to_string());
        let expanded = run(&dir, &root, &overlays);
        assert!(expanded.text.contains("from_editor"), "{}", expanded.text);
        assert!(!expanded.text.contains("from_disk"));
    }

    #[test]
    fn reports_missing_include() {
        let dir = work("missing");
        let root = dir.join("board.dts");
        write(&root, "#include \"nope.dtsi\"\n");
        let expanded = run(&dir, &root, &BTreeMap::new());
        assert!(!expanded.errors.is_empty());
        assert!(expanded.errors[0].message.contains("cannot find include"));
    }

    #[test]
    fn respects_include_guards() {
        let dir = work("guards");
        let root = dir.join("board.dts");
        let header = dir.join("h.dtsi");
        write(&header, "#ifndef H_GUARD\n#define H_GUARD\n/ { once {}; };\n#endif\n");
        write(&root, "#include \"h.dtsi\"\n#include \"h.dtsi\"\n");
        let expanded = run(&dir, &root, &BTreeMap::new());
        assert_eq!(expanded.text.matches("once {").count(), 1, "{}", expanded.text);
    }

    #[test]
    fn strips_comments_without_losing_lines() {
        let dir = work("comments");
        let root = dir.join("board.dts");
        write(&root, "/* } */\n/ {\n\t// x = <1>;\n\ty = <2>;\n};\n");
        let expanded = run(&dir, &root, &BTreeMap::new());
        assert_eq!(expanded.text.matches('}').count(), 1, "{}", expanded.text);
        assert_eq!(expanded.lines.len(), expanded.text.lines().count());
    }

    #[test]
    #[ignore]
    fn bench_real_board() {
        let root = std::path::Path::new(
            "/linux-7.2.2/arch/arm64/boot/dts/qcom/sm8650-qrd.dts",
        );
        let include_dirs = [
            PathBuf::from("/linux-7.2.2/include"),
            PathBuf::from("/linux-7.2.2/scripts/dtc/include-prefixes"),
        ];
        let overlays = BTreeMap::new();
        let started = std::time::Instant::now();
        let expanded = expand(&Input {
            root,
            include_dirs: &include_dirs,
            overlays: &overlays,
        });
        println!(
            "expand: {:?}, files={}, text_lines={}, text_bytes={}, defines={}, errors={}",
            started.elapsed(),
            expanded.files.len(),
            expanded.lines.len(),
            expanded.text.len(),
            expanded.defines.len(),
            expanded.errors.len()
        );
        for error in expanded.errors.iter().take(5) {
            println!("  error: {}:{}", expanded.files[error.file as usize].display(), error.line);
        }
    }

    #[test]
    fn hash_properties_are_not_directives() {
        let dir = work("hash");
        let root = dir.join("board.dts");
        write(
            &root,
            "/ {\n\t#address-cells = <1>;\n\t#size-cells = <1>;\n\tnode {\n\t\t#clock-cells = <2>;\n\t};\n};\n",
        );
        let expanded = run(&dir, &root, &BTreeMap::new());
        assert!(expanded.text.contains("#address-cells = <1>;"), "{}", expanded.text);
        assert!(expanded.text.contains("#clock-cells = <2>;"), "{}", expanded.text);
    }

    #[test]
    fn function_like_macros_expand() {
        let dir = work("fn");
        let root = dir.join("board.dts");
        write(
            &root,
            "#define CELLS(n) <n 1>\n#define TWO 2\n/ { x = CELLS(TWO); };\n",
        );
        let expanded = run(&dir, &root, &BTreeMap::new());
        assert!(expanded.text.contains("(2) 1"), "{}", expanded.text);
    }
}
