//! `od` — dump files in octal, hex, decimal, or character form.
//!
//! This is a GNU-compatible subset covering the forms real scripts use:
//! `-A {o,d,x,n}`, `-t {a,c,d,o,u,x}[SIZE]`, the legacy shorthand flags
//! (`-b -c -d -o -x`), `-j`/`-N`, `-w`, `-v`, and the repeated-line `*`
//! collapse. The default is GNU's `-t o2` with a 16-byte line.
//!
//! Not covered: multiple simultaneous `-t` types (GNU prints one line per type
//! per group; here the last `-t` wins) and `--traditional` width rules.

use crate::helpers::read_input_bytes;
use crate::UtilContext;

/// How each value is rendered.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Radix {
    Named,
    Char,
    Signed,
    Unsigned,
    Octal,
    Hex,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AddrRadix {
    Octal,
    Decimal,
    Hex,
    None,
}

struct OdSpec {
    radix: Radix,
    size: usize,
}

struct OdOptions {
    spec: OdSpec,
    addr: AddrRadix,
    width: usize,
    skip: usize,
    limit: Option<usize>,
    collapse: bool,
}

impl Default for OdOptions {
    fn default() -> Self {
        Self {
            spec: OdSpec {
                radix: Radix::Octal,
                size: 2,
            },
            addr: AddrRadix::Octal,
            width: 16,
            skip: 0,
            limit: None,
            collapse: true,
        }
    }
}

pub(crate) fn util_od(ctx: &mut UtilContext<'_>, argv: &[&str]) -> i32 {
    let (opts, files) = match parse_args(argv) {
        Ok(v) => v,
        Err(msg) => {
            ctx.output.stderr(msg.as_bytes());
            return 1;
        }
    };

    let mut data = match read_input_bytes(ctx, &files, "od") {
        Ok(d) => d,
        Err(status) => return status,
    };
    let start = opts.skip.min(data.len());
    data.drain(..start);
    if let Some(limit) = opts.limit {
        data.truncate(limit.min(data.len()));
    }

    emit(ctx, &data, start, &opts);
    0
}

fn parse_args<'a>(argv: &'a [&'a str]) -> Result<(OdOptions, Vec<&'a str>), String> {
    let mut opts = OdOptions::default();
    let mut files = Vec::new();
    let mut i = 1;
    let mut only_files = false;

    while i < argv.len() {
        let arg = argv[i];
        if only_files || !arg.starts_with('-') || arg == "-" {
            files.push(arg);
            i += 1;
            continue;
        }
        match arg {
            "--" => {
                only_files = true;
                i += 1;
            }
            "-A" => {
                let value = argv
                    .get(i + 1)
                    .ok_or("od: option requires an argument -- 'A'")?;
                opts.addr = parse_addr_radix(value)?;
                i += 2;
            }
            "-t" => {
                let value = argv
                    .get(i + 1)
                    .ok_or("od: option requires an argument -- 't'")?;
                opts.spec = parse_type(value)?;
                i += 2;
            }
            "-j" | "-s" => {
                let value = argv
                    .get(i + 1)
                    .ok_or("od: option requires an argument -- 'j'")?;
                opts.skip = parse_size(value)?;
                i += 2;
            }
            "-N" => {
                let value = argv
                    .get(i + 1)
                    .ok_or("od: option requires an argument -- 'N'")?;
                opts.limit = Some(parse_size(value)?);
                i += 2;
            }
            "-w" => {
                let value = argv
                    .get(i + 1)
                    .ok_or("od: option requires an argument -- 'w'")?;
                opts.width = parse_size(value)?.max(1);
                i += 2;
            }
            "-v" => {
                opts.collapse = false;
                i += 1;
            }
            "-b" => {
                opts.spec = OdSpec {
                    radix: Radix::Octal,
                    size: 1,
                };
                i += 1;
            }
            "-c" => {
                opts.spec = OdSpec {
                    radix: Radix::Char,
                    size: 1,
                };
                i += 1;
            }
            "-d" => {
                opts.spec = OdSpec {
                    radix: Radix::Unsigned,
                    size: 2,
                };
                i += 1;
            }
            "-o" => {
                opts.spec = OdSpec {
                    radix: Radix::Octal,
                    size: 2,
                };
                i += 1;
            }
            "-x" => {
                opts.spec = OdSpec {
                    radix: Radix::Hex,
                    size: 2,
                };
                i += 1;
            }
            "-a" => {
                opts.spec = OdSpec {
                    radix: Radix::Named,
                    size: 1,
                };
                i += 1;
            }
            _ if arg.starts_with("-A") && arg.len() > 2 => {
                opts.addr = parse_addr_radix(&arg[2..])?;
                i += 1;
            }
            _ if arg.starts_with("-t") && arg.len() > 2 => {
                opts.spec = parse_type(&arg[2..])?;
                i += 1;
            }
            _ if arg.starts_with("-N") && arg.len() > 2 => {
                opts.limit = Some(parse_size(&arg[2..])?);
                i += 1;
            }
            _ if arg.starts_with("-j") && arg.len() > 2 => {
                opts.skip = parse_size(&arg[2..])?;
                i += 1;
            }
            _ if arg.starts_with("-w") && arg.len() > 2 => {
                opts.width = parse_size(&arg[2..])?.max(1);
                i += 1;
            }
            // Anything that merely starts with `-` is treated as clustered
            // legacy flags such as `-bc`; a genuinely unknown flag fails.
            _ => {
                for ch in arg[1..].chars() {
                    match ch {
                        'b' => {
                            opts.spec = OdSpec {
                                radix: Radix::Octal,
                                size: 1,
                            }
                        }
                        'c' => {
                            opts.spec = OdSpec {
                                radix: Radix::Char,
                                size: 1,
                            }
                        }
                        'a' => {
                            opts.spec = OdSpec {
                                radix: Radix::Named,
                                size: 1,
                            }
                        }
                        'v' => opts.collapse = false,
                        'd' => {
                            opts.spec = OdSpec {
                                radix: Radix::Unsigned,
                                size: 2,
                            }
                        }
                        'o' => {
                            opts.spec = OdSpec {
                                radix: Radix::Octal,
                                size: 2,
                            }
                        }
                        'x' => {
                            opts.spec = OdSpec {
                                radix: Radix::Hex,
                                size: 2,
                            }
                        }
                        other => return Err(format!("od: invalid option -- '{other}'\n")),
                    }
                }
                i += 1;
            }
        }
    }
    Ok((opts, files))
}

fn parse_addr_radix(value: &str) -> Result<AddrRadix, String> {
    match value.chars().next() {
        Some('o') => Ok(AddrRadix::Octal),
        Some('d') => Ok(AddrRadix::Decimal),
        Some('x') => Ok(AddrRadix::Hex),
        Some('n') => Ok(AddrRadix::None),
        _ => Err(format!("od: invalid address radix '{value}'\n")),
    }
}

fn parse_type(value: &str) -> Result<OdSpec, String> {
    let mut chars = value.chars();
    let kind = chars
        .next()
        .ok_or_else(|| "od: empty type string\n".to_string())?;
    let rest: String = chars.collect();
    // A trailing size (1, 2, 4, 8) or a C-style byte count (C, S, I, L, or
    // their `_`-suffixed forms) selects the unit width.
    let size = match rest.as_str() {
        "" | "1" | "C" => 1,
        "2" | "S" => 2,
        "4" | "I" | "L" => 4,
        "8" => 8,
        other => {
            return Err(format!("od: invalid type string '{kind}{other}'\n"));
        }
    };
    let radix = match kind {
        'a' => Radix::Named,
        'c' => Radix::Char,
        'd' => Radix::Signed,
        'u' => Radix::Unsigned,
        'o' => Radix::Octal,
        'x' => Radix::Hex,
        other => return Err(format!("od: invalid type '{other}'\n")),
    };
    // Character and named forms are inherently one byte per unit.
    let size = if matches!(radix, Radix::Named | Radix::Char) {
        1
    } else {
        size
    };
    Ok(OdSpec { radix, size })
}

fn parse_size(value: &str) -> Result<usize, String> {
    let trimmed = value.trim_start_matches('+');
    let (radix, digits) = match trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        Some(hex) => (16, hex),
        None => match trimmed.strip_prefix('0') {
            Some(oct) if !oct.is_empty() => (8, oct),
            _ => (10, trimmed),
        },
    };
    usize::from_str_radix(digits, radix).map_err(|_| format!("od: invalid number '{value}'\n"))
}

fn addr_width(radix: AddrRadix) -> usize {
    match radix {
        AddrRadix::Octal | AddrRadix::Decimal => 7,
        AddrRadix::Hex => 6,
        AddrRadix::None => 0,
    }
}

fn format_offset(offset: usize, radix: AddrRadix) -> String {
    match radix {
        AddrRadix::Octal => format!("{offset:07o}"),
        AddrRadix::Decimal => format!("{offset:07}"),
        AddrRadix::Hex => format!("{offset:06x}"),
        AddrRadix::None => String::new(),
    }
}

/// Field width (excluding the single leading separator) for a numeric type.
fn field_width(radix: Radix, size: usize) -> usize {
    match radix {
        // Digits needed for the largest unsigned value of that width:
        // octal is ceil(bits/3), hex is ceil(bits/4).
        Radix::Octal => (size * 8).div_ceil(3),
        Radix::Hex => size * 2,
        Radix::Unsigned => match size {
            1 => 3,
            2 => 5,
            4 => 10,
            _ => 20,
        },
        Radix::Signed => match size {
            1 => 4,
            2 => 6,
            4 => 11,
            _ => 20,
        },
        Radix::Named | Radix::Char => 3,
    }
}

fn read_unit(bytes: &[u8], size: usize) -> u64 {
    let mut value = 0u64;
    for (i, &b) in bytes.iter().enumerate().take(size) {
        value |= u64::from(b) << (8 * i);
    }
    value
}

fn format_unit(radix: Radix, size: usize, bytes: &[u8], width: usize) -> String {
    match radix {
        Radix::Char => {
            let b = bytes[0];
            let text = match b {
                0 => "\\0".to_string(),
                7 => "\\a".to_string(),
                8 => "\\b".to_string(),
                9 => "\\t".to_string(),
                10 => "\\n".to_string(),
                11 => "\\v".to_string(),
                12 => "\\f".to_string(),
                13 => "\\r".to_string(),
                0x20..=0x7e => (b as char).to_string(),
                _ => format!("{b:03o}"),
            };
            format!("{text:>width$}")
        }
        Radix::Named => {
            const NAMES: [&str; 33] = [
                "nul", "soh", "stx", "etx", "eot", "enq", "ack", "bel", "bs", "ht", "nl", "vt",
                "ff", "cr", "so", "si", "dle", "dc1", "dc2", "dc3", "dc4", "nak", "syn", "etb",
                "can", "em", "sub", "esc", "fs", "gs", "rs", "us", "sp",
            ];
            // GNU indexes the ASCII name table with the low seven bits, so a
            // high byte such as 0x80 reads as `nul` and 0xff as `del`.
            let b = bytes[0];
            let idx = b & 0x7f;
            let text = if idx < 33 {
                NAMES[idx as usize]
            } else if idx == 0x7f {
                "del"
            } else {
                return format!("{:>width$}", (idx as char).to_string());
            };
            format!("{text:>width$}")
        }
        // Octal and hex are zero-padded to their fixed digit width; decimal
        // forms are space-padded (GNU od).
        Radix::Octal => format!("{:0>width$o}", read_unit(bytes, size)),
        Radix::Hex => format!("{:0>width$x}", read_unit(bytes, size)),
        Radix::Unsigned => format!("{:>width$}", read_unit(bytes, size)),
        Radix::Signed => {
            let raw = read_unit(bytes, size);
            // Sign-extend from the unit width.
            let bits = size * 8;
            let value = if bits < 64 && (raw >> (bits - 1)) & 1 == 1 {
                (raw as i128) - (1i128 << bits)
            } else {
                raw as i128
            };
            format!("{value:>width$}")
        }
    }
}

fn emit(ctx: &mut UtilContext<'_>, data: &[u8], base: usize, opts: &OdOptions) {
    let size = opts.spec.size.max(1);
    let radix = opts.spec.radix;
    // A width that is not a whole number of units is rejected by GNU, which
    // warns and falls back to a single unit on each line.
    let units_per_line = if opts.width.is_multiple_of(size) {
        (opts.width / size).max(1)
    } else {
        ctx.output.stderr(
            format!(
                "od: warning: invalid width {}; using {size} instead\n",
                opts.width
            )
            .as_bytes(),
        );
        1
    };
    let line_bytes = units_per_line * size;
    let width = field_width(radix, size);
    let addr_len = addr_width(opts.addr);

    let mut previous: Option<Vec<u8>> = None;
    let mut collapsed = false;
    let mut offset = 0usize;

    while offset < data.len() {
        let end = (offset + line_bytes).min(data.len());
        let chunk = &data[offset..end];
        let same_as_previous = previous.as_deref() == Some(chunk);
        if same_as_previous && opts.collapse {
            if !collapsed {
                ctx.output.stdout(b"*\n");
                collapsed = true;
            }
            offset = end;
            continue;
        }
        collapsed = false;
        previous = Some(chunk.to_vec());
        let mut line = render_line(
            chunk,
            base + offset,
            size,
            radix,
            width,
            addr_len,
            units_per_line,
            opts.addr,
        );
        line.push('\n');
        ctx.output.stdout(line.as_bytes());
        offset = end;
    }

    // A final offset line is printed unless the address column is suppressed
    // (`-An`). It appears even for empty input.
    if addr_len > 0 {
        ctx.output
            .stdout(format!("{}\n", format_offset(base + data.len(), opts.addr)).as_bytes());
    }
}

#[allow(clippy::too_many_arguments)]
fn render_line(
    chunk: &[u8],
    offset: usize,
    size: usize,
    radix: Radix,
    width: usize,
    addr_len: usize,
    units_per_line: usize,
    addr: AddrRadix,
) -> String {
    let mut line = String::new();
    if addr_len > 0 {
        line.push_str(&format_offset(offset, addr));
    }
    let mut emitted_units = 0usize;
    for unit in chunk.chunks(size) {
        line.push(' ');
        line.push_str(&format_unit(radix, size, unit, width));
        emitted_units += 1;
    }
    // Pad a short final line so the address column aligns on the next file.
    for _ in emitted_units..units_per_line {
        line.push(' ');
        line.push_str(&" ".repeat(width));
    }
    // GNU od trims trailing padding on the final line.
    while line.ends_with(' ') {
        line.pop();
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VecOutput;
    use wasmsh_fs::MemoryFs;

    fn od(argv: &[&str], stdin: Option<&[u8]>) -> (i32, String) {
        let mut fs = MemoryFs::new();
        let mut output = VecOutput::default();
        let status = {
            let mut ctx = UtilContext {
                fs: &mut fs,
                output: &mut output,
                cwd: "/",
                stdin: stdin.map(crate::UtilStdin::from_bytes),
                state: None,
                network: None,
                clock: None,
            };
            util_od(&mut ctx, argv)
        };
        (status, output.stdout_str().to_string())
    }

    #[test]
    fn default_is_two_byte_octal_words() {
        let (status, out) = od(&["od"], Some(b"ABC"));
        assert_eq!(status, 0);
        assert_eq!(out, "0000000 041101 000103\n0000003\n");
    }

    #[test]
    fn char_type_names_control_bytes() {
        let (status, out) = od(&["od", "-An", "-c"], Some(b"A\tB"));
        assert_eq!(status, 0);
        assert_eq!(out, "   A  \\t   B\n");
    }

    #[test]
    fn hex_one_byte_is_sixteen_per_line() {
        let data: Vec<u8> = (0u8..20).collect();
        let (status, out) = od(&["od", "-An", "-tx1"], Some(&data));
        assert_eq!(status, 0);
        assert_eq!(
            out,
            " 00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e 0f\n 10 11 12 13\n"
        );
    }

    #[test]
    fn skip_and_limit_select_a_range() {
        let data: Vec<u8> = (0u8..8).collect();
        let (status, out) = od(&["od", "-An", "-tx1", "-j", "2", "-N", "3"], Some(&data));
        assert_eq!(status, 0);
        assert_eq!(out, " 02 03 04\n");
    }

    #[test]
    fn repeated_lines_collapse_unless_verbose() {
        let data = vec![0u8; 40];
        let (_, collapsed) = od(&["od", "-An", "-tx1"], Some(&data));
        assert!(
            collapsed.lines().any(|l| l == "*"),
            "collapsed={collapsed:?}"
        );
        let (_, verbose) = od(&["od", "-An", "-tx1", "-v"], Some(&data));
        assert!(!verbose.lines().any(|l| l == "*"));
    }

    #[test]
    fn empty_input_prints_only_the_offset() {
        let (status, out) = od(&["od"], Some(b""));
        assert_eq!(status, 0);
        assert_eq!(out, "0000000\n");
    }

    #[test]
    fn field_offsets_are_absolute() {
        let data: Vec<u8> = (0u8..4).collect();
        let (_, out) = od(&["od", "-Ad", "-tx1", "-j", "2"], Some(&data));
        assert_eq!(out, "0000002 02 03\n0000004\n");
    }
}
