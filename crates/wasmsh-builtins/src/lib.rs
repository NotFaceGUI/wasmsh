//! Shell builtin commands for wasmsh.
//!
//! Builtins run in-process and can modify shell state directly.
//! Output goes through an `OutputSink` abstraction suitable for
//! browser streaming.

use std::io::{Cursor, Read};

use indexmap::IndexMap;
use smol_str::SmolStr;
use wasmsh_fs::Vfs;
use wasmsh_state::{ShellState, ShellVar, VarValue};

/// Abstraction for stdout/stderr output, suitable for browser streaming.
pub trait OutputSink {
    fn stdout(&mut self, data: &[u8]);
    fn stderr(&mut self, data: &[u8]);
}

/// An `OutputSink` that collects output into byte vectors (for testing).
#[derive(Debug, Default, Clone)]
pub struct VecSink {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl OutputSink for VecSink {
    fn stdout(&mut self, data: &[u8]) {
        self.stdout.extend_from_slice(data);
    }
    fn stderr(&mut self, data: &[u8]) {
        self.stderr.extend_from_slice(data);
    }
}

impl VecSink {
    #[must_use]
    pub fn stdout_str(&self) -> &str {
        std::str::from_utf8(&self.stdout).unwrap_or("<invalid utf-8>")
    }
    #[must_use]
    pub fn stderr_str(&self) -> &str {
        std::str::from_utf8(&self.stderr).unwrap_or("<invalid utf-8>")
    }
}

pub struct BuiltinStdin<'a> {
    reader: Box<dyn Read + 'a>,
}

impl std::fmt::Debug for BuiltinStdin<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltinStdin").finish_non_exhaustive()
    }
}

impl<'a> BuiltinStdin<'a> {
    #[must_use]
    pub fn from_bytes(data: &'a [u8]) -> Self {
        Self {
            reader: Box::new(Cursor::new(data)),
        }
    }

    #[must_use]
    pub fn from_reader<R>(reader: R) -> Self
    where
        R: Read + 'a,
    {
        Self {
            reader: Box::new(reader),
        }
    }

    pub fn read_chunk(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
    }
}

/// Context passed to builtin implementations.
pub struct BuiltinContext<'a> {
    pub state: &'a mut ShellState,
    pub output: &'a mut dyn OutputSink,
    /// Optional VFS access (needed by `test -f`, etc.).
    pub fs: Option<&'a dyn Vfs>,
    /// Stdin source from pipe or here-doc.
    pub stdin: Option<BuiltinStdin<'a>>,
}

impl std::fmt::Debug for BuiltinContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltinContext").finish_non_exhaustive()
    }
}

impl BuiltinContext<'_> {
    #[must_use]
    pub fn has_stdin(&self) -> bool {
        self.stdin.is_some()
    }
}

/// Signature for a builtin command function.
/// Receives the context and argv (argv\[0\] is the command name).
/// Returns the exit status.
pub type BuiltinFn = fn(&mut BuiltinContext<'_>, &[&str]) -> i32;

/// Registry of builtin commands.
pub struct BuiltinRegistry {
    builtins: IndexMap<&'static str, BuiltinFn>,
}

impl std::fmt::Debug for BuiltinRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltinRegistry")
            .field("count", &self.builtins.len())
            .finish()
    }
}

impl BuiltinRegistry {
    /// Create a registry with all standard builtins.
    #[must_use]
    pub fn new() -> Self {
        let mut builtins = IndexMap::<&'static str, BuiltinFn>::new();
        builtins.insert(":", builtin_colon);
        builtins.insert("true", builtin_true);
        builtins.insert("false", builtin_false);
        builtins.insert("echo", builtin_echo);
        builtins.insert("printf", builtin_printf);
        builtins.insert("pwd", builtin_pwd);
        builtins.insert("cd", builtin_cd);
        builtins.insert("export", builtin_export);
        builtins.insert("unset", builtin_unset);
        builtins.insert("readonly", builtin_readonly);
        builtins.insert("test", builtin_test);
        builtins.insert("[", builtin_test);
        builtins.insert("read", builtin_read);
        builtins.insert("shift", builtin_shift);
        builtins.insert("return", builtin_return);
        builtins.insert("exit", builtin_exit);
        builtins.insert("local", builtin_local);
        builtins.insert("type", builtin_type);
        builtins.insert("command", builtin_command);
        builtins.insert("eval", builtin_eval);
        builtins.insert("set", builtin_set);
        builtins.insert("getopts", builtin_getopts);
        builtins.insert("trap", builtin_trap);
        Self { builtins }
    }

    /// Look up a builtin by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<BuiltinFn> {
        self.builtins.get(name).copied()
    }

    /// Check if a name is a builtin.
    #[must_use]
    pub fn is_builtin(&self, name: &str) -> bool {
        self.builtins.contains_key(name)
    }
}

impl Default for BuiltinRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---- Builtin implementations ----

/// `:` — no-op, always returns 0.
fn builtin_colon(_ctx: &mut BuiltinContext<'_>, _argv: &[&str]) -> i32 {
    0
}

/// `true` — always returns 0.
fn builtin_true(_ctx: &mut BuiltinContext<'_>, _argv: &[&str]) -> i32 {
    0
}

/// `false` — always returns 1.
fn builtin_false(_ctx: &mut BuiltinContext<'_>, _argv: &[&str]) -> i32 {
    1
}

/// `echo` — print arguments separated by spaces.
/// Supports `-n` to suppress trailing newline and `-e` for escape interpretation.
fn builtin_echo(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    let args = &argv[1..];
    let mut suppress_newline = false;
    let mut interpret_escapes = false;
    let mut start = 0;

    for (i, arg) in args.iter().enumerate() {
        // Each flag-like argument must consist entirely of valid echo flags
        let bytes = arg.as_bytes();
        if bytes.first() != Some(&b'-') || bytes.len() < 2 {
            break;
        }
        let all_flags = bytes[1..].iter().all(|b| matches!(b, b'n' | b'e'));
        if !all_flags {
            break;
        }
        for &b in &bytes[1..] {
            match b {
                b'n' => suppress_newline = true,
                b'e' => interpret_escapes = true,
                _ => {}
            }
        }
        start = i + 1;
    }

    let text = args[start..].join(" ");
    if interpret_escapes {
        let processed = process_echo_escapes(&text);
        ctx.output.stdout(processed.as_bytes());
    } else {
        ctx.output.stdout(text.as_bytes());
    }
    if !suppress_newline {
        ctx.output.stdout(b"\n");
    }
    0
}

/// Parse an octal escape `\0NNN` (up to 3 octal digits).
fn parse_echo_octal(bytes: &[u8], mut i: usize) -> (char, usize) {
    let mut val: u8 = 0;
    let mut count = 0;
    while i < bytes.len() && count < 3 && bytes[i] >= b'0' && bytes[i] <= b'7' {
        val = val * 8 + (bytes[i] - b'0');
        i += 1;
        count += 1;
    }
    (val as char, i)
}

fn process_echo_escapes(s: &str) -> String {
    let mut result = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' => result.push('\n'),
                b't' => result.push('\t'),
                b'\\' => result.push('\\'),
                b'a' => result.push('\x07'),
                b'b' => result.push('\x08'),
                b'r' => result.push('\r'),
                b'0' => {
                    let (ch, new_i) = parse_echo_octal(bytes, i + 2);
                    result.push(ch);
                    i = new_i;
                    continue;
                }
                _ => {
                    result.push('\\');
                    result.push(bytes[i + 1] as char);
                }
            }
            i += 2;
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

/// `printf` — formatted output.
/// Supports: `%s`, `%d`, `%x`, `%o`, `%f`, `%c`, `%b`, `%q`, `%%`, width/precision,
/// left-align (`%-`), zero-pad (`%0`), and `\n`, `\t`, `\\` escape sequences.
/// Repeats the format string while there are remaining arguments (POSIX behavior).
fn builtin_printf(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    // A leading `--` ends option processing (bash compatibility); without this
    // `printf -- '%s\n' x` prints the literal format `--`.
    let argv = match argv {
        [prog, rest @ ..] if matches!(rest.first(), Some(&"--")) => {
            let mut trimmed: Vec<&str> = Vec::with_capacity(argv.len() - 1);
            trimmed.push(*prog);
            trimmed.extend_from_slice(&rest[1..]);
            trimmed
        }
        _ => argv.to_vec(),
    };
    if argv.len() < 2 {
        ctx.output
            .stderr(b"printf: usage: printf format [arguments]\n");
        return 1;
    }

    let format = argv[1];
    let args = &argv[2..];
    let mut arg_idx = 0;
    let mut output = String::new();
    let bytes = format.as_bytes();

    loop {
        let start_arg_idx = arg_idx;
        printf_format_once(bytes, args, &mut arg_idx, &mut output);
        if arg_idx == start_arg_idx || arg_idx >= args.len() {
            break;
        }
    }

    ctx.output.stdout(output.as_bytes());
    0
}

/// Process one pass of a printf format string.
fn printf_format_once(bytes: &[u8], args: &[&str], arg_idx: &mut usize, output: &mut String) {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'%' {
                output.push('%');
                i += 2;
                continue;
            }
            i += 1;
            i = printf_format_spec(bytes, i, args, arg_idx, output);
        } else if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i = printf_escape(bytes, i, output);
        } else {
            output.push(bytes[i] as char);
            i += 1;
        }
    }
}

/// Parse printf format flags (`-` for left-align, `0` for zero-pad).
/// Returns `(left_align, zero_pad, new_position)`.
fn printf_parse_flags(bytes: &[u8], mut i: usize) -> (bool, bool, usize) {
    let mut left_align = false;
    let mut zero_pad = false;
    loop {
        if i < bytes.len() && bytes[i] == b'-' {
            left_align = true;
            i += 1;
        } else if i < bytes.len() && bytes[i] == b'0' && !left_align {
            zero_pad = true;
            i += 1;
        } else {
            break;
        }
    }
    (left_align, zero_pad, i)
}

/// Parse a run of ASCII digits as a `usize`, returning `(value, new_position)`.
fn printf_parse_digits(bytes: &[u8], mut i: usize) -> (usize, usize) {
    let mut value: usize = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        value = value * 10 + (bytes[i] - b'0') as usize;
        i += 1;
    }
    (value, i)
}

/// If the next byte is `.`, parse the precision digits that follow.
fn printf_parse_precision(bytes: &[u8], mut i: usize) -> (Option<usize>, usize) {
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let (prec, new_i) = printf_parse_digits(bytes, i);
        (Some(prec), new_i)
    } else {
        (None, i)
    }
}

/// Parse and apply a single printf format specifier starting after the `%`.
/// Returns the new position in the format byte string.
fn printf_format_spec(
    bytes: &[u8],
    mut i: usize,
    args: &[&str],
    arg_idx: &mut usize,
    output: &mut String,
) -> usize {
    let (left_align, zero_pad, pos) = printf_parse_flags(bytes, i);
    i = pos;
    let (width, pos) = printf_parse_digits(bytes, i);
    i = pos;
    let (precision, pos) = printf_parse_precision(bytes, i);
    i = pos;
    if i >= bytes.len() {
        output.push('%');
        return i;
    }
    let conv = bytes[i];
    i += 1;
    let arg_str = args.get(*arg_idx).copied().unwrap_or("");
    let formatted = printf_convert(conv, arg_str, precision, arg_idx);
    printf_apply_width(output, &formatted, width, left_align, zero_pad);
    i
}

/// Convert a single printf format conversion character.
fn printf_convert(
    conv: u8,
    arg_str: &str,
    precision: Option<usize>,
    arg_idx: &mut usize,
) -> String {
    match conv {
        b's' => {
            *arg_idx += 1;
            let s = precision.map_or(arg_str, |prec| {
                if prec < arg_str.len() {
                    &arg_str[..prec]
                } else {
                    arg_str
                }
            });
            s.to_string()
        }
        b'd' => {
            *arg_idx += 1;
            arg_str.parse::<i64>().unwrap_or(0).to_string()
        }
        b'x' => {
            *arg_idx += 1;
            format!("{:x}", arg_str.parse::<i64>().unwrap_or(0))
        }
        b'o' => {
            *arg_idx += 1;
            format!("{:o}", arg_str.parse::<i64>().unwrap_or(0))
        }
        b'f' => {
            *arg_idx += 1;
            let val: f64 = arg_str.parse().unwrap_or(0.0);
            let prec = precision.unwrap_or(6);
            format!("{val:.prec$}")
        }
        b'c' => {
            *arg_idx += 1;
            arg_str
                .chars()
                .next()
                .map_or(String::new(), |c| c.to_string())
        }
        b'b' => {
            *arg_idx += 1;
            process_printf_backslash_escapes(arg_str)
        }
        b'q' => {
            *arg_idx += 1;
            shell_quote(arg_str)
        }
        _ => format!("%{}", conv as char),
    }
}

/// Apply width and alignment to a formatted string.
fn printf_apply_width(
    output: &mut String,
    formatted: &str,
    width: usize,
    left_align: bool,
    zero_pad: bool,
) {
    if width == 0 || formatted.len() >= width {
        output.push_str(formatted);
        return;
    }

    let pad_char = if zero_pad && !left_align { '0' } else { ' ' };
    let padding = width - formatted.len();
    if left_align {
        output.push_str(formatted);
        push_repeated_char(output, ' ', padding);
    } else {
        push_repeated_char(output, pad_char, padding);
        output.push_str(formatted);
    }
}

fn push_repeated_char(output: &mut String, ch: char, count: usize) {
    for _ in 0..count {
        output.push(ch);
    }
}

/// Process a backslash escape in a printf format string. Returns new position.
fn printf_escape(bytes: &[u8], i: usize, output: &mut String) -> usize {
    match bytes[i + 1] {
        b'n' => {
            output.push('\n');
            i + 2
        }
        b't' => {
            output.push('\t');
            i + 2
        }
        b'\\' => {
            output.push('\\');
            i + 2
        }
        b'r' => {
            output.push('\r');
            i + 2
        }
        b'a' => {
            output.push('\x07');
            i + 2
        }
        b'b' => {
            output.push('\x08');
            i + 2
        }
        b'f' => {
            output.push('\x0c');
            i + 2
        }
        b'v' => {
            output.push('\x0b');
            i + 2
        }
        // `\NNN` — one to three octal digits, leading `0` not required.
        b'0'..=b'7' => {
            let (ch, new_i) = parse_echo_octal(bytes, i + 1);
            output.push(ch);
            new_i
        }
        // `\xNN` — one or two hex digits.
        b'x' => {
            let (ch, new_i) = parse_hex_escape(bytes, i + 2);
            output.push(ch);
            new_i
        }
        _ => {
            output.push('\\');
            i + 1
        }
    }
}

/// Parse up to two hex digits after `\x`, returning `(char, new_index)`.
fn parse_hex_escape(bytes: &[u8], mut i: usize) -> (char, usize) {
    let mut val: u8 = 0;
    let mut count = 0;
    while i < bytes.len() && count < 2 {
        let digit = match bytes[i] {
            b'0'..=b'9' => bytes[i] - b'0',
            b'a'..=b'f' => bytes[i] - b'a' + 10,
            b'A'..=b'F' => bytes[i] - b'A' + 10,
            _ => break,
        };
        val = val * 16 + digit;
        i += 1;
        count += 1;
    }
    (val as char, i)
}

/// Process backslash escape sequences for `%b` in printf.
fn process_printf_backslash_escapes(s: &str) -> String {
    let mut result = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' => result.push('\n'),
                b't' => result.push('\t'),
                b'\\' => result.push('\\'),
                b'a' => result.push('\x07'),
                b'b' => result.push('\x08'),
                b'r' => result.push('\r'),
                b'f' => result.push('\x0c'),
                b'v' => result.push('\x0b'),
                // `%b` octal has two forms: `\NNN` (up to 3 digits) and
                // `\0NNN` (a leading `0` that does not count toward the three).
                // So `\101` and `\0101` are both `A`, while `\010` is `\b`.
                b'0' => {
                    let (ch, new_i) = parse_echo_octal(bytes, i + 2);
                    result.push(ch);
                    i = new_i;
                    continue;
                }
                b'1'..=b'7' => {
                    let (ch, new_i) = parse_echo_octal(bytes, i + 1);
                    result.push(ch);
                    i = new_i;
                    continue;
                }
                b'x' => {
                    let (ch, new_i) = parse_hex_escape(bytes, i + 2);
                    result.push(ch);
                    i = new_i;
                    continue;
                }
                _ => {
                    result.push('\\');
                    result.push(bytes[i + 1] as char);
                }
            }
            i += 2;
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

/// Shell-quote a string for `%q` in printf: wrap in $'...' with escapes.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    // Check if the string needs quoting
    let needs_quoting = s
        .bytes()
        .any(|b| !b.is_ascii_alphanumeric() && !matches!(b, b'_' | b'-' | b'.' | b'/' | b':'));
    if !needs_quoting {
        return s.to_string();
    }
    let mut result = String::from("$'");
    for ch in s.chars() {
        match ch {
            '\'' => result.push_str("\\'"),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\t' => result.push_str("\\t"),
            '\r' => result.push_str("\\r"),
            '\x07' => result.push_str("\\a"),
            '\x08' => result.push_str("\\b"),
            _ => result.push(ch),
        }
    }
    result.push('\'');
    result
}

/// `pwd` — print working directory.
fn builtin_pwd(ctx: &mut BuiltinContext<'_>, _argv: &[&str]) -> i32 {
    ctx.output.stdout(ctx.state.cwd.as_bytes());
    ctx.output.stdout(b"\n");
    0
}

/// `cd` — change working directory.
/// - `cd` (no args): go to HOME
/// - `cd -`: go to OLDPWD
/// - `cd DIR`: set cwd to DIR
fn builtin_cd(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    let target = if argv.len() < 2 {
        // cd with no args → HOME
        if let Some(home) = ctx.state.get_var("HOME") {
            home.to_string()
        } else {
            ctx.output.stderr(b"cd: HOME not set\n");
            return 1;
        }
    } else if argv[1] == "-" {
        if let Some(old) = ctx.state.get_var("OLDPWD") {
            let s = old.to_string();
            ctx.output.stdout(s.as_bytes());
            ctx.output.stdout(b"\n");
            s
        } else {
            ctx.output.stderr(b"cd: OLDPWD not set\n");
            return 1;
        }
    } else {
        argv[1].to_string()
    };

    let old_pwd = ctx.state.cwd.clone();
    ctx.state.cwd = target;
    ctx.state.set_var("OLDPWD".into(), SmolStr::from(old_pwd));
    ctx.state
        .set_var("PWD".into(), SmolStr::from(ctx.state.cwd.as_str()));
    0
}

/// `export` — mark variables as exported.
/// - `export NAME=VALUE`: set and export
/// - `export NAME`: export existing variable
fn builtin_export(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    let mut status = 0;
    let mut print = false;
    let mut unexport = false;
    let mut args = Vec::new();

    for arg in &argv[1..] {
        match *arg {
            "-p" => print = true,
            "-n" => unexport = true,
            flag if flag.starts_with('-') && flag.len() > 1 => {}
            _ => args.push(*arg),
        }
    }

    if print || args.is_empty() {
        print_exported_vars(ctx);
        if args.is_empty() {
            return 0;
        }
    }

    for arg in args {
        if unexport {
            status |= i32::from(!export_clear(ctx, arg));
        } else if let Some(eq_pos) = arg.find('=') {
            export_with_value(ctx, &arg[..eq_pos], &arg[eq_pos + 1..]);
        } else {
            export_name_only(ctx, arg);
        }
    }
    status
}

fn print_exported_vars(ctx: &mut BuiltinContext<'_>) {
    let mut entries = IndexMap::<SmolStr, SmolStr>::new();
    for scope in &ctx.state.env.scopes {
        for (name, var) in scope {
            if var.exported {
                entries.insert(name.clone(), var.value.as_scalar());
            }
        }
    }
    for (name, value) in entries {
        let line = format!("declare -x {name}=\"{value}\"\n");
        ctx.output.stdout(line.as_bytes());
    }
}

fn export_clear(ctx: &mut BuiltinContext<'_>, name: &str) -> bool {
    let Some(var) = ctx.state.env.get_mut(name) else {
        return false;
    };
    var.exported = false;
    true
}

fn export_with_value(ctx: &mut BuiltinContext<'_>, name: &str, value: &str) {
    if let Some(existing) = ctx.state.env.get(name) {
        if existing.readonly {
            let msg = format!("export: {name}: readonly variable\n");
            ctx.output.stderr(msg.as_bytes());
            return;
        }
    }
    ctx.state.env.set(
        SmolStr::from(name),
        ShellVar {
            value: VarValue::Scalar(SmolStr::from(value)),
            exported: true,
            readonly: false,
            integer: false,
            nameref: false,
        },
    );
}

fn export_name_only(ctx: &mut BuiltinContext<'_>, name: &str) {
    if let Some(var) = ctx.state.env.get(name) {
        let mut var = var.clone();
        var.exported = true;
        ctx.state.env.set(SmolStr::from(name), var);
    } else {
        ctx.state.env.set(
            SmolStr::from(name),
            ShellVar {
                value: VarValue::Scalar(SmolStr::default()),
                exported: true,
                readonly: false,
                integer: false,
                nameref: false,
            },
        );
    }
}

/// `unset` — remove variables from the environment.
/// Supports `unset 'arr[N]'` to remove a single array element.
fn builtin_unset(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    let mut status = 0;
    for name in &argv[1..] {
        // Check for array element syntax: name[index]
        if let Some(bracket_pos) = name.find('[') {
            if name.ends_with(']') {
                let base = &name[..bracket_pos];
                let index = &name[bracket_pos + 1..name.len() - 1];
                ctx.state.unset_array_element(base, index);
                continue;
            }
        }
        if let Err(e) = ctx.state.unset_var(name) {
            let msg = format!("unset: {e}\n");
            ctx.output.stderr(msg.as_bytes());
            status = 1;
        }
    }
    status
}

/// `readonly` — mark variables as readonly.
/// - `readonly NAME=VALUE`: set and mark readonly
/// - `readonly NAME`: mark existing variable readonly
fn builtin_readonly(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    let mut print = false;
    let mut args = Vec::new();
    for arg in &argv[1..] {
        match *arg {
            "-p" => print = true,
            flag if flag.starts_with('-') && flag.len() > 1 => {}
            _ => args.push(*arg),
        }
    }

    if print || args.is_empty() {
        print_readonly_vars(ctx);
        if args.is_empty() {
            return 0;
        }
    }

    for arg in args {
        if let Some(eq_pos) = arg.find('=') {
            let name = &arg[..eq_pos];
            let value = &arg[eq_pos + 1..];
            ctx.state
                .set_readonly(SmolStr::from(name), SmolStr::from(value));
        } else {
            // Mark existing variable readonly
            let value = ctx.state.get_var(arg).unwrap_or_default();
            ctx.state.set_readonly(SmolStr::from(arg), value);
        }
    }
    0
}

fn print_readonly_vars(ctx: &mut BuiltinContext<'_>) {
    let mut entries = IndexMap::<SmolStr, SmolStr>::new();
    for scope in &ctx.state.env.scopes {
        for (name, var) in scope {
            if var.readonly {
                entries.insert(name.clone(), var.value.as_scalar());
            }
        }
    }
    for (name, value) in entries {
        let line = format!("declare -r {name}=\"{value}\"\n");
        ctx.output.stdout(line.as_bytes());
    }
}

/// `test` / `[` — conditional expression evaluation.
fn builtin_test(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    let args: Vec<&str> = if argv.first() == Some(&"[") {
        if argv.last() != Some(&"]") {
            ctx.output.stderr(b"[: missing ']'\n");
            return 2;
        }
        argv[1..argv.len() - 1].to_vec()
    } else {
        argv[1..].to_vec()
    };

    if args.is_empty() {
        return 1;
    }

    i32::from(!test_check(&args, ctx))
}

fn test_check(args: &[&str], ctx: &BuiltinContext<'_>) -> bool {
    // Handle `!` prefix at any arg count (e.g. `! -f /path`, `! "a" = "b"`)
    if !args.is_empty() && args[0] == "!" {
        return !test_check(&args[1..], ctx);
    }
    match args.len() {
        1 => !args[0].is_empty(),
        2 => test_unary(args[0], args[1], ctx),
        3 => test_binary(args[0], args[1], args[2]),
        _ => false,
    }
}

fn test_unary(op: &str, val: &str, ctx: &BuiltinContext<'_>) -> bool {
    match op {
        "-n" => !val.is_empty(),
        "-z" | "!" => val.is_empty(),
        "-f" => ctx
            .fs
            .is_some_and(|fs| fs.stat(val).is_ok_and(|m| !m.is_dir)),
        "-d" => ctx
            .fs
            .is_some_and(|fs| fs.stat(val).is_ok_and(|m| m.is_dir)),
        "-e" => ctx.fs.is_some_and(|fs| fs.stat(val).is_ok()),
        "-s" => ctx
            .fs
            .is_some_and(|fs| fs.stat(val).is_ok_and(|m| m.size > 0)),
        "-O" | "-G" => ctx.fs.is_some_and(|fs| fs.stat(val).is_ok()),
        "-N" => ctx
            .fs
            .is_some_and(|fs| fs.stat(val).is_ok_and(|m| m.size > 0)),
        "-t" => val == "0" && ctx.stdin.is_some(),
        // These used to answer "does it exist", which made `test -r` on a
        // `chmod 000` file report success and any script gating on it take
        // the wrong branch.
        "-r" => ctx
            .fs
            .is_some_and(|fs| fs.stat(val).is_ok_and(|m| m.is_readable())),
        "-w" => ctx
            .fs
            .is_some_and(|fs| fs.stat(val).is_ok_and(|m| m.is_writable())),
        "-x" => ctx
            .fs
            .is_some_and(|fs| fs.stat(val).is_ok_and(|m| m.is_executable())),
        _ => false,
    }
}

fn test_binary(left: &str, op: &str, right: &str) -> bool {
    match op {
        "!=" => left != right,
        "=" | "==" | "-ef" => left == right,
        "-nt" => !left.is_empty() && right.is_empty(),
        "-ot" => left.is_empty() && !right.is_empty(),
        "-eq" => int(left) == int(right),
        "-ne" => int(left) != int(right),
        "-lt" => int(left) < int(right),
        "-gt" => int(left) > int(right),
        "-le" => int(left) <= int(right),
        "-ge" => int(left) >= int(right),
        _ => false,
    }
}

fn int(s: &str) -> i64 {
    s.trim().parse().unwrap_or(0)
}

/// `shift` — shift positional parameters left by N (default 1).
fn builtin_shift(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    let n: usize = argv.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
    if n > ctx.state.positional.len() {
        ctx.output.stderr(b"shift: shift count out of range\n");
        return 1;
    }
    ctx.state.positional = ctx.state.positional[n..].to_vec();
    0
}

/// `return` — return from a function with optional status.
/// In our model this just sets the exit status; the function body
/// execution loop in `WorkerRuntime` checks it.
fn builtin_return(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    argv.get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(ctx.state.last_status)
}

/// `exit` — exit the shell with optional status.
fn builtin_exit(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    argv.get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(ctx.state.last_status)
}

/// `local` — declare local variables (in function scope).
/// Runtime uses save/restore stack for function-local variables.
/// `local VAR=val` sets the variable in the current scope.
fn builtin_local(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    for arg in &argv[1..] {
        if let Some(eq_pos) = arg.find('=') {
            let name = &arg[..eq_pos];
            let value = &arg[eq_pos + 1..];
            ctx.state.set_var(SmolStr::from(name), SmolStr::from(value));
        } else {
            // Declare without value
            ctx.state.set_var(SmolStr::from(*arg), SmolStr::default());
        }
    }
    0
}

/// `type` — display information about command type.
fn builtin_type(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    let registry = BuiltinRegistry::new();
    let mut status = 0;
    for name in &argv[1..] {
        if registry.is_builtin(name) {
            let msg = format!("{name} is a shell builtin\n");
            ctx.output.stdout(msg.as_bytes());
        } else {
            let msg = format!("{name}: not found\n");
            ctx.output.stderr(msg.as_bytes());
            status = 1;
        }
    }
    status
}

/// `command` — execute command, bypassing functions. `-v` shows type.
fn builtin_command(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    let args = &argv[1..];
    if args.first() == Some(&"-v") {
        let registry = BuiltinRegistry::new();
        for name in &args[1..] {
            if registry.is_builtin(name) {
                let msg = format!("{name}\n");
                ctx.output.stdout(msg.as_bytes());
            } else {
                return 1;
            }
        }
        return 0;
    }
    // Without -v, command just runs the command (function bypass handled at runtime level)
    0
}

/// `eval` — evaluate arguments as shell code.
/// Intercepted at runtime level, not a placeholder. The runtime re-parses
/// and executes the concatenated arguments directly.
fn builtin_eval(_ctx: &mut BuiltinContext<'_>, _argv: &[&str]) -> i32 {
    // Actual eval is handled in WorkerRuntime by re-parsing the concatenated args.
    // The runtime intercepts "eval" before reaching this builtin.
    0
}

/// `set` — set shell options or positional parameters.
const SET_OPTIONS: &[(&str, &str)] = &[
    ("allexport", "SHOPT_a"),
    ("errexit", "SHOPT_e"),
    ("errtrace", "SHOPT_E"),
    ("functrace", "SHOPT_T"),
    ("noclobber", "SHOPT_C"),
    ("noglob", "SHOPT_f"),
    ("noexec", "SHOPT_n"),
    ("nounset", "SHOPT_u"),
    ("pipefail", "SHOPT_o_pipefail"),
    ("privileged", "SHOPT_p"),
    ("verbose", "SHOPT_v"),
    ("xtrace", "SHOPT_x"),
];

/// Map a long option name (used with `-o`/`+o`) to its short-flag equivalent.
/// Returns the `SHOPT_*` variable name for the option.
fn set_long_option_var(name: &str) -> Option<&'static str> {
    SET_OPTIONS
        .iter()
        .find_map(|(opt, var)| (*opt == name).then_some(*var))
}

fn print_set_option_table(ctx: &mut BuiltinContext<'_>) {
    for (name, var) in SET_OPTIONS {
        let enabled = ctx.state.get_var(var).as_deref() == Some("1");
        let status = if enabled { "on" } else { "off" };
        let line = format!("{name:<12} {status}\n");
        ctx.output.stdout(line.as_bytes());
    }
}

fn print_set_option_commands(ctx: &mut BuiltinContext<'_>) {
    for (name, var) in SET_OPTIONS {
        let enabled = ctx.state.get_var(var).as_deref() == Some("1");
        let prefix = if enabled { "-" } else { "+" };
        let line = format!("set {prefix}o {name}\n");
        ctx.output.stdout(line.as_bytes());
    }
}

fn builtin_set(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    let args = &argv[1..];
    if args.is_empty() {
        return 0;
    }
    if args[0] == "--" {
        ctx.state.positional = args[1..].iter().map(|s| SmolStr::from(*s)).collect();
        return 0;
    }
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if (arg.starts_with('-') || arg.starts_with('+')) && arg.len() > 1 {
            let enable = arg.starts_with('-');
            set_parse_option(ctx, args, &mut i, enable);
        }
        i += 1;
    }
    0
}

/// Parse a single `set` option flag and apply it.
fn set_parse_option(ctx: &mut BuiltinContext<'_>, args: &[&str], i: &mut usize, enable: bool) {
    let flags = &args[*i][1..];
    let val = if enable { "1" } else { "0" };
    if flags == "o" {
        if *i + 1 < args.len() {
            *i += 1;
            if let Some(var) = set_long_option_var(args[*i]) {
                ctx.state.set_var(SmolStr::from(var), SmolStr::from(val));
            } else {
                let msg = format!("set: unrecognized option: {}\n", args[*i]);
                ctx.output.stderr(msg.as_bytes());
            }
        } else if enable {
            print_set_option_table(ctx);
        } else {
            print_set_option_commands(ctx);
        }
    } else {
        for c in flags.chars() {
            let opt_name = format!("SHOPT_{c}");
            ctx.state
                .set_var(SmolStr::from(opt_name.as_str()), SmolStr::from(val));
        }
    }
}

/// `getopts` — parse positional parameters for options.
///
/// Supports clustered options (`-ab`), attached arguments (`-bval`),
/// separate arguments (`-b val`), `--` termination, and the leading `:`
/// silent mode. `OPTARG`/`OPTIND` follow bash semantics.
fn builtin_getopts(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    if argv.len() < 3 {
        ctx.output
            .stderr(b"getopts: usage: getopts optstring name\n");
        return 2;
    }
    let optstring = argv[1];
    let var_name = argv[2];

    let explicit_args: Vec<String> = if argv.len() > 3 {
        argv[3..].iter().map(|s| (*s).to_string()).collect()
    } else {
        ctx.state
            .positional
            .iter()
            .map(ToString::to_string)
            .collect()
    };
    let args: Vec<&str> = explicit_args.iter().map(String::as_str).collect();

    let silent = optstring.starts_with(':');
    let optind = ctx
        .state
        .get_var("OPTIND")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);

    // Resume the current cluster only if OPTIND was not changed by the user.
    let stored_ind = ctx
        .state
        .get_var("_GETOPTS_OPTIND")
        .and_then(|v| v.parse::<usize>().ok());
    let mut offset = if stored_ind == Some(optind) {
        ctx.state
            .get_var("_GETOPTS_OFFSET")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1)
    } else {
        1
    };
    let mut optind = optind;

    let result = loop {
        if optind > args.len() {
            break None;
        }
        let arg = args[optind - 1];
        if offset <= 1 {
            if arg == "--" {
                optind += 1;
                break None;
            }
            if !arg.starts_with('-') || arg == "-" {
                break None;
            }
        }
        let chars: Vec<char> = arg.chars().collect();
        if offset >= chars.len() {
            optind += 1;
            offset = 1;
            continue;
        }
        break Some(chars);
    };

    let Some(chars) = result else {
        ctx.state.set_var(
            SmolStr::from("OPTIND"),
            SmolStr::from(optind.to_string().as_str()),
        );
        clear_getopts_state(ctx.state);
        return 1;
    };

    let c = chars[offset];
    let spec: Vec<char> = optstring.chars().collect();
    let found = spec.iter().position(|&s| s == c);
    let requires_arg = found.is_some_and(|idx| spec.get(idx + 1) == Some(&':'));
    let consumed_this_arg = offset + 1 >= chars.len();

    if found.is_none() {
        // Unknown option.
        if !silent {
            let shell = ctx
                .state
                .script_name
                .clone()
                .unwrap_or_else(|| SmolStr::from("wasmsh"));
            ctx.output
                .stderr(format!("{shell}: illegal option -- {c}\n").as_bytes());
            ctx.state.unset_var("OPTARG").ok();
        } else {
            ctx.state.set_var(
                SmolStr::from("OPTARG"),
                SmolStr::from(c.to_string().as_str()),
            );
        }
        ctx.state
            .set_var(SmolStr::from(var_name), SmolStr::from("?"));
        if consumed_this_arg {
            optind += 1;
            offset = 1;
        } else {
            offset += 1;
        }
    } else if requires_arg {
        let rest: String = chars[offset + 1..].iter().collect();
        if !rest.is_empty() {
            ctx.state
                .set_var(SmolStr::from("OPTARG"), SmolStr::from(rest.as_str()));
            optind += 1;
            offset = 1;
        } else if optind < args.len() {
            ctx.state
                .set_var(SmolStr::from("OPTARG"), SmolStr::from(args[optind]));
            optind += 2;
            offset = 1;
        } else {
            // Missing required argument.
            if silent {
                ctx.state.set_var(
                    SmolStr::from("OPTARG"),
                    SmolStr::from(c.to_string().as_str()),
                );
                ctx.state
                    .set_var(SmolStr::from(var_name), SmolStr::from(":"));
            } else {
                let shell = ctx
                    .state
                    .script_name
                    .clone()
                    .unwrap_or_else(|| SmolStr::from("wasmsh"));
                ctx.output
                    .stderr(format!("{shell}: option requires an argument -- {c}\n").as_bytes());
                ctx.state.unset_var("OPTARG").ok();
                ctx.state
                    .set_var(SmolStr::from(var_name), SmolStr::from("?"));
            }
            optind += 1;
            ctx.state.set_var(
                SmolStr::from("OPTIND"),
                SmolStr::from(optind.to_string().as_str()),
            );
            ctx.state.set_var(
                SmolStr::from("_GETOPTS_OPTIND"),
                SmolStr::from(optind.to_string().as_str()),
            );
            ctx.state
                .set_var(SmolStr::from("_GETOPTS_OFFSET"), SmolStr::from("1"));
            return 0;
        }
        ctx.state.set_var(
            SmolStr::from(var_name),
            SmolStr::from(c.to_string().as_str()),
        );
    } else {
        ctx.state.set_var(
            SmolStr::from(var_name),
            SmolStr::from(c.to_string().as_str()),
        );
        ctx.state.unset_var("OPTARG").ok();
        if consumed_this_arg {
            optind += 1;
            offset = 1;
        } else {
            offset += 1;
        }
    }

    ctx.state.set_var(
        SmolStr::from("OPTIND"),
        SmolStr::from(optind.to_string().as_str()),
    );
    ctx.state.set_var(
        SmolStr::from("_GETOPTS_OPTIND"),
        SmolStr::from(optind.to_string().as_str()),
    );
    ctx.state.set_var(
        SmolStr::from("_GETOPTS_OFFSET"),
        SmolStr::from(offset.to_string().as_str()),
    );
    0
}

fn clear_getopts_state(state: &mut ShellState) {
    state.unset_var("_GETOPTS_OPTIND").ok();
    state.unset_var("_GETOPTS_OFFSET").ok();
}

/// Parsed options for the `read` builtin.
struct ReadOpts<'a> {
    prompt: Option<&'a str>,
    delimiter: char,
    nchars: Option<usize>,
    exact_nchars: Option<usize>,
    array_name: Option<&'a str>,
    fd: Option<u32>,
    /// `-r`: a backslash is an ordinary character rather than an escape.
    raw: bool,
    remaining_args: &'a [&'a str],
}

/// Parse `read` builtin flags, returning parsed options.
fn parse_read_opts<'a>(argv: &'a [&'a str]) -> ReadOpts<'a> {
    let mut args = &argv[1..];
    let mut opts = ReadOpts {
        prompt: None,
        delimiter: '\n',
        nchars: None,
        exact_nchars: None,
        array_name: None,
        fd: None,
        raw: false,
        remaining_args: &[],
    };
    while let Some(arg) = args.first() {
        match *arg {
            "-r" => {
                opts.raw = true;
                args = &args[1..];
            }
            "-s" | "-e" => args = &args[1..],
            "-p" => {
                opts.prompt = take_read_opt_value(&mut args);
            }
            "-d" => {
                if let Some(value) = take_read_opt_value(&mut args) {
                    opts.delimiter = value.chars().next().unwrap_or('\n');
                }
            }
            "-n" => {
                opts.nchars = take_read_opt_value(&mut args).and_then(|value| value.parse().ok());
            }
            "-N" => {
                opts.exact_nchars =
                    take_read_opt_value(&mut args).and_then(|value| value.parse().ok());
            }
            "-a" => {
                opts.array_name = take_read_opt_value(&mut args);
            }
            "-u" => {
                opts.fd = take_read_opt_value(&mut args).and_then(|value| value.parse().ok());
            }
            "-i" | "-t" => drop(take_read_opt_value(&mut args)),
            _ => break,
        }
    }
    opts.remaining_args = args;
    opts
}

fn take_read_opt_value<'a>(args: &mut &'a [&'a str]) -> Option<&'a str> {
    if args.len() > 1 {
        let value = args[1];
        *args = &args[2..];
        Some(value)
    } else {
        *args = &args[1..];
        None
    }
}

/// `read` — read a line from stdin into variable(s).
/// Supports: `-r` (no backslash interpretation), `-p prompt`, `-d delim`,
/// `-n nchars`, `-N nchars`, `-a array`, `-t timeout`, `-s` (silent).
fn builtin_read(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    let opts = parse_read_opts(argv);
    if opts.fd.is_some_and(|fd| fd != 0) {
        ctx.output
            .stderr(b"read: only file descriptor 0 is supported\n");
        return 1;
    }
    emit_read_prompt(ctx, opts.prompt);
    let var_names = read_var_names(&opts);
    let Some((line, remaining, found_delimiter)) = read_input(ctx, &opts) else {
        return 1;
    };
    // Without `-r`, `\` escapes the following character (including IFS
    // characters) and is removed. With `-r` the line is taken verbatim.
    let line = if opts.raw {
        line
    } else {
        unescape_read_line(&line)
    };

    store_read_remaining(ctx, &remaining);
    if let Some(arr_name) = opts.array_name {
        read_into_array(ctx.state, &line, arr_name);
    } else {
        read_assign_vars(ctx.state, &line, &var_names);
    }
    // A line terminated by the delimiter succeeds; EOF without a delimiter
    // still assigns the partial line but reports failure (bash semantics).
    i32::from(!found_delimiter)
}

fn emit_read_prompt(ctx: &mut BuiltinContext<'_>, prompt: Option<&str>) {
    if let Some(prompt) = prompt {
        ctx.output.stderr(prompt.as_bytes());
    }
}

fn read_var_names<'a>(opts: &'a ReadOpts<'a>) -> Vec<&'a str> {
    if opts.array_name.is_some() || opts.remaining_args.is_empty() {
        vec!["REPLY"]
    } else {
        opts.remaining_args.to_vec()
    }
}

fn store_read_remaining(ctx: &mut BuiltinContext<'_>, remaining: &str) {
    ctx.state
        .set_var(SmolStr::from("_STDIN_REMAINING"), SmolStr::from(remaining));
}

/// Obtain input text for `read` from stdin or the `_STDIN_REMAINING` variable.
/// The bool in the returned tuple is true when the record ended at the
/// configured delimiter (rather than at end-of-input).
fn read_input(ctx: &mut BuiltinContext<'_>, opts: &ReadOpts<'_>) -> Option<(String, String, bool)> {
    if let Some(mut stdin) = ctx.stdin.take() {
        return read_input_from_stdin(ctx, &mut stdin, opts).ok();
    }
    let input_text = read_input_text(ctx)?;
    Some(read_split_input(&input_text, opts))
}

fn read_input_text(ctx: &mut BuiltinContext<'_>) -> Option<String> {
    let rem = ctx.state.get_var("_STDIN_REMAINING")?;
    if rem.is_empty() {
        return None;
    }
    Some(rem.to_string())
}

fn read_input_from_stdin(
    ctx: &mut BuiltinContext<'_>,
    stdin: &mut BuiltinStdin<'_>,
    opts: &ReadOpts<'_>,
) -> Result<(String, String, bool), ()> {
    // A streaming reader cannot be pushed back, so consume the remaining
    // input once and hand the unconsumed tail to the caller. Subsequent
    // `read` calls in the same shell scope resume from that tail, which is
    // what makes `while read x; do ...; done < file` iterate.
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = match stdin.read_chunk(&mut buf) {
            Ok(n) => n,
            Err(err) => {
                let msg = format!("read: stdin read error: {err}\n");
                ctx.output.stderr(msg.as_bytes());
                return Err(());
            }
        };
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }

    let text = String::from_utf8_lossy(&data).to_string();
    let (line, remaining, found) = read_split_input(&text, opts);
    // No data and no delimiter: end-of-input with nothing read.
    if !found && line.is_empty() && remaining.is_empty() {
        ctx.state
            .set_var(SmolStr::from("_STDIN_REMAINING"), SmolStr::from(""));
        return Ok((String::new(), String::new(), false));
    }
    Ok((line, remaining, found))
}

/// Split input into (`current_line`, `remaining`, `found_delimiter`) according
/// to `read` options (-N, -n, delimiter). `found_delimiter` is false when the
/// record ended at end-of-input rather than at the delimiter, which is what
/// makes `read` return non-zero while still assigning the partial line.
fn read_split_input(input_text: &str, opts: &ReadOpts<'_>) -> (String, String, bool) {
    if let Some(n) = opts.exact_nchars {
        let (line, rest) = read_split_exact(input_text, n);
        let found = line.chars().count() >= n;
        return (line, rest, found);
    }
    if let Some(n) = opts.nchars {
        return read_split_nchars(input_text, n, opts.delimiter);
    }
    // Normal line-based read using delimiter
    if let Some(pos) = input_text.find(opts.delimiter) {
        let line = input_text[..pos].to_string();
        let rest = input_text[pos + opts.delimiter.len_utf8()..].to_string();
        (line, rest, true)
    } else {
        (input_text.to_string(), String::new(), false)
    }
}

/// Split for `-N` (exact N characters, no delimiter stop).
fn read_split_exact(input_text: &str, n: usize) -> (String, String) {
    let chars: String = input_text.chars().take(n).collect();
    let rest_start = chars.len();
    let rest = if rest_start < input_text.len() {
        &input_text[rest_start..]
    } else {
        ""
    };
    (chars, rest.to_string())
}

/// Split for `-n` (at most N characters, stop at delimiter too).
fn read_split_nchars(input_text: &str, n: usize, delimiter: char) -> (String, String, bool) {
    let mut chars = String::new();
    let mut rest_start = 0;
    let mut reached_delimiter = false;
    for ch in input_text.chars() {
        if chars.len() >= n {
            break;
        }
        if ch == delimiter {
            reached_delimiter = true;
            break;
        }
        chars.push(ch);
        rest_start += ch.len_utf8();
    }
    // Skip the delimiter if present
    if rest_start < input_text.len()
        && input_text.as_bytes().get(rest_start) == Some(&(delimiter as u8))
    {
        rest_start += 1;
    }
    let rest = if rest_start < input_text.len() {
        &input_text[rest_start..]
    } else {
        ""
    };
    let found = reached_delimiter || chars.chars().count() >= n;
    (chars, rest.to_string(), found)
}

/// Split a line by IFS and store fields into an indexed array.
fn read_into_array(state: &mut ShellState, line: &str, arr_name: &str) {
    let fields = ifs_split_fields(state, line);
    state.init_indexed_array(SmolStr::from(arr_name));
    for (i, field) in fields.iter().enumerate() {
        state.set_array_element(
            SmolStr::from(arr_name),
            &i.to_string(),
            SmolStr::from(*field),
        );
    }
}

/// Split a line by IFS and assign fields to the given variable names.
///
/// The separators *between* words are preserved inside the value handed to the
/// final variable: `read -r a rest` on `a\tb\tc` gives `rest` the exact text
/// `b\tc`, not a space-joined `b c`. Earlier variables receive one field each.
fn read_assign_vars(state: &mut ShellState, line: &str, var_names: &[&str]) {
    let ifs = state
        .get_var("IFS")
        .unwrap_or_else(|| SmolStr::from(" \t\n"));
    let mut rest = line;
    for (i, var_name) in var_names.iter().enumerate() {
        if i + 1 == var_names.len() {
            // Last variable: the remainder, minus trailing IFS whitespace.
            let value = if ifs.is_empty() {
                rest.to_string()
            } else {
                rest.trim_end_matches(|c: char| is_ifs_whitespace(c, &ifs))
                    .to_string()
            };
            state.set_var(SmolStr::from(*var_name), SmolStr::from(value.as_str()));
        } else {
            let (field, consumed) = take_read_field(rest, &ifs);
            state.set_var(SmolStr::from(*var_name), SmolStr::from(field.as_str()));
            rest = &rest[consumed..];
        }
    }
}

/// Remove backslash escapes from a `read` record (used when `-r` is absent).
/// A backslash escapes the next character whether or not it is an IFS
/// character; a trailing backslash escapes nothing and is kept.
fn unescape_read_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(next) => out.push(next),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// IFS characters that are also whitespace delimit runs rather than single
/// fields, and are ignored at the start and end of a record.
fn is_ifs_whitespace(c: char, ifs: &str) -> bool {
    (c == ' ' || c == '\t' || c == '\n') && ifs.contains(c)
}

/// Extract the next field from `line`, returning the field text and the byte
/// offset just past the delimiter that ended it. Leading IFS whitespace is
/// skipped, and a run of IFS whitespace counts as a single delimiter.
fn take_read_field(line: &str, ifs: &str) -> (String, usize) {
    if ifs.is_empty() {
        return (line.to_string(), line.len());
    }
    let is_delim = |c: char| ifs.contains(c);
    let mut chars = line.char_indices().peekable();
    while let Some(&(_, c)) = chars.peek() {
        if is_ifs_whitespace(c, ifs) {
            chars.next();
        } else {
            break;
        }
    }
    let start = chars.peek().map_or(line.len(), |&(i, _)| i);
    let mut end = start;
    while let Some(&(i, c)) = chars.peek() {
        if is_delim(c) {
            break;
        }
        end = i + c.len_utf8();
        chars.next();
    }
    let field = line[start..end].to_string();
    while let Some(&(_, c)) = chars.peek() {
        if is_ifs_whitespace(c, ifs) {
            chars.next();
        } else {
            break;
        }
    }
    if let Some(&(_, c)) = chars.peek() {
        if is_delim(c) {
            chars.next();
        }
    }
    let consumed = chars.peek().map_or(line.len(), |&(i, _)| i);
    (field, consumed)
}

/// Split a line by IFS characters, filtering empty fields (used by `read -a`).
fn ifs_split_fields<'a>(state: &ShellState, line: &'a str) -> Vec<&'a str> {
    let ifs = state
        .get_var("IFS")
        .unwrap_or_else(|| SmolStr::from(" \t\n"));
    if ifs.is_empty() {
        vec![line]
    } else {
        line.split(|c: char| ifs.contains(c))
            .filter(|s| !s.is_empty())
            .collect()
    }
}

struct TrapSpec {
    name: &'static str,
    number: Option<u8>,
    handler_var: &'static str,
    ignore_var: &'static str,
    trappable: bool,
}

const TRAP_EVENT_SPECS: &[TrapSpec] = &[
    TrapSpec {
        name: "EXIT",
        number: Some(0),
        handler_var: "_TRAP_EXIT",
        ignore_var: "_TRAP_IGNORE_EXIT",
        trappable: true,
    },
    TrapSpec {
        name: "ERR",
        number: None,
        handler_var: "_TRAP_ERR",
        ignore_var: "_TRAP_IGNORE_ERR",
        trappable: true,
    },
    TrapSpec {
        name: "DEBUG",
        number: None,
        handler_var: "_TRAP_DEBUG",
        ignore_var: "_TRAP_IGNORE_DEBUG",
        trappable: true,
    },
    TrapSpec {
        name: "RETURN",
        number: None,
        handler_var: "_TRAP_RETURN",
        ignore_var: "_TRAP_IGNORE_RETURN",
        trappable: true,
    },
];

const TRAP_SIGNAL_SPECS: &[TrapSpec] = &[
    TrapSpec {
        name: "HUP",
        number: Some(1),
        handler_var: "_TRAP_SIG_HUP",
        ignore_var: "_TRAP_IGNORE_SIG_HUP",
        trappable: true,
    },
    TrapSpec {
        name: "INT",
        number: Some(2),
        handler_var: "_TRAP_SIG_INT",
        ignore_var: "_TRAP_IGNORE_SIG_INT",
        trappable: true,
    },
    TrapSpec {
        name: "QUIT",
        number: Some(3),
        handler_var: "_TRAP_SIG_QUIT",
        ignore_var: "_TRAP_IGNORE_SIG_QUIT",
        trappable: true,
    },
    TrapSpec {
        name: "ILL",
        number: Some(4),
        handler_var: "_TRAP_SIG_ILL",
        ignore_var: "_TRAP_IGNORE_SIG_ILL",
        trappable: true,
    },
    TrapSpec {
        name: "ABRT",
        number: Some(6),
        handler_var: "_TRAP_SIG_ABRT",
        ignore_var: "_TRAP_IGNORE_SIG_ABRT",
        trappable: true,
    },
    TrapSpec {
        name: "FPE",
        number: Some(8),
        handler_var: "_TRAP_SIG_FPE",
        ignore_var: "_TRAP_IGNORE_SIG_FPE",
        trappable: true,
    },
    TrapSpec {
        name: "KILL",
        number: Some(9),
        handler_var: "_TRAP_SIG_KILL",
        ignore_var: "_TRAP_IGNORE_SIG_KILL",
        trappable: false,
    },
    TrapSpec {
        name: "USR1",
        number: Some(10),
        handler_var: "_TRAP_SIG_USR1",
        ignore_var: "_TRAP_IGNORE_SIG_USR1",
        trappable: true,
    },
    TrapSpec {
        name: "SEGV",
        number: Some(11),
        handler_var: "_TRAP_SIG_SEGV",
        ignore_var: "_TRAP_IGNORE_SIG_SEGV",
        trappable: true,
    },
    TrapSpec {
        name: "USR2",
        number: Some(12),
        handler_var: "_TRAP_SIG_USR2",
        ignore_var: "_TRAP_IGNORE_SIG_USR2",
        trappable: true,
    },
    TrapSpec {
        name: "PIPE",
        number: Some(13),
        handler_var: "_TRAP_SIG_PIPE",
        ignore_var: "_TRAP_IGNORE_SIG_PIPE",
        trappable: true,
    },
    TrapSpec {
        name: "ALRM",
        number: Some(14),
        handler_var: "_TRAP_SIG_ALRM",
        ignore_var: "_TRAP_IGNORE_SIG_ALRM",
        trappable: true,
    },
    TrapSpec {
        name: "TERM",
        number: Some(15),
        handler_var: "_TRAP_SIG_TERM",
        ignore_var: "_TRAP_IGNORE_SIG_TERM",
        trappable: true,
    },
    TrapSpec {
        name: "CHLD",
        number: Some(17),
        handler_var: "_TRAP_SIG_CHLD",
        ignore_var: "_TRAP_IGNORE_SIG_CHLD",
        trappable: true,
    },
    TrapSpec {
        name: "CONT",
        number: Some(18),
        handler_var: "_TRAP_SIG_CONT",
        ignore_var: "_TRAP_IGNORE_SIG_CONT",
        trappable: true,
    },
    TrapSpec {
        name: "STOP",
        number: Some(19),
        handler_var: "_TRAP_SIG_STOP",
        ignore_var: "_TRAP_IGNORE_SIG_STOP",
        trappable: false,
    },
    TrapSpec {
        name: "TSTP",
        number: Some(20),
        handler_var: "_TRAP_SIG_TSTP",
        ignore_var: "_TRAP_IGNORE_SIG_TSTP",
        trappable: true,
    },
    TrapSpec {
        name: "TTIN",
        number: Some(21),
        handler_var: "_TRAP_SIG_TTIN",
        ignore_var: "_TRAP_IGNORE_SIG_TTIN",
        trappable: true,
    },
    TrapSpec {
        name: "TTOU",
        number: Some(22),
        handler_var: "_TRAP_SIG_TTOU",
        ignore_var: "_TRAP_IGNORE_SIG_TTOU",
        trappable: true,
    },
    TrapSpec {
        name: "WINCH",
        number: Some(28),
        handler_var: "_TRAP_SIG_WINCH",
        ignore_var: "_TRAP_IGNORE_SIG_WINCH",
        trappable: true,
    },
];

fn trap_specs() -> impl Iterator<Item = &'static TrapSpec> {
    TRAP_EVENT_SPECS.iter().chain(TRAP_SIGNAL_SPECS.iter())
}

fn find_trap_spec(name: &str) -> Option<&'static TrapSpec> {
    if name == "0" {
        return TRAP_EVENT_SPECS.iter().find(|spec| spec.name == "EXIT");
    }
    if let Ok(number) = name.parse::<u8>() {
        return TRAP_SIGNAL_SPECS
            .iter()
            .find(|spec| spec.number == Some(number));
    }
    let normalized = name
        .strip_prefix("SIG")
        .unwrap_or(name)
        .to_ascii_uppercase();
    trap_specs().find(|spec| spec.name == normalized)
}

fn set_trap_handler(ctx: &mut BuiltinContext<'_>, spec: &TrapSpec, handler: &str) {
    if handler == "-" {
        ctx.state.unset_var(spec.handler_var).ok();
        ctx.state.unset_var(spec.ignore_var).ok();
        return;
    }

    if handler.is_empty() {
        ctx.state.unset_var(spec.handler_var).ok();
        ctx.state
            .set_var(SmolStr::from(spec.ignore_var), SmolStr::from("1"));
        return;
    }

    ctx.state
        .set_var(SmolStr::from(spec.handler_var), SmolStr::from(handler));
    ctx.state.unset_var(spec.ignore_var).ok();
}

fn print_traps(ctx: &mut BuiltinContext<'_>) {
    for spec in trap_specs() {
        let ignored = ctx.state.get_var(spec.ignore_var).as_deref() == Some("1");
        if ignored {
            let line = format!("trap -- '' {}\n", spec.name);
            ctx.output.stdout(line.as_bytes());
            continue;
        }

        let Some(handler) = ctx.state.get_var(spec.handler_var) else {
            continue;
        };
        if handler.is_empty() {
            continue;
        }

        let line = format!("trap -- {} {}\n", shell_quote(handler.as_str()), spec.name);
        ctx.output.stdout(line.as_bytes());
    }
}

fn list_traps(ctx: &mut BuiltinContext<'_>) {
    for spec in trap_specs() {
        let line = match spec.number {
            Some(number) => format!("{number} {}\n", spec.name),
            None => format!("{}\n", spec.name),
        };
        ctx.output.stdout(line.as_bytes());
    }
}

/// `trap` — set handlers for signals/events.
fn builtin_trap(ctx: &mut BuiltinContext<'_>, argv: &[&str]) -> i32 {
    let args = &argv[1..];
    if args.is_empty() {
        print_traps(ctx);
        return 0;
    }

    match args[0] {
        "-p" => {
            print_traps(ctx);
            return 0;
        }
        "-l" => {
            list_traps(ctx);
            return 0;
        }
        _ => {}
    }

    if args.len() < 2 {
        return 0;
    }

    let handler = args[0];
    let mut status = 0;
    for signal in &args[1..] {
        let Some(spec) = find_trap_spec(signal) else {
            let msg = format!("trap: {signal}: signal not supported\n");
            ctx.output.stderr(msg.as_bytes());
            status = 1;
            continue;
        };
        if !spec.trappable {
            let msg = format!("trap: {signal}: cannot trap this signal\n");
            ctx.output.stderr(msg.as_bytes());
            status = 1;
            continue;
        }
        set_trap_handler(ctx, spec, handler);
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trap_handler_var(name: &str) -> &'static str {
        find_trap_spec(name).unwrap().handler_var
    }

    fn trap_ignore_var(name: &str) -> &'static str {
        find_trap_spec(name).unwrap().ignore_var
    }

    fn run_builtin(name: &str, argv: &[&str]) -> (i32, VecSink) {
        let registry = BuiltinRegistry::new();
        let mut state = ShellState::new();
        let mut sink = VecSink::default();
        let builtin = registry.get(name).unwrap();
        let status = {
            let mut ctx = BuiltinContext {
                state: &mut state,
                output: &mut sink,
                fs: None,
                stdin: None,
            };
            builtin(&mut ctx, argv)
        };
        (status, sink)
    }

    fn run_builtin_with_state(name: &str, argv: &[&str], state: &mut ShellState) -> (i32, VecSink) {
        let registry = BuiltinRegistry::new();
        let mut sink = VecSink::default();
        let builtin = registry.get(name).unwrap();
        let status = {
            let mut ctx = BuiltinContext {
                state,
                output: &mut sink,
                fs: None,
                stdin: None,
            };
            builtin(&mut ctx, argv)
        };
        (status, sink)
    }

    #[test]
    fn colon_returns_zero() {
        let (status, _) = run_builtin(":", &[":"]);
        assert_eq!(status, 0);
    }

    #[test]
    fn true_returns_zero() {
        let (status, _) = run_builtin("true", &["true"]);
        assert_eq!(status, 0);
    }

    #[test]
    fn false_returns_one() {
        let (status, _) = run_builtin("false", &["false"]);
        assert_eq!(status, 1);
    }

    #[test]
    fn echo_basic() {
        let (status, sink) = run_builtin("echo", &["echo", "hello", "world"]);
        assert_eq!(status, 0);
        assert_eq!(sink.stdout_str(), "hello world\n");
    }

    #[test]
    fn echo_no_args() {
        let (_, sink) = run_builtin("echo", &["echo"]);
        assert_eq!(sink.stdout_str(), "\n");
    }

    #[test]
    fn echo_suppress_newline() {
        let (_, sink) = run_builtin("echo", &["echo", "-n", "hello"]);
        assert_eq!(sink.stdout_str(), "hello");
    }

    #[test]
    fn printf_basic() {
        let (status, sink) = run_builtin("printf", &["printf", "hello %s\\n", "world"]);
        assert_eq!(status, 0);
        assert_eq!(sink.stdout_str(), "hello world\n");
    }

    #[test]
    fn printf_int() {
        let (_, sink) = run_builtin("printf", &["printf", "%d", "42"]);
        assert_eq!(sink.stdout_str(), "42");
    }

    #[test]
    fn printf_no_args() {
        let (status, sink) = run_builtin("printf", &["printf"]);
        assert_eq!(status, 1);
        assert!(!sink.stderr_str().is_empty());
    }

    #[test]
    fn pwd_shows_cwd() {
        let mut state = ShellState::new();
        state.cwd = "/home/user".into();
        let (status, sink) = run_builtin_with_state("pwd", &["pwd"], &mut state);
        assert_eq!(status, 0);
        assert_eq!(sink.stdout_str(), "/home/user\n");
    }

    #[test]
    fn cd_changes_cwd() {
        let mut state = ShellState::new();
        let (status, _) = run_builtin_with_state("cd", &["cd", "/tmp"], &mut state);
        assert_eq!(status, 0);
        assert_eq!(state.cwd, "/tmp");
        assert_eq!(state.get_var("PWD").unwrap(), "/tmp");
        assert_eq!(state.get_var("OLDPWD").unwrap(), "/");
    }

    #[test]
    fn cd_dash_returns_to_oldpwd() {
        let mut state = ShellState::new();
        run_builtin_with_state("cd", &["cd", "/tmp"], &mut state);
        let (status, sink) = run_builtin_with_state("cd", &["cd", "-"], &mut state);
        assert_eq!(status, 0);
        assert_eq!(state.cwd, "/");
        assert_eq!(sink.stdout_str(), "/\n");
    }

    #[test]
    fn cd_no_args_goes_home() {
        let mut state = ShellState::new();
        state.set_var("HOME".into(), "/home/user".into());
        let (status, _) = run_builtin_with_state("cd", &["cd"], &mut state);
        assert_eq!(status, 0);
        assert_eq!(state.cwd, "/home/user");
    }

    #[test]
    fn cd_no_home_error() {
        let mut state = ShellState::new();
        let (status, sink) = run_builtin_with_state("cd", &["cd"], &mut state);
        assert_eq!(status, 1);
        assert!(sink.stderr_str().contains("HOME not set"));
    }

    #[test]
    fn export_name_value() {
        let mut state = ShellState::new();
        run_builtin_with_state("export", &["export", "FOO=bar"], &mut state);
        let var = state.env.get("FOO").unwrap();
        assert_eq!(var.value.as_scalar(), "bar");
        assert!(var.exported);
    }

    #[test]
    fn export_existing() {
        let mut state = ShellState::new();
        state.set_var("X".into(), "val".into());
        assert!(!state.env.get("X").unwrap().exported);
        run_builtin_with_state("export", &["export", "X"], &mut state);
        assert!(state.env.get("X").unwrap().exported);
    }

    #[test]
    fn unset_variable() {
        let mut state = ShellState::new();
        state.set_var("FOO".into(), "bar".into());
        run_builtin_with_state("unset", &["unset", "FOO"], &mut state);
        // After unset, variable is truly gone
        assert!(state.get_var("FOO").is_none());
    }

    #[test]
    fn unset_readonly_fails() {
        let mut state = ShellState::new();
        state.set_readonly("X".into(), "locked".into());
        let (status, sink) = run_builtin_with_state("unset", &["unset", "X"], &mut state);
        assert_eq!(status, 1);
        assert!(sink.stderr_str().contains("readonly"));
        assert!(state.get_var("X").is_some()); // still set
    }

    #[test]
    fn readonly_set_value() {
        let mut state = ShellState::new();
        run_builtin_with_state("readonly", &["readonly", "X=locked"], &mut state);
        assert_eq!(state.get_var("X").unwrap(), "locked");
        let var = state.env.get("X").unwrap();
        assert!(var.readonly);
    }

    #[test]
    fn readonly_mark_existing() {
        let mut state = ShellState::new();
        state.set_var("X".into(), "val".into());
        run_builtin_with_state("readonly", &["readonly", "X"], &mut state);
        assert!(state.env.get("X").unwrap().readonly);
    }

    #[test]
    fn registry_lookup() {
        let registry = BuiltinRegistry::new();
        assert!(registry.is_builtin("echo"));
        assert!(registry.is_builtin(":"));
        assert!(registry.is_builtin("readonly"));
        assert!(!registry.is_builtin("ls"));
    }

    // ---- set builtin: -o option tests ----

    #[test]
    fn set_short_flag() {
        let mut state = ShellState::new();
        run_builtin_with_state("set", &["set", "-e"], &mut state);
        assert_eq!(state.get_var("SHOPT_e").unwrap(), "1");
    }

    #[test]
    fn set_plus_disables_flag() {
        let mut state = ShellState::new();
        run_builtin_with_state("set", &["set", "-e"], &mut state);
        run_builtin_with_state("set", &["set", "+e"], &mut state);
        assert_eq!(state.get_var("SHOPT_e").unwrap(), "0");
    }

    #[test]
    fn set_o_pipefail() {
        let mut state = ShellState::new();
        run_builtin_with_state("set", &["set", "-o", "pipefail"], &mut state);
        assert_eq!(state.get_var("SHOPT_o_pipefail").unwrap(), "1");
    }

    #[test]
    fn set_plus_o_pipefail() {
        let mut state = ShellState::new();
        run_builtin_with_state("set", &["set", "-o", "pipefail"], &mut state);
        run_builtin_with_state("set", &["set", "+o", "pipefail"], &mut state);
        assert_eq!(state.get_var("SHOPT_o_pipefail").unwrap(), "0");
    }

    #[test]
    fn set_o_errexit_aliases_e() {
        let mut state = ShellState::new();
        run_builtin_with_state("set", &["set", "-o", "errexit"], &mut state);
        assert_eq!(state.get_var("SHOPT_e").unwrap(), "1");
    }

    #[test]
    fn set_o_nounset_aliases_u() {
        let mut state = ShellState::new();
        run_builtin_with_state("set", &["set", "-o", "nounset"], &mut state);
        assert_eq!(state.get_var("SHOPT_u").unwrap(), "1");
    }

    #[test]
    fn set_o_xtrace_aliases_x() {
        let mut state = ShellState::new();
        run_builtin_with_state("set", &["set", "-o", "xtrace"], &mut state);
        assert_eq!(state.get_var("SHOPT_x").unwrap(), "1");
    }

    #[test]
    fn set_o_noglob_aliases_f() {
        let mut state = ShellState::new();
        run_builtin_with_state("set", &["set", "-o", "noglob"], &mut state);
        assert_eq!(state.get_var("SHOPT_f").unwrap(), "1");
    }

    #[test]
    fn set_o_allexport_aliases_a() {
        let mut state = ShellState::new();
        run_builtin_with_state("set", &["set", "-o", "allexport"], &mut state);
        assert_eq!(state.get_var("SHOPT_a").unwrap(), "1");
    }

    #[test]
    fn set_o_noclobber_aliases_capital_c() {
        let mut state = ShellState::new();
        run_builtin_with_state("set", &["set", "-o", "noclobber"], &mut state);
        assert_eq!(state.get_var("SHOPT_C").unwrap(), "1");
    }

    #[test]
    fn set_o_errtrace_aliases_capital_e() {
        let mut state = ShellState::new();
        run_builtin_with_state("set", &["set", "-o", "errtrace"], &mut state);
        assert_eq!(state.get_var("SHOPT_E").unwrap(), "1");
    }

    #[test]
    fn set_o_functrace_aliases_capital_t() {
        let mut state = ShellState::new();
        run_builtin_with_state("set", &["set", "-o", "functrace"], &mut state);
        assert_eq!(state.get_var("SHOPT_T").unwrap(), "1");
    }

    #[test]
    fn set_o_noexec_aliases_n() {
        let mut state = ShellState::new();
        run_builtin_with_state("set", &["set", "-o", "noexec"], &mut state);
        assert_eq!(state.get_var("SHOPT_n").unwrap(), "1");
    }

    #[test]
    fn set_o_privileged_aliases_p() {
        let mut state = ShellState::new();
        run_builtin_with_state("set", &["set", "-o", "privileged"], &mut state);
        assert_eq!(state.get_var("SHOPT_p").unwrap(), "1");
    }

    #[test]
    fn set_o_verbose_aliases_v() {
        let mut state = ShellState::new();
        run_builtin_with_state("set", &["set", "-o", "verbose"], &mut state);
        assert_eq!(state.get_var("SHOPT_v").unwrap(), "1");
    }

    #[test]
    fn set_dash_o_prints_option_table() {
        let mut state = ShellState::new();
        state.set_var("SHOPT_e".into(), "1".into());
        let (status, sink) = run_builtin_with_state("set", &["set", "-o"], &mut state);
        assert_eq!(status, 0);
        let stdout = sink.stdout_str();
        assert!(stdout.contains("errexit"));
        assert!(stdout.contains("nounset"));
        assert!(stdout.contains("pipefail"));
        assert!(stdout
            .lines()
            .any(|line| line.starts_with("errexit") && line.ends_with("on")));
    }

    #[test]
    fn set_plus_o_prints_recreatable_commands() {
        let mut state = ShellState::new();
        state.set_var("SHOPT_e".into(), "1".into());
        let (status, sink) = run_builtin_with_state("set", &["set", "+o"], &mut state);
        assert_eq!(status, 0);
        let stdout = sink.stdout_str();
        assert!(stdout.contains("set -o errexit"));
        assert!(stdout.contains("set +o nounset"));
        assert!(stdout.contains("set +o pipefail"));
    }

    #[test]
    fn set_o_unrecognized_option_reports_error() {
        let mut state = ShellState::new();
        let (status, sink) =
            run_builtin_with_state("set", &["set", "-o", "nonexistent"], &mut state);
        assert_eq!(status, 0); // set doesn't fail, just warns on stderr
        assert!(sink.stderr_str().contains("unrecognized option"));
    }

    #[test]
    fn trap_registers_debug_and_return_handlers() {
        let mut state = ShellState::new();
        let (status, _) =
            run_builtin_with_state("trap", &["trap", "echo hi", "DEBUG", "RETURN"], &mut state);
        assert_eq!(status, 0);
        assert_eq!(
            state.get_var(trap_handler_var("DEBUG")).as_deref(),
            Some("echo hi")
        );
        assert_eq!(
            state.get_var(trap_handler_var("RETURN")).as_deref(),
            Some("echo hi")
        );
    }

    #[test]
    fn trap_reset_clears_handler_and_ignore_state() {
        let mut state = ShellState::new();
        run_builtin_with_state("trap", &["trap", "", "EXIT"], &mut state);
        let (status, _) = run_builtin_with_state("trap", &["trap", "-", "EXIT"], &mut state);
        assert_eq!(status, 0);
        assert_eq!(state.get_var(trap_handler_var("EXIT")), None);
        assert_eq!(state.get_var(trap_ignore_var("EXIT")), None);
    }

    #[test]
    fn trap_prints_registered_handlers() {
        let mut state = ShellState::new();
        run_builtin_with_state("trap", &["trap", "echo cleanup", "EXIT"], &mut state);
        run_builtin_with_state("trap", &["trap", "", "ERR"], &mut state);
        let (status, sink) = run_builtin_with_state("trap", &["trap", "-p"], &mut state);
        assert_eq!(status, 0);
        assert!(sink.stdout_str().contains("trap -- $'echo cleanup' EXIT"));
        assert!(sink.stdout_str().contains("trap -- '' ERR"));
    }

    #[test]
    fn trap_lists_known_events_and_signals() {
        let (status, sink) = run_builtin("trap", &["trap", "-l"]);
        assert_eq!(status, 0);
        assert!(sink.stdout_str().contains("0 EXIT"));
        assert!(sink.stdout_str().contains("DEBUG"));
        assert!(sink.stdout_str().contains("15 TERM"));
    }

    #[test]
    fn trap_accepts_signal_names_without_warning() {
        let (status, sink) = run_builtin("trap", &["trap", "echo hup", "SIGTERM", "INT"]);
        assert_eq!(status, 0);
        assert!(sink.stderr_str().is_empty());
    }

    #[test]
    fn trap_rejects_untrappable_signals() {
        let mut state = ShellState::new();
        let (status, sink) = run_builtin_with_state(
            "trap",
            &["trap", "echo nope", "KILL", "SIGSTOP"],
            &mut state,
        );
        assert_eq!(status, 1);
        assert!(sink.stderr_str().contains("KILL: cannot trap this signal"));
        assert!(sink
            .stderr_str()
            .contains("SIGSTOP: cannot trap this signal"));
        assert_eq!(state.get_var(trap_handler_var("KILL")), None);
        assert_eq!(state.get_var(trap_handler_var("STOP")), None);
    }
}
