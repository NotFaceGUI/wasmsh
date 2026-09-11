import { spawn, spawnSync } from "node:child_process";
import { join, posix } from "node:path";

const MAX_BUFFER_BYTES = 64 * 1024 * 1024;
const MAX_TIMEOUT_MS = 300_000;

function asOptions(optionsJson) {
  let options;
  try {
    options = JSON.parse(optionsJson || "{}");
  } catch (error) {
    throw new Error(`external options are not valid JSON: ${error.message}`);
  }
  if (!options || Array.isArray(options) || typeof options !== "object") {
    throw new Error("external options must be a JSON object");
  }
  const maxInput = options.max_input_bytes ?? 16 * 1024 * 1024;
  const maxOutput = options.max_output_bytes ?? 16 * 1024 * 1024;
  const timeout = options.timeout_ms ?? 30_000;
  const streamQueue = options.stream_queue_bytes ?? 64 * 1024;
  const streamChunk = options.stream_chunk_bytes ?? 4096;
  for (const [label, value, limit] of [
    ["max_input_bytes", maxInput, MAX_BUFFER_BYTES],
    ["max_output_bytes", maxOutput, MAX_BUFFER_BYTES],
    ["timeout_ms", timeout, MAX_TIMEOUT_MS],
    ["stream_queue_bytes", streamQueue, 1024 * 1024],
    ["stream_chunk_bytes", streamChunk, 64 * 1024],
  ]) {
    if (!Number.isSafeInteger(value) || value < 1 || value > limit) {
      throw new Error(`${label} is outside the finite external-host limit`);
    }
  }
  if (options.cwd !== null && typeof options.cwd !== "string") {
    throw new Error("external cwd must be a host filesystem string or null");
  }
  if (options.cwd === null || options.cwd === undefined) {
    throw new Error("external cwd is not configured; VFS cwd is never inferred");
  }
  if (!options.env || typeof options.env !== "object" || Array.isArray(options.env)) {
    throw new Error("external env must be an explicit object");
  }
  return {
    ...options,
    maxInput,
    maxOutput,
    timeout,
    stream_queue_bytes: streamQueue,
    stream_chunk_bytes: streamChunk,
  };
}

function mapVirtualPath(value, mappings) {
  if (!value.startsWith("/")) {
    return value;
  }
  const normalized = posix.normalize(value);
  const mapping = [...mappings]
    .sort((left, right) => right.vfs_prefix.length - left.vfs_prefix.length)
    .find(({ vfs_prefix }) =>
      normalized === vfs_prefix || normalized.startsWith(`${vfs_prefix}/`),
    );
  if (!mapping) {
    throw new Error(`unmapped VFS path argument: ${value}`);
  }
  const suffix = normalized.slice(mapping.vfs_prefix.length).replace(/^\//, "");
  return suffix ? join(mapping.host_prefix, suffix) : mapping.host_prefix;
}

function bytes(value, label) {
  if (value === undefined || value === null) {
    return Buffer.alloc(0);
  }
  if (!(value instanceof Uint8Array) && !Buffer.isBuffer(value)) {
    throw new Error(`external ${label} must be a byte array`);
  }
  return Buffer.from(value);
}

function boundedStreams(stdout, stderr, limit) {
  const boundedStdout = stdout.subarray(0, limit);
  const remaining = Math.max(0, limit - boundedStdout.length);
  return {
    stdout: boundedStdout,
    stderr: stderr.subarray(0, remaining),
  };
}

function withDiagnostic(stdout, stderr, message, limit) {
  const diagnostic = Buffer.from(`${message}\n`, "utf8");
  const bounded = boundedStreams(stdout, stderr, limit);
  if (bounded.stdout.length >= limit) {
    return bounded;
  }
  const remaining = limit - bounded.stdout.length;
  return {
    stdout: bounded.stdout,
    stderr: Buffer.concat([bounded.stderr, diagnostic]).subarray(0, remaining),
  };
}

function signalStatus(signal) {
  const numbers = {
    SIGINT: 2,
    SIGTERM: 15,
    SIGKILL: 9,
    SIGHUP: 1,
  };
  return 128 + (numbers[signal] || 1);
}

function resultStreams(result) {
  return {
    stdout: Buffer.isBuffer(result.stdout) ? result.stdout : Buffer.from(result.stdout || []),
    stderr: Buffer.isBuffer(result.stderr) ? result.stderr : Buffer.from(result.stderr || []),
  };
}

/**
 * Create the synchronous executor consumed by WasmShell.set_external_executor.
 *
 * The runtime has already parsed shell argv. This adapter removes argv[0]
 * exactly once, prepends only trusted fixed arguments from the registration,
 * and invokes the trusted executable with shell:false. It deliberately does
 * not inherit process.env and refuses an unconfigured host cwd.
 */
export function createNodeExternalExecutor() {
  return (commandName, executable, argv, stdin, optionsJson) => {
    let options;
    try {
      options = asOptions(optionsJson);
      if (!Array.isArray(argv) || argv[0] !== commandName) {
        throw new Error("external argv must include the registered command as argv[0]");
      }
      const args = [
        ...options.argv_prefix,
        ...argv.slice(1).map((arg) => mapVirtualPath(String(arg), options.vfs_path_mappings || [])),
      ];
      const input = bytes(stdin, "stdin");
      if (input.length > options.maxInput) {
        return {
          status: 125,
          stdout: new Uint8Array(),
          stderr: Buffer.from(`wasmsh: ${commandName}: external stdin limit exceeded\n`),
        };
      }
      const env = Object.fromEntries(
        Object.entries(options.env).map(([key, value]) => [key, String(value)]),
      );
      const child = spawnSync(executable, args, {
        cwd: options.cwd,
        env,
        input,
        encoding: null,
        shell: false,
        timeout: options.timeout,
        maxBuffer: options.maxOutput,
        windowsHide: true,
        stdio: ["pipe", "pipe", "pipe"],
      });
      const { stdout, stderr } = resultStreams(child);
      const total = stdout.length + stderr.length;
      const outputLimitHit =
        total > options.maxOutput ||
        child.error?.code === "ENOBUFS" ||
        child.error?.code === "ERR_CHILD_PROCESS_STDIO_MAXBUFFER";
      if (outputLimitHit) {
        const bounded = withDiagnostic(
          stdout,
          stderr,
          `wasmsh: ${commandName}: external output limit exceeded (${options.maxOutput} bytes)`,
          options.maxOutput,
        );
        return { status: 125, ...bounded };
      }
      if (child.error?.code === "ETIMEDOUT") {
        const bounded = withDiagnostic(
          stdout,
          stderr,
          `wasmsh: ${commandName}: external process timed out after ${options.timeout} ms`,
          options.maxOutput,
        );
        return { status: 124, ...bounded };
      }
      if (child.error) {
        const bounded = withDiagnostic(
          stdout,
          stderr,
          `wasmsh: ${commandName}: failed to start: ${child.error.message}`,
          options.maxOutput,
        );
        return { status: 126, ...bounded };
      }
      const status = Number.isInteger(child.status)
        ? child.status
        : signalStatus(child.signal);
      return { status, ...boundedStreams(stdout, stderr, options.maxOutput) };
    } catch (error) {
      return {
        status: 126,
        stdout: new Uint8Array(),
        stderr: Buffer.from(`wasmsh: ${commandName}: external host error: ${error.message}\n`),
      };
    }
  };
}

function streamSignalStatus(signal) {
  return signal ? signalStatus(signal) : 125;
}

function killManagedProcess(child) {
  if (!child || child.exitCode !== null) {
    return;
  }
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/pid", String(child.pid), "/t", "/f"], {
      stdio: "ignore",
      windowsHide: true,
      shell: false,
    });
  } else if (child.pid) {
    try {
      process.kill(-child.pid, "SIGTERM");
    } catch {
      child.kill("SIGTERM");
    }
    // Escalate if the process (or tree) traps/ignores SIGTERM, so cancelling
    // cannot leave a live child behind.
    const escalate = () => {
      if (child.exitCode !== null || child.signalCode !== null) {
        return;
      }
      try {
        process.kill(-child.pid, "SIGKILL");
      } catch {
        try {
          child.kill("SIGKILL");
        } catch {
          // Process already gone.
        }
      }
    };
    setTimeout(escalate, 500).unref?.();
  } else {
    child.kill("SIGTERM");
  }
}

function queueChunk(state, streamName, chunk) {
  const stream = state[streamName];
  const remainingOutput = state.options.max_output_bytes - state.producedBytes;
  const allowed = Math.min(chunk.length, remainingOutput);
  if (allowed > 0) {
    state.producedBytes += allowed;
    stream.deferred.push(chunk.subarray(0, allowed));
    stream.deferredBytes += allowed;
    promoteDeferred(stream);
  }
  if (allowed < chunk.length) {
    state.outputLimit = true;
    state.status = 125;
    killManagedProcess(state.child);
  }
  if (stream.deferredBytes > 0 || stream.queuedBytes >= state.options.stream_queue_bytes) {
    stream.source.pause();
    stream.paused = true;
  }
}

function promoteDeferred(stream) {
  const limit = stream.options.stream_queue_bytes;
  while (stream.deferred.length > 0 && stream.queuedBytes < limit) {
    const chunk = stream.deferred[0];
    const take = Math.min(limit - stream.queuedBytes, chunk.length);
    stream.chunks.push(chunk.subarray(0, take));
    stream.queuedBytes += take;
    stream.deferredBytes -= take;
    if (take === chunk.length) stream.deferred.shift();
    else stream.deferred[0] = chunk.subarray(take);
  }
  if (
    stream.paused &&
    stream.deferredBytes === 0 &&
    stream.queuedBytes <= Math.floor(limit / 2)
  ) {
    stream.source.resume();
    stream.paused = false;
  }
}

function drainQueue(stream, maxBytes) {
  if (stream.queuedBytes === 0) {
    return Buffer.alloc(0);
  }
  if (stream.queuedBytes === 0) {
    return Buffer.alloc(0);
  }
  const parts = [];
  let remaining = maxBytes;
  while (remaining > 0 && stream.chunks.length > 0) {
    const chunk = stream.chunks[0];
    const take = Math.min(remaining, chunk.length);
    parts.push(chunk.subarray(0, take));
    remaining -= take;
    stream.queuedBytes -= take;
    if (take === chunk.length) {
      stream.chunks.shift();
    } else {
      stream.chunks[0] = chunk.subarray(take);
    }
  }
  promoteDeferred(stream);
  return Buffer.concat(parts);
}

function createStreamState(child, options, commandName) {
  const state = {
    child,
    options,
    commandName,
    producedBytes: 0,
    outputLimit: false,
    status: null,
    error: null,
    stdinWritable: true,
    stdinClosed: false,
    timeoutTimer: null,
    stdout: { chunks: [], deferred: [], deferredBytes: 0, queuedBytes: 0, paused: false, ended: false, source: child.stdout, options },
    stderr: { chunks: [], deferred: [], deferredBytes: 0, queuedBytes: 0, paused: false, ended: false, source: child.stderr, options },
  };
  child.stdout.on("data", (chunk) => queueChunk(state, "stdout", Buffer.from(chunk)));
  child.stderr.on("data", (chunk) => queueChunk(state, "stderr", Buffer.from(chunk)));
  child.stdout.on("end", () => { state.stdout.ended = true; });
  child.stderr.on("end", () => { state.stderr.ended = true; });
  child.stdin.on("drain", () => { state.stdinWritable = true; });
  child.stdin.on("error", (error) => {
    state.stdinClosed = true;
    state.error ||= error.message;
  });
  child.on("error", (error) => {
    state.error = `failed to start: ${error.message}`;
    state.status = 126;
    state.stdout.ended = true;
    state.stderr.ended = true;
  });
  child.on("close", (code, signal) => {
    if (state.timeoutTimer) {
      clearTimeout(state.timeoutTimer);
      state.timeoutTimer = null;
    }
    if (state.status === null) {
      state.status = Number.isInteger(code) ? code : streamSignalStatus(signal);
    }
    state.stdout.ended = true;
    state.stderr.ended = true;
  });
  state.timeoutTimer = setTimeout(() => {
    if (state.status === null) {
      state.status = 124;
      state.error = `external process timed out after ${options.timeout} ms`;
      killManagedProcess(child);
    }
  }, options.timeout);
  return state;
}

/**
 * Create the synchronous operation callback for progressive external runs.
 * Each callback only starts, writes, closes, polls, or cancels; child output
 * arrives through bounded event queues between WASM poll calls.
 */
export function createNodeExternalStreamExecutor() {
  const processes = new Map();
  let nextProcessId = 1;
  return (request) => {
    if (!request || typeof request !== "object") {
      throw new Error("external stream request must be an object");
    }
    const operation = request.operation;
    if (operation === "start") {
      const options = asOptions(request.options_json);
      if (!Array.isArray(request.argv) || request.argv[0] !== request.command_name) {
        throw new Error("external stream argv must include command_name as argv[0]");
      }
      const args = [
        ...options.argv_prefix,
        ...request.argv.slice(1).map((arg) => mapVirtualPath(String(arg), options.vfs_path_mappings || [])),
      ];
      const child = spawn(request.executable, args, {
        cwd: options.cwd,
        env: Object.fromEntries(Object.entries(options.env).map(([key, value]) => [key, String(value)])),
        shell: false,
        detached: process.platform !== "win32",
        windowsHide: true,
        stdio: ["pipe", "pipe", "pipe"],
      });
      const processId = String(nextProcessId++);
      processes.set(processId, createStreamState(child, options, request.command_name));
      return { process_id: processId };
    }

    const state = processes.get(String(request.process_id));
    if (!state) {
      return {
        accepted: 0,
        closed: true,
        would_block: false,
        stdout: new Uint8Array(),
        stderr: new Uint8Array(),
        stdout_eof: true,
        stderr_eof: true,
        status: 126,
        stdin_writable: false,
        error: "external process handle is no longer managed",
      };
    }
    if (operation === "write_stdin") {
      if (state.stdinClosed || state.child.stdin.destroyed || state.child.exitCode !== null) {
        return { accepted: 0, closed: true, would_block: false };
      }
      if (!state.stdinWritable) {
        return { accepted: 0, closed: false, would_block: true };
      }
      const data = bytes(request.data, "stdin");
      try {
        const writable = state.child.stdin.write(data);
        state.stdinWritable = writable;
        return { accepted: data.length, closed: false, would_block: !writable };
      } catch (error) {
        state.stdinClosed = true;
        state.error = error.message;
        return { accepted: 0, closed: true, would_block: false };
      }
    }
    if (operation === "close_stdin") {
      if (!state.stdinClosed) {
        state.stdinClosed = true;
        state.child.stdin.end();
      }
      return {};
    }
    if (operation === "poll") {
      const stdout = drainQueue(state.stdout, state.options.stream_chunk_bytes);
      const stderr = drainQueue(state.stderr, state.options.stream_chunk_bytes);
      const stdoutEof = state.stdout.ended && state.stdout.queuedBytes === 0;
      const stderrEof = state.stderr.ended && state.stderr.queuedBytes === 0;
      const response = {
        stdout,
        stderr,
        stdout_eof: stdoutEof,
        stderr_eof: stderrEof,
        status: state.status,
        stdin_writable: state.stdinWritable,
        error: state.error,
      };
      if (stdoutEof && stderrEof && state.status !== null) {
        processes.delete(String(request.process_id));
      }
      return response;
    }
    if (operation === "cancel") {
      if (state.timeoutTimer) {
        clearTimeout(state.timeoutTimer);
        state.timeoutTimer = null;
      }
      killManagedProcess(state.child);
      state.child.stdin.destroy();
      state.child.stdout.destroy();
      state.child.stderr.destroy();
      processes.delete(String(request.process_id));
      return {};
    }
    throw new Error(`unknown external stream operation: ${operation}`);
  };
}
