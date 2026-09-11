/**
 * Web Worker bootstrap for wasmsh.
 *
 * Accepts messages of the form:
 *   { type: "Init", step_budget: number, allowed_hosts?: string[], network_policy?: object }
 *   { type: "Run",  input: string }
 *   { type: "StartRun", input: string }
 *   { type: "PollRun" }
 *   { type: "Cancel" }
 *   { type: "WriteFile", path: string, data: number[] }
 *   { type: "ReadFile",  path: string }
 *   { type: "ListDir",   path: string }
 *   { type: "Clock", unix_ms?: number, values?: number[], mode?: string }
 *   { type: "RegisterExternal", name: string, executable: string, options?: object }
 *   { type: "UnregisterExternal", name: string }
 *   { type: "ExternalCommands" }
 *
 * Replies with { events: WorkerEvent[] } where events are the parsed
 * JSON array returned by WasmShell methods.
 */
import wasmInit, { WasmShell } from "./pkg/wasmsh_browser.js";

let shell = null;
let ready = false;
const pending = [];
let clockMode = "real";
let clockValue = 0;
let clockValues = [];

function installClock() {
  if (clockMode === "real") {
    shell.set_clock_callback(() => Date.now());
  } else if (clockMode === "sequence") {
    shell.set_clock_callback(() => clockValues.shift() ?? clockValue);
  } else if (clockMode === "fixed") {
    shell.set_fixed_time_ms(BigInt(clockValue));
  } else if (clockMode === "throw") {
    shell.set_clock_callback(() => {
      throw new Error("test clock failure");
    });
  } else if (clockMode === "invalid") {
    shell.set_clock_callback(() => NaN);
  } else if (clockMode === "unsafe") {
    shell.set_clock_callback(() => Number.MAX_SAFE_INTEGER + 1);
  } else {
    shell.clear_clock_callback();
  }
}

function networkConfigForMessage(msg) {
  const hasAllowedHosts = Object.prototype.hasOwnProperty.call(msg, "allowed_hosts");
  const hasNetworkPolicy = Object.prototype.hasOwnProperty.call(msg, "network_policy");
  if (hasAllowedHosts || hasNetworkPolicy) {
    // Keep both fields when present so the WASM boundary can reject a
    // conflicting legacy/structured configuration instead of dropping one.
    return {
      ...(hasAllowedHosts ? { allowed_hosts: msg.allowed_hosts } : {}),
      ...(hasNetworkPolicy ? { network_policy: msg.network_policy } : {}),
    };
  }
  return [];
}

/**
 * Synchronous HTTP fetch for wasmsh curl/wget utilities.
 * Called from WASM via wasm-bindgen extern. Uses synchronous XMLHttpRequest
 * which is available in Web Worker contexts.
 */
self.wasmsh_http_fetch = function () {
  // Synchronous XHR follows redirects before Rust can inspect the next hop.
  // This fixture deliberately refuses the path; a trusted redirect-aware
  // broker is required for browser network enablement.
  return {
    status: 0,
    headers_json: "[]",
    body: new Uint8Array(0),
    error: "synchronous XHR is refused because this browser worker cannot guarantee per-hop redirect policy",
  };
};

async function boot() {
  await wasmInit();
  shell = new WasmShell();
  installClock();
  ready = true;
  // Drain any messages that arrived while loading.
  for (const msg of pending) {
    handle(msg);
  }
  pending.length = 0;
}

function handle(msg) {
  let json;
  switch (msg.type) {
    case "Clock":
      clockMode = msg.mode ?? (Array.isArray(msg.values) ? "sequence" : "fixed");
      clockValue = Number(msg.unix_ms ?? 0);
      clockValues = Array.isArray(msg.values) ? msg.values.map(Number) : [];
      installClock();
      json = "[]";
      break;
    case "Init":
      json = shell.init(
        BigInt(msg.step_budget ?? 0),
        JSON.stringify(networkConfigForMessage(msg)),
      );
      break;
    case "Run":
      json = shell.exec(msg.input);
      break;
    case "StartRun":
      json = shell.start_run(msg.input);
      break;
    case "PollRun":
      json = shell.poll_run();
      break;
    case "WriteFile":
      json = shell.write_file(msg.path, new Uint8Array(msg.data));
      break;
    case "ReadFile":
      json = shell.read_file(msg.path);
      break;
    case "ListDir":
      json = shell.list_dir(msg.path);
      break;
    case "RegisterExternal":
      try {
        shell.register_external(
          msg.name,
          msg.executable,
          JSON.stringify(msg.options ?? {}),
        );
        json = "[]";
      } catch (error) {
        json = JSON.stringify([
          { Diagnostic: ["Error", `invalid external registration: ${error.message}`] },
        ]);
      }
      break;
    case "UnregisterExternal":
      json = JSON.stringify([{ ExternalUnregistered: shell.unregister_external(msg.name) }]);
      break;
    case "ExternalCommands":
      json = JSON.stringify([{ ExternalCommands: JSON.parse(shell.external_commands()) }]);
      break;
    case "Cancel":
      json = shell.cancel();
      break;
    default:
      self.postMessage({ error: "unknown command: " + msg.type });
      return;
  }
  self.postMessage({ events: JSON.parse(json) });
}

self.onmessage = function (e) {
  if (!ready) {
    pending.push(e.data);
  } else {
    handle(e.data);
  }
};

boot().catch(function (err) {
  self.postMessage({ error: err.message || String(err) });
});
