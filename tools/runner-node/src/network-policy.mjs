import {
  isNetworkAllowed,
  normalizeNetworkPolicy,
  normalizeNetworkTarget,
} from "../../../packages/npm/wasmsh-pyodide/lib/allowlist.mjs";

/**
 * Thrown by the fetch broker when an allowlist check rejects a URL.
 * Callers `instanceof`-check this class rather than matching on
 * `error.message`, so future message tweaks cannot accidentally
 * promote a denied-host error into a generic transport failure.
 */
export class HostDeniedError extends Error {
  constructor(url, reason = "network policy denied target") {
    super(`host denied: ${reason}: ${safeTargetForError(url)}`);
    this.name = "HostDeniedError";
    this.url = url;
  }
}

function safeTargetForError(url) {
  try {
    const target = normalizeNetworkTarget(url);
    const host = target.host.includes(":") ? `[${target.host}]` : target.host;
    return `${new URL(url).protocol}//${host}:${target.port}`;
  } catch {
    return "<invalid URL>";
  }
}

export class InvalidUrlError extends Error {
  constructor(url, cause) {
    super(`invalid URL: ${cause?.message ?? cause ?? url}`);
    this.name = "InvalidUrlError";
    this.url = url;
  }
}

export class TooManyRedirectsError extends Error {
  constructor(limit) {
    super(`too many redirects: exceeded ${limit} redirects`);
    this.name = "TooManyRedirectsError";
    this.limit = limit;
  }
}

export function assertNetworkAllowed(url, networkConfig) {
  const policy = normalizeNetworkPolicy(networkConfig);
  try {
    normalizeNetworkTarget(url);
  } catch (error) {
    throw new InvalidUrlError(url, error);
  }
  if (!isNetworkAllowed(url, policy)) {
    const reason = policy.enabled ? "network policy denied target" : "network policy is disabled";
    throw new HostDeniedError(url, reason);
  }
  return policy;
}

/** Backwards-compatible name for existing broker callers. */
export function assertAllowedHost(url, allowedHosts) {
  return assertNetworkAllowed(url, allowedHosts);
}
