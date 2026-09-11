//! System/env utilities: env, printenv, id, whoami, uname, hostname, sleep, date.

use std::fmt::Write;

use crate::clock::{sample_utc, UtcDateTime};
use crate::UtilContext;

pub(crate) fn print_all_exported(ctx: &mut UtilContext<'_>) {
    if let Some(state) = ctx.state {
        for (name, value) in &state.env.exported_vars() {
            let line = format!("{name}={value}\n");
            ctx.output.stdout(line.as_bytes());
        }
    }
}

pub(crate) fn util_env(ctx: &mut UtilContext<'_>, argv: &[&str]) -> i32 {
    let mut ignore_env = false;
    let mut unset_vars: Vec<&str> = Vec::new();
    let mut null_sep = false;
    let mut extra_vars: Vec<(&str, &str)> = Vec::new();
    let mut i = 1;

    while i < argv.len() {
        let arg = argv[i];
        if arg == "-i" || arg == "--ignore-environment" {
            ignore_env = true;
            i += 1;
        } else if arg == "-u" && i + 1 < argv.len() {
            unset_vars.push(argv[i + 1]);
            i += 2;
        } else if arg == "-0" || arg == "--null" {
            null_sep = true;
            i += 1;
        } else if let Some((k, v)) = arg.split_once('=') {
            extra_vars.push((k, v));
            i += 1;
        } else {
            break;
        }
    }

    let sep = if null_sep { "\0" } else { "\n" };

    if ignore_env {
        // Only print extra vars
        for (k, v) in &extra_vars {
            let line = format!("{k}={v}{sep}");
            ctx.output.stdout(line.as_bytes());
        }
    } else {
        if let Some(state) = ctx.state {
            for (name, value) in &state.env.exported_vars() {
                if unset_vars.contains(&name.as_str()) {
                    continue;
                }
                let line = format!("{name}={value}{sep}");
                ctx.output.stdout(line.as_bytes());
            }
        }
        for (k, v) in &extra_vars {
            let line = format!("{k}={v}{sep}");
            ctx.output.stdout(line.as_bytes());
        }
    }
    0
}

pub(crate) fn util_printenv(ctx: &mut UtilContext<'_>, argv: &[&str]) -> i32 {
    let mut null_sep = false;
    let mut names = Vec::new();
    for arg in &argv[1..] {
        if *arg == "-0" || *arg == "--null" {
            null_sep = true;
        } else {
            names.push(*arg);
        }
    }
    if !names.is_empty() {
        if let Some(state) = ctx.state {
            let vars = state.env.exported_vars();
            let mut found_any = false;
            for name in &names {
                if let Some(value) = vars.get(*name) {
                    ctx.output.stdout(value.as_bytes());
                    ctx.output.stdout(if null_sep { b"\0" } else { b"\n" });
                    found_any = true;
                }
            }
            return i32::from(!found_any);
        }
        return 1;
    }
    print_all_exported(ctx);
    0
}

pub(crate) fn util_id(ctx: &mut UtilContext<'_>, argv: &[&str]) -> i32 {
    let mut show_user = false;
    let mut show_group = false;
    let mut show_groups = false;
    let mut show_name = false;

    for arg in &argv[1..] {
        if arg.starts_with('-') && arg.len() > 1 {
            for ch in arg[1..].chars() {
                match ch {
                    'u' => show_user = true,
                    'g' => show_group = true,
                    'G' => show_groups = true,
                    'n' => show_name = true,
                    // 'r' (real id, same as effective in VFS) etc. — no-op
                    _ => {}
                }
            }
        }
    }

    if show_user || show_group || show_groups {
        if show_name {
            ctx.output.stdout(b"user\n");
        } else {
            ctx.output.stdout(b"1000\n");
        }
    } else {
        ctx.output
            .stdout(b"uid=1000(user) gid=1000(user) groups=1000(user)\n");
    }
    0
}

pub(crate) fn util_whoami(ctx: &mut UtilContext<'_>, _argv: &[&str]) -> i32 {
    ctx.output.stdout(b"user\n");
    0
}

pub(crate) fn util_uname(ctx: &mut UtilContext<'_>, argv: &[&str]) -> i32 {
    let args = &argv[1..];
    if args.is_empty() || args.contains(&"-s") {
        ctx.output.stdout(b"wasmsh\n");
    } else if args.contains(&"-a") {
        ctx.output.stdout(b"wasmsh wasmsh 0.1.0 wasm32 wasmsh\n");
    } else if args.contains(&"-m") {
        ctx.output.stdout(b"wasm32\n");
    } else if args.contains(&"-r") {
        ctx.output.stdout(b"0.1.0\n");
    } else if args.contains(&"-n") || args.contains(&"-o") {
        ctx.output.stdout(b"wasmsh\n");
    } else if args.contains(&"-p") {
        ctx.output.stdout(b"wasm32\n");
    } else if args.contains(&"-v") {
        ctx.output.stdout(b"0.1.0\n");
    }
    0
}

pub(crate) fn util_hostname(ctx: &mut UtilContext<'_>, _argv: &[&str]) -> i32 {
    ctx.output.stdout(b"wasmsh\n");
    0
}

pub(crate) fn util_sleep(_ctx: &mut UtilContext<'_>, _argv: &[&str]) -> i32 {
    0
}

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

const MONTH_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

const WEEKDAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

const WEEKDAY_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

fn format_date(fmt: &str, parts: UtcDateTime) -> String {
    let mut result = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            match chars.next() {
                Some('Y') => {
                    let _ = write!(result, "{:04}", parts.year);
                }
                Some('m') => {
                    let _ = write!(result, "{:02}", parts.month);
                }
                Some('d') => {
                    let _ = write!(result, "{:02}", parts.day);
                }
                Some('H') => {
                    let _ = write!(result, "{:02}", parts.hour);
                }
                Some('M') => {
                    let _ = write!(result, "{:02}", parts.minute);
                }
                Some('S') => {
                    let _ = write!(result, "{:02}", parts.second);
                }
                Some('s') => result.push_str(&parts.epoch_seconds().to_string()),
                Some('N') => {
                    let _ = write!(result, "{:03}000000", parts.millisecond);
                }
                Some('F') => {
                    let _ = write!(
                        result,
                        "{:04}-{:02}-{:02}",
                        parts.year, parts.month, parts.day
                    );
                }
                Some('T') => {
                    let _ = write!(
                        result,
                        "{:02}:{:02}:{:02}",
                        parts.hour, parts.minute, parts.second
                    );
                }
                Some('A') => {
                    let dow = parts.weekday_sunday_zero();
                    result.push_str(WEEKDAY_NAMES[dow]);
                }
                Some('a') => {
                    let dow = parts.weekday_sunday_zero();
                    result.push_str(WEEKDAY_ABBR[dow]);
                }
                Some('B') => {
                    if parts.month >= 1 && parts.month <= 12 {
                        result.push_str(MONTH_NAMES[(parts.month - 1) as usize]);
                    }
                }
                Some('b' | 'h') => {
                    if parts.month >= 1 && parts.month <= 12 {
                        result.push_str(MONTH_ABBR[(parts.month - 1) as usize]);
                    }
                }
                Some('Z') => result.push_str("UTC"),
                Some('z') => result.push_str("+0000"),
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('%') | None => result.push('%'),
                Some('e') => {
                    let _ = write!(result, "{:>2}", parts.day);
                }
                Some('I') => {
                    let h12 = if parts.hour == 0 {
                        12
                    } else if parts.hour > 12 {
                        parts.hour - 12
                    } else {
                        parts.hour
                    };
                    let _ = write!(result, "{h12:02}");
                }
                Some('p') => {
                    result.push_str(if parts.hour < 12 { "AM" } else { "PM" });
                }
                Some('j') => {
                    let month_days: [u16; 12] =
                        [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
                    let leap = parts.year.is_multiple_of(4)
                        && (!parts.year.is_multiple_of(100) || parts.year.is_multiple_of(400));
                    let offset = month_days[(parts.month - 1) as usize];
                    let leap_day = u16::from(leap && parts.month > 2);
                    let _ = write!(result, "{:03}", offset + u16::from(parts.day) + leap_day);
                }
                Some('u') => {
                    result.push_str(&(((parts.weekday_sunday_zero() + 6) % 7 + 1).to_string()));
                }
                Some('w') => result.push_str(&parts.weekday_sunday_zero().to_string()),
                Some('R') => {
                    let _ = write!(result, "{:02}:{:02}", parts.hour, parts.minute);
                }
                Some(c) => {
                    result.push('%');
                    result.push(c);
                }
            }
        } else {
            result.push(ch);
        }
    }
    result
}

pub(crate) fn util_date(ctx: &mut UtilContext<'_>, argv: &[&str]) -> i32 {
    let mut format_arg: Option<&str> = None;
    let mut explicit_date: Option<&str> = None;
    let mut output_mode = DateOutput::Default;
    let mut i = 1;
    while i < argv.len() {
        let arg = argv[i];
        if let Some(fmt) = arg.strip_prefix('+') {
            format_arg = Some(fmt);
        } else if arg == "-d" || arg == "--date" {
            if i + 1 >= argv.len() {
                return date_error(ctx, "option requires an argument: -d");
            }
            explicit_date = Some(argv[i + 1]);
            i += 1;
        } else if let Some(value) = arg.strip_prefix("--date=") {
            explicit_date = Some(value);
        } else if arg == "-u" || arg == "--utc" {
            // All shell dates are UTC. Keep the option explicit and accepted.
        } else if arg == "-R" || arg == "--rfc-email" {
            output_mode = DateOutput::Rfc2822;
        } else if arg == "-I" || arg == "--iso-8601" {
            output_mode = DateOutput::Iso(DatePrecision::Date);
        } else if let Some(value) = arg.strip_prefix("-I") {
            output_mode = match DatePrecision::parse(value) {
                Some(precision) => DateOutput::Iso(precision),
                None => return date_error(ctx, "unsupported -I precision"),
            };
        } else if let Some(value) = arg.strip_prefix("--iso-8601=") {
            output_mode = match DatePrecision::parse(value) {
                Some(precision) => DateOutput::Iso(precision),
                None => return date_error(ctx, "unsupported ISO-8601 precision"),
            };
        } else if arg == "--" {
            if i + 1 < argv.len() {
                i += 1;
                if let Some(fmt) = argv[i].strip_prefix('+') {
                    format_arg = Some(fmt);
                } else {
                    return date_error(ctx, "unexpected operand");
                }
            }
        } else if arg.starts_with('-') {
            return date_error(ctx, &format!("unsupported option: {arg}"));
        } else {
            return date_error(ctx, "unexpected operand");
        }
        i += 1;
    }

    let parts = if let Some(value) = explicit_date {
        match UtcDateTime::parse_legacy(value) {
            Ok(parts) => parts,
            Err(error) => return date_error(ctx, &error.to_string()),
        }
    } else if let Some(clock) = ctx.clock {
        match sample_utc(clock) {
            Ok(parts) => parts,
            Err(error) => return date_error(ctx, &error.to_string()),
        }
    } else {
        let Some(raw) = ctx.state.and_then(|state| state.get_var("WASMSH_DATE")) else {
            return date_error(
                ctx,
                "clock unavailable; install a host callback or set WASMSH_DATE in legacy mode",
            );
        };
        match UtcDateTime::parse_legacy(&raw) {
            Ok(parts) => parts,
            Err(error) => return date_error(ctx, &error.to_string()),
        }
    };

    let output = if let Some(fmt) = format_arg {
        format_date(fmt, parts)
    } else {
        match output_mode {
            DateOutput::Default => format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
                parts.year, parts.month, parts.day, parts.hour, parts.minute, parts.second
            ),
            DateOutput::Rfc2822 => {
                format!(
                    "{}, {:02} {} {:04} {:02}:{:02}:{:02} +0000",
                    WEEKDAY_ABBR[parts.weekday_sunday_zero()],
                    parts.day,
                    MONTH_ABBR[(parts.month - 1) as usize],
                    parts.year,
                    parts.hour,
                    parts.minute,
                    parts.second
                )
            }
            DateOutput::Iso(precision) => format_iso(parts, precision),
        }
    };
    ctx.output.stdout(output.as_bytes());
    ctx.output.stdout(b"\n");
    0
}

#[derive(Clone, Copy)]
enum DateOutput {
    Default,
    Rfc2822,
    Iso(DatePrecision),
}

#[derive(Clone, Copy)]
enum DatePrecision {
    Date,
    Hours,
    Minutes,
    Seconds,
}

impl DatePrecision {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "" | "date" => Some(Self::Date),
            "hours" => Some(Self::Hours),
            "minutes" => Some(Self::Minutes),
            "seconds" => Some(Self::Seconds),
            _ => None,
        }
    }
}

fn format_iso(parts: UtcDateTime, precision: DatePrecision) -> String {
    match precision {
        DatePrecision::Date => format!("{:04}-{:02}-{:02}", parts.year, parts.month, parts.day),
        DatePrecision::Hours => format!(
            "{:04}-{:02}-{:02}T{:02}+00:00",
            parts.year, parts.month, parts.day, parts.hour
        ),
        DatePrecision::Minutes => format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}+00:00",
            parts.year, parts.month, parts.day, parts.hour, parts.minute
        ),
        DatePrecision::Seconds => format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
            parts.year, parts.month, parts.day, parts.hour, parts.minute, parts.second
        ),
    }
}

fn date_error(ctx: &mut UtilContext<'_>, message: &str) -> i32 {
    ctx.output.stderr(format!("date: {message}\n").as_bytes());
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FixedClock, UtcDateTime};
    use wasmsh_fs::BackendFs;
    use wasmsh_state::{ShellState, ShellVar};

    fn run_printenv(argv: &[&str], exports: &[(&str, &str)]) -> (i32, String, String) {
        let mut fs = BackendFs::new();
        let mut output = crate::VecOutput::default();
        let mut state = ShellState::new();
        for (name, value) in exports {
            let mut var = ShellVar::scalar((*value).into());
            var.exported = true;
            state.env.set((*name).into(), var);
        }
        let status = util_printenv(
            &mut UtilContext {
                fs: &mut fs,
                output: &mut output,
                cwd: "/",
                stdin: None,
                state: Some(&state),
                network: None,
                clock: None,
            },
            argv,
        );
        (
            status,
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }

    #[test]
    fn printenv_prints_all_requested_names() {
        let (status, stdout, stderr) = run_printenv(
            &["printenv", "FOO", "BAR"],
            &[("FOO", "one"), ("BAR", "two")],
        );
        assert_eq!(status, 0);
        assert_eq!(stdout, "one\ntwo\n");
        assert_eq!(stderr, "");
    }

    #[test]
    fn printenv_returns_failure_when_all_requested_names_are_missing() {
        let (status, stdout, stderr) = run_printenv(&["printenv", "MISSING"], &[("FOO", "one")]);
        assert_eq!(status, 1);
        assert_eq!(stdout, "");
        assert_eq!(stderr, "");
    }

    fn run_date(argv: &[&str], clock: Option<&dyn crate::ClockProvider>) -> (i32, String, String) {
        let mut fs = BackendFs::new();
        let mut output = crate::VecOutput::default();
        let status = util_date(
            &mut UtilContext {
                fs: &mut fs,
                output: &mut output,
                cwd: "/",
                stdin: None,
                state: None,
                network: None,
                clock,
            },
            argv,
        );
        (
            status,
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }

    #[test]
    fn date_samples_one_fixed_clock_value_for_all_format_fields() {
        let clock = FixedClock::new(
            UtcDateTime::from_calendar(2024, 2, 29, 23, 59, 59, 987)
                .unwrap()
                .epoch_ms(),
        )
        .unwrap();
        let (status, stdout, stderr) =
            run_date(&["date", "+%Y-%m-%d %H:%M:%S %s %N %A"], Some(&clock));
        assert_eq!(status, 0);
        assert_eq!(
            stdout,
            "2024-02-29 23:59:59 1709251199 987000000 Thursday\n"
        );
        assert_eq!(stderr, "");
    }

    #[test]
    fn date_without_clock_fails_instead_of_using_a_startup_default() {
        let (status, stdout, stderr) = run_date(&["date", "+%s"], None);
        assert_eq!(status, 1);
        assert_eq!(stdout, "");
        assert!(stderr.contains("clock unavailable"));
    }

    #[test]
    fn date_options_and_legacy_wasmsh_date_are_explicit() {
        let (status, stdout, stderr) = run_date(&["date", "-R"], None);
        assert_eq!(status, 1);
        assert_eq!(stdout, "");
        assert!(stderr.contains("clock unavailable"));

        let mut fs = BackendFs::new();
        let mut output = crate::VecOutput::default();
        let mut state = ShellState::new();
        state.set_var("WASMSH_DATE".into(), "2026-01-02 03:04:05 UTC".into());
        let status = util_date(
            &mut UtilContext {
                fs: &mut fs,
                output: &mut output,
                cwd: "/",
                stdin: None,
                state: Some(&state),
                network: None,
                clock: None,
            },
            &["date", "-R"],
        );
        assert_eq!(status, 0);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "Fri, 02 Jan 2026 03:04:05 +0000\n"
        );

        let (status, _, stderr) = run_date(&["date", "--silently-ignore-me"], None);
        assert_eq!(status, 1);
        assert!(stderr.contains("unsupported option"));
    }
}
