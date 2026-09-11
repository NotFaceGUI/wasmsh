# Sandbox and Capabilities

This page documents what the wasmsh sandbox prevents, what it allows, and
the knobs a host has for tuning that surface.

## Threat model

wasmsh is designed to safely execute **untrusted shell scripts** inside a
host application that is itself trusted. The host (a Node program, a
browser page, a Rust embedder) decides what the script can see; the
script cannot escape that decision through any documented surface.

In particular:

- The script has **no host filesystem access**. All file operations go
  through `BackendFs`, which is either an in-process `MemoryFs` or, in
  Pyodide builds, an `EmscriptenFs` shared with the in-sandbox Python
  interpreter. Neither backend touches the host's real filesystem.
- The script has **no network access** unless the host explicitly
  allowlists hostnames at `Init` time.
- The WASM script has **no ambient process model**. There is no `fork`, no
  shell-spawned `exec`, and no job control. A trusted host may explicitly
  register a fixed external executable; only that registration can create a
  host process, and `kill`, `wait`, and `jobs` still are not shell features.
  Modeled signals target the shell session; the Node stream adapter separately
  reaps or kills its registered process tree on completion/cancel.
- The script **cannot run arbitrary native code**. Builtins, utilities,
  and external command handlers are the only ways to enter native code,
  and all of them are registered statically by the host.
- The script's **execution time is bounded** by a step budget that the
  host configures.
- All system-information commands (`id`, `whoami`, `hostname`, `uname`)
  return deterministic virtual values. The script cannot fingerprint the
  host.

The sandbox is **not** designed to protect the host from:

- **Side channels in the host's network** when `allowed_hosts` is
  populated. If you allow `*.example.com`, a malicious script can
  exfiltrate via DNS or via the request shape. Allowlist carefully.
- **Resource exhaustion in the embedder.** A very large `step_budget`
  combined with a memory-hungry script can OOM the wasm module.
- **Bugs in builtins or utilities** that mishandle input. We treat any
  panic in this code as a bug.

## Step budget

The VM is cooperative. Every IR instruction increments a counter; when it
exceeds `step_budget`, execution stops with an `Exit` event and a
diagnostic. This bounds the wall-clock time of any single `Run` call by a
host-controllable factor.

Set `step_budget` via `HostCommand::Init`:

```rust
rt.handle_command(HostCommand::Init {
    step_budget: 100_000,
    allowed_hosts: vec![],
    network_policy: None,
});
```

| Value     | Meaning |
|-----------|---------|
| `0`       | Unlimited (no budget enforced). Use only for trusted input. |
| `1_000`   | Tiny scripts only — useful for syntax-check style use. |
| `100_000` | Reasonable default for most short interactive commands. |
| `10_000_000` | Long-running scripts; expect noticeable wall time. |

The exact wall time per step depends on the host CPU and the instruction
mix, but as a rule of thumb 100k steps complete in well under a second on
modern hardware.

When the budget is hit the runtime emits a `Diagnostic(Warning, …)` and
an `Exit` event with a non-zero code. The next `Run` call starts fresh
(the budget is per-call, not per-session).

## Output limits

Independently of step budget, the runtime tracks total output bytes
produced by a single `Run` call. When that approaches a sandbox-friendly
ceiling, a diagnostic is emitted; if it crosses the hard limit, output is
truncated and the command exits.

This protects the host from `yes | head` style scripts that would
otherwise pin a buffer on the wire indefinitely.

## Network policy

wasmsh ships two networking utilities: `curl` and `wget`. Both use the same
`NetworkPolicy` matcher and every actual backend request is checked before I/O.
The default is `enabled: false`, which disables networking. The legacy
`allowed_hosts` array maps to enabled allowlist mode.

1. The host provides a list of patterns at `Init`.
2. `curl`/`wget` check the requested URL against the allowlist before any
   network call is made.
3. Denied requests fail with an error and a diagnostic; they never touch
   the network.

Structured configuration example:

```json
{
  "network_policy": {
    "enabled": true,
    "default_action": "deny",
    "allow": ["example.com", "*.example.com:443"],
    "deny": ["blocked.example.com"]
  }
}
```

### Pattern syntax

| Pattern                  | Matches                                     |
|--------------------------|---------------------------------------------|
| `api.example.com`        | Exactly that host on any port               |
| `*.example.com`          | Any strict subdomain (but not `example.com` itself) |
| `*`                      | Any valid HTTP(S) host when explicitly configured |
| `192.168.1.100`          | That IP exactly                             |
| `[2001:db8::1]:8080`     | That IPv6 address and port                 |
| `api.example.com:8080`   | That host on that specific port             |

An empty legacy list disables network access entirely. Structured policies
support `default_action: "deny"` for allowlist mode, `"allow"` for blacklist
mode, and combinations of both lists. A matching `deny` rule is always
evaluated first and cannot be overridden by `allow`. Sending both policy
forms is an initialization error, even when `allowed_hosts` is empty.

Rules and URLs are normalized for case, a trailing dot, IDNA, effective
HTTP/HTTPS default ports, IPv6 representation, and label boundaries. Partial
wildcards, regular expressions, CIDR, userinfo, malformed hosts, and invalid
ports are rejected during initialization. Redirect following is manual and
rechecks every hop; a browser synchronous XHR backend is refused unless a
trusted redirect-aware broker is installed.

See [ADR-0021](../adr/adr-0021-network-capability.md) for the design
rationale.

## Virtual system commands

The following commands return fixed values, regardless of the host:

| Command       | Output |
|---------------|--------|
| `whoami`      | `user` |
| `id`          | `uid=1000(user) gid=1000(user) groups=1000(user)` |
| `hostname`    | `wasmsh` |
| `uname`       | `wasmsh` |
| `uname -m`    | `wasm32` |
| `uname -a`    | `wasmsh wasmsh 0.1.0 wasm32 wasmsh` |

This is a deliberate design choice (see
[Design decisions: Virtual system commands](../explanation/design-decisions.md#virtual-system-commands)).
Reproducible output for tests and no host fingerprinting.

## Clock capability

Standalone production sessions use a host-installed synchronous clock
callback. Each `date` command samples the callback once, and SigV4 uses the
same capability. The callback returns a safe integer Unix timestamp in
milliseconds; a missing, throwing, or invalid callback fails the command
instead of fabricating a startup timestamp.

```js
shell.set_clock_callback(() => Date.now());
shell.set_fixed_time_ms(1767225600000n); // explicit deterministic test mode
shell.clear_clock_callback();            // time-dependent commands fail
```

The low-level legacy `UtilContext` may use `WASMSH_DATE` only when no runtime
clock provider is installed. It is not a production override and does not
change the shared SigV4 clock. `$SECONDS` and execution timing use a separate
monotonic clock. VFS file timestamps retain their existing virtual semantics.

## Recognised environment variables

| Variable        | Read by    | Effect |
|-----------------|------------|--------|
| `WASMSH_DATE`   | legacy low-level `date` | Explicit compatibility input only when no runtime clock provider is installed; it cannot override standalone production clock callbacks. |
| `HOME`          | `cd`, tilde expansion | Seeded to `/home/user` at `Init`; the `~` target and `cd` destination. Explicit overwrites are preserved. |
| `PWD` / `OLDPWD`| `cd`       | `PWD` is seeded to `/` at `Init` and both are maintained by `cd`; readable by scripts. |
| `IFS`           | word splitting | Field separator characters. Defaults to space/tab/newline when unset. |
| `PATH`          | `command -v`, source resolution | Seeded to `/usr/bin:/bin` at `Init`; search path for commands and `source` lookups. |
| `BASH_REMATCH`  | `[[ … =~ … ]]` | Regex capture groups. |
| `RANDOM`        | dynamic    | 16-bit value from an internal XorShift PRNG; writable to reseed. |
| `LINENO`        | dynamic    | Current source line. |
| `SECONDS`       | dynamic    | Seconds since shell init; writable to reset the origin. |
| `FUNCNAME`      | dynamic    | Current function name (within a function call). |
| `BASH_SOURCE`   | dynamic    | Current source file (within a `source`d file). |
| `PIPESTATUS`    | dynamic    | Indexed array of exit codes from the most recent pipeline. |
| `REPLY`         | `read`     | Default destination for `read` without an explicit variable name. |
| `MAPFILE`       | `mapfile`  | Default destination for `mapfile`/`readarray`. |
| `OPTIND`        | `getopts`  | Next index to process. |

Setting `RANDOM` reseeds the PRNG; setting `SECONDS` resets the origin
the dynamic value is computed from.

## What the sandbox does *not* enforce

These items are out of scope for the sandbox itself; the host is
responsible for them:

- **Memory pressure.** A script can allocate a multi-megabyte string in
  the wasm heap. Cap your wasm module memory at the host level.
- **Wall-clock total runtime.** `step_budget` bounds steps, not all wall
  time. Registered native processes have host-side timeouts, while a host
  must still enforce an outer deadline around the WASM call.
- **Concurrent runtime instances.** Each `WorkerRuntime` is independent;
  if you spawn many you must manage them yourself.
- **Persistence.** State is in-memory and reset by `Init`. Use
  `WriteFile` and `ReadFile` to snapshot if you need persistence.

## See Also

- [Worker protocol reference](protocol.md) for the `Init` command and
  diagnostic events.
- [ADR-0009: Budgets and cancellation](../adr/ADR-0009-budgets-cancellation.md)
- [ADR-0021: Network capability](../adr/adr-0021-network-capability.md)
- [Design decisions: Cooperative VM](../explanation/design-decisions.md#cooperative-vm-with-step-budgets)
