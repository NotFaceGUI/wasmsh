//! Network capability types for wasmsh utilities.
//!
//! Provides a `NetworkBackend` trait that embedding layers implement to give
//! `curl` and `wget` utilities controlled HTTP access.  URL validation against
//! an allowlist happens in Rust before any network call leaves WASM.

use std::fmt;
use std::net::IpAddr;

use url::{Host, Url};

pub use wasmsh_protocol::NetworkDefaultAction;
pub use wasmsh_protocol::{NetworkDefaultAction as DefaultAction, NetworkPolicyConfig};

/// A validated, immutable network policy shared by all HTTP utilities.
#[derive(Debug, Clone)]
pub struct NetworkPolicy {
    enabled: bool,
    default_action: DefaultAction,
    allow: Vec<NetworkRule>,
    deny: Vec<NetworkRule>,
}

/// Invalid network policy configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPolicyError(String);

impl fmt::Display for NetworkPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl NetworkPolicy {
    /// Build and validate a policy from structured configuration.
    pub fn new(config: NetworkPolicyConfig) -> Result<Self, NetworkPolicyError> {
        Self::try_from_config(config)
    }

    /// Build and validate a policy from structured configuration.
    pub fn try_from_config(config: NetworkPolicyConfig) -> Result<Self, NetworkPolicyError> {
        let allow = parse_rules("allow", config.allow)?;
        let deny = parse_rules("deny", config.deny)?;
        Ok(Self {
            enabled: config.enabled,
            default_action: config.default_action,
            allow,
            deny,
        })
    }

    /// Build the legacy `allowed_hosts` policy: enabled allowlist mode when
    /// non-empty, and disabled networking when empty.
    pub fn try_from_allowed_hosts(allowed_hosts: Vec<String>) -> Result<Self, NetworkPolicyError> {
        let enabled = !allowed_hosts.is_empty();
        Self::try_from_config(NetworkPolicyConfig {
            enabled,
            default_action: DefaultAction::Deny,
            allow: allowed_hosts,
            deny: Vec::new(),
        })
    }

    /// Return the policy's enabled state.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Check a URL without performing network I/O.
    pub fn check(&self, url: &str) -> Result<(), NetworkError> {
        if !self.enabled {
            return Err(NetworkError::HostDenied(
                "network policy is disabled".into(),
            ));
        }
        let target = NormalizedTarget::parse(url)?;

        // Deny is intentionally evaluated first and cannot be overridden by
        // an allow rule, including when the two rules are identical.
        if self.deny.iter().any(|rule| rule.matches(&target)) {
            return Err(NetworkError::HostDenied(format!(
                "normalized target '{}' matches a deny rule",
                target.display()
            )));
        }
        if self.allow.iter().any(|rule| rule.matches(&target)) {
            return Ok(());
        }
        if self.default_action == DefaultAction::Allow {
            return Ok(());
        }
        Err(NetworkError::HostDenied(format!(
            "normalized target '{}' is not allowed",
            target.display()
        )))
    }

    /// Alias used by network backends and redirect brokers.
    pub fn check_url(&self, url: &str) -> Result<(), NetworkError> {
        self.check(url)
    }
}

/// Validate an HTTP(S) URL using the same normalization rules as policies.
/// This is a protocol check and therefore also runs for backends that do not
/// install a policy of their own.
pub fn validate_http_url(url: &str) -> Result<(), NetworkError> {
    NormalizedTarget::parse(url).map(|_| ())
}

fn parse_rules(kind: &str, values: Vec<String>) -> Result<Vec<NetworkRule>, NetworkPolicyError> {
    values
        .into_iter()
        .map(|value| {
            NetworkRule::parse(&value)
                .map_err(|e| NetworkPolicyError(format!("{kind} rule '{value}': {e}")))
        })
        .collect()
}

#[derive(Debug, Clone)]
enum NetworkRule {
    Any {
        port: Option<u16>,
    },
    Host {
        host: String,
        wildcard: bool,
        port: Option<u16>,
    },
}

impl NetworkRule {
    fn parse(raw: &str) -> Result<Self, String> {
        if raw.is_empty() || raw.trim() != raw || raw.chars().any(char::is_control) {
            return Err("rule must not be empty, padded, or contain control characters".into());
        }
        let bracketed = raw.starts_with('[');
        let (host_part, port) = split_rule_port(raw)?;
        if bracketed && !matches!(host_part.parse::<IpAddr>(), Ok(IpAddr::V6(_))) {
            return Err("bracketed rules must contain an IPv6 address".into());
        }
        if host_part == "*" {
            return Ok(Self::Any { port });
        }
        let wildcard = host_part.starts_with("*.");
        let host = if wildcard {
            let suffix = &host_part[2..];
            if suffix.is_empty() || suffix.contains('*') || suffix.contains('?') {
                return Err("wildcard must be exactly '*.' followed by a host".into());
            }
            normalize_rule_host(suffix)?
        } else {
            if host_part.contains('*') || host_part.contains('?') {
                return Err("only '*' and '*.example.com' wildcards are supported".into());
            }
            normalize_rule_host(host_part)?
        };
        if wildcard && host.parse::<IpAddr>().is_ok() {
            return Err("wildcard rules cannot target an IP address".into());
        }
        Ok(Self::Host {
            host,
            wildcard,
            port,
        })
    }

    fn matches(&self, target: &NormalizedTarget) -> bool {
        match self {
            Self::Any { port } => port_matches(*port, target.port),
            Self::Host {
                host,
                wildcard,
                port,
            } => {
                let host_matches = if *wildcard {
                    target.host.ends_with(&format!(".{host}")) && target.host.len() > host.len() + 1
                } else {
                    target.host == *host
                };
                host_matches && port_matches(*port, target.port)
            }
        }
    }
}

fn split_rule_port(raw: &str) -> Result<(&str, Option<u16>), String> {
    if let Some(rest) = raw.strip_prefix('[') {
        let close = rest
            .find(']')
            .ok_or_else(|| "bracketed IPv6 rule is missing ']'".to_string())?;
        let host_end = close + 1;
        let host = &rest[..close];
        let suffix = &rest[host_end..];
        let port = if suffix.is_empty() {
            None
        } else if let Some(port) = suffix.strip_prefix(':') {
            parse_port(port)?
        } else {
            return Err("characters after bracketed IPv6 host are not allowed".into());
        };
        return Ok((host, port));
    }
    if raw.contains('[') || raw.contains(']') {
        return Err("IPv6 rules must use brackets".into());
    }
    if raw.matches(':').count() > 1 {
        return Err("IPv6 rules must use brackets".into());
    }
    if let Some((host, port)) = raw.rsplit_once(':') {
        if host.is_empty() {
            return Err("rule host is empty".into());
        }
        return Ok((host, parse_port(port)?));
    }
    Ok((raw, None))
}

fn parse_port(raw: &str) -> Result<Option<u16>, String> {
    if raw.is_empty() {
        return Err("port is empty".into());
    }
    raw.parse::<u16>()
        .map(Some)
        .map_err(|_| "port must be a decimal value from 0 to 65535".into())
}

fn normalize_rule_host(raw: &str) -> Result<String, String> {
    let raw = raw.strip_suffix('.').unwrap_or(raw);
    let raw = raw
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(raw);
    if raw.is_empty()
        || raw.ends_with('.')
        || raw.contains('/')
        || raw.contains('@')
        || raw.contains('?')
        || raw.contains('#')
        || raw.contains('%')
        || raw.contains('\\')
    {
        return Err("invalid host syntax".into());
    }
    if let Ok(ip) = raw.parse::<IpAddr>() {
        return Ok(ip.to_string());
    }
    let parsed = Url::parse(&format!("http://{raw}/")).map_err(|e| format!("invalid host: {e}"))?;
    let host = match parsed.host() {
        Some(Host::Domain(host)) => host.to_ascii_lowercase(),
        Some(Host::Ipv4(ip)) => ip.to_string(),
        Some(Host::Ipv6(ip)) => ip.to_string(),
        None => return Err("host is missing".into()),
    };
    validate_domain(&host)?;
    Ok(host)
}

fn validate_domain(host: &str) -> Result<(), String> {
    if host.len() > 253 {
        return Err("domain exceeds 253 bytes".into());
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 || label.starts_with('-') || label.ends_with('-') {
            return Err("domain contains an invalid label".into());
        }
        if !label
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err("domain contains an invalid label".into());
        }
    }
    Ok(())
}

#[derive(Debug)]
struct NormalizedTarget {
    scheme: String,
    host: String,
    port: Option<u16>,
}

impl NormalizedTarget {
    fn parse(raw: &str) -> Result<Self, NetworkError> {
        let parsed = Url::parse(raw).map_err(|e| NetworkError::InvalidUrl(e.to_string()))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(NetworkError::InvalidUrl(format!(
                "scheme '{}' is not supported",
                parsed.scheme()
            )));
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(NetworkError::InvalidUrl(
                "URL userinfo is not allowed".into(),
            ));
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| NetworkError::InvalidUrl("URL has no host".into()))?;
        let host = normalize_rule_host(host)
            .map_err(|e| NetworkError::InvalidUrl(format!("invalid URL host: {e}")))?;
        Ok(Self {
            scheme: parsed.scheme().to_string(),
            host,
            port: parsed.port_or_known_default(),
        })
    }

    fn display(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{0}]", self.host)
        } else {
            self.host.clone()
        };
        match self.port {
            Some(port) => format!("{}://{}:{}", self.scheme, host, port),
            None => format!("{}://{}", self.scheme, host),
        }
    }
}

fn port_matches(allowed: Option<u16>, actual: Option<u16>) -> bool {
    allowed.is_none() || allowed == actual
}

/// Backwards-compatible allowlist facade. New code should use [`NetworkPolicy`].
#[derive(Debug, Clone)]
pub struct HostAllowlist {
    policy: Option<NetworkPolicy>,
    invalid_rule: Option<String>,
}

impl HostAllowlist {
    /// Fallible constructor for new callers that must reject bad config at init.
    pub fn try_new(patterns: Vec<String>) -> Result<Self, NetworkPolicyError> {
        let policy = NetworkPolicy::try_from_allowed_hosts(patterns)?;
        Ok(Self {
            policy: Some(policy),
            invalid_rule: None,
        })
    }

    /// Legacy infallible constructor. Invalid patterns are retained as a
    /// fail-closed error and are reported by [`Self::check`].
    #[must_use]
    pub fn new(patterns: Vec<String>) -> Self {
        match Self::try_new(patterns) {
            Ok(value) => value,
            Err(error) => Self {
                policy: None,
                invalid_rule: Some(error.to_string()),
            },
        }
    }

    /// Check a URL against the legacy allowlist semantics.
    pub fn check(&self, url: &str) -> Result<(), NetworkError> {
        if let Some(error) = &self.invalid_rule {
            return Err(NetworkError::InvalidUrl(format!(
                "invalid allowlist: {error}"
            )));
        }
        self.policy
            .as_ref()
            .expect("valid legacy allowlist has a policy")
            .check(url)
    }
}

/// An HTTP request to be executed by the host.
///
/// The additional sandbox limit fields (`timeout_ms`, `connect_timeout_ms`,
/// `max_redirs`, `max_response_bytes`) are advisory: backends that can enforce
/// them should; the `wasmsh-utils` layer also enforces `max_response_bytes`
/// after the fetch returns so the feature works with backends that ignore it.
#[derive(Debug, Clone, Default)]
pub struct HttpRequest {
    /// Fully-qualified URL (e.g. `https://api.example.com/data`).
    pub url: String,
    /// HTTP method (GET, POST, HEAD, PUT, DELETE, PATCH).
    pub method: String,
    /// Request headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Optional request body.
    pub body: Option<Vec<u8>>,
    /// Whether to follow HTTP 3xx redirects.
    pub follow_redirects: bool,
    /// Advisory overall timeout in milliseconds (None = backend default).
    pub timeout_ms: Option<u64>,
    /// Advisory connection timeout in milliseconds (None = backend default).
    pub connect_timeout_ms: Option<u64>,
    /// Cap on redirect chain length (None = backend default).
    pub max_redirs: Option<u32>,
    /// Cap on response body bytes (None = no cap).
    pub max_response_bytes: Option<u64>,
}

/// An HTTP response returned by the host.
#[derive(Debug, Clone, Default)]
pub struct HttpResponse {
    /// HTTP status code (e.g. 200, 404).
    pub status: u16,
    /// Response headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

/// Errors from network operations.
#[derive(Debug, Clone)]
pub enum NetworkError {
    /// The target host is not in the allowlist.
    HostDenied(String),
    /// The connection could not be established.
    ConnectionFailed(String),
    /// The request timed out.
    Timeout(String),
    /// The URL could not be parsed.
    InvalidUrl(String),
    /// The response exceeded the configured size cap.
    ResponseTooLarge(String),
    /// Redirect chain exceeded the configured limit.
    TooManyRedirects(String),
    /// Any other network error.
    Other(String),
}

impl fmt::Display for NetworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostDenied(msg) => write!(f, "host denied: {msg}"),
            Self::ConnectionFailed(msg) => write!(f, "connection failed: {msg}"),
            Self::Timeout(msg) => write!(f, "timeout: {msg}"),
            Self::InvalidUrl(msg) => write!(f, "invalid URL: {msg}"),
            Self::ResponseTooLarge(msg) => write!(f, "response too large: {msg}"),
            Self::TooManyRedirects(msg) => write!(f, "too many redirects: {msg}"),
            Self::Other(msg) => write!(f, "network error: {msg}"),
        }
    }
}

/// Trait for performing HTTP requests from utility commands.
///
/// Implementations are provided by the embedding layer:
/// - Standalone browser: wasm-bindgen → synchronous `XMLHttpRequest` in Web Worker
/// - Pyodide: Emscripten FFI → synchronous `XMLHttpRequest` in Web Worker
/// - Tests: mock backend with canned responses
///
/// The `fetch` method is synchronous because all utilities run synchronously.
pub trait NetworkBackend {
    /// Execute an HTTP request and return the response.
    ///
    /// Implementations must validate the URL against the host allowlist
    /// before performing any network I/O.
    fn fetch(&self, request: &HttpRequest) -> Result<HttpResponse, NetworkError>;

    /// Validate a URL against the backend's policy without performing I/O.
    ///
    /// Used by the redirect-following loop so each hop of a 3xx chain is
    /// re-validated against the same allowlist as the initial request. The
    /// default fails closed: a backend that has no policy of its own must
    /// override this rather than silently allowing every request.
    fn check_url(&self, url: &str) -> Result<(), NetworkError> {
        let _ = url;
        Err(NetworkError::HostDenied(
            "network backend has no policy installed".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_allowlist_denies_all() {
        let al = HostAllowlist::new(vec![]);
        assert!(al.check("https://example.com").is_err());
    }

    #[test]
    fn exact_host_match() {
        let al = HostAllowlist::new(vec!["api.example.com".into()]);
        assert!(al.check("https://api.example.com/path").is_ok());
        assert!(al.check("http://api.example.com:443/path").is_ok());
        assert!(al.check("https://other.example.com").is_err());
        assert!(al.check("https://example.com").is_err());
    }

    #[test]
    fn exact_host_case_insensitive() {
        let al = HostAllowlist::new(vec!["API.Example.COM".into()]);
        assert!(al.check("https://api.example.com/path").is_ok());
    }

    #[test]
    fn wildcard_subdomain() {
        let al = HostAllowlist::new(vec!["*.example.com".into()]);
        assert!(al.check("https://api.example.com/path").is_ok());
        assert!(al.check("https://deep.sub.example.com").is_ok());
        // Apex domain is NOT covered by `*.example.com`; callers must list it
        // explicitly. This matches the documented contract in
        // docs/reference/sandbox-and-capabilities.md.
        assert!(al.check("https://example.com").is_err());
        assert!(al.check("https://notexample.com").is_err());
    }

    #[test]
    fn wildcard_does_not_match_lookalike_suffix() {
        let al = HostAllowlist::new(vec!["*.example.com".into()]);
        // foo.evil-example.com must not match *.example.com because the
        // ".example.com" boundary requires a literal subdomain separator.
        assert!(al.check("https://evilexample.com").is_err());
        assert!(al.check("https://foo.notexample.com").is_err());
    }

    #[test]
    fn apex_listed_explicitly_alongside_wildcard() {
        // The supported way to allow both apex and subdomains.
        let al = HostAllowlist::new(vec!["example.com".into(), "*.example.com".into()]);
        assert!(al.check("https://example.com").is_ok());
        assert!(al.check("https://api.example.com").is_ok());
    }

    struct PolicyLessBackend;

    impl NetworkBackend for PolicyLessBackend {
        fn fetch(&self, _request: &HttpRequest) -> Result<HttpResponse, NetworkError> {
            Ok(HttpResponse::default())
        }
    }

    #[test]
    fn backend_without_policy_fails_closed_on_check_url() {
        // Regression: the default `check_url` must not validate-only, or a
        // production backend that forgets to override it silently allows
        // every redirect target.
        let backend = PolicyLessBackend;
        assert!(matches!(
            backend.check_url("https://example.com/"),
            Err(NetworkError::HostDenied(_))
        ));
    }

    #[test]
    fn rejects_non_http_schemes() {
        let al = HostAllowlist::new(vec!["example.com".into()]);
        for url in [
            "file:///etc/passwd",
            "data:text/plain,abc",
            "javascript:alert(1)",
            "ws://example.com",
            "wss://example.com",
            "ftp://example.com",
            "blob:example.com",
        ] {
            let r = al.check(url);
            assert!(
                matches!(
                    r,
                    Err(NetworkError::HostDenied(_) | NetworkError::InvalidUrl(_))
                ),
                "expected denial for {url}, got {r:?}"
            );
        }
    }

    #[test]
    fn trailing_dot_normalised() {
        let al = HostAllowlist::new(vec!["example.com".into()]);
        // FQDN with trailing dot must be treated identically to the bare name.
        assert!(al.check("https://example.com./path").is_ok());
    }

    #[test]
    fn trailing_dot_normalised_under_wildcard() {
        let al = HostAllowlist::new(vec!["*.example.com".into()]);
        assert!(al.check("https://api.example.com./path").is_ok());
    }

    #[test]
    fn ip_address() {
        let al = HostAllowlist::new(vec!["192.168.1.100".into()]);
        assert!(al.check("http://192.168.1.100/data").is_ok());
        assert!(al.check("http://192.168.1.101/data").is_err());
    }

    #[test]
    fn host_with_port() {
        let al = HostAllowlist::new(vec!["localhost:8080".into()]);
        assert!(al.check("http://localhost:8080/api").is_ok());
        assert!(al.check("http://localhost:9090/api").is_err());
        assert!(al.check("http://localhost/api").is_err());
    }

    #[test]
    fn wildcard_with_port() {
        let al = HostAllowlist::new(vec!["*.internal.co:9090".into()]);
        assert!(al.check("http://api.internal.co:9090/x").is_ok());
        assert!(al.check("http://api.internal.co:8080/x").is_err());
    }

    #[test]
    fn invalid_url() {
        let al = HostAllowlist::new(vec!["example.com".into()]);
        assert!(matches!(
            al.check("not a url"),
            Err(NetworkError::InvalidUrl(_))
        ));
    }

    #[test]
    fn multiple_patterns() {
        let al = HostAllowlist::new(vec![
            "api.example.com".into(),
            "*.internal.co".into(),
            "10.0.0.1".into(),
        ]);
        assert!(al.check("https://api.example.com/a").is_ok());
        assert!(al.check("https://svc.internal.co/b").is_ok());
        assert!(al.check("http://10.0.0.1/c").is_ok());
        assert!(al.check("https://evil.com").is_err());
    }

    #[test]
    fn policy_supports_disabled_allow_deny_and_explicit_allow() {
        let disabled = NetworkPolicy::new(NetworkPolicyConfig {
            enabled: false,
            default_action: DefaultAction::Allow,
            allow: vec!["*".into()],
            deny: Vec::new(),
        })
        .unwrap();
        assert!(matches!(
            disabled.check("https://example.com"),
            Err(NetworkError::HostDenied(_))
        ));

        let blacklist = NetworkPolicy::new(NetworkPolicyConfig {
            enabled: true,
            default_action: DefaultAction::Allow,
            allow: Vec::new(),
            deny: vec!["blocked.example".into()],
        })
        .unwrap();
        assert!(blacklist.check("https://ok.example").is_ok());
        assert!(blacklist.check("https://blocked.example").is_err());

        let combined = NetworkPolicy::new(NetworkPolicyConfig {
            enabled: true,
            default_action: DefaultAction::Deny,
            allow: vec!["*".into()],
            deny: vec!["blocked.example".into()],
        })
        .unwrap();
        assert!(combined.check("https://ok.example").is_ok());
        assert!(combined.check("https://blocked.example").is_err());
    }

    #[test]
    fn wildcard_is_strict_and_normalization_is_shared() {
        let policy = NetworkPolicy::new(NetworkPolicyConfig {
            enabled: true,
            default_action: DefaultAction::Deny,
            allow: vec!["*.EXAMPLE.com.".into()],
            deny: Vec::new(),
        })
        .unwrap();
        assert!(policy.check("https://A.B.Example.COM./x").is_ok());
        assert!(policy.check("https://example.com").is_err());
        assert!(policy.check("https://badexample.com").is_err());
        assert!(policy.check("https://example.com.evil.test").is_err());
    }

    #[test]
    fn policy_normalizes_idna_default_ports_and_ipv6() {
        let policy = NetworkPolicy::new(NetworkPolicyConfig {
            enabled: true,
            default_action: DefaultAction::Deny,
            allow: vec!["bücher.example:443".into(), "[2001:db8::1]:8080".into()],
            deny: Vec::new(),
        })
        .unwrap();
        assert!(policy.check("https://xn--bcher-kva.example").is_ok());
        assert!(policy.check("https://xn--bcher-kva.example:443").is_ok());
        assert!(policy.check("http://xn--bcher-kva.example").is_err());
        let ipv6_result = policy.check("http://[2001:0DB8:0:0:0:0:0:1]:8080");
        assert!(ipv6_result.is_ok(), "IPv6 result: {ipv6_result:?}");
        assert!(policy.check("http://[2001:db8::1]:8081").is_err());
    }

    #[test]
    fn unsupported_rules_and_userinfo_are_rejected() {
        for rule in [
            "api.*.example",
            "foo*",
            "?",
            "example.com:",
            "[::1",
            "[example.com]",
            "[127.0.0.1]",
        ] {
            assert!(NetworkPolicy::new(NetworkPolicyConfig {
                enabled: true,
                allow: vec![rule.into()],
                ..NetworkPolicyConfig::default()
            })
            .is_err());
        }
        let policy = NetworkPolicy::new(NetworkPolicyConfig {
            enabled: true,
            default_action: DefaultAction::Allow,
            ..NetworkPolicyConfig::default()
        })
        .unwrap();
        assert!(matches!(
            policy.check("https://user:secret@example.com"),
            Err(NetworkError::InvalidUrl(_))
        ));
    }
}
