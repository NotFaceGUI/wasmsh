# ADR-0021: Network Capability Model

## Status

Accepted

## Context

wasmsh is a fully sandboxed shell runtime with no network access by default. LLM agents using the sandbox frequently need to fetch data from APIs or download files. Rather than opening unrestricted network access, we need a controlled mechanism where the sandbox creator specifies exactly which hosts are reachable.

## Decision

Network access uses a **capability-based `NetworkPolicy` model**. The sandbox
creator provides a structured policy at initialization. The default is
`enabled: false`, meaning no network access. The legacy `allowed_hosts` array
is retained as an enabled allowlist shorthand; both forms cannot be supplied
at once.

### Architecture

A `NetworkBackend` trait in `wasmsh-utils` provides the network capability to `curl` and `wget` utilities via `UtilContext`, mirroring how `BackendFs` provides filesystem access via `UtilContext.fs`.

```
HostCommand::Init {
    step_budget,
    network_policy: {
        enabled: true,
        default_action: deny,
        allow: ["api.example.com"],
        deny: [],
    },
}
    │
    ▼
WorkerRuntime.network: Option<Box<dyn NetworkBackend>>
    │
    ▼
UtilContext.network: Option<&dyn NetworkBackend>
    │
    ▼
curl/wget → backend.fetch(HttpRequest) → HttpResponse
```

### Policy patterns

- Exact hostname: `api.example.com`
- Wildcard subdomain: `*.example.com` (matches one or more subdomain labels,
  not bare `example.com`)
- Explicit wildcard: `*` (matches any valid HTTP(S) host)
- IP address: `192.168.1.100`
- IPv6 address: `[2001:db8::1]:8080`
- Host with port: `api.example.com:8080` (only matches that specific port)

Rules and URLs share case, trailing-dot, IDNA, effective-default-port, IPv6,
and label-boundary normalization. Partial wildcards, regex, CIDR, userinfo,
and malformed rules are rejected during initialization. Evaluation order is
disabled check, URL validation, deny, allow, then `default_action`; deny always
wins.

### Defense in depth

URL validation happens at three layers:

1. **Rust `NetworkPolicy`** in the runtime wrapper and backend — primary
   enforcement, runs before any I/O
2. **JS-side policy membrane/broker** — shared normalization and secondary
   enforcement for Pyodide fetches and installs
3. **Host transport** — the trusted broker rechecks every redirect hop and
   enforces request/response limits

### Synchronous HTTP

Utilities are synchronous (`fn -> i32`). Browser synchronous XHR cannot
reliably observe and block a cross-origin redirect before the next request is
sent, so the standalone and Pyodide browser adapters refuse network calls
unless a trusted redirect-aware broker is installed. The Node runner provides
such a broker and forces the underlying fetch to use manual redirects. A
trusted broker also removes sensitive headers on cross-origin redirects and
enforces timeout, redirect, and response-size limits.

## Alternatives Considered

### Protocol extension (bidirectional messaging)

Adding `HostCommand::Fetch` / `WorkerEvent::FetchResult` would require restructuring the protocol from request-response-per-command to a bidirectional stream where the worker can send requests to the host mid-execution. This is a much larger architectural change for a single feature.

### External command handler

Routing `curl`/`wget` through the existing `ExternalCommandHandler` would bypass the utility registry pattern and lose access to `UtilContext` (filesystem for `-o` output, shell state for variable expansion). The handler also has a different ownership model that doesn't fit.

## Consequences

- `curl` and `wget` are available as utilities (88 total, up from 86)
- No network access without explicit opt-in via `NetworkPolicy`
- Existing sandboxes are unaffected (empty allowlist is the default)
- The `HostCommand::Init` protocol carries `network_policy`; `allowed_hosts`
  remains a compatibility field
- Platform backends share policy semantics, but browsers require a trusted
  redirect-aware broker for enabled synchronous network access
