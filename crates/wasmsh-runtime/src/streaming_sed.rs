//! Streaming `sed` stage: parse-and-apply pipeline for the bounded sed subset
//! supported by the streaming executor.
//!
//! Types and readers in this module are used by the pipeline builder in
//! `lib.rs`. Shared streaming primitives (`streaming_read_next_line`,
//! `take_pending_output`, `streaming_simple_grep_match`) live in the parent
//! module.

use std::io::Read;

use crate::{streaming_read_next_line, streaming_simple_grep_match, take_pending_output};

#[derive(Clone, Debug)]
pub(crate) struct StreamingSedSubstitute {
    pub(crate) pattern: String,
    pub(crate) replacement: String,
    pub(crate) global: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum StreamingSedAddr {
    None,
    Line(usize),
    Last,
    Regex(String),
    Range(Box<StreamingSedAddr>, Box<StreamingSedAddr>),
}

#[derive(Clone, Debug)]
pub(crate) enum StreamingSedCmd {
    Substitute(StreamingSedSubstitute),
    Delete,
    Print,
    Transliterate(Vec<char>, Vec<char>),
    AppendText(String),
    InsertText(String),
    ChangeText(String),
    Quit,
}

#[derive(Clone, Debug)]
pub(crate) struct StreamingSedInstruction {
    pub(crate) addr: StreamingSedAddr,
    pub(crate) cmd: StreamingSedCmd,
}

#[derive(Clone, Debug)]
pub(crate) struct StreamingSedStage {
    pub(crate) suppress_print: bool,
    pub(crate) instructions: Vec<StreamingSedInstruction>,
}

pub(crate) struct SedStreamReader<R> {
    inner: R,
    stage: StreamingSedStage,
    input_pending: Vec<u8>,
    output_pending: Vec<u8>,
    output_offset: usize,
    initialized: bool,
    finished: bool,
    current: Option<(String, bool)>,
    next: Option<(String, bool)>,
    line_num: usize,
    range_states: Vec<bool>,
    input_eof: bool,
}

enum StreamingSedLineResult {
    Continue,
    Delete,
    Quit,
}

fn parse_streaming_sed_substitute(expr: &str) -> Option<StreamingSedSubstitute> {
    if !expr.starts_with('s') || expr.len() < 4 {
        return None;
    }
    let delim = expr.as_bytes()[1] as char;
    let rest = &expr[2..];
    let parts: Vec<&str> = rest.split(delim).collect();
    if parts.len() < 2 {
        return None;
    }
    Some(StreamingSedSubstitute {
        pattern: parts[0].to_string(),
        replacement: parts[1].to_string(),
        global: parts.get(2).is_some_and(|flags| flags.contains('g')),
    })
}

fn parse_streaming_sed_addr(s: &str) -> (StreamingSedAddr, &str) {
    if let Some(stripped) = s.strip_prefix('/') {
        if let Some(end) = stripped.find('/') {
            let pat = &stripped[..end];
            let rest = &stripped[end + 1..];
            if let Some(after_comma) = rest.strip_prefix(',') {
                let (addr2, rest2) = parse_streaming_sed_addr(after_comma);
                return (
                    StreamingSedAddr::Range(
                        Box::new(StreamingSedAddr::Regex(pat.to_string())),
                        Box::new(addr2),
                    ),
                    rest2,
                );
            }
            return (StreamingSedAddr::Regex(pat.to_string()), rest);
        }
    }
    if let Some(rest) = s.strip_prefix('$') {
        if let Some(after_comma) = rest.strip_prefix(',') {
            let (addr2, rest2) = parse_streaming_sed_addr(after_comma);
            return (
                StreamingSedAddr::Range(Box::new(StreamingSedAddr::Last), Box::new(addr2)),
                rest2,
            );
        }
        return (StreamingSedAddr::Last, rest);
    }
    let num_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if num_end > 0 {
        if let Ok(n) = s[..num_end].parse::<usize>() {
            let rest = &s[num_end..];
            if let Some(after_comma) = rest.strip_prefix(',') {
                let (addr2, rest2) = parse_streaming_sed_addr(after_comma);
                return (
                    StreamingSedAddr::Range(Box::new(StreamingSedAddr::Line(n)), Box::new(addr2)),
                    rest2,
                );
            }
            return (StreamingSedAddr::Line(n), rest);
        }
    }
    (StreamingSedAddr::None, s)
}

fn parse_streaming_sed_cmd(rest: &str) -> Option<StreamingSedCmd> {
    if rest.starts_with('s') {
        return parse_streaming_sed_substitute(rest).map(StreamingSedCmd::Substitute);
    }
    match rest {
        "d" => return Some(StreamingSedCmd::Delete),
        "p" => return Some(StreamingSedCmd::Print),
        "q" => return Some(StreamingSedCmd::Quit),
        _ => {}
    }
    if rest.starts_with("y/") || rest.starts_with("y|") {
        let delim = rest.as_bytes()[1] as char;
        let parts: Vec<&str> = rest[2..].split(delim).collect();
        return (parts.len() >= 2).then(|| {
            StreamingSedCmd::Transliterate(parts[0].chars().collect(), parts[1].chars().collect())
        });
    }
    if let Some(text) = rest.strip_prefix("a\\") {
        return Some(StreamingSedCmd::AppendText(text.trim_start().to_string()));
    }
    if let Some(text) = rest.strip_prefix("i\\") {
        return Some(StreamingSedCmd::InsertText(text.trim_start().to_string()));
    }
    if let Some(text) = rest.strip_prefix("c\\") {
        return Some(StreamingSedCmd::ChangeText(text.trim_start().to_string()));
    }
    None
}

pub(crate) fn parse_streaming_sed_script(script: &str) -> Vec<StreamingSedInstruction> {
    let mut instructions = Vec::new();
    for part in script.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (addr, rest) = parse_streaming_sed_addr(part);
        if let Some(cmd) = parse_streaming_sed_cmd(rest.trim()) {
            instructions.push(StreamingSedInstruction { addr, cmd });
        }
    }
    instructions
}

fn streaming_sed_addr_matches(
    addr: &StreamingSedAddr,
    line_num: usize,
    is_last: bool,
    line: &str,
    in_range: &mut bool,
) -> bool {
    match addr {
        StreamingSedAddr::None => true,
        StreamingSedAddr::Line(n) => line_num == *n,
        StreamingSedAddr::Last => is_last,
        StreamingSedAddr::Regex(pat) => {
            use posix_regex::compile::PosixRegexBuilder;
            PosixRegexBuilder::new(pat.as_bytes())
                .with_default_classes()
                .compile()
                .map_or_else(
                    |_| streaming_simple_grep_match(line, pat),
                    |re| !re.matches(line.as_bytes(), Some(1)).is_empty(),
                )
        }
        StreamingSedAddr::Range(start, end) => {
            if *in_range {
                if streaming_sed_addr_matches(end, line_num, is_last, line, &mut false) {
                    *in_range = false;
                }
                true
            } else if streaming_sed_addr_matches(start, line_num, is_last, line, &mut false) {
                *in_range = true;
                true
            } else {
                false
            }
        }
    }
}

/// Split sed's line anchors off a BRE pattern.
///
/// `posix-regex` does not treat `^`/`$` as zero-width anchors on a subject
/// line: a leading `^` consumes one character, so `s/^/X/` on `ab` left `Xb`.
/// A sed script is applied once per line, so the anchors are enforced by the
/// caller as start/end constraints instead of being handed to the regex.
fn streaming_sed_split_anchors(pattern: &str) -> (&str, bool, bool) {
    let mut pat = pattern;
    let anchored_start = pat.starts_with('^');
    if anchored_start {
        pat = &pat[1..];
    }
    let anchored_end = streaming_sed_has_trailing_anchor(pat);
    if anchored_end {
        pat = &pat[..pat.len() - 1];
    }
    (pat, anchored_start, anchored_end)
}

/// True when `pattern` ends with an unescaped `$` anchor. An odd number of
/// preceding backslashes escapes the `$` into a literal dollar.
fn streaming_sed_has_trailing_anchor(pattern: &str) -> bool {
    if !pattern.ends_with('$') {
        return false;
    }
    let bytes = pattern.as_bytes();
    let mut backslashes = 0usize;
    let mut i = bytes.len() - 1;
    while i > 0 && bytes[i - 1] == b'\\' {
        backslashes += 1;
        i -= 1;
    }
    backslashes.is_multiple_of(2)
}

struct SedMatch {
    start: usize,
    end: usize,
    groups: Vec<Option<(usize, usize)>>,
}

/// Find the next acceptable match at or after `from`. `posix-regex` returns
/// the leftmost match in the slice it is given, so a match rejected by an
/// anchor constraint is retried one character later. Group offsets are
/// returned relative to `text`.
fn streaming_sed_find_match(
    text: &str,
    from: usize,
    re: &posix_regex::PosixRegex<'_>,
    anchored_start: bool,
    anchored_end: bool,
) -> Option<SedMatch> {
    let len = text.len();
    let mut pos = if anchored_start { 0 } else { from };
    loop {
        if pos > len {
            return None;
        }
        let remaining = &text[pos..];
        if let Some(caps) = re.matches(remaining.as_bytes(), Some(1)).into_iter().next() {
            if let Some((rs, re_end)) = caps.first().copied().flatten() {
                let start = pos + rs;
                let end = pos + re_end;
                let start_ok = !anchored_start || start == 0;
                let end_ok = !anchored_end || end == len;
                if start_ok && end_ok {
                    let groups = caps
                        .iter()
                        .map(|g| g.map(|(s, e)| (pos + s, pos + e)))
                        .collect();
                    return Some(SedMatch { start, end, groups });
                }
            }
        }
        if anchored_start || pos == len {
            return None;
        }
        pos += text[pos..].chars().next().map_or(1, char::len_utf8);
    }
}

/// Perform a sed `s///` substitution with POSIX BRE regex support.
/// Falls back to literal replacement if the pattern fails to compile.
fn streaming_sed_substitute(text: &str, pattern: &str, replacement: &str, global: bool) -> String {
    use posix_regex::compile::PosixRegexBuilder;

    let (bare_pattern, anchored_start, anchored_end) = streaming_sed_split_anchors(pattern);

    if bare_pattern.is_empty() {
        return streaming_sed_substitute_empty(text, replacement, anchored_start, anchored_end);
    }

    match PosixRegexBuilder::new(bare_pattern.as_bytes())
        .with_default_classes()
        .compile()
    {
        Ok(re) => streaming_sed_substitute_regex(
            text,
            &re,
            replacement,
            global,
            anchored_start,
            anchored_end,
        ),
        Err(_) => {
            if global {
                text.replace(bare_pattern, replacement)
            } else {
                text.replacen(bare_pattern, replacement, 1)
            }
        }
    }
}

/// A zero-width BRE. `^` selects the start of the line, `$` the end; `^$`
/// matches only an empty line.
fn streaming_sed_substitute_empty(
    text: &str,
    replacement: &str,
    anchored_start: bool,
    anchored_end: bool,
) -> String {
    if anchored_start && anchored_end {
        return if text.is_empty() {
            replacement.to_string()
        } else {
            text.to_string()
        };
    }
    let at = if anchored_end { text.len() } else { 0 };
    let mut out = String::with_capacity(text.len() + replacement.len());
    out.push_str(&text[..at]);
    out.push_str(replacement);
    out.push_str(&text[at..]);
    out
}

fn streaming_sed_substitute_regex(
    text: &str,
    re: &posix_regex::PosixRegex<'_>,
    replacement: &str,
    global: bool,
    anchored_start: bool,
    anchored_end: bool,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    // sed suppresses a zero-width match immediately after a non-empty one, so
    // `s/a*/Y/g` over `baaab` yields `YbYbY` rather than `YbYYbY`.
    let mut suppress_empty_here = false;

    while let Some(m) = streaming_sed_find_match(text, cursor, re, anchored_start, anchored_end) {
        if m.start == m.end && suppress_empty_here && m.start == cursor {
            if cursor >= text.len() {
                break;
            }
            let ch = text[cursor..].chars().next().unwrap_or('\0');
            out.push(ch);
            cursor += ch.len_utf8();
            suppress_empty_here = false;
            continue;
        }
        out.push_str(&text[cursor..m.start]);
        streaming_sed_expand_replacement(&mut out, replacement, text, &m.groups);
        if m.end > m.start {
            cursor = m.end;
            suppress_empty_here = true;
        } else if m.start >= text.len() {
            cursor = m.end;
            break;
        } else {
            let ch = text[m.start..].chars().next().unwrap_or('\0');
            out.push(ch);
            cursor = m.start + ch.len_utf8();
            suppress_empty_here = false;
        }
        if !global {
            break;
        }
    }

    if cursor <= text.len() {
        out.push_str(&text[cursor..]);
    }
    out
}

fn streaming_sed_expand_replacement(
    out: &mut String,
    template: &str,
    text: &str,
    groups: &[Option<(usize, usize)>],
) {
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => streaming_sed_expand_escape(out, &mut chars, text, groups),
            '&' => streaming_sed_expand_whole_match(out, text, groups),
            other => out.push(other),
        }
    }
}

fn streaming_sed_expand_escape(
    out: &mut String,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    text: &str,
    groups: &[Option<(usize, usize)>],
) {
    let Some(&next) = chars.peek() else {
        out.push('\\');
        return;
    };
    if let Some(digit) = next.to_digit(10) {
        chars.next();
        if let Some(Some((s, e))) = groups.get(digit as usize).copied() {
            out.push_str(text.get(s..e).unwrap_or(""));
        }
        return;
    }
    chars.next();
    match next {
        '\\' => out.push('\\'),
        '&' => out.push('&'),
        'n' => out.push('\n'),
        't' => out.push('\t'),
        other => out.push(other),
    }
}

fn streaming_sed_expand_whole_match(
    out: &mut String,
    text: &str,
    groups: &[Option<(usize, usize)>],
) {
    if let Some(Some((s, e))) = groups.first().copied() {
        out.push_str(text.get(s..e).unwrap_or(""));
    }
}

fn streaming_sed_emit_line(output: &mut Vec<u8>, line: &str) {
    output.extend_from_slice(line.as_bytes());
    output.push(b'\n');
}

fn streaming_sed_transliterate(text: &str, from: &[char], to: &[char]) -> String {
    text.chars()
        .map(|c| {
            if let Some(pos) = from.iter().position(|&fc| fc == c) {
                to.get(pos).or(to.last()).copied().unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

impl<R> SedStreamReader<R> {
    pub(crate) fn new(inner: R, stage: StreamingSedStage) -> Self {
        let range_states = vec![false; stage.instructions.len()];
        Self {
            inner,
            stage,
            input_pending: Vec::new(),
            output_pending: Vec::new(),
            output_offset: 0,
            initialized: false,
            finished: false,
            current: None,
            next: None,
            line_num: 1,
            range_states,
            input_eof: false,
        }
    }

    fn fill_lookahead(&mut self) -> std::io::Result<()>
    where
        R: Read,
    {
        if self.current.is_none() {
            self.current = streaming_read_next_line(&mut self.inner, &mut self.input_pending)?;
        }
        if self.current.is_some() && self.next.is_none() && !self.input_eof {
            match streaming_read_next_line(&mut self.inner, &mut self.input_pending)? {
                Some(line) => self.next = Some(line),
                None => self.input_eof = true,
            }
        }
        Ok(())
    }

    fn initialize(&mut self) -> std::io::Result<()>
    where
        R: Read,
    {
        if self.initialized {
            return Ok(());
        }
        self.fill_lookahead()?;
        self.initialized = true;
        Ok(())
    }

    fn apply_instructions(&mut self, line: String, is_last: bool) -> StreamingSedLineResult {
        let mut current_text = line;
        let mut printed = false;
        for idx in 0..self.stage.instructions.len() {
            let matches_addr = streaming_sed_addr_matches(
                &self.stage.instructions[idx].addr,
                self.line_num,
                is_last,
                &current_text,
                &mut self.range_states[idx],
            );
            if !matches_addr {
                continue;
            }
            if let Some(result) = self.apply_sed_instruction(idx, &mut current_text, &mut printed) {
                return result;
            }
        }
        if !self.stage.suppress_print && !printed {
            streaming_sed_emit_line(&mut self.output_pending, &current_text);
        }
        StreamingSedLineResult::Continue
    }

    fn apply_sed_instruction(
        &mut self,
        idx: usize,
        current_text: &mut String,
        printed: &mut bool,
    ) -> Option<StreamingSedLineResult> {
        let cmd = self.stage.instructions[idx].cmd.clone();
        match cmd {
            StreamingSedCmd::Substitute(sub) => {
                *current_text = streaming_sed_substitute(
                    current_text,
                    &sub.pattern,
                    &sub.replacement,
                    sub.global,
                );
            }
            StreamingSedCmd::Delete => return Some(StreamingSedLineResult::Delete),
            StreamingSedCmd::Print => {
                streaming_sed_emit_line(&mut self.output_pending, current_text);
                *printed = true;
            }
            StreamingSedCmd::Transliterate(from, to) => {
                *current_text = streaming_sed_transliterate(current_text, &from, &to);
            }
            StreamingSedCmd::AppendText(text) => {
                if !self.stage.suppress_print && !*printed {
                    streaming_sed_emit_line(&mut self.output_pending, current_text);
                    *printed = true;
                }
                streaming_sed_emit_line(&mut self.output_pending, &text);
            }
            StreamingSedCmd::InsertText(text) => {
                streaming_sed_emit_line(&mut self.output_pending, &text);
            }
            StreamingSedCmd::ChangeText(text) => {
                streaming_sed_emit_line(&mut self.output_pending, &text);
                return Some(StreamingSedLineResult::Delete);
            }
            StreamingSedCmd::Quit => {
                if !self.stage.suppress_print && !*printed {
                    streaming_sed_emit_line(&mut self.output_pending, current_text);
                }
                return Some(StreamingSedLineResult::Quit);
            }
        }
        None
    }
}

impl<R: Read> Read for SedStreamReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let copied =
                take_pending_output(&mut self.output_pending, &mut self.output_offset, buf);
            if copied > 0 {
                return Ok(copied);
            }
            if self.finished {
                return Ok(0);
            }

            self.initialize()?;
            self.fill_lookahead()?;
            let Some((line, _had_newline)) = self.current.take() else {
                self.finished = true;
                continue;
            };

            let is_last = self.input_eof && self.next.is_none();
            match self.apply_instructions(line, is_last) {
                StreamingSedLineResult::Quit => self.finished = true,
                StreamingSedLineResult::Delete | StreamingSedLineResult::Continue => {
                    self.current = self.next.take();
                    if self.current.is_some() {
                        self.line_num += 1;
                        self.fill_lookahead()?;
                    } else {
                        self.finished = true;
                    }
                }
            }
        }
    }
}
