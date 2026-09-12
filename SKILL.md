---
name: wasmsh-bash-sandbox
description: How to write commands for wasmsh, a Bash-compatible shell sandbox, when it is the execution environment for a task. Use when running shell commands inside wasmsh, when a command fails unexpectedly with a non-zero status, or when a script depends on time, networking, background jobs, or host-specific tools.
---

# Working inside wasmsh

wasmsh is your shell for this task. It behaves like Bash, but it is a **sandbox**:
no host access unless explicitly granted, and a few commands are deliberately
limited. Write normal Bash — then apply the boundaries below so you don't mistake a
refusal for a bug, or a stub for a real result.

## What you can rely on

- **Shell language**: variables, quoting, `$@`/arrays, functions, `if`/`case`/`for`/`while`,
  command substitution `` $( ) ``/backticks, arithmetic `$(( ))`, pipelines, `|&`,
  redirections (`> >> < << <<< &>`), here-docs, `trap ... EXIT`, `set -euo pipefail`,
  `$?`, `PIPESTATUS`.
- **Utilities** (88): `grep sed awk sort uniq cut tr head tail wc find xargs diff patch`,
  `jq yq`, `tar gzip zip`, `sha256sum base64 od cmp`, `date stat chmod ln readlink`,
  `printf`, `join`, and more. `command -v NAME` and `type NAME` list what exists.
- **Filesystem**: a private POSIX VFS. Paths start at `/`; `$HOME` is `/home/user`,
  `/tmp` exists, and the shell starts in `/`. Create directories before writing into them.
- **Persistence within a session**: the working directory, variables, and files survive
  between commands in the same session. Different sessions are isolated.

Start scripts with `set -euo pipefail` when you want a failure to stop the script, and
check `$?` / `PIPESTATUS` when it matters. Files and pipes carry raw bytes; decoding is
only for display.

## The boundaries that matter

**Networking is off by default and host-controlled.** `curl`/`wget` work only for hosts
the host has allowlisted, and only over `http`/`https`. A denial is not a transient
network error: the exit status is non-zero and stderr explains the policy. Do not
blindly retry a denied request. Redirect targets are checked too, so a redirect to a
blocked host simply fails.

**External commands exist only if the host registered them.** If so, they behave like
normal commands, including in pipelines, redirections, `$?`, and `PIPESTATUS`. Map the
status: `127` = not registered, `126` = could not start / host has no native processes,
`124` = timed out, `125` = input/output limit hit.

**Time, background jobs, and a few tools are limited.** These are intentional, not
bugs — do not design around them as if they work:

| Trap | What happens | Do instead |
| --- | --- | --- |
| `sleep N` | returns immediately | don't rely on delays for ordering; the sandbox has no wall-clock stall |
| `timeout N cmd` | does **not** run `cmd`; returns 125 | rely on the host's own limits |
| `&` / `jobs` / `fg` / `bg` | no real background jobs | run commands sequentially |
| `$SECONDS` | does not advance within one command | don't use it to measure elapsed time inside a script |
| `nproc` | fixed value | don't derive parallelism from it |
| `awk` `getline` | not implemented | use a `while read` loop or `awk`'s main record loop |

**No terminal.** There is no PTY, so interactive/TUI programs and cursor control do not
work. Anything needing a real terminal will fail rather than render.

## Environment contract

- Text is UTF-8; timezone is UTC; locale is `C`.
- The host environment, `PATH`, and startup files are **not** inherited — only explicitly
  granted variables are present. Don't assume a variable or host path exists.
- Host files are invisible unless the host mapped them; use VFS paths, not `C:\...` or
  `/home/you/...` host paths.
- Unsupported behavior returns non-zero **and** writes a diagnostic; it never silently
  pretends to succeed. Always check the status before trusting output.

## When something fails

1. Read stderr — refusals and policy errors are explained there.
2. Check the exit status (`echo $?`), including inside pipelines (`PIPESTATUS`).
3. If it is a capability refusal (network, external command, native process), do not
   retry it; choose a supported approach or report the limitation.
4. For the full supported surface and the current divergence list, read `SUPPORTED.md`
   that ships next to this file.
