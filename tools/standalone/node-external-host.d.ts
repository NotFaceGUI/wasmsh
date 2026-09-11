export interface ExternalExecutionResult {
  status: number;
  stdout: Uint8Array;
  stderr: Uint8Array;
}

export type ExternalExecutor = (
  commandName: string,
  fixedExecutable: string,
  argv: string[],
  stdin: Uint8Array,
  optionsJson: string,
) => ExternalExecutionResult;

export function createNodeExternalExecutor(): ExternalExecutor;

export interface ExternalStreamRequest {
  operation: "start" | "write_stdin" | "close_stdin" | "poll" | "cancel";
  process_id: string;
  command_name: string;
  executable: string;
  argv: string[];
  data: Uint8Array;
  options_json: string;
}

export interface ExternalStreamResponse {
  process_id?: string;
  accepted?: number;
  would_block?: boolean;
  closed?: boolean;
  stdout?: Uint8Array;
  stderr?: Uint8Array;
  stdout_eof?: boolean;
  stderr_eof?: boolean;
  stdin_writable?: boolean;
  status?: number | null;
  error?: string | null;
}

export type ExternalStreamExecutor = (
  request: ExternalStreamRequest,
) => ExternalStreamResponse;

export function createNodeExternalStreamExecutor(): ExternalStreamExecutor;
