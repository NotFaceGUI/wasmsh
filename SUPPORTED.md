# Supported Syntax and Commands

## Verification status

This document describes a **verified subset of Bash**, not a complete Bash
implementation. "Verified" means the behaviour is either:

1. **Differentially tested** — a TOML case under
   [`tests/suite/differential/`](tests/suite/differential/) runs the same
   script through wasmsh and a real `bash`, and asserts identical stdout,
   stderr, and exit status (see `[oracle] compare = true`). These cases run in
   the `oracle` CI job, where a missing reference shell is a **visible SKIP**,
   never a silent pass.
2. **Behaviourally tested** — a declarative case with an explicit expected
   output or a Rust unit test.

Everything else is either **known divergent** (documented below) or
**unsupported**. The sandbox does not aim to be a drop-in GNU Bash: it has no
PTY or job control, several utilities are stubs, and locale handling is fixed
to UTF-8/C. See [Known divergences and degraded commands](#known-divergences-and-degraded-commands).

## Delivery Status

The standalone artifact is the primary AI-shell delivery. Its base tier is
the in-process Bash-compatible runtime, POSIX virtual filesystem, binary
protocol events, session isolation, and cooperative step-budget cancellation.
The complete Node host tier adds live clock callbacks, structured network
policy plus a redirect-aware broker, finite external commands, and progressive
external pipelines. The final-artifact Node/bundler smoke and Playwright suite
exercise these against a real build, and that suite has run green in the cloud:
`Standalone Release` run `34689670098` (tag `v0.9.5`) passed validate → build →
verify → three-platform host matrix (ubuntu-24.04 / macos-15 / windows-2025) →
publish, and `Standalone WASM` run `34690078437` passed on `main`. The `v0.9.5`
Release asset was downloaded and re-verified locally (archive SHA256
`cc2cc06cf972fe19bf8bda475fc33cb8a5ed36c3bb2dcfa779cc1bb062d0f7d8`, all
`SHA256SUMS` OK, three target WASMs byte-identical). The browser worker has no
native process executor.

Known limits: the included browser network path refuses synchronous XHR because
it cannot enforce redirect policy before the next request; browser native
external processes require a separate trusted broker; `Cancel` is cooperative;
and the runtime is not a full GNU Bash, PTY, or job-control implementation.
Windows/Linux/macOS host consumption is defined in
`.github/workflows/wasm-build.yml` and has executed green in the cloud run above.
See the [target verification report](docs/verification-report.md) and
[`docs/implementation-tracker.md`](docs/implementation-tracker.md) for the
actual verification status.

## Shell Syntax

### Implemented

**Commands and lists**
- Simple commands: `cmd arg1 arg2 ...`
- Pipelines: `cmd1 | cmd2 | cmd3`
- Stderr-to-pipe: `cmd1 |& cmd2`
- And/or lists: `cmd1 && cmd2`, `cmd1 || cmd2`
- Semicolon lists: `cmd1; cmd2; cmd3`
- Background execution: `cmd &` (parsed; browser runtime runs synchronously)
- Pipeline negation: `! cmd`
- Variable assignments: `VAR=value`, `VAR=value cmd`
- Append assignments: `VAR+=value`

**Compound commands**
- `if/then/elif/else/fi`
- `while/do/done`
- `until/do/done`
- `for var in words; do/done`
- `for (( init; cond; step )); do/done` (C-style arithmetic for)
- `case/esac` with `;;`, `;&` (fall-through), `;;&` (continue-testing)
- `select/do/done` (menu-driven; repeats until `break` or EOF)
- `(( expr ))` arithmetic command
- `[[ expr ]]` extended test
- Subshells: `( ... )`
- Brace groups: `{ ...; }`
- Function definitions: `name() { ... }`, `function name { ... }`, `function name() { ... }`

**Redirections**
- Input: `<`
- Output (truncate): `>`
- Output (append): `>>`
- Read-write: `<>`
- Here-document: `<<DELIM`, `<<-DELIM` (tab-stripping), quoted delimiters suppress expansion
- Here-string: `<<<`
- FD-prefixed: `2>`, `2>>`, `2>&1`
- Combined stdout+stderr: `&>`

**Quoting and escaping**
- Single quoting: `'literal text'`
- Double quoting: `"text with $expansion"`
- Backslash escaping: `\char`
- ANSI-C quoting: `$'...'` (lexer support)
- Comments: `# comment`

**Expansions**
- Parameter expansion: `$var`, `${var}` and all operators (see section below)
- Command substitution: `$(...)`
- Arithmetic expansion: `$(( expr ))`
- Process substitution: `<(cmd)`, `>(cmd)`
- Brace expansion: `{a,b,c}`, `{1..10}`
- Tilde expansion: `~`, `~/path`
- Field splitting on `IFS`
- Glob/pathname expansion: `*`, `?`, `[...]`, extglob patterns (see below)

### Not Yet Implemented

- Coprocesses: `coproc`
- Full job-linked stop/continue signal semantics

---

## Builtins

| Command      | Status | Notes |
|--------------|--------|-------|
| `:`          | Done   | No-op, always returns 0 |
| `true`       | Done   | Returns 0 |
| `false`      | Done   | Returns 1 |
| `echo`       | Done   | `-n` (suppress newline), `-e` (escape sequences: `\n \t \\ \a \b \r \0NNN`) |
| `printf`     | Done   | `%s %d %x %o %f %c %b %q %%`; width, precision, `-` (left-align), `0` (zero-pad); repeats format for extra args |
| `pwd`        | Done   | Prints working directory |
| `cd`         | Done   | `cd -` (OLDPWD), `cd` (HOME); sets PWD and OLDPWD |
| `export`     | Done   | `export NAME=VALUE`, `export NAME`; respects readonly |
| `unset`      | Done   | Removes variable or array element (`unset 'arr[N]'`); respects readonly |
| `readonly`   | Done   | `readonly NAME=VALUE`, `readonly NAME` |
| `test` / `[` | Done   | Unary: `-n -z -f -d -e -s -r -w -x`; binary: `= == != -eq -ne -lt -gt -le -ge`; `!` negation |
| `read`       | Done   | `-r` (raw), `-p prompt`, `-d delim`, `-n N`, `-N N`, `-a array`, `-t timeout`, `-s` (silent); IFS splitting; default var REPLY |
| `shift`      | Done   | `shift [N]`; shifts positional parameters |
| `return`     | Done   | Returns from function with optional exit status |
| `exit`       | Done   | Exits shell with optional status; fires EXIT trap |
| `local`      | Done   | `local VAR=val`; save/restore stack for function scope |
| `type`       | Done   | Reports alias/function/builtin/utility classification |
| `command`    | Done   | `-v` shows command type; bypasses functions |
| `eval`       | Done   | Re-parses and executes concatenated arguments |
| `set`        | Done   | `-a -C -e -E -f -n -p -T -u -v -x`; long names `allexport errexit errtrace functrace noclobber noglob noexec nounset pipefail privileged verbose xtrace`; `set -o` / `set +o` list known options; `-E` inherits `ERR` into functions/source and `-T` inherits `DEBUG`/`RETURN`; `set -- args` sets positionals |
| `getopts`    | Done   | Parses short options from positional parameters; updates OPTIND |
| `trap`       | Done   | `EXIT`, `ERR`, `DEBUG`, `RETURN`, `trap -p`, `trap -l`, reset/ignore; signal traps are host-deliverable via the worker protocol/browser wrapper; `KILL` and `STOP` are listed but rejected as non-trappable |
| `declare` / `typeset` | Done | `-i` (integer), `-a` (indexed array), `-A` (assoc array), `-x` (export), `-r` (readonly), `-l` (lowercase), `-u` (uppercase), `-n` (nameref), `-p` (print); compound assignment `arr=(...)` |
| `let`        | Done   | Evaluates arithmetic expressions; exit status is 0 if last result is non-zero |
| `shopt`      | Done   | `-s` / `-u`; options: `extglob nullglob dotglob globstar nocasematch nocaseglob failglob lastpipe expand_aliases sourcepath` |
| `alias`      | Done   | Define and list aliases; aliases expand recursively |
| `unalias`    | Done   | `-a` removes all aliases |
| `source` / `.` | Done | Reads and executes a file from VFS; PATH lookup for bare names is controlled by `shopt sourcepath` |
| `mapfile` / `readarray` | Done | `-t` (strip newline); default array MAPFILE |
| `builtin`    | Done   | Bypasses aliases and functions; invokes named builtin directly |

---

## Utilities (88)

All utilities operate on the in-process VFS (no OS calls).

### File utilities (14)

| Command      | Status | Notes |
|--------------|--------|-------|
| `cat`        | Done   | Concatenate files; reads stdin when no files given |
| `ls`         | Done   | Directory listing |
| `mkdir`      | Done   | Create directories |
| `rm`         | Done   | Remove files and directories |
| `touch`      | Done   | Create empty files or update timestamps |
| `mv`         | Done   | Move/rename files |
| `cp`         | Done   | Copy files |
| `ln`         | Done   | Create hard and symbolic links |
| `readlink`   | Done   | Read symlink target |
| `realpath`   | Done   | Resolve to absolute path |
| `stat`       | Done   | Show file metadata |
| `find`       | Done   | Search filesystem |
| `chmod`      | Yes    | Octal and symbolic modes, `-R`. Owner bits are enforced on `open`; `ls -l` and `test -r/-w/-x` read them. Ownership is not modelled — the VFS has one principal. |
| `mktemp`     | Done   | Create a temporary file |

### Text utilities (14)

| Command      | Status | Notes |
|--------------|--------|-------|
| `head`       | Done   | First N lines (`-n N`) |
| `tail`       | Done   | Last N lines (`-n N`) |
| `wc`         | Done   | Line/word/byte counts (`-l -w -c`) |
| `grep`       | Done   | Pattern search |
| `sed`        | Done   | Stream editor |
| `sort`       | Done   | Sort lines |
| `uniq`       | Done   | Remove duplicate adjacent lines |
| `cut`        | Done   | Cut fields or characters |
| `tr`         | Done   | Translate or delete characters |
| `tee`        | Done   | Write stdin to file and stdout |
| `paste`      | Done   | Merge lines of files |
| `rev`        | Done   | Reverse characters in each line |
| `column`     | Done   | Format input into columns |
| `bat`        | Done   | Syntax-highlighted file viewer |

### Data and string utilities (9)

| Command      | Status | Notes |
|--------------|--------|-------|
| `seq`        | Done   | Generate sequences of numbers |
| `basename`   | Done   | Strip directory and suffix from path |
| `dirname`    | Done   | Extract directory part of path |
| `expr`       | Done   | Evaluate expression |
| `xargs`      | Done   | Build and execute commands from stdin |
| `yes`        | Done   | Output string repeatedly |
| `md5sum`     | Done   | Compute MD5 checksums (clean-room RFC 1321) |
| `sha256sum`  | Done   | Compute SHA-256 checksums (clean-room FIPS 180-4) |
| `base64`     | Done   | Encode/decode base64 |

### System and environment utilities (8)

| Command      | Status | Notes |
|--------------|--------|-------|
| `env`        | Done   | Print or set environment |
| `printenv`   | Done   | Print environment variables |
| `id`         | Done   | Print user/group identity (static sandbox values) |
| `whoami`     | Done   | Print current user (static sandbox value) |
| `uname`      | Done   | Print system information (static sandbox values) |
| `hostname`   | Done   | Print hostname (static sandbox value) |
| `sleep`      | Done   | Delay (no-op in sandbox; returns immediately) |
| `date`       | Done   | Uses the host clock callback in standalone production mode; fixed time and legacy `WASMSH_DATE` are explicit test/compatibility modes. |

### Simple utilities

| Command      | Status | Notes |
|--------------|--------|-------|
| `which`      | Done   | Locate a command |
| `rmdir`      | Done   | Remove empty directories |
| `tac`        | Done   | Reverse lines of file |
| `nl`         | Done   | Number lines |
| `shuf`       | Done   | Shuffle lines |
| `cmp`        | Done   | Compare two files byte by byte (incl. `-` for stdin) |
| `comm`       | Done   | Compare two sorted files line by line |
| `od`         | Done   | Octal/hex/decimal/character dump (`-A`, `-t`, legacy flags) |
| `join`       | Done   | Join two sorted files on a field (`-1/-2/-j -t -a -v -o -e -i`) |
| `fold`       | Done   | Wrap lines to specified width |
| `nproc`      | Done   | Print number of processing units |
| `expand`     | Done   | Convert tabs to spaces |
| `unexpand`   | Done   | Convert spaces to tabs |
| `truncate`   | Done   | Shrink or extend file size |
| `factor`     | Done   | Print prime factors |
| `cksum`      | Done   | Print CRC checksum and byte count |
| `tsort`      | Done   | Topological sort |
| `install`    | Done   | Copy files and set attributes |
| `timeout`    | Rejected | Returns status 125 with a diagnostic. The synchronous sandbox cannot preempt an in-process command; host adapters enforce a real wall-clock limit for external processes. |
| `cal`        | Done   | Display a calendar |

### Diff and patch (2)

| Command      | Status | Notes |
|--------------|--------|-------|
| `diff`       | Done   | Compare files line by line (unified format) |
| `patch`      | Done   | Apply unified diff patches |

### Directory visualization (1)

| Command      | Status | Notes |
|--------------|--------|-------|
| `tree`       | Done   | Recursive directory listing with tree-style output |

### Code search (2)

| Command      | Status | Notes |
|--------------|--------|-------|
| `rg`         | Done   | Ripgrep-compatible search with built-in regex engine |
| `fd`         | Done   | Fast file finder (fd-find compatible) |

### Embedded interpreters (4)

| Command      | Status | Notes |
|--------------|--------|-------|
| `awk`        | Done   | Full AWK interpreter: lexer, parser, evaluator; associative arrays, user functions, regex |
| `jq`         | Done   | JSON processor: handwritten JSON parser, filter language, 90+ built-in functions |
| `yq`         | Done   | YAML processor: handwritten YAML parser, jq-compatible filter subset |
| `bc`         | Done   | Calculator: expression parser, variables, control flow, user-defined functions |

### Hash utilities (2)

| Command      | Status | Notes |
|--------------|--------|-------|
| `sha1sum`    | Done   | Compute SHA-1 checksums (clean-room RFC 3174) |
| `sha512sum`  | Done   | Compute SHA-512 checksums (clean-room FIPS 180-4) |

### Binary utilities (5)

| Command      | Status | Notes |
|--------------|--------|-------|
| `xxd`        | Done   | Hex dump and reverse |
| `dd`         | Done   | Copy and convert data |
| `strings`    | Done   | Print printable strings from binary data |
| `split`      | Done   | Split file into pieces |
| `file`       | Done   | Determine file type via magic bytes |

### Archive and compression (5)

| Command      | Status | Notes |
|--------------|--------|-------|
| `tar`        | Done   | Create, extract, list (`-f -` reads stdin / writes stdout for piping) |
| `gzip`       | Done   | Compress files (DEFLATE, clean-room CRC-32) |
| `gunzip`     | Done   | Decompress gzip files |
| `zcat`       | Done   | Decompress and print to stdout |
| `unzip`      | Done   | Extract ZIP archives |

### Disk usage (2)

| Command      | Status | Notes |
|--------------|--------|-------|
| `du`         | Done   | Estimate file space usage |
| `df`         | Done   | Report filesystem disk space usage |

### Network utilities (2)

| Command      | Status | Notes |
|--------------|--------|-------|
| `curl`       | Done   | HTTP client — GET/POST/HEAD/PUT/DELETE/PATCH; multi-URL (positional + `--next`/`-:` + `--remote-name-all`); request shaping (`-H`, `-d`/`--data-ascii`/`--data-binary`/`--data-raw`/`--data-urlencode`/`--json`, `@file` bodies, `-F`/`--form-string` multipart, `-T`/`--upload-file`, `-G`/`--get`, `-r`/`--range`, `-b`/`--cookie` (literal or `@file`), `-z`/`--time-cond`, `--compressed` with gzip/deflate decoding); auth (`-u` basic, `--oauth2-bearer`, `-n`/`--netrc`/`--netrc-file`, `--aws-sigv4`, `-A`, `-e`); config expansion (`-K`/`--config FILE`); response shaping (`-i`/`-D`, `-o`/`-O`/`-J`, `--output-dir`, `--create-dirs`, `-w` tokens incl. `url_effective`, `method`, `scheme`, `urlnum`, `num_headers`, `header{X}`, `json`, `header_json`, `http_version`, `content_type`, `size_download`, `--fail`/`--fail-with-body`); sandbox limits (`--max-time`, `--connect-timeout`, `--max-filesize`, `--max-redirs`, `--retry*`). Cosmetic/transport-controlled flags (`--http*`, `--tlsv*`, `-4`/`-6`, `--tcp-*`, `--resolve`, `-#`, `-k`, `--path-as-is`, `-Z`/`--parallel`, …) are silently accepted so scripts written for real curl run unchanged. Tier-4 flags that require capabilities the sandbox cannot safely expose (FTP/SMTP, proxies, client certs, Unix sockets, DoH, SOCKS, cookie jar, `--trace*`, …) are rejected with a "not supported in sandbox" diagnostic. |
| `wget`       | Done   | File downloader — multi-URL, `-O`/`--output-document` (incl. `-`), `--header`, `--user=`/`--password=` basic auth, `--post-data=`, `--tries=`, `--timeout=`, `--content-disposition`, quiet mode. `--no-check-certificate`/`--server-response`/`--show-progress` accepted as silent no-ops. |

Network access requires an allowlist or structured policy configured at
sandbox initialization. Without it, both commands return an error. Standalone
Node consumers must install the shipped no-redirect network broker; the
browser fixture intentionally refuses its synchronous XHR path. See
[ADR-0021](docs/adr/adr-0021-network-capability.md).

---

## Parameter Expansion

| Operator                       | Meaning |
|--------------------------------|---------|
| `$var` / `${var}`              | Value of variable |
| `${#var}`                      | String length of value |
| `${var:-word}`                 | Value if set and non-empty, else `word` |
| `${var-word}`                  | Value if set, else `word` |
| `${var:=word}`                 | Value if set and non-empty, else assign and use `word` |
| `${var=word}`                  | Value if set, else assign and use `word` |
| `${var:+word}`                 | `word` if set and non-empty, else empty |
| `${var+word}`                  | `word` if set, else empty |
| `${var:?word}`                 | Value if set and non-empty, else error with `word` |
| `${var#pattern}`               | Remove shortest prefix matching `pattern` |
| `${var##pattern}`              | Remove longest prefix matching `pattern` |
| `${var%pattern}`               | Remove shortest suffix matching `pattern` |
| `${var%%pattern}`              | Remove longest suffix matching `pattern` |
| `${var/pat/rep}`               | Replace first occurrence of `pat` with `rep` |
| `${var//pat/rep}`              | Replace all occurrences of `pat` with `rep` |
| `${var/#pat/rep}`              | Replace `pat` anchored at start |
| `${var/%pat/rep}`              | Replace `pat` anchored at end |
| `${var:offset}`                | Substring from `offset` |
| `${var:offset:length}`         | Substring from `offset`, `length` chars |
| `${var^}`                      | Uppercase first character |
| `${var^^}`                     | Uppercase all characters |
| `${var,}`                      | Lowercase first character |
| `${var,,}`                     | Lowercase all characters |
| `${var@Q}`                     | Quote value for reuse as shell input |
| `${var@E}`                     | Expand backslash escape sequences |
| `${var@U}`                     | Uppercase all characters |
| `${var@L}`                     | Lowercase all characters |
| `${var@u}`                     | Uppercase first character |
| `${var@A}`                     | Assignment statement form (`declare -- var="value"`) |
| `${!var}`                      | Indirect expansion (value of the variable named by `$var`) |
| `${!prefix*}` / `${!prefix@}`  | Names of all variables with the given prefix |
| `${arr[@]}` / `${arr[*]}`      | All elements of indexed or associative array |
| `${arr[N]}`                    | Single element of array by index or key |
| `${#arr[@]}` / `${#arr[*]}`    | Number of elements in array |
| `${!arr[@]}` / `${!arr[*]}`    | All keys/indices of array |
| `$?`                           | Exit status of last command |
| `$#`                           | Number of positional parameters |
| `$@` / `$*`                    | All positional parameters |
| `$0`                           | Script/shell name |
| `$1`–`$N`                      | Positional parameters |

---

## Arithmetic

Arithmetic is available in `$(( ))`, `(( ))`, `let`, and `declare -i` contexts. The evaluator is a full recursive-descent parser.

### Operators (in precedence order, lowest to highest)

| Operator            | Description |
|---------------------|-------------|
| `expr ? a : b`      | Ternary conditional |
| `,`                 | Comma (evaluate both, return right) |
| `= += -= *= /= %= <<= >>= &= ^= \|=` | Assignment operators |
| `\|\|`              | Logical OR |
| `&&`                | Logical AND |
| `\|`                | Bitwise OR |
| `^`                 | Bitwise XOR |
| `&`                 | Bitwise AND |
| `== !=`             | Equality |
| `< > <= >=`         | Comparison |
| `<< >>`             | Bitwise shift |
| `+ -`               | Addition, subtraction |
| `* / %`             | Multiplication, division, modulo |
| `**`                | Exponentiation |
| `! ~ - +`           | Unary NOT, bitwise complement, negate, plus |
| `++ --`             | Prefix and postfix increment/decrement |

### Literal formats

| Format          | Example |
|-----------------|---------|
| Decimal         | `42` |
| Hexadecimal     | `0xff` |
| Binary          | `0b1010` |
| Octal           | `0755` |
| Arbitrary base  | `16#ff`, `2#1010` |

---

## Glob and Pathname Expansion

| Pattern          | Description |
|------------------|-------------|
| `*`              | Match any string (not leading `.` unless `dotglob`) |
| `?`              | Match any single character |
| `[abc]`          | Match any character in the set |
| `[a-z]`          | Match any character in the range |
| `[!abc]`         | Match any character not in the set |
| `**`             | Match zero or more directories (requires `shopt -s globstar`) |
| `?(pat)`         | Match zero or one occurrence (requires `extglob`) |
| `*(pat)`         | Match zero or more occurrences (requires `extglob`) |
| `+(pat)`         | Match one or more occurrences (requires `extglob`) |
| `@(pat)`         | Match exactly one occurrence (requires `extglob`) |
| `!(pat)`         | Match anything except `pat` (requires `extglob`) |

`extglob` is enabled by default. `nullglob`, `dotglob`, `globstar`, `nocasematch`, `nocaseglob`, `failglob` are available via `shopt`.

---

## Shell Options

### `set` options

| Flag      | Long name    | Description |
|-----------|--------------|-------------|
| `-e`      | `errexit`    | Exit on any command failure |
| `-E`      | `errtrace`   | Inherit `ERR` traps into functions, `source`, and nested shell evaluations |
| `-u`      | `nounset`    | Error on unset variable reference |
| `-x`      | `xtrace`     | Print commands before executing (`PS4` prefix) |
| `-f`      | `noglob`     | Disable glob expansion |
| `-a`      | `allexport`  | Auto-export all variable assignments |
| `-C`      | `noclobber`  | Prevent `>` from overwriting existing files |
| `-n`      | `noexec`     | Parse input but skip executing subsequently submitted commands |
| `-p`      | `privileged` | Track privileged-mode flag for compatibility |
| `-T`      | `functrace`  | Inherit `DEBUG`/`RETURN` traps into functions, `source`, and nested shell evaluations |
| `-v`      | `verbose`    | Echo subsequently submitted input before execution |
| `-o pipefail` | `pipefail` | Pipeline exit status is rightmost non-zero stage |

### `shopt` options

| Option           | Default | Description |
|------------------|---------|-------------|
| `extglob`        | on      | Enable extended glob patterns |
| `nullglob`       | off     | Unmatched globs expand to nothing |
| `dotglob`        | off     | Globs match filenames starting with `.` |
| `globstar`       | off     | `**` matches directories recursively |
| `nocasematch`    | off     | Case-insensitive `case` and `[[ =~ ]]` matching |
| `nocaseglob`     | off     | Case-insensitive glob matching |
| `failglob`       | off     | Error when glob matches nothing |
| `lastpipe`       | off     | Last pipeline stage runs in current shell |
| `expand_aliases` | on      | Enable alias expansion |
| `sourcepath`     | on      | `source` searches `PATH` for bare names |

---

## Special Variables

| Variable       | Description |
|----------------|-------------|
| `?`            | Exit status of last command |
| `$$`           | Virtual shell PID |
| `$!`           | Last background PID slot (currently `0` without real job control) |
| `#`            | Number of positional parameters |
| `@` / `*`      | All positional parameters |
| `$_`           | Last argument of the previous command |
| `$-`           | Active single-letter shell flags |
| `0`            | Shell/script name |
| `IFS`          | Input field separator (default: space, tab, newline) |
| `HOME`         | Home directory for tilde expansion and `cd` |
| `PWD`          | Current working directory |
| `OLDPWD`       | Previous working directory |
| `PATH`         | Colon-separated search path for `source` when `shopt sourcepath` is enabled |
| `OPTIND`       | Current index for `getopts` |
| `REPLY`        | Default variable for `read` |
| `PIPESTATUS`   | Array of exit statuses for last pipeline stages |
| `PS4`          | Prompt prefix for `set -x` xtrace output |
| `LINENO`       | Current line number in executing script |
| `FUNCNAME`     | Stack of currently executing function names |
| `BASH_SOURCE`  | Stack of filenames for `source` calls |
| `MAPFILE`      | Default array for `mapfile` |

---

## Build Targets

| Target | Triple | FS Backend | Python | Build Command |
|--------|--------|------------|--------|---------------|
| **Standalone** | `wasm32-unknown-unknown` | `MemoryFs` (in-process) | No Python runtime | `just build-standalone` |
| **Pyodide legacy** | `wasm32-unknown-emscripten` | `EmscriptenFs` (libc, shared with Python) | In-process via `PyRun_SimpleString` | `just build-pyodide` |

### Pyodide-only legacy profile

The following commands and package-install behavior belong to the separate
Pyodide build. They are not present in, and are not downloaded by, the
standalone artifact.

| Command | Flags | Description |
|---------|-------|-------------|
| `python` / `python3` | `-c CODE` | Run Python code in-process; stdin from heredoc/pipe also supported |
| `pip` / `pip3` | `install PKG [PKG ...]` | Install pure-Python packages via micropip |

Python stdout and stderr are captured and surfaced as normal `Stdout`/`Stderr` worker events. File I/O from Python goes through the same Emscripten filesystem the shell uses.

### Python package installation (micropip)

The Pyodide build includes [micropip](https://micropip.pyodide.org/) for installing Python packages at runtime. Packages are installed into the in-process virtual filesystem and become importable immediately.

**Supported install methods:**
- `pip install <package>` — resolved from Pyodide CDN or PyPI
- `pip install https://host/pkg-1.0-py3-none-any.whl` — direct URL (requires `allowedHosts`)
- Session API: `session.installPythonPackages("package")` from JavaScript

**What works:**
- Pure-Python wheels (`py3-none-any`): six, attrs, click, packaging, beautifulsoup4, networkx, idna, certifi, pyyaml, jinja2, toml, tomli, markupsafe, chardet, pyparsing, more-itertools, decorator, wrapt, pluggy, and many others
- Pyodide pre-compiled packages with pure-Python fallbacks (e.g., pyyaml)

**What doesn't work yet:**
- C extension packages that require `dlopen` (numpy, pandas, scipy, regex) — install succeeds but import fails because the build uses `MAIN_MODULE=2`. Switching to `MAIN_MODULE=1` with explicit symbol filtering would enable these.

**Security:**
- Network installs require `allowedHosts` configured at session creation
- `file:` URIs are rejected
- `emfs:` installs (from the in-sandbox filesystem) always work
- Installs are session-local and do not persist

---

## Known divergences and degraded commands

These are known, intentional, or environmental limits. Each is either covered
by a test that documents the divergence or listed here so it is not mistaken
for full compatibility.

### Stubs and degraded semantics

| Command | Behaviour | Impact |
|---------|-----------|--------|
| `sleep` | Returns immediately; does not delay | Timing-dependent scripts complete instantly. No wall-clock stall is modelled. |
| `nproc` | Returns a fixed value (see `trivial_ops`) | Parallelism decisions see a constant core count. |
| `timeout` | Accepted, no real timer is enforced for in-process commands | A command that never terminates is only bounded by the host step budget, not by `timeout`. |
| `ulimit` | Read-only/no-op reporting | Resource limits are governed by the host, not by `ulimit`. |
| `stat` | Report format is a compatible subset | Some format specifiers and fields are not implemented. |
| `date` | Deterministic under the test clock; live clock on native hosts | Format coverage is a subset of GNU `date`. |
| `&` (background) | Parsed but runs synchronously | No `jobs`/`fg`/`bg`. |

### Environment-dependent differences

- **Locale**: wasmsh sorts and matches under a fixed UTF-8 / `LC_ALL=C`
  ordering. `LC_ALL=C sort` is differentially tested; other locales are not
  modelled. Assigning `LC_ALL` does not change collation.
- **Line endings**: input scripts with CRLF line endings are read literally
  (the CR is part of the line). Real Bash on Windows/MSYS strips CR in some
  contexts. Scripts intended for wasmsh should use LF.
- **Symbolic links on Windows**: real Bash under Git Bash only creates true
  symlinks when `MSYS=winsymlinks:lnk` is set; the differential oracle sets
  this so `ln -s` comparisons are meaningful. wasmsh's VFS always models real
  symlinks.
- **`ls -l` metadata**: wasmsh renders a fixed owner/group and fixed
  timestamp (`Jan 1 00:00`) because the VFS has no ownership or mtime. Byte
  size and link targets match.

### Fixed in the differential-hardening pass

The following previously-silent divergences were found by the oracle and
fixed; each is now guarded by a case in `tests/suite/differential/`:

- `sort -k N -n`, `-k2n`, and `-t: -k2 -n`
- `sort` default stability/last-resort ordering and `-r`
- awk `print`/`printf` redirection (`>`, `>>`, `| "cmd"`)
- awk array parameters passed by reference
- awk `$`-anchored regex leftmost-longest matching
- awk `printf "%c", N` from a numeric code
- `while read` over a pipe / heredoc / file redirect (single-iteration bug)
- `${VAR:-N}` when the operand is numeric
- `if` without `else` returning non-zero (and tripping `set -e`)
- `$(cmd)` assignment status (`x=$(false); echo $?`)
- group/subshell redirection `{ ...; } > f`, `( ... ) > f`
- quoted here-doc delimiters (`<<'EOF'` must not expand)
- backslash-newline line continuation (unquoted and in double quotes)
- `trap ... EXIT` firing at script end
- `ln -s`, `cp -s`, `ln -sf`, and `readlink` for real symlinks
- `grep -A/-B/-C` context lines, including glued forms (`-A2`)
- `wc -l` on input without a trailing newline, and GNU column alignment
- `getopts` with `OPTARG`/`OPTIND`, clustering, attached args, `--`, and `:`
- `tar -C` directory switching for create and extract
- `sh file` / `sh -c` running as a child shell, with options reset and no
  variable/function leakage into the parent, and stdin redirection forwarded
  (`sh -c 'cat' < file`)

### Fixed in the sandbox-hardening pass

A follow-up differential audit against the in-process runtime
(`wasmsh-dev`) surfaced a second batch. Each is guarded by a case in
`tests/suite/differential/` or a Rust unit test:

- **Parser depth bounds (availability).** The recursive-descent shell parser,
  the lexer's `$( )` scanner, the arithmetic evaluator, and the awk expression
  parser had no recursion limit; deeply nested input overflowed the stack. A
  stack overflow is a hard abort — in the WASM build it is an uncatchable trap
  that also permanently poisons the `WasmShell` instance (wasm-bindgen's borrow
  guard is never released). All four now reject over-deep input with a normal
  error. The limits are `MAX_NESTING_DEPTH=24` (parse), `MAX_LEX_DEPTH=48`
  (lexer substitutions), `MAX_ARITH_DEPTH=64`, `MAX_AWK_DEPTH=64`, and the
  runtime `MAX_RECURSION_DEPTH=48` (shared by eval/source/function calls and
  command substitution), sized against the ~1 MiB native/WASM stack (measured
  overflow at ~59/84/~1000/~200/~120 respectively).
- `return` did not unwind the current function or loop; execution continued
  past it. It now sets a dedicated unwind flag honored by functions and loops.
- `$((x/0))` and `$((x%0))` returned `0` instead of failing the command.
- Arithmetic `$`-parameters were dropped: `$(( $1 + 1 ))` used the literal `1`,
  and `$(( $# ))` was `0`. `$1`, `$#`, `$?`, `${x}`, `${a[i]}` now resolve.
- `stat -c %a` / `%A` / `%f` returned hardcoded 644/755 instead of the VFS mode,
  contradicting `ls -l` and `[ -x ]`.
- `echo a#b` truncated at the `#`; `#` is only a comment at the start of a word.
- `${#@}` / `${#*}` returned the character count of the joined parameters
  instead of the number of positional parameters.
- `${arr[@]:offset[:length]}` expanded to an empty string.
- `${x:?msg}` printed the message to stdout and exited 0; it now fails the
  command (fatal for a non-interactive script).
- A write to a `readonly` variable was silently dropped; it now fails the
  command with a diagnostic.
- `for` word lists split quoted and backslash-escaped text (`for w in "a b"`
  yielded two fields) and did not split unquoted command substitution. Field
  splitting is now quote-aware, and `"${arr[@]}"` / `"$@"` yield one field per
  element.

### Fixed in the audit-driven pass (v0.9.2)

A differential audit that used wasmsh as the only bash sandbox reported a batch
of divergences. Re-testing the "kills the whole process" class directly showed
none of those triggers terminates the runtime; the real defects were silent
wrong results and scope leaks. Each is guarded by a case in
`tests/suite/differential/`:

- `sed 's/^/X/'` dropped the first character (the regex engine treats a leading
  `^` as consuming); `s/[[:space:]]*$//` left trailing space; `s/a*/Y/g` swallowed
  the character after a zero-width match. `^`/`$` are now line anchors enforced
  by the caller, and zero-width global matches follow sed's suppression rule.
- Pathname expansion was word-scoped: `"$dir"/*.sh` did not glob, and an
  unquoted `$p` whose value contained `*` did not glob. Quoting is now a
  per-byte mask, so quoted bytes stay literal while unquoted metacharacters
  remain active; `\*` matches a literal asterisk.
- `${v: -2}` and `${v:1:-2}` (negative string offset/length) returned empty.
- An unquoted here-document did not expand `$(( ))`, `$( )` or backticks.
- A child shell (`sh -c`, `sh file`) inherited the caller's `set -u`/`-e`/
  `pipefail`; bash resets them. A child shell now starts with default options.
- A fatal expansion (nounset, `${x:?}`, recursion) or `exit` inside `( … )`
  ended the whole runtime instead of just the subshell.
- Functions and aliases defined in `( … )` leaked into the parent.
- `trap … EXIT` set inside a child `sh script` never fired.
- `read` re-joined IFS-split fields with spaces, corrupting tab-separated data,
  and ignored `-r`. Separators are preserved and `-r`/backslash semantics work.
- `printf -- '%s\n' x` printed the literal format `--`.
- `xargs -I{}` (attached replacement string) was a parse error.
- `cmp` did not accept `-` for standard input.

### Fixed in the output-process-substitution pass (v0.9.3)

`tee >(wc -c > file) <<< hi` (and any `>(...)` whose consumer has its own
redirection, compound command, or external command) aborted the whole WASM
module with

```
internal error: entered unreachable code: buffered pipeline stage requires runtime access
```

Root cause: the `>(cmd)` builder did not mirror the `<(cmd)` builder's guard.
When a pipeline stage needs runtime access (`BufferedCommand` for `cmd > file`
or a compound command, `External` for a host executable) the runner must own an
isolated runtime. `<(cmd)` returned `None` in that case and fell back to a
buffered capture; `>(cmd)` built the runner anyway and, at end of command,
polled it through the no-runtime path — where the invariant check used
`unreachable!()`. The standalone `WasmShell` always installs an external spec
handler, which disables the isolated runtime, so this path was reached on every
call.

Fixes:
- The `>(cmd)` builder now shares the `<(cmd)` guard: if any stage requires
  runtime access and no isolated runtime can be cloned, it returns `None` and
  the command uses the buffered fallback. No panic, correct output.
- A utility that opens the substitution path directly (`tee >(consumer)`,
  `cp x >(consumer)`) writes it through the filesystem rather than the runtime
  sink. The sink now falls back to reading and removing that file, so the
  consumer still receives the payload instead of silently getting nothing.
- The two `poll_without_runtime` / `close_without_runtime` arms that held the
  invariant are no longer panicking; if the invariant is ever violated, the
  stage is closed and reported finished so the shell instance survives.

Note on recovery: wasm32-unknown-unknown is compiled with `panic = "abort"`
(verified via `rustc --print cfg`), so a Rust panic is an uncatchable trap and
`std::panic::catch_unwind` cannot be used to restore the wasm-bindgen borrow
state. Eliminating reachable panics is the only durable mitigation; the
invariant arms above now fail soft instead of aborting.

### Fixed in the subshell-trap pass (v0.9.4)

A `trap … EXIT` installed inside `( … )` did not fire when the subshell ended:

```sh
( trap 'echo INNER' EXIT; echo body ); echo after
# bash:  body / INNER / after
# wasmsh (before): body / after
```

The subshell now saves the inherited EXIT trap, clears it for the subshell
scope, runs the body, and — if the body installed a trap — fires it as the
subshell ends, before the parent continues. A trap installed *outside* the
subshell still does not fire inside it (bash fires it only when the outer shell
exits). Guarded by `tests/suite/differential/subshell_exit_trap.toml`.

### Fixed in the tool-completeness pass (v0.9.5)

Driven by a bash differential audit's remaining gap list. Each fix has a
`tests/suite/differential/` case compared byte-for-byte against bash/GNU tools.

- **`[ -f ]` / `[[ -f ]]` ignored the cwd.** File predicates passed the raw
  operand to `Vfs::stat`, which is cwd-agnostic, so `[ -f rel.txt ]` failed
  while `[ -f /abs/rel.txt ]` succeeded. Relative operands now resolve against
  `$PWD` for `-f -d -e -s -r -w -x -O -G -N` and for `[[ -nt/-ot/-ef ]]`.
- **`cd relative` did not resolve, and never failed.** `cd sub` stored the
  literal string `sub` as the cwd, so every later relative path resolved
  against a bogus directory (`sub/sub/...`); a missing target silently
  "succeeded". `cd` now normalizes against the current directory and reports
  `No such file or directory` / `Not a directory` with status 1, as bash does.
- **`/tmp` and `$HOME` did not exist.** The VFS started with only `/`, so
  `cd /tmp` failed and staging temp files there needed an explicit `mkdir`.
  `/tmp`, `/home`, and `/home/user` are now seeded, matching the POSIX layout
  the AI-facing environment contract assumes.
- **`od` was missing.** Implemented `od` with `-A {o,d,x,n}`, `-t {a,c,d,o,u,x}`
  with unit sizes, the legacy `-b -c -d -o -x -a` flags, `-j`/`-N`, `-w`, `-v`,
  and repeated-line `*` collapse. Output is byte-identical to GNU od across the
  tested flag combinations, including C-locale control-byte names.
- **`join` was missing.** Implemented `join` with `-1/-2/-j`, `-t`, `-a`, `-v`,
  `-o` (including `0` and `auto`), `-e`, `-i`, duplicate-key cross products, and
  the out-of-order diagnostic plus non-zero exit.
- **`tar --exclude` was missing.** A pattern containing `/` matches the whole
  member name; one without matches the basename at any depth, so a directory
  exclusion prunes its subtree. Both `--exclude PAT` and `--exclude=PAT`.
- **`sh -n` / `sh -x` were missing.** `-n` parses and reports a syntax error
  (status 2) without executing; `-x` traces commands to stderr while running
  them. Flags bundle (`-nx`) and combine with `-c`.
- **`jq --version` / `yq --version`** were parsed as filters and reported as
  parse errors; both now print their version line and exit 0.
- **awk `printf "%c"` for a numeric code > 0x7F** emitted the value's UTF-8
  encoding (2 bytes for 255) instead of the single raw byte. The runtime pins
  `LC_ALL=C`, so a numeric `%c` is a byte code; `sprintf` keeps character
  semantics. The differential oracle is now pinned to `LC_ALL=C` too, so a
  locale difference is not misreported as a divergence.

### Fixed in the AI-shell stability pass (unreleased)

Four defects that an external black-box audit found while using the standalone
WASM as the only bash sandbox. Each is reproduced by a differential case that
compares byte-for-byte against a real `bash`.

- **Command-prefix assignments were permanent.** `V=1 echo hi` left `V=1` in the
  shell, and the leak survived into later `exec` calls, so one script could
  contaminate the next and defeat the "stable, reproducible environment" goal.
  Prefix assignments are now scoped to the command; `export V` / `readonly V` /
  `declare -x|-r V` still promote the value, matching bash.
  Guarded by `tests/suite/differential/prefix_assignment_temporary.toml`.
- **`sh -c '<cmd>' < file` lost stdin.** The child shell was launched with no
  stdin target, so `cat` read the inherited (empty) stream and failed with
  "missing operand". The launching command's input redirection is now forwarded
  to `sh -c` and `sh file`. Guarded by
  `tests/suite/differential/child_shell_stdin_redirect.toml`.
- **`"$@"` collapsed into one joined field.** Multi-field parameter expansion is
  now separate from single-string expansion: `"$@"` and `"${a[@]}"` yield one
  field per element (zero fields when empty), on a pipe's right-hand side as
  well. The VM fast path is bypassed for these words. Guarded by
  `tests/suite/differential/quoted_at_multi_field.toml`.
- **`${v%%[ ]*}` and other bracket-class strips returned empty.** `simple_glob_match`
  only understood `*` and `?`, and `try_expand_array_single_element` mistook
  `${v%%[ ]*}` for an array subscript. The matcher now supports `[abc]`,
  `[a-z]`, and `[!…]`, the strip operators match the full pattern, and only a
  valid identifier can be an array base. Guarded by
  `tests/suite/differential/param_strip_bracket_class.toml`.

### Remaining known divergences (not yet fixed)

- `${arr[@]:1:-1}` (negative slice length on an array): bash reports an error,
  wasmsh returns the truncated slice. Negative-length string slices match bash.
- `timeout`, `sleep`, and `nproc` remain intentional stubs (see the table
  above): `timeout` never executes its command (always 125), `sleep` returns
  immediately, `nproc` is always 1. These are deliberate because the sandbox is
  deterministic and has no wall clock or OS process model.
- `readonly` assignment fails the command with status 1 but does not abort a
  script; bash's abort behaviour here is inconsistent (it depends on `;` versus
  newline separation), so wasmsh uses the predictable non-fatal form.
- `$((1/0))` and unbounded recursion recover with a non-zero status where bash
  aborts the shell or overflows the stack; wasmsh is the more forgiving of the two.
- `awk` has no `getline`.

---

## Non-Goals

- Not a BusyBox port or Bash fork — clean-room implementation
- No real OS processes in the browser — all commands run in-process
- No kernel or network administration tools
- No TTY/terminal emulation
- No full job control (`fg`, `bg`, `jobs`); `&` parses but runs synchronously
- No OS process model or job control; signals are modeled at shell-session level and can be injected by the host, but `fg`/`bg`/`jobs` semantics still do not exist
- No coprocesses
- GPL/AGPL/SSPL code is forbidden in the core
