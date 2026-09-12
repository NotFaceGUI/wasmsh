//! `join` — join lines of two files on a common field.
//!
//! GNU-compatible subset covering the options real scripts use: `-1`/`-2`/`-j`,
//! `-t`, `-a`, `-v`, `-o` (including `auto` and `0`), `-e`, and `-i`.
//! Inputs must be sorted on the join field; out-of-order input reports the
//! offending line on stderr and exits non-zero, after emitting the pairs that
//! could be formed.

use crate::helpers::{collect_path_text, resolve_path};
use crate::UtilContext;

/// One field of the requested `-o` output.
#[derive(Clone, Copy)]
enum OutField {
    /// The join field itself (`-o 0`).
    Join,
    /// Field `n` (1-based) of input 1 or 2 (`-o 1.n`).
    Input(usize, usize),
}

#[allow(clippy::struct_excessive_bools)]
struct JoinOptions {
    field1: usize,
    field2: usize,
    separator: Option<char>,
    unpairable1: bool,
    unpairable2: bool,
    only_unpairable: bool,
    format: Vec<OutField>,
    empty: Option<String>,
    ignore_case: bool,
    check_order: bool,
}

impl Default for JoinOptions {
    fn default() -> Self {
        Self {
            field1: 1,
            field2: 1,
            separator: None,
            unpairable1: false,
            unpairable2: false,
            only_unpairable: false,
            format: Vec::new(),
            empty: None,
            ignore_case: false,
            check_order: true,
        }
    }
}

struct Record {
    line: String,
    fields: Vec<String>,
}

pub(crate) fn util_join(ctx: &mut UtilContext<'_>, argv: &[&str]) -> i32 {
    let (opts, files) = match parse_args(argv) {
        Ok(v) => v,
        Err(code) => {
            if code != 0 {
                ctx.output.stderr(b"join: invalid option or operand\n");
            }
            return if code == 0 { 1 } else { code };
        }
    };
    if files.len() < 2 {
        ctx.output.stderr(b"join: missing operand\n");
        return 1;
    }

    let path1 = resolve_path(ctx.cwd, files[0]);
    let path2 = resolve_path(ctx.cwd, files[1]);
    let text1 = match collect_path_text(ctx, &path1, files[0], "join") {
        Ok(t) => t,
        Err(code) => return code,
    };
    let text2 = match collect_path_text(ctx, &path2, files[1], "join") {
        Ok(t) => t,
        Err(code) => return code,
    };

    let recs1 = parse_records(&text1, &opts);
    let recs2 = parse_records(&text2, &opts);

    let mut out_of_order = false;
    if opts.check_order {
        out_of_order |= check_order_of(ctx, &recs1, files[0], opts.field1, &opts);
        out_of_order |= check_order_of(ctx, &recs2, files[1], opts.field2, &opts);
    }

    let text = emit_join(&recs1, &recs2, &opts);
    ctx.output.stdout(text.as_bytes());

    if out_of_order {
        ctx.output.stderr(b"join: input is not in sorted order\n");
        1
    } else {
        0
    }
}

fn parse_args<'a>(argv: &'a [&'a str]) -> Result<(JoinOptions, Vec<&'a str>), i32> {
    let mut opts = JoinOptions::default();
    let mut files = Vec::new();
    let mut i = 1;
    let mut end_of_options = false;

    while i < argv.len() {
        let arg = argv[i];
        if end_of_options || arg == "-" || !arg.starts_with('-') {
            files.push(arg);
            i += 1;
            continue;
        }
        match arg {
            "--" => {
                end_of_options = true;
                i += 1;
            }
            "--ignore-case" | "-i" => {
                opts.ignore_case = true;
                i += 1;
            }
            "--check-order" => {
                opts.check_order = true;
                i += 1;
            }
            "--nocheck-order" => {
                opts.check_order = false;
                i += 1;
            }
            "-a" | "-v" => {
                let value = take_value(argv, &mut i)?;
                apply_file_selector(&mut opts, value, arg == "-v")?;
            }
            "-e" => {
                opts.empty = Some(take_value(argv, &mut i)?.to_string());
            }
            "-1" => {
                opts.field1 = parse_field(take_value(argv, &mut i)?)?;
            }
            "-2" => {
                opts.field2 = parse_field(take_value(argv, &mut i)?)?;
            }
            "-j" => {
                let n = parse_field(take_value(argv, &mut i)?)?;
                opts.field1 = n;
                opts.field2 = n;
            }
            "-t" => {
                opts.separator = take_value(argv, &mut i)?.chars().next();
            }
            "-o" => {
                let value = take_value(argv, &mut i)?;
                parse_output_format(value, &mut opts)?;
            }
            _ if arg.starts_with("-o") && arg.len() > 2 => {
                parse_output_format(&arg[2..], &mut opts)?;
                i += 1;
            }
            _ if arg.starts_with("-t") && arg.len() > 2 => {
                opts.separator = arg[2..].chars().next();
                i += 1;
            }
            _ if arg.starts_with("-a") && arg.len() > 2 => {
                apply_file_selector(&mut opts, &arg[2..], false)?;
                i += 1;
            }
            _ if arg.starts_with("-v") && arg.len() > 2 => {
                apply_file_selector(&mut opts, &arg[2..], true)?;
                i += 1;
            }
            _ if arg.starts_with("-1") && arg.len() > 2 => {
                opts.field1 = parse_field(&arg[2..])?;
                i += 1;
            }
            _ if arg.starts_with("-2") && arg.len() > 2 => {
                opts.field2 = parse_field(&arg[2..])?;
                i += 1;
            }
            _ if arg.starts_with("-j") && arg.len() > 2 => {
                let n = parse_field(&arg[2..])?;
                opts.field1 = n;
                opts.field2 = n;
                i += 1;
            }
            _ => return Err(1),
        }
    }

    // `-v` prints only unpairable lines, so any `-o` list is ignored.
    if opts.only_unpairable {
        opts.format.clear();
    }
    Ok((opts, files))
}

fn take_value<'a>(argv: &'a [&'a str], i: &mut usize) -> Result<&'a str, i32> {
    let value = argv.get(*i + 1).copied().ok_or(1)?;
    *i += 2;
    Ok(value)
}

fn apply_file_selector(
    opts: &mut JoinOptions,
    value: &str,
    only_unpairable: bool,
) -> Result<(), i32> {
    match value {
        "1" => opts.unpairable1 = true,
        "2" => opts.unpairable2 = true,
        _ => return Err(1),
    }
    if only_unpairable {
        opts.only_unpairable = true;
    }
    Ok(())
}

fn parse_field(value: &str) -> Result<usize, i32> {
    value.parse::<usize>().ok().filter(|n| *n > 0).ok_or(1)
}

fn parse_output_format(spec: &str, opts: &mut JoinOptions) -> Result<(), i32> {
    // `auto` reproduces the default layout; the empty list is not accepted.
    if spec == "auto" {
        opts.format.clear();
        return Ok(());
    }
    let mut parsed = Vec::new();
    for token in spec.split([' ', ',']).filter(|t| !t.is_empty()) {
        if token == "0" {
            parsed.push(OutField::Join);
            continue;
        }
        let Some((input, field)) = token.split_once('.') else {
            return Err(1);
        };
        let input: usize = input.parse().map_err(|_| 1)?;
        let field: usize = field.parse().map_err(|_| 1)?;
        if !(1..=2).contains(&input) || field == 0 {
            return Err(1);
        }
        parsed.push(OutField::Input(input, field));
    }
    if parsed.is_empty() {
        return Err(1);
    }
    opts.format = parsed;
    Ok(())
}

fn split_fields<'a>(line: &'a str, opts: &JoinOptions) -> Vec<String> {
    match opts.separator {
        Some(sep) => line.split(sep).map(str::to_string).collect(),
        None => line.split_whitespace().map(str::to_string).collect(),
    }
}

fn parse_records(text: &str, opts: &JoinOptions) -> Vec<Record> {
    let mut records: Vec<Record> = text
        .split('\n')
        .map(|raw| {
            let line = raw.strip_suffix('\r').unwrap_or(raw).to_string();
            let fields = split_fields(&line, opts);
            Record { line, fields }
        })
        .collect();
    // A trailing newline yields one empty tail element; GNU does not treat that
    // as a record. An interior blank line is a record with no fields.
    if records.last().is_some_and(|r| r.line.is_empty()) {
        records.pop();
    }
    records
}

fn key_of<'r>(rec: &'r Record, field: usize, ignore_case: bool) -> Option<String> {
    let value = rec.fields.get(field - 1)?;
    Some(if ignore_case {
        value.to_lowercase()
    } else {
        value.clone()
    })
}

/// Whitespace-separated output uses a single space; `-t` uses that character.
fn separator(opts: &JoinOptions) -> String {
    match opts.separator {
        Some(sep) => sep.to_string(),
        None => " ".to_string(),
    }
}

fn check_order_of(
    ctx: &mut UtilContext<'_>,
    recs: &[Record],
    name: &str,
    key_field: usize,
    opts: &JoinOptions,
) -> bool {
    let mut previous: Option<String> = None;
    for (idx, rec) in recs.iter().enumerate() {
        let Some(key) = key_of(rec, key_field, opts.ignore_case) else {
            continue;
        };
        if previous.as_ref().is_some_and(|prev| key < *prev) {
            let msg = format!("join: {name}:{}: is not sorted: {}\n", idx + 1, rec.line);
            ctx.output.stderr(msg.as_bytes());
            return true;
        }
        previous = Some(key);
    }
    false
}

fn emit_join(recs1: &[Record], recs2: &[Record], opts: &JoinOptions) -> String {
    let mut out = String::new();
    let (mut c1, mut c2) = (0usize, 0usize);

    while c1 < recs1.len() && c2 < recs2.len() {
        let (Some(k1), Some(k2)) = (
            key_of(&recs1[c1], opts.field1, opts.ignore_case),
            key_of(&recs2[c2], opts.field2, opts.ignore_case),
        ) else {
            // A blank line has no key and can never pair.
            if key_of(&recs1[c1], opts.field1, opts.ignore_case).is_none() {
                emit_unpairable(&mut out, &recs1[c1], 1, opts);
                c1 += 1;
                continue;
            }
            emit_unpairable(&mut out, &recs2[c2], 2, opts);
            c2 += 1;
            continue;
        };
        match k1.cmp(&k2) {
            std::cmp::Ordering::Less => {
                emit_unpairable(&mut out, &recs1[c1], 1, opts);
                c1 += 1;
            }
            std::cmp::Ordering::Greater => {
                emit_unpairable(&mut out, &recs2[c2], 2, opts);
                c2 += 1;
            }
            std::cmp::Ordering::Equal => {
                let end1 = group_end(recs1, c1, &k1, opts.field1, opts);
                let end2 = group_end(recs2, c2, &k2, opts.field2, opts);
                if !opts.only_unpairable {
                    for left in &recs1[c1..end1] {
                        for right in &recs2[c2..end2] {
                            emit_pair(&mut out, left, right, opts);
                        }
                    }
                }
                c1 = end1;
                c2 = end2;
            }
        }
    }

    while c1 < recs1.len() {
        emit_unpairable(&mut out, &recs1[c1], 1, opts);
        c1 += 1;
    }
    while c2 < recs2.len() {
        emit_unpairable(&mut out, &recs2[c2], 2, opts);
        c2 += 1;
    }
    out
}

fn group_end(
    recs: &[Record],
    start: usize,
    key: &str,
    key_field: usize,
    opts: &JoinOptions,
) -> usize {
    let mut end = start;
    while end < recs.len() {
        match key_of(&recs[end], key_field, opts.ignore_case) {
            Some(k) if k == key => end += 1,
            _ => break,
        }
    }
    end
}

/// The join key as printed: from the first record that has one.
fn join_key(left: &Record, right: &Record, opts: &JoinOptions) -> String {
    left.fields
        .get(opts.field1 - 1)
        .or_else(|| right.fields.get(opts.field2 - 1))
        .cloned()
        .unwrap_or_default()
}

/// Fields of `rec` other than its key, in order.
fn non_key_fields(rec: &Record, key_field: usize) -> impl Iterator<Item = &String> {
    rec.fields
        .iter()
        .enumerate()
        .filter(move |(i, _)| *i != key_field - 1)
        .map(|(_, f)| f)
}

fn emit_pair(out: &mut String, left: &Record, right: &Record, opts: &JoinOptions) {
    let sep = separator(opts);
    if opts.format.is_empty() {
        let mut parts = vec![join_key(left, right, opts)];
        parts.extend(non_key_fields(left, opts.field1).cloned());
        parts.extend(non_key_fields(right, opts.field2).cloned());
        out.push_str(&parts.join(&sep));
    } else {
        let parts: Vec<String> = opts
            .format
            .iter()
            .map(|f| format_field(f, Some(left), Some(right), opts))
            .collect();
        out.push_str(&parts.join(&sep));
    }
    out.push('\n');
}

/// Look up field `field` (1-based) of `rec`, where `key_field` is its key
/// position. The key field maps to the join key; other fields index the
/// non-key projection, so `2.2` is the first non-key field of input 2.
fn field_value(rec: &Record, key_field: usize, field: usize) -> Option<&String> {
    if field == key_field {
        return rec.fields.get(key_field - 1);
    }
    let idx = if field > key_field {
        field - 2
    } else {
        field - 1
    };
    rec.fields
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != key_field - 1)
        .nth(idx)
        .map(|(_, v)| v)
}

fn format_field(
    spec: &OutField,
    left: Option<&Record>,
    right: Option<&Record>,
    opts: &JoinOptions,
) -> String {
    let empty = opts.empty.clone().unwrap_or_default();
    match spec {
        OutField::Join => match (left, right) {
            (Some(l), Some(r)) => join_key(l, r, opts),
            (Some(l), None) => l.fields.get(opts.field1 - 1).cloned().unwrap_or(empty),
            (None, Some(r)) => r.fields.get(opts.field2 - 1).cloned().unwrap_or(empty),
            (None, None) => empty,
        },
        OutField::Input(input, field) => {
            let (rec, key_field) = if *input == 1 {
                (left, opts.field1)
            } else {
                (right, opts.field2)
            };
            match rec.and_then(|r| field_value(r, key_field, *field)) {
                Some(v) => v.clone(),
                None => empty,
            }
        }
    }
}

fn emit_unpairable(out: &mut String, rec: &Record, input: usize, opts: &JoinOptions) {
    let selected = match input {
        1 => opts.unpairable1,
        _ => opts.unpairable2,
    };
    if !selected {
        return;
    }
    let sep = separator(opts);
    if opts.only_unpairable {
        // `-v`: the record verbatim (fields rejoined for whitespace input).
        if opts.separator.is_none() {
            out.push_str(&rec.fields.join(&sep));
        } else {
            out.push_str(&rec.line);
        }
        out.push('\n');
        return;
    }
    if opts.format.is_empty() {
        let key_field = if input == 1 { opts.field1 } else { opts.field2 };
        let mut parts: Vec<String> = Vec::new();
        if let Some(key) = rec.fields.get(key_field - 1) {
            parts.push(key.clone());
        }
        parts.extend(non_key_fields(rec, key_field).cloned());
        out.push_str(&parts.join(&sep));
    } else {
        let (left, right) = if input == 1 {
            (Some(rec), None)
        } else {
            (None, Some(rec))
        };
        let parts: Vec<String> = opts
            .format
            .iter()
            .map(|f| format_field(f, left, right, opts))
            .collect();
        out.push_str(&parts.join(&sep));
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VecOutput;
    use wasmsh_fs::{MemoryFs, OpenOptions, Vfs};

    fn write(fs: &mut MemoryFs, path: &str, text: &str) {
        let h = fs.open(path, OpenOptions::write()).unwrap();
        fs.write_file(h, text.as_bytes()).unwrap();
        fs.close(h);
    }

    fn run(argv: &[&str], f1: &str, f2: &str) -> (i32, String, String) {
        let mut fs = MemoryFs::new();
        write(&mut fs, "/f1", f1);
        write(&mut fs, "/f2", f2);
        let mut output = VecOutput::default();
        let status = {
            let mut ctx = UtilContext {
                fs: &mut fs,
                output: &mut output,
                cwd: "/",
                stdin: None,
                state: None,
                network: None,
                clock: None,
            };
            util_join(&mut ctx, argv)
        };
        (
            status,
            output.stdout_str().to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    const A: &str = "1 a\n2 b\n3 c\n";
    const B: &str = "1 X\n2 Y\n4 Z\n";

    #[test]
    fn inner_join_on_first_field() {
        let (status, out, _) = run(&["join", "/f1", "/f2"], A, B);
        assert_eq!(status, 0);
        assert_eq!(out, "1 a X\n2 b Y\n");
    }

    #[test]
    fn outer_joins() {
        assert_eq!(
            run(&["join", "-a1", "/f1", "/f2"], A, B).1,
            "1 a X\n2 b Y\n3 c\n"
        );
        assert_eq!(
            run(&["join", "-a2", "/f1", "/f2"], A, B).1,
            "1 a X\n2 b Y\n4 Z\n"
        );
    }

    #[test]
    fn only_unpairable() {
        assert_eq!(run(&["join", "-v1", "/f1", "/f2"], A, B).1, "3 c\n");
        assert_eq!(run(&["join", "-v2", "/f1", "/f2"], A, B).1, "4 Z\n");
    }

    #[test]
    fn output_format_and_placeholder() {
        assert_eq!(
            run(&["join", "-o", "1.1,1.2,2.2", "/f1", "/f2"], A, B).1,
            "1 a X\n2 b Y\n"
        );
        assert_eq!(
            run(
                &["join", "-a1", "-e", "NA", "-o", "1.1,2.2", "/f1", "/f2"],
                A,
                B
            )
            .1,
            "1 X\n2 Y\n3 NA\n"
        );
    }

    #[test]
    fn separator_and_alternate_fields() {
        assert_eq!(
            run(&["join", "-t,", "/f1", "/f2"], "a,1\nb,2\n", "a,x\nc,z\n").1,
            "a,1,x\n"
        );
        assert_eq!(
            run(
                &["join", "-1", "2", "-2", "1", "/f1", "/f2"],
                "a 1\nb 2\n",
                "1 P\n2 Q\n"
            )
            .1,
            "1 a P\n2 b Q\n"
        );
    }

    #[test]
    fn duplicate_keys_cross_product() {
        let (_, out, _) = run(
            &["join", "/f1", "/f2"],
            "1 a\n1 b\n2 c\n",
            "1 X\n1 Y\n3 Z\n",
        );
        assert_eq!(out, "1 a X\n1 a Y\n1 b X\n1 b Y\n");
    }

    #[test]
    fn out_of_order_warns_and_fails() {
        let (status, _, err) = run(&["join", "/f1", "/f2"], "2 b\n1 a\n", B);
        assert_eq!(status, 1);
        assert!(err.contains("is not sorted"), "err={err:?}");
    }
}
