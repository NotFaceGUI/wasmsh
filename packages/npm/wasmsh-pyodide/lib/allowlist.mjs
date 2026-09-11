/**
 * Shared JavaScript implementation of the wasmsh network policy.
 *
 * This mirrors `wasmsh_utils::net_types::NetworkPolicy` for host paths that
 * cannot call into Rust directly, including Python's `js.fetch` membrane.
 * Rules are parsed once at session initialization; malformed rules throw so
 * a host cannot accidentally start with a weaker policy.
 */

const DEFAULT_PORTS = { "http:": 80, "https:": 443 };
const MAX_SAFE_PORT = 65535;

function policyError(message) {
  return new TypeError(`wasmsh: invalid network policy: ${message}`);
}

function parsePort(raw) {
  if (!/^\d+$/.test(raw)) {
    throw policyError("port must be a decimal value from 0 to 65535");
  }
  const port = Number(raw);
  if (!Number.isSafeInteger(port) || port > MAX_SAFE_PORT) {
    throw policyError("port must be a decimal value from 0 to 65535");
  }
  return port;
}

function splitRulePort(raw) {
  if (raw.startsWith("[")) {
    const close = raw.indexOf("]");
    if (close < 0) throw policyError("bracketed IPv6 rule is missing ']'");
    const host = raw.slice(1, close);
    const suffix = raw.slice(close + 1);
    if (!suffix) return { host, port: null, bracketed: true };
    if (!suffix.startsWith(":")) {
      throw policyError("characters after bracketed IPv6 host are not allowed");
    }
    return { host, port: parsePort(suffix.slice(1)), bracketed: true };
  }
  if (raw.includes("[") || raw.includes("]")) {
    throw policyError("IPv6 rules must use brackets");
  }
  const colonCount = [...raw].filter((char) => char === ":").length;
  if (colonCount > 1) throw policyError("IPv6 rules must use brackets");
  if (colonCount === 1) {
    const split = raw.lastIndexOf(":");
    const host = raw.slice(0, split);
    if (!host) throw policyError("rule host is empty");
    return { host, port: parsePort(raw.slice(split + 1)), bracketed: false };
  }
  return { host: raw, port: null, bracketed: false };
}

function normalizeHost(raw) {
  if (typeof raw !== "string" || !raw || raw.trim() !== raw) {
    throw policyError("host must be a non-empty, unpadded string");
  }
  if ([...raw].some((char) => char.charCodeAt(0) < 0x20 || char === "\u007f")) {
    throw policyError("host must not contain control characters");
  }
  let host = raw.endsWith(".") ? raw.slice(0, -1) : raw;
  if (host.startsWith("[")) {
    if (!host.endsWith("]")) throw policyError("invalid bracketed host");
    host = host.slice(1, -1);
  }
  if (!host || host.endsWith(".") || /[\/@?#%\\]/.test(host)) {
    throw policyError("invalid host syntax");
  }

  if (host.includes(":")) {
    try {
      const parsed = new URL(`http://[${host}]/`);
      const normalized = parsed.hostname.slice(1, -1).toLowerCase();
      if (!normalized.includes(":")) throw new Error("not IPv6");
      return normalized;
    } catch {
      throw policyError("invalid IPv6 host");
    }
  }

  let parsed;
  try {
    parsed = new URL(`http://${host}/`);
  } catch {
    throw policyError("invalid host");
  }
  const normalized = parsed.hostname.toLowerCase().replace(/\.$/, "");
  if (!normalized || normalized.includes(":")) throw policyError("invalid host");
  if (new TextEncoder().encode(normalized).byteLength > 253) {
    throw policyError("domain exceeds 253 bytes");
  }
  for (const label of normalized.split(".")) {
    if (
      !label ||
      label.length > 63 ||
      label.startsWith("-") ||
      label.endsWith("-") ||
      !/^[a-z0-9-]+$/.test(label)
    ) {
      throw policyError("domain contains an invalid label");
    }
  }
  return normalized;
}

function parseRule(raw) {
  if (typeof raw !== "string" || !raw || raw.trim() !== raw) {
    throw policyError("rule must not be empty, padded, or non-string");
  }
  const { host: hostPart, port, bracketed } = splitRulePort(raw);
  if (bracketed && !hostPart.includes(":")) {
    throw policyError("bracketed rules must contain an IPv6 address");
  }
  if (hostPart === "*") return { kind: "any", port };

  const wildcard = hostPart.startsWith("*.");
  if (wildcard) {
    const suffix = hostPart.slice(2);
    if (!suffix || suffix.includes("*") || suffix.includes("?")) {
      throw policyError("wildcard must be exactly '*.example.com' style");
    }
  } else if (hostPart.includes("*") || hostPart.includes("?")) {
    throw policyError("only '*' and '*.example.com' wildcards are supported");
  }

  const host = normalizeHost(wildcard ? hostPart.slice(2) : hostPart);
  if (wildcard && host.includes(":")) {
    throw policyError("wildcard rules cannot target an IP address");
  }
  return { kind: "host", host, wildcard, port };
}

function normalizeRuleList(value, name) {
  if (value === undefined) return [];
  if (!Array.isArray(value)) throw policyError(`${name} must be an array of strings`);
  try {
    return value.map(parseRule);
  } catch (error) {
    if (error instanceof TypeError && error.message.startsWith("wasmsh:")) {
      throw policyError(`${name}: ${error.message.replace(/^wasmsh: invalid network policy: /, "")}`);
    }
    throw error;
  }
}

/** Normalize a legacy array or structured policy into an immutable matcher. */
export function normalizeNetworkPolicy(config) {
  if (
    config &&
    typeof config === "object" &&
    !Array.isArray(config) &&
    (config.defaultAction === "deny" || config.defaultAction === "allow") &&
    typeof config.enabled === "boolean" &&
    Array.isArray(config.allow) &&
    Array.isArray(config.deny) &&
    [...config.allow, ...config.deny].every(
      (rule) => rule && typeof rule === "object" &&
        (rule.kind === "any" || rule.kind === "host") &&
        (rule.port === null || Number.isInteger(rule.port)),
    )
  ) {
    return config;
  }
  if (Array.isArray(config)) {
    return Object.freeze({
      enabled: config.length > 0,
      defaultAction: "deny",
      allow: normalizeRuleList(config, "allow"),
      deny: [],
    });
  }
  if (!config || typeof config !== "object") {
    throw policyError("configuration must be an array or object");
  }

  if (Object.hasOwn(config, "allowed_hosts")) {
    const policyKeys = [
      "network_policy",
      "enabled",
      "default_action",
      "allow",
      "deny",
    ];
    if (policyKeys.some((key) => Object.hasOwn(config, key))) {
      throw policyError("network_policy and allowed_hosts cannot both be configured");
    }
    return normalizeNetworkPolicy(config.allowed_hosts);
  }
  if (Object.hasOwn(config, "network_policy")) {
    if (Object.keys(config).some((key) => key !== "network_policy")) {
      throw policyError("network_policy cannot be combined with sibling settings");
    }
    return normalizeNetworkPolicy(config.network_policy);
  }

  if (config.enabled !== undefined && typeof config.enabled !== "boolean") {
    throw policyError("enabled must be boolean");
  }
  const defaultAction = config.default_action ?? "deny";
  if (defaultAction !== "deny" && defaultAction !== "allow") {
    throw policyError("default_action must be 'deny' or 'allow'");
  }
  const policy = {
    enabled: config.enabled ?? false,
    defaultAction,
    allow: normalizeRuleList(config.allow, "allow"),
    deny: normalizeRuleList(config.deny, "deny"),
  };
  for (const key of Object.keys(config)) {
    if (!["enabled", "default_action", "allow", "deny"].includes(key)) {
      throw policyError(`unknown network policy field '${key}'`);
    }
  }
  return Object.freeze(policy);
}

export function normalizeNetworkTarget(url) {
  if (typeof url !== "string") {
    throw new TypeError("URL must be a string");
  }
  let parsed;
  try {
    parsed = new URL(url);
  } catch {
    throw new TypeError("invalid URL");
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new TypeError(`unsupported scheme '${parsed.protocol}'`);
  }
  if (parsed.username || parsed.password) {
    throw new TypeError("URL userinfo is not allowed");
  }
  const host = normalizeHost(parsed.hostname);
  const port = parsed.port ? Number(parsed.port) : DEFAULT_PORTS[parsed.protocol];
  return { host, port };
}

function portMatches(rulePort, targetPort) {
  return rulePort === null || rulePort === targetPort;
}

function ruleMatches(rule, target) {
  if (!portMatches(rule.port, target.port)) return false;
  if (rule.kind === "any") return true;
  if (rule.wildcard) {
    return target.host.endsWith(`.${rule.host}`) &&
      target.host.length > rule.host.length + 1;
  }
  return target.host === rule.host;
}

/** Check a URL against a normalized policy without performing network I/O. */
export function isNetworkAllowed(url, config) {
  const policy = normalizeNetworkPolicy(config);
  if (!policy.enabled) return false;
  let target;
  try {
    target = normalizeNetworkTarget(url);
  } catch {
    return false;
  }
  if (policy.deny.some((rule) => ruleMatches(rule, target))) return false;
  if (policy.allow.some((rule) => ruleMatches(rule, target))) return true;
  return policy.defaultAction === "allow";
}

/** Backwards-compatible allowlist helper for existing JS embedders. */
export function isHostAllowed(url, allowedHosts) {
  return isNetworkAllowed(url, normalizeNetworkPolicy(allowedHosts));
}
