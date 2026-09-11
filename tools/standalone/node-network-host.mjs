import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import http from "node:http";
import https from "node:https";

const MAX_TIMEOUT_MS = 300_000;
const MAX_REQUEST_BYTES = 64 * 1024 * 1024;
const MAX_RESPONSE_BYTES = 64 * 1024 * 1024;
const CHILD_SCRIPT = fileURLToPath(import.meta.url);

function jsonResult(result) {
  process.stdout.write(`${JSON.stringify(result)}\n`);
}

function asChildOptions(optionsJson) {
  let options = {};
  try {
    options = JSON.parse(optionsJson || "{}");
  } catch (error) {
    throw new Error(`network broker options are not valid JSON: ${error.message}`);
  }
  if (!options || Array.isArray(options) || typeof options !== "object") {
    throw new Error("network broker options must be a JSON object");
  }
  const timeout = options.timeout_ms ?? 30_000;
  const connectTimeout = options.connect_timeout_ms ?? timeout;
  const maxResponse = options.max_response_bytes ?? MAX_RESPONSE_BYTES;
  for (const [label, value, limit] of [
    ["timeout_ms", timeout, MAX_TIMEOUT_MS],
    ["connect_timeout_ms", connectTimeout, MAX_TIMEOUT_MS],
    ["max_response_bytes", maxResponse, MAX_RESPONSE_BYTES],
  ]) {
    if (!Number.isSafeInteger(value) || value < 1 || value > limit) {
      throw new Error(`${label} is outside the network broker limit`);
    }
  }
  return { timeout, connectTimeout, maxResponse };
}

function requestHeaders(headerPairs, bodyLength) {
  if (!Array.isArray(headerPairs)) {
    throw new Error("network broker headers must be an array");
  }
  const headers = {};
  for (const pair of headerPairs) {
    if (!Array.isArray(pair) || pair.length !== 2) {
      throw new Error("network broker headers must contain [name, value] pairs");
    }
    const [name, value] = pair;
    if (typeof name !== "string" || typeof value !== "string") {
      throw new Error("network broker header names and values must be strings");
    }
    headers[name] = value;
  }
  // Do not allow a shell-provided length to disagree with the binary payload.
  for (const name of Object.keys(headers)) {
    if (name.toLowerCase() === "content-length") delete headers[name];
  }
  if (bodyLength > 0) headers["Content-Length"] = String(bodyLength);
  return headers;
}

async function performRequest(payload) {
  const parsed = new URL(payload.url);
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new Error("network broker only supports HTTP(S)");
  }
  if (parsed.username || parsed.password) {
    throw new Error("network broker refuses URL userinfo");
  }
  if (payload.follow_redirects) {
    throw new Error("network broker never follows redirects; Rust must inspect each hop");
  }
  const body = Buffer.from(payload.body_base64 || "", "base64");
  if (body.length > MAX_REQUEST_BYTES) {
    throw new Error(`request body exceeds ${MAX_REQUEST_BYTES} bytes`);
  }
  const options = asChildOptions(payload.options_json);
  const transport = parsed.protocol === "https:" ? https : http;
  const request = transport.request({
    protocol: parsed.protocol,
    hostname: parsed.hostname,
    port: parsed.port || undefined,
    path: `${parsed.pathname || "/"}${parsed.search}`,
    method: payload.method,
    headers: requestHeaders(payload.headers, body.length),
    // The transport owns no redirect policy and never follows Location.
    maxHeaderSize: 64 * 1024,
  });

  return await new Promise((resolveResult) => {
    let settled = false;
    let timedOut = false;
    const finish = (result) => {
      if (settled) return;
      settled = true;
      resolveResult(result);
    };
    const fail = (message, reason = "connection") => finish({
      status: 0,
      headers: [],
      body_base64: "",
      error: message,
      error_reason: reason,
    });

    request.setTimeout(Math.min(options.timeout, options.connectTimeout), () => {
      timedOut = true;
      request.destroy(new Error("network request timed out"));
    });
    request.once("error", (error) => {
      fail(timedOut ? "network request timed out" : error.message, timedOut ? "timeout" : "connection");
    });
    request.once("response", (response) => {
      const chunks = [];
      let received = 0;
      const headers = [];
      for (let index = 0; index + 1 < response.rawHeaders.length; index += 2) {
        headers.push([response.rawHeaders[index], response.rawHeaders[index + 1]]);
      }
      response.on("data", (chunk) => {
        received += chunk.length;
        if (received > options.maxResponse) {
          fail(`response exceeds ${options.maxResponse} bytes`, "response_too_large");
          response.destroy();
          return;
        }
        chunks.push(Buffer.from(chunk));
      });
      response.once("aborted", () => {
        if (!settled) fail("network response was aborted", "connection");
      });
      response.once("error", (error) => fail(error.message, "connection"));
      response.once("end", () => finish({
        status: response.statusCode || 0,
        headers,
        body_base64: Buffer.concat(chunks).toString("base64"),
      }));
    });
    request.end(body);
  });
}

async function runChild() {
  try {
    const payload = JSON.parse(readFileSync(0, "utf8"));
    jsonResult(await performRequest(payload));
  } catch (error) {
    jsonResult({
      status: 0,
      headers: [],
      body_base64: "",
      error: error instanceof Error ? error.message : String(error),
      error_reason: "configuration",
    });
  }
}

function decodeResponse(stdout, error) {
  if (error) {
    return {
      status: 0,
      headers_json: "[]",
      body: new Uint8Array(),
      error: error.code === "ETIMEDOUT" ? "network broker timed out" : error.message,
      error_reason: error.code === "ETIMEDOUT" ? "timeout" : "connection",
    };
  }
  let response;
  try {
    response = JSON.parse(stdout);
  } catch {
    return {
      status: 0,
      headers_json: "[]",
      body: new Uint8Array(),
      error: "network broker returned invalid JSON",
      error_reason: "connection",
    };
  }
  return {
    status: Number.isInteger(response.status) ? response.status : 0,
    headers_json: JSON.stringify(response.headers || []),
    body: Uint8Array.from(Buffer.from(response.body_base64 || "", "base64")),
    ...(response.error ? { error: response.error } : {}),
    ...(response.error_reason ? { error_reason: response.error_reason } : {}),
  };
}

/**
 * Create the synchronous transport callback consumed by WasmShell.
 *
 * Node's HTTP API is asynchronous, while wasmsh utilities run synchronously.
 * The callback therefore runs a short-lived Node child that performs exactly
 * one request. The child never follows redirects; Rust validates every hop
 * before calling it again. No shell or Python runtime is involved.
 */
export function createNodeNetworkBroker() {
  return (url, method, headersJson, body, bodyLength, followRedirects, optionsJson) => {
    let headers;
    try {
      headers = JSON.parse(headersJson || "[]");
      const bytes = body instanceof Uint8Array ? body.subarray(0, bodyLength) : new Uint8Array();
      const options = asChildOptions(optionsJson);
      if (bytes.length > MAX_REQUEST_BYTES) {
        return {
          status: 0,
          headers_json: "[]",
          body: new Uint8Array(),
          error: `request body exceeds ${MAX_REQUEST_BYTES} bytes`,
          error_reason: "request_too_large",
        };
      }
      const child = spawnSync(
        process.execPath,
        [CHILD_SCRIPT, "--child"],
        {
          input: JSON.stringify({
            url,
            method,
            headers,
            body_base64: Buffer.from(bytes).toString("base64"),
            follow_redirects: Boolean(followRedirects),
            options_json: optionsJson || "{}",
          }),
          encoding: "utf8",
          maxBuffer: MAX_RESPONSE_BYTES * 2,
          timeout: options.timeout + 5_000,
          windowsHide: true,
          shell: false,
        },
      );
      return decodeResponse(child.stdout, child.error);
    } catch (error) {
      return {
        status: 0,
        headers_json: "[]",
        body: new Uint8Array(),
        error: error instanceof Error ? error.message : String(error),
        error_reason: "connection",
      };
    }
  };
}

/** Install the broker at the global name used by wasm-bindgen's import. */
export function installNodeNetworkBroker() {
  const broker = createNodeNetworkBroker();
  const previous = globalThis.wasmsh_http_fetch;
  globalThis.wasmsh_http_fetch = broker;
  return () => {
    if (globalThis.wasmsh_http_fetch !== broker) return;
    if (previous === undefined) delete globalThis.wasmsh_http_fetch;
    else globalThis.wasmsh_http_fetch = previous;
  };
}

if (process.argv[2] === "--child") {
  await runChild();
}
