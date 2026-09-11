export type WasmHttpFetch = (
  url: string,
  method: string,
  headersJson: string,
  body: Uint8Array,
  bodyLength: number,
  followRedirects: boolean,
  optionsJson: string,
) => {
  status: number;
  headers_json: string;
  body: Uint8Array;
  error?: string;
  error_reason?: "configuration" | "connection" | "invalid_url" | "request_too_large" | "response_too_large" | "timeout";
};

/** Create a synchronous, no-redirect HTTP(S) transport for WasmShell. */
export declare function createNodeNetworkBroker(): WasmHttpFetch;

/** Install the transport at globalThis.wasmsh_http_fetch and return cleanup. */
export declare function installNodeNetworkBroker(): () => void;
