//! Shared shell runtime core for wasmsh.
//!
//! Platform-agnostic execution engine:
//! `parse -> AST -> HIR -> runtime executor`.
//!
//! Most shell semantics are executed by interpreting HIR directly inside
//! this crate. A bounded subset of top-level `and/or` lists is lowered
//! through `wasmsh-ir` into `wasmsh-vm`, but that is an optimization and
//! parity path rather than the primary executor for the whole grammar.

mod dbl_bracket;
mod fd_table;
mod pattern;
mod signals;
mod streaming_cut;
mod streaming_grep;
mod streaming_sed;
mod streaming_tr;
mod streaming_uniq;

use streaming_cut::{
    CutStreamReader, StreamingCutMode, StreamingCutParseState, StreamingCutRange, StreamingCutStage,
};
use streaming_grep::{GrepStreamReader, StreamingGrepFlags, StreamingGrepStage, StreamingGrepStep};
use streaming_sed::{parse_streaming_sed_script, SedStreamReader, StreamingSedStage};
use streaming_tr::{streaming_tr_expand_set, StreamingTrStage, TrStreamReader};
use streaming_uniq::{StreamingUniqFlags, UniqStreamReader};

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::io::{Cursor, ErrorKind, Read};
use std::rc::Rc;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::dbl_bracket::dbl_bracket_eval_or;
use crate::fd_table::{ExecIo, InputTarget, OutputTarget};
use crate::pattern::{glob_match_ext, glob_match_inner, has_extglob_pattern};
use crate::signals::{find_runtime_signal_spec, RuntimeSignalSpec, SignalDefaultAction};

pub use crate::pattern::extglob_match;
use wasmsh_ast::{CaseTerminator, RedirectionOp, Span, Word, WordPart};
use wasmsh_expand::expand_words_argv;
use wasmsh_fs::{BackendFs, FileHandle, OpenOptions, Vfs, VfsWriteSink};
use wasmsh_hir::{
    HirAndOr, HirAndOrOp, HirCommand, HirCompleteCommand, HirPipeline, HirProgram, HirRedirection,
};
use wasmsh_ir::{lower_supported_and_or, IrProgram, IrRedirection, LoweringError};
use wasmsh_protocol::{
    DiagnosticLevel, HostCommand, NetworkPolicyConfig as ProtocolNetworkPolicyConfig, WorkerEvent,
    PROTOCOL_VERSION,
};
use wasmsh_state::ShellState;
use wasmsh_utils::net_types::{NetworkBackend, NetworkError, NetworkPolicy};
#[cfg(not(target_arch = "wasm32"))]
use wasmsh_utils::SystemClock;
#[cfg(target_arch = "wasm32")]
use wasmsh_utils::UnavailableClock;
use wasmsh_utils::{ClockProvider, UtilContext, UtilRegistry};
use wasmsh_vm::pipe::{PipeBuffer, ReadResult, WriteResult};
use wasmsh_vm::{BudgetCategory, ExecutionLimits, ExhaustionReason, StopReason, Vm, VmExecutor};

/// Sentinel FD value for `&>` (redirect both stdout and stderr).
const FD_BOTH: u32 = u32::MAX;

// Runtime-level command names dispatched before builtins.
const CMD_LOCAL: &str = "local";
const CMD_BREAK: &str = "break";
const CMD_CONTINUE: &str = "continue";
const CMD_RETURN: &str = "return";
const CMD_EXIT: &str = "exit";
const CMD_EVAL: &str = "eval";
const CMD_SOURCE: &str = "source";
const CMD_DOT: &str = ".";
const CMD_DECLARE: &str = "declare";
const CMD_TYPESET: &str = "typeset";
const CMD_LET: &str = "let";
const CMD_SHOPT: &str = "shopt";
const CMD_ALIAS: &str = "alias";
const CMD_UNALIAS: &str = "unalias";
const CMD_BUILTIN: &str = "builtin";
const CMD_MAPFILE: &str = "mapfile";
const CMD_READARRAY: &str = "readarray";
const CMD_TYPE: &str = "type";
const CMD_COMMAND: &str = "command";
const CMD_EXEC: &str = "exec";
const CMD_HASH: &str = "hash";
const CMD_TIMES: &str = "times";
const CMD_DIRS: &str = "dirs";
const CMD_PUSHD: &str = "pushd";
const CMD_POPD: &str = "popd";
const CMD_UMASK: &str = "umask";
const CMD_WAIT: &str = "wait";
const CMD_ULIMIT: &str = "ulimit";

/// Configuration for the browser runtime.
#[derive(Debug, Clone)]
pub struct BrowserConfig {
    pub step_budget: u64,
    /// Hostnames/IPs allowed for network access (empty = no network).
    pub allowed_hosts: Vec<String>,
    pub output_byte_limit: u64,
    pub pipe_byte_limit: u64,
    /// Maximum bytes an external command may receive through stdin.
    pub external_input_byte_limit: u64,
    /// Maximum combined stdout/stderr bytes retained from one external command.
    pub external_output_byte_limit: u64,
    pub recursion_limit: u32,
    pub vm_subset_enabled: bool,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            step_budget: 100_000,
            allowed_hosts: Vec::new(),
            // Secure defaults (B4 from external audit). Set to nonzero so a
            // caller who forgets to configure limits still gets a 64 MiB
            // visible cap rather than unlimited memory growth. The previous
            // 0 == unlimited sentinel is still honored by the runtime path
            // when callers opt in explicitly via set_output_byte_limit(0).
            output_byte_limit: 64 * 1024 * 1024,
            pipe_byte_limit: 64 * 1024 * 1024,
            external_input_byte_limit: DEFAULT_EXTERNAL_INPUT_BYTES,
            external_output_byte_limit: DEFAULT_EXTERNAL_OUTPUT_BYTES,
            recursion_limit: MAX_RECURSION_DEPTH,
            vm_subset_enabled: true,
        }
    }
}

/// Maximum recursion depth for eval, source, function calls, and command
/// substitution. These share one counter, which bounds the *total* nesting
/// even when a function body contains a substitution.
///
/// Measured overflow on a ~1 MiB stack (native main thread and the WASM
/// default): ~84 nested command substitutions and ~120 nested function calls.
/// 48 keeps roughly a 2x margin below the tighter of the two while still
/// allowing realistic recursive scripts (tree walks, backtracking). The
/// previous value of 100 sat above the substitution overflow point.
const MAX_RECURSION_DEPTH: u32 = 48;

/// Transient execution state, reset between top-level commands.
#[derive(Clone)]
#[allow(clippy::struct_excessive_bools)]
struct ExecState {
    break_depth: u32,
    loop_continue: bool,
    /// Set by the `return` builtin to unwind the current function (or sourced
    /// file). Holds the return status. Unlike `exit_requested` it is cleared
    /// when the enclosing function frame finishes.
    return_requested: Option<i32>,
    exit_requested: Option<i32>,
    errexit_suppressed: bool,
    local_save_stack: Vec<(smol_str::SmolStr, Option<smol_str::SmolStr>)>,
    recursion_depth: u32,
    /// Set when a resource limit (step budget, output limit, cancel) is hit.
    resource_exhausted: bool,
    stop_reason: Option<StopReason>,
    /// Set when word expansion reports a hard semantic error.
    expansion_failed: bool,
    /// Trap handlers suppress nested trap reentry while they run.
    trap_depth: u32,
    /// Nested shell scopes (functions, sourced files, command substitutions).
    nested_shell_depth: u32,
    /// Nested output capture scopes for pipelines and substitutions.
    output_captures: Vec<OutputCapture>,
    /// Snapshot of the EXIT trap handler at the start of the current run, used
    /// to decide whether a normally-completing script installed one.
    exit_trap_at_run_start: Option<String>,
}

impl ExecState {
    fn new() -> Self {
        Self {
            break_depth: 0,
            loop_continue: false,
            return_requested: None,
            exit_requested: None,
            errexit_suppressed: false,
            local_save_stack: Vec::new(),
            recursion_depth: 0,
            resource_exhausted: false,
            stop_reason: None,
            expansion_failed: false,
            trap_depth: 0,
            nested_shell_depth: 0,
            output_captures: Vec::new(),
            exit_trap_at_run_start: None,
        }
    }

    fn reset(&mut self) {
        self.break_depth = 0;
        self.loop_continue = false;
        self.return_requested = None;
        self.exit_requested = None;
        self.errexit_suppressed = false;
        self.resource_exhausted = false;
        self.stop_reason = None;
        self.expansion_failed = false;
        self.trap_depth = 0;
        self.nested_shell_depth = 0;
        self.output_captures.clear();
    }
}

const STREAMING_YES_MAX_LINES: usize = 65_536;
const PIPEBUFFER_STREAMING_CAPACITY: usize = 1;
/// Bounded queue capacity used by progressive external pipelines.
const EXTERNAL_STREAM_PIPE_CAPACITY: usize = 64 * 1024;

#[derive(Clone, Debug, Default)]
struct OutputCapture {
    capture_stdout: bool,
    capture_stderr: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
struct CapturedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct RuntimeOutputRouter<'a> {
    exec: &'a mut ExecState,
    exec_io: Option<&'a mut ExecIo>,
    proc_subst_out_scopes: &'a mut Vec<Vec<PendingProcessSubstOut>>,
    vm_stdout: &'a mut Vec<u8>,
    vm_stderr: &'a mut Vec<u8>,
    vm_output_bytes: &'a mut u64,
    vm_output_limit: u64,
    vm_diagnostics: &'a mut Vec<wasmsh_vm::DiagnosticEvent>,
}

impl RuntimeOutputRouter<'_> {
    fn process_subst_out_sink_mut(&mut self, path: &str) -> Option<&mut PendingProcessSubstOut> {
        for scope in self.proc_subst_out_scopes.iter_mut().rev() {
            if let Some(index) = scope.iter().position(|sink| sink.path == path) {
                return scope.get_mut(index);
            }
        }
        None
    }

    fn append_visible_output_direct(&mut self, data: &[u8], stdout: bool) {
        if stdout {
            self.vm_stdout.extend_from_slice(data);
        } else {
            self.vm_stderr.extend_from_slice(data);
        }
    }

    fn write_output_destination_direct(&mut self, destination: &OutputTarget, data: &[u8]) -> bool {
        match destination {
            OutputTarget::InheritStdout => {
                self.append_visible_output_direct(data, true);
                true
            }
            OutputTarget::InheritStderr => {
                self.append_visible_output_direct(data, false);
                true
            }
            OutputTarget::ProcessSubst { path } => {
                if let Some(sink) = self.process_subst_out_sink_mut(path) {
                    sink.write(data);
                }
                false
            }
            OutputTarget::File { path, sink, .. } => {
                if let Err(err) = sink.borrow_mut().write(data) {
                    let msg = format!("wasmsh: write error: {err}\n");
                    self.append_visible_output_direct(msg.as_bytes(), false);
                    self.vm_diagnostics.push(wasmsh_vm::DiagnosticEvent {
                        level: wasmsh_vm::DiagLevel::Error,
                        category: wasmsh_vm::DiagCategory::Filesystem,
                        message: format!("write failed for {path}: {err}"),
                    });
                }
                false
            }
            OutputTarget::Pipe(pipe) => {
                pipe.borrow_mut().write_all(data);
                false
            }
            OutputTarget::Closed => false,
        }
    }

    fn route_output(&mut self, data: &[u8], stdout: bool) -> bool {
        let mut routed_stdout = stdout;
        if let Some(exec_io) = self.exec_io.as_deref_mut() {
            let destination = exec_io.output_target(stdout);
            match destination {
                OutputTarget::InheritStdout => {
                    routed_stdout = true;
                }
                OutputTarget::InheritStderr => {
                    routed_stdout = false;
                }
                OutputTarget::File { .. }
                | OutputTarget::ProcessSubst { .. }
                | OutputTarget::Pipe(_)
                | OutputTarget::Closed => {
                    return self.write_output_destination_direct(&destination, data);
                }
            }
        }

        for capture in self.exec.output_captures.iter_mut().rev() {
            let should_capture = if routed_stdout {
                capture.capture_stdout
            } else {
                capture.capture_stderr
            };
            if !should_capture {
                continue;
            }
            if routed_stdout {
                capture.stdout.extend_from_slice(data);
            } else {
                capture.stderr.extend_from_slice(data);
            }
            return false;
        }

        self.append_visible_output_direct(data, routed_stdout);
        true
    }

    fn account_output(&mut self, bytes: usize) {
        *self.vm_output_bytes += bytes as u64;
        self.exec.stop_reason = None;
        if self.exec.resource_exhausted {
            return;
        }
        let used = *self.vm_output_bytes;
        if self.vm_output_limit > 0 && used > self.vm_output_limit {
            let reason = ExhaustionReason {
                category: BudgetCategory::VisibleOutputBytes,
                used,
                limit: self.vm_output_limit,
            };
            self.exec.resource_exhausted = true;
            self.exec.stop_reason = Some(StopReason::Exhausted(reason.clone()));
            self.vm_diagnostics.push(wasmsh_vm::DiagnosticEvent {
                level: wasmsh_vm::DiagLevel::Error,
                category: wasmsh_vm::DiagCategory::Budget,
                message: reason.diagnostic_message(),
            });
        }
    }

    fn write_stdout(&mut self, data: &[u8]) {
        if self.route_output(data, true) {
            self.account_output(data.len());
        }
    }

    fn write_stderr(&mut self, data: &[u8]) {
        if self.route_output(data, false) {
            self.account_output(data.len());
        }
    }
}

struct RuntimeBuiltinSink<'a> {
    router: &'a mut RuntimeOutputRouter<'a>,
}

impl wasmsh_builtins::OutputSink for RuntimeBuiltinSink<'_> {
    fn stdout(&mut self, data: &[u8]) {
        self.router.write_stdout(data);
    }

    fn stderr(&mut self, data: &[u8]) {
        self.router.write_stderr(data);
    }
}

struct RuntimeUtilSink<'a> {
    router: &'a mut RuntimeOutputRouter<'a>,
    command_pipes: &'a RefCell<Vec<(String, Vec<u8>)>>,
}

impl wasmsh_utils::UtilOutput for RuntimeUtilSink<'_> {
    fn stdout(&mut self, data: &[u8]) {
        self.router.write_stdout(data);
    }

    fn stderr(&mut self, data: &[u8]) {
        self.router.write_stderr(data);
    }

    fn command_pipe(&mut self, command: &str, data: &[u8]) {
        self.command_pipes
            .borrow_mut()
            .push((command.to_string(), data.to_vec()));
    }
}

/// Wrap a single `WordPart` as a standalone `Word` for one-part expansion.
fn synthetic_word(part: &WordPart) -> Word {
    Word {
        parts: vec![part.clone()],
        span: Span { start: 0, end: 0 },
    }
}

fn resolve_path_from_cwd(cwd: &str, path: &str) -> String {
    if path.starts_with('/') {
        wasmsh_fs::normalize_path(path)
    } else {
        wasmsh_fs::normalize_path(&format!("{cwd}/{path}"))
    }
}

/// Scan `$(...)` starting just past the opening `(`, returning
/// `(end_index, inner)` where `end_index` is past the closing `)`.
fn scan_command_subst(bytes: &[u8], open: usize) -> Option<(usize, &str)> {
    let mut depth = 1usize;
    let mut i = open;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == q {
                quote = None;
            } else if b == b'\\' && q == b'"' {
                i += 1;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' | b'"' => quote = Some(b),
            b'\\' => i += 1,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((i + 1, std::str::from_utf8(&bytes[open..i]).ok()?));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Scan a `` `...` `` substitution starting after the opening backtick.
fn scan_backtick(bytes: &[u8], open: usize) -> Option<(usize, &str)> {
    let mut i = open;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == b'`' {
            return Some((i + 1, std::str::from_utf8(&bytes[open..i]).ok()?));
        }
        i += 1;
    }
    None
}

/// Scan a nested `$(( ... ))` starting at the `$`, returning the end index.
fn scan_arith_double_paren(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = start + 2; // past `$(`
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                if depth == 0 {
                    if bytes.get(i + 1) == Some(&b')') {
                        return Some(i + 2);
                    }
                    return None;
                }
                depth -= 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

struct PipeReader {
    pipe: Rc<RefCell<PipeBuffer>>,
}

impl PipeReader {
    fn new(pipe: Rc<RefCell<PipeBuffer>>) -> Self {
        Self { pipe }
    }
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.pipe.borrow_mut().read(buf) {
            ReadResult::Read(read) => Ok(read),
            ReadResult::WouldBlock => Err(std::io::Error::new(ErrorKind::WouldBlock, "pipe empty")),
            ReadResult::Eof => Ok(0),
        }
    }
}

impl Drop for PipeReader {
    fn drop(&mut self) {
        self.pipe.borrow_mut().close_read();
    }
}

#[derive(Clone, Copy)]
enum PipeProcessPoll {
    Ready,
    PendingRead,
    PendingWrite,
    Exited,
}

struct LiveProcessSubstRunner {
    isolated_runtime: Option<Box<WorkerRuntime>>,
    source_pipe: Rc<RefCell<PipeBuffer>>,
    processes: Vec<StreamingPipeProcess<'static>>,
    finished: Vec<bool>,
    final_pipe: Rc<RefCell<PipeBuffer>>,
    stage_stderr: Vec<Rc<RefCell<Vec<u8>>>>,
    stage_pipe_stderr: Vec<bool>,
    captured_stdout: Vec<u8>,
    captured_stderr: Vec<u8>,
    captured_diagnostics: Vec<wasmsh_vm::DiagnosticEvent>,
    done: bool,
    synced_steps: u64,
}

struct LiveProcessSubstInReader {
    isolated_runtime: Option<Box<WorkerRuntime>>,
    processes: Vec<StreamingPipeProcess<'static>>,
    finished: Vec<bool>,
    final_pipe: Rc<RefCell<PipeBuffer>>,
    stage_stderr: Vec<Rc<RefCell<Vec<u8>>>>,
    stage_pipe_stderr: Vec<bool>,
    flushed_stderr: Rc<RefCell<Vec<u8>>>,
    flushed_diagnostics: Rc<RefCell<Vec<wasmsh_vm::DiagnosticEvent>>>,
    done: bool,
}

impl LiveProcessSubstInReader {
    fn finalize_stderr(&mut self) {
        let mut flushed = self.flushed_stderr.borrow_mut();
        for (idx, stderr) in self.stage_stderr.iter().enumerate() {
            if self.stage_pipe_stderr[idx] {
                continue;
            }
            let data = stderr.borrow();
            if !data.is_empty() {
                flushed.extend_from_slice(&data);
            }
        }
        if let Some(runtime) = self.isolated_runtime.as_mut() {
            self.flushed_diagnostics
                .borrow_mut()
                .extend(runtime.vm.diagnostics.drain(..));
        }
    }

    fn pump(&mut self) -> bool {
        if self.done {
            return false;
        }
        let progressed = if self.isolated_runtime.is_some() {
            self.pump_with_isolated_runtime()
        } else {
            self.pump_without_runtime_loop()
        };
        if self.finished.iter().all(|done| *done) {
            self.finalize_stderr();
            self.done = true;
        }
        progressed
    }

    fn pump_with_isolated_runtime(&mut self) -> bool {
        let runtime = self
            .isolated_runtime
            .as_mut()
            .expect("isolated runtime present");
        let mut progressed = false;
        for idx in (0..self.processes.len()).rev() {
            if self.finished[idx] {
                continue;
            }
            let outcome = self.processes[idx].poll(runtime.as_mut());
            if apply_process_poll_outcome(&mut self.finished[idx], outcome) {
                progressed = true;
            }
        }
        progressed
    }

    fn pump_without_runtime_loop(&mut self) -> bool {
        let mut progressed = false;
        for idx in (0..self.processes.len()).rev() {
            if self.finished[idx] {
                continue;
            }
            let outcome = self.processes[idx].poll_without_runtime();
            if apply_process_poll_outcome(&mut self.finished[idx], outcome) {
                progressed = true;
            }
        }
        progressed
    }
}

fn apply_process_poll_outcome(finished: &mut bool, outcome: PipeProcessPoll) -> bool {
    match outcome {
        PipeProcessPoll::Ready => true,
        PipeProcessPoll::PendingRead | PipeProcessPoll::PendingWrite => false,
        PipeProcessPoll::Exited => {
            *finished = true;
            true
        }
    }
}

impl Read for LiveProcessSubstInReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let read_result = {
                let mut pipe = self.final_pipe.borrow_mut();
                pipe.read(buf)
            };
            match read_result {
                ReadResult::Read(read) => return Ok(read),
                ReadResult::Eof if self.done => return Ok(0),
                ReadResult::WouldBlock | ReadResult::Eof => {}
            }

            if !self.pump() {
                if self.done {
                    continue;
                }
                return Err(std::io::Error::new(
                    ErrorKind::WouldBlock,
                    "process substitution pipeline stalled",
                ));
            }
        }
    }
}

impl Drop for LiveProcessSubstInReader {
    fn drop(&mut self) {
        self.final_pipe.borrow_mut().close_read();
        if let Some(runtime) = self.isolated_runtime.as_mut() {
            for process in &mut self.processes {
                process.close(runtime.as_mut());
            }
        } else {
            for process in &mut self.processes {
                process.close_without_runtime();
            }
        }
    }
}

impl LiveProcessSubstRunner {
    fn sync_isolated_runtime_with_parent(&mut self, parent: &mut WorkerRuntime) {
        let Some(runtime) = self.isolated_runtime.as_mut() else {
            return;
        };
        if parent.vm.cancellation_token().is_cancelled() {
            runtime.vm.cancellation_token().cancel();
        }
        let current_steps = runtime.vm.steps;
        if current_steps > self.synced_steps {
            let delta = current_steps - self.synced_steps;
            parent.vm.steps = parent.vm.steps.saturating_add(delta);
            parent.vm.budget.steps = parent.vm.steps;
            self.synced_steps = current_steps;
            if parent.vm.steps > parent.vm.limits.step_limit && parent.vm.limits.step_limit > 0 {
                let reason = ExhaustionReason {
                    category: BudgetCategory::Steps,
                    used: parent.vm.steps,
                    limit: parent.vm.limits.step_limit,
                };
                parent.mark_budget_exhaustion(reason.clone());
                parent.vm.emit_diagnostic(
                    wasmsh_vm::DiagLevel::Error,
                    wasmsh_vm::DiagCategory::Budget,
                    reason.diagnostic_message(),
                );
                runtime.vm.cancellation_token().cancel();
            }
        }
    }

    fn drain_final_pipe(&mut self) -> bool {
        let mut progressed = false;
        loop {
            let mut buffer = [0u8; 4096];
            let read_result = {
                let mut pipe = self.final_pipe.borrow_mut();
                pipe.read(&mut buffer)
            };
            match read_result {
                ReadResult::Read(read) => {
                    self.captured_stdout.extend_from_slice(&buffer[..read]);
                    progressed = true;
                }
                ReadResult::WouldBlock | ReadResult::Eof => break,
            }
        }
        progressed
    }

    fn finalize_stderr(&mut self) {
        for (idx, stderr) in self.stage_stderr.iter().enumerate() {
            if self.stage_pipe_stderr[idx] {
                continue;
            }
            let data = stderr.borrow();
            if !data.is_empty() {
                self.captured_stderr.extend_from_slice(&data);
            }
        }
        if let Some(runtime) = self.isolated_runtime.as_mut() {
            self.captured_diagnostics
                .append(&mut runtime.vm.diagnostics);
        }
    }

    fn pump(&mut self, parent: Option<&mut WorkerRuntime>) -> bool {
        if self.done {
            return false;
        }
        let mut progressed = if self.isolated_runtime.is_some() {
            self.pump_isolated_with_parent(parent)
        } else {
            self.pump_without_runtime_pass()
        };
        if self.drain_final_pipe() {
            progressed = true;
        }
        if self.finished.iter().all(|done| *done) {
            self.finalize_stderr();
            self.done = true;
        }
        progressed
    }

    fn pump_isolated_with_parent(&mut self, parent: Option<&mut WorkerRuntime>) -> bool {
        let mut parent = parent;
        if let Some(parent_rt) = parent.as_deref_mut() {
            self.sync_isolated_runtime_with_parent(parent_rt);
        }
        let progressed = self.pump_isolated_pass();
        if let Some(parent_rt) = parent {
            self.sync_isolated_runtime_with_parent(parent_rt);
        }
        progressed
    }

    fn pump_isolated_pass(&mut self) -> bool {
        let runtime = self
            .isolated_runtime
            .as_mut()
            .expect("isolated process substitution runtime missing");
        let mut progressed = false;
        for idx in (0..self.processes.len()).rev() {
            if self.finished[idx] {
                continue;
            }
            let outcome = self.processes[idx].poll(runtime.as_mut());
            if apply_process_poll_outcome(&mut self.finished[idx], outcome) {
                progressed = true;
            }
        }
        progressed
    }

    fn pump_without_runtime_pass(&mut self) -> bool {
        let mut progressed = false;
        for idx in (0..self.processes.len()).rev() {
            if self.finished[idx] {
                continue;
            }
            let outcome = self.processes[idx].poll_without_runtime();
            if apply_process_poll_outcome(&mut self.finished[idx], outcome) {
                progressed = true;
            }
        }
        progressed
    }

    fn write_input(&mut self, data: &[u8]) {
        let mut offset = 0;
        while offset < data.len() && !self.done {
            let write_result = {
                let mut pipe = self.source_pipe.borrow_mut();
                pipe.write(&data[offset..])
            };
            match write_result {
                WriteResult::Written(written) | WriteResult::WouldBlock(written) if written > 0 => {
                    offset += written;
                    let _ = self.pump(None);
                }
                WriteResult::Written(_) | WriteResult::WouldBlock(_) => {
                    if !self.pump(None) {
                        break;
                    }
                }
                WriteResult::BrokenPipe => {
                    self.source_pipe.borrow_mut().close_write();
                    while self.pump(None) {}
                    break;
                }
            }
        }
    }

    fn write_input_with_parent(&mut self, parent: &mut WorkerRuntime, data: &[u8]) {
        let mut offset = 0;
        while offset < data.len() && !self.done {
            let write_result = {
                let mut pipe = self.source_pipe.borrow_mut();
                pipe.write(&data[offset..])
            };
            match write_result {
                WriteResult::Written(written) | WriteResult::WouldBlock(written) if written > 0 => {
                    offset += written;
                    let _ = self.pump(Some(parent));
                }
                WriteResult::Written(_) | WriteResult::WouldBlock(_) => {
                    if !self.pump(Some(parent)) {
                        break;
                    }
                }
                WriteResult::BrokenPipe => {
                    self.source_pipe.borrow_mut().close_write();
                    while self.pump(Some(parent)) {}
                    break;
                }
            }
        }
    }

    fn finish(&mut self) {
        if self.done {
            return;
        }
        self.source_pipe.borrow_mut().close_write();
        while self.pump(None) {}
        if !self.done {
            self.finalize_stderr();
            self.done = true;
        }
        let _ = self.drain_final_pipe();
    }

    fn finish_with_parent(&mut self, parent: &mut WorkerRuntime) {
        if self.done {
            return;
        }
        self.source_pipe.borrow_mut().close_write();
        while self.pump(Some(parent)) {}
        if !self.done {
            self.finalize_stderr();
            self.done = true;
        }
        self.sync_isolated_runtime_with_parent(parent);
        let _ = self.drain_final_pipe();
    }
}

enum PendingProcessSubstOutMode {
    Buffered { data: Vec<u8> },
    Live { runner: LiveProcessSubstRunner },
}

struct PendingProcessSubstOut {
    path: String,
    inner: String,
    mode: PendingProcessSubstOutMode,
}

impl std::fmt::Debug for PendingProcessSubstOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingProcessSubstOut")
            .field("path", &self.path)
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

struct PendingProcessSubstIn {
    path: String,
    stderr: Option<Rc<RefCell<Vec<u8>>>>,
    diagnostics: Option<Rc<RefCell<Vec<wasmsh_vm::DiagnosticEvent>>>>,
}

impl PendingProcessSubstOut {
    fn clear(&mut self) {
        match &mut self.mode {
            PendingProcessSubstOutMode::Buffered { data } => data.clear(),
            PendingProcessSubstOutMode::Live { .. } => {}
        }
    }

    fn write(&mut self, data: &[u8]) {
        match &mut self.mode {
            PendingProcessSubstOutMode::Buffered { data: buffered } => {
                buffered.extend_from_slice(data);
            }
            PendingProcessSubstOutMode::Live { runner } => runner.write_input(data),
        }
    }

    fn write_with_parent(&mut self, runtime: &mut WorkerRuntime, data: &[u8]) {
        match &mut self.mode {
            PendingProcessSubstOutMode::Buffered { data: buffered } => {
                buffered.extend_from_slice(data);
            }
            PendingProcessSubstOutMode::Live { runner } => {
                if runner.isolated_runtime.is_some() {
                    runner.write_input_with_parent(runtime, data);
                } else {
                    runner.write_input(data);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
enum BufferedPipelineCommand {
    Argv(Vec<String>),
    Hir(HirCommand),
}

enum StreamingPipeProcess<'a> {
    Read(PipeReadProcess<'a>),
    Head(HeadPipeProcess),
    Tee(TeePipeProcess<'a>),
    External(ExternalPipeProcess),
    Buffered(BufferedPipeProcess),
}

impl StreamingPipeProcess<'_> {
    fn poll(&mut self, runtime: &mut WorkerRuntime) -> PipeProcessPoll {
        match self {
            Self::Read(process) => process.poll(),
            Self::Head(process) => process.poll(),
            Self::Tee(process) => process.poll(),
            Self::External(process) => process.poll(),
            Self::Buffered(process) => process.poll(runtime),
        }
    }

    fn close(&mut self, runtime: &mut WorkerRuntime) {
        match self {
            Self::Tee(process) => process.close(),
            Self::External(process) => process.close(),
            Self::Buffered(process) => process.close(runtime),
            Self::Read(_) | Self::Head(_) => {}
        }
    }

    fn poll_without_runtime(&mut self) -> PipeProcessPoll {
        match self {
            Self::Read(process) => process.poll(),
            Self::Head(process) => process.poll(),
            Self::Tee(process) => process.poll(),
            Self::External(_) => unreachable!("streaming external process requires runtime access"),
            Self::Buffered(_) => {
                unreachable!("buffered pipeline stage requires runtime access")
            }
        }
    }

    fn close_without_runtime(&mut self) {
        match self {
            Self::Tee(process) => process.close(),
            Self::External(_) => unreachable!("streaming external process requires runtime access"),
            Self::Read(_) | Self::Head(_) => {}
            Self::Buffered(_) => {
                unreachable!("buffered pipeline stage requires runtime access")
            }
        }
    }
}

/// A non-blocking external process connected to the runtime pipe graph.
#[allow(clippy::struct_excessive_bools)]
struct ExternalPipeProcess {
    input: Option<Rc<RefCell<PipeBuffer>>>,
    output: Rc<RefCell<PipeBuffer>>,
    argv: Vec<String>,
    spec: ExternalCommandSpec,
    pipe_stderr: bool,
    process: Option<Box<dyn ExternalProcess>>,
    pending_stdin: Vec<u8>,
    stdin_offset: usize,
    stdin_closed: bool,
    stdin_writable: bool,
    stdin_bytes: u64,
    pending_stdout: Vec<u8>,
    stdout_offset: usize,
    pending_stderr: Vec<u8>,
    stderr_offset: usize,
    stdout_eof: bool,
    stderr_eof: bool,
    status: Option<i32>,
    output_bytes: u64,
    finished: bool,
    stage_stderr: Rc<RefCell<Vec<u8>>>,
    stage_status: Rc<RefCell<i32>>,
}

impl ExternalPipeProcess {
    fn start(
        runtime: &mut WorkerRuntime,
        input: Option<Rc<RefCell<PipeBuffer>>>,
        output: Rc<RefCell<PipeBuffer>>,
        argv: Vec<String>,
        spec: ExternalCommandSpec,
        pipe_stderr: bool,
        stage_stderr: Rc<RefCell<Vec<u8>>>,
        stage_status: Rc<RefCell<i32>>,
    ) -> Self {
        let process = runtime
            .external_stream_handler
            .as_mut()
            .ok_or_else(|| "streaming external executor is unavailable".to_string())
            .and_then(|handler| handler(&spec, &argv));
        match process {
            Ok(process) => Self {
                input,
                output,
                argv,
                spec,
                pipe_stderr,
                process: Some(process),
                pending_stdin: Vec::new(),
                stdin_offset: 0,
                stdin_closed: false,
                stdin_writable: true,
                stdin_bytes: 0,
                pending_stdout: Vec::new(),
                stdout_offset: 0,
                pending_stderr: Vec::new(),
                stderr_offset: 0,
                stdout_eof: false,
                stderr_eof: false,
                status: None,
                output_bytes: 0,
                finished: false,
                stage_stderr,
                stage_status,
            },
            Err(error) => Self::failed(
                input,
                output,
                argv,
                spec,
                pipe_stderr,
                stage_stderr,
                stage_status,
                126,
                &format!("external process start failed: {error}"),
            ),
        }
    }

    fn failed(
        input: Option<Rc<RefCell<PipeBuffer>>>,
        output: Rc<RefCell<PipeBuffer>>,
        argv: Vec<String>,
        spec: ExternalCommandSpec,
        pipe_stderr: bool,
        stage_stderr: Rc<RefCell<Vec<u8>>>,
        stage_status: Rc<RefCell<i32>>,
        status: i32,
        message: &str,
    ) -> Self {
        let mut process = Self {
            input,
            output,
            argv,
            spec,
            pipe_stderr,
            process: None,
            pending_stdin: Vec::new(),
            stdin_offset: 0,
            stdin_closed: true,
            stdin_writable: false,
            stdin_bytes: 0,
            pending_stdout: Vec::new(),
            stdout_offset: 0,
            pending_stderr: Vec::new(),
            stderr_offset: 0,
            stdout_eof: true,
            stderr_eof: true,
            status: Some(status),
            output_bytes: 0,
            finished: false,
            stage_stderr,
            stage_status,
        };
        let diagnostic = format!("wasmsh: {}: {message}\n", process.argv[0]);
        if process.pipe_stderr {
            process.pending_stderr = diagnostic.into_bytes();
        } else {
            process
                .stage_stderr
                .borrow_mut()
                .extend_from_slice(diagnostic.as_bytes());
        }
        *process.stage_status.borrow_mut() = status;
        process
    }

    fn command_name(&self) -> &str {
        self.argv.first().map_or("external", String::as_str)
    }

    fn close_input(&mut self) {
        if let Some(input) = &self.input {
            input.borrow_mut().close_read();
        }
    }

    fn cancel_process(&mut self, status: i32) {
        if let Some(process) = self.process.as_mut() {
            process.cancel();
        }
        self.close_input();
        self.pending_stdin.clear();
        self.stdin_offset = 0;
        self.stdin_closed = true;
        self.status.get_or_insert(status);
        *self.stage_status.borrow_mut() = self.status.unwrap_or(status);
        self.output.borrow_mut().close_write();
        self.process = None;
        self.finished = true;
    }

    fn fail(&mut self, status: i32, message: &str) -> PipeProcessPoll {
        if let Some(process) = self.process.as_mut() {
            process.cancel();
        }
        self.close_input();
        self.pending_stdin.clear();
        self.stdin_offset = 0;
        self.stdin_closed = true;
        self.status = Some(status);
        self.stdout_eof = true;
        self.stderr_eof = true;
        self.process = None;
        *self.stage_status.borrow_mut() = status;
        let diagnostic = format!("wasmsh: {}: {message}\n", self.command_name());
        if self.pipe_stderr {
            self.pending_stderr.extend_from_slice(diagnostic.as_bytes());
        } else {
            self.stage_stderr
                .borrow_mut()
                .extend_from_slice(diagnostic.as_bytes());
        }
        if self.flush_pending_output().is_some() {
            PipeProcessPoll::PendingWrite
        } else {
            self.output.borrow_mut().close_write();
            self.finished = true;
            PipeProcessPoll::Exited
        }
    }

    fn flush_pending_output(&mut self) -> Option<PipeProcessPoll> {
        if self.stdout_offset < self.pending_stdout.len() {
            let result = {
                let mut output = self.output.borrow_mut();
                output.write(&self.pending_stdout[self.stdout_offset..])
            };
            match result {
                WriteResult::Written(written) | WriteResult::WouldBlock(written) if written > 0 => {
                    self.stdout_offset += written;
                    if self.stdout_offset == self.pending_stdout.len() {
                        self.pending_stdout.clear();
                        self.stdout_offset = 0;
                    }
                    if self.stdout_offset < self.pending_stdout.len() {
                        return Some(PipeProcessPoll::PendingWrite);
                    }
                }
                WriteResult::Written(_) => {}
                WriteResult::WouldBlock(_) => return Some(PipeProcessPoll::PendingWrite),
                WriteResult::BrokenPipe => {
                    self.cancel_process(141);
                    return Some(PipeProcessPoll::Exited);
                }
            }
        }
        if self.stderr_offset < self.pending_stderr.len() {
            let result = {
                let mut output = self.output.borrow_mut();
                output.write(&self.pending_stderr[self.stderr_offset..])
            };
            match result {
                WriteResult::Written(written) | WriteResult::WouldBlock(written) if written > 0 => {
                    self.stderr_offset += written;
                    if self.stderr_offset == self.pending_stderr.len() {
                        self.pending_stderr.clear();
                        self.stderr_offset = 0;
                    }
                    if self.stderr_offset < self.pending_stderr.len() {
                        return Some(PipeProcessPoll::PendingWrite);
                    }
                }
                WriteResult::Written(_) => {}
                WriteResult::WouldBlock(_) => return Some(PipeProcessPoll::PendingWrite),
                WriteResult::BrokenPipe => {
                    self.cancel_process(141);
                    return Some(PipeProcessPoll::Exited);
                }
            }
        }
        None
    }

    fn fill_stdin(&mut self) -> PipeProcessPoll {
        if self.stdin_closed || !self.stdin_writable || !self.pending_stdin.is_empty() {
            return PipeProcessPoll::Ready;
        }
        let Some(input) = &self.input else {
            self.stdin_closed = true;
            if let Some(process) = self.process.as_mut() {
                process.close_stdin();
            }
            return PipeProcessPoll::Ready;
        };
        let mut buffer = vec![0u8; self.spec.options.stream_chunk_bytes as usize];
        let read_result = {
            let mut input = input.borrow_mut();
            input.read(&mut buffer)
        };
        match read_result {
            ReadResult::Read(read) => {
                buffer.truncate(read);
                self.pending_stdin = buffer;
                self.stdin_offset = 0;
                PipeProcessPoll::Ready
            }
            ReadResult::WouldBlock => PipeProcessPoll::PendingRead,
            ReadResult::Eof => {
                input.borrow_mut().close_read();
                self.stdin_closed = true;
                if let Some(process) = self.process.as_mut() {
                    process.close_stdin();
                }
                PipeProcessPoll::Ready
            }
        }
    }

    fn write_stdin_chunk(&mut self) -> PipeProcessPoll {
        if self.stdin_closed || self.pending_stdin.is_empty() || !self.stdin_writable {
            return PipeProcessPoll::Ready;
        }
        let Some(process) = self.process.as_mut() else {
            return PipeProcessPoll::Exited;
        };
        let result = process.write_stdin(&self.pending_stdin[self.stdin_offset..]);
        if result.accepted > self.pending_stdin.len().saturating_sub(self.stdin_offset) {
            return self.fail(126, "external host accepted more stdin than supplied");
        }
        self.stdin_offset += result.accepted;
        if self.stdin_offset == self.pending_stdin.len() {
            self.pending_stdin.clear();
            self.stdin_offset = 0;
        }
        if result.closed {
            return self.fail(141, "external stdin closed before all input was written");
        }
        self.stdin_bytes = self.stdin_bytes.saturating_add(result.accepted as u64);
        let limit = self.spec.options.max_input_bytes;
        if limit != 0 && self.stdin_bytes > limit {
            return self.fail(
                125,
                &format!("external stdin limit exceeded (limit {limit} bytes)"),
            );
        }
        self.stdin_writable = !result.would_block;
        if result.would_block || result.accepted == 0 {
            PipeProcessPoll::PendingWrite
        } else {
            PipeProcessPoll::Ready
        }
    }

    fn append_poll_output(&mut self, poll: ExternalProcessPoll) -> Result<(), (i32, String)> {
        let incoming = poll.stdout.len() as u64 + poll.stderr.len() as u64;
        let next = self.output_bytes.saturating_add(incoming);
        if next > self.spec.options.max_output_bytes {
            return Err((
                125,
                format!(
                    "external output limit exceeded (limit {} bytes)",
                    self.spec.options.max_output_bytes
                ),
            ));
        }
        if poll.stdout.len() as u64 > self.spec.options.stream_chunk_bytes
            || poll.stderr.len() as u64 > self.spec.options.stream_chunk_bytes
        {
            return Err((
                126,
                "external host returned a chunk larger than stream_chunk_bytes".into(),
            ));
        }
        self.output_bytes = next;
        self.pending_stdout.extend_from_slice(&poll.stdout);
        if self.pipe_stderr {
            self.pending_stderr.extend_from_slice(&poll.stderr);
        } else {
            self.stage_stderr
                .borrow_mut()
                .extend_from_slice(&poll.stderr);
        }
        self.stdout_eof |= poll.stdout_eof;
        self.stderr_eof |= poll.stderr_eof;
        if let Some(status) = poll.status {
            self.status = Some(status);
            *self.stage_status.borrow_mut() = status;
        }
        self.stdin_writable = poll.stdin_writable;
        if let Some(error) = poll.error {
            return Err((self.status.unwrap_or(126), error));
        }
        Ok(())
    }

    fn can_finish(&self) -> bool {
        self.stdout_eof
            && self.stderr_eof
            && self.status.is_some()
            && self.pending_stdout.is_empty()
            && self.pending_stderr.is_empty()
    }

    fn poll(&mut self) -> PipeProcessPoll {
        if self.finished {
            return PipeProcessPoll::Exited;
        }
        if self.output.borrow().is_read_closed() {
            self.cancel_process(141);
            return PipeProcessPoll::Exited;
        }
        if let Some(poll) = self.flush_pending_output() {
            return poll;
        }
        if self.process.is_none() {
            self.output.borrow_mut().close_write();
            self.finished = true;
            return PipeProcessPoll::Exited;
        }

        let fill = self.fill_stdin();
        let write = self.write_stdin_chunk();
        let mut progressed =
            matches!(fill, PipeProcessPoll::Ready) || matches!(write, PipeProcessPoll::Ready);
        if matches!(write, PipeProcessPoll::Exited) {
            return PipeProcessPoll::Exited;
        }

        let Some(process) = self.process.as_mut() else {
            // `write_stdin_chunk` may have failed the process while leaving
            // pending output to flush; treat that as a clean exit.
            self.output.borrow_mut().close_write();
            self.finished = true;
            return PipeProcessPoll::Exited;
        };
        let poll = process.poll();
        let has_data = !poll.stdout.is_empty() || !poll.stderr.is_empty();
        if has_data {
            progressed = true;
        }
        if let Err((status, error)) = self.append_poll_output(poll) {
            return self.fail(status, &error);
        }
        if let Some(flush) = self.flush_pending_output() {
            return flush;
        }
        if self.can_finish() {
            self.output.borrow_mut().close_write();
            self.finished = true;
            return PipeProcessPoll::Exited;
        }
        if progressed {
            PipeProcessPoll::Ready
        } else if !self.stdin_writable {
            PipeProcessPoll::PendingWrite
        } else {
            PipeProcessPoll::PendingRead
        }
    }

    fn close(&mut self) {
        if let Some(process) = self.process.as_mut() {
            process.cancel();
        }
        self.close_input();
        self.output.borrow_mut().close_write();
        self.process = None;
        self.finished = true;
    }
}

#[allow(clippy::struct_excessive_bools)]
struct PendingStreamingPipeline {
    processes: Vec<StreamingPipeProcess<'static>>,
    finished: Vec<bool>,
    output_pipes: Vec<Rc<RefCell<PipeBuffer>>>,
    final_pipe: Rc<RefCell<PipeBuffer>>,
    stage_statuses: Vec<Rc<RefCell<i32>>>,
    stage_stderr: Vec<Rc<RefCell<Vec<u8>>>>,
    stage_pipe_stderr: Vec<bool>,
    stage_stderr_offsets: Vec<usize>,
    pipefail: bool,
    negated: bool,
    timed: bool,
    time_posix: bool,
    started_ms: u64,
    /// Wall-clock deadline (monotonic ms) for the whole pipeline, derived
    /// from the shortest external `timeout_ms`. Zero means no runtime limit.
    deadline_ms: u64,
    last_arg: Option<String>,
}

struct BufferedPipeProcess {
    input: Option<Rc<RefCell<PipeBuffer>>>,
    output: Rc<RefCell<PipeBuffer>>,
    command: BufferedPipelineCommand,
    pipe_stderr: bool,
    pending_stdout: Vec<u8>,
    pending_offset: usize,
    finished: bool,
    command_ran: bool,
    stage_stderr: Rc<RefCell<Vec<u8>>>,
    stage_status: Rc<RefCell<i32>>,
    staging_path: Option<String>,
    staging_handle: Option<FileHandle>,
    staged_input_bytes: u64,
}

impl BufferedPipeProcess {
    fn new(
        input: Option<Rc<RefCell<PipeBuffer>>>,
        output: Rc<RefCell<PipeBuffer>>,
        command: BufferedPipelineCommand,
        pipe_stderr: bool,
        stage_stderr: Rc<RefCell<Vec<u8>>>,
        stage_status: Rc<RefCell<i32>>,
    ) -> Self {
        Self {
            input,
            output,
            command,
            pipe_stderr,
            pending_stdout: Vec::new(),
            pending_offset: 0,
            finished: false,
            command_ran: false,
            stage_stderr,
            stage_status,
            staging_path: None,
            staging_handle: None,
            staged_input_bytes: 0,
        }
    }

    fn command_label(&self) -> String {
        match &self.command {
            BufferedPipelineCommand::Argv(argv) => argv
                .first()
                .cloned()
                .unwrap_or_else(|| "command".to_string()),
            BufferedPipelineCommand::Hir(cmd) => Self::hir_command_label(cmd).to_string(),
        }
    }

    fn hir_command_label(cmd: &HirCommand) -> &'static str {
        match cmd {
            HirCommand::Exec(_) => "exec",
            HirCommand::Assign(_) => "assign",
            HirCommand::RedirectOnly(_) => "redirect",
            HirCommand::If(_) => "if",
            HirCommand::While(_) => "while",
            HirCommand::Until(_) => "until",
            HirCommand::For(_) => "for",
            HirCommand::Subshell(_) => "subshell",
            HirCommand::Group(_) => "group",
            HirCommand::FunctionDef(_) => "function",
            HirCommand::Case(_) => "case",
            HirCommand::DoubleBracket(_) => "[[",
            HirCommand::ArithFor(_) => "arith-for",
            HirCommand::ArithCommand(_) => "arith",
            HirCommand::Select(_) => "select",
            _ => "command",
        }
    }

    fn ensure_staging_handle(
        &mut self,
        runtime: &mut WorkerRuntime,
    ) -> Result<(String, FileHandle), String> {
        if let (Some(path), Some(handle)) = (&self.staging_path, self.staging_handle) {
            return Ok((path.clone(), handle));
        }
        let path = format!(
            "/tmp/_wasmsh_pipe_{}",
            WorkerRuntime::next_pending_input_id()
        );
        let create_handle = runtime
            .fs
            .open(&path, OpenOptions::write())
            .map_err(|err| err.to_string())?;
        runtime.fs.close(create_handle);
        let handle = runtime
            .fs
            .open(&path, OpenOptions::append())
            .map_err(|err| err.to_string())?;
        self.staging_path = Some(path.clone());
        self.staging_handle = Some(handle);
        Ok((path, handle))
    }

    fn emit_error(
        &mut self,
        runtime: &mut WorkerRuntime,
        cmd_name: &str,
        err: &str,
    ) -> PipeProcessPoll {
        *self.stage_status.borrow_mut() = 1;
        self.stage_stderr.borrow_mut().extend_from_slice(
            format!("wasmsh: {cmd_name}: failed to stage pipeline input for streaming: {err}\n")
                .as_bytes(),
        );
        self.output.borrow_mut().close_write();
        self.close(runtime);
        self.finished = true;
        PipeProcessPoll::Exited
    }

    fn emit_input_limit_error(
        &mut self,
        runtime: &mut WorkerRuntime,
        cmd_name: &str,
        limit: u64,
    ) -> PipeProcessPoll {
        *self.stage_status.borrow_mut() = 125;
        self.stage_stderr.borrow_mut().extend_from_slice(
            format!("wasmsh: {cmd_name}: external stdin limit exceeded (limit {limit} bytes)\n")
                .as_bytes(),
        );
        self.output.borrow_mut().close_write();
        self.close(runtime);
        self.finished = true;
        PipeProcessPoll::Exited
    }

    fn input_limit(&self, runtime: &WorkerRuntime) -> Option<u64> {
        let BufferedPipelineCommand::Argv(argv) = &self.command else {
            return None;
        };
        let cmd_name = argv.first()?;
        if let Some(spec) = runtime.external_specs.get(cmd_name) {
            return Some(spec.options.max_input_bytes);
        }
        if matches!(
            runtime.resolve_command(cmd_name, argv),
            ResolvedCommand::External
        ) {
            Some(runtime.config.external_input_byte_limit)
        } else {
            None
        }
    }

    fn run_command(&mut self, runtime: &mut WorkerRuntime) -> PipeProcessPoll {
        if let Some(handle) = self.staging_handle.take() {
            runtime.fs.close(handle);
        }
        let saved_exec_io = runtime.current_exec_io.take();
        if let Some(path) = self.staging_path.take() {
            runtime.set_pending_input_file(path, true);
        }
        let ((), captured) =
            runtime.with_output_capture(true, self.pipe_stderr, |runtime| match &self.command {
                BufferedPipelineCommand::Argv(argv) => runtime.execute_argv_command(argv),
                BufferedPipelineCommand::Hir(cmd) => runtime.execute_command(cmd),
            });
        *self.stage_status.borrow_mut() = runtime.vm.state.last_status;
        if self.pipe_stderr {
            self.pending_stdout = captured.stdout;
            self.pending_stdout.extend_from_slice(&captured.stderr);
        } else {
            self.pending_stdout = captured.stdout;
            self.stage_stderr
                .borrow_mut()
                .extend_from_slice(&captured.stderr);
        }
        runtime.clear_pending_input();
        runtime.current_exec_io = saved_exec_io;
        self.pending_offset = 0;
        self.command_ran = true;
        if self.pending_stdout.is_empty() {
            self.output.borrow_mut().close_write();
            self.finished = true;
            PipeProcessPoll::Exited
        } else {
            PipeProcessPoll::Ready
        }
    }

    fn close(&mut self, runtime: &mut WorkerRuntime) {
        if let Some(handle) = self.staging_handle.take() {
            runtime.fs.close(handle);
        }
        if let Some(path) = self.staging_path.take() {
            let _ = runtime.fs.remove_file(&path);
        }
    }

    fn poll(&mut self, runtime: &mut WorkerRuntime) -> PipeProcessPoll {
        if self.finished {
            return PipeProcessPoll::Exited;
        }
        if self.pending_offset < self.pending_stdout.len() {
            return self.buffered_drain_pending();
        }
        if self.command_ran {
            self.output.borrow_mut().close_write();
            self.finished = true;
            return PipeProcessPoll::Exited;
        }
        self.buffered_pump_input(runtime)
    }

    fn buffered_drain_pending(&mut self) -> PipeProcessPoll {
        let write_result = {
            let mut pipe = self.output.borrow_mut();
            pipe.write(&self.pending_stdout[self.pending_offset..])
        };
        match write_result {
            WriteResult::Written(written) => {
                self.pending_offset += written;
                if self.pending_offset == self.pending_stdout.len() {
                    self.pending_stdout.clear();
                    self.pending_offset = 0;
                    if self.command_ran {
                        self.output.borrow_mut().close_write();
                        self.finished = true;
                        return PipeProcessPoll::Exited;
                    }
                }
                PipeProcessPoll::Ready
            }
            WriteResult::WouldBlock(0) => PipeProcessPoll::PendingWrite,
            WriteResult::WouldBlock(written) => {
                self.pending_offset += written;
                PipeProcessPoll::Ready
            }
            WriteResult::BrokenPipe => {
                self.output.borrow_mut().close_write();
                self.finished = true;
                PipeProcessPoll::Exited
            }
        }
    }

    fn buffered_pump_input(&mut self, runtime: &mut WorkerRuntime) -> PipeProcessPoll {
        let Some(input) = &self.input else {
            return self.run_command(runtime);
        };
        let cmd_name = self.command_label();
        let mut scratch = [0u8; 4096];
        let read_result = {
            let mut input = input.borrow_mut();
            input.read(&mut scratch)
        };
        match read_result {
            ReadResult::Read(read) => {
                if let Some(limit) = self.input_limit(runtime) {
                    let next_size = self.staged_input_bytes + read as u64;
                    if next_size > limit {
                        return self.emit_input_limit_error(runtime, &cmd_name, limit);
                    }
                    self.staged_input_bytes = next_size;
                }
                let (_, handle) = match self.ensure_staging_handle(runtime) {
                    Ok(parts) => parts,
                    Err(err) => return self.emit_error(runtime, &cmd_name, &err),
                };
                if let Err(err) = runtime.fs.write_file(handle, &scratch[..read]) {
                    return self.emit_error(runtime, &cmd_name, &err.to_string());
                }
                PipeProcessPoll::Ready
            }
            ReadResult::WouldBlock => PipeProcessPoll::PendingRead,
            ReadResult::Eof => {
                input.borrow_mut().close_read();
                self.run_command(runtime)
            }
        }
    }
}

struct HeadPipeProcess {
    input: Rc<RefCell<PipeBuffer>>,
    output: Rc<RefCell<PipeBuffer>>,
    mode: StreamingHeadMode,
    pending: Vec<u8>,
    pending_offset: usize,
    lines_seen: usize,
    input_closed: bool,
    stream_complete: bool,
    finished: bool,
}

impl HeadPipeProcess {
    fn new(
        input: Rc<RefCell<PipeBuffer>>,
        output: Rc<RefCell<PipeBuffer>>,
        mode: StreamingHeadMode,
    ) -> Self {
        Self {
            input,
            output,
            mode,
            pending: Vec::new(),
            pending_offset: 0,
            lines_seen: 0,
            input_closed: false,
            stream_complete: false,
            finished: false,
        }
    }

    fn close_input(&mut self) {
        if !self.input_closed {
            self.input.borrow_mut().close_read();
            self.input_closed = true;
        }
    }

    fn finish(&mut self) -> PipeProcessPoll {
        self.close_input();
        self.output.borrow_mut().close_write();
        self.finished = true;
        PipeProcessPoll::Exited
    }

    fn try_flush_pending(&mut self) -> Option<PipeProcessPoll> {
        if self.pending_offset >= self.pending.len() {
            return None;
        }
        let write_result = {
            let mut pipe = self.output.borrow_mut();
            pipe.write(&self.pending[self.pending_offset..])
        };
        match write_result {
            WriteResult::Written(written) => {
                self.pending_offset += written;
                if self.pending_offset == self.pending.len() {
                    self.pending.clear();
                    self.pending_offset = 0;
                    if self.stream_complete {
                        return Some(self.finish());
                    }
                }
                Some(PipeProcessPoll::Ready)
            }
            WriteResult::WouldBlock(0) => Some(PipeProcessPoll::PendingWrite),
            WriteResult::WouldBlock(written) => {
                self.pending_offset += written;
                Some(PipeProcessPoll::Ready)
            }
            WriteResult::BrokenPipe => Some(self.finish()),
        }
    }

    fn update_head_limit(&mut self, byte: u8, read: usize) {
        match &mut self.mode {
            StreamingHeadMode::Bytes(remaining) => {
                *remaining = remaining.saturating_sub(read);
                if *remaining == 0 {
                    self.stream_complete = true;
                    self.close_input();
                }
            }
            StreamingHeadMode::Lines(limit) => {
                if byte == b'\n' {
                    self.lines_seen += 1;
                    if self.lines_seen >= *limit {
                        self.stream_complete = true;
                        self.close_input();
                    }
                }
            }
        }
    }

    fn poll(&mut self) -> PipeProcessPoll {
        if self.finished {
            return PipeProcessPoll::Exited;
        }
        loop {
            if let Some(result) = self.try_flush_pending() {
                return result;
            }
            if self.stream_complete {
                return self.finish();
            }

            let mut one = [0u8; 1];
            let read_result = {
                let mut input = self.input.borrow_mut();
                input.read(&mut one)
            };
            match read_result {
                ReadResult::Read(read) => {
                    self.pending.extend_from_slice(&one[..read]);
                    self.update_head_limit(one[0], read);
                }
                ReadResult::WouldBlock => return PipeProcessPoll::PendingRead,
                ReadResult::Eof => {
                    self.stream_complete = true;
                    self.close_input();
                }
            }
        }
    }
}

struct PipeReadProcess<'a> {
    reader: Option<Box<dyn Read + 'a>>,
    output: Rc<RefCell<PipeBuffer>>,
    pending: Vec<u8>,
    pending_offset: usize,
    stderr_offset: usize,
    finished: bool,
    stderr: Rc<RefCell<Vec<u8>>>,
    status: Rc<RefCell<i32>>,
    label: &'static str,
    pipe_stderr: bool,
    reader_done: bool,
}

impl<'a> PipeReadProcess<'a> {
    fn new(
        reader: Box<dyn Read + 'a>,
        output: Rc<RefCell<PipeBuffer>>,
        stderr: Rc<RefCell<Vec<u8>>>,
        status: Rc<RefCell<i32>>,
        label: &'static str,
        pipe_stderr: bool,
    ) -> Self {
        Self {
            reader: Some(reader),
            output,
            pending: Vec::new(),
            pending_offset: 0,
            stderr_offset: 0,
            finished: false,
            stderr,
            status,
            label,
            pipe_stderr,
            reader_done: false,
        }
    }

    fn finish(&mut self) -> PipeProcessPoll {
        self.output.borrow_mut().close_write();
        self.reader = None;
        self.finished = true;
        PipeProcessPoll::Exited
    }

    fn poll_stderr(&mut self) -> Option<PipeProcessPoll> {
        if !self.pipe_stderr {
            return None;
        }
        let len = self.stderr.borrow().len();
        if self.stderr_offset >= len {
            return None;
        }
        let chunk = {
            let stderr = self.stderr.borrow();
            stderr[self.stderr_offset..].to_vec()
        };
        let write_result = {
            let mut output = self.output.borrow_mut();
            output.write(&chunk)
        };
        match write_result {
            WriteResult::Written(written) | WriteResult::WouldBlock(written) if written > 0 => {
                self.stderr_offset += written;
                Some(PipeProcessPoll::Ready)
            }
            WriteResult::Written(_) | WriteResult::WouldBlock(_) => {
                Some(PipeProcessPoll::PendingWrite)
            }
            WriteResult::BrokenPipe => Some(self.finish()),
        }
    }

    fn poll(&mut self) -> PipeProcessPoll {
        if self.finished {
            return PipeProcessPoll::Exited;
        }
        loop {
            if let Some(poll) = self.read_drain_pending() {
                return poll;
            }
            if let Some(poll) = self.poll_stderr() {
                return poll;
            }
            if self.reader_done {
                return self.finish();
            }
            if let Some(poll) = self.read_fill_from_reader() {
                return poll;
            }
        }
    }

    fn read_drain_pending(&mut self) -> Option<PipeProcessPoll> {
        if self.pending_offset >= self.pending.len() {
            return None;
        }
        let write_result = {
            let mut pipe = self.output.borrow_mut();
            pipe.write(&self.pending[self.pending_offset..])
        };
        Some(match write_result {
            WriteResult::Written(written) => {
                self.pending_offset += written;
                if self.pending_offset == self.pending.len() {
                    self.pending.clear();
                    self.pending_offset = 0;
                }
                PipeProcessPoll::Ready
            }
            WriteResult::WouldBlock(0) => PipeProcessPoll::PendingWrite,
            WriteResult::WouldBlock(written) => {
                self.pending_offset += written;
                PipeProcessPoll::Ready
            }
            WriteResult::BrokenPipe => self.finish(),
        })
    }

    fn read_fill_from_reader(&mut self) -> Option<PipeProcessPoll> {
        let mut buffer = [0u8; 4096];
        let reader = self
            .reader
            .as_mut()
            .expect("pipe read process polled after reader finished");
        match reader.read(&mut buffer) {
            Ok(0) => {
                self.reader_done = true;
                None
            }
            Ok(read) => {
                self.pending.extend_from_slice(&buffer[..read]);
                None
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => Some(PipeProcessPoll::PendingRead),
            Err(err) => {
                *self.status.borrow_mut() = 1;
                self.stderr.borrow_mut().extend_from_slice(
                    format!(
                        "wasmsh: {}: streaming pipeline read error: {err}\n",
                        self.label
                    )
                    .as_bytes(),
                );
                self.reader_done = true;
                None
            }
        }
    }
}

struct TeePipeProcess<'a> {
    reader: Option<Box<dyn Read + 'a>>,
    output: Rc<RefCell<PipeBuffer>>,
    pending: Vec<u8>,
    pending_offset: usize,
    stderr_offset: usize,
    finished: bool,
    stderr: Rc<RefCell<Vec<u8>>>,
    status: Rc<RefCell<i32>>,
    targets: Vec<TeeTarget>,
    pipe_stderr: bool,
    reader_done: bool,
}

impl<'a> TeePipeProcess<'a> {
    fn new(
        reader: Box<dyn Read + 'a>,
        output: Rc<RefCell<PipeBuffer>>,
        fs: &mut BackendFs,
        cwd: &str,
        stage: &StreamingTeeStage,
        stderr: Rc<RefCell<Vec<u8>>>,
        status: Rc<RefCell<i32>>,
        pipe_stderr: bool,
    ) -> Self {
        let mut targets = Vec::new();
        for path in &stage.paths {
            let resolved = resolve_path_from_cwd(cwd, path);
            match fs.open_write_sink(&resolved, stage.append) {
                Ok(sink) => targets.push(TeeTarget {
                    display_path: path.clone(),
                    sink,
                }),
                Err(err) => {
                    stderr
                        .borrow_mut()
                        .extend_from_slice(format!("tee: {path}: {err}\n").as_bytes());
                    *status.borrow_mut() = 1;
                }
            }
        }
        Self {
            reader: Some(reader),
            output,
            pending: Vec::new(),
            pending_offset: 0,
            stderr_offset: 0,
            finished: false,
            stderr,
            status,
            targets,
            pipe_stderr,
            reader_done: false,
        }
    }

    fn close(&mut self) {
        self.reader = None;
        self.targets.clear();
    }

    fn finish(&mut self) -> PipeProcessPoll {
        self.output.borrow_mut().close_write();
        self.close();
        self.finished = true;
        PipeProcessPoll::Exited
    }

    fn write_targets(&mut self, chunk: &[u8]) {
        for target in &mut self.targets {
            if let Err(err) = target.sink.write(chunk) {
                self.stderr
                    .borrow_mut()
                    .extend_from_slice(format!("tee: {}: {err}\n", target.display_path).as_bytes());
                *self.status.borrow_mut() = 1;
            }
        }
    }

    fn poll(&mut self) -> PipeProcessPoll {
        if self.finished {
            return PipeProcessPoll::Exited;
        }
        loop {
            if let Some(poll) = self.tee_drain_pending() {
                return poll;
            }
            if let Some(poll) = self.tee_drain_stderr() {
                return poll;
            }
            if self.reader_done {
                return self.finish();
            }
            if let Some(poll) = self.tee_fill_from_reader() {
                return poll;
            }
        }
    }

    fn tee_drain_pending(&mut self) -> Option<PipeProcessPoll> {
        if self.pending_offset >= self.pending.len() {
            return None;
        }
        let write_result = {
            let mut pipe = self.output.borrow_mut();
            pipe.write(&self.pending[self.pending_offset..])
        };
        Some(match write_result {
            WriteResult::Written(written) => {
                let end = self.pending_offset + written;
                let chunk = self.pending[self.pending_offset..end].to_vec();
                self.write_targets(&chunk);
                self.pending_offset += written;
                if self.pending_offset == self.pending.len() {
                    self.pending.clear();
                    self.pending_offset = 0;
                }
                PipeProcessPoll::Ready
            }
            WriteResult::WouldBlock(0) => PipeProcessPoll::PendingWrite,
            WriteResult::WouldBlock(written) => {
                let end = self.pending_offset + written;
                let chunk = self.pending[self.pending_offset..end].to_vec();
                self.write_targets(&chunk);
                self.pending_offset += written;
                PipeProcessPoll::Ready
            }
            WriteResult::BrokenPipe => self.finish(),
        })
    }

    fn tee_drain_stderr(&mut self) -> Option<PipeProcessPoll> {
        if !self.pipe_stderr {
            return None;
        }
        let len = self.stderr.borrow().len();
        if self.stderr_offset >= len {
            return None;
        }
        let chunk = {
            let stderr = self.stderr.borrow();
            stderr[self.stderr_offset..].to_vec()
        };
        let write_result = {
            let mut output = self.output.borrow_mut();
            output.write(&chunk)
        };
        Some(match write_result {
            WriteResult::Written(written) | WriteResult::WouldBlock(written) if written > 0 => {
                self.stderr_offset += written;
                PipeProcessPoll::Ready
            }
            WriteResult::Written(_) | WriteResult::WouldBlock(_) => PipeProcessPoll::PendingWrite,
            WriteResult::BrokenPipe => self.finish(),
        })
    }

    fn tee_fill_from_reader(&mut self) -> Option<PipeProcessPoll> {
        let mut buffer = [0u8; 4096];
        let reader = self
            .reader
            .as_mut()
            .expect("tee pipe process polled after reader finished");
        match reader.read(&mut buffer) {
            Ok(0) => {
                self.reader_done = true;
                None
            }
            Ok(read) => {
                self.pending.extend_from_slice(&buffer[..read]);
                None
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => Some(PipeProcessPoll::PendingRead),
            Err(err) => {
                *self.status.borrow_mut() = 1;
                self.stderr.borrow_mut().extend_from_slice(
                    format!("wasmsh: tee: streaming pipeline read error: {err}\n").as_bytes(),
                );
                self.reader_done = true;
                None
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum StreamingHeadMode {
    Lines(usize),
    Bytes(usize),
}

#[derive(Clone, Copy, Debug)]
enum StreamingTailMode {
    Lines(usize),
    Bytes(usize),
}

struct YesStreamReader {
    line: Vec<u8>,
    offset: usize,
    remaining_lines: usize,
}

impl YesStreamReader {
    fn new(line: Vec<u8>, remaining_lines: usize) -> Self {
        Self {
            line,
            offset: 0,
            remaining_lines,
        }
    }
}

impl Read for YesStreamReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() || self.line.is_empty() || self.remaining_lines == 0 {
            return Ok(0);
        }
        let mut written = 0usize;
        while written < buf.len() && self.remaining_lines > 0 {
            let remaining_line = &self.line[self.offset..];
            let to_copy = remaining_line.len().min(buf.len() - written);
            buf[written..written + to_copy].copy_from_slice(&remaining_line[..to_copy]);
            written += to_copy;
            self.offset += to_copy;
            if self.offset == self.line.len() {
                self.offset = 0;
                self.remaining_lines = self.remaining_lines.saturating_sub(1);
            }
        }
        Ok(written)
    }
}

struct HeadStreamReader<R> {
    inner: R,
    mode: StreamingHeadMode,
    finished: bool,
    pending: Vec<u8>,
    pending_offset: usize,
    lines_seen: usize,
}

struct TailStreamReader<R> {
    inner: R,
    mode: StreamingTailMode,
    output_pending: Vec<u8>,
    output_offset: usize,
    finalized: bool,
    byte_ring: VecDeque<u8>,
    line_ring: VecDeque<Vec<u8>>,
    current_line: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
struct StreamingBatStage {
    show_numbers: bool,
    show_header: bool,
    line_range: Option<(Option<usize>, Option<usize>)>,
    show_all: bool,
}

struct BatStreamReader<R> {
    inner: R,
    stage: StreamingBatStage,
    input_pending: Vec<u8>,
    output_pending: Vec<u8>,
    output_offset: usize,
    finished: bool,
    header_emitted: bool,
    footer_emitted: bool,
    line_num: usize,
}

#[derive(Clone, Debug)]
struct StreamingPasteStage {
    delimiter: String,
    serial: bool,
}

struct PasteStreamReader<R> {
    inner: R,
    stage: StreamingPasteStage,
    input_pending: Vec<u8>,
    output_pending: Vec<u8>,
    output_offset: usize,
    finalized: bool,
    ended_with_newline: bool,
    serial_first: bool,
}

#[derive(Clone, Copy, Debug)]
struct StreamingColumnStage;

struct ColumnStreamReader<R> {
    inner: R,
    output_pending: Vec<u8>,
    output_offset: usize,
    finalized: bool,
    ended_with_newline: bool,
}

#[derive(Clone, Debug)]
struct StreamingTeeStage {
    append: bool,
    paths: Vec<String>,
}

struct TeeTarget {
    display_path: String,
    sink: Box<dyn VfsWriteSink>,
}

#[derive(Clone, Copy, Debug)]
#[allow(clippy::struct_excessive_bools)]
struct StreamingWcFlags {
    lines: bool,
    words: bool,
    bytes: bool,
    max_line_length: bool,
}

#[allow(clippy::struct_excessive_bools)]
struct WcStreamReader<R> {
    inner: R,
    flags: StreamingWcFlags,
    summary: Vec<u8>,
    summary_offset: usize,
    finalized: bool,
    lines: usize,
    words: usize,
    bytes: usize,
    max_line_length: usize,
    current_line_length: usize,
    in_word: bool,
    saw_input: bool,
    ended_with_newline: bool,
}

#[derive(Copy, Clone, Debug)]
enum StreamingSedStep {
    Advance(usize),
    Break,
}

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)]
struct TypeFlags {
    all: bool,
    skip_functions: bool,
    path_only: bool,
    force_path: bool,
    type_only: bool,
}

impl<R> WcStreamReader<R> {
    fn new(inner: R, flags: StreamingWcFlags) -> Self {
        Self {
            inner,
            flags,
            summary: Vec::new(),
            summary_offset: 0,
            finalized: false,
            lines: 0,
            words: 0,
            bytes: 0,
            max_line_length: 0,
            current_line_length: 0,
            in_word: false,
            saw_input: false,
            ended_with_newline: false,
        }
    }

    fn take_summary(&mut self, buf: &mut [u8]) -> usize {
        if self.summary_offset >= self.summary.len() {
            return 0;
        }
        let remaining = &self.summary[self.summary_offset..];
        let to_copy = remaining.len().min(buf.len());
        buf[..to_copy].copy_from_slice(&remaining[..to_copy]);
        self.summary_offset += to_copy;
        to_copy
    }

    fn process_chunk(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.saw_input = true;
        self.bytes += chunk.len();
        for &byte in chunk {
            let is_whitespace = byte.is_ascii_whitespace();
            if is_whitespace {
                self.in_word = false;
            } else if !self.in_word {
                self.words += 1;
                self.in_word = true;
            }

            if byte == b'\n' {
                self.lines += 1;
                self.max_line_length = self.max_line_length.max(self.current_line_length);
                self.current_line_length = 0;
                self.ended_with_newline = true;
            } else {
                self.current_line_length += 1;
                self.ended_with_newline = false;
            }
        }
    }

    fn finalize_summary(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        // `wc -l` counts newlines; an unterminated final line is not a line.
        if self.saw_input && !self.ended_with_newline {
            self.max_line_length = self.max_line_length.max(self.current_line_length);
        }

        let columns = usize::from(self.flags.lines)
            + usize::from(self.flags.words)
            + usize::from(self.flags.bytes)
            + usize::from(self.flags.max_line_length);
        // A pipe's size is unknown, so GNU wc pads a multi-column result to a
        // fixed seven-character field.
        let width = if columns > 1 {
            let mut digits = 1usize;
            let mut consider = |v: usize| {
                let mut value = v;
                let mut n = 1usize;
                while value >= 10 {
                    value /= 10;
                    n += 1;
                }
                digits = digits.max(n);
            };
            if self.flags.lines {
                consider(self.lines);
            }
            if self.flags.words {
                consider(self.words);
            }
            if self.flags.bytes {
                consider(self.bytes);
            }
            if self.flags.max_line_length {
                consider(self.max_line_length);
            }
            digits.max(7)
        } else {
            1
        };

        let fmt = |n: usize| format!("{n:>width$}");
        let mut parts = Vec::new();
        if self.flags.lines {
            parts.push(fmt(self.lines));
        }
        if self.flags.words {
            parts.push(fmt(self.words));
        }
        if self.flags.bytes {
            parts.push(fmt(self.bytes));
        }
        if self.flags.max_line_length {
            parts.push(fmt(self.max_line_length));
        }
        let mut output = parts.join(" ");
        output.push('\n');
        self.summary = output.into_bytes();
    }
}

impl<R: Read> Read for WcStreamReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let copied = self.take_summary(buf);
        if copied > 0 {
            return Ok(copied);
        }
        if self.finalized {
            return Ok(0);
        }

        let mut scratch = [0u8; 4096];
        loop {
            let read = self.inner.read(&mut scratch)?;
            if read == 0 {
                self.finalize_summary();
                return Ok(self.take_summary(buf));
            }
            self.process_chunk(&scratch[..read]);
        }
    }
}

impl<R> HeadStreamReader<R> {
    fn new(inner: R, mode: StreamingHeadMode) -> Self {
        Self {
            inner,
            mode,
            finished: false,
            pending: Vec::new(),
            pending_offset: 0,
            lines_seen: 0,
        }
    }

    fn take_from_pending(&mut self, buf: &mut [u8]) -> usize {
        if self.pending_offset >= self.pending.len() {
            self.pending.clear();
            self.pending_offset = 0;
            return 0;
        }
        let remaining = &self.pending[self.pending_offset..];
        let to_copy = remaining.len().min(buf.len());
        buf[..to_copy].copy_from_slice(&remaining[..to_copy]);
        self.pending_offset += to_copy;
        if self.pending_offset == self.pending.len() {
            self.pending.clear();
            self.pending_offset = 0;
        }
        to_copy
    }
}

impl<R: Read> Read for HeadStreamReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let copied = self.take_from_pending(buf);
        if copied > 0 {
            return Ok(copied);
        }
        if self.finished {
            return Ok(0);
        }
        match self.mode {
            StreamingHeadMode::Bytes(_) => self.read_bytes_mode(buf),
            StreamingHeadMode::Lines(limit) => self.read_lines_mode(buf, limit),
        }
    }
}

impl<R: Read> HeadStreamReader<R> {
    fn read_bytes_mode(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let StreamingHeadMode::Bytes(ref mut remaining) = self.mode else {
            unreachable!("read_bytes_mode called in non-Bytes mode")
        };
        if *remaining == 0 {
            self.finished = true;
            return Ok(0);
        }
        let to_read = (*remaining).min(buf.len());
        let read = self.inner.read(&mut buf[..to_read])?;
        *remaining = remaining.saturating_sub(read);
        if read == 0 || *remaining == 0 {
            self.finished = true;
        }
        Ok(read)
    }

    fn read_lines_mode(&mut self, buf: &mut [u8], limit: usize) -> std::io::Result<usize> {
        if self.lines_seen >= limit {
            self.finished = true;
            return Ok(0);
        }
        let mut produced = 0usize;
        while produced < buf.len() && self.lines_seen < limit {
            match self.read_one_line_byte(&mut buf[produced..=produced], produced)? {
                HeadLinesStep::Produced => produced += 1,
                HeadLinesStep::EofBreak => break,
                HeadLinesStep::WouldBlockYield => return Ok(produced),
            }
        }
        if self.lines_seen >= limit {
            self.finished = true;
        }
        Ok(produced)
    }

    fn read_one_line_byte(
        &mut self,
        slot: &mut [u8],
        produced: usize,
    ) -> std::io::Result<HeadLinesStep> {
        let read = match self.inner.read(slot) {
            Ok(n) => n,
            Err(err) if err.kind() == ErrorKind::WouldBlock && produced > 0 => {
                return Ok(HeadLinesStep::WouldBlockYield);
            }
            Err(err) => return Err(err),
        };
        if read == 0 {
            self.finished = true;
            return Ok(HeadLinesStep::EofBreak);
        }
        if slot[0] == b'\n' {
            self.lines_seen += 1;
        }
        Ok(HeadLinesStep::Produced)
    }
}

enum HeadLinesStep {
    Produced,
    EofBreak,
    WouldBlockYield,
}

impl<R> TailStreamReader<R> {
    fn new(inner: R, mode: StreamingTailMode) -> Self {
        Self {
            inner,
            mode,
            output_pending: Vec::new(),
            output_offset: 0,
            finalized: false,
            byte_ring: VecDeque::new(),
            line_ring: VecDeque::new(),
            current_line: Vec::new(),
        }
    }

    fn push_tail_byte(&mut self, byte: u8) {
        let StreamingTailMode::Bytes(limit) = self.mode else {
            return;
        };
        if limit == 0 {
            return;
        }
        if self.byte_ring.len() == limit {
            self.byte_ring.pop_front();
        }
        self.byte_ring.push_back(byte);
    }

    fn push_tail_line(&mut self, line: Vec<u8>) {
        let StreamingTailMode::Lines(limit) = self.mode else {
            return;
        };
        if limit == 0 {
            return;
        }
        if self.line_ring.len() == limit {
            self.line_ring.pop_front();
        }
        self.line_ring.push_back(line);
    }

    fn process_chunk(&mut self, chunk: &[u8]) {
        match self.mode {
            StreamingTailMode::Bytes(_) => {
                for &byte in chunk {
                    self.push_tail_byte(byte);
                }
            }
            StreamingTailMode::Lines(_) => {
                for &byte in chunk {
                    if byte == b'\n' {
                        let line = std::mem::take(&mut self.current_line);
                        self.push_tail_line(line);
                    } else {
                        self.current_line.push(byte);
                    }
                }
            }
        }
    }

    fn finalize_output(&mut self) {
        if self.finalized {
            return;
        }
        match self.mode {
            StreamingTailMode::Bytes(_) => {
                self.output_pending.extend(self.byte_ring.drain(..));
            }
            StreamingTailMode::Lines(_) => {
                if !self.current_line.is_empty() {
                    let line = std::mem::take(&mut self.current_line);
                    self.push_tail_line(line);
                }
                for line in self.line_ring.drain(..) {
                    self.output_pending.extend_from_slice(&line);
                    self.output_pending.push(b'\n');
                }
            }
        }
        self.finalized = true;
    }
}

impl<R: Read> Read for TailStreamReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let copied = take_pending_output(&mut self.output_pending, &mut self.output_offset, buf);
        if copied > 0 {
            return Ok(copied);
        }
        if self.finalized {
            return Ok(0);
        }
        loop {
            let mut scratch = [0u8; 4096];
            match self.inner.read(&mut scratch) {
                Ok(0) => {
                    self.finalize_output();
                    return Ok(take_pending_output(
                        &mut self.output_pending,
                        &mut self.output_offset,
                        buf,
                    ));
                }
                Ok(read) => self.process_chunk(&scratch[..read]),
                Err(err) => return Err(err),
            }
        }
    }
}

fn streaming_bat_in_range(line_num: usize, range: Option<(Option<usize>, Option<usize>)>) -> bool {
    let Some((start, end)) = range else {
        return true;
    };
    if start.is_some_and(|s| line_num < s) {
        return false;
    }
    end.is_none_or(|e| line_num <= e)
}

fn streaming_make_visible(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch == '\t' {
            out.push_str("\\t");
        } else if ch == '\r' {
            out.push_str("\\r");
        } else if ch.is_control() {
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("\\x{:02x}", ch as u32));
        } else {
            out.push(ch);
        }
    }
    out
}

impl<R> BatStreamReader<R> {
    fn new(inner: R, stage: StreamingBatStage) -> Self {
        Self {
            inner,
            stage,
            input_pending: Vec::new(),
            output_pending: Vec::new(),
            output_offset: 0,
            finished: false,
            header_emitted: false,
            footer_emitted: false,
            line_num: 0,
        }
    }

    fn emit_header(&mut self) {
        if !self.stage.show_header || self.header_emitted {
            return;
        }
        self.header_emitted = true;
        let separator = "\u{2500}";
        let rule_left: String = separator.repeat(7);
        let rule_right: String = separator.repeat(20);
        let top_corner = "\u{252C}";
        let mid_corner = "\u{253C}";
        self.output_pending
            .extend_from_slice(format!("{rule_left}{top_corner}{rule_right}\n").as_bytes());
        self.output_pending
            .extend_from_slice(format!("{rule_left}{mid_corner}{rule_right}\n").as_bytes());
    }

    fn emit_footer(&mut self) {
        if !self.stage.show_header || self.footer_emitted {
            return;
        }
        self.footer_emitted = true;
        let separator = "\u{2500}";
        let rule_left: String = separator.repeat(7);
        let rule_right: String = separator.repeat(20);
        let bot_corner = "\u{2534}";
        self.output_pending
            .extend_from_slice(format!("{rule_left}{bot_corner}{rule_right}\n").as_bytes());
    }
}

impl<R: Read> Read for BatStreamReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let copied =
                take_pending_output(&mut self.output_pending, &mut self.output_offset, buf);
            if copied > 0 {
                return Ok(copied);
            }
            if self.finished {
                return Ok(0);
            }
            self.emit_header();
            let copied =
                take_pending_output(&mut self.output_pending, &mut self.output_offset, buf);
            if copied > 0 {
                return Ok(copied);
            }
            self.pump_next_bat_line()?;
        }
    }
}

impl<R: Read> BatStreamReader<R> {
    fn pump_next_bat_line(&mut self) -> std::io::Result<()> {
        if let Some((line, _had_newline)) =
            streaming_read_next_line(&mut self.inner, &mut self.input_pending)?
        {
            self.line_num += 1;
            if streaming_bat_in_range(self.line_num, self.stage.line_range) {
                self.emit_bat_line(&line);
            }
        } else {
            self.emit_footer();
            self.finished = true;
        }
        Ok(())
    }

    fn emit_bat_line(&mut self, line: &str) {
        let display_line = if self.stage.show_all {
            streaming_make_visible(line)
        } else {
            line.to_string()
        };
        if self.stage.show_numbers {
            self.output_pending.extend_from_slice(
                format!("{:>5}   \u{2502} {display_line}\n", self.line_num).as_bytes(),
            );
        } else {
            self.output_pending
                .extend_from_slice(format!("{display_line}\n").as_bytes());
        }
    }
}

pub(crate) fn streaming_simple_grep_match(line: &str, pattern: &str) -> bool {
    if let Some(rest) = pattern.strip_prefix('^') {
        if let Some(mid) = rest.strip_suffix('$') {
            line == mid
        } else {
            line.starts_with(rest)
        }
    } else if let Some(rest) = pattern.strip_suffix('$') {
        line.ends_with(rest)
    } else {
        line.contains(pattern)
    }
}

impl<R> PasteStreamReader<R> {
    fn new(inner: R, stage: StreamingPasteStage) -> Self {
        Self {
            inner,
            stage,
            input_pending: Vec::new(),
            output_pending: Vec::new(),
            output_offset: 0,
            finalized: false,
            ended_with_newline: true,
            serial_first: true,
        }
    }

    fn finalize_serial(&mut self) -> std::io::Result<()>
    where
        R: Read,
    {
        while let Some((line, _had_newline)) =
            streaming_read_next_line(&mut self.inner, &mut self.input_pending)?
        {
            if !self.serial_first {
                self.output_pending
                    .extend_from_slice(self.stage.delimiter.as_bytes());
            }
            self.output_pending.extend_from_slice(line.as_bytes());
            self.serial_first = false;
        }
        self.output_pending.push(b'\n');
        Ok(())
    }
}

impl<R: Read> Read for PasteStreamReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let copied =
                take_pending_output(&mut self.output_pending, &mut self.output_offset, buf);
            if copied > 0 {
                return Ok(copied);
            }
            if self.finalized {
                return Ok(0);
            }

            if self.stage.serial {
                self.finalize_serial()?;
                self.finalized = true;
                continue;
            }

            let mut scratch = [0u8; 4096];
            let read = self.inner.read(&mut scratch)?;
            if read == 0 {
                if !self.ended_with_newline {
                    self.output_pending.push(b'\n');
                }
                self.finalized = true;
                continue;
            }
            self.ended_with_newline = scratch[read - 1] == b'\n';
            self.output_pending.extend_from_slice(&scratch[..read]);
        }
    }
}

impl<R> ColumnStreamReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            output_pending: Vec::new(),
            output_offset: 0,
            finalized: false,
            ended_with_newline: true,
        }
    }
}

impl<R: Read> Read for ColumnStreamReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let copied =
                take_pending_output(&mut self.output_pending, &mut self.output_offset, buf);
            if copied > 0 {
                return Ok(copied);
            }
            if self.finalized {
                return Ok(0);
            }

            let mut scratch = [0u8; 4096];
            let read = self.inner.read(&mut scratch)?;
            if read == 0 {
                if !self.ended_with_newline {
                    self.output_pending.push(b'\n');
                }
                self.finalized = true;
                continue;
            }
            self.ended_with_newline = scratch[read - 1] == b'\n';
            self.output_pending.extend_from_slice(&scratch[..read]);
        }
    }
}

pub(crate) fn take_pending_output(
    pending: &mut Vec<u8>,
    pending_offset: &mut usize,
    buf: &mut [u8],
) -> usize {
    if *pending_offset >= pending.len() {
        pending.clear();
        *pending_offset = 0;
        return 0;
    }
    let remaining = &pending[*pending_offset..];
    let to_copy = remaining.len().min(buf.len());
    buf[..to_copy].copy_from_slice(&remaining[..to_copy]);
    *pending_offset += to_copy;
    if *pending_offset == pending.len() {
        pending.clear();
        *pending_offset = 0;
    }
    to_copy
}

pub(crate) fn streaming_read_next_line(
    reader: &mut dyn Read,
    pending: &mut Vec<u8>,
) -> std::io::Result<Option<(String, bool)>> {
    loop {
        if let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            let mut line = pending.drain(..=pos).collect::<Vec<u8>>();
            let _ = line.pop();
            return Ok(Some((String::from_utf8_lossy(&line).to_string(), true)));
        }

        let mut buffer = [0u8; 4096];
        match reader.read(&mut buffer) {
            Ok(0) => {
                if pending.is_empty() {
                    return Ok(None);
                }
                let line = std::mem::take(pending);
                return Ok(Some((String::from_utf8_lossy(&line).to_string(), false)));
            }
            Ok(read) => pending.extend_from_slice(&buffer[..read]),
            Err(err) => return Err(err),
        }
    }
}

struct RevStreamReader<R> {
    inner: R,
    input_pending: Vec<u8>,
    output_pending: Vec<u8>,
    output_offset: usize,
    finished: bool,
}

impl<R> RevStreamReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            input_pending: Vec::new(),
            output_pending: Vec::new(),
            output_offset: 0,
            finished: false,
        }
    }
}

impl<R: Read> Read for RevStreamReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let copied = take_pending_output(&mut self.output_pending, &mut self.output_offset, buf);
        if copied > 0 {
            return Ok(copied);
        }
        if self.finished {
            return Ok(0);
        }

        if let Some((line, _had_newline)) =
            streaming_read_next_line(&mut self.inner, &mut self.input_pending)?
        {
            let reversed: String = line.chars().rev().collect();
            self.output_pending.extend_from_slice(reversed.as_bytes());
            self.output_pending.push(b'\n');
            Ok(take_pending_output(
                &mut self.output_pending,
                &mut self.output_offset,
                buf,
            ))
        } else {
            self.finished = true;
            Ok(0)
        }
    }
}

/// Result from an external command handler.
#[derive(Debug)]
pub struct ExternalCommandResult {
    /// Data written to stdout.
    pub stdout: Vec<u8>,
    /// Data written to stderr.
    pub stderr: Vec<u8>,
    /// Exit code (0 = success).
    pub status: i32,
}

pub struct ExternalCommandStdin<'a> {
    reader: Box<dyn Read + 'a>,
}

struct LimitedReader {
    reader: Box<dyn Read>,
    remaining: u64,
}

impl Read for LimitedReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut probe = [0u8; 1];
            return match self.reader.read(&mut probe) {
                Ok(0) => Ok(0),
                Ok(_) => Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "external stdin limit exceeded",
                )),
                Err(error) => Err(error),
            };
        }
        let read_len = buf.len().min(self.remaining as usize);
        let read = self.reader.read(&mut buf[..read_len])?;
        self.remaining = self.remaining.saturating_sub(read as u64);
        Ok(read)
    }
}

impl std::fmt::Debug for ExternalCommandStdin<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalCommandStdin")
            .finish_non_exhaustive()
    }
}

impl<'a> ExternalCommandStdin<'a> {
    #[must_use]
    pub fn from_bytes(data: &'a [u8]) -> Self {
        Self {
            reader: Box::new(Cursor::new(data)),
        }
    }

    #[must_use]
    pub fn from_reader<R>(reader: R) -> Self
    where
        R: Read + 'a,
    {
        Self {
            reader: Box::new(reader),
        }
    }

    fn from_limited_reader(reader: Box<dyn Read>, max_bytes: u64) -> Self {
        Self {
            reader: Box::new(LimitedReader {
                reader,
                remaining: max_bytes,
            }),
        }
    }

    pub fn read_chunk(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
    }
}

impl Read for ExternalCommandStdin<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.read_chunk(buf)
    }
}

/// Callback type for external (host-provided) commands.
///
/// Called with `(command_name, argv, stdin)`. Returns `Some(result)` if
/// the command was handled, `None` to fall through to "command not found".
pub type ExternalCommandHandler = Box<
    dyn FnMut(&str, &[String], Option<ExternalCommandStdin<'_>>) -> Option<ExternalCommandResult>,
>;

impl ExternalCommandSpec {
    fn validate_name(name: &str) -> Result<(), String> {
        if name.is_empty() || name.chars().any(char::is_whitespace) || name.contains('\0') {
            return Err(
                "external command name must be non-empty and contain no whitespace or NUL".into(),
            );
        }
        if name.contains('/') {
            return Err("external command name must not contain '/'".into());
        }
        Ok(())
    }

    fn validate(options: &ExternalCommandOptions) -> Result<(), String> {
        if let Some(cwd) = &options.cwd {
            if cwd.is_empty() || cwd.contains('\0') {
                return Err("external cwd must be non-empty and contain no NUL".into());
            }
        }
        if options.max_input_bytes == 0 || options.max_input_bytes > MAX_EXTERNAL_BUFFER_BYTES {
            return Err(format!(
                "external max_input_bytes must be between 1 and {MAX_EXTERNAL_BUFFER_BYTES}"
            ));
        }
        if options.max_output_bytes == 0 || options.max_output_bytes > MAX_EXTERNAL_BUFFER_BYTES {
            return Err(format!(
                "external max_output_bytes must be between 1 and {MAX_EXTERNAL_BUFFER_BYTES}"
            ));
        }
        if options.timeout_ms == 0 || options.timeout_ms > MAX_EXTERNAL_TIMEOUT_MS {
            return Err(format!(
                "external timeout_ms must be between 1 and {MAX_EXTERNAL_TIMEOUT_MS}"
            ));
        }
        if options.stream_queue_bytes == 0
            || options.stream_queue_bytes > MAX_EXTERNAL_STREAM_QUEUE_BYTES
        {
            return Err(format!(
                "external stream_queue_bytes must be between 1 and {MAX_EXTERNAL_STREAM_QUEUE_BYTES}"
            ));
        }
        if options.stream_chunk_bytes == 0
            || options.stream_chunk_bytes > MAX_EXTERNAL_STREAM_CHUNK_BYTES
        {
            return Err(format!(
                "external stream_chunk_bytes must be between 1 and {MAX_EXTERNAL_STREAM_CHUNK_BYTES}"
            ));
        }
        for (key, value) in &options.env {
            if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0') {
                return Err("external env keys/values must not contain '=' or NUL".into());
            }
        }
        for arg in &options.argv_prefix {
            if arg.contains('\0') {
                return Err("external argv_prefix must not contain NUL".into());
            }
        }
        for mapping in &options.vfs_path_mappings {
            if !mapping.vfs_prefix.starts_with('/')
                || mapping.vfs_prefix != wasmsh_fs::normalize_path(&mapping.vfs_prefix)
                || mapping.host_prefix.is_empty()
                || mapping.host_prefix.contains('\0')
            {
                return Err(
                    "external VFS mappings require a normalized absolute vfs_prefix and non-empty host_prefix"
                        .into(),
                );
            }
        }
        Ok(())
    }

    /// Validate and construct a fixed executable registration.
    pub fn new(
        name: impl Into<String>,
        executable: impl Into<String>,
        options: ExternalCommandOptions,
    ) -> Result<Self, String> {
        let name = name.into();
        let executable = executable.into();
        Self::validate_name(&name)?;
        if executable.is_empty() || executable.contains('\0') {
            return Err("external executable must be non-empty and contain no NUL".into());
        }
        Self::validate(&options)?;
        Ok(Self {
            name,
            executable,
            options,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuntimeCommandKind {
    Local,
    Break,
    Continue,
    Return,
    Exit,
    Eval,
    Source,
    Declare,
    Let,
    Shopt,
    Alias,
    Unalias,
    BuiltinKeyword,
    Mapfile,
    Type,
    CommandKeyword,
    ExecKeyword,
    Hash,
    Times,
    Dirs,
    Pushd,
    Popd,
    Umask,
    Wait,
    Ulimit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UtilityCommandKind {
    Plain,
    FindWithExec,
    Xargs,
}

#[derive(Clone, Debug)]
enum ResolvedCommand {
    Runtime(RuntimeCommandKind),
    ShellScript,
    /// A file with `#!/bin/bash` or similar shebang, executed directly by path.
    ShebangScript,
    Function(HirCommand),
    Builtin(wasmsh_builtins::BuiltinFn),
    Utility(UtilityCommandKind, wasmsh_utils::UtilFn),
    External,
}

#[derive(Clone, Debug)]
struct ActiveRun {
    input: String,
    hir: HirProgram,
    complete_index: usize,
    and_or_index: usize,
}

impl ActiveRun {
    fn new(input: String, hir: HirProgram) -> Self {
        Self {
            input,
            hir,
            complete_index: 0,
            and_or_index: 0,
        }
    }

    fn is_done(&self) -> bool {
        self.complete_index >= self.hir.items.len()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActiveRunStep {
    Pending,
    Wait,
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionPoll {
    Yield(Vec<WorkerEvent>),
    Done(Vec<WorkerEvent>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum VmSubsetFallbackReason {
    Disabled,
    Lowering(LoweringError),
    AssignmentShape,
    UnsupportedWord,
    ShellExpansion,
    AliasExpansion,
    NonBuiltinCommand,
    CommandEnvPrefixes,
    UnsupportedRedirection,
}

struct RuntimeVmExecutor<'a> {
    fs: &'a mut BackendFs,
    builtins: &'a wasmsh_builtins::BuiltinRegistry,
    current_exec_io: &'a mut Option<ExecIo>,
    proc_subst_out_scopes: &'a mut Vec<Vec<PendingProcessSubstOut>>,
    exec: &'a mut ExecState,
}

impl RuntimeVmExecutor<'_> {
    fn prepare_exec_io(
        &mut self,
        state: &mut ShellState,
        redirections: &[IrRedirection],
    ) -> Result<Option<ExecIo>, String> {
        let mut exec_io = self.current_exec_io.clone().unwrap_or_default();
        let mut handled_any = false;

        for redirection in redirections {
            let fd = redirection.fd.unwrap_or(1);
            let append = matches!(redirection.op, RedirectionOp::Append);
            let target = wasmsh_expand::expand_word(&redirection.target, state);
            let path = resolve_path_from_cwd(&state.cwd, &target);
            if matches!(redirection.op, RedirectionOp::Output)
                && state.get_var("SHOPT_C").as_deref() == Some("1")
                && self.fs.stat(&path).is_ok()
            {
                return Err(format!(
                    "wasmsh: {target}: cannot overwrite existing file\n"
                ));
            }
            let sink = match self.fs.open_write_sink(&path, append) {
                Ok(sink) => sink,
                Err(err) => {
                    return Err(format!("wasmsh: {target}: {err}\n"));
                }
            };
            exec_io.fds_mut().open_output(
                fd,
                OutputTarget::File {
                    path,
                    append,
                    sink: Rc::new(RefCell::new(sink)),
                },
            );
            handled_any = true;
        }

        Ok(handled_any.then_some(exec_io))
    }

    fn with_exec_io_scope<T>(
        current_exec_io: &mut Option<ExecIo>,
        proc_subst_out_scopes: &mut Vec<Vec<PendingProcessSubstOut>>,
        exec: &mut ExecState,
        exec_io: Option<ExecIo>,
        f: impl FnOnce(&mut Option<ExecIo>, &mut Vec<Vec<PendingProcessSubstOut>>, &mut ExecState) -> T,
    ) -> T {
        if let Some(exec_io) = exec_io {
            let saved = current_exec_io.replace(exec_io);
            let result = f(current_exec_io, proc_subst_out_scopes, exec);
            let current = current_exec_io.take();
            *current_exec_io = match (saved, current) {
                (Some(mut saved), Some(mut current)) => {
                    let stdin = current.take_stdin();
                    saved.fds_mut().set_input(stdin);
                    Some(saved)
                }
                (saved, _) => saved,
            };
            result
        } else {
            f(current_exec_io, proc_subst_out_scopes, exec)
        }
    }

    fn write_visible_stderr(&mut self, vm: &mut Vm, data: &[u8]) {
        let mut router = RuntimeOutputRouter {
            exec: self.exec,
            exec_io: self.current_exec_io.as_mut(),
            proc_subst_out_scopes: self.proc_subst_out_scopes,
            vm_stdout: &mut vm.stdout,
            vm_stderr: &mut vm.stderr,
            vm_output_bytes: &mut vm.output_bytes,
            vm_output_limit: vm.limits.output_byte_limit,
            vm_diagnostics: &mut vm.diagnostics,
        };
        router.write_stderr(data);
    }

    fn take_pending_input_reader(
        &mut self,
        cmd_name: &str,
    ) -> Result<Option<Box<dyn Read>>, String> {
        let Some(exec_io) = self.current_exec_io.as_mut() else {
            return Ok(None);
        };
        match exec_io.take_stdin() {
            InputTarget::Inherit | InputTarget::Closed => Ok(None),
            InputTarget::Bytes(data) => Ok(Some(Box::new(Cursor::new(data)))),
            InputTarget::File {
                path,
                remove_after_read,
            } => {
                let handle = self
                    .fs
                    .open(&path, OpenOptions::read())
                    .map_err(|err| format!("wasmsh: {cmd_name}: {err}\n"))?;
                let reader = self
                    .fs
                    .stream_file(handle)
                    .map_err(|err| format!("wasmsh: {cmd_name}: {err}\n"));
                self.fs.close(handle);
                if remove_after_read {
                    let _ = self.fs.remove_file(&path);
                }
                reader.map(Some)
            }
            InputTarget::Pipe(pipe) => Ok(Some(Box::new(PipeReader::new(pipe)))),
        }
    }

    fn take_builtin_stdin(
        &mut self,
        cmd_name: &str,
    ) -> Result<Option<wasmsh_builtins::BuiltinStdin<'static>>, String> {
        let reader = self.take_pending_input_reader(cmd_name)?;
        Ok(reader.map(wasmsh_builtins::BuiltinStdin::from_reader))
    }

    /// Drain a pending nounset error from parameter expansion so the VM-subset
    /// path reports it the same way the fallback interpreter does.
    fn consume_nounset_error(&mut self, vm: &mut Vm) -> bool {
        let Some(var_name) = vm.state.take_nounset_error() else {
            return false;
        };
        let msg = format!("wasmsh: {var_name}: unbound variable\n");
        self.write_visible_stderr(vm, msg.as_bytes());
        vm.state.last_status = 1;
        // `set -u` on an unbound variable aborts a non-interactive script.
        self.exec.exit_requested = Some(1);
        true
    }

    /// Report a pending hard arithmetic error (`$((1/0))`, depth guard, syntax)
    /// so the simple command fails with status 1 instead of silently returning
    /// `0`. Mirrors [`Self::consume_nounset_error`].
    fn consume_arith_error(&mut self, vm: &mut Vm) -> bool {
        let Some(message) = vm.state.take_arith_error() else {
            return false;
        };
        let msg = format!("wasmsh: {message}\n");
        self.write_visible_stderr(vm, msg.as_bytes());
        vm.state.last_status = 1;
        true
    }

    /// Report a pending shell error (`${x:?message}`, readonly assignment).
    /// Fatal errors additionally request script exit, matching bash.
    fn consume_expansion_error(&mut self, vm: &mut Vm) -> bool {
        let Some((fatal, message)) = vm.state.take_shell_error() else {
            return false;
        };
        let msg = format!("wasmsh: {message}\n");
        self.write_visible_stderr(vm, msg.as_bytes());
        vm.state.last_status = 1;
        if fatal {
            self.exec.exit_requested = Some(1);
        }
        true
    }
}

impl VmExecutor for RuntimeVmExecutor<'_> {
    fn assign(&mut self, vm: &mut Vm, name: &str, value: Option<&Word>) {
        let value = value.map_or_else(String::new, |word| {
            wasmsh_expand::expand_word(word, &mut vm.state)
        });
        if self.consume_nounset_error(vm)
            || self.consume_arith_error(vm)
            || self.consume_expansion_error(vm)
        {
            return;
        }
        if vm.state.is_var_readonly(name) {
            vm.state
                .set_shell_error(&format!("{name}: readonly variable"));
            let _ = self.consume_expansion_error(vm);
            vm.state.last_status = 1;
            return;
        }
        let trimmed = value.trim();
        if trimmed.starts_with('(') && trimmed.ends_with(')') {
            let inner = &trimmed[1..trimmed.len() - 1];
            let elements = WorkerRuntime::parse_array_elements(inner);
            let name_key = smol_str::SmolStr::from(name);

            if WorkerRuntime::is_assoc_array_assignment(inner, &elements) {
                vm.state.init_assoc_array(name_key.clone());
                for (key, value) in WorkerRuntime::parse_assoc_pairs(inner) {
                    vm.state.set_array_element(
                        name_key.clone(),
                        &key,
                        smol_str::SmolStr::from(value.as_str()),
                    );
                }
            } else {
                vm.state.init_indexed_array(name_key.clone());
                for (idx, element) in elements.iter().enumerate() {
                    vm.state
                        .set_array_element(name_key.clone(), &idx.to_string(), element.clone());
                }
            }
            vm.state.last_status = 0;
            return;
        }

        let assigned = if vm.state.env.get(name).is_some_and(|var| var.integer) {
            wasmsh_expand::eval_arithmetic(trimmed, &mut vm.state).to_string()
        } else {
            value
        };
        vm.state.set_var(name.into(), assigned.into());
        vm.state.last_status = 0;
    }

    fn execute_builtin(
        &mut self,
        vm: &mut Vm,
        name: &str,
        argv: &[Word],
        redirections: &[IrRedirection],
    ) -> i32 {
        let Some(builtin_fn) = self.builtins.get(name) else {
            vm.emit_diagnostic(
                wasmsh_vm::DiagLevel::Error,
                wasmsh_vm::DiagCategory::Builtin,
                format!("unknown builtin: {name}"),
            );
            vm.state.last_status = 127;
            return 127;
        };
        let expanded: Vec<String> = argv
            .iter()
            .map(|word| wasmsh_expand::expand_word(word, &mut vm.state))
            .collect();
        if self.consume_nounset_error(vm)
            || self.consume_arith_error(vm)
            || self.consume_expansion_error(vm)
        {
            return 1;
        }
        let argv_refs: Vec<&str> = expanded.iter().map(String::as_str).collect();
        let stdin = match self.take_builtin_stdin(name) {
            Ok(stdin) => stdin,
            Err(message) => {
                self.write_visible_stderr(vm, message.as_bytes());
                vm.state.last_status = 1;
                return 1;
            }
        };
        let exec_io = match self.prepare_exec_io(&mut vm.state, redirections) {
            Ok(exec_io) => exec_io,
            Err(message) => {
                self.write_visible_stderr(vm, message.as_bytes());
                vm.state.last_status = 1;
                return 1;
            }
        };

        let fs = &*self.fs;
        let status = Self::with_exec_io_scope(
            &mut *self.current_exec_io,
            &mut *self.proc_subst_out_scopes,
            &mut *self.exec,
            exec_io,
            |current_exec_io, proc_subst_out_scopes, exec| {
                let mut router = RuntimeOutputRouter {
                    exec,
                    exec_io: current_exec_io.as_mut(),
                    proc_subst_out_scopes,
                    vm_stdout: &mut vm.stdout,
                    vm_stderr: &mut vm.stderr,
                    vm_output_bytes: &mut vm.output_bytes,
                    vm_output_limit: vm.limits.output_byte_limit,
                    vm_diagnostics: &mut vm.diagnostics,
                };
                let mut sink = RuntimeBuiltinSink {
                    router: &mut router,
                };
                {
                    let status = {
                        let mut ctx = wasmsh_builtins::BuiltinContext {
                            state: &mut vm.state,
                            output: &mut sink,
                            fs: Some(fs),
                            stdin,
                        };
                        builtin_fn(&mut ctx, &argv_refs)
                    };
                    if name == "read" {
                        install_read_remainder(&mut vm.state, current_exec_io);
                    }
                    status
                }
            },
        );
        if let Some(last) = expanded.last() {
            vm.state.set_last_argument(last.as_str());
        }
        vm.state.last_status = status;
        status
    }
}

/// The worker-side runtime that processes host commands.
struct PolicyNetworkBackend {
    policy: Rc<RefCell<NetworkPolicy>>,
    inner: Box<dyn NetworkBackend>,
}

/// Default maximum bytes buffered for one external command's stdin.
pub const DEFAULT_EXTERNAL_INPUT_BYTES: u64 = 16 * 1024 * 1024;
/// Default maximum combined bytes buffered from one external command's output.
pub const DEFAULT_EXTERNAL_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;
/// Hard upper bound for the finite external-command compatibility interface.
pub const MAX_EXTERNAL_BUFFER_BYTES: u64 = 64 * 1024 * 1024;
/// Default wall-clock timeout requested from a native host executor.
pub const DEFAULT_EXTERNAL_TIMEOUT_MS: u64 = 30_000;
/// Hard upper bound for a finite native external-command invocation.
pub const MAX_EXTERNAL_TIMEOUT_MS: u64 = 300_000;
/// Default per-stream queue limit for the polling external protocol.
pub const DEFAULT_EXTERNAL_STREAM_QUEUE_BYTES: u64 = 64 * 1024;
/// Default maximum chunk returned by one external stream poll.
pub const DEFAULT_EXTERNAL_STREAM_CHUNK_BYTES: u64 = 4096;
/// Hard upper bound for one external stream queue.
pub const MAX_EXTERNAL_STREAM_QUEUE_BYTES: u64 = 1024 * 1024;
/// Hard upper bound for one external stream chunk.
pub const MAX_EXTERNAL_STREAM_CHUNK_BYTES: u64 = 64 * 1024;

fn default_external_input_bytes() -> u64 {
    DEFAULT_EXTERNAL_INPUT_BYTES
}

fn default_external_output_bytes() -> u64 {
    DEFAULT_EXTERNAL_OUTPUT_BYTES
}

fn default_external_timeout_ms() -> u64 {
    DEFAULT_EXTERNAL_TIMEOUT_MS
}

fn default_external_stream_queue_bytes() -> u64 {
    DEFAULT_EXTERNAL_STREAM_QUEUE_BYTES
}

fn default_external_stream_chunk_bytes() -> u64 {
    DEFAULT_EXTERNAL_STREAM_CHUNK_BYTES
}

/// A virtual-to-host path prefix made available to a registered executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalPathMapping {
    /// Absolute POSIX path prefix in the shell VFS.
    pub vfs_prefix: String,
    /// Host filesystem prefix used only by the external executor.
    pub host_prefix: String,
}

/// Finite-input options for one registered external command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExternalCommandOptions {
    /// Host filesystem cwd. This is not a VFS path and is never inferred.
    pub cwd: Option<String>,
    /// Exact environment exported to the child. The host environment is not inherited.
    pub env: BTreeMap<String, String>,
    /// Explicit VFS path mappings applied by a host executor to path arguments.
    pub vfs_path_mappings: Vec<ExternalPathMapping>,
    /// Fixed arguments inserted by the host adapter before shell argv[1..].
    pub argv_prefix: Vec<String>,
    /// Maximum bytes accepted from stdin.
    #[serde(default = "default_external_input_bytes")]
    pub max_input_bytes: u64,
    /// Maximum combined stdout/stderr bytes retained from the child.
    #[serde(default = "default_external_output_bytes")]
    pub max_output_bytes: u64,
    /// Native child wall-clock timeout in milliseconds.
    #[serde(default = "default_external_timeout_ms")]
    pub timeout_ms: u64,
    /// Maximum queued bytes per stdout/stderr stream in streaming mode.
    #[serde(default = "default_external_stream_queue_bytes")]
    pub stream_queue_bytes: u64,
    /// Maximum bytes returned by one streaming poll per stream.
    #[serde(default = "default_external_stream_chunk_bytes")]
    pub stream_chunk_bytes: u64,
}

impl Default for ExternalCommandOptions {
    fn default() -> Self {
        Self {
            cwd: None,
            env: BTreeMap::new(),
            vfs_path_mappings: Vec::new(),
            argv_prefix: Vec::new(),
            max_input_bytes: DEFAULT_EXTERNAL_INPUT_BYTES,
            max_output_bytes: DEFAULT_EXTERNAL_OUTPUT_BYTES,
            timeout_ms: DEFAULT_EXTERNAL_TIMEOUT_MS,
            stream_queue_bytes: DEFAULT_EXTERNAL_STREAM_QUEUE_BYTES,
            stream_chunk_bytes: DEFAULT_EXTERNAL_STREAM_CHUNK_BYTES,
        }
    }
}

/// Trusted executable and finite-I/O policy for one shell command name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalCommandSpec {
    /// Name used in shell source.
    pub name: String,
    /// Fixed host executable selected by the trusted host.
    pub executable: String,
    /// Finite-I/O and host mapping options.
    pub options: ExternalCommandOptions,
}

/// Result from a spec-aware external command handler.
pub type ExternalCommandSpecHandler = Box<
    dyn FnMut(
        &ExternalCommandSpec,
        &[String],
        Option<ExternalCommandStdin<'_>>,
    ) -> Option<ExternalCommandResult>,
>;

/// Result of writing one chunk to a streaming external process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalProcessWrite {
    /// Number of bytes accepted by the host process.
    pub accepted: usize,
    /// The host is applying backpressure; poll before writing more.
    pub would_block: bool,
    /// The child stdin is closed and cannot accept more data.
    pub closed: bool,
}

/// One non-blocking snapshot of a streaming external process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalProcessPoll {
    /// Bytes currently available on stdout.
    pub stdout: Vec<u8>,
    /// Bytes currently available on stderr.
    pub stderr: Vec<u8>,
    /// True only after stdout has reached EOF and its queued bytes are empty.
    pub stdout_eof: bool,
    /// True only after stderr has reached EOF and its queued bytes are empty.
    pub stderr_eof: bool,
    /// Child exit status once the process has closed.
    pub status: Option<i32>,
    /// Whether a previously blocked stdin can accept another write.
    pub stdin_writable: bool,
    /// Host-side failure category/diagnostic, if any.
    pub error: Option<String>,
}

/// Handle for one started external process.
///
/// Every method is non-blocking. A host implementation must retain unread
/// bytes in bounded queues and report `WouldBlock`/non-EOF states rather than
/// converting temporary lack of data into EOF.
pub trait ExternalProcess {
    /// Attempt to write a chunk to child stdin.
    fn write_stdin(&mut self, data: &[u8]) -> ExternalProcessWrite;
    /// Close child stdin so the process observes EOF.
    fn close_stdin(&mut self);
    /// Poll both output streams and process status without waiting.
    fn poll(&mut self) -> ExternalProcessPoll;
    /// Terminate and synchronously initiate cleanup of the managed process tree.
    fn cancel(&mut self);
}

/// Starts registered external processes for progressive `StartRun`/`PollRun`.
pub type ExternalStreamHandler =
    Box<dyn FnMut(&ExternalCommandSpec, &[String]) -> Result<Box<dyn ExternalProcess>, String>>;

impl NetworkBackend for PolicyNetworkBackend {
    fn fetch(
        &self,
        request: &wasmsh_utils::net_types::HttpRequest,
    ) -> Result<wasmsh_utils::net_types::HttpResponse, NetworkError> {
        self.policy.borrow().check(&request.url)?;
        self.inner.fetch(request)
    }

    fn check_url(&self, url: &str) -> Result<(), NetworkError> {
        self.policy.borrow().check(url)
    }
}

#[allow(missing_debug_implementations)]
pub struct WorkerRuntime {
    config: BrowserConfig,
    vm: Vm,
    fs: BackendFs,
    utils: UtilRegistry,
    builtins: wasmsh_builtins::BuiltinRegistry,
    initialized: bool,
    /// Command-scoped stdin/stdout/stderr routing for the currently executing command.
    current_exec_io: Option<ExecIo>,
    /// Deferred `>(...)` sinks scoped to the currently executing command.
    proc_subst_out_scopes: Vec<Vec<PendingProcessSubstOut>>,
    /// Deferred `<(...)` cleanup and stderr flush scoped to the current command.
    proc_subst_in_scopes: Vec<Vec<PendingProcessSubstIn>>,
    /// Registered shell functions (name → HIR body).
    functions: IndexMap<String, HirCommand>,
    /// Transient execution state (loop control, exit, locals).
    exec: ExecState,
    /// Shell aliases (name → replacement text).
    aliases: IndexMap<String, String>,
    /// Optional handler for external commands (e.g. python3 in Pyodide).
    external_handler: Option<ExternalCommandHandler>,
    /// Registered fixed executable specifications for standalone hosts.
    external_specs: IndexMap<String, ExternalCommandSpec>,
    /// Handler that receives a registered executable specification.
    external_spec_handler: Option<ExternalCommandSpecHandler>,
    /// Non-blocking process starter used only by progressive runs.
    external_stream_handler: Option<ExternalStreamHandler>,
    /// Whether the active run may use the non-blocking process protocol.
    allow_external_streaming: bool,
    /// Optional network backend for curl/wget utilities.
    network: Option<Box<dyn NetworkBackend>>,
    /// Validated policy selected by the most recent initialization.
    network_policy: Option<NetworkPolicy>,
    /// Shared policy state for the installed wrapper. Keeping this separate
    /// from the transport lets repeated Init calls replace policy atomically
    /// without stacking wrappers around an older policy.
    network_policy_state: Option<Rc<RefCell<NetworkPolicy>>>,
    /// Whether `network` currently contains the policy wrapper.
    network_is_policy_wrapped: bool,
    /// Session clock capability. Wall and monotonic reads share this object.
    clock: Rc<dyn ClockProvider>,
    /// Monotonic origin for the current initialized session.
    monotonic_origin_ms: Option<u64>,
    /// Active top-level execution, if a run has been started and not yet completed.
    active_run: Option<ActiveRun>,
    /// Pipeline suspended at a non-blocking external process poll boundary.
    pending_streaming_pipeline: Option<PendingStreamingPipeline>,
    /// Signals queued for the next progressive poll.
    pending_signals: VecDeque<&'static RuntimeSignalSpec>,
    /// Exit status of the most recent command substitution executed while
    /// expanding an assignment value. Consumed by the assignment statement to
    /// report the substitution's status (bash: `x=$(false); echo $?` → 1).
    last_subst_status: Option<i32>,
}

/// Action to take for a character during array element parsing.
enum ArrayCharAction {
    Append(char),
    Skip,
    SplitField,
}

enum StreamingPipelineStage {
    Literal(Vec<u8>),
    File(String),
    Yes { line: Vec<u8> },
    External(Vec<String>),
    BufferedCommand(BufferedPipelineCommand),
    Cat,
    Head(StreamingHeadMode),
    Tail(StreamingTailMode),
    Bat(StreamingBatStage),
    Sed(StreamingSedStage),
    Tee(StreamingTeeStage),
    Paste(StreamingPasteStage),
    Column(StreamingColumnStage),
    Grep(StreamingGrepStage),
    Uniq(StreamingUniqFlags),
    Rev,
    Cut(StreamingCutStage),
    Tr(StreamingTrStage),
    Wc(StreamingWcFlags),
}

struct StreamingStageCtx<'a> {
    stages: &'a [StreamingPipelineStage],
    stage_pipe_stderr: &'a [bool],
    stage_statuses: &'a [Rc<RefCell<i32>>],
    stage_stderr: &'a [Rc<RefCell<Vec<u8>>>],
    output_pipes: &'a [Rc<RefCell<PipeBuffer>>],
}

/// Quoting state for parsing array elements.
#[derive(Default)]
struct ArrayParseState {
    in_single_quote: bool,
    in_double_quote: bool,
    escape_next: bool,
}

impl ArrayParseState {
    fn process_char(&mut self, ch: char) -> ArrayCharAction {
        if self.escape_next {
            self.escape_next = false;
            return ArrayCharAction::Append(ch);
        }
        if ch == '\\' && !self.in_single_quote {
            self.escape_next = true;
            return ArrayCharAction::Skip;
        }
        if ch == '\'' && !self.in_double_quote {
            self.in_single_quote = !self.in_single_quote;
            return ArrayCharAction::Skip;
        }
        if ch == '"' && !self.in_single_quote {
            self.in_double_quote = !self.in_double_quote;
            return ArrayCharAction::Skip;
        }
        if ch.is_ascii_whitespace() && !self.in_single_quote && !self.in_double_quote {
            return ArrayCharAction::SplitField;
        }
        ArrayCharAction::Append(ch)
    }
}

/// Parsed flags for `declare`/`typeset`.
#[allow(clippy::struct_excessive_bools)]
struct DeclareFlags {
    is_assoc: bool,
    is_indexed: bool,
    is_integer: bool,
    is_export: bool,
    is_readonly: bool,
    is_lower: bool,
    is_upper: bool,
    is_print: bool,
    is_nameref: bool,
    is_functions: bool,
    is_function_names: bool,
    is_trace: bool,
}

#[derive(Clone, Copy, Debug)]
enum CommandLookupKind {
    Alias,
    Function,
    Builtin,
    Utility,
    External,
    File,
}

#[derive(Clone, Debug)]
struct CommandLookup {
    kind: CommandLookupKind,
    name: String,
    detail: String,
}

fn format_command_verbose(lookup: &CommandLookup) -> String {
    match lookup.kind {
        CommandLookupKind::Alias => format!("alias {}='{}'", lookup.name, lookup.detail),
        CommandLookupKind::Function | CommandLookupKind::Builtin | CommandLookupKind::Utility => {
            lookup.name.clone()
        }
        CommandLookupKind::External => format!("{} -> {}", lookup.name, lookup.detail),
        CommandLookupKind::File => lookup.detail.clone(),
    }
}

fn format_type_lookup(lookup: &CommandLookup, type_only: bool, path_only: bool) -> String {
    if type_only {
        return match lookup.kind {
            CommandLookupKind::Alias => "alias".to_string(),
            CommandLookupKind::Function => "function".to_string(),
            CommandLookupKind::Builtin => "builtin".to_string(),
            CommandLookupKind::Utility => "utility".to_string(),
            CommandLookupKind::External => "external".to_string(),
            CommandLookupKind::File => "file".to_string(),
        };
    }
    if path_only {
        return lookup.detail.clone();
    }
    match lookup.kind {
        CommandLookupKind::Alias => {
            format!("{} is aliased to `{}`", lookup.name, lookup.detail)
        }
        CommandLookupKind::Function => format!("{} is a function", lookup.name),
        CommandLookupKind::Builtin => format!("{} is a shell builtin", lookup.name),
        CommandLookupKind::Utility => format!("{} is a shell utility", lookup.name),
        CommandLookupKind::External => {
            format!("{} is an external command ({})", lookup.name, lookup.detail)
        }
        CommandLookupKind::File => format!("{} is {}", lookup.name, lookup.detail),
    }
}

#[derive(Clone, Debug)]
struct MapfileOptions {
    strip_delimiter: bool,
    delimiter: u8,
    count: Option<usize>,
    origin: usize,
    skip: usize,
    fd: u32,
    array_name: String,
}

/// Parse declare/typeset flags from argv, returning (flags, `name_indices`).
fn parse_declare_flags(argv: &[String]) -> (DeclareFlags, Vec<usize>) {
    let mut flags = DeclareFlags {
        is_assoc: false,
        is_indexed: false,
        is_integer: false,
        is_export: false,
        is_readonly: false,
        is_lower: false,
        is_upper: false,
        is_print: false,
        is_nameref: false,
        is_functions: false,
        is_function_names: false,
        is_trace: false,
    };
    let mut names = Vec::new();

    for (i, arg) in argv[1..].iter().enumerate() {
        if arg.starts_with('-') && arg.len() > 1 {
            for ch in arg[1..].chars() {
                match ch {
                    'A' => flags.is_assoc = true,
                    'a' => flags.is_indexed = true,
                    'i' => flags.is_integer = true,
                    'x' => flags.is_export = true,
                    'r' => flags.is_readonly = true,
                    'l' => flags.is_lower = true,
                    'u' => flags.is_upper = true,
                    'p' => flags.is_print = true,
                    'n' => flags.is_nameref = true,
                    'f' => flags.is_functions = true,
                    'F' => flags.is_function_names = true,
                    't' => flags.is_trace = true,
                    _ => {}
                }
            }
        } else {
            names.push(i + 1);
        }
    }
    (flags, names)
}

impl WorkerRuntime {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: BrowserConfig::default(),
            vm: Vm::with_limits(ShellState::new(), ExecutionLimits::default()),
            fs: BackendFs::new(),
            utils: UtilRegistry::new(),
            builtins: wasmsh_builtins::BuiltinRegistry::new(),
            initialized: false,
            current_exec_io: None,
            proc_subst_out_scopes: Vec::new(),
            proc_subst_in_scopes: Vec::new(),
            functions: IndexMap::new(),
            exec: ExecState::new(),
            aliases: IndexMap::new(),
            external_handler: None,
            external_specs: IndexMap::new(),
            external_spec_handler: None,
            external_stream_handler: None,
            allow_external_streaming: false,
            network: None,
            network_policy: None,
            network_policy_state: None,
            network_is_policy_wrapped: false,
            clock: default_clock_provider(),
            monotonic_origin_ms: None,
            active_run: None,
            pending_streaming_pipeline: None,
            pending_signals: VecDeque::new(),
            last_subst_status: None,
        }
    }

    /// Register a handler for external commands (e.g. `python3` in Pyodide).
    pub fn set_external_handler(&mut self, handler: ExternalCommandHandler) {
        self.external_handler = Some(handler);
    }

    /// Register the host callback used for fixed executable registrations.
    pub fn set_external_spec_handler(&mut self, handler: ExternalCommandSpecHandler) {
        self.external_spec_handler = Some(handler);
    }

    /// Register a non-blocking external process starter for progressive runs.
    pub fn set_external_stream_handler(&mut self, handler: ExternalStreamHandler) {
        self.external_stream_handler = Some(handler);
    }

    /// Register or replace one fixed executable external command.
    pub fn register_external(
        &mut self,
        name: impl Into<String>,
        executable: impl Into<String>,
        options: ExternalCommandOptions,
    ) -> Result<(), String> {
        let spec = ExternalCommandSpec::new(name, executable, options)?;
        self.external_specs.insert(spec.name.clone(), spec);
        Ok(())
    }

    /// Remove one registered external command. Returns whether it existed.
    pub fn unregister_external(&mut self, name: &str) -> bool {
        self.external_specs.shift_remove(name).is_some()
    }

    /// Return registered external command names in registration order.
    #[must_use]
    pub fn external_command_names(&self) -> Vec<String> {
        self.external_specs.keys().cloned().collect()
    }

    /// Return registered external command specifications in registration order.
    #[must_use]
    pub fn external_command_specs(&self) -> Vec<ExternalCommandSpec> {
        self.external_specs.values().cloned().collect()
    }

    /// Register a network backend for `curl`/`wget` utilities.
    pub fn set_network_backend(&mut self, backend: Box<dyn NetworkBackend>) {
        self.network = Some(backend);
        self.network_is_policy_wrapped = false;
        self.apply_network_policy();
    }

    /// Install a validated policy before initialization. The next `Init`
    /// command remains authoritative and may replace it.
    pub fn set_network_policy(&mut self, policy: NetworkPolicy) {
        self.network_policy = Some(policy.clone());
        let state = self
            .network_policy_state
            .get_or_insert_with(|| Rc::new(RefCell::new(policy.clone())))
            .clone();
        *state.borrow_mut() = policy;
        self.apply_network_policy();
    }

    fn apply_network_policy(&mut self) {
        let Some(policy) = self.network_policy.clone() else {
            return;
        };
        let state = self
            .network_policy_state
            .get_or_insert_with(|| Rc::new(RefCell::new(policy.clone())))
            .clone();
        *state.borrow_mut() = policy;
        if self.network_is_policy_wrapped {
            return;
        }
        if let Some(inner) = self.network.take() {
            self.network = Some(Box::new(PolicyNetworkBackend {
                policy: state,
                inner,
            }));
            self.network_is_policy_wrapped = true;
        }
    }

    /// Register the session clock used by `date`, `SigV4`, `$SECONDS`, and
    /// `time`. The provider is retained until replaced or the runtime drops.
    pub fn set_clock_provider(&mut self, provider: Box<dyn ClockProvider>) {
        self.clock = Rc::from(provider);
        self.monotonic_origin_ms = None;
    }

    fn refresh_monotonic_state(&mut self) {
        let Some(now) = self.clock.monotonic_now_ms().ok() else {
            return;
        };
        let origin = *self.monotonic_origin_ms.get_or_insert(now);
        self.vm
            .state
            .set_monotonic_elapsed_ms(now.saturating_sub(origin));
    }

    fn monotonic_now_ms(&self) -> u64 {
        self.clock.monotonic_now_ms().unwrap_or(0)
    }

    /// Process a host command and return a list of events to send back.
    pub fn handle_command(&mut self, cmd: HostCommand) -> Vec<WorkerEvent> {
        match cmd {
            HostCommand::Init {
                step_budget,
                allowed_hosts,
                network_policy,
            } => self.handle_init_command(step_budget, allowed_hosts, network_policy),
            HostCommand::Run { input } => self.handle_run_command(input, true),
            HostCommand::StartRun { input } => self.handle_run_command(input, false),
            HostCommand::PollRun => self.handle_poll_run_command(),
            HostCommand::Signal { signal } => self.handle_signal_command(&signal),
            HostCommand::Cancel => {
                self.cancel_active_execution();
                vec![WorkerEvent::Diagnostic(
                    DiagnosticLevel::Info,
                    "cancel received".into(),
                )]
            }
            HostCommand::ReadFile { path } => self.handle_read_file_command(&path),
            HostCommand::WriteFile { path, data } => self.handle_write_file_command(path, &data),
            HostCommand::ListDir { path } => self.handle_list_dir_command(&path),
            HostCommand::Mount { .. } => {
                vec![WorkerEvent::Diagnostic(
                    DiagnosticLevel::Warning,
                    "mount not yet implemented".into(),
                )]
            }
            _ => vec![WorkerEvent::Diagnostic(
                DiagnosticLevel::Warning,
                "unknown command".into(),
            )],
        }
    }

    fn handle_init_command(
        &mut self,
        step_budget: u64,
        allowed_hosts: Vec<String>,
        network_policy: Option<ProtocolNetworkPolicyConfig>,
    ) -> Vec<WorkerEvent> {
        if network_policy.is_some() && !allowed_hosts.is_empty() {
            return vec![WorkerEvent::Diagnostic(
                DiagnosticLevel::Error,
                "network_policy and allowed_hosts cannot both be configured".into(),
            )];
        }
        let policy_config = network_policy.unwrap_or_else(|| ProtocolNetworkPolicyConfig {
            enabled: !allowed_hosts.is_empty(),
            default_action: wasmsh_protocol::NetworkDefaultAction::Deny,
            allow: allowed_hosts.clone(),
            deny: Vec::new(),
        });
        let policy = match NetworkPolicy::try_from_config(policy_config) {
            Ok(policy) => policy,
            Err(error) => {
                return vec![WorkerEvent::Diagnostic(
                    DiagnosticLevel::Error,
                    format!("invalid network policy: {error}"),
                )];
            }
        };
        self.network_policy = Some(policy);
        self.apply_network_policy();
        self.config.step_budget = step_budget;
        self.config.allowed_hosts = allowed_hosts;
        self.cancel_pending_streaming_pipeline();
        self.allow_external_streaming = false;
        self.vm = Vm::with_limits(
            ShellState::new(),
            ExecutionLimits {
                step_limit: step_budget,
                output_byte_limit: self.config.output_byte_limit,
                pipe_byte_limit: self.config.pipe_byte_limit,
                recursion_limit: self.config.recursion_limit,
            },
        );
        self.fs = BackendFs::new();
        self.current_exec_io = None;
        self.proc_subst_out_scopes.clear();
        self.proc_subst_in_scopes.clear();
        self.functions = IndexMap::new();
        self.exec.reset();
        self.aliases = IndexMap::new();
        self.active_run = None;
        self.pending_signals.clear();
        self.monotonic_origin_ms = self.clock.monotonic_now_ms().ok();
        self.refresh_monotonic_state();
        self.initialized = true;
        // Set default shopt options (bash defaults)
        self.vm.state.set_var("SHOPT_extglob".into(), "1".into());
        self.vm
            .state
            .set_var("SHOPT_expand_aliases".into(), "1".into());
        self.vm.state.set_var("SHOPT_sourcepath".into(), "1".into());
        self.seed_default_environment();
        vec![WorkerEvent::Version(PROTOCOL_VERSION.to_string())]
    }

    /// Seed the fixed virtual environment so common AI scripts work without
    /// inheriting anything from the host. Non-exported to keep `env` output
    /// limited to what the script explicitly exports.
    fn seed_default_environment(&mut self) {
        let defaults = [
            ("HOME", "/home/user"),
            ("PWD", "/"),
            ("PATH", "/usr/bin:/bin"),
        ];
        for (name, value) in defaults {
            if self.vm.state.get_var(name).is_none() {
                self.vm.state.set_var(name.into(), value.into());
            }
        }
        if self.vm.state.cwd.is_empty() {
            self.vm.state.cwd = "/".into();
        }
    }

    fn handle_run_command(&mut self, input: String, run_to_completion: bool) -> Vec<WorkerEvent> {
        if !self.initialized {
            return vec![WorkerEvent::Diagnostic(
                DiagnosticLevel::Error,
                "runtime not initialized".into(),
            )];
        }
        self.refresh_monotonic_state();
        match self.start_execution_with_streaming(input, !run_to_completion) {
            Ok(()) => {
                if run_to_completion {
                    self.poll_active_run_to_completion()
                } else {
                    vec![WorkerEvent::Yielded]
                }
            }
            Err(events) => events,
        }
    }

    fn handle_poll_run_command(&mut self) -> Vec<WorkerEvent> {
        self.refresh_monotonic_state();
        match self.poll_active_run() {
            Some(ExecutionPoll::Yield(mut events)) => {
                events.push(WorkerEvent::Yielded);
                events
            }
            Some(ExecutionPoll::Done(events)) => events,
            None => vec![WorkerEvent::Diagnostic(
                DiagnosticLevel::Error,
                "no active run".into(),
            )],
        }
    }

    fn handle_read_file_command(&mut self, path: &str) -> Vec<WorkerEvent> {
        use wasmsh_fs::OpenOptions;
        let handle = match self.fs.open(path, OpenOptions::read()) {
            Ok(h) => h,
            Err(e) => {
                return vec![WorkerEvent::Diagnostic(
                    DiagnosticLevel::Error,
                    format!("read error: {e}"),
                )];
            }
        };
        let result = self.fs.read_file(handle);
        self.fs.close(handle);
        match result {
            Ok(data) => vec![WorkerEvent::Stdout(data)],
            Err(e) => vec![WorkerEvent::Diagnostic(
                DiagnosticLevel::Error,
                format!("read error: {path}: {e}"),
            )],
        }
    }

    fn handle_write_file_command(&mut self, path: String, data: &[u8]) -> Vec<WorkerEvent> {
        use wasmsh_fs::OpenOptions;
        match self.fs.open(&path, OpenOptions::write()) {
            Ok(h) => {
                if let Err(e) = self.fs.write_file(h, data) {
                    self.write_stderr(format!("wasmsh: write error: {e}\n").as_bytes());
                }
                self.fs.close(h);
                vec![WorkerEvent::FsChanged(path)]
            }
            Err(e) => vec![WorkerEvent::Diagnostic(
                DiagnosticLevel::Error,
                format!("write error: {e}"),
            )],
        }
    }

    fn handle_list_dir_command(&mut self, path: &str) -> Vec<WorkerEvent> {
        match self.fs.read_dir(path) {
            Ok(entries) => {
                let names: Vec<u8> = entries
                    .iter()
                    .map(|e| e.name.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into_bytes();
                vec![WorkerEvent::Stdout(names)]
            }
            Err(e) => vec![WorkerEvent::Diagnostic(
                DiagnosticLevel::Error,
                format!("readdir error: {e}"),
            )],
        }
    }

    pub fn start_execution(&mut self, input: String) -> Result<(), Vec<WorkerEvent>> {
        self.start_execution_with_streaming(input, false)
    }

    fn start_execution_with_streaming(
        &mut self,
        input: String,
        allow_external_streaming: bool,
    ) -> Result<(), Vec<WorkerEvent>> {
        if !self.initialized {
            return Err(vec![WorkerEvent::Diagnostic(
                DiagnosticLevel::Error,
                "runtime not initialized".into(),
            )]);
        }
        if self.active_run.is_some() {
            return Err(vec![WorkerEvent::Diagnostic(
                DiagnosticLevel::Error,
                "execution already active".into(),
            )]);
        }

        let hir = match wasmsh_parse::parse(&input) {
            Ok(ast) => wasmsh_hir::lower(&ast),
            Err(e) => {
                self.vm.state.last_status = 2;
                return Err(vec![
                    WorkerEvent::Stderr(format!("wasmsh: parse error: {e}\n").into_bytes()),
                    WorkerEvent::Exit(2),
                ]);
            }
        };

        self.exec.reset();
        self.exec.exit_trap_at_run_start =
            self.vm.state.get_var("_TRAP_EXIT").map(|v| v.to_string());
        self.current_exec_io = None;
        self.proc_subst_out_scopes.clear();
        self.proc_subst_in_scopes.clear();
        self.vm.steps = 0;
        self.vm.budget.steps = 0;
        self.vm.budget.visible_output_bytes = self.vm.output_bytes;
        self.vm.budget.pipe_bytes = 0;
        self.vm.budget.recursion_depth = 0;
        self.vm.budget.clear_stop_reason();
        self.vm.cancellation_token().reset();
        self.pending_signals.clear();
        self.cancel_pending_streaming_pipeline();
        self.allow_external_streaming = allow_external_streaming;
        self.active_run = Some(ActiveRun::new(input, hir));
        Ok(())
    }

    /// Minimum per-poll step limit so that small batch sizes (e.g. `step_budget=1`
    /// for progressive yield-per-command) still allow enough internal steps for
    /// pipelines and compound commands to complete.
    const MIN_POLL_STEPS: u64 = 100;

    pub fn poll_active_run(&mut self) -> Option<ExecutionPoll> {
        let mut run = self.active_run.take()?;
        let previous_step_limit = self.vm.limits.step_limit;
        self.vm.steps = 0;
        self.vm.budget.steps = 0;
        // Keep the VM step_limit active so that loops (while/for) can enforce
        // the budget via `check_resource_limits()` on each iteration.  The
        // outer `remaining` counter governs how many top-level commands we
        // execute per poll; the VM limit catches runaway inner loops.
        self.vm.limits.step_limit = if self.config.step_budget == 0 {
            0
        } else {
            self.config.step_budget.max(Self::MIN_POLL_STEPS)
        };

        let mut remaining = if self.config.step_budget == 0 {
            usize::MAX
        } else {
            self.config.step_budget as usize
        };
        let pending_signal_events = self.drain_pending_signal_events();
        let mut finished = run.is_done();

        while !finished && remaining > 0 {
            // Check cancellation without advancing the step counter — the
            // step counter is advanced inside command/loop dispatch.
            if self.vm.cancellation_token().is_cancelled() {
                self.vm.budget.note_cancelled();
                self.exec.resource_exhausted = true;
            }
            if self.exec.exit_requested.is_some() || self.exec.resource_exhausted {
                finished = true;
                break;
            }

            let step_outcome = self.poll_active_run_step(&mut run);
            remaining -= 1;
            finished = matches!(step_outcome, ActiveRunStep::Done);
            if matches!(step_outcome, ActiveRunStep::Wait) {
                break;
            }
        }

        self.vm.limits.step_limit = previous_step_limit;

        if finished || self.exec.exit_requested.is_some() || self.exec.resource_exhausted {
            self.cancel_pending_streaming_pipeline();
            self.ensure_stop_reason();
            let ended_normally =
                self.exec.exit_requested.is_none() && !self.exec.resource_exhausted;
            let mut events = pending_signal_events;
            self.run_exit_trap_if_needed(&mut events, ended_normally);
            self.drain_io_events(&mut events);
            self.drain_diagnostic_events(&mut events);
            let exit_status = self.current_run_exit_status();
            events.push(WorkerEvent::Exit(exit_status));
            self.active_run = None;
            self.allow_external_streaming = false;
            Some(ExecutionPoll::Done(events))
        } else {
            let mut events = pending_signal_events;
            events.extend(self.drain_partial_run_events());
            self.active_run = Some(run);
            Some(ExecutionPoll::Yield(events))
        }
    }

    pub fn cancel_active_execution(&mut self) {
        self.cancel_pending_streaming_pipeline();
        self.vm.cancellation_token().cancel();
    }

    fn handle_signal_command(&mut self, signal: &str) -> Vec<WorkerEvent> {
        if !self.initialized {
            return vec![WorkerEvent::Diagnostic(
                DiagnosticLevel::Error,
                "runtime not initialized".into(),
            )];
        }

        let Some(spec) = find_runtime_signal_spec(signal) else {
            return vec![WorkerEvent::Diagnostic(
                DiagnosticLevel::Error,
                format!("unsupported signal: {signal}"),
            )];
        };

        if self.active_run.is_some() {
            self.pending_signals.push_back(spec);
            if self.signal_trap_handler(spec).is_some()
                || self.vm.state.get_var(spec.ignore_var).as_deref() == Some("1")
            {
                return Vec::new();
            }
            return match spec.default_action {
                SignalDefaultAction::Terminate => vec![WorkerEvent::Diagnostic(
                    DiagnosticLevel::Info,
                    format!("signal {} received", spec.name),
                )],
                SignalDefaultAction::Ignore => Vec::new(),
                SignalDefaultAction::StopLike => vec![WorkerEvent::Diagnostic(
                    DiagnosticLevel::Warning,
                    format!(
                        "signal {} requires job-control stop semantics and is not modeled yet",
                        spec.name
                    ),
                )],
                SignalDefaultAction::ContinueLike => vec![WorkerEvent::Diagnostic(
                    DiagnosticLevel::Info,
                    format!(
                        "signal {} has no effect without a stopped job in the current sandbox model",
                        spec.name
                    ),
                )],
            };
        }

        if let Some(handler) = self.signal_trap_handler(spec) {
            let mut events = self.run_signal_trap(spec, &handler);
            self.drain_diagnostic_events(&mut events);
            if self.exec.exit_requested.is_some() {
                events.extend(self.finish_idle_signal_exit());
            }
            return events;
        }

        if self.vm.state.get_var(spec.ignore_var).as_deref() == Some("1") {
            return Vec::new();
        }

        match spec.default_action {
            SignalDefaultAction::Terminate => {
                self.exec.exit_requested = Some(128 + spec.number);
                if self.active_run.is_some() {
                    vec![WorkerEvent::Diagnostic(
                        DiagnosticLevel::Info,
                        format!("signal {} received", spec.name),
                    )]
                } else {
                    self.finish_idle_signal_exit()
                }
            }
            SignalDefaultAction::Ignore => Vec::new(),
            SignalDefaultAction::StopLike => vec![WorkerEvent::Diagnostic(
                DiagnosticLevel::Warning,
                format!(
                    "signal {} requires job-control stop semantics and is not modeled yet",
                    spec.name
                ),
            )],
            SignalDefaultAction::ContinueLike => vec![WorkerEvent::Diagnostic(
                DiagnosticLevel::Info,
                format!(
                    "signal {} has no effect without a stopped job in the current sandbox model",
                    spec.name
                ),
            )],
        }
    }

    fn drain_pending_signal_events(&mut self) -> Vec<WorkerEvent> {
        let mut events = Vec::new();
        while let Some(spec) = self.pending_signals.pop_front() {
            if let Some(handler) = self.signal_trap_handler(spec) {
                events.extend(self.run_signal_trap(spec, &handler));
                self.drain_diagnostic_events(&mut events);
            } else if self.vm.state.get_var(spec.ignore_var).as_deref() == Some("1") {
                continue;
            } else {
                match spec.default_action {
                    SignalDefaultAction::Terminate => {
                        self.exec.exit_requested = Some(128 + spec.number);
                    }
                    SignalDefaultAction::Ignore => {}
                    SignalDefaultAction::StopLike => events.push(WorkerEvent::Diagnostic(
                        DiagnosticLevel::Warning,
                        format!(
                            "signal {} requires job-control stop semantics and is not modeled yet",
                            spec.name
                        ),
                    )),
                    SignalDefaultAction::ContinueLike => events.push(WorkerEvent::Diagnostic(
                        DiagnosticLevel::Info,
                        format!(
                            "signal {} has no effect without a stopped job in the current sandbox model",
                            spec.name
                        ),
                    )),
                }
            }

            if self.exec.exit_requested.is_some() || self.exec.resource_exhausted {
                break;
            }
        }
        events
    }

    fn finish_idle_signal_exit(&mut self) -> Vec<WorkerEvent> {
        let mut events = Vec::new();
        // A signal is an abnormal termination, so the EXIT trap always fires.
        self.run_exit_trap_if_needed(&mut events, false);
        self.drain_io_events(&mut events);
        self.drain_diagnostic_events(&mut events);
        let exit_status = self.current_run_exit_status();
        events.push(WorkerEvent::Exit(exit_status));
        self.exec.reset();
        events
    }

    fn poll_active_run_to_completion(&mut self) -> Vec<WorkerEvent> {
        let mut events = Vec::new();
        while let Some(poll) = self.poll_active_run() {
            match poll {
                ExecutionPoll::Yield(mut batch) => {
                    events.append(&mut batch);
                }
                ExecutionPoll::Done(mut batch) => {
                    events.append(&mut batch);
                    break;
                }
            }
        }
        events
    }

    fn poll_active_run_step(&mut self, run: &mut ActiveRun) -> ActiveRunStep {
        if run.is_done() || self.exec.exit_requested.is_some() || self.exec.resource_exhausted {
            return ActiveRunStep::Done;
        }

        if self.pending_streaming_pipeline.is_some() {
            if self.poll_pending_streaming_pipeline() {
                self.finish_pending_streaming_pipeline();
                let and_or = run.hir.items[run.complete_index].list[run.and_or_index].clone();
                self.handle_post_and_or(&and_or);
                Self::advance_active_run_after_and_or(run);
                return if run.is_done()
                    || self.exec.exit_requested.is_some()
                    || self.exec.resource_exhausted
                {
                    ActiveRunStep::Done
                } else {
                    ActiveRunStep::Pending
                };
            }
            return ActiveRunStep::Wait;
        }

        let cc = &run.hir.items[run.complete_index];
        if run.and_or_index == 0 {
            self.vm.state.lineno = Self::line_number_for_offset(&run.input, cc.span.start as usize);
            self.maybe_write_verbose_input(&run.input, cc);
        }
        if self.is_set_option_enabled('n') {
            run.complete_index += 1;
            run.and_or_index = 0;
            return if run.is_done()
                || self.exec.exit_requested.is_some()
                || self.exec.resource_exhausted
            {
                ActiveRunStep::Done
            } else {
                ActiveRunStep::Pending
            };
        }
        let and_or = cc.list[run.and_or_index].clone();
        if self.try_start_pending_streaming_pipeline(&and_or) {
            if self.poll_pending_streaming_pipeline() {
                self.finish_pending_streaming_pipeline();
                self.handle_post_and_or(&and_or);
                Self::advance_active_run_after_and_or(run);
            } else {
                return ActiveRunStep::Wait;
            }
        } else {
            self.execute_and_or(&and_or);
            self.handle_post_and_or(&and_or);
            Self::advance_active_run_after_and_or(run);
        }

        if run.is_done() || self.exec.exit_requested.is_some() || self.exec.resource_exhausted {
            ActiveRunStep::Done
        } else {
            ActiveRunStep::Pending
        }
    }

    fn advance_active_run_after_and_or(run: &mut ActiveRun) {
        run.and_or_index += 1;
        if run.and_or_index >= run.hir.items[run.complete_index].list.len() {
            run.complete_index += 1;
            run.and_or_index = 0;
        }
    }

    fn try_start_pending_streaming_pipeline(&mut self, and_or: &HirAndOr) -> bool {
        if !self.allow_external_streaming || self.external_stream_handler.is_none() {
            return false;
        }
        let pipeline = &and_or.first;
        let (stages, stage_last_args) = self.compile_pipeline_stages(&pipeline.commands, true);
        if !stages
            .iter()
            .any(|stage| matches!(stage, StreamingPipelineStage::External(_)))
        {
            return false;
        }
        let stage_statuses = Self::seed_stage_statuses(&stages);
        let stage_stderr: Vec<Rc<RefCell<Vec<u8>>>> = stages
            .iter()
            .map(|_| Rc::new(RefCell::new(Vec::new())))
            .collect();
        let stage_pipe_stderr: Vec<bool> = (0..stages.len())
            .map(|idx| pipeline.pipe_stderr.get(idx).copied().unwrap_or(false))
            .collect();
        let last_arg = stage_last_args.iter().rev().flatten().next().cloned();
        let Some(mut pending) = self.build_pending_streaming_pipeline(
            None,
            &stages,
            stage_pipe_stderr,
            stage_statuses,
            stage_stderr,
        ) else {
            return false;
        };
        pending.pipefail = self.vm.state.get_var("SHOPT_o_pipefail").as_deref() == Some("1");
        pending.negated = pipeline.negated;
        pending.timed = pipeline.timed;
        pending.time_posix = pipeline.time_posix;
        pending.started_ms = self.monotonic_now_ms();
        pending.deadline_ms = self.streaming_pipeline_deadline(&stages, pending.started_ms);
        pending.last_arg = last_arg;
        self.pending_streaming_pipeline = Some(pending);
        true
    }

    fn build_pending_streaming_pipeline(
        &mut self,
        source_reader: Option<Box<dyn Read>>,
        stages: &[StreamingPipelineStage],
        stage_pipe_stderr: Vec<bool>,
        stage_statuses: Vec<Rc<RefCell<i32>>>,
        stage_stderr: Vec<Rc<RefCell<Vec<u8>>>>,
    ) -> Option<PendingStreamingPipeline> {
        let output_pipes: Vec<Rc<RefCell<PipeBuffer>>> = (0..stages.len())
            .map(|_| Rc::new(RefCell::new(PipeBuffer::new(EXTERNAL_STREAM_PIPE_CAPACITY))))
            .collect();
        let ctx = StreamingStageCtx {
            stages,
            stage_pipe_stderr: &stage_pipe_stderr,
            stage_statuses: &stage_statuses,
            stage_stderr: &stage_stderr,
            output_pipes: &output_pipes,
        };
        let mut processes = Vec::new();
        if self
            .setup_first_streaming_process(source_reader, &ctx, &mut processes)
            .is_some()
        {
            Self::close_streaming_processes(&mut processes, self);
            return None;
        }
        for idx in 1..stages.len() {
            if !self.setup_later_streaming_stage(idx, &ctx, &mut processes) {
                Self::close_streaming_processes(&mut processes, self);
                return None;
            }
        }
        let final_pipe = output_pipes.last().cloned()?;
        let process_count = processes.len();
        Some(PendingStreamingPipeline {
            processes,
            finished: vec![false; process_count],
            output_pipes,
            final_pipe,
            stage_statuses,
            stage_stderr_offsets: vec![0; stage_stderr.len()],
            stage_stderr,
            stage_pipe_stderr,
            pipefail: false,
            negated: false,
            timed: false,
            time_posix: false,
            started_ms: 0,
            deadline_ms: 0,
            last_arg: None,
        })
    }

    /// Derive the pipeline-wide wall-clock deadline from the registered
    /// external specs. A custom host may ignore the per-command `timeout_ms`,
    /// so the runtime enforces its own bound to guarantee forward progress.
    fn streaming_pipeline_deadline(&self, stages: &[StreamingPipelineStage], now_ms: u64) -> u64 {
        let mut limit: Option<u64> = None;
        for stage in stages {
            let StreamingPipelineStage::External(argv) = stage else {
                continue;
            };
            let Some(spec) = argv.first().and_then(|name| self.external_specs.get(name)) else {
                continue;
            };
            let timeout = spec.options.timeout_ms;
            if timeout == 0 {
                continue;
            }
            limit = Some(limit.map_or(timeout, |current| current.min(timeout)));
        }
        limit.map_or(0, |timeout| now_ms.saturating_add(timeout))
    }

    /// Close every process in a partially built pipeline so an aborted
    /// construction cannot leak host children.
    fn close_streaming_processes(
        processes: &mut [StreamingPipeProcess<'static>],
        runtime: &mut WorkerRuntime,
    ) {
        for process in processes.iter_mut() {
            process.close(runtime);
        }
    }

    fn poll_pending_streaming_pipeline(&mut self) -> bool {
        let Some(mut pending) = self.pending_streaming_pipeline.take() else {
            return true;
        };
        if pending.deadline_ms != 0 && self.monotonic_now_ms() >= pending.deadline_ms {
            self.apply_streaming_deadline(&mut pending);
            self.pending_streaming_pipeline = Some(pending);
            return true;
        }
        let mut progressed = false;
        for idx in (0..pending.processes.len()).rev() {
            if pending.finished[idx] {
                continue;
            }
            match pending.processes[idx].poll(self) {
                PipeProcessPoll::Ready => progressed = true,
                PipeProcessPoll::PendingRead | PipeProcessPoll::PendingWrite => {}
                PipeProcessPoll::Exited => {
                    pending.finished[idx] = true;
                    progressed = true;
                }
            }
        }
        let buffered_pipe_bytes = pending
            .output_pipes
            .iter()
            .map(|pipe| pipe.borrow().len() as u64)
            .sum();
        self.sync_pipe_budget(buffered_pipe_bytes);
        if !self.exec.resource_exhausted {
            self.drain_final_pipe_to_stdout(&pending.final_pipe, &mut progressed);
            self.drain_pending_streaming_stderr(&mut pending);
        }
        let finished = pending.finished.iter().all(|done| *done);
        if self.exec.resource_exhausted || finished {
            for (idx, process) in pending.processes.iter_mut().enumerate() {
                if !pending.finished[idx] {
                    process.close(self);
                    pending.finished[idx] = true;
                }
            }
            self.pending_streaming_pipeline = Some(pending);
            true
        } else {
            self.pending_streaming_pipeline = Some(pending);
            let _ = progressed;
            false
        }
    }

    /// Stop every remaining process with status 124 (timeout) and record the
    /// failure on the external stages so `$?`/PIPESTATUS observe it.
    fn apply_streaming_deadline(&mut self, pending: &mut PendingStreamingPipeline) {
        for (idx, process) in pending.processes.iter_mut().enumerate() {
            if pending.finished[idx] {
                continue;
            }
            if let StreamingPipeProcess::External(external) = process {
                if external.process.is_some() {
                    *external.stage_status.borrow_mut() = 124;
                    let diagnostic = format!(
                        "wasmsh: {}: external command timed out\n",
                        external.command_name()
                    );
                    // Surface the diagnostic on the stage's stderr channel;
                    // the process is being torn down, so a merged pipe could
                    // never be flushed anyway.
                    external
                        .stage_stderr
                        .borrow_mut()
                        .extend_from_slice(diagnostic.as_bytes());
                }
            }
            process.close(self);
            pending.finished[idx] = true;
        }
    }

    fn drain_pending_streaming_stderr(&mut self, pending: &mut PendingStreamingPipeline) {
        for idx in 0..pending.stage_stderr.len() {
            if pending.stage_pipe_stderr[idx] {
                continue;
            }
            let start = pending.stage_stderr_offsets[idx];
            let data = {
                let stderr = pending.stage_stderr[idx].borrow();
                if start >= stderr.len() {
                    Vec::new()
                } else {
                    stderr[start..].to_vec()
                }
            };
            if !data.is_empty() {
                pending.stage_stderr_offsets[idx] += data.len();
                self.write_stderr(&data);
            }
        }
    }

    fn finish_pending_streaming_pipeline(&mut self) {
        let Some(mut pending) = self.pending_streaming_pipeline.take() else {
            return;
        };
        for process in &mut pending.processes {
            process.close(self);
        }
        self.drain_pending_streaming_stderr(&mut pending);
        let statuses: Vec<i32> = pending
            .stage_statuses
            .iter()
            .map(|status| *status.borrow())
            .collect();
        if let Some(last_arg) = pending.last_arg {
            self.vm.state.set_last_argument(last_arg);
        }
        self.set_pipestatus(&statuses);
        if !self.exec.resource_exhausted {
            self.vm.state.last_status =
                Self::resolve_pipeline_exit_status(&statuses, pending.pipefail);
            if pending.negated {
                self.vm.state.last_status = i32::from(self.vm.state.last_status == 0);
            }
            if pending.timed {
                let elapsed_seconds =
                    self.monotonic_now_ms().saturating_sub(pending.started_ms) as f64 / 1000.0;
                self.emit_pipeline_timing(pending.time_posix, elapsed_seconds);
            }
        }
    }

    fn cancel_pending_streaming_pipeline(&mut self) {
        let Some(mut pending) = self.pending_streaming_pipeline.take() else {
            return;
        };
        for process in &mut pending.processes {
            process.close(self);
        }
        for pipe in pending.output_pipes {
            let mut pipe = pipe.borrow_mut();
            pipe.close_read();
            pipe.close_write();
        }
    }

    fn drain_partial_run_events(&mut self) -> Vec<WorkerEvent> {
        let mut events = Vec::new();
        self.drain_io_events(&mut events);
        self.drain_diagnostic_events(&mut events);
        events
    }

    fn current_run_exit_status(&self) -> i32 {
        if self.exec.resource_exhausted {
            match self.exec.stop_reason.as_ref() {
                Some(StopReason::Cancelled) => 130,
                _ => 128,
            }
        } else {
            self.exec
                .exit_requested
                .unwrap_or(self.vm.state.last_status)
        }
    }

    fn mark_stop_reason(&mut self, reason: StopReason) {
        self.exec.resource_exhausted = true;
        self.exec.stop_reason = Some(reason);
    }

    fn mark_budget_exhaustion(&mut self, reason: ExhaustionReason) {
        self.mark_stop_reason(StopReason::Exhausted(reason));
    }

    /// Record that a recursion-depth limit was hit for the *current command*.
    ///
    /// Unlike a step-output/step-budget exhaustion this must not abort the
    /// whole run: recursion is inherently recoverable (the stack is bounded by
    /// the call itself), and the sandbox contract is that a runaway recursion
    /// in one command fails that command while later commands still run
    /// (`f(){ f; }; f || true; echo after`). The structured reason is still
    /// recorded for observability, and the command reports status 128.
    fn mark_recursion_exhaustion(&mut self, reason: ExhaustionReason) {
        self.exec.stop_reason = Some(StopReason::Exhausted(reason));
        self.vm.state.last_status = 128;
    }

    fn ensure_stop_reason(&mut self) {
        if !self.exec.resource_exhausted || self.exec.stop_reason.is_some() {
            return;
        }
        if self.vm.cancellation_token().is_cancelled() {
            self.mark_stop_reason(StopReason::Cancelled);
            return;
        }
        if let Some(reason) = self.vm.stop_reason().cloned() {
            self.mark_stop_reason(reason);
            return;
        }
        let limit = self.vm.limits.output_byte_limit;
        if limit > 0 && self.vm.output_bytes > limit {
            self.mark_budget_exhaustion(ExhaustionReason {
                category: BudgetCategory::VisibleOutputBytes,
                used: self.vm.output_bytes,
                limit,
            });
        }
    }

    fn sync_pipe_budget(&mut self, used: u64) {
        if self.exec.resource_exhausted {
            return;
        }
        let limit = self.vm.limits.pipe_byte_limit;
        if let Err(reason) = self.vm.budget.set_pipe_bytes(used, limit) {
            self.mark_budget_exhaustion(reason.clone());
            self.vm.emit_diagnostic(
                wasmsh_vm::DiagLevel::Error,
                wasmsh_vm::DiagCategory::Budget,
                reason.diagnostic_message(),
            );
        }
    }

    pub fn set_output_byte_limit(&mut self, limit: u64) {
        self.config.output_byte_limit = limit;
        self.vm.limits.output_byte_limit = limit;
    }

    pub fn set_pipe_byte_limit(&mut self, limit: u64) {
        self.config.pipe_byte_limit = limit;
        self.vm.limits.pipe_byte_limit = limit;
    }

    /// Set the finite stdin limit for legacy external handlers.
    pub fn set_external_input_byte_limit(&mut self, limit: u64) {
        self.config.external_input_byte_limit = limit.clamp(1, MAX_EXTERNAL_BUFFER_BYTES);
    }

    /// Set the finite combined stdout/stderr limit for legacy external handlers.
    pub fn set_external_output_byte_limit(&mut self, limit: u64) {
        self.config.external_output_byte_limit = limit.clamp(1, MAX_EXTERNAL_BUFFER_BYTES);
    }

    pub fn set_recursion_limit(&mut self, limit: u32) {
        self.config.recursion_limit = limit;
        self.vm.limits.recursion_limit = limit;
    }

    pub fn set_vm_subset_enabled(&mut self, enabled: bool) {
        self.config.vm_subset_enabled = enabled;
    }

    fn execute_and_or(&mut self, and_or: &HirAndOr) {
        if let Ok(program) = self.lower_vm_subset_and_or(and_or) {
            self.run_debug_trap_if_needed();
            self.execute_ir_program(&program);
            return;
        }
        self.execute_pipeline_chain(and_or);
    }

    fn execute_ir_program(&mut self, program: &IrProgram) {
        let mut executor = RuntimeVmExecutor {
            fs: &mut self.fs,
            builtins: &self.builtins,
            current_exec_io: &mut self.current_exec_io,
            proc_subst_out_scopes: &mut self.proc_subst_out_scopes,
            exec: &mut self.exec,
        };
        let _ = self.vm.run_with_executor(program, &mut executor);
    }

    fn lower_vm_subset_and_or(
        &self,
        and_or: &HirAndOr,
    ) -> Result<IrProgram, VmSubsetFallbackReason> {
        if !self.config.vm_subset_enabled {
            return Err(VmSubsetFallbackReason::Disabled);
        }

        self.validate_vm_subset_and_or(and_or)?;
        lower_supported_and_or(and_or).map_err(VmSubsetFallbackReason::Lowering)
    }

    fn validate_vm_subset_and_or(&self, and_or: &HirAndOr) -> Result<(), VmSubsetFallbackReason> {
        self.validate_vm_subset_pipeline(&and_or.first)?;
        for (_, pipeline) in &and_or.rest {
            self.validate_vm_subset_pipeline(pipeline)?;
        }
        Ok(())
    }

    fn validate_vm_subset_pipeline(
        &self,
        pipeline: &HirPipeline,
    ) -> Result<(), VmSubsetFallbackReason> {
        if pipeline.timed || pipeline.time_posix || pipeline.negated || pipeline.commands.len() != 1
        {
            return Err(VmSubsetFallbackReason::Lowering(
                LoweringError::Unsupported("pipeline shape is outside the VM subset"),
            ));
        }
        self.validate_vm_subset_command(&pipeline.commands[0])
    }

    fn validate_vm_subset_command(&self, cmd: &HirCommand) -> Result<(), VmSubsetFallbackReason> {
        match cmd {
            HirCommand::Assign(node) => Self::validate_vm_subset_assign(node),
            HirCommand::Exec(node) => self.validate_vm_subset_exec(node),
            _ => Err(VmSubsetFallbackReason::Lowering(
                LoweringError::Unsupported("command kind is outside the VM subset"),
            )),
        }
    }

    fn validate_vm_subset_assign(
        node: &wasmsh_hir::HirAssign,
    ) -> Result<(), VmSubsetFallbackReason> {
        if !node.redirections.is_empty()
            || node
                .assignments
                .iter()
                .any(|a| !Self::vm_supported_assignment_name(&a.name))
            || node
                .assignments
                .iter()
                .filter_map(|a| a.value.as_ref())
                .any(|word| !Self::vm_supported_word(word))
        {
            return Err(VmSubsetFallbackReason::AssignmentShape);
        }
        Ok(())
    }

    fn validate_vm_subset_exec(
        &self,
        node: &wasmsh_hir::HirExec,
    ) -> Result<(), VmSubsetFallbackReason> {
        if !node.env.is_empty() {
            return Err(VmSubsetFallbackReason::CommandEnvPrefixes);
        }
        if node.argv.is_empty() || node.argv.iter().any(|word| !Self::vm_supported_word(word)) {
            return Err(VmSubsetFallbackReason::UnsupportedWord);
        }
        if node
            .redirections
            .iter()
            .any(|redir| !Self::vm_supported_redirection(redir))
        {
            return Err(VmSubsetFallbackReason::UnsupportedRedirection);
        }
        if self.vm.state.get_var("SHOPT_x").as_deref() == Some("1")
            || node
                .argv
                .iter()
                .any(Self::vm_word_requires_full_shell_execution)
        {
            return Err(VmSubsetFallbackReason::ShellExpansion);
        }
        let Some(name) = Self::literal_word_text(&node.argv[0]) else {
            return Err(VmSubsetFallbackReason::UnsupportedWord);
        };
        if self.get_shopt_value("expand_aliases") && self.aliases.contains_key(name.as_str()) {
            return Err(VmSubsetFallbackReason::AliasExpansion);
        }
        let argv = vec![name.to_string()];
        if !matches!(
            self.resolve_command(name.as_str(), &argv),
            ResolvedCommand::Builtin(_)
        ) {
            return Err(VmSubsetFallbackReason::NonBuiltinCommand);
        }
        Ok(())
    }

    fn vm_supported_assignment_name(name: &smol_str::SmolStr) -> bool {
        !name.as_str().contains('[') && !name.as_str().ends_with('+')
    }

    fn vm_supported_redirection(redirection: &HirRedirection) -> bool {
        matches!(
            redirection.op,
            RedirectionOp::Output | RedirectionOp::Append
        ) && redirection.fd.unwrap_or(1) == 1
            && redirection.here_doc_body.is_none()
            && Self::vm_supported_word(&redirection.target)
    }

    fn vm_supported_word(word: &Word) -> bool {
        word.parts.iter().all(Self::vm_supported_word_part)
    }

    fn vm_word_requires_full_shell_execution(word: &Word) -> bool {
        word.parts
            .iter()
            .any(Self::vm_word_part_requires_full_shell_execution)
    }

    fn vm_word_part_requires_full_shell_execution(part: &WordPart) -> bool {
        match part {
            WordPart::Literal(text) => Self::text_has_brace_or_glob_literal(text),
            WordPart::SingleQuoted(_)
            | WordPart::DoubleQuoted(_)
            | WordPart::Parameter(_)
            | WordPart::Arithmetic(_) => false,
            WordPart::CommandSubstitution(_)
            | WordPart::ProcessSubstIn(_)
            | WordPart::ProcessSubstOut(_)
            | _ => true,
        }
    }

    fn vm_supported_word_part(part: &WordPart) -> bool {
        match part {
            WordPart::Literal(_) | WordPart::SingleQuoted(_) | WordPart::Parameter(_) => true,
            // The VM's arithmetic evaluator has no runtime access, so an
            // arithmetic expression containing a command substitution must be
            // handled by the full interpreter (which resolves it first).
            WordPart::Arithmetic(expr) => !expr.contains("$(") && !expr.contains('`'),
            WordPart::DoubleQuoted(parts) => parts.iter().all(Self::vm_supported_word_part),
            WordPart::CommandSubstitution(_)
            | WordPart::ProcessSubstIn(_)
            | WordPart::ProcessSubstOut(_)
            | _ => false,
        }
    }

    fn literal_word_text(word: &Word) -> Option<smol_str::SmolStr> {
        fn append_literal(part: &WordPart, out: &mut String) -> Option<()> {
            match part {
                WordPart::Literal(text) | WordPart::SingleQuoted(text) => {
                    out.push_str(text);
                    Some(())
                }
                WordPart::DoubleQuoted(parts) => {
                    for part in parts {
                        append_literal(part, out)?;
                    }
                    Some(())
                }
                _ => None,
            }
        }

        let mut text = String::new();
        for part in &word.parts {
            append_literal(part, &mut text)?;
        }
        Some(text.into())
    }

    fn line_number_for_offset(input: &str, offset: usize) -> u32 {
        input
            .as_bytes()
            .iter()
            .take(offset)
            .filter(|&&b| b == b'\n')
            .count() as u32
            + 1
    }

    /// Execute input and return collected events (used by eval/source).
    fn execute_input_inner(&mut self, input: &str) -> Vec<WorkerEvent> {
        self.exec.recursion_depth += 1;
        if let Err(reason) = self
            .vm
            .budget
            .enter_recursion(self.vm.limits.recursion_limit)
        {
            self.exec.recursion_depth -= 1;
            self.mark_recursion_exhaustion(reason);
            return vec![WorkerEvent::Stderr(
                b"wasmsh: maximum recursion depth exceeded\n".to_vec(),
            )];
        }
        let result = self.execute_input_inner_impl(input);
        self.exec.recursion_depth -= 1;
        self.vm.budget.exit_recursion();
        result
    }

    /// Inner implementation of `execute_input_inner` (after recursion check).
    fn execute_input_inner_impl(&mut self, input: &str) -> Vec<WorkerEvent> {
        let ast = match wasmsh_parse::parse(input) {
            Ok(ast) => ast,
            Err(e) => {
                self.vm.state.last_status = 2;
                return vec![WorkerEvent::Stderr(
                    format!("wasmsh: parse error: {e}\n").into_bytes(),
                )];
            }
        };
        let hir = wasmsh_hir::lower(&ast);
        for cc in &hir.items {
            if self.exec.exit_requested.is_some() {
                break;
            }
            // Update $LINENO from span position
            let line = input
                .as_bytes()
                .iter()
                .take(cc.span.start as usize)
                .filter(|&&b| b == b'\n')
                .count() as u32
                + 1;
            self.vm.state.lineno = line;
            self.maybe_write_verbose_input(input, cc);
            if self.is_set_option_enabled('n') {
                continue;
            }
            self.execute_complete_command(cc);
        }
        // Drain stdout/stderr into events
        let mut events = Vec::new();
        if !self.vm.stdout.is_empty() {
            events.push(WorkerEvent::Stdout(std::mem::take(&mut self.vm.stdout)));
        }
        if !self.vm.stderr.is_empty() {
            events.push(WorkerEvent::Stderr(std::mem::take(&mut self.vm.stderr)));
        }
        events
    }

    /// Run the `EXIT` trap. `script_ended_normally` is true when the top-level
    /// script simply ran off its end rather than calling `exit`/being
    /// signalled. In that case the trap only fires if it was installed or
    /// changed during this run, so a persistent runtime session does not
    /// re-fire an inherited trap at the end of every `Run`.
    fn run_exit_trap_if_needed(
        &mut self,
        events: &mut Vec<WorkerEvent>,
        script_ended_normally: bool,
    ) {
        if self.exec.trap_depth > 0 {
            return;
        }
        let Some(handler_str) = self.trap_handler("_TRAP_EXIT", "_TRAP_IGNORE_EXIT") else {
            return;
        };
        if script_ended_normally
            && self.exec.exit_trap_at_run_start.as_deref() == Some(handler_str.as_str())
        {
            return;
        }
        // The trap runs on explicit `exit` and on normal end-of-script.
        let exit_code = self
            .exec
            .exit_requested
            .unwrap_or(self.vm.state.last_status);
        self.exec.trap_depth += 1;
        self.exec.exit_requested = None;
        self.vm.state.last_status = exit_code;
        events.extend(self.execute_input_inner(&handler_str));
        self.exec.trap_depth -= 1;
        if self.exec.exit_requested.is_none() {
            self.exec.exit_requested = Some(exit_code);
        }
        self.vm.state.last_status = self.exec.exit_requested.unwrap_or(exit_code);
    }

    fn handle_post_and_or(&mut self, and_or: &HirAndOr) {
        self.run_err_trap_if_needed(and_or);
        if self.should_errexit(and_or) {
            self.exec.exit_requested = Some(self.vm.state.last_status);
        }
    }

    fn should_run_err_trap(&self, and_or: &HirAndOr) -> bool {
        !self.exec.errexit_suppressed
            && (self.exec.nested_shell_depth == 0 || self.is_set_option_enabled('E'))
            && and_or.rest.is_empty()
            && !and_or.first.negated
            && self.vm.state.last_status != 0
            && self.exec.exit_requested.is_none()
            && self.exec.trap_depth == 0
    }

    fn run_err_trap_if_needed(&mut self, and_or: &HirAndOr) {
        if !self.should_run_err_trap(and_or) {
            return;
        }
        self.run_trap_and_merge(
            "_TRAP_ERR",
            "_TRAP_IGNORE_ERR",
            self.vm.state.last_status,
            true,
        );
    }

    fn run_debug_trap_if_needed(&mut self) {
        if self.exec.trap_depth > 0
            || self.exec.resource_exhausted
            || (self.exec.nested_shell_depth > 0 && !self.is_set_option_enabled('T'))
        {
            return;
        }
        self.run_trap_and_merge(
            "_TRAP_DEBUG",
            "_TRAP_IGNORE_DEBUG",
            self.vm.state.last_status,
            true,
        );
    }

    fn run_return_trap_if_needed(&mut self) {
        if self.exec.trap_depth > 0
            || self.exec.resource_exhausted
            || (self.exec.nested_shell_depth > 0 && !self.is_set_option_enabled('T'))
        {
            return;
        }
        self.run_trap_and_merge(
            "_TRAP_RETURN",
            "_TRAP_IGNORE_RETURN",
            self.vm.state.last_status,
            true,
        );
    }

    fn run_trap_and_merge(
        &mut self,
        handler_var: &str,
        ignore_var: &str,
        trigger_status: i32,
        restore_status: bool,
    ) {
        let Some(handler) = self.trap_handler(handler_var, ignore_var) else {
            return;
        };
        let saved_status = self.vm.state.last_status;
        let saved_exit_requested = self.exec.exit_requested;
        self.exec.trap_depth += 1;
        self.vm.state.last_status = trigger_status;
        let events = self.execute_input_inner(&handler);
        self.exec.trap_depth -= 1;
        self.merge_sub_events_with_diagnostics(events);
        if restore_status
            && !self.exec.resource_exhausted
            && self.exec.exit_requested == saved_exit_requested
        {
            self.vm.state.last_status = saved_status;
        }
    }

    fn trap_handler(&self, handler_var: &str, ignore_var: &str) -> Option<String> {
        if self.exec.trap_depth > 0 || self.vm.state.get_var(ignore_var).as_deref() == Some("1") {
            return None;
        }
        let handler = self.vm.state.get_var(handler_var)?;
        if handler.is_empty() {
            return None;
        }
        Some(handler.to_string())
    }

    fn signal_trap_handler(&self, spec: &RuntimeSignalSpec) -> Option<String> {
        if !spec.trappable {
            return None;
        }
        self.trap_handler(spec.handler_var, spec.ignore_var)
    }

    fn run_signal_trap(&mut self, spec: &RuntimeSignalSpec, handler: &str) -> Vec<WorkerEvent> {
        let saved_status = self.vm.state.last_status;
        let saved_exit_requested = self.exec.exit_requested;
        let saved_exec_io = self.current_exec_io.take();
        let saved_output_captures = std::mem::take(&mut self.exec.output_captures);
        self.exec.trap_depth += 1;
        self.vm.state.last_status = 128 + spec.number;
        let events = self.execute_input_inner(handler);
        self.exec.trap_depth -= 1;
        self.current_exec_io = saved_exec_io;
        self.exec.output_captures = saved_output_captures;
        if !self.exec.resource_exhausted && self.exec.exit_requested == saved_exit_requested {
            self.vm.state.last_status = saved_status;
        }
        events
    }

    fn with_nested_shell_scope<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.exec.nested_shell_depth += 1;
        let out = f(self);
        self.exec.nested_shell_depth -= 1;
        out
    }

    fn drain_io_events(&mut self, events: &mut Vec<WorkerEvent>) {
        self.push_buffer_event(events, true);
        self.push_buffer_event(events, false);
    }

    fn push_buffer_event(&mut self, events: &mut Vec<WorkerEvent>, stdout: bool) {
        let buffer = if stdout {
            &mut self.vm.stdout
        } else {
            &mut self.vm.stderr
        };
        if buffer.is_empty() {
            return;
        }

        let data = std::mem::take(buffer);
        events.push(if stdout {
            WorkerEvent::Stdout(data)
        } else {
            WorkerEvent::Stderr(data)
        });
    }

    fn push_output_capture(&mut self, capture_stdout: bool, capture_stderr: bool) {
        self.exec.output_captures.push(OutputCapture {
            capture_stdout,
            capture_stderr,
            ..OutputCapture::default()
        });
    }

    fn pop_output_capture(&mut self) -> CapturedOutput {
        let capture = self
            .exec
            .output_captures
            .pop()
            .expect("output capture stack underflow");
        CapturedOutput {
            stdout: capture.stdout,
            stderr: capture.stderr,
        }
    }

    fn with_output_capture<T>(
        &mut self,
        capture_stdout: bool,
        capture_stderr: bool,
        f: impl FnOnce(&mut Self) -> T,
    ) -> (T, CapturedOutput) {
        self.push_output_capture(capture_stdout, capture_stderr);
        let result = f(self);
        let captured = self.pop_output_capture();
        (result, captured)
    }

    fn with_exec_io_scope<T>(
        &mut self,
        exec_io: Option<ExecIo>,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        if let Some(exec_io) = exec_io {
            let saved = self.current_exec_io.replace(exec_io);
            let result = f(self);
            let current = self.current_exec_io.take();
            self.current_exec_io = match (saved, current) {
                (Some(mut saved), Some(mut current)) => {
                    let stdin = current.take_stdin();
                    saved.fds_mut().set_input(stdin);
                    Some(saved)
                }
                (saved, _) => saved,
            };
            result
        } else {
            f(self)
        }
    }

    fn append_visible_output_direct(&mut self, data: &[u8], stdout: bool) {
        if stdout {
            self.vm.stdout.extend_from_slice(data);
        } else {
            self.vm.stderr.extend_from_slice(data);
        }
    }

    fn write_output_destination_direct(&mut self, destination: &OutputTarget, data: &[u8]) -> bool {
        match destination {
            OutputTarget::InheritStdout => {
                self.append_visible_output_direct(data, true);
                true
            }
            OutputTarget::InheritStderr => {
                self.append_visible_output_direct(data, false);
                true
            }
            OutputTarget::File { path, sink, .. } => {
                if let Err(err) = sink.borrow_mut().write(data) {
                    let msg = format!("wasmsh: write error: {err}\n");
                    self.emit_visible_stderr_direct(msg.as_bytes());
                    self.vm.diagnostics.push(wasmsh_vm::DiagnosticEvent {
                        level: wasmsh_vm::DiagLevel::Error,
                        category: wasmsh_vm::DiagCategory::Filesystem,
                        message: format!("write failed for {path}: {err}"),
                    });
                }
                false
            }
            OutputTarget::ProcessSubst { path } => {
                if let Some(sink) = self.process_subst_out_sink_mut(path) {
                    sink.write(data);
                } else {
                    let msg = format!("wasmsh: {path}: process substitution sink not found\n");
                    self.emit_visible_stderr_direct(msg.as_bytes());
                }
                false
            }
            OutputTarget::Pipe(pipe) => {
                pipe.borrow_mut().write_all(data);
                false
            }
            OutputTarget::Closed => false,
        }
    }

    fn emit_visible_stderr_direct(&mut self, data: &[u8]) {
        self.append_visible_output_direct(data, false);
        self.account_output(data.len());
    }

    fn route_output(&mut self, data: &[u8], stdout: bool) -> bool {
        let mut routed_stdout = stdout;
        if let Some(exec_io) = self.current_exec_io.as_ref() {
            let destination = exec_io.output_target(stdout);
            match destination {
                OutputTarget::InheritStdout => {
                    routed_stdout = true;
                }
                OutputTarget::InheritStderr => {
                    routed_stdout = false;
                }
                OutputTarget::File { .. }
                | OutputTarget::ProcessSubst { .. }
                | OutputTarget::Pipe(_)
                | OutputTarget::Closed => {
                    return self.write_output_destination_direct(&destination, data);
                }
            }
        }

        for capture in self.exec.output_captures.iter_mut().rev() {
            let should_capture = if routed_stdout {
                capture.capture_stdout
            } else {
                capture.capture_stderr
            };
            if !should_capture {
                continue;
            }
            if routed_stdout {
                capture.stdout.extend_from_slice(data);
            } else {
                capture.stderr.extend_from_slice(data);
            }
            return false;
        }

        if routed_stdout {
            self.vm.stdout.extend_from_slice(data);
        } else {
            self.vm.stderr.extend_from_slice(data);
        }
        true
    }

    fn account_output(&mut self, bytes: usize) {
        self.vm.track_output(bytes as u64);
        self.flag_output_limit_if_needed();
    }

    fn write_stdout(&mut self, data: &[u8]) {
        if self.route_output(data, true) {
            self.account_output(data.len());
        }
    }

    fn write_stderr(&mut self, data: &[u8]) {
        if self.route_output(data, false) {
            self.account_output(data.len());
        }
    }

    fn write_streams(&mut self, stdout: &[u8], stderr: &[u8]) {
        let visible_stdout = self.route_output(stdout, true);
        let visible_stderr = self.route_output(stderr, false);
        let visible_bytes =
            usize::from(visible_stdout) * stdout.len() + usize::from(visible_stderr) * stderr.len();
        if visible_bytes > 0 {
            self.account_output(visible_bytes);
        }
    }

    fn flag_output_limit_if_needed(&mut self) {
        if self.exec.resource_exhausted {
            return;
        }
        if self.vm.check_output_limit().is_err() {
            self.exec.resource_exhausted = true;
        }
    }

    fn drain_diagnostic_events(&mut self, events: &mut Vec<WorkerEvent>) {
        for diag in self.vm.diagnostics.drain(..) {
            events.push(WorkerEvent::Diagnostic(
                Self::to_protocol_diag_level(diag.level),
                diag.message,
            ));
        }
    }

    fn to_protocol_diag_level(level: wasmsh_vm::DiagLevel) -> DiagnosticLevel {
        match level {
            wasmsh_vm::DiagLevel::Trace => DiagnosticLevel::Trace,
            wasmsh_vm::DiagLevel::Info => DiagnosticLevel::Info,
            wasmsh_vm::DiagLevel::Warning => DiagnosticLevel::Warning,
            wasmsh_vm::DiagLevel::Error => DiagnosticLevel::Error,
        }
    }

    fn execute_pipeline_chain(&mut self, and_or: &HirAndOr) {
        self.execute_pipeline(&and_or.first);
        for (op, pipeline) in &and_or.rest {
            match op {
                HirAndOrOp::And => {
                    if self.vm.state.last_status == 0 {
                        self.execute_pipeline(pipeline);
                    }
                }
                HirAndOrOp::Or => {
                    if self.vm.state.last_status != 0 {
                        self.execute_pipeline(pipeline);
                    }
                }
            }
        }
    }

    fn execute_pipeline(&mut self, pipeline: &HirPipeline) {
        let started = self.monotonic_now_ms();
        let cmds = &pipeline.commands;
        self.execute_scheduled_pipeline(cmds, pipeline);
        if pipeline.negated {
            self.vm.state.last_status = i32::from(self.vm.state.last_status == 0);
        }
        if pipeline.timed {
            let elapsed_seconds = self.monotonic_now_ms().saturating_sub(started) as f64 / 1000.0;
            self.emit_pipeline_timing(pipeline.time_posix, elapsed_seconds);
        }
    }

    fn execute_scheduled_pipeline(&mut self, cmds: &[HirCommand], pipeline: &HirPipeline) {
        self.execute_scheduled_pipeline_with_source_reader(cmds, pipeline, None);
    }

    fn execute_scheduled_pipeline_with_source_reader(
        &mut self,
        cmds: &[HirCommand],
        pipeline: &HirPipeline,
        source_reader: Option<Box<dyn Read>>,
    ) {
        let pipefail = self.vm.state.get_var("SHOPT_o_pipefail").as_deref() == Some("1");
        let (stages, stage_last_args) = self.compile_pipeline_stages(cmds, source_reader.is_none());
        if source_reader.is_none() && stages.len() == 1 {
            self.run_single_pipeline_stage(&cmds[0], &stages[0], stage_last_args[0].as_deref());
            return;
        }
        let stage_statuses = Self::seed_stage_statuses(&stages);
        let stage_stderr: Vec<Rc<RefCell<Vec<u8>>>> = stages
            .iter()
            .map(|_| Rc::new(RefCell::new(Vec::new())))
            .collect();
        let stage_pipe_stderr: Vec<bool> = (0..stages.len())
            .map(|idx| pipeline.pipe_stderr.get(idx).copied().unwrap_or(false))
            .collect();

        self.execute_pipebuffer_streaming_pipeline(
            source_reader,
            &stages,
            &stage_pipe_stderr,
            &stage_statuses,
            &stage_stderr,
        );

        let statuses: Vec<i32> = stage_statuses
            .iter()
            .map(|status| *status.borrow())
            .collect();
        if let Some(last_arg) = stage_last_args.iter().rev().flatten().next() {
            self.vm.state.set_last_argument(last_arg.as_str());
        }
        self.set_pipestatus(&statuses);
        if !self.exec.resource_exhausted {
            self.vm.state.last_status = Self::resolve_pipeline_exit_status(&statuses, pipefail);
        }
    }

    fn compile_pipeline_stages(
        &mut self,
        cmds: &[HirCommand],
        no_source_reader: bool,
    ) -> (Vec<StreamingPipelineStage>, Vec<Option<String>>) {
        cmds.iter()
            .enumerate()
            .map(|(idx, cmd)| {
                self.compile_pipeline_stage_with_last_argument(cmd, idx == 0 && no_source_reader)
            })
            .unzip()
    }

    fn run_single_pipeline_stage(
        &mut self,
        cmd: &HirCommand,
        stage: &StreamingPipelineStage,
        last_arg: Option<&str>,
    ) {
        if self.command_needs_full_single_stage_execution(cmd) {
            self.execute_command(cmd);
            let status = self.vm.state.last_status;
            self.set_pipestatus(&[status]);
            return;
        }
        if !matches!(stage, StreamingPipelineStage::BufferedCommand(_))
            && !Self::command_requires_runtime_expansion(cmd)
        {
            if let Some(argv) = self.resolve_streaming_pipeline_argv(cmd) {
                self.trace_command(&argv);
            }
        }
        let status = self.execute_scheduled_single_stage(stage);
        if let Some(last_arg) = last_arg {
            self.vm.state.set_last_argument(last_arg);
        }
        self.set_pipestatus(&[status]);
        if !self.exec.resource_exhausted {
            self.vm.state.last_status = status;
        }
    }

    fn seed_stage_statuses(stages: &[StreamingPipelineStage]) -> Vec<Rc<RefCell<i32>>> {
        stages
            .iter()
            .map(|stage| {
                Rc::new(RefCell::new(i32::from(matches!(
                    stage,
                    StreamingPipelineStage::Grep(_)
                ))))
            })
            .collect()
    }

    fn resolve_pipeline_exit_status(statuses: &[i32], pipefail: bool) -> i32 {
        if pipefail {
            statuses
                .iter()
                .rev()
                .copied()
                .find(|status| *status != 0)
                .unwrap_or(0)
        } else {
            statuses.last().copied().unwrap_or(0)
        }
    }

    fn execute_scheduled_single_stage(&mut self, stage: &StreamingPipelineStage) -> i32 {
        match stage {
            StreamingPipelineStage::Literal(data) => {
                self.write_stdout(data);
                0
            }
            StreamingPipelineStage::File(path) => self.execute_single_stage_file(path),
            StreamingPipelineStage::Yes { line } => self.execute_single_stage_yes(line),
            StreamingPipelineStage::BufferedCommand(BufferedPipelineCommand::Argv(argv)) => {
                self.trace_command(argv);
                self.execute_argv_command(argv);
                self.vm.state.last_status
            }
            StreamingPipelineStage::BufferedCommand(BufferedPipelineCommand::Hir(cmd)) => {
                self.execute_command(cmd);
                self.vm.state.last_status
            }
            _ => {
                self.vm.state.last_status = 1;
                self.write_stderr(b"wasmsh: unsupported single-stage scheduler node\n");
                1
            }
        }
    }

    fn execute_single_stage_file(&mut self, path: &str) -> i32 {
        let resolved = self.resolve_cwd_path(path);
        let Ok(mut reader) = self.open_streaming_file_reader(&resolved, "cat") else {
            return self.vm.state.last_status;
        };
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return 0,
                Ok(read) => {
                    self.write_stdout(&buffer[..read]);
                    if self.exec.resource_exhausted {
                        return 1;
                    }
                }
                Err(err) => {
                    self.write_stderr(format!("wasmsh: cat: stdin read error: {err}\n").as_bytes());
                    return 1;
                }
            }
        }
    }

    fn execute_single_stage_yes(&mut self, line: &[u8]) -> i32 {
        for _ in 0..STREAMING_YES_MAX_LINES {
            self.write_stdout(line);
            if self.exec.resource_exhausted {
                return 1;
            }
        }
        0
    }

    fn compile_pipeline_stage(
        &mut self,
        cmd: &HirCommand,
        is_first: bool,
    ) -> StreamingPipelineStage {
        let resolved_argv = self.resolve_streaming_pipeline_argv(cmd);
        self.compile_pipeline_stage_from_argv(cmd, is_first, resolved_argv)
    }

    fn compile_pipeline_stage_with_last_argument(
        &mut self,
        cmd: &HirCommand,
        is_first: bool,
    ) -> (StreamingPipelineStage, Option<String>) {
        let resolved_argv = self.resolve_streaming_pipeline_argv(cmd);
        let last_arg = resolved_argv.as_ref().and_then(|argv| argv.last().cloned());
        (
            self.compile_pipeline_stage_from_argv(cmd, is_first, resolved_argv),
            last_arg,
        )
    }

    fn compile_pipeline_stage_from_argv(
        &mut self,
        cmd: &HirCommand,
        is_first: bool,
        resolved_argv: Option<Vec<String>>,
    ) -> StreamingPipelineStage {
        if let Some(argv) = resolved_argv {
            if self.get_shopt_value("expand_aliases")
                && argv
                    .first()
                    .is_some_and(|name| self.aliases.contains_key(name))
            {
                return StreamingPipelineStage::BufferedCommand(BufferedPipelineCommand::Hir(
                    cmd.clone(),
                ));
            }
            if argv
                .first()
                .is_some_and(|name| self.functions.contains_key(name))
            {
                return StreamingPipelineStage::BufferedCommand(BufferedPipelineCommand::Argv(
                    argv,
                ));
            }
            if let Some(stage) = self.parse_streaming_stage(&argv, is_first) {
                if Self::uses_native_pipe_scheduler(&stage) {
                    return stage;
                }
                return StreamingPipelineStage::BufferedCommand(BufferedPipelineCommand::Argv(
                    argv,
                ));
            }
            return StreamingPipelineStage::BufferedCommand(BufferedPipelineCommand::Hir(
                cmd.clone(),
            ));
        }
        StreamingPipelineStage::BufferedCommand(BufferedPipelineCommand::Hir(cmd.clone()))
    }

    fn uses_native_pipe_scheduler(stage: &StreamingPipelineStage) -> bool {
        !matches!(stage, StreamingPipelineStage::BufferedCommand(_))
    }

    fn execute_pipebuffer_streaming_pipeline(
        &mut self,
        source_reader: Option<Box<dyn Read>>,
        stages: &[StreamingPipelineStage],
        stage_pipe_stderr: &[bool],
        stage_statuses: &[Rc<RefCell<i32>>],
        stage_stderr: &[Rc<RefCell<Vec<u8>>>],
    ) -> bool {
        let mut processes = Vec::new();
        let output_pipes: Vec<Rc<RefCell<PipeBuffer>>> = (0..stages.len())
            .map(|_| Rc::new(RefCell::new(PipeBuffer::new(PIPEBUFFER_STREAMING_CAPACITY))))
            .collect();
        let ctx = StreamingStageCtx {
            stages,
            stage_pipe_stderr,
            stage_statuses,
            stage_stderr,
            output_pipes: &output_pipes,
        };

        if let Some(early) = self.setup_first_streaming_process(source_reader, &ctx, &mut processes)
        {
            return early;
        }
        for idx in 1..stages.len() {
            if !self.setup_later_streaming_stage(idx, &ctx, &mut processes) {
                return false;
            }
        }

        let final_pipe = output_pipes
            .last()
            .cloned()
            .expect("final pipe missing for streaming pipeline");
        self.drive_streaming_pipeline(&mut processes, &output_pipes, &final_pipe);

        for process in &mut processes {
            process.close(self);
        }
        self.drain_streaming_stage_stderr(stage_pipe_stderr, stage_stderr);
        true
    }

    fn setup_first_streaming_process(
        &mut self,
        source_reader: Option<Box<dyn Read>>,
        ctx: &StreamingStageCtx<'_>,
        processes: &mut Vec<StreamingPipeProcess<'static>>,
    ) -> Option<bool> {
        if let Some(source_reader) = source_reader {
            self.setup_first_with_source(source_reader, ctx, processes)
        } else {
            self.setup_first_without_source(ctx, processes)
        }
    }

    fn setup_first_with_source(
        &mut self,
        source_reader: Box<dyn Read>,
        ctx: &StreamingStageCtx<'_>,
        processes: &mut Vec<StreamingPipeProcess<'static>>,
    ) -> Option<bool> {
        let source_pipe = Rc::new(RefCell::new(PipeBuffer::new(PIPEBUFFER_STREAMING_CAPACITY)));
        let source_stderr = Rc::new(RefCell::new(Vec::new()));
        let source_status = Rc::new(RefCell::new(0));
        processes.push(StreamingPipeProcess::Read(PipeReadProcess::new(
            source_reader,
            source_pipe.clone(),
            source_stderr,
            source_status,
            "source",
            false,
        )));
        match &ctx.stages[0] {
            StreamingPipelineStage::Tee(stage) => {
                let reader = Box::new(PipeReader::new(source_pipe)) as Box<dyn Read>;
                processes.push(StreamingPipeProcess::Tee(TeePipeProcess::new(
                    reader,
                    ctx.output_pipes[0].clone(),
                    &mut self.fs,
                    self.vm.state.cwd.as_str(),
                    stage,
                    ctx.stage_stderr[0].clone(),
                    ctx.stage_statuses[0].clone(),
                    ctx.stage_pipe_stderr[0],
                )));
                None
            }
            StreamingPipelineStage::External(argv) => {
                let Some(spec) = argv
                    .first()
                    .and_then(|name| self.external_specs.get(name))
                    .cloned()
                else {
                    return Some(false);
                };
                processes.push(StreamingPipeProcess::External(ExternalPipeProcess::start(
                    self,
                    Some(source_pipe),
                    ctx.output_pipes[0].clone(),
                    argv.clone(),
                    spec,
                    ctx.stage_pipe_stderr[0],
                    ctx.stage_stderr[0].clone(),
                    ctx.stage_statuses[0].clone(),
                )));
                None
            }
            StreamingPipelineStage::BufferedCommand(argv) => {
                processes.push(StreamingPipeProcess::Buffered(BufferedPipeProcess::new(
                    Some(source_pipe),
                    ctx.output_pipes[0].clone(),
                    argv.clone(),
                    ctx.stage_pipe_stderr[0],
                    ctx.stage_stderr[0].clone(),
                    ctx.stage_statuses[0].clone(),
                )));
                None
            }
            _ => {
                let reader = Box::new(PipeReader::new(source_pipe)) as Box<dyn Read>;
                let Some(stage_reader) = Self::wrap_non_tee_streaming_stage(
                    reader,
                    &ctx.stages[0],
                    0,
                    ctx.stage_statuses,
                ) else {
                    return Some(false);
                };
                processes.push(StreamingPipeProcess::Read(PipeReadProcess::new(
                    stage_reader,
                    ctx.output_pipes[0].clone(),
                    ctx.stage_stderr[0].clone(),
                    ctx.stage_statuses[0].clone(),
                    "stage",
                    ctx.stage_pipe_stderr[0],
                )));
                None
            }
        }
    }

    fn setup_first_without_source(
        &mut self,
        ctx: &StreamingStageCtx<'_>,
        processes: &mut Vec<StreamingPipeProcess<'static>>,
    ) -> Option<bool> {
        match &ctx.stages[0] {
            StreamingPipelineStage::Literal(data) => {
                let first_reader: Box<dyn Read> = Box::new(Cursor::new(data.clone()));
                processes.push(StreamingPipeProcess::Read(PipeReadProcess::new(
                    first_reader,
                    ctx.output_pipes[0].clone(),
                    ctx.stage_stderr[0].clone(),
                    ctx.stage_statuses[0].clone(),
                    "source",
                    ctx.stage_pipe_stderr[0],
                )));
                None
            }
            StreamingPipelineStage::File(path) => {
                let resolved = self.resolve_cwd_path(path);
                let Ok(first_reader) = self.open_streaming_file_reader(&resolved, "cat") else {
                    *ctx.stage_statuses[0].borrow_mut() = self.vm.state.last_status;
                    return Some(true);
                };
                processes.push(StreamingPipeProcess::Read(PipeReadProcess::new(
                    first_reader,
                    ctx.output_pipes[0].clone(),
                    ctx.stage_stderr[0].clone(),
                    ctx.stage_statuses[0].clone(),
                    "source",
                    ctx.stage_pipe_stderr[0],
                )));
                None
            }
            StreamingPipelineStage::Yes { line } => {
                let first_reader: Box<dyn Read> =
                    Box::new(YesStreamReader::new(line.clone(), STREAMING_YES_MAX_LINES));
                processes.push(StreamingPipeProcess::Read(PipeReadProcess::new(
                    first_reader,
                    ctx.output_pipes[0].clone(),
                    ctx.stage_stderr[0].clone(),
                    ctx.stage_statuses[0].clone(),
                    "source",
                    ctx.stage_pipe_stderr[0],
                )));
                None
            }
            StreamingPipelineStage::External(argv) => {
                let Some(cmd_name) = argv.first() else {
                    return Some(false);
                };
                let Some(spec) = self.external_specs.get(cmd_name).cloned() else {
                    return Some(false);
                };
                processes.push(StreamingPipeProcess::External(ExternalPipeProcess::start(
                    self,
                    None,
                    ctx.output_pipes[0].clone(),
                    argv.clone(),
                    spec,
                    ctx.stage_pipe_stderr[0],
                    ctx.stage_stderr[0].clone(),
                    ctx.stage_statuses[0].clone(),
                )));
                None
            }
            StreamingPipelineStage::BufferedCommand(argv) => {
                processes.push(StreamingPipeProcess::Buffered(BufferedPipeProcess::new(
                    None,
                    ctx.output_pipes[0].clone(),
                    argv.clone(),
                    ctx.stage_pipe_stderr[0],
                    ctx.stage_stderr[0].clone(),
                    ctx.stage_statuses[0].clone(),
                )));
                None
            }
            _ => unreachable!("unexpected first pipeline stage"),
        }
    }

    fn setup_later_streaming_stage(
        &mut self,
        idx: usize,
        ctx: &StreamingStageCtx<'_>,
        processes: &mut Vec<StreamingPipeProcess<'static>>,
    ) -> bool {
        match &ctx.stages[idx] {
            StreamingPipelineStage::Head(mode) => {
                processes.push(StreamingPipeProcess::Head(HeadPipeProcess::new(
                    ctx.output_pipes[idx - 1].clone(),
                    ctx.output_pipes[idx].clone(),
                    *mode,
                )));
            }
            StreamingPipelineStage::Tee(stage) => {
                let reader =
                    Box::new(PipeReader::new(ctx.output_pipes[idx - 1].clone())) as Box<dyn Read>;
                processes.push(StreamingPipeProcess::Tee(TeePipeProcess::new(
                    reader,
                    ctx.output_pipes[idx].clone(),
                    &mut self.fs,
                    self.vm.state.cwd.as_str(),
                    stage,
                    ctx.stage_stderr[idx].clone(),
                    ctx.stage_statuses[idx].clone(),
                    ctx.stage_pipe_stderr[idx],
                )));
            }
            StreamingPipelineStage::External(argv) => {
                let Some(cmd_name) = argv.first() else {
                    return false;
                };
                let Some(spec) = self.external_specs.get(cmd_name).cloned() else {
                    return false;
                };
                processes.push(StreamingPipeProcess::External(ExternalPipeProcess::start(
                    self,
                    Some(ctx.output_pipes[idx - 1].clone()),
                    ctx.output_pipes[idx].clone(),
                    argv.clone(),
                    spec,
                    ctx.stage_pipe_stderr[idx],
                    ctx.stage_stderr[idx].clone(),
                    ctx.stage_statuses[idx].clone(),
                )));
            }
            StreamingPipelineStage::BufferedCommand(argv) => {
                processes.push(StreamingPipeProcess::Buffered(BufferedPipeProcess::new(
                    Some(ctx.output_pipes[idx - 1].clone()),
                    ctx.output_pipes[idx].clone(),
                    argv.clone(),
                    ctx.stage_pipe_stderr[idx],
                    ctx.stage_stderr[idx].clone(),
                    ctx.stage_statuses[idx].clone(),
                )));
            }
            _ => {
                let reader =
                    Box::new(PipeReader::new(ctx.output_pipes[idx - 1].clone())) as Box<dyn Read>;
                let Some(stage_reader) = Self::wrap_non_tee_streaming_stage(
                    reader,
                    &ctx.stages[idx],
                    idx,
                    ctx.stage_statuses,
                ) else {
                    return false;
                };
                processes.push(StreamingPipeProcess::Read(PipeReadProcess::new(
                    stage_reader,
                    ctx.output_pipes[idx].clone(),
                    ctx.stage_stderr[idx].clone(),
                    ctx.stage_statuses[idx].clone(),
                    "stage",
                    ctx.stage_pipe_stderr[idx],
                )));
            }
        }
        true
    }

    fn drive_streaming_pipeline(
        &mut self,
        processes: &mut [StreamingPipeProcess<'static>],
        output_pipes: &[Rc<RefCell<PipeBuffer>>],
        final_pipe: &Rc<RefCell<PipeBuffer>>,
    ) {
        let mut finished = vec![false; processes.len()];
        loop {
            if self.check_resource_limits() {
                final_pipe.borrow_mut().close_read();
                break;
            }

            let mut progressed = self.poll_streaming_processes(processes, &mut finished);

            let buffered_pipe_bytes = output_pipes
                .iter()
                .map(|pipe| pipe.borrow().len() as u64)
                .sum();
            self.sync_pipe_budget(buffered_pipe_bytes);
            if self.exec.resource_exhausted {
                final_pipe.borrow_mut().close_read();
                break;
            }

            if self.drain_final_pipe_to_stdout(final_pipe, &mut progressed) {
                break;
            }

            if self.exec.resource_exhausted || finished.iter().all(|done| *done) || !progressed {
                break;
            }
        }
    }

    fn poll_streaming_processes(
        &mut self,
        processes: &mut [StreamingPipeProcess<'static>],
        finished: &mut [bool],
    ) -> bool {
        let mut progressed = false;
        for idx in (0..processes.len()).rev() {
            if finished[idx] {
                continue;
            }
            match processes[idx].poll(self) {
                PipeProcessPoll::Ready => progressed = true,
                PipeProcessPoll::PendingRead | PipeProcessPoll::PendingWrite => {}
                PipeProcessPoll::Exited => {
                    finished[idx] = true;
                    progressed = true;
                }
            }
        }
        progressed
    }

    fn drain_final_pipe_to_stdout(
        &mut self,
        final_pipe: &Rc<RefCell<PipeBuffer>>,
        progressed: &mut bool,
    ) -> bool {
        loop {
            let mut buffer = [0u8; 4096];
            let read_result = {
                let mut pipe = final_pipe.borrow_mut();
                pipe.read(&mut buffer)
            };
            match read_result {
                ReadResult::Read(read) => {
                    self.write_stdout(&buffer[..read]);
                    *progressed = true;
                    if self.exec.resource_exhausted {
                        final_pipe.borrow_mut().close_read();
                        return true;
                    }
                }
                ReadResult::WouldBlock | ReadResult::Eof => return false,
            }
        }
    }

    fn drain_streaming_stage_stderr(
        &mut self,
        stage_pipe_stderr: &[bool],
        stage_stderr: &[Rc<RefCell<Vec<u8>>>],
    ) {
        for (idx, stderr) in stage_stderr.iter().enumerate() {
            if stage_pipe_stderr[idx] {
                continue;
            }
            let data = stderr.borrow();
            if !data.is_empty() {
                self.write_stderr(&data);
            }
        }
    }

    fn wrap_non_tee_streaming_stage<'a>(
        reader: Box<dyn Read + 'a>,
        stage: &StreamingPipelineStage,
        idx: usize,
        stage_statuses: &[Rc<RefCell<i32>>],
    ) -> Option<Box<dyn Read + 'a>> {
        match stage {
            StreamingPipelineStage::Cat => Some(reader),
            StreamingPipelineStage::Head(mode) => Some(match mode {
                StreamingHeadMode::Lines(limit) => Box::new(HeadStreamReader::new(
                    reader,
                    StreamingHeadMode::Lines(*limit),
                )),
                StreamingHeadMode::Bytes(limit) => Box::new(HeadStreamReader::new(
                    reader,
                    StreamingHeadMode::Bytes(*limit),
                )),
            }),
            StreamingPipelineStage::Tail(mode) => Some(match mode {
                StreamingTailMode::Lines(limit) => Box::new(TailStreamReader::new(
                    reader,
                    StreamingTailMode::Lines(*limit),
                )),
                StreamingTailMode::Bytes(limit) => Box::new(TailStreamReader::new(
                    reader,
                    StreamingTailMode::Bytes(*limit),
                )),
            }),
            StreamingPipelineStage::Bat(stage) => {
                Some(Box::new(BatStreamReader::new(reader, *stage)))
            }
            StreamingPipelineStage::Sed(stage) => {
                Some(Box::new(SedStreamReader::new(reader, stage.clone())))
            }
            StreamingPipelineStage::Paste(stage) => {
                Some(Box::new(PasteStreamReader::new(reader, stage.clone())))
            }
            StreamingPipelineStage::Column(_) => Some(Box::new(ColumnStreamReader::new(reader))),
            StreamingPipelineStage::Grep(stage) => Some(Box::new(GrepStreamReader::new(
                reader,
                stage.clone(),
                stage_statuses[idx].clone(),
            ))),
            StreamingPipelineStage::Uniq(flags) => {
                Some(Box::new(UniqStreamReader::new(reader, flags.clone())))
            }
            StreamingPipelineStage::Rev => Some(Box::new(RevStreamReader::new(reader))),
            StreamingPipelineStage::Cut(stage) => {
                Some(Box::new(CutStreamReader::new(reader, stage.clone())))
            }
            StreamingPipelineStage::Tr(stage) => {
                Some(Box::new(TrStreamReader::new(reader, stage.clone())))
            }
            StreamingPipelineStage::Wc(flags) => {
                Some(Box::new(WcStreamReader::new(reader, *flags)))
            }
            StreamingPipelineStage::Tee(_)
            | StreamingPipelineStage::Literal(_)
            | StreamingPipelineStage::File(_)
            | StreamingPipelineStage::Yes { .. }
            | StreamingPipelineStage::External(_)
            | StreamingPipelineStage::BufferedCommand(_) => None,
        }
    }

    fn resolve_streaming_pipeline_argv(&mut self, cmd: &HirCommand) -> Option<Vec<String>> {
        let HirCommand::Exec(exec) = cmd else {
            return None;
        };
        if !exec.env.is_empty()
            || !exec.redirections.is_empty()
            || Self::command_requires_runtime_expansion(cmd)
        {
            return None;
        }
        let resolved = self.resolve_command_subst(&exec.argv);
        if self.exec.expansion_failed {
            return None;
        }
        let expanded = expand_words_argv(&resolved, &mut self.vm.state);
        if self.check_nounset_error()
            || self.check_arith_error()
            || self.check_expansion_error()
            || expanded.is_empty()
        {
            return None;
        }
        let tagged: Vec<(String, bool)> = expanded
            .into_iter()
            .flat_map(|ew| {
                if ew.was_quoted {
                    vec![(ew.text, true)]
                } else {
                    wasmsh_expand::expand_braces(&ew.text)
                        .into_iter()
                        .map(|s| (s, false))
                        .collect()
                }
            })
            .collect();
        Some(self.expand_globs_tagged(tagged))
    }

    fn parse_streaming_stage(
        &self,
        argv: &[String],
        is_first: bool,
    ) -> Option<StreamingPipelineStage> {
        let cmd_name = argv.first()?.as_str();
        if let Some(stage) = Self::parse_streaming_first_stage(cmd_name, argv, is_first) {
            return Some(stage);
        }
        if let Some(stage) = Self::parse_streaming_internal_stage(cmd_name, argv, is_first) {
            return Some(stage);
        }
        if self.allow_external_streaming
            && self.external_stream_handler.is_some()
            && self.external_specs.contains_key(cmd_name)
        {
            return Some(StreamingPipelineStage::External(argv.to_vec()));
        }
        if self.is_buffered_stage_candidate(cmd_name) {
            return Some(StreamingPipelineStage::BufferedCommand(
                BufferedPipelineCommand::Argv(argv.to_vec()),
            ));
        }
        None
    }

    fn parse_streaming_first_stage(
        cmd_name: &str,
        argv: &[String],
        is_first: bool,
    ) -> Option<StreamingPipelineStage> {
        if !is_first {
            return None;
        }
        match cmd_name {
            "echo" => Some(StreamingPipelineStage::Literal(Self::streaming_echo_bytes(
                &argv[1..],
            ))),
            "yes" => {
                let text = if argv.len() > 1 {
                    argv[1..].join(" ")
                } else {
                    "y".to_string()
                };
                Some(StreamingPipelineStage::Yes {
                    line: format!("{text}\n").into_bytes(),
                })
            }
            _ => None,
        }
    }

    fn parse_streaming_internal_stage(
        cmd_name: &str,
        argv: &[String],
        is_first: bool,
    ) -> Option<StreamingPipelineStage> {
        if cmd_name == "cat" {
            return Self::parse_streaming_cat_stage(&argv[1..], is_first);
        }
        if is_first {
            return None;
        }
        match cmd_name {
            "head" => Self::parse_streaming_head_stage(&argv[1..]),
            "tail" => Self::parse_streaming_tail_stage(&argv[1..]),
            "bat" => Self::parse_streaming_bat_stage(&argv[1..]),
            "sed" => Self::parse_streaming_sed_stage(&argv[1..]),
            "tee" => Self::parse_streaming_tee_stage(&argv[1..]),
            "paste" => Self::parse_streaming_paste_stage(&argv[1..]),
            "column" => Self::parse_streaming_column_stage(&argv[1..]),
            "grep" => Self::parse_streaming_grep_stage(&argv[1..]),
            "uniq" => Self::parse_streaming_uniq_stage(&argv[1..]),
            "rev" => Self::parse_streaming_rev_stage(&argv[1..]),
            "cut" => Self::parse_streaming_cut_stage(&argv[1..]),
            "tr" => Self::parse_streaming_tr_stage(&argv[1..]),
            "wc" => Self::parse_streaming_wc_stage(&argv[1..]),
            _ => None,
        }
    }

    fn is_buffered_stage_candidate(&self, cmd_name: &str) -> bool {
        cmd_name == "bash"
            || cmd_name == "sh"
            || cmd_name == "builtin"
            || self.functions.contains_key(cmd_name)
            || self.builtins.is_builtin(cmd_name)
            || self.utils.is_utility(cmd_name)
            || self.external_handler.is_some()
            || self.external_spec_handler.is_some()
            || self.external_specs.contains_key(cmd_name)
    }

    fn streaming_echo_bytes(args: &[String]) -> Vec<u8> {
        let mut suppress_newline = false;
        let mut interpret_escapes = false;
        let mut start = 0usize;

        for (i, arg) in args.iter().enumerate() {
            let bytes = arg.as_bytes();
            if bytes.first() != Some(&b'-') || bytes.len() < 2 {
                break;
            }
            if !bytes[1..].iter().all(|b| matches!(b, b'n' | b'e')) {
                break;
            }
            for &byte in &bytes[1..] {
                match byte {
                    b'n' => suppress_newline = true,
                    b'e' => interpret_escapes = true,
                    _ => {}
                }
            }
            start = i + 1;
        }

        let text = args[start..].join(" ");
        let rendered = if interpret_escapes {
            Self::process_streaming_echo_escapes(&text)
        } else {
            text
        };
        let mut output = rendered.into_bytes();
        if !suppress_newline {
            output.push(b'\n');
        }
        output
    }

    fn process_streaming_echo_escapes(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut output = String::new();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                match bytes[i + 1] {
                    b'n' => output.push('\n'),
                    b't' => output.push('\t'),
                    b'r' => output.push('\r'),
                    b'\\' => output.push('\\'),
                    other => {
                        output.push('\\');
                        output.push(other as char);
                    }
                }
                i += 2;
            } else {
                output.push(bytes[i] as char);
                i += 1;
            }
        }
        output
    }

    fn parse_streaming_cat_stage(
        args: &[String],
        is_first: bool,
    ) -> Option<StreamingPipelineStage> {
        let non_separator: Vec<&String> = args.iter().filter(|arg| arg.as_str() != "--").collect();
        if non_separator.iter().any(|arg| arg.starts_with('-')) {
            return None;
        }
        if is_first {
            if non_separator.len() == 1 {
                return Some(StreamingPipelineStage::File(non_separator[0].clone()));
            }
            return None;
        }
        Some(StreamingPipelineStage::Cat)
    }

    fn parse_streaming_head_stage(args: &[String]) -> Option<StreamingPipelineStage> {
        let mut mode = StreamingHeadMode::Lines(10);
        let mut files: Vec<&str> = Vec::new();
        let mut i = 0usize;
        while i < args.len() {
            i = Self::apply_streaming_head_arg(args, i, &mut mode, &mut files)?;
        }
        files
            .is_empty()
            .then_some(StreamingPipelineStage::Head(mode))
    }

    fn apply_streaming_head_arg<'a>(
        args: &'a [String],
        i: usize,
        mode: &mut StreamingHeadMode,
        files: &mut Vec<&'a str>,
    ) -> Option<usize> {
        let arg = args[i].as_str();
        if arg == "--" {
            return Some(i + 1);
        }
        if arg == "-c" && i + 1 < args.len() {
            *mode = StreamingHeadMode::Bytes(args[i + 1].parse().ok()?);
            return Some(i + 2);
        }
        if arg == "-n" && i + 1 < args.len() {
            *mode = StreamingHeadMode::Lines(args[i + 1].parse().ok()?);
            return Some(i + 2);
        }
        if arg.starts_with('-') && arg.len() > 1 {
            *mode = StreamingHeadMode::Lines(arg[1..].parse().ok()?);
            return Some(i + 1);
        }
        files.push(arg);
        Some(i + 1)
    }

    fn parse_streaming_tail_stage(args: &[String]) -> Option<StreamingPipelineStage> {
        let mut mode = StreamingTailMode::Lines(10);
        let mut files: Vec<&str> = Vec::new();
        let mut i = 0usize;
        while i < args.len() {
            i = Self::apply_streaming_tail_arg(args, i, &mut mode, &mut files)?;
        }
        files
            .is_empty()
            .then_some(StreamingPipelineStage::Tail(mode))
    }

    fn apply_streaming_tail_arg<'a>(
        args: &'a [String],
        i: usize,
        mode: &mut StreamingTailMode,
        files: &mut Vec<&'a str>,
    ) -> Option<usize> {
        let arg = args[i].as_str();
        if arg == "-f" {
            return None;
        }
        if arg == "--" {
            return Some(i + 1);
        }
        if arg == "-c" && i + 1 < args.len() {
            *mode = StreamingTailMode::Bytes(args[i + 1].parse().ok()?);
            return Some(i + 2);
        }
        if arg == "-n" && i + 1 < args.len() {
            *mode = Self::parse_streaming_tail_lines_value(&args[i + 1])?;
            return Some(i + 2);
        }
        if arg.starts_with('-') && arg.len() > 1 {
            *mode = StreamingTailMode::Lines(arg[1..].parse().ok()?);
            return Some(i + 1);
        }
        files.push(arg);
        Some(i + 1)
    }

    fn parse_streaming_tail_lines_value(value: &str) -> Option<StreamingTailMode> {
        if value.starts_with('+') {
            return None;
        }
        Some(StreamingTailMode::Lines(value.parse().ok()?))
    }

    fn parse_streaming_bat_stage(args: &[String]) -> Option<StreamingPipelineStage> {
        let mut stage = StreamingBatStage {
            show_numbers: true,
            show_header: true,
            line_range: None,
            show_all: false,
        };
        let mut i = 0usize;
        while i < args.len() {
            let advance = Self::apply_streaming_bat_arg(args, i, &mut stage)?;
            i += advance;
        }
        Some(StreamingPipelineStage::Bat(stage))
    }

    fn apply_streaming_bat_arg(
        args: &[String],
        i: usize,
        stage: &mut StreamingBatStage,
    ) -> Option<usize> {
        let arg = args[i].as_str();
        match arg {
            "-n" | "--number" => {
                stage.show_numbers = true;
                Some(1)
            }
            "-p" | "--plain" | "--style=plain" => {
                stage.show_numbers = false;
                stage.show_header = false;
                Some(1)
            }
            "-A" | "--show-all" => {
                stage.show_all = true;
                Some(1)
            }
            "-r" | "--line-range" if i + 1 < args.len() => {
                stage.line_range = Self::parse_streaming_bat_range(&args[i + 1]);
                Some(2)
            }
            "-l" | "--language" | "--paging" if i + 1 < args.len() => Some(2),
            "--style=numbers" => {
                stage.show_numbers = true;
                stage.show_header = false;
                Some(1)
            }
            "--style=header" => {
                stage.show_numbers = false;
                stage.show_header = true;
                Some(1)
            }
            "--" => (i + 1 == args.len()).then_some(1),
            _ => Self::apply_streaming_bat_long_or_short(arg, stage),
        }
    }

    fn apply_streaming_bat_long_or_short(
        value: &str,
        stage: &mut StreamingBatStage,
    ) -> Option<usize> {
        if value.starts_with("--style=") {
            stage.show_numbers = true;
            stage.show_header = true;
            return Some(1);
        }
        if let Some(range_spec) = value.strip_prefix("--line-range=") {
            stage.line_range = Self::parse_streaming_bat_range(range_spec);
            return Some(1);
        }
        if value.starts_with("--paging=") || value.starts_with("--language=") {
            return Some(1);
        }
        if value.starts_with('-') && value.len() > 1 && !value.starts_with("--") {
            Self::apply_streaming_bat_short_cluster(&value[1..], stage)?;
            return Some(1);
        }
        None
    }

    fn apply_streaming_bat_short_cluster(flags: &str, stage: &mut StreamingBatStage) -> Option<()> {
        for ch in flags.chars() {
            match ch {
                'n' => stage.show_numbers = true,
                'p' => {
                    stage.show_numbers = false;
                    stage.show_header = false;
                }
                'A' => stage.show_all = true,
                _ => return None,
            }
        }
        Some(())
    }

    fn parse_streaming_bat_range(s: &str) -> Option<(Option<usize>, Option<usize>)> {
        if let Some((start, end)) = s.split_once(':') {
            let start = if start.is_empty() {
                None
            } else {
                start.parse().ok()
            };
            let end = if end.is_empty() {
                None
            } else {
                end.parse().ok()
            };
            Some((start, end))
        } else {
            let n = s.parse().ok()?;
            Some((Some(n), Some(n)))
        }
    }

    fn parse_streaming_sed_stage(args: &[String]) -> Option<StreamingPipelineStage> {
        let mut suppress_print = false;
        let mut expressions = Vec::new();
        let mut i = 0usize;
        while i < args.len() {
            let step =
                Self::apply_streaming_sed_arg(args, i, &mut suppress_print, &mut expressions)?;
            match step {
                StreamingSedStep::Advance(n) => i += n,
                StreamingSedStep::Break => break,
            }
        }
        if expressions.is_empty() {
            return None;
        }
        let script = expressions.join(";");
        let instructions = parse_streaming_sed_script(&script);
        if instructions.is_empty() {
            return None;
        }
        Some(StreamingPipelineStage::Sed(StreamingSedStage {
            suppress_print,
            instructions,
        }))
    }

    fn apply_streaming_sed_arg(
        args: &[String],
        i: usize,
        suppress_print: &mut bool,
        expressions: &mut Vec<String>,
    ) -> Option<StreamingSedStep> {
        let arg = args[i].as_str();
        if arg == "-n" {
            *suppress_print = true;
            return Some(StreamingSedStep::Advance(1));
        }
        if arg == "-e" && i + 1 < args.len() {
            expressions.push(args[i + 1].clone());
            return Some(StreamingSedStep::Advance(2));
        }
        if arg == "-E" || arg == "-r" {
            return Some(StreamingSedStep::Advance(1));
        }
        if Self::streaming_sed_arg_rejected(arg) {
            return None;
        }
        if arg == "--" {
            return Self::streaming_sed_handle_doubledash(args, i, expressions);
        }
        if expressions.is_empty() {
            expressions.push(args[i].clone());
            Some(StreamingSedStep::Advance(1))
        } else {
            None
        }
    }

    fn streaming_sed_arg_rejected(arg: &str) -> bool {
        arg == "-f"
            || arg == "-i"
            || arg.starts_with("-i")
            || (arg.starts_with('-') && arg.len() > 1 && arg != "--")
    }

    fn streaming_sed_handle_doubledash(
        args: &[String],
        i: usize,
        expressions: &mut Vec<String>,
    ) -> Option<StreamingSedStep> {
        if i + 1 >= args.len() {
            return Some(StreamingSedStep::Break);
        }
        if !expressions.is_empty() {
            return None;
        }
        expressions.push(args[i + 1].clone());
        Some(StreamingSedStep::Advance(2))
    }

    fn parse_streaming_paste_stage(args: &[String]) -> Option<StreamingPipelineStage> {
        let mut delimiter = "\t".to_string();
        let mut serial = false;
        let mut i = 0usize;
        while i < args.len() {
            i = Self::apply_streaming_paste_arg(args, i, &mut delimiter, &mut serial)?;
        }
        Some(StreamingPipelineStage::Paste(StreamingPasteStage {
            delimiter,
            serial,
        }))
    }

    fn apply_streaming_paste_arg(
        args: &[String],
        i: usize,
        delimiter: &mut String,
        serial: &mut bool,
    ) -> Option<usize> {
        let arg = args[i].as_str();
        if arg == "-d" && i + 1 < args.len() {
            delimiter.clone_from(&args[i + 1]);
            return Some(i + 2);
        }
        if arg == "-s" {
            *serial = true;
            return Some(i + 1);
        }
        if arg == "--" {
            return (i + 1 == args.len()).then_some(i + 1);
        }
        if arg.starts_with('-') && arg.len() > 1 {
            let extra = Self::apply_streaming_paste_short_cluster(args, i, delimiter, serial)?;
            return Some(i + 1 + extra);
        }
        None
    }

    fn apply_streaming_paste_short_cluster(
        args: &[String],
        i: usize,
        delimiter: &mut String,
        serial: &mut bool,
    ) -> Option<usize> {
        let arg = args[i].as_str();
        let mut extra = 0usize;
        for ch in arg[1..].chars() {
            match ch {
                's' => *serial = true,
                'd' if i + 1 < args.len() => {
                    delimiter.clone_from(&args[i + 1]);
                    extra = 1;
                }
                _ => return None,
            }
        }
        Some(extra)
    }

    fn parse_streaming_tee_stage(args: &[String]) -> Option<StreamingPipelineStage> {
        let mut append = false;
        let mut paths = Vec::new();
        let mut i = 0usize;
        while i < args.len() {
            let arg = args[i].as_str();
            if arg == "-a" {
                append = true;
                i += 1;
            } else if arg == "-i" {
                i += 1;
            } else if arg == "--" {
                paths.extend(args[i + 1..].iter().cloned());
                break;
            } else if arg.starts_with('-') && arg.len() > 1 {
                for ch in arg[1..].chars() {
                    match ch {
                        'a' => append = true,
                        'i' => {}
                        _ => return None,
                    }
                }
                i += 1;
            } else {
                paths.push(args[i].clone());
                i += 1;
            }
        }
        Some(StreamingPipelineStage::Tee(StreamingTeeStage {
            append,
            paths,
        }))
    }

    fn parse_streaming_column_stage(args: &[String]) -> Option<StreamingPipelineStage> {
        let mut i = 0usize;
        while i < args.len() {
            let arg = args[i].as_str();
            if arg == "-t" {
                return None;
            }
            if arg == "-s" && i + 1 < args.len() {
                return None;
            }
            if arg.starts_with('-') && arg.len() > 1 {
                i += 1;
            } else if arg == "--" {
                if i + 1 != args.len() {
                    return None;
                }
                i += 1;
            } else {
                return None;
            }
        }
        Some(StreamingPipelineStage::Column(StreamingColumnStage))
    }

    fn parse_streaming_rev_stage(args: &[String]) -> Option<StreamingPipelineStage> {
        if args.iter().all(|arg| arg == "--") {
            Some(StreamingPipelineStage::Rev)
        } else {
            None
        }
    }

    fn parse_streaming_grep_stage(args: &[String]) -> Option<StreamingPipelineStage> {
        let mut flags = StreamingGrepFlags {
            ignore_case: false,
            invert: false,
            count_only: false,
            show_line_numbers: false,
            files_only: false,
            word_match: false,
            only_matching: false,
            quiet: false,
            extended: false,
            fixed: false,
            after_context: 0,
            before_context: 0,
            max_count: None,
            show_filename: None,
        };
        let mut patterns = Vec::new();
        let mut rest = Vec::new();
        let mut i = 0usize;
        while i < args.len() {
            let arg = args[i].as_str();
            if arg == "--" {
                rest.extend(args[i + 1..].iter().cloned());
                break;
            }
            if Self::streaming_grep_arg_rejected(arg) {
                return None;
            }
            match Self::parse_streaming_grep_value_flag(args, i, &mut flags, &mut patterns)? {
                StreamingGrepStep::Advance(delta) => {
                    i += delta;
                    continue;
                }
                StreamingGrepStep::NotMatched => {}
            }
            if arg.starts_with('-') && arg.len() > 1 {
                Self::apply_streaming_grep_short_flags(&arg[1..], &mut flags)?;
                i += 1;
            } else {
                rest.push(args[i].clone());
                i += 1;
            }
        }

        let (patterns, file_args) = if patterns.is_empty() {
            let first = rest.first()?.clone();
            (vec![first], rest[1..].to_vec())
        } else {
            (patterns, rest)
        };
        if !file_args.is_empty() {
            return None;
        }
        Some(StreamingPipelineStage::Grep(StreamingGrepStage {
            flags,
            patterns,
        }))
    }

    fn streaming_grep_arg_rejected(arg: &str) -> bool {
        arg.starts_with("--include=")
            || arg.starts_with("--exclude=")
            || arg == "--color"
            || arg.starts_with("--color=")
            || arg == "-r"
            || arg == "-R"
            || arg == "--recursive"
    }

    fn parse_streaming_grep_value_flag(
        args: &[String],
        i: usize,
        flags: &mut StreamingGrepFlags,
        patterns: &mut Vec<String>,
    ) -> Option<StreamingGrepStep> {
        let arg = args[i].as_str();
        // Glued numeric value: `-A2`, `-B10`, `-C3`, `-m5`.
        if arg.len() > 2 {
            let (letter, rest) = arg.split_at(2);
            if matches!(letter, "-A" | "-B" | "-C" | "-m") {
                let n: usize = rest.parse().ok()?;
                match letter {
                    "-A" => flags.after_context = n,
                    "-B" => flags.before_context = n,
                    "-C" => {
                        flags.before_context = n;
                        flags.after_context = n;
                    }
                    "-m" => flags.max_count = Some(n),
                    _ => unreachable!(),
                }
                return Some(StreamingGrepStep::Advance(1));
            }
        }
        let has_next = i + 1 < args.len();
        if !has_next {
            return Some(StreamingGrepStep::NotMatched);
        }
        match arg {
            "-e" => {
                patterns.push(args[i + 1].clone());
                Some(StreamingGrepStep::Advance(2))
            }
            "-f" => None,
            "-A" => {
                flags.after_context = args[i + 1].parse().ok()?;
                Some(StreamingGrepStep::Advance(2))
            }
            "-B" => {
                flags.before_context = args[i + 1].parse().ok()?;
                Some(StreamingGrepStep::Advance(2))
            }
            "-C" => {
                let n = args[i + 1].parse().ok()?;
                flags.before_context = n;
                flags.after_context = n;
                Some(StreamingGrepStep::Advance(2))
            }
            "-m" => {
                flags.max_count = args[i + 1].parse().ok();
                Some(StreamingGrepStep::Advance(2))
            }
            _ => Some(StreamingGrepStep::NotMatched),
        }
    }

    fn apply_streaming_grep_short_flags(
        short_flags: &str,
        flags: &mut StreamingGrepFlags,
    ) -> Option<()> {
        for ch in short_flags.chars() {
            match ch {
                'i' => flags.ignore_case = true,
                'v' => flags.invert = true,
                'c' => flags.count_only = true,
                'n' => flags.show_line_numbers = true,
                'l' => flags.files_only = true,
                'E' | 'P' => flags.extended = true,
                'F' => flags.fixed = true,
                'w' => flags.word_match = true,
                'o' => flags.only_matching = true,
                'q' => flags.quiet = true,
                'h' => flags.show_filename = Some(false),
                'H' => flags.show_filename = Some(true),
                'z' => {}
                _ => return None,
            }
        }
        Some(())
    }

    fn parse_streaming_uniq_stage(args: &[String]) -> Option<StreamingPipelineStage> {
        let mut flags = StreamingUniqFlags {
            count: false,
            duplicates_only: false,
            unique_only: false,
            ignore_case: false,
            skip_fields: 0,
            skip_chars: 0,
            compare_chars: None,
        };
        let mut i = 0usize;
        while i < args.len() {
            i = Self::apply_streaming_uniq_arg(args, i, &mut flags)?;
        }
        Some(StreamingPipelineStage::Uniq(flags))
    }

    fn apply_streaming_uniq_arg(
        args: &[String],
        i: usize,
        flags: &mut StreamingUniqFlags,
    ) -> Option<usize> {
        let arg = args[i].as_str();
        if arg == "--" {
            return Some(i + 1);
        }
        if i + 1 < args.len() {
            match arg {
                "-f" => {
                    flags.skip_fields = args[i + 1].parse().ok()?;
                    return Some(i + 2);
                }
                "-s" => {
                    flags.skip_chars = args[i + 1].parse().ok()?;
                    return Some(i + 2);
                }
                "-w" => {
                    flags.compare_chars = args[i + 1].parse().ok();
                    return Some(i + 2);
                }
                _ => {}
            }
        }
        if arg.starts_with('-') && arg.len() > 1 {
            Self::apply_streaming_uniq_short_cluster(&arg[1..], flags)?;
            return Some(i + 1);
        }
        None
    }

    fn apply_streaming_uniq_short_cluster(
        short_flags: &str,
        flags: &mut StreamingUniqFlags,
    ) -> Option<()> {
        for ch in short_flags.chars() {
            match ch {
                'c' => flags.count = true,
                'd' => flags.duplicates_only = true,
                'u' => flags.unique_only = true,
                'i' => flags.ignore_case = true,
                'z' => {}
                _ => return None,
            }
        }
        Some(())
    }

    fn parse_streaming_cut_ranges(spec: &str) -> Vec<StreamingCutRange> {
        spec.split(',')
            .filter_map(|part| {
                if let Some((start, end)) = part.split_once('-') {
                    Some(StreamingCutRange {
                        start: if start.is_empty() {
                            None
                        } else {
                            start.parse().ok()
                        },
                        end: if end.is_empty() {
                            None
                        } else {
                            end.parse().ok()
                        },
                    })
                } else {
                    let n: usize = part.parse().ok()?;
                    Some(StreamingCutRange {
                        start: Some(n),
                        end: Some(n),
                    })
                }
            })
            .collect()
    }

    fn parse_streaming_cut_stage(args: &[String]) -> Option<StreamingPipelineStage> {
        let mut state = StreamingCutParseState {
            delim: '\t',
            mode: None,
            complement: false,
            only_delimited: false,
            output_delim: None,
        };
        let mut i = 0usize;
        while i < args.len() {
            i = Self::apply_streaming_cut_arg(args, i, &mut state)?;
        }
        Some(StreamingPipelineStage::Cut(StreamingCutStage {
            mode: state.mode?,
            delim: state.delim,
            complement: state.complement,
            only_delimited: state.only_delimited,
            output_delim: state
                .output_delim
                .unwrap_or_else(|| state.delim.to_string()),
        }))
    }

    fn apply_streaming_cut_arg(
        args: &[String],
        i: usize,
        state: &mut StreamingCutParseState,
    ) -> Option<usize> {
        let arg = args[i].as_str();
        if let Some(advance) = Self::streaming_cut_try_mode_flag(args, i, &mut state.mode) {
            return Some(advance);
        }
        if let Some(advance) = Self::streaming_cut_try_delim_flag(args, i, &mut state.delim) {
            return Some(advance);
        }
        match arg {
            "--complement" => {
                state.complement = true;
                Some(i + 1)
            }
            "-s" => {
                state.only_delimited = true;
                Some(i + 1)
            }
            "-z" | "--" => Some(i + 1),
            _ => {
                if let Some(out) = arg.strip_prefix("--output-delimiter=") {
                    state.output_delim = Some(out.to_string());
                    Some(i + 1)
                } else {
                    None
                }
            }
        }
    }

    fn streaming_cut_try_mode_flag(
        args: &[String],
        i: usize,
        mode: &mut Option<StreamingCutMode>,
    ) -> Option<usize> {
        let arg = args[i].as_str();
        let (flag, wrap): (&str, fn(Vec<StreamingCutRange>) -> StreamingCutMode) =
            if arg == "-f" || arg.starts_with("-f") {
                ("-f", StreamingCutMode::Fields)
            } else if arg == "-c" || arg.starts_with("-c") {
                ("-c", StreamingCutMode::Chars)
            } else if arg == "-b" || arg.starts_with("-b") {
                ("-b", StreamingCutMode::Bytes)
            } else {
                return None;
            };
        if arg == flag && i + 1 < args.len() {
            *mode = Some(wrap(Self::parse_streaming_cut_ranges(&args[i + 1])));
            return Some(i + 2);
        }
        if let Some(spec) = arg.strip_prefix(flag) {
            if !spec.is_empty() {
                *mode = Some(wrap(Self::parse_streaming_cut_ranges(spec)));
                return Some(i + 1);
            }
        }
        None
    }

    fn streaming_cut_try_delim_flag(args: &[String], i: usize, delim: &mut char) -> Option<usize> {
        let arg = args[i].as_str();
        if arg == "-d" && i + 1 < args.len() {
            *delim = args[i + 1].chars().next().unwrap_or('\t');
            return Some(i + 2);
        }
        if arg.starts_with("-d") && arg.len() > 2 {
            *delim = arg[2..].chars().next().unwrap_or('\t');
            return Some(i + 1);
        }
        None
    }

    fn parse_streaming_tr_stage(args: &[String]) -> Option<StreamingPipelineStage> {
        let mut delete = false;
        let mut squeeze = false;
        let mut complement = false;
        let mut set_args = Vec::new();
        for arg in args {
            if arg.starts_with('-') && arg.len() > 1 {
                Self::apply_streaming_tr_flags(
                    &arg[1..],
                    &mut delete,
                    &mut squeeze,
                    &mut complement,
                )?;
            } else {
                set_args.push(arg.as_str());
            }
        }
        let from_chars = streaming_tr_expand_set(set_args.first()?);
        let to_chars = Self::streaming_tr_resolve_to_chars(&set_args, delete, squeeze)?;
        Some(StreamingPipelineStage::Tr(StreamingTrStage {
            delete,
            squeeze,
            complement,
            from_chars,
            to_chars,
        }))
    }

    fn apply_streaming_tr_flags(
        flags: &str,
        delete: &mut bool,
        squeeze: &mut bool,
        complement: &mut bool,
    ) -> Option<()> {
        for ch in flags.chars() {
            match ch {
                'd' => *delete = true,
                's' => *squeeze = true,
                'c' | 'C' => *complement = true,
                't' => {}
                _ => return None,
            }
        }
        Some(())
    }

    fn streaming_tr_resolve_to_chars(
        set_args: &[&str],
        delete: bool,
        squeeze: bool,
    ) -> Option<Vec<char>> {
        if delete {
            let to = if squeeze && set_args.len() >= 2 {
                streaming_tr_expand_set(set_args[1])
            } else {
                Vec::new()
            };
            return Some(to);
        }
        if squeeze && set_args.len() < 2 {
            return Some(Vec::new());
        }
        if set_args.len() < 2 {
            return None;
        }
        Some(streaming_tr_expand_set(set_args[1]))
    }

    fn parse_streaming_wc_stage(args: &[String]) -> Option<StreamingPipelineStage> {
        let mut flags = StreamingWcFlags {
            lines: false,
            words: false,
            bytes: false,
            max_line_length: false,
        };
        let mut parsing_flags = true;
        for arg in args {
            if !Self::apply_streaming_wc_arg(arg, &mut flags, &mut parsing_flags)? {
                return None;
            }
        }
        if !flags.lines && !flags.words && !flags.bytes && !flags.max_line_length {
            flags.lines = true;
            flags.words = true;
            flags.bytes = true;
        }
        Some(StreamingPipelineStage::Wc(flags))
    }

    fn apply_streaming_wc_arg(
        arg: &str,
        flags: &mut StreamingWcFlags,
        parsing_flags: &mut bool,
    ) -> Option<bool> {
        if *parsing_flags && arg == "--" {
            *parsing_flags = false;
            return Some(true);
        }
        if *parsing_flags && arg.starts_with('-') && arg.len() > 1 {
            Self::apply_streaming_wc_short_cluster(&arg[1..], flags)?;
            return Some(true);
        }
        Some(false)
    }

    fn apply_streaming_wc_short_cluster(short: &str, flags: &mut StreamingWcFlags) -> Option<()> {
        for ch in short.chars() {
            match ch {
                'l' => flags.lines = true,
                'w' => flags.words = true,
                'c' | 'm' => flags.bytes = true,
                'L' => flags.max_line_length = true,
                _ => return None,
            }
        }
        Some(())
    }

    fn set_pipestatus(&mut self, statuses: &[i32]) {
        let status_key = smol_str::SmolStr::from("PIPESTATUS");
        self.vm.state.init_indexed_array(status_key.clone());
        for (i, s) in statuses.iter().enumerate() {
            self.vm.state.set_array_element(
                status_key.clone(),
                &i.to_string(),
                smol_str::SmolStr::from(s.to_string()),
            );
        }
    }

    fn open_streaming_file_reader(
        &mut self,
        path: &str,
        cmd_name: &str,
    ) -> Result<Box<dyn Read>, ()> {
        let resolved = self.resolve_cwd_path(path);
        match Self::open_streaming_file_reader_in_fs(&mut self.fs, &resolved) {
            Ok(reader) => Ok(reader),
            Err(err) => {
                let msg =
                    format!("wasmsh: {cmd_name}: failed to open stdin source {resolved}: {err}\n");
                self.write_stderr(msg.as_bytes());
                self.vm.state.last_status = 1;
                Err(())
            }
        }
    }

    fn open_streaming_file_reader_in_fs(
        fs: &mut BackendFs,
        resolved: &str,
    ) -> Result<Box<dyn Read>, String> {
        let handle = fs
            .open(resolved, OpenOptions::read())
            .map_err(|err| err.to_string())?;
        let reader_result = fs.stream_file(handle).map_err(|err| err.to_string());
        fs.close(handle);
        reader_result
    }

    fn execute_inner_capture_stdout(&mut self, input: &str) -> Vec<u8> {
        let events = self.execute_isolated_input_events(input, None);
        let mut stdout = Vec::new();
        let mut exit_status = None;
        for event in events {
            match event {
                WorkerEvent::Stdout(data) => stdout.extend_from_slice(&data),
                WorkerEvent::Stderr(data) => self.write_stderr(&data),
                WorkerEvent::Exit(status) => exit_status = Some(status),
                WorkerEvent::Diagnostic(level, msg) => self.vm.emit_diagnostic(
                    convert_diag_level(level),
                    wasmsh_vm::DiagCategory::Runtime,
                    msg,
                ),
                _ => {}
            }
        }
        if let Some(status) = exit_status {
            self.last_subst_status = Some(status);
        }
        stdout
    }

    fn execute_isolated_input_events(
        &mut self,
        input: &str,
        pending_input: Option<InputTarget>,
    ) -> Vec<WorkerEvent> {
        let saved_state = self.vm.state.clone();
        let saved_functions = self.functions.clone();
        let saved_aliases = self.aliases.clone();
        let saved_exec = self.exec.clone();
        let saved_exec_io = self.current_exec_io.take();
        let saved_stdout = std::mem::take(&mut self.vm.stdout);
        let saved_stderr = std::mem::take(&mut self.vm.stderr);
        let saved_diagnostics = std::mem::take(&mut self.vm.diagnostics);
        let saved_output_bytes = self.vm.output_bytes;
        let saved_proc_subst_out_scopes = std::mem::take(&mut self.proc_subst_out_scopes);
        let saved_proc_subst_in_scopes = std::mem::take(&mut self.proc_subst_in_scopes);

        self.current_exec_io = pending_input.map(|target| {
            let mut exec_io = ExecIo::default();
            exec_io.fds_mut().set_input(target);
            exec_io
        });
        let (mut inner_events, captured) = self.with_output_capture(true, true, |runtime| {
            runtime.with_nested_shell_scope(|nested| nested.execute_input_inner(input))
        });
        let inner_status = self.vm.state.last_status;
        let inner_resource_exhausted = self.exec.resource_exhausted;
        let inner_diagnostics = self
            .vm
            .diagnostics
            .drain(..)
            .map(|diag| {
                WorkerEvent::Diagnostic(Self::to_protocol_diag_level(diag.level), diag.message)
            })
            .collect::<Vec<_>>();
        self.clear_pending_input();
        for scope in self.proc_subst_out_scopes.drain(..) {
            for sink in scope {
                let _ = self.fs.remove_file(&sink.path);
            }
        }
        for scope in self.proc_subst_in_scopes.drain(..) {
            for sink in scope {
                let _ = self.fs.remove_file(&sink.path);
            }
        }

        self.vm.state = saved_state;
        self.functions = saved_functions;
        self.aliases = saved_aliases;
        self.exec = saved_exec;
        self.exec.resource_exhausted |= inner_resource_exhausted;
        self.current_exec_io = saved_exec_io;
        self.vm.stdout = saved_stdout;
        self.vm.stderr = saved_stderr;
        self.vm.diagnostics = saved_diagnostics;
        self.vm.output_bytes = saved_output_bytes;
        self.vm.budget.visible_output_bytes = saved_output_bytes;
        self.proc_subst_out_scopes = saved_proc_subst_out_scopes;
        self.proc_subst_in_scopes = saved_proc_subst_in_scopes;

        let mut events = Self::seed_isolated_events_from_capture(captured);
        Self::merge_isolated_inner_events(&mut events, inner_events.drain(..));
        events.extend(inner_diagnostics);
        // Record the isolated scope's final status so a command substitution
        // can report it even though the shell state is restored afterwards.
        self.last_subst_status = Some(inner_status);
        events
    }

    fn seed_isolated_events_from_capture(capture: CapturedOutput) -> Vec<WorkerEvent> {
        let mut events = Vec::new();
        if !capture.stdout.is_empty() {
            events.push(WorkerEvent::Stdout(capture.stdout));
        }
        if !capture.stderr.is_empty() {
            events.push(WorkerEvent::Stderr(capture.stderr));
        }
        events
    }

    fn merge_isolated_inner_events(
        events: &mut Vec<WorkerEvent>,
        inner_events: impl IntoIterator<Item = WorkerEvent>,
    ) {
        for event in inner_events {
            match &event {
                WorkerEvent::Stdout(_)
                    if !events.iter().any(|e| matches!(e, WorkerEvent::Stdout(_))) =>
                {
                    events.push(event);
                }
                WorkerEvent::Stderr(_)
                    if !events.iter().any(|e| matches!(e, WorkerEvent::Stderr(_))) =>
                {
                    events.push(event);
                }
                WorkerEvent::Stdout(_) | WorkerEvent::Stderr(_) => {}
                _ => events.push(event),
            }
        }
    }

    fn execute_isolated_scheduled_pipeline_events_from_reader(
        &mut self,
        pipeline: &HirPipeline,
        reader: Box<dyn Read>,
    ) -> Vec<WorkerEvent> {
        let saved_state = self.vm.state.clone();
        let saved_functions = self.functions.clone();
        let saved_aliases = self.aliases.clone();
        let saved_exec = self.exec.clone();
        let saved_exec_io = self.current_exec_io.take();
        let saved_stdout = std::mem::take(&mut self.vm.stdout);
        let saved_stderr = std::mem::take(&mut self.vm.stderr);
        let saved_diagnostics = std::mem::take(&mut self.vm.diagnostics);
        let saved_output_bytes = self.vm.output_bytes;
        let saved_proc_subst_out_scopes = std::mem::take(&mut self.proc_subst_out_scopes);
        let saved_proc_subst_in_scopes = std::mem::take(&mut self.proc_subst_in_scopes);

        self.current_exec_io = None;
        self.proc_subst_out_scopes.clear();
        self.proc_subst_in_scopes.clear();
        self.exec.recursion_depth += 1;
        if let Err(reason) = self
            .vm
            .budget
            .enter_recursion(self.vm.limits.recursion_limit)
        {
            self.exec.recursion_depth -= 1;
            self.vm.state = saved_state;
            self.functions = saved_functions;
            self.aliases = saved_aliases;
            self.exec = saved_exec;
            self.current_exec_io = saved_exec_io;
            self.vm.stdout = saved_stdout;
            self.vm.stderr = saved_stderr;
            self.vm.diagnostics = saved_diagnostics;
            self.vm.output_bytes = saved_output_bytes;
            self.vm.budget.visible_output_bytes = saved_output_bytes;
            self.proc_subst_out_scopes = saved_proc_subst_out_scopes;
            self.proc_subst_in_scopes = saved_proc_subst_in_scopes;
            self.mark_recursion_exhaustion(reason);
            return vec![WorkerEvent::Stderr(
                b"wasmsh: maximum recursion depth exceeded\n".to_vec(),
            )];
        }

        let ((), captured) = self.with_output_capture(true, true, |runtime| {
            runtime.with_nested_shell_scope(|nested| {
                nested.execute_scheduled_pipeline_with_source_reader(
                    &pipeline.commands,
                    pipeline,
                    Some(reader),
                );
            });
        });
        self.exec.recursion_depth -= 1;
        self.vm.budget.exit_recursion();
        let inner_resource_exhausted = self.exec.resource_exhausted;
        let inner_diagnostics = self
            .vm
            .diagnostics
            .drain(..)
            .map(|diag| {
                WorkerEvent::Diagnostic(Self::to_protocol_diag_level(diag.level), diag.message)
            })
            .collect::<Vec<_>>();
        self.clear_pending_input();
        let pending_scopes: Vec<Vec<PendingProcessSubstOut>> =
            self.proc_subst_out_scopes.drain(..).collect();
        for scope in pending_scopes {
            for sink in scope {
                self.flush_process_subst_out(sink);
            }
        }
        let pending_in_scopes: Vec<Vec<PendingProcessSubstIn>> =
            self.proc_subst_in_scopes.drain(..).collect();
        for scope in pending_in_scopes {
            self.flush_process_subst_in_scope(scope);
        }

        self.vm.state = saved_state;
        self.functions = saved_functions;
        self.aliases = saved_aliases;
        self.exec = saved_exec;
        self.exec.resource_exhausted |= inner_resource_exhausted;
        self.current_exec_io = saved_exec_io;
        self.vm.stdout = saved_stdout;
        self.vm.stderr = saved_stderr;
        self.vm.diagnostics = saved_diagnostics;
        self.vm.output_bytes = saved_output_bytes;
        self.vm.budget.visible_output_bytes = saved_output_bytes;
        self.proc_subst_out_scopes = saved_proc_subst_out_scopes;
        self.proc_subst_in_scopes = saved_proc_subst_in_scopes;

        let mut events = Vec::new();
        if !captured.stdout.is_empty() {
            events.push(WorkerEvent::Stdout(captured.stdout));
        }
        if !captured.stderr.is_empty() {
            events.push(WorkerEvent::Stderr(captured.stderr));
        }
        events.extend(inner_diagnostics);
        events
    }

    /// Execute a command substitution and return the trimmed output.
    fn execute_subst(&mut self, inner: &str) -> smol_str::SmolStr {
        let stdout = self.execute_inner_capture_stdout(inner);
        let result = String::from_utf8_lossy(&stdout).to_string();
        smol_str::SmolStr::from(result.trim_end_matches('\n'))
    }

    fn word_parts_require_runtime_expansion(parts: &[WordPart]) -> bool {
        parts.iter().any(|part| match part {
            WordPart::Literal(_) | WordPart::SingleQuoted(_) => false,
            WordPart::DoubleQuoted(inner) => Self::word_parts_require_runtime_expansion(inner),
            WordPart::Parameter(_)
            | WordPart::Arithmetic(_)
            | WordPart::CommandSubstitution(_)
            | WordPart::ProcessSubstIn(_)
            | WordPart::ProcessSubstOut(_)
            | _ => true,
        })
    }

    fn command_requires_runtime_expansion(cmd: &HirCommand) -> bool {
        let HirCommand::Exec(exec) = cmd else {
            return false;
        };
        exec.argv
            .iter()
            .any(|word| Self::word_parts_require_runtime_expansion(&word.parts))
    }

    fn command_needs_full_single_stage_execution(&self, cmd: &HirCommand) -> bool {
        if self.vm.state.get_var("SHOPT_x").as_deref() == Some("1") {
            return true;
        }
        let HirCommand::Exec(exec) = cmd else {
            return false;
        };
        exec.argv.iter().any(Self::word_has_brace_or_glob_literal)
    }

    fn word_has_brace_or_glob_literal(word: &Word) -> bool {
        word.parts
            .iter()
            .any(Self::word_part_has_brace_or_glob_literal)
    }

    fn word_part_has_brace_or_glob_literal(part: &WordPart) -> bool {
        match part {
            WordPart::Literal(text) | WordPart::SingleQuoted(text) | WordPart::Parameter(text) => {
                Self::text_has_brace_or_glob_literal(text)
            }
            WordPart::DoubleQuoted(parts) => {
                parts.iter().any(Self::word_part_has_brace_or_glob_literal)
            }
            WordPart::Arithmetic(_) => false,
            WordPart::CommandSubstitution(_)
            | WordPart::ProcessSubstIn(_)
            | WordPart::ProcessSubstOut(_)
            | _ => true,
        }
    }

    fn text_has_brace_or_glob_literal(text: &str) -> bool {
        text.contains('{')
            || text.contains('}')
            || text.contains('*')
            || text.contains('?')
            || text.contains('[')
    }

    fn parse_single_pipeline_input(input: &str) -> Option<HirPipeline> {
        let ast = wasmsh_parse::parse(input).ok()?;
        let hir = wasmsh_hir::lower(&ast);
        let cc = hir.items.first()?;
        if hir.items.len() != 1 || cc.list.len() != 1 {
            return None;
        }
        let and_or = cc.list.first()?;
        if !and_or.rest.is_empty() {
            return None;
        }
        Some(and_or.first.clone())
    }

    /// Counter for generating unique temp file paths for process substitution.
    fn next_proc_subst_id() -> u64 {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn next_pending_input_id() -> u64 {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn set_pending_input_bytes(&mut self, data: Vec<u8>) {
        self.current_exec_io
            .get_or_insert_with(ExecIo::default)
            .fds_mut()
            .set_input(InputTarget::Bytes(data));
    }

    fn set_pending_input_file(&mut self, path: String, remove_after_read: bool) {
        self.current_exec_io
            .get_or_insert_with(ExecIo::default)
            .fds_mut()
            .set_input(InputTarget::File {
                path,
                remove_after_read,
            });
    }

    fn clear_pending_input(&mut self) {
        let Some(exec_io) = self.current_exec_io.as_mut() else {
            return;
        };
        if let InputTarget::File {
            path,
            remove_after_read: true,
        } = exec_io.take_stdin()
        {
            let _ = self.fs.remove_file(&path);
        }
    }

    fn take_pending_input_reader(&mut self, cmd_name: &str) -> Result<Option<Box<dyn Read>>, ()> {
        let Some(exec_io) = self.current_exec_io.as_mut() else {
            return Ok(None);
        };
        match exec_io.take_stdin() {
            InputTarget::Inherit | InputTarget::Closed => Ok(None),
            InputTarget::Bytes(data) => Ok(Some(Box::new(Cursor::new(data)))),
            InputTarget::File {
                path,
                remove_after_read,
            } => {
                let reader_result = self.open_streaming_file_reader(&path, cmd_name);
                if remove_after_read {
                    let _ = self.fs.remove_file(&path);
                }
                reader_result.map(Some)
            }
            InputTarget::Pipe(pipe) => Ok(Some(Box::new(PipeReader::new(pipe)))),
        }
    }

    fn take_builtin_stdin(
        &mut self,
        cmd_name: &str,
    ) -> Result<Option<wasmsh_builtins::BuiltinStdin<'static>>, ()> {
        let reader = self.take_pending_input_reader(cmd_name)?;
        Ok(reader.map(wasmsh_builtins::BuiltinStdin::from_reader))
    }

    fn take_util_stdin(
        &mut self,
        cmd_name: &str,
    ) -> Result<Option<wasmsh_utils::UtilStdin<'static>>, ()> {
        // A regular-file redirect has a known size; expose it so `wc` can
        // match GNU's width choice for seekable input.
        let size_hint = match self.current_exec_io.as_ref().map(ExecIo::stdin_target_kind) {
            Some(InputTarget::File { path, .. }) if !path.starts_with("/tmp/_wasmsh_pipe_") => {
                self.fs.stat(&path).ok().map(|m| m.size)
            }
            _ => None,
        };
        let reader = self.take_pending_input_reader(cmd_name)?;
        Ok(reader.map(|reader| match size_hint {
            Some(size) => wasmsh_utils::UtilStdin::from_sized_reader(reader, size),
            None => wasmsh_utils::UtilStdin::from_reader(reader),
        }))
    }

    fn take_external_stdin(
        &mut self,
        cmd_name: &str,
        max_bytes: u64,
    ) -> Result<Option<ExternalCommandStdin<'static>>, ()> {
        let reader = self.take_pending_input_reader(cmd_name)?;
        Ok(reader.map(|reader| ExternalCommandStdin::from_limited_reader(reader, max_bytes)))
    }

    fn can_use_isolated_process_subst_runtime(&self) -> bool {
        self.external_handler.is_none()
            && self.external_spec_handler.is_none()
            && self.external_specs.is_empty()
            && self.network.is_none()
    }

    fn clone_for_isolated_process_subst(&self) -> Option<Self> {
        if !self.can_use_isolated_process_subst_runtime() {
            return None;
        }
        let mut exec = ExecState::new();
        exec.recursion_depth = self.exec.recursion_depth;
        Some(Self {
            config: self.config.clone(),
            vm: Vm::with_limits(self.vm.state.clone(), self.vm.limits.clone()),
            fs: self.fs.clone(),
            utils: UtilRegistry::new(),
            builtins: wasmsh_builtins::BuiltinRegistry::new(),
            initialized: self.initialized,
            current_exec_io: None,
            proc_subst_out_scopes: Vec::new(),
            proc_subst_in_scopes: Vec::new(),
            functions: self.functions.clone(),
            exec,
            aliases: self.aliases.clone(),
            external_handler: None,
            external_specs: IndexMap::new(),
            external_spec_handler: None,
            external_stream_handler: None,
            allow_external_streaming: false,
            network: None,
            network_policy: None,
            network_policy_state: None,
            network_is_policy_wrapped: false,
            clock: self.clock.clone(),
            monotonic_origin_ms: self.monotonic_origin_ms,
            active_run: None,
            pending_streaming_pipeline: None,
            pending_signals: VecDeque::new(),
            last_subst_status: None,
        })
    }

    fn build_live_process_subst_pipeline(
        &mut self,
        pipeline: &HirPipeline,
        source_pipe: Option<Rc<RefCell<PipeBuffer>>>,
    ) -> Option<(
        Vec<StreamingPipeProcess<'static>>,
        Vec<Rc<RefCell<Vec<u8>>>>,
        Vec<bool>,
        Rc<RefCell<PipeBuffer>>,
        Vec<Rc<RefCell<i32>>>,
    )> {
        let stages: Vec<StreamingPipelineStage> = pipeline
            .commands
            .iter()
            .enumerate()
            .map(|(idx, cmd)| self.compile_pipeline_stage(cmd, idx == 0 && source_pipe.is_none()))
            .collect();
        let stage_statuses: Vec<Rc<RefCell<i32>>> = stages
            .iter()
            .map(|stage| {
                Rc::new(RefCell::new(i32::from(matches!(
                    stage,
                    StreamingPipelineStage::Grep(_)
                ))))
            })
            .collect();
        let stage_stderr: Vec<Rc<RefCell<Vec<u8>>>> = stages
            .iter()
            .map(|_| Rc::new(RefCell::new(Vec::new())))
            .collect();
        let stage_pipe_stderr = vec![false; stages.len()];
        let output_pipes: Vec<Rc<RefCell<PipeBuffer>>> = (0..stages.len())
            .map(|_| Rc::new(RefCell::new(PipeBuffer::new(PIPEBUFFER_STREAMING_CAPACITY))))
            .collect();
        let mut processes = Vec::new();

        let ctx = StreamingStageCtx {
            stages: &stages,
            stage_pipe_stderr: &stage_pipe_stderr,
            stage_statuses: &stage_statuses,
            stage_stderr: &stage_stderr,
            output_pipes: &output_pipes,
        };

        self.setup_process_subst_stages(source_pipe, &ctx, &mut processes)?;

        let final_pipe = output_pipes.last().cloned()?;
        Some((
            processes,
            stage_stderr,
            stage_pipe_stderr,
            final_pipe,
            stage_statuses,
        ))
    }

    /// Wire the first stage (with or without an upstream pipe) and then every
    /// subsequent stage of a process-substitution pipeline. Extracted from
    /// `build_live_process_subst_pipeline` to keep its cognitive complexity
    /// under the project threshold.
    fn setup_process_subst_stages(
        &mut self,
        source_pipe: Option<Rc<RefCell<PipeBuffer>>>,
        ctx: &StreamingStageCtx<'_>,
        processes: &mut Vec<StreamingPipeProcess<'static>>,
    ) -> Option<()> {
        if let Some(source_pipe) = source_pipe {
            self.setup_process_subst_first_stage_from_pipe(source_pipe, ctx, processes)?;
        } else {
            self.setup_process_subst_first_stage_standalone(ctx, processes)?;
        }
        for idx in 1..ctx.stages.len() {
            self.setup_process_subst_later_stage(idx, ctx, processes)?;
        }
        Some(())
    }

    /// First stage of a process-substitution pipeline when there is an upstream
    /// pipe feeding it. Returns `None` if wrapping is required but fails, which
    /// aborts pipeline construction.
    fn setup_process_subst_first_stage_from_pipe(
        &mut self,
        source_pipe: Rc<RefCell<PipeBuffer>>,
        ctx: &StreamingStageCtx<'_>,
        processes: &mut Vec<StreamingPipeProcess<'static>>,
    ) -> Option<()> {
        match &ctx.stages[0] {
            StreamingPipelineStage::Tee(stage) => {
                let reader = Box::new(PipeReader::new(source_pipe)) as Box<dyn Read>;
                processes.push(StreamingPipeProcess::Tee(TeePipeProcess::new(
                    reader,
                    ctx.output_pipes[0].clone(),
                    &mut self.fs,
                    self.vm.state.cwd.as_str(),
                    stage,
                    ctx.stage_stderr[0].clone(),
                    ctx.stage_statuses[0].clone(),
                    false,
                )));
            }
            StreamingPipelineStage::BufferedCommand(argv) => {
                processes.push(StreamingPipeProcess::Buffered(BufferedPipeProcess::new(
                    Some(source_pipe),
                    ctx.output_pipes[0].clone(),
                    argv.clone(),
                    false,
                    ctx.stage_stderr[0].clone(),
                    ctx.stage_statuses[0].clone(),
                )));
            }
            _ => {
                let reader = Box::new(PipeReader::new(source_pipe)) as Box<dyn Read>;
                let stage_reader = Self::wrap_non_tee_streaming_stage(
                    reader,
                    &ctx.stages[0],
                    0,
                    ctx.stage_statuses,
                )?;
                processes.push(StreamingPipeProcess::Read(PipeReadProcess::new(
                    stage_reader,
                    ctx.output_pipes[0].clone(),
                    ctx.stage_stderr[0].clone(),
                    ctx.stage_statuses[0].clone(),
                    "process-subst",
                    false,
                )));
            }
        }
        Some(())
    }

    /// First stage of a process-substitution pipeline with no upstream pipe —
    /// the stage itself supplies the initial data. Returns `None` for stages
    /// that cannot produce output without an input pipe.
    fn setup_process_subst_first_stage_standalone(
        &mut self,
        ctx: &StreamingStageCtx<'_>,
        processes: &mut Vec<StreamingPipeProcess<'static>>,
    ) -> Option<()> {
        let reader: Box<dyn Read> = match &ctx.stages[0] {
            StreamingPipelineStage::Literal(data) => Box::new(Cursor::new(data.clone())),
            StreamingPipelineStage::File(path) => {
                let resolved = self.resolve_cwd_path(path);
                self.open_streaming_file_reader(&resolved, "cat").ok()?
            }
            StreamingPipelineStage::Yes { line } => {
                Box::new(YesStreamReader::new(line.clone(), STREAMING_YES_MAX_LINES))
            }
            StreamingPipelineStage::BufferedCommand(argv) => {
                processes.push(StreamingPipeProcess::Buffered(BufferedPipeProcess::new(
                    None,
                    ctx.output_pipes[0].clone(),
                    argv.clone(),
                    false,
                    ctx.stage_stderr[0].clone(),
                    ctx.stage_statuses[0].clone(),
                )));
                return Some(());
            }
            _ => return None,
        };
        processes.push(StreamingPipeProcess::Read(PipeReadProcess::new(
            reader,
            ctx.output_pipes[0].clone(),
            ctx.stage_stderr[0].clone(),
            ctx.stage_statuses[0].clone(),
            "process-subst",
            false,
        )));
        Some(())
    }

    /// Subsequent stage (idx > 0) of a process-substitution pipeline. Each
    /// stage reads from the previous stage's output pipe.
    fn setup_process_subst_later_stage(
        &mut self,
        idx: usize,
        ctx: &StreamingStageCtx<'_>,
        processes: &mut Vec<StreamingPipeProcess<'static>>,
    ) -> Option<()> {
        match &ctx.stages[idx] {
            StreamingPipelineStage::Head(mode) => {
                processes.push(StreamingPipeProcess::Head(HeadPipeProcess::new(
                    ctx.output_pipes[idx - 1].clone(),
                    ctx.output_pipes[idx].clone(),
                    *mode,
                )));
            }
            StreamingPipelineStage::Tee(stage) => {
                let reader =
                    Box::new(PipeReader::new(ctx.output_pipes[idx - 1].clone())) as Box<dyn Read>;
                processes.push(StreamingPipeProcess::Tee(TeePipeProcess::new(
                    reader,
                    ctx.output_pipes[idx].clone(),
                    &mut self.fs,
                    self.vm.state.cwd.as_str(),
                    stage,
                    ctx.stage_stderr[idx].clone(),
                    ctx.stage_statuses[idx].clone(),
                    false,
                )));
            }
            StreamingPipelineStage::BufferedCommand(argv) => {
                processes.push(StreamingPipeProcess::Buffered(BufferedPipeProcess::new(
                    Some(ctx.output_pipes[idx - 1].clone()),
                    ctx.output_pipes[idx].clone(),
                    argv.clone(),
                    false,
                    ctx.stage_stderr[idx].clone(),
                    ctx.stage_statuses[idx].clone(),
                )));
            }
            _ => {
                let reader =
                    Box::new(PipeReader::new(ctx.output_pipes[idx - 1].clone())) as Box<dyn Read>;
                let stage_reader = Self::wrap_non_tee_streaming_stage(
                    reader,
                    &ctx.stages[idx],
                    idx,
                    ctx.stage_statuses,
                )?;
                processes.push(StreamingPipeProcess::Read(PipeReadProcess::new(
                    stage_reader,
                    ctx.output_pipes[idx].clone(),
                    ctx.stage_stderr[idx].clone(),
                    ctx.stage_statuses[idx].clone(),
                    "process-subst",
                    false,
                )));
            }
        }
        Some(())
    }

    fn try_build_live_process_subst_in_reader(
        &mut self,
        inner: &str,
    ) -> Option<(
        Box<dyn Read>,
        Rc<RefCell<Vec<u8>>>,
        Rc<RefCell<Vec<wasmsh_vm::DiagnosticEvent>>>,
    )> {
        let pipeline = Self::parse_single_pipeline_input(inner)?;
        let requires_runtime = pipeline.commands.iter().enumerate().any(|(idx, cmd)| {
            matches!(
                self.compile_pipeline_stage(cmd, idx == 0),
                StreamingPipelineStage::BufferedCommand(_)
            )
        });
        let mut isolated_runtime = if requires_runtime {
            self.clone_for_isolated_process_subst().map(Box::new)
        } else {
            None
        };
        let (processes, stage_stderr, stage_pipe_stderr, final_pipe, _) =
            if let Some(runtime) = isolated_runtime.as_mut() {
                runtime.build_live_process_subst_pipeline(&pipeline, None)?
            } else {
                if requires_runtime {
                    return None;
                }
                self.build_live_process_subst_pipeline(&pipeline, None)?
            };

        let flushed_stderr = Rc::new(RefCell::new(Vec::new()));
        let flushed_diagnostics = Rc::new(RefCell::new(Vec::new()));
        let reader = LiveProcessSubstInReader {
            isolated_runtime,
            processes,
            finished: vec![false; stage_stderr.len()],
            final_pipe,
            stage_stderr,
            stage_pipe_stderr,
            flushed_stderr: flushed_stderr.clone(),
            flushed_diagnostics: flushed_diagnostics.clone(),
            done: false,
        };
        Some((Box::new(reader), flushed_stderr, flushed_diagnostics))
    }

    /// Execute `<(cmd)` by registering a command-scoped readable path.
    fn execute_process_subst_in(&mut self, inner: &str) -> smol_str::SmolStr {
        let path = format!("/tmp/_proc_subst_{}", Self::next_proc_subst_id());
        if self.proc_subst_in_scopes.is_empty() {
            self.proc_subst_in_scopes.push(Vec::new());
        }

        if let Some((reader, stderr, diagnostics)) =
            self.try_build_live_process_subst_in_reader(inner)
        {
            if self.fs.install_stream_reader(&path, reader).is_ok() {
                self.proc_subst_in_scopes
                    .last_mut()
                    .expect("process substitution input scope stack is empty")
                    .push(PendingProcessSubstIn {
                        path: path.clone(),
                        stderr: Some(stderr),
                        diagnostics: Some(diagnostics),
                    });
                return smol_str::SmolStr::from(path);
            }
        }

        let output = self.execute_inner_capture_stdout(inner);
        if let Ok(h) = self.fs.open(&path, OpenOptions::write()) {
            let _ = self.fs.write_file(h, &output);
            self.fs.close(h);
        }
        self.proc_subst_in_scopes
            .last_mut()
            .expect("process substitution input scope stack is empty")
            .push(PendingProcessSubstIn {
                path: path.clone(),
                stderr: None,
                diagnostics: None,
            });
        smol_str::SmolStr::from(path)
    }

    fn try_build_live_process_subst_runner(
        &mut self,
        inner: &str,
    ) -> Option<LiveProcessSubstRunner> {
        let pipeline = Self::parse_single_pipeline_input(inner)?;
        let source_pipe = Rc::new(RefCell::new(PipeBuffer::new(PIPEBUFFER_STREAMING_CAPACITY)));
        let mut isolated_runtime = self.clone_for_isolated_process_subst();
        let (processes, stage_stderr, stage_pipe_stderr, final_pipe, _) =
            if let Some(runtime) = isolated_runtime.as_mut() {
                runtime.build_live_process_subst_pipeline(&pipeline, Some(source_pipe.clone()))?
            } else {
                self.build_live_process_subst_pipeline(&pipeline, Some(source_pipe.clone()))?
            };

        Some(LiveProcessSubstRunner {
            isolated_runtime: isolated_runtime.map(Box::new),
            source_pipe,
            processes,
            finished: vec![false; stage_stderr.len()],
            final_pipe,
            stage_stderr,
            stage_pipe_stderr,
            captured_stdout: Vec::new(),
            captured_stderr: Vec::new(),
            captured_diagnostics: Vec::new(),
            done: false,
            synced_steps: self.vm.steps,
        })
    }

    fn register_process_subst_out(&mut self, inner: &str) -> String {
        if self.proc_subst_out_scopes.is_empty() {
            self.proc_subst_out_scopes.push(Vec::new());
        }
        let path = format!("/tmp/_proc_subst_{}", Self::next_proc_subst_id());
        let mode = if let Some(runner) = self.try_build_live_process_subst_runner(inner) {
            PendingProcessSubstOutMode::Live { runner }
        } else {
            PendingProcessSubstOutMode::Buffered { data: Vec::new() }
        };
        self.proc_subst_out_scopes
            .last_mut()
            .expect("process substitution scope stack is empty")
            .push(PendingProcessSubstOut {
                path: path.clone(),
                inner: inner.to_string(),
                mode,
            });
        path
    }

    fn flush_process_subst_out_scope(&mut self, scope: Vec<PendingProcessSubstOut>) {
        for sink in scope {
            self.flush_process_subst_out(sink);
        }
    }

    fn flush_process_subst_in_scope(&mut self, scope: Vec<PendingProcessSubstIn>) {
        for sink in scope {
            if let Some(stderr) = sink.stderr {
                let data = stderr.borrow();
                if !data.is_empty() {
                    self.write_stderr(&data);
                }
            }
            if let Some(diagnostics) = sink.diagnostics {
                let mut diagnostics = diagnostics.borrow_mut();
                for event in diagnostics.drain(..) {
                    self.vm
                        .emit_diagnostic(event.level, event.category, event.message);
                }
            }
            let _ = self.fs.remove_file(&sink.path);
        }
    }

    fn flush_process_subst_out(&mut self, sink: PendingProcessSubstOut) {
        let saved_status = self.vm.state.last_status;
        match sink.mode {
            PendingProcessSubstOutMode::Buffered { data } => {
                self.flush_buffered_process_subst_out(&sink.inner, data);
            }
            PendingProcessSubstOutMode::Live { runner } => {
                self.flush_live_process_subst_out(runner);
            }
        }
        self.vm.state.last_status = saved_status;
    }

    fn flush_buffered_process_subst_out(&mut self, inner: &str, data: Vec<u8>) {
        let events = if let Some(pipeline) = Self::parse_single_pipeline_input(inner) {
            self.execute_isolated_scheduled_pipeline_events_from_reader(
                &pipeline,
                Box::new(Cursor::new(data.clone())),
            )
        } else {
            self.execute_isolated_input_events(inner, Some(InputTarget::Bytes(data)))
        };
        for event in events {
            self.apply_isolated_flush_event(event);
        }
    }

    fn apply_isolated_flush_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Stdout(data) => self.write_stdout(&data),
            WorkerEvent::Stderr(data) => self.write_stderr(&data),
            WorkerEvent::Diagnostic(level, msg) => self.vm.emit_diagnostic(
                convert_diag_level(level),
                wasmsh_vm::DiagCategory::Runtime,
                msg,
            ),
            _ => {}
        }
    }

    fn flush_live_process_subst_out(&mut self, mut runner: LiveProcessSubstRunner) {
        if runner.isolated_runtime.is_some() {
            runner.finish_with_parent(self);
        } else {
            runner.finish();
        }
        if !runner.captured_stdout.is_empty() {
            self.write_stdout(&runner.captured_stdout);
        }
        if !runner.captured_stderr.is_empty() {
            self.write_stderr(&runner.captured_stderr);
        }
        for diag in runner.captured_diagnostics {
            self.vm
                .emit_diagnostic(diag.level, diag.category, diag.message);
        }
    }

    /// Execute `>(cmd)` by creating a writable temp path and scheduling the
    /// consumer command to run once the enclosing command finishes writing to it.
    fn execute_process_subst_out(&mut self, inner: &str) -> smol_str::SmolStr {
        smol_str::SmolStr::from(self.register_process_subst_out(inner))
    }

    /// Resolve command substitutions in a list of words by executing them.
    fn resolve_command_subst(&mut self, words: &[Word]) -> Vec<Word> {
        words
            .iter()
            .map(|w| {
                let parts: Vec<WordPart> = w
                    .parts
                    .iter()
                    .map(|p| match p {
                        WordPart::CommandSubstitution(inner) => {
                            WordPart::Literal(self.execute_subst(inner))
                        }
                        WordPart::ProcessSubstIn(inner) => {
                            WordPart::Literal(self.execute_process_subst_in(inner))
                        }
                        WordPart::ProcessSubstOut(inner) => {
                            WordPart::Literal(self.execute_process_subst_out(inner))
                        }
                        // `$(( ... $(cmd) ... ))`: arithmetic is evaluated by
                        // the expansion layer, which has no runtime access, so
                        // resolve nested substitutions here first.
                        WordPart::Arithmetic(expr) => {
                            WordPart::Arithmetic(self.resolve_arith_command_subst(expr).into())
                        }
                        WordPart::DoubleQuoted(inner_parts) => {
                            let resolved: Vec<WordPart> = inner_parts
                                .iter()
                                .map(|ip| match ip {
                                    WordPart::CommandSubstitution(inner) => {
                                        WordPart::Literal(self.execute_subst(inner))
                                    }
                                    WordPart::ProcessSubstIn(inner) => {
                                        WordPart::Literal(self.execute_process_subst_in(inner))
                                    }
                                    WordPart::ProcessSubstOut(inner) => {
                                        WordPart::Literal(self.execute_process_subst_out(inner))
                                    }
                                    WordPart::Arithmetic(expr) => WordPart::Arithmetic(
                                        self.resolve_arith_command_subst(expr).into(),
                                    ),
                                    other => other.clone(),
                                })
                                .collect();
                            WordPart::DoubleQuoted(resolved)
                        }
                        other => other.clone(),
                    })
                    .collect();
                Word {
                    parts,
                    span: w.span,
                }
            })
            .collect()
    }

    /// Replace `$(...)` and `` `...` `` command substitutions inside an
    /// arithmetic expression with their output.
    fn resolve_arith_command_subst(&mut self, expr: &str) -> String {
        if !expr.contains('$') && !expr.contains('`') {
            return expr.to_string();
        }
        let bytes = expr.as_bytes();
        let mut out = String::with_capacity(expr.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'$'
                && bytes.get(i + 1) == Some(&b'(')
                && bytes.get(i + 2) == Some(&b'(')
            {
                // Nested `$(( ))` is left intact for the arithmetic evaluator.
                if let Some(end) = scan_arith_double_paren(bytes, i) {
                    out.push_str(&expr[i..end]);
                    i = end;
                    continue;
                }
            }
            if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'(') {
                if let Some((end, inner)) = scan_command_subst(bytes, i + 2) {
                    out.push_str(&self.execute_subst(inner));
                    i = end;
                    continue;
                }
            }
            if bytes[i] == b'`' {
                if let Some((end, inner)) = scan_backtick(bytes, i + 1) {
                    out.push_str(&self.execute_subst(inner));
                    i = end;
                    continue;
                }
            }
            out.push(bytes[i] as char);
            i += 1;
        }
        out
    }

    fn execute_command(&mut self, cmd: &HirCommand) {
        self.run_debug_trap_if_needed();
        self.proc_subst_out_scopes.push(Vec::new());
        self.proc_subst_in_scopes.push(Vec::new());
        self.execute_command_body(cmd);
        let in_scope = self
            .proc_subst_in_scopes
            .pop()
            .expect("process substitution input scope stack underflow");
        let scope = self
            .proc_subst_out_scopes
            .pop()
            .expect("process substitution scope stack underflow");
        self.flush_process_subst_out_scope(scope);
        self.flush_process_subst_in_scope(in_scope);
    }

    fn execute_command_body(&mut self, cmd: &HirCommand) {
        match cmd {
            HirCommand::Exec(exec) => self.execute_exec(exec),
            HirCommand::Assign(assign) => {
                self.last_subst_status = None;
                for a in &assign.assignments {
                    self.execute_assignment(&a.name, a.value.as_ref());
                }
                let stdout_before = self.current_stdout_len();
                self.apply_redirections(&assign.redirections, stdout_before);
                // A pure assignment reports the exit status of the last
                // command substitution in its value (bash: `x=$(false)` → 1).
                self.vm.state.last_status = self.last_subst_status.take().unwrap_or(0);
            }
            HirCommand::If(if_cmd) => {
                let redirs = if_cmd.redirections.clone();
                self.execute_compound_with_redirections(&redirs, |rt| rt.execute_if(if_cmd));
            }
            HirCommand::While(loop_cmd) => {
                let redirs = loop_cmd.redirections.clone();
                self.execute_compound_with_redirections(&redirs, |rt| {
                    rt.execute_while_loop(loop_cmd);
                });
            }
            HirCommand::Until(loop_cmd) => {
                let redirs = loop_cmd.redirections.clone();
                self.execute_compound_with_redirections(&redirs, |rt| {
                    rt.execute_until_loop(loop_cmd);
                });
            }
            HirCommand::For(for_cmd) => {
                let redirs = for_cmd.redirections.clone();
                self.execute_compound_with_redirections(&redirs, |rt| rt.execute_for_loop(for_cmd));
            }
            HirCommand::Group(block) => {
                let redirs = block.redirections.clone();
                self.execute_compound_with_redirections(&redirs, |rt| {
                    rt.execute_body(&block.body);
                });
            }
            HirCommand::Subshell(block) => {
                let redirs = block.redirections.clone();
                self.execute_compound_with_redirections(&redirs, |rt| {
                    rt.vm.state.env.push_scope();
                    rt.execute_body(&block.body);
                    rt.vm.state.env.pop_scope();
                });
            }
            HirCommand::Case(case_cmd) => {
                let redirs = case_cmd.redirections.clone();
                self.execute_compound_with_redirections(&redirs, |rt| rt.execute_case(case_cmd));
            }
            HirCommand::FunctionDef(fd) => {
                self.functions
                    .insert(fd.name.to_string(), (*fd.body).clone());
                self.vm.state.last_status = 0;
            }
            HirCommand::RedirectOnly(ro) => {
                let stdout_before = self.current_stdout_len();
                self.apply_redirections(&ro.redirections, stdout_before);
                self.vm.state.last_status = 0;
            }
            HirCommand::DoubleBracket(db) => {
                let result = self.eval_double_bracket(&db.words);
                self.vm.state.last_status = i32::from(!result);
            }
            HirCommand::ArithCommand(ac) => {
                let result = wasmsh_expand::eval_arithmetic(&ac.expr, &mut self.vm.state);
                self.vm.state.last_status = i32::from(result == 0);
            }
            HirCommand::ArithFor(af) => {
                let redirs = af.redirections.clone();
                self.execute_compound_with_redirections(&redirs, |rt| rt.execute_arith_for(af));
            }
            HirCommand::Select(sel) => self.execute_select(sel),
            _ => {}
        }
    }

    /// Apply a compound command's trailing redirections for the duration of `f`.
    fn execute_compound_with_redirections(
        &mut self,
        redirections: &[HirRedirection],
        f: impl FnOnce(&mut Self),
    ) {
        if redirections.is_empty() {
            f(self);
            return;
        }
        let Ok(exec_io) = self.prepare_exec_io(redirections) else {
            return;
        };
        self.with_exec_io_scope(exec_io, f);
    }

    /// Execute a simple command (`HirCommand::Exec`).
    fn execute_exec(&mut self, exec: &wasmsh_hir::HirExec) {
        let resolved = self.resolve_command_subst(&exec.argv);
        if self.exec.expansion_failed {
            return;
        }
        let expanded = expand_words_argv(&resolved, &mut self.vm.state);

        if self.check_nounset_error() || self.check_arith_error() || self.check_expansion_error() {
            return;
        }
        if expanded.is_empty() {
            return;
        }

        // Brace and glob expansion must be suppressed for quoted words (POSIX + bash).
        let tagged: Vec<(String, bool)> = expanded
            .into_iter()
            .flat_map(|ew| {
                if ew.was_quoted {
                    vec![(ew.text, true)]
                } else {
                    wasmsh_expand::expand_braces(&ew.text)
                        .into_iter()
                        .map(|s| (s, false))
                        .collect()
                }
            })
            .collect();
        let argv = self.expand_globs_tagged(tagged);

        for assignment in &exec.env {
            self.execute_assignment(&assignment.name, assignment.value.as_ref());
        }

        if self.try_alias_expansion(&argv) {
            return;
        }

        let Ok(exec_io) = self.prepare_exec_io(&exec.redirections) else {
            return;
        };
        self.with_exec_io_scope(exec_io, |runtime| {
            runtime.trace_command(&argv);
            runtime.execute_argv_command(&argv);
        });
    }

    /// Drain a pending nounset error from parameter expansion and report it
    /// through the fallback interpreter's stderr sink.
    fn check_nounset_error(&mut self) -> bool {
        let Some(var_name) = self.vm.state.take_nounset_error() else {
            return false;
        };
        let msg = format!("wasmsh: {var_name}: unbound variable\n");
        self.write_stderr(msg.as_bytes());
        self.vm.state.last_status = 1;
        // `set -u` on an unbound variable aborts a non-interactive script.
        self.exec.exit_requested = Some(1);
        true
    }

    /// Drain a pending hard arithmetic error and report it through the fallback
    /// interpreter's stderr sink.
    fn check_arith_error(&mut self) -> bool {
        let Some(message) = self.vm.state.take_arith_error() else {
            return false;
        };
        let msg = format!("wasmsh: {message}\n");
        self.write_stderr(msg.as_bytes());
        self.vm.state.last_status = 1;
        true
    }

    /// Drain a pending shell error (`${x:?message}`, readonly assignment).
    /// Fatal errors additionally request script exit, matching bash.
    fn check_expansion_error(&mut self) -> bool {
        let Some((fatal, message)) = self.vm.state.take_shell_error() else {
            return false;
        };
        let msg = format!("wasmsh: {message}\n");
        self.write_stderr(msg.as_bytes());
        self.vm.state.last_status = 1;
        if fatal {
            self.exec.exit_requested = Some(1);
        }
        true
    }

    /// Collect stdin from here-doc bodies or input redirections. Returns true if
    /// an error occurred and execution should stop.
    fn collect_stdin_from_redirections(&mut self, redirections: &[HirRedirection]) -> bool {
        for redir in redirections {
            if self.collect_stdin_from_redir(redir) {
                return true;
            }
        }
        false
    }

    fn collect_stdin_from_redir(&mut self, redir: &HirRedirection) -> bool {
        match redir.op {
            RedirectionOp::HereDoc | RedirectionOp::HereDocStrip => {
                self.collect_stdin_heredoc(redir);
                false
            }
            RedirectionOp::HereString => {
                self.collect_stdin_herestring(redir);
                false
            }
            RedirectionOp::Input => self.collect_stdin_input(redir),
            _ => false,
        }
    }

    fn collect_stdin_heredoc(&mut self, redir: &HirRedirection) {
        if let Some(body) = &redir.here_doc_body {
            let content = if body.expand {
                wasmsh_expand::expand_string(&body.content, &mut self.vm.state)
            } else {
                body.content.to_string()
            };
            self.set_pending_input_bytes(content.into_bytes());
        }
    }

    fn collect_stdin_herestring(&mut self, redir: &HirRedirection) {
        let resolved = self.resolve_command_subst(std::slice::from_ref(&redir.target));
        let resolved_target = resolved.first().unwrap_or(&redir.target);
        let content = wasmsh_expand::expand_word(resolved_target, &mut self.vm.state);
        let mut data = content.into_bytes();
        data.push(b'\n');
        self.set_pending_input_bytes(data);
    }

    fn collect_stdin_input(&mut self, redir: &HirRedirection) -> bool {
        let resolved = self.resolve_command_subst(std::slice::from_ref(&redir.target));
        let resolved_target = resolved.first().unwrap_or(&redir.target);
        let target = wasmsh_expand::expand_word(resolved_target, &mut self.vm.state);
        let path = self.resolve_cwd_path(&target);
        match self.fs.stat(&path) {
            Ok(metadata) if !metadata.is_dir => {
                self.set_pending_input_file(path, false);
                false
            }
            Ok(_) => self.fail_stdin_input(&target, "Is a directory"),
            Err(_) => self.fail_stdin_input(&target, "No such file or directory"),
        }
    }

    fn fail_stdin_input(&mut self, target: &str, reason: &str) -> bool {
        let msg = format!("wasmsh: {target}: {reason}\n");
        self.write_stderr(msg.as_bytes());
        self.vm.state.last_status = 1;
        true
    }

    fn read_pending_input_bytes(&mut self, cmd_name: &str) -> Result<Option<Vec<u8>>, ()> {
        let Some(mut reader) = self.take_pending_input_reader(cmd_name)? else {
            return Ok(None);
        };
        let mut data = Vec::new();
        match reader.read_to_end(&mut data) {
            Ok(_) => Ok(Some(data)),
            Err(err) => {
                let msg = format!("wasmsh: {cmd_name}: stdin read error: {err}\n");
                self.write_stderr(msg.as_bytes());
                self.vm.state.last_status = 1;
                Err(())
            }
        }
    }

    /// Try alias expansion for the command. Returns true if an alias was expanded.
    fn try_alias_expansion(&mut self, argv: &[String]) -> bool {
        if !self.get_shopt_value("expand_aliases") {
            return false;
        }
        if let Some(alias_val) = self.aliases.get(&argv[0]).cloned() {
            let rest = if argv.len() > 1 {
                format!(" {}", argv[1..].join(" "))
            } else {
                String::new()
            };
            let expanded = format!("{alias_val}{rest}");
            let sub_events = self.execute_input_inner(&expanded);
            self.merge_sub_events(sub_events);
            return true;
        }
        false
    }

    /// Print xtrace output if enabled.
    fn trace_command(&mut self, argv: &[String]) {
        if self.vm.state.get_var("SHOPT_x").as_deref() == Some("1") {
            let ps4 = self
                .vm
                .state
                .get_var("PS4")
                .unwrap_or_else(|| smol_str::SmolStr::from("+ "));
            let trace_line = format!("{}{}\n", ps4, argv.join(" "));
            self.write_stderr(trace_line.as_bytes());
        }
    }

    fn resolve_runtime_command(cmd_name: &str) -> Option<RuntimeCommandKind> {
        match cmd_name {
            CMD_LOCAL => Some(RuntimeCommandKind::Local),
            CMD_BREAK => Some(RuntimeCommandKind::Break),
            CMD_CONTINUE => Some(RuntimeCommandKind::Continue),
            CMD_RETURN => Some(RuntimeCommandKind::Return),
            CMD_EXIT => Some(RuntimeCommandKind::Exit),
            CMD_EVAL => Some(RuntimeCommandKind::Eval),
            CMD_SOURCE | CMD_DOT => Some(RuntimeCommandKind::Source),
            CMD_DECLARE | CMD_TYPESET => Some(RuntimeCommandKind::Declare),
            CMD_LET => Some(RuntimeCommandKind::Let),
            CMD_SHOPT => Some(RuntimeCommandKind::Shopt),
            CMD_ALIAS => Some(RuntimeCommandKind::Alias),
            CMD_UNALIAS => Some(RuntimeCommandKind::Unalias),
            CMD_BUILTIN => Some(RuntimeCommandKind::BuiltinKeyword),
            CMD_MAPFILE | CMD_READARRAY => Some(RuntimeCommandKind::Mapfile),
            CMD_TYPE => Some(RuntimeCommandKind::Type),
            CMD_COMMAND => Some(RuntimeCommandKind::CommandKeyword),
            CMD_EXEC => Some(RuntimeCommandKind::ExecKeyword),
            CMD_HASH => Some(RuntimeCommandKind::Hash),
            CMD_TIMES => Some(RuntimeCommandKind::Times),
            CMD_DIRS => Some(RuntimeCommandKind::Dirs),
            CMD_PUSHD => Some(RuntimeCommandKind::Pushd),
            CMD_POPD => Some(RuntimeCommandKind::Popd),
            CMD_UMASK => Some(RuntimeCommandKind::Umask),
            CMD_WAIT => Some(RuntimeCommandKind::Wait),
            CMD_ULIMIT => Some(RuntimeCommandKind::Ulimit),
            _ => None,
        }
    }

    fn resolve_command(&self, cmd_name: &str, argv: &[String]) -> ResolvedCommand {
        if let Some(kind) = Self::resolve_runtime_command(cmd_name) {
            return ResolvedCommand::Runtime(kind);
        }
        if cmd_name == "bash" || cmd_name == "sh" {
            return ResolvedCommand::ShellScript;
        }
        if let Some(body) = self.functions.get(cmd_name).cloned() {
            return ResolvedCommand::Function(body);
        }
        if let Some(builtin_fn) = self.builtins.get(cmd_name) {
            return ResolvedCommand::Builtin(builtin_fn);
        }
        if let Some(util_fn) = self.utils.get(cmd_name) {
            return ResolvedCommand::Utility(Self::utility_kind(cmd_name, argv), util_fn);
        }
        ResolvedCommand::External
    }

    fn resolve_command_without_functions(
        &self,
        cmd_name: &str,
        argv: &[String],
    ) -> ResolvedCommand {
        if let Some(kind) = Self::resolve_runtime_command(cmd_name) {
            return ResolvedCommand::Runtime(kind);
        }
        if cmd_name == "bash" || cmd_name == "sh" {
            return ResolvedCommand::ShellScript;
        }
        if let Some(builtin_fn) = self.builtins.get(cmd_name) {
            return ResolvedCommand::Builtin(builtin_fn);
        }
        if let Some(util_fn) = self.utils.get(cmd_name) {
            return ResolvedCommand::Utility(Self::utility_kind(cmd_name, argv), util_fn);
        }
        ResolvedCommand::External
    }

    fn utility_kind(cmd_name: &str, argv: &[String]) -> UtilityCommandKind {
        if cmd_name == "find" && argv.iter().any(|arg| arg == "-exec") {
            UtilityCommandKind::FindWithExec
        } else if cmd_name == "xargs" {
            UtilityCommandKind::Xargs
        } else {
            UtilityCommandKind::Plain
        }
    }

    fn find_command_path(&self, name: &str) -> Option<String> {
        if name.contains('/') {
            let path = self.resolve_cwd_path(name);
            self.fs.stat(&path).ok().map(|_| path)
        } else {
            self.search_path_for_file(name)
        }
    }

    fn command_lookups(
        &self,
        name: &str,
        skip_functions: bool,
        force_path: bool,
    ) -> Vec<CommandLookup> {
        let mut lookups = Vec::new();

        if !force_path {
            if let Some(value) = self.aliases.get(name) {
                lookups.push(CommandLookup {
                    kind: CommandLookupKind::Alias,
                    name: name.to_string(),
                    detail: value.clone(),
                });
            }
            if !skip_functions && self.functions.contains_key(name) {
                lookups.push(CommandLookup {
                    kind: CommandLookupKind::Function,
                    name: name.to_string(),
                    detail: name.to_string(),
                });
            }
            if self.builtins.is_builtin(name) {
                lookups.push(CommandLookup {
                    kind: CommandLookupKind::Builtin,
                    name: name.to_string(),
                    detail: name.to_string(),
                });
            }
            // Utilities ship with the sandbox but are not VFS files, so an AI
            // adapter can only discover them through `command -v`/`type`.
            if self.utils.is_utility(name) {
                lookups.push(CommandLookup {
                    kind: CommandLookupKind::Utility,
                    name: name.to_string(),
                    detail: name.to_string(),
                });
            }
        }

        if let Some(path) = self.find_command_path(name) {
            lookups.push(CommandLookup {
                kind: CommandLookupKind::File,
                name: name.to_string(),
                detail: path,
            });
        }

        if let Some(spec) = self.external_specs.get(name) {
            lookups.push(CommandLookup {
                kind: CommandLookupKind::External,
                name: name.to_string(),
                detail: spec.executable.clone(),
            });
        }

        lookups
    }

    fn execute_argv_command(&mut self, argv: &[String]) {
        if self.check_resource_limits() || argv.is_empty() {
            return;
        }
        if let Some(last) = argv.last() {
            self.vm.state.set_last_argument(last.as_str());
        }
        let mut resolved = self.resolve_command(&argv[0], argv);
        // If the command would be dispatched externally and the path
        // contains a `/`, check whether the file has a shell shebang
        // so we can execute it natively instead of forwarding to the
        // external handler (which may not exist).
        if matches!(resolved, ResolvedCommand::External) && argv[0].contains('/') {
            if let Some(interp) = self.detect_shell_shebang(&argv[0]) {
                if interp == "bash"
                    || interp == "sh"
                    || interp == "/bin/bash"
                    || interp == "/bin/sh"
                    || interp.ends_with("/bash")
                    || interp.ends_with("/sh")
                {
                    resolved = ResolvedCommand::ShebangScript;
                }
            }
        }
        self.execute_resolved_command(resolved, argv);
    }

    fn execute_resolved_command(&mut self, resolved: ResolvedCommand, argv: &[String]) {
        match resolved {
            ResolvedCommand::Runtime(kind) => self.execute_runtime_command(kind, argv),
            ResolvedCommand::ShellScript => self.call_shell_script(argv),
            ResolvedCommand::ShebangScript => self.call_shebang_script(argv),
            ResolvedCommand::Function(body) => self.call_shell_function(&argv[0], argv, &body),
            ResolvedCommand::Builtin(builtin_fn) => self.call_builtin(&argv[0], builtin_fn, argv),
            ResolvedCommand::Utility(kind, util_fn) => match kind {
                UtilityCommandKind::Plain => self.call_utility(&argv[0], util_fn, argv),
                UtilityCommandKind::FindWithExec => self.call_find_with_exec(util_fn, argv),
                UtilityCommandKind::Xargs => self.call_xargs_with_exec(util_fn, argv),
            },
            ResolvedCommand::External => self.call_external(argv),
        }
    }

    fn execute_runtime_command(&mut self, kind: RuntimeCommandKind, argv: &[String]) {
        match kind {
            RuntimeCommandKind::Local => self.execute_local(argv),
            RuntimeCommandKind::Break => {
                self.exec.break_depth = argv.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
                self.vm.state.last_status = 0;
            }
            RuntimeCommandKind::Continue => {
                self.exec.loop_continue = true;
                self.vm.state.last_status = 0;
            }
            RuntimeCommandKind::Return => {
                // Unwind the current function (or sourced file). Outside a
                // function, bash reports an error and does not stop the script.
                if self.vm.state.func_stack.is_empty() {
                    self.write_stderr(
                        b"wasmsh: return: can only `return' from a function or sourced script\n",
                    );
                    self.vm.state.last_status = 1;
                } else {
                    let code = argv
                        .get(1)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(self.vm.state.last_status);
                    self.exec.return_requested = Some(code);
                    self.vm.state.last_status = code;
                }
            }
            RuntimeCommandKind::Exit => {
                let code = argv
                    .get(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(self.vm.state.last_status);
                self.exec.exit_requested = Some(code);
                self.vm.state.last_status = code;
            }
            RuntimeCommandKind::Eval => {
                let code = argv[1..].join(" ");
                let sub_events = self.execute_input_inner(&code);
                self.merge_sub_events_with_diagnostics(sub_events);
            }
            RuntimeCommandKind::Source => self.execute_source(argv),
            RuntimeCommandKind::Declare => self.execute_declare(argv),
            RuntimeCommandKind::Let => self.execute_let(argv),
            RuntimeCommandKind::Shopt => self.execute_shopt(argv),
            RuntimeCommandKind::Alias => self.execute_alias(argv),
            RuntimeCommandKind::Unalias => self.execute_unalias(argv),
            RuntimeCommandKind::BuiltinKeyword => self.execute_builtin_keyword(argv),
            RuntimeCommandKind::Mapfile => self.execute_mapfile(argv),
            RuntimeCommandKind::Type => self.execute_type(argv),
            RuntimeCommandKind::CommandKeyword => self.execute_command_keyword(argv),
            RuntimeCommandKind::ExecKeyword => self.execute_exec_keyword(argv),
            RuntimeCommandKind::Hash => self.execute_hash(argv),
            RuntimeCommandKind::Times => self.execute_times(),
            RuntimeCommandKind::Dirs => self.execute_dirs(),
            RuntimeCommandKind::Pushd => self.execute_pushd(argv),
            RuntimeCommandKind::Popd => self.execute_popd(),
            RuntimeCommandKind::Umask => self.execute_umask(argv),
            RuntimeCommandKind::Wait => self.execute_wait(argv),
            RuntimeCommandKind::Ulimit => self.execute_ulimit(argv),
        }
    }

    /// Execute `local` — save old variable values and set new ones.
    fn execute_local(&mut self, argv: &[String]) {
        for arg in &argv[1..] {
            let (name, value) = if let Some(eq) = arg.find('=') {
                (&arg[..eq], Some(&arg[eq + 1..]))
            } else {
                (arg.as_str(), None)
            };
            let old = self.vm.state.get_var(name);
            self.exec
                .local_save_stack
                .push((smol_str::SmolStr::from(name), old));
            let val = value.map_or(smol_str::SmolStr::default(), smol_str::SmolStr::from);
            self.vm.state.set_var(smol_str::SmolStr::from(name), val);
        }
        self.vm.state.last_status = 0;
    }

    /// Execute `source`/`.` — read and execute a file.
    fn execute_source(&mut self, argv: &[String]) {
        let Some(path) = argv.get(1) else { return };
        let resolved = if path.contains('/') {
            Some(self.resolve_cwd_path(path))
        } else {
            let direct = self.resolve_cwd_path(path);
            if self.fs.stat(&direct).is_ok() {
                Some(direct)
            } else if self.get_shopt_value("sourcepath") {
                self.search_path_for_file(path)
            } else {
                None
            }
        };
        let Some(full) = resolved else {
            let msg = format!("source: {path}: not found\n");
            self.write_stderr(msg.as_bytes());
            self.vm.state.last_status = 1;
            return;
        };
        let Ok(h) = self.fs.open(&full, OpenOptions::read()) else {
            let msg = format!("source: {path}: not found\n");
            self.write_stderr(msg.as_bytes());
            self.vm.state.last_status = 1;
            return;
        };
        match self.fs.read_file(h) {
            Ok(data) => {
                self.fs.close(h);
                self.vm
                    .state
                    .source_stack
                    .push(smol_str::SmolStr::from(full.as_str()));
                let code = String::from_utf8_lossy(&data).to_string();
                self.with_nested_shell_scope(|runtime| {
                    let sub_events = runtime.execute_input_inner(&code);
                    runtime.merge_sub_events_with_diagnostics(sub_events);
                    runtime.run_return_trap_if_needed();
                });
                self.vm.state.source_stack.pop();
            }
            Err(e) => {
                self.fs.close(h);
                let msg = format!("source: {path}: read error: {e}\n");
                self.write_stderr(msg.as_bytes());
                self.vm.state.last_status = 1;
            }
        }
    }

    /// Merge sub-events (stdout/stderr only) into the current VM buffers.
    fn merge_sub_events(&mut self, events: Vec<WorkerEvent>) {
        for e in events {
            match e {
                WorkerEvent::Stdout(d) => self.write_stdout(&d),
                WorkerEvent::Stderr(d) => self.write_stderr(&d),
                _ => {}
            }
        }
    }

    /// Merge sub-events including diagnostics into the current VM buffers.
    fn merge_sub_events_with_diagnostics(&mut self, events: Vec<WorkerEvent>) {
        for e in events {
            match e {
                WorkerEvent::Stdout(d) => self.write_stdout(&d),
                WorkerEvent::Stderr(d) => self.write_stderr(&d),
                WorkerEvent::Diagnostic(level, msg) => self.vm.emit_diagnostic(
                    convert_diag_level(level),
                    wasmsh_vm::DiagCategory::Runtime,
                    msg,
                ),
                _ => {}
            }
        }
    }

    /// Handle `bash`/`sh` commands by reading the script and executing it.
    fn call_shell_script(&mut self, argv: &[String]) {
        if argv.len() < 2 {
            // Interactive shell not supported — just return
            return;
        }

        // Check for -c flag (inline script)
        // bash -c 'script' [name [args...]]
        // $0 = name (argv[3]), $1.. = args (argv[4..])
        if argv[1] == "-c" {
            if let Some(script) = argv.get(2) {
                let old_positional = std::mem::take(&mut self.vm.state.positional);
                let old_script_name = self.vm.state.script_name.take();
                if let Some(name) = argv.get(3) {
                    self.vm.state.script_name = Some(smol_str::SmolStr::from(name.as_str()));
                }
                self.vm.state.positional = argv
                    .get(4..)
                    .unwrap_or_default()
                    .iter()
                    .map(|s| smol_str::SmolStr::from(s.as_str()))
                    .collect();
                let events = self.execute_isolated_input_events(script, None);
                let child_status = self.last_subst_status;
                self.vm.state.positional = old_positional;
                self.vm.state.script_name = old_script_name;
                self.apply_isolated_script_events(events, child_status);
            }
            return;
        }

        // Read script file from VFS
        let path = if argv[1].starts_with('/') {
            argv[1].clone()
        } else {
            format!("{}/{}", self.vm.state.cwd, argv[1])
        };
        let Ok(h) = self.fs.open(&path, OpenOptions::read()) else {
            let msg = format!("{}: {}: No such file or directory\n", argv[0], argv[1]);
            self.write_stderr(msg.as_bytes());
            self.vm.state.last_status = 127;
            return;
        };
        let data = self.fs.read_file(h).unwrap_or_default();
        self.fs.close(h);
        let content = String::from_utf8_lossy(&data).to_string();

        // `sh file` runs in a child shell: `exit` ends the child only, and
        // variables/functions do not leak back into the caller.
        let old_positional = std::mem::take(&mut self.vm.state.positional);
        let old_script_name = self.vm.state.script_name.take();
        self.vm.state.script_name = Some(smol_str::SmolStr::from(argv[1].as_str()));
        self.vm.state.positional = argv[2..]
            .iter()
            .map(|s| smol_str::SmolStr::from(s.as_str()))
            .collect();

        self.vm
            .state
            .source_stack
            .push(smol_str::SmolStr::from(path.as_str()));
        let events = self.execute_isolated_input_events(&content, None);
        let child_status = self.last_subst_status;
        self.vm.state.source_stack.pop();

        self.vm.state.positional = old_positional;
        self.vm.state.script_name = old_script_name;
        self.apply_isolated_script_events(events, child_status);
    }

    /// Deliver the events of an isolated child-shell run to the parent: merge
    /// stdout/stderr and adopt the child's exit status without letting its
    /// `exit` terminate the parent.
    fn apply_isolated_script_events(
        &mut self,
        events: Vec<WorkerEvent>,
        child_status: Option<i32>,
    ) {
        let mut status = child_status;
        for event in &events {
            if let WorkerEvent::Exit(code) = event {
                status = Some(*code);
            }
        }
        self.merge_sub_events_with_diagnostics(events);
        if let Some(code) = status {
            self.vm.state.last_status = code;
        }
    }

    /// Detect a shell shebang at the start of a file.
    /// Returns the interpreter command (e.g. "bash", "/bin/sh") if found.
    fn detect_shell_shebang(&mut self, cmd_name: &str) -> Option<String> {
        let path = if cmd_name.starts_with('/') {
            cmd_name.to_string()
        } else {
            format!("{}/{cmd_name}", self.vm.state.cwd)
        };
        let h = self.fs.open(&path, OpenOptions::read()).ok()?;
        let data = self.fs.read_file(h).unwrap_or_default();
        self.fs.close(h);
        if data.len() < 3 || data[0] != b'#' || data[1] != b'!' {
            return None;
        }
        let end = data.iter().position(|&b| b == b'\n').unwrap_or(data.len());
        let line = String::from_utf8_lossy(&data[2..end]).trim().to_string();
        // Handle "#!/usr/bin/env bash" → "bash"
        if let Some(rest) = line.strip_prefix("/usr/bin/env ") {
            Some(rest.trim().to_string())
        } else {
            // e.g. "/bin/bash" → extract basename for matching
            Some(line.clone())
        }
    }

    /// Execute a script file that was invoked directly by path (e.g. `/workspace/script.sh`).
    /// The shebang has already been validated as a shell interpreter.
    fn call_shebang_script(&mut self, argv: &[String]) {
        let cmd_name = &argv[0];
        let path = if cmd_name.starts_with('/') {
            cmd_name.clone()
        } else {
            format!("{}/{cmd_name}", self.vm.state.cwd)
        };
        let Ok(h) = self.fs.open(&path, OpenOptions::read()) else {
            let msg = format!("wasmsh: {cmd_name}: No such file or directory\n");
            self.write_stderr(msg.as_bytes());
            self.vm.state.last_status = 127;
            return;
        };
        let data = self.fs.read_file(h).unwrap_or_default();
        self.fs.close(h);
        let content = String::from_utf8_lossy(&data).to_string();

        // Set $0 to the script path, positional parameters from argv[1..]
        let old_positional = std::mem::take(&mut self.vm.state.positional);
        let old_script_name = self.vm.state.script_name.take();
        self.vm.state.script_name = Some(smol_str::SmolStr::from(cmd_name.as_str()));
        self.vm.state.positional = argv[1..]
            .iter()
            .map(|s| smol_str::SmolStr::from(s.as_str()))
            .collect();

        self.vm
            .state
            .source_stack
            .push(smol_str::SmolStr::from(path.as_str()));
        let sub_events =
            self.with_nested_shell_scope(|runtime| runtime.execute_input_inner(&content));
        self.vm.state.source_stack.pop();
        self.merge_sub_events_with_diagnostics(sub_events);

        self.vm.state.positional = old_positional;
        self.vm.state.script_name = old_script_name;
    }

    fn call_external(&mut self, argv: &[String]) {
        let cmd_name = &argv[0];
        let spec = self.external_specs.get(cmd_name).cloned();
        let max_input_bytes = spec
            .as_ref()
            .map_or(self.config.external_input_byte_limit, |spec| {
                spec.options.max_input_bytes
            });
        let Ok(stdin) = self.take_external_stdin(cmd_name, max_input_bytes) else {
            return;
        };
        if let Some(spec) = spec {
            if let Some(handler) = self.external_spec_handler.as_mut() {
                if let Some(result) = handler(&spec, argv, stdin) {
                    self.apply_external_result(cmd_name, result, spec.options.max_output_bytes);
                } else {
                    self.external_host_failure(
                        cmd_name,
                        126,
                        "registered external command was not handled by the host",
                    );
                }
            } else {
                self.external_host_failure(
                    cmd_name,
                    126,
                    "native external processes are not supported by this host",
                );
            }
        } else if let Some(ref mut handler) = self.external_handler {
            if let Some(result) = handler(cmd_name, argv, stdin) {
                self.apply_external_result(
                    cmd_name,
                    result,
                    self.config.external_output_byte_limit,
                );
            } else {
                self.external_host_failure(cmd_name, 127, "command not found");
            }
        } else {
            self.external_host_failure(cmd_name, 127, "command not found");
        }
    }

    fn external_host_failure(&mut self, cmd_name: &str, status: i32, reason: &str) {
        self.write_stderr(format!("wasmsh: {cmd_name}: {reason}\n").as_bytes());
        self.vm.state.last_status = status;
    }

    fn apply_external_result(
        &mut self,
        cmd_name: &str,
        mut result: ExternalCommandResult,
        max_output_bytes: u64,
    ) {
        let total = result.stdout.len() as u64 + result.stderr.len() as u64;
        if total > max_output_bytes {
            let allowed = max_output_bytes as usize;
            if result.stdout.len() > allowed {
                result.stdout.truncate(allowed);
                result.stderr.clear();
            } else {
                result.stderr.truncate(allowed - result.stdout.len());
            }
            result.status = 125;
            self.vm.emit_diagnostic(
                wasmsh_vm::DiagLevel::Error,
                wasmsh_vm::DiagCategory::Budget,
                format!(
                    "external output limit exceeded for {cmd_name}: {total} bytes (limit {max_output_bytes})"
                ),
            );
        }
        self.write_streams(&result.stdout, &result.stderr);
        self.vm.state.last_status = result.status;
    }

    /// Invoke a shell function.
    fn call_shell_function(&mut self, cmd_name: &str, argv: &[String], body: &HirCommand) {
        self.exec.recursion_depth += 1;
        if let Err(reason) = self
            .vm
            .budget
            .enter_recursion(self.vm.limits.recursion_limit)
        {
            self.exec.recursion_depth -= 1;
            self.mark_recursion_exhaustion(reason);
            self.write_stderr(b"wasmsh: maximum recursion depth exceeded\n");
            return;
        }
        let old_positional = std::mem::take(&mut self.vm.state.positional);
        self.vm.state.positional = argv[1..]
            .iter()
            .map(|s| smol_str::SmolStr::from(s.as_str()))
            .collect();
        self.vm
            .state
            .func_stack
            .push(smol_str::SmolStr::from(cmd_name));
        let locals_before = self.exec.local_save_stack.len();
        self.with_nested_shell_scope(|runtime| {
            runtime.execute_command(body);
            runtime.run_return_trap_if_needed();
        });
        // A `return` inside the body unwinds only this function frame; clear it
        // so the caller's statements keep running.
        if self.exec.return_requested.is_some() {
            self.exec.return_requested = None;
        }
        let new_locals: Vec<_> = self.exec.local_save_stack.drain(locals_before..).collect();
        for (name, old_val) in new_locals.into_iter().rev() {
            if let Some(val) = old_val {
                self.vm.state.set_var(name, val);
            } else {
                self.vm.state.unset_var(&name).ok();
            }
        }
        self.vm.state.func_stack.pop();
        self.vm.state.positional = old_positional;
        self.vm.budget.exit_recursion();
        self.exec.recursion_depth -= 1;
    }

    /// Invoke a builtin command.
    fn call_builtin(
        &mut self,
        cmd_name: &str,
        builtin_fn: wasmsh_builtins::BuiltinFn,
        argv: &[String],
    ) {
        let Ok(stdin) = self.take_builtin_stdin(cmd_name) else {
            return;
        };
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let status = {
            let mut router = RuntimeOutputRouter {
                exec: &mut self.exec,
                exec_io: self.current_exec_io.as_mut(),
                proc_subst_out_scopes: &mut self.proc_subst_out_scopes,
                vm_stdout: &mut self.vm.stdout,
                vm_stderr: &mut self.vm.stderr,
                vm_output_bytes: &mut self.vm.output_bytes,
                vm_output_limit: self.vm.limits.output_byte_limit,
                vm_diagnostics: &mut self.vm.diagnostics,
            };
            let mut sink = RuntimeBuiltinSink {
                router: &mut router,
            };
            let mut ctx = wasmsh_builtins::BuiltinContext {
                state: &mut self.vm.state,
                output: &mut sink,
                fs: Some(&self.fs),
                stdin,
            };
            builtin_fn(&mut ctx, &argv_refs)
        };
        if cmd_name == "read" {
            install_read_remainder(&mut self.vm.state, &mut self.current_exec_io);
        }
        self.vm.state.last_status = status;
    }

    /// Extract `-exec CMD [args...] {} \;` from find argv.
    /// Returns `(exec_template, cleaned_argv)` or `None` if no `-exec` present.
    fn extract_find_exec(argv: &[String]) -> Option<(Vec<String>, Vec<String>)> {
        let exec_pos = argv.iter().position(|a| a == "-exec")?;
        // Find the terminator: \; or ;
        let term_pos = argv[exec_pos + 1..]
            .iter()
            .position(|a| a == "\\;" || a == ";")
            .map(|p| p + exec_pos + 1)?;
        let template: Vec<String> = argv[exec_pos + 1..term_pos].to_vec();
        if template.is_empty() {
            return None;
        }
        let mut cleaned: Vec<String> = argv[..exec_pos].to_vec();
        cleaned.extend_from_slice(&argv[term_pos + 1..]);
        Some((template, cleaned))
    }

    /// Shell-quote a path for safe interpolation into a command string.
    fn shell_quote(s: &str) -> String {
        if s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
        {
            s.to_string()
        } else {
            format!("'{}'", s.replace('\'', "'\\''"))
        }
    }

    /// Handle `find ... -exec CMD {} \;` by running find for paths, then executing
    /// the command for each matched path via the shell.
    fn call_find_with_exec(&mut self, find_fn: wasmsh_utils::UtilFn, argv: &[String]) {
        let Some((template, cleaned_argv)) = Self::extract_find_exec(argv) else {
            // Malformed -exec (missing \;), fall through to normal find
            self.call_utility("find", find_fn, argv);
            return;
        };

        // Phase 1: run find with cleaned argv, capturing stdout
        let ((), captured) = self.with_output_capture(true, false, |runtime| {
            runtime.call_utility("find", find_fn, &cleaned_argv);
        });
        let find_output = captured.stdout;

        // Phase 2: parse matched paths
        let paths_str = String::from_utf8_lossy(&find_output);
        let paths: Vec<&str> = paths_str.lines().filter(|l| !l.is_empty()).collect();

        // Phase 3: execute the command for each path
        let mut last_status = 0i32;
        for path in paths {
            let cmd_line: String = template
                .iter()
                .map(|t| {
                    if t == "{}" {
                        Self::shell_quote(path)
                    } else {
                        t.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            let sub_events = self.execute_input_inner(&cmd_line);
            self.merge_sub_events(sub_events);
            if self.vm.state.last_status != 0 {
                last_status = self.vm.state.last_status;
            }
        }
        self.vm.state.last_status = last_status;
    }

    /// Handle `xargs` with actual command execution for non-echo commands.
    /// The existing xargs utility already formats correct command lines for
    /// non-echo; we capture those and execute them via the shell.
    fn call_xargs_with_exec(&mut self, xargs_fn: wasmsh_utils::UtilFn, argv: &[String]) {
        // Determine if xargs has a non-echo command by scanning past flags
        let mut has_non_echo = false;
        let mut i = 1;
        while i < argv.len() {
            let arg = &argv[i];
            if matches!(arg.as_str(), "-I" | "-n" | "-d" | "-P" | "-L") && i + 1 < argv.len() {
                i += 2;
            } else if matches!(arg.as_str(), "-0" | "--null" | "-t" | "-p") || arg.starts_with('-')
            {
                i += 1;
            } else {
                // First non-flag arg is the command
                if arg != "echo" {
                    has_non_echo = true;
                }
                break;
            }
        }

        if !has_non_echo {
            self.call_utility("xargs", xargs_fn, argv);
            return;
        }

        // Run xargs utility — it outputs formatted command lines for non-echo
        let ((), captured) = self.with_output_capture(true, false, |runtime| {
            runtime.call_utility("xargs", xargs_fn, argv);
        });
        let xargs_output = captured.stdout;

        // Execute each output line as a command
        let output_str = String::from_utf8_lossy(&xargs_output);
        let mut last_status = 0i32;
        for line in output_str.lines().filter(|l| !l.is_empty()) {
            let sub_events = self.execute_input_inner(line);
            self.merge_sub_events(sub_events);
            if self.vm.state.last_status != 0 {
                last_status = self.vm.state.last_status;
            }
        }
        self.vm.state.last_status = last_status;
    }

    /// Invoke a utility command.
    fn call_utility(&mut self, cmd_name: &str, util_fn: wasmsh_utils::UtilFn, argv: &[String]) {
        let Ok(stdin) = self.take_util_stdin(cmd_name) else {
            return;
        };
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let cwd = self.vm.state.cwd.clone();
        let command_pipes = RefCell::new(Vec::<(String, Vec<u8>)>::new());
        let status = {
            let mut router = RuntimeOutputRouter {
                exec: &mut self.exec,
                exec_io: self.current_exec_io.as_mut(),
                proc_subst_out_scopes: &mut self.proc_subst_out_scopes,
                vm_stdout: &mut self.vm.stdout,
                vm_stderr: &mut self.vm.stderr,
                vm_output_bytes: &mut self.vm.output_bytes,
                vm_output_limit: self.vm.limits.output_byte_limit,
                vm_diagnostics: &mut self.vm.diagnostics,
            };
            let mut output = RuntimeUtilSink {
                router: &mut router,
                command_pipes: &command_pipes,
            };
            let mut ctx = UtilContext {
                fs: &mut self.fs,
                output: &mut output,
                cwd: &cwd,
                stdin,
                state: Some(&self.vm.state),
                network: self.network.as_deref(),
                clock: Some(self.clock.as_ref()),
            };
            util_fn(&mut ctx, &argv_refs)
        };
        self.vm.state.last_status = status;

        // Deliver awk-style `print | "cmd"` pipes: run each command with the
        // buffered text as its stdin, exactly once, after the utility returns.
        for (command, data) in command_pipes.into_inner() {
            let events =
                self.execute_isolated_input_events(&command, Some(InputTarget::Bytes(data)));
            self.merge_sub_events_with_diagnostics(events);
        }
    }

    /// Execute an `if` command.
    fn execute_if(&mut self, if_cmd: &wasmsh_hir::HirIf) {
        let saved_suppress = self.exec.errexit_suppressed;
        self.exec.errexit_suppressed = true;
        self.execute_body(&if_cmd.condition);
        self.exec.errexit_suppressed = saved_suppress;
        if self.vm.state.last_status == 0 {
            self.execute_body(&if_cmd.then_body);
            return;
        }
        for elif in &if_cmd.elifs {
            let saved = self.exec.errexit_suppressed;
            self.exec.errexit_suppressed = true;
            self.execute_body(&elif.condition);
            self.exec.errexit_suppressed = saved;
            if self.vm.state.last_status == 0 {
                self.execute_body(&elif.then_body);
                return;
            }
        }
        if let Some(else_body) = &if_cmd.else_body {
            self.execute_body(else_body);
        } else {
            // POSIX: when no condition is true and there is no `else`, the
            // exit status of the `if` compound command is zero.
            self.vm.state.last_status = 0;
        }
    }

    /// Execute a `while` loop.
    fn execute_while_loop(&mut self, loop_cmd: &wasmsh_hir::HirLoop) {
        let mut ran_body = false;
        let mut last_body_status = 0;
        loop {
            if self.check_resource_limits() {
                break;
            }
            let saved = self.exec.errexit_suppressed;
            self.exec.errexit_suppressed = true;
            self.execute_body(&loop_cmd.condition);
            self.exec.errexit_suppressed = saved;
            if self.vm.state.last_status != 0 {
                break;
            }
            self.execute_body(&loop_cmd.body);
            ran_body = true;
            last_body_status = self.vm.state.last_status;
            if self.handle_loop_control() {
                break;
            }
        }
        // POSIX: the loop's exit status is the last body command's status, or
        // zero if the body never ran.
        self.vm.state.last_status = if ran_body { last_body_status } else { 0 };
    }

    /// Execute an `until` loop.
    fn execute_until_loop(&mut self, loop_cmd: &wasmsh_hir::HirLoop) {
        let mut ran_body = false;
        let mut last_body_status = 0;
        loop {
            if self.check_resource_limits() {
                break;
            }
            let saved = self.exec.errexit_suppressed;
            self.exec.errexit_suppressed = true;
            self.execute_body(&loop_cmd.condition);
            self.exec.errexit_suppressed = saved;
            if self.vm.state.last_status == 0 {
                break;
            }
            self.execute_body(&loop_cmd.body);
            ran_body = true;
            last_body_status = self.vm.state.last_status;
            if self.handle_loop_control() {
                break;
            }
        }
        self.vm.state.last_status = if ran_body { last_body_status } else { 0 };
    }

    /// Handle loop control flow (break/continue/exit). Returns true if the loop should break.
    fn handle_loop_control(&mut self) -> bool {
        if self.exec.break_depth > 0 {
            self.exec.break_depth -= 1;
            return true;
        }
        if self.exec.loop_continue {
            self.exec.loop_continue = false;
        }
        // `return` must escape every enclosing loop up to the function frame;
        // it is left pending so the frame can clear it and set the status.
        self.exec.return_requested.is_some() || self.exec.exit_requested.is_some()
    }

    /// Execute a `for` loop.
    fn execute_for_loop(&mut self, for_cmd: &wasmsh_hir::HirFor) {
        let words = self.expand_for_words(for_cmd.words.as_deref());
        let mut ran_body = false;
        let mut last_body_status = 0;
        for word in words {
            if self.check_resource_limits() {
                break;
            }
            self.vm.state.set_var(for_cmd.var_name.clone(), word.into());
            self.execute_body(&for_cmd.body);
            ran_body = true;
            last_body_status = self.vm.state.last_status;
            if self.exec.break_depth > 0 {
                self.exec.break_depth -= 1;
                break;
            }
            if self.exec.loop_continue {
                self.exec.loop_continue = false;
                continue;
            }
            if self.exec.exit_requested.is_some() {
                break;
            }
        }
        // An empty `for` list leaves the exit status at zero.
        self.vm.state.last_status = if ran_body { last_body_status } else { 0 };
    }

    /// Expand word list for `for` and `select` commands.
    fn expand_for_words(&mut self, words: Option<&[Word]>) -> Vec<String> {
        if let Some(ws) = words {
            let mut result = Vec::new();
            // Split while the AST still distinguishes quoted, literal and
            // substitution parts. Resolving `$( )` first would erase that
            // distinction, so `for f in $(ls)` could no longer split on IFS
            // while `for f in "$(ls)"` must not.
            for w in ws {
                self.split_for_word(w, &mut result);
            }
            let result: Vec<String> = result
                .into_iter()
                .flat_map(|arg| wasmsh_expand::expand_braces(&arg))
                .collect();
            self.expand_globs(result)
        } else {
            self.vm
                .state
                .positional
                .iter()
                .map(ToString::to_string)
                .collect()
        }
    }

    /// Split one `for`-list word into fields the way bash does.
    ///
    /// Literal and quoted text is never split (`for w in a\ b x` is two fields,
    /// not three). Unquoted expansions split on IFS. A quoted multi-field
    /// expansion (`"${a[@]}"`, `"$@"`) yields one field per element, with the
    /// first element merging into the text before it and the last into the text
    /// after it.
    fn split_for_word(&mut self, word: &Word, out: &mut Vec<String>) {
        let ifs = self
            .vm
            .state
            .get_var("IFS")
            .unwrap_or_else(|| smol_str::SmolStr::from(" \t\n"));
        let mut current = String::new();
        let mut quoted_seen = false;
        self.split_for_parts(
            &word.parts,
            &ifs,
            false,
            &mut current,
            out,
            &mut quoted_seen,
        );
        if !current.is_empty() || quoted_seen {
            out.push(current);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn split_for_parts(
        &mut self,
        parts: &[WordPart],
        ifs: &str,
        protected: bool,
        current: &mut String,
        out: &mut Vec<String>,
        quoted_seen: &mut bool,
    ) {
        for part in parts {
            match part {
                WordPart::Literal(text) => current.push_str(text),
                WordPart::SingleQuoted(text) => {
                    *quoted_seen = true;
                    current.push_str(text);
                }
                WordPart::DoubleQuoted(inner) => {
                    *quoted_seen = true;
                    for p in inner {
                        if let WordPart::Parameter(name) = p {
                            if let Some(elements) =
                                wasmsh_expand::array_multi_expansion(name, &mut self.vm.state)
                            {
                                if elements.is_empty() {
                                    continue;
                                }
                                current.push_str(&elements[0]);
                                for element in &elements[1..] {
                                    out.push(std::mem::take(current));
                                    current.push_str(element);
                                }
                                continue;
                            }
                        }
                        match p {
                            WordPart::Literal(text) | WordPart::SingleQuoted(text) => {
                                current.push_str(text);
                            }
                            WordPart::CommandSubstitution(inner) => {
                                current.push_str(&self.execute_subst(inner));
                            }
                            _ => {
                                let expanded = wasmsh_expand::expand_word(
                                    &synthetic_word(p),
                                    &mut self.vm.state,
                                );
                                current.push_str(&expanded);
                            }
                        }
                    }
                }
                WordPart::CommandSubstitution(inner) => {
                    let text = self.execute_subst(inner).to_string();
                    self.split_unquoted(&text, ifs, current, out);
                }
                WordPart::Parameter(_) | WordPart::Arithmetic(_) => {
                    let expanded =
                        wasmsh_expand::expand_word(&synthetic_word(part), &mut self.vm.state);
                    if protected {
                        current.push_str(&expanded);
                    } else {
                        self.split_unquoted(&expanded, ifs, current, out);
                    }
                }
                _ => {}
            }
        }
    }

    /// Split unquoted expansion text on IFS, appending to `current` and pushing
    /// completed fields. The first piece joins any pending `current`.
    #[allow(clippy::unused_self)]
    fn split_unquoted(&self, text: &str, ifs: &str, current: &mut String, out: &mut Vec<String>) {
        if ifs.is_empty() {
            current.push_str(text);
            return;
        }
        let mut first = true;
        for piece in text.split(|c: char| ifs.contains(c)) {
            if piece.is_empty() {
                continue;
            }
            if first {
                current.push_str(piece);
                first = false;
            } else {
                if !current.is_empty() {
                    out.push(std::mem::take(current));
                }
                current.push_str(piece);
            }
        }
    }

    /// Execute a `case` command.
    fn execute_case(&mut self, case_cmd: &wasmsh_hir::HirCase) {
        let nocasematch = self.vm.state.get_var("SHOPT_nocasematch").as_deref() == Some("1");
        let value = wasmsh_expand::expand_word(&case_cmd.word, &mut self.vm.state);
        let mut i = 0;
        let mut fallthrough = false;
        while i < case_cmd.items.len() {
            let item = &case_cmd.items[i];
            let pattern_matched = if fallthrough {
                true
            } else {
                item.patterns.iter().any(|pattern| {
                    let pat = wasmsh_expand::expand_word(pattern, &mut self.vm.state);
                    if nocasematch {
                        glob_match_inner(
                            pat.to_lowercase().as_bytes(),
                            value.to_lowercase().as_bytes(),
                        )
                    } else {
                        glob_match_inner(pat.as_bytes(), value.as_bytes())
                    }
                })
            };
            if pattern_matched {
                self.execute_body(&item.body);
                match item.terminator {
                    CaseTerminator::Break => break,
                    CaseTerminator::Fallthrough => {
                        fallthrough = true;
                        i += 1;
                    }
                    CaseTerminator::ContinueTesting => {
                        fallthrough = false;
                        i += 1;
                    }
                }
            } else {
                fallthrough = false;
                i += 1;
            }
        }
    }

    /// Execute a C-style `for (( init; cond; step ))` loop.
    fn execute_arith_for(&mut self, af: &wasmsh_hir::HirArithFor) {
        if !af.init.is_empty() {
            wasmsh_expand::eval_arithmetic(&af.init, &mut self.vm.state);
        }
        let mut ran_body = false;
        let mut last_body_status = 0;
        loop {
            if self.check_resource_limits() {
                break;
            }
            if !af.cond.is_empty() {
                let cond_val = wasmsh_expand::eval_arithmetic(&af.cond, &mut self.vm.state);
                if cond_val == 0 {
                    break;
                }
            }
            self.execute_body(&af.body);
            ran_body = true;
            last_body_status = self.vm.state.last_status;
            if self.handle_loop_control() {
                break;
            }
            if !af.step.is_empty() {
                wasmsh_expand::eval_arithmetic(&af.step, &mut self.vm.state);
            }
        }
        self.vm.state.last_status = if ran_body { last_body_status } else { 0 };
    }

    /// Execute a `select` command.
    fn execute_select(&mut self, sel: &wasmsh_hir::HirSelect) {
        if self.collect_stdin_from_redirections(&sel.redirections) {
            return;
        }
        let words = self.expand_for_words(sel.words.as_deref());
        if words.is_empty() {
            return;
        }
        self.print_select_menu(&words);
        let Ok(input) = self.read_pending_input_bytes("select") else {
            return;
        };
        let input = String::from_utf8_lossy(&input.unwrap_or_default()).into_owned();

        for line in input.lines() {
            let reply = line.trim();
            self.bind_select_iteration_vars(sel, reply, &words);
            self.execute_body(&sel.body);
            if !self.consume_select_loop_control() {
                break;
            }
            if reply.is_empty() {
                self.print_select_menu(&words);
            }
        }
    }

    /// Set `REPLY` and the user-named variable for one iteration of `select`.
    fn bind_select_iteration_vars(
        &mut self,
        sel: &wasmsh_hir::HirSelect,
        reply: &str,
        words: &[String],
    ) {
        self.vm
            .state
            .set_var(smol_str::SmolStr::from("REPLY"), reply.into());
        let selected = Self::pick_select_word(reply, words).unwrap_or_default();
        self.vm.state.set_var(sel.var_name.clone(), selected.into());
    }

    /// Resolve the user's reply to the word at that 1-based menu index, if any.
    fn pick_select_word(reply: &str, words: &[String]) -> Option<String> {
        reply
            .parse::<usize>()
            .ok()
            .filter(|&n| n >= 1 && n <= words.len())
            .map(|n| words[n - 1].clone())
    }

    /// Apply the post-body break/continue/exit checks. Returns `true` to keep
    /// looping, `false` to break out of the `select` loop.
    fn consume_select_loop_control(&mut self) -> bool {
        if self.exec.break_depth > 0 {
            self.exec.break_depth -= 1;
            return false;
        }
        if self.exec.loop_continue {
            self.exec.loop_continue = false;
        }
        if self.exec.return_requested.is_some() || self.exec.exit_requested.is_some() {
            return false;
        }
        true
    }

    fn print_select_menu(&mut self, words: &[String]) {
        for (idx, word) in words.iter().enumerate() {
            let line = format!("{}) {word}\n", idx + 1);
            self.write_stderr(line.as_bytes());
        }
    }

    // ---- [[ ]] extended test evaluation ----

    /// Expand a word inside `[[ ]]` — no word splitting or glob expansion.
    fn dbl_bracket_expand(&mut self, word: &Word) -> String {
        let resolved = self.resolve_command_subst(std::slice::from_ref(word));
        wasmsh_expand::expand_word(&resolved[0], &mut self.vm.state)
    }

    /// Evaluate a `[[ expression ]]` command. Returns true for exit-status 0.
    fn eval_double_bracket(&mut self, words: &[Word]) -> bool {
        // Expand all words (no splitting/globbing) into string tokens for the evaluator
        let tokens: Vec<String> = words.iter().map(|w| self.dbl_bracket_expand(w)).collect();
        let mut pos = 0;
        dbl_bracket_eval_or(&tokens, &mut pos, &self.fs, &mut self.vm.state)
    }

    fn resolve_cwd_path(&self, path: &str) -> String {
        if path.starts_with('/') {
            wasmsh_fs::normalize_path(path)
        } else {
            wasmsh_fs::normalize_path(&format!("{}/{}", self.vm.state.cwd, path))
        }
    }

    /// Execute `alias [name[='value'] ...]`.
    fn execute_alias(&mut self, argv: &[String]) {
        let args = &argv[1..];
        if args.is_empty() {
            // List all aliases
            let alias_lines: Vec<String> = self
                .aliases
                .iter()
                .map(|(name, value)| format!("alias {name}='{value}'\n"))
                .collect();
            for line in alias_lines {
                self.write_stdout(line.as_bytes());
            }
            self.vm.state.last_status = 0;
            return;
        }
        for arg in args {
            if let Some(eq_pos) = arg.find('=') {
                let name = &arg[..eq_pos];
                let value = &arg[eq_pos + 1..];
                self.aliases.insert(name.to_string(), value.to_string());
            } else {
                // Show specific alias
                if let Some(value) = self.aliases.get(arg.as_str()) {
                    let line = format!("alias {arg}='{value}'\n");
                    self.write_stdout(line.as_bytes());
                } else {
                    let msg = format!("alias: {arg}: not found\n");
                    self.write_stderr(msg.as_bytes());
                    self.vm.state.last_status = 1;
                    return;
                }
            }
        }
        self.vm.state.last_status = 0;
    }

    /// Execute `unalias [-a] name ...`.
    fn execute_unalias(&mut self, argv: &[String]) {
        let args = &argv[1..];
        if args.is_empty() {
            self.write_stderr(b"unalias: usage: unalias [-a] name ...\n");
            self.vm.state.last_status = 1;
            return;
        }
        for arg in args {
            if arg == "-a" {
                self.aliases.clear();
            } else if self.aliases.shift_remove(arg.as_str()).is_none() {
                let msg = format!("unalias: {arg}: not found\n");
                self.write_stderr(msg.as_bytes());
                self.vm.state.last_status = 1;
                return;
            }
        }
        self.vm.state.last_status = 0;
    }

    /// Execute `type name ...` — report how each name would be interpreted.
    /// Checks aliases, functions, builtins, and utilities in that order.
    fn execute_type(&mut self, argv: &[String]) {
        let (flags, names) = Self::parse_type_args(&argv[1..]);
        let mut status = 0;
        for name in names {
            if !self.render_type_name(name, &flags) {
                status = 1;
            }
        }
        self.vm.state.last_status = status;
    }

    fn parse_type_args<'a>(args: &'a [String]) -> (TypeFlags, Vec<&'a str>) {
        let mut flags = TypeFlags::default();
        let mut names = Vec::new();
        for arg in args {
            if arg.starts_with('-') && arg.len() > 1 {
                Self::apply_type_short_flags(&arg[1..], &mut flags);
            } else {
                names.push(arg.as_str());
            }
        }
        (flags, names)
    }

    fn apply_type_short_flags(short: &str, flags: &mut TypeFlags) {
        for ch in short.chars() {
            match ch {
                'a' => flags.all = true,
                'f' => flags.skip_functions = true,
                'p' => flags.path_only = true,
                'P' => {
                    flags.path_only = true;
                    flags.force_path = true;
                }
                't' => flags.type_only = true,
                _ => {}
            }
        }
    }

    fn render_type_name(&mut self, name: &str, flags: &TypeFlags) -> bool {
        let mut lookups = self.command_lookups(name, flags.skip_functions, flags.force_path);
        if flags.path_only {
            lookups.retain(|lookup| matches!(lookup.kind, CommandLookupKind::File));
        }
        if lookups.is_empty() {
            let msg = format!("wasmsh: type: {name}: not found\n");
            self.write_stderr(msg.as_bytes());
            return false;
        }
        let limit = if flags.all { usize::MAX } else { 1 };
        for lookup in lookups.into_iter().take(limit) {
            let line = format_type_lookup(&lookup, flags.type_only, flags.path_only);
            self.write_stdout(format!("{line}\n").as_bytes());
        }
        true
    }

    /// Execute `builtin name [args...]` — skip alias and function lookup,
    /// invoke the named builtin directly.
    fn execute_builtin_keyword(&mut self, argv: &[String]) {
        if argv.len() < 2 {
            self.vm.state.last_status = 0;
            return;
        }
        let builtin_argv: Vec<String> = argv[1..].to_vec();
        let cmd_name = &builtin_argv[0];
        if let Some(builtin_fn) = self.builtins.get(cmd_name) {
            self.execute_resolved_command(ResolvedCommand::Builtin(builtin_fn), &builtin_argv);
        } else {
            let msg = format!("builtin: {cmd_name}: not a shell builtin\n");
            self.write_stderr(msg.as_bytes());
            self.vm.state.last_status = 1;
        }
    }

    fn execute_command_keyword(&mut self, argv: &[String]) {
        let mut use_default_path = false;
        let mut verbose = false;
        let mut describe = false;
        let mut index = 1usize;

        while let Some(arg) = argv.get(index) {
            match arg.as_str() {
                "-p" => use_default_path = true,
                "-v" => verbose = true,
                "-V" => describe = true,
                _ if arg.starts_with('-') && arg.len() > 1 => {}
                _ => break,
            }
            index += 1;
        }

        let args = &argv[index..];
        if verbose || describe {
            let mut status = 0;
            for name in args {
                let lookups = self.command_lookups(name, true, use_default_path);
                let Some(lookup) = lookups.first() else {
                    status = 1;
                    continue;
                };
                let line = if verbose {
                    format_command_verbose(lookup)
                } else {
                    format_type_lookup(lookup, false, false)
                };
                self.write_stdout(format!("{line}\n").as_bytes());
            }
            self.vm.state.last_status = status;
            return;
        }

        if args.is_empty() {
            self.vm.state.last_status = 0;
            return;
        }

        let resolved = self.resolve_command_without_functions(&args[0], args);
        self.execute_resolved_command(resolved, args);
    }

    fn execute_exec_keyword(&mut self, argv: &[String]) {
        if argv.len() <= 1 {
            self.vm.state.last_status = 0;
            return;
        }
        let args = &argv[1..];
        let resolved = self.resolve_command_without_functions(&args[0], args);
        self.execute_resolved_command(resolved, args);
    }

    fn execute_hash(&mut self, argv: &[String]) {
        let mut print_paths = false;
        let mut status = 0;

        for arg in &argv[1..] {
            match arg.as_str() {
                "-r" => {}
                "-t" => print_paths = true,
                name => {
                    let lookups = self.command_lookups(name, true, true);
                    let Some(lookup) = lookups
                        .iter()
                        .find(|lookup| matches!(lookup.kind, CommandLookupKind::File))
                    else {
                        status = 1;
                        continue;
                    };
                    if print_paths {
                        self.write_stdout(format!("{}\n", lookup.detail).as_bytes());
                    }
                }
            }
        }

        self.vm.state.last_status = status;
    }

    fn execute_times(&mut self) {
        self.write_stdout(b"0m0.000s 0m0.000s\n0m0.000s 0m0.000s\n");
        self.vm.state.last_status = 0;
    }

    fn emit_pipeline_timing(&mut self, posix_format: bool, elapsed_seconds: f64) {
        let output = if posix_format {
            format!("real {elapsed_seconds:.3}\nuser 0.000\nsys 0.000\n")
        } else {
            let minutes = (elapsed_seconds / 60.0).floor() as u64;
            let seconds = elapsed_seconds - (minutes as f64 * 60.0);
            format!("real\t{minutes}m{seconds:.3}s\nuser\t0m0.000s\nsys\t0m0.000s\n")
        };
        self.write_stderr(output.as_bytes());
    }

    fn execute_dirs(&mut self) {
        let mut dirs = vec![self.vm.state.cwd.clone()];
        dirs.extend(self.vm.state.dir_stack.iter().map(ToString::to_string));
        self.write_stdout(format!("{}\n", dirs.join(" ")).as_bytes());
        self.vm.state.last_status = 0;
    }

    fn execute_pushd(&mut self, argv: &[String]) {
        let target = if let Some(path) = argv.get(1) {
            path.clone()
        } else if let Some(path) = self.vm.state.dir_stack.first() {
            path.to_string()
        } else {
            self.write_stderr(b"pushd: no other directory\n");
            self.vm.state.last_status = 1;
            return;
        };

        let old_cwd = self.vm.state.cwd.clone();
        if !self.change_directory(&target) {
            return;
        }
        self.vm
            .state
            .dir_stack
            .insert(0, smol_str::SmolStr::from(old_cwd.as_str()));
        self.execute_dirs();
    }

    fn execute_popd(&mut self) {
        let Some(target) = self.vm.state.dir_stack.first().cloned() else {
            self.write_stderr(b"popd: directory stack empty\n");
            self.vm.state.last_status = 1;
            return;
        };
        self.vm.state.dir_stack.remove(0);
        if !self.change_directory(&target) {
            return;
        }
        self.execute_dirs();
    }

    fn execute_umask(&mut self, argv: &[String]) {
        if argv.len() <= 1 {
            self.write_stdout(format!("{:03o}\n", self.vm.state.umask).as_bytes());
            self.vm.state.last_status = 0;
            return;
        }

        let value = argv[1].trim_start_matches('0');
        let value = if value.is_empty() { "0" } else { value };
        if let Ok(value) = u32::from_str_radix(value, 8) {
            self.vm.state.umask = value;
            self.vm.state.last_status = 0;
        } else {
            self.write_stderr(b"umask: invalid mode\n");
            self.vm.state.last_status = 1;
        }
    }

    fn execute_wait(&mut self, argv: &[String]) {
        if argv.len() <= 1 {
            self.vm.state.last_status = 0;
            return;
        }

        let mut status = 0;
        for arg in &argv[1..] {
            let Ok(pid) = arg.parse::<u32>() else {
                self.write_stderr(format!("wait: {arg}: not a pid or valid job spec\n").as_bytes());
                status = 1;
                continue;
            };
            if self.vm.state.last_background_pid != Some(pid) {
                self.write_stderr(
                    format!("wait: pid {pid} is not a child of this shell\n").as_bytes(),
                );
                status = 127;
            }
        }
        self.vm.state.last_status = status;
    }

    fn execute_ulimit(&mut self, argv: &[String]) {
        if argv.len() <= 1 || argv.get(1).is_some_and(|arg| arg == "-a") {
            self.write_stdout(b"unlimited\n");
        }
        self.vm.state.last_status = 0;
    }

    /// Execute `mapfile`/`readarray` — read stdin lines into an indexed array.
    /// Supports the common Bash flags needed by scripts in the sandbox model.
    fn execute_mapfile(&mut self, argv: &[String]) {
        let Ok(opts) = Self::parse_mapfile_args(&argv[1..]) else {
            self.vm.state.last_status = 1;
            return;
        };
        if opts.fd != 0 {
            self.write_stderr(b"wasmsh: mapfile: only file descriptor 0 is supported\n");
            self.vm.state.last_status = 1;
            return;
        }

        let name_key = smol_str::SmolStr::from(opts.array_name.as_str());
        if opts.origin == 0
            || !matches!(
                self.vm
                    .state
                    .env
                    .get(name_key.as_str())
                    .map(|var| &var.value),
                Some(wasmsh_state::VarValue::IndexedArray(_))
            )
        {
            self.vm.state.init_indexed_array(name_key.clone());
        }

        let Ok(bytes) = self.read_pending_input_bytes("mapfile") else {
            return;
        };
        self.populate_mapfile_array(&name_key, &bytes.unwrap_or_default(), &opts);
        self.vm.state.last_status = 0;
    }

    fn parse_mapfile_args(args: &[String]) -> Result<MapfileOptions, ()> {
        let mut opts = MapfileOptions {
            strip_delimiter: false,
            delimiter: b'\n',
            count: None,
            origin: 0,
            skip: 0,
            fd: 0,
            array_name: "MAPFILE".to_string(),
        };
        let mut i = 0usize;
        while i < args.len() {
            match args[i].as_str() {
                "-t" => opts.strip_delimiter = true,
                "-d" => {
                    i += 1;
                    let Some(value) = args.get(i) else {
                        return Err(());
                    };
                    opts.delimiter = value.as_bytes().first().copied().unwrap_or(0);
                }
                "-n" => {
                    i += 1;
                    let Some(value) = args.get(i).and_then(|arg| arg.parse::<usize>().ok()) else {
                        return Err(());
                    };
                    opts.count = Some(value);
                }
                "-O" => {
                    i += 1;
                    let Some(value) = args.get(i).and_then(|arg| arg.parse::<usize>().ok()) else {
                        return Err(());
                    };
                    opts.origin = value;
                }
                "-s" => {
                    i += 1;
                    let Some(value) = args.get(i).and_then(|arg| arg.parse::<usize>().ok()) else {
                        return Err(());
                    };
                    opts.skip = value;
                }
                "-u" => {
                    i += 1;
                    let Some(value) = args.get(i).and_then(|arg| arg.parse::<u32>().ok()) else {
                        return Err(());
                    };
                    opts.fd = value;
                }
                "-C" | "-c" => {
                    i += 1;
                    if args.get(i).is_none() {
                        return Err(());
                    }
                }
                value if value.starts_with('-') && value.len() > 1 => {}
                value => opts.array_name = value.to_string(),
            }
            i += 1;
        }
        Ok(opts)
    }

    fn populate_mapfile_array(
        &mut self,
        name_key: &smol_str::SmolStr,
        text: &[u8],
        opts: &MapfileOptions,
    ) {
        let mut records = Vec::new();
        let mut current = Vec::new();
        for &byte in text {
            if byte == opts.delimiter {
                if !opts.strip_delimiter {
                    current.push(byte);
                }
                records.push(std::mem::take(&mut current));
            } else {
                current.push(byte);
            }
        }
        if !current.is_empty() {
            records.push(current);
        }

        for (offset, record) in records
            .into_iter()
            .skip(opts.skip)
            .take(opts.count.unwrap_or(usize::MAX))
            .enumerate()
        {
            let value = String::from_utf8_lossy(&record).to_string();
            self.vm.state.set_array_element(
                name_key.clone(),
                &(opts.origin + offset).to_string(),
                smol_str::SmolStr::from(value.as_str()),
            );
        }
    }

    fn change_directory(&mut self, target: &str) -> bool {
        let path = self.resolve_cwd_path(target);
        match self.fs.stat(&path) {
            Ok(meta) if meta.is_dir => {
                let old_pwd = self.vm.state.cwd.clone();
                self.vm.state.cwd.clone_from(&path);
                self.vm
                    .state
                    .set_var("OLDPWD".into(), smol_str::SmolStr::from(old_pwd.as_str()));
                self.vm
                    .state
                    .set_var("PWD".into(), smol_str::SmolStr::from(path.as_str()));
                self.vm.state.last_status = 0;
                true
            }
            Ok(_) => {
                self.write_stderr(format!("wasmsh: {target}: Not a directory\n").as_bytes());
                self.vm.state.last_status = 1;
                false
            }
            Err(_) => {
                self.write_stderr(
                    format!("wasmsh: {target}: No such file or directory\n").as_bytes(),
                );
                self.vm.state.last_status = 1;
                false
            }
        }
    }

    /// Search `$PATH` directories in the VFS for a file. Returns the first match.
    fn search_path_for_file(&self, filename: &str) -> Option<String> {
        let path_var = self.vm.state.get_var("PATH")?;
        for dir in path_var.split(':') {
            if dir.is_empty() {
                continue;
            }
            let candidate = format!("{dir}/{filename}");
            let full = self.resolve_cwd_path(&candidate);
            if self.fs.stat(&full).is_ok() {
                return Some(full);
            }
        }
        None
    }

    fn should_errexit(&self, and_or: &HirAndOr) -> bool {
        !self.exec.errexit_suppressed
            && and_or.rest.is_empty()
            && !and_or.first.negated
            && self.vm.state.get_var("SHOPT_e").as_deref() == Some("1")
            && self.vm.state.last_status != 0
            && self.exec.exit_requested.is_none()
    }

    /// Execute `let expr1 expr2 ...` — evaluate each as arithmetic.
    /// Exit status: 0 if the last expression is non-zero, 1 if zero.
    fn execute_let(&mut self, argv: &[String]) {
        if argv.len() < 2 {
            self.vm
                .stderr
                .extend_from_slice(b"let: expression expected\n");
            self.vm.state.last_status = 1;
            return;
        }
        let mut last_val: i64 = 0;
        for expr in &argv[1..] {
            last_val = wasmsh_expand::eval_arithmetic(expr, &mut self.vm.state);
        }
        self.vm.state.last_status = i32::from(last_val == 0);
    }

    /// Known `shopt` option names.
    const SHOPT_OPTIONS: &'static [&'static str] = &[
        "extglob",
        "nullglob",
        "dotglob",
        "globstar",
        "nocasematch",
        "nocaseglob",
        "failglob",
        "lastpipe",
        "expand_aliases",
        "sourcepath",
    ];

    /// Execute `shopt [-s|-u] [optname ...]`.
    fn execute_shopt(&mut self, argv: &[String]) {
        let (set_mode, names) = Self::parse_shopt_args(&argv[1..]);
        if let Some(enable) = set_mode {
            self.shopt_set_options(&names, enable);
        } else {
            self.shopt_print_options(&names);
        }
    }

    fn parse_shopt_args(args: &[String]) -> (Option<bool>, Vec<&str>) {
        let mut set_mode = None;
        let mut names = Vec::new();

        for arg in args {
            match arg.as_str() {
                "-s" => set_mode = Some(true),
                "-u" => set_mode = Some(false),
                _ => names.push(arg.as_str()),
            }
        }

        (set_mode, names)
    }

    /// Set shopt options (`-s` or `-u`).
    fn shopt_set_options(&mut self, names: &[&str], enable: bool) {
        if names.is_empty() {
            self.vm
                .stderr
                .extend_from_slice(b"shopt: option name required\n");
            self.vm.state.last_status = 1;
            return;
        }
        let val = if enable { "1" } else { "0" };
        for name in names {
            if self.reject_invalid_shopt_name(name) {
                return;
            }
            self.set_shopt_value(name, val);
        }
        self.vm.state.last_status = 0;
    }

    /// Print shopt option statuses. If `names` is empty, print all.
    fn shopt_print_options(&mut self, names: &[&str]) {
        let options_to_print: Vec<&str> = if names.is_empty() {
            Self::SHOPT_OPTIONS.to_vec()
        } else {
            names.to_vec()
        };
        for name in &options_to_print {
            if self.reject_invalid_shopt_name(name) {
                return;
            }
            let enabled = self.get_shopt_value(name);
            let status_str = if enabled { "on" } else { "off" };
            let line = format!("{name}\t{status_str}\n");
            self.write_stdout(line.as_bytes());
        }
        self.vm.state.last_status = 0;
    }

    fn reject_invalid_shopt_name(&mut self, name: &str) -> bool {
        if Self::SHOPT_OPTIONS.contains(&name) {
            return false;
        }

        let msg = format!("shopt: {name}: invalid shell option name\n");
        self.write_stderr(msg.as_bytes());
        self.vm.state.last_status = 1;
        true
    }

    fn shopt_var_name(name: &str) -> String {
        format!("SHOPT_{name}")
    }

    fn set_shopt_value(&mut self, name: &str, value: &str) {
        let var = Self::shopt_var_name(name);
        self.vm.state.set_var(
            smol_str::SmolStr::from(var.as_str()),
            smol_str::SmolStr::from(value),
        );
    }

    fn get_shopt_value(&self, name: &str) -> bool {
        let var = Self::shopt_var_name(name);
        self.vm.state.get_var(&var).as_deref() == Some("1")
    }

    fn is_set_option_enabled(&self, flag: char) -> bool {
        let var = format!("SHOPT_{flag}");
        self.vm.state.get_var(&var).as_deref() == Some("1")
    }

    fn maybe_write_verbose_input(&mut self, input: &str, cc: &HirCompleteCommand) {
        if !self.is_set_option_enabled('v') {
            return;
        }
        let start = cc.span.start as usize;
        let end = cc.span.end as usize;
        let Some(snippet) = input.get(start..end) else {
            return;
        };
        if snippet.is_empty() {
            return;
        }
        self.write_stderr(snippet.as_bytes());
        if !snippet.ends_with('\n') {
            self.write_stderr(b"\n");
        }
    }

    /// Execute `declare`/`typeset` with flag parsing.
    /// Supports: -i, -a, -A, -x, -r, -l, -u, -p, -n, name=value.
    fn execute_declare(&mut self, argv: &[String]) {
        let (flags, names) = parse_declare_flags(argv);

        if flags.is_print || flags.is_functions || flags.is_function_names {
            self.declare_print(argv, &names);
            return;
        }

        for &idx in &names {
            self.declare_one_name(argv, idx, &flags);
        }
        self.vm.state.last_status = 0;
    }

    /// Handle `declare -p` printing.
    fn declare_print(&mut self, argv: &[String], names: &[usize]) {
        let (flags, _) = parse_declare_flags(argv);
        if flags.is_functions || flags.is_function_names {
            self.declare_print_functions(argv, names, flags.is_function_names);
            return;
        }
        self.declare_print_vars(argv, names);
    }

    fn declare_print_functions(&mut self, argv: &[String], names: &[usize], names_only: bool) {
        let function_names: Vec<String> = if names.is_empty() {
            self.functions.keys().cloned().collect()
        } else {
            names.iter().map(|&idx| argv[idx].clone()).collect()
        };
        for name in function_names {
            if !self.functions.contains_key(name.as_str()) {
                continue;
            }
            let line = if names_only {
                format!("declare -f {name}\n")
            } else {
                format!("{name} () {{ :; }}\n")
            };
            self.write_stdout(line.as_bytes());
        }
        self.vm.state.last_status = 0;
    }

    fn declare_print_vars(&mut self, argv: &[String], names: &[usize]) {
        if names.is_empty() {
            let vars: Vec<(String, String)> = self
                .vm
                .state
                .env
                .scopes
                .iter()
                .flat_map(|scope| {
                    scope
                        .iter()
                        .map(|(n, v)| (n.to_string(), v.value.as_scalar().to_string()))
                })
                .collect();
            for (name, val) in &vars {
                let line = format!("declare -- {name}=\"{val}\"\n");
                self.write_stdout(line.as_bytes());
            }
        } else {
            for &idx in names {
                let name_arg = &argv[idx];
                let name = name_arg
                    .find('=')
                    .map_or(name_arg.as_str(), |eq| &name_arg[..eq]);
                if let Some(var) = self.vm.state.env.get(name) {
                    let val = var.value.as_scalar();
                    let line = format!("declare -- {name}=\"{val}\"\n");
                    self.write_stdout(line.as_bytes());
                }
            }
        }
        self.vm.state.last_status = 0;
    }

    /// Process a single name in a `declare`/`typeset` command.
    fn declare_one_name(&mut self, argv: &[String], idx: usize, flags: &DeclareFlags) {
        let name_arg = &argv[idx];
        let (name, value) = if let Some(eq) = name_arg.find('=') {
            (&name_arg[..eq], Some(&name_arg[eq + 1..]))
        } else {
            (name_arg.as_str(), None)
        };

        if flags.is_assoc {
            self.vm
                .state
                .init_assoc_array(smol_str::SmolStr::from(name));
        } else if flags.is_indexed {
            self.vm
                .state
                .init_indexed_array(smol_str::SmolStr::from(name));
        }

        if let Some(val) = value {
            self.declare_assign_value(name, val, flags);
        } else if !flags.is_assoc && !flags.is_indexed && self.vm.state.get_var(name).is_none() {
            self.vm
                .state
                .set_var(smol_str::SmolStr::from(name), smol_str::SmolStr::default());
        }

        self.declare_apply_attributes(name, flags);

        if flags.is_nameref {
            self.declare_apply_nameref(name);
        }
    }

    /// Assign a value in `declare`, handling compound arrays and scalar transforms.
    fn declare_assign_value(&mut self, name: &str, val: &str, flags: &DeclareFlags) {
        let trimmed = val.trim();
        if trimmed.starts_with('(') && trimmed.ends_with(')') {
            self.declare_assign_compound(name, &trimmed[1..trimmed.len() - 1], flags);
            return;
        }
        let final_val = Self::transform_declare_scalar(trimmed, flags, &mut self.vm.state);
        self.vm.state.set_var(
            smol_str::SmolStr::from(name),
            smol_str::SmolStr::from(final_val.as_str()),
        );
    }

    fn declare_assign_compound(&mut self, name: &str, inner: &str, flags: &DeclareFlags) {
        let name_key = smol_str::SmolStr::from(name);
        if flags.is_assoc || inner.contains("]=") {
            self.declare_assign_assoc_compound(&name_key, inner);
        } else {
            self.declare_assign_indexed_compound(&name_key, inner);
        }
    }

    fn declare_assign_assoc_compound(&mut self, name_key: &smol_str::SmolStr, inner: &str) {
        self.vm.state.init_assoc_array(name_key.clone());
        for pair in Self::parse_assoc_pairs(inner) {
            self.vm.state.set_array_element(
                name_key.clone(),
                &pair.0,
                smol_str::SmolStr::from(pair.1.as_str()),
            );
        }
    }

    fn declare_assign_indexed_compound(&mut self, name_key: &smol_str::SmolStr, inner: &str) {
        let elements = Self::parse_array_elements(inner);
        self.vm.state.init_indexed_array(name_key.clone());
        for (i, elem) in elements.iter().enumerate() {
            self.vm
                .state
                .set_array_element(name_key.clone(), &i.to_string(), elem.clone());
        }
    }

    fn transform_declare_scalar(val: &str, flags: &DeclareFlags, state: &mut ShellState) -> String {
        if flags.is_integer {
            wasmsh_expand::eval_arithmetic(val, state).to_string()
        } else if flags.is_lower {
            val.to_lowercase()
        } else if flags.is_upper {
            val.to_uppercase()
        } else {
            val.to_string()
        }
    }

    /// Apply export, readonly, integer attributes after declare assignment.
    fn declare_apply_attributes(&mut self, name: &str, flags: &DeclareFlags) {
        if let Some(var) = self.vm.state.env.get_mut(name) {
            if flags.is_export {
                var.exported = true;
            }
            if flags.is_readonly {
                var.readonly = true;
            }
            if flags.is_integer {
                var.integer = true;
            }
        }
    }

    /// Apply nameref attribute for `declare -n`.
    fn declare_apply_nameref(&mut self, name: &str) {
        let target_value = if let Some(eq_pos) = name.find('=') {
            smol_str::SmolStr::from(&name[eq_pos + 1..])
        } else if let Some(var) = self.vm.state.env.get(name) {
            var.value.as_scalar()
        } else {
            smol_str::SmolStr::default()
        };
        let actual_name = name.find('=').map_or(name, |eq| &name[..eq]);
        self.vm.state.env.set(
            smol_str::SmolStr::from(actual_name),
            wasmsh_state::ShellVar {
                value: wasmsh_state::VarValue::Scalar(target_value),
                exported: false,
                readonly: false,
                integer: false,
                nameref: true,
            },
        );
    }

    fn should_stop_execution(&self) -> bool {
        self.exec.break_depth > 0
            || self.exec.loop_continue
            || self.exec.return_requested.is_some()
            || self.exec.exit_requested.is_some()
            || self.exec.resource_exhausted
    }

    /// Check resource limits (step budget, output limit, cancellation).
    /// Returns true if execution should stop. Emits a diagnostic on first violation.
    fn check_resource_limits(&mut self) -> bool {
        if self.exec.resource_exhausted {
            return true;
        }
        if self.vm.begin_step().is_err() {
            self.exec.resource_exhausted = true;
            self.exec.stop_reason = self.vm.stop_reason().cloned();
            return true;
        }
        false
    }

    fn execute_body(&mut self, body: &[HirCompleteCommand]) {
        for cc in body {
            if self.should_stop_execution() || self.check_resource_limits() {
                break;
            }
            if self.is_set_option_enabled('n') {
                continue;
            }
            self.execute_complete_command(cc);
        }
    }

    fn execute_complete_command(&mut self, cc: &HirCompleteCommand) {
        for and_or in &cc.list {
            if self.should_stop_execution() || self.is_set_option_enabled('n') {
                break;
            }
            self.execute_and_or(and_or);
            if self.exec.exit_requested.is_some() {
                break;
            }
            self.handle_post_and_or(and_or);
        }
    }

    /// Expand a word value via command substitution and word expansion.
    fn expand_assignment_value(&mut self, value: Option<&Word>) -> String {
        if let Some(w) = value {
            let resolved = self.resolve_command_subst(std::slice::from_ref(w));
            wasmsh_expand::expand_word(&resolved[0], &mut self.vm.state)
        } else {
            String::new()
        }
    }

    /// Execute a variable assignment, handling array syntax:
    /// - `name=(val1 val2 ...)` -- indexed array compound assignment
    /// - `name[idx]=val` -- single element assignment
    /// - `name+=(val1 val2 ...)` -- array append
    /// - Plain `name=val` -- scalar assignment
    fn execute_assignment(&mut self, raw_name: &smol_str::SmolStr, value: Option<&Word>) {
        let (name_str, is_append) = Self::split_assignment_name(raw_name.as_str());
        // A write to a readonly variable fails the command; `set_var` would
        // otherwise silently drop it. Report immediately (bash prints the
        // error and continues with the next command) rather than deferring it
        // to the next dispatch, which would abort an unrelated command.
        if self.vm.state.is_var_readonly(name_str) {
            let msg = format!("wasmsh: {name_str}: readonly variable\n");
            self.write_stderr(msg.as_bytes());
            self.vm.state.last_status = 1;
            return;
        }
        if self.try_assign_array_element(name_str, value) {
            return;
        }

        let val_str = self.expand_assignment_value(value);
        let trimmed = val_str.trim();
        if trimmed.starts_with('(') && trimmed.ends_with(')') {
            self.assign_compound_array(name_str, trimmed, is_append);
            return;
        }

        let final_val = self.resolve_scalar_assignment_value(name_str, &val_str, is_append);
        self.vm
            .state
            .set_var(smol_str::SmolStr::from(name_str), final_val.into());
    }

    fn split_assignment_name(name: &str) -> (&str, bool) {
        if let Some(stripped) = name.strip_suffix('+') {
            (stripped, true)
        } else {
            (name, false)
        }
    }

    fn parse_array_element_assignment(name: &str) -> Option<(&str, &str)> {
        let bracket_pos = name.find('[')?;
        name.ends_with(']')
            .then_some((&name[..bracket_pos], &name[bracket_pos + 1..name.len() - 1]))
    }

    fn try_assign_array_element(&mut self, name: &str, value: Option<&Word>) -> bool {
        let Some((base, index)) = Self::parse_array_element_assignment(name) else {
            return false;
        };
        let val = self.expand_assignment_value(value);
        self.vm
            .state
            .set_array_element(smol_str::SmolStr::from(base), index, val.into());
        true
    }

    fn resolve_scalar_assignment_value(
        &mut self,
        name: &str,
        value: &str,
        is_append: bool,
    ) -> String {
        if self.vm.state.env.get(name).is_some_and(|v| v.integer) {
            return self.eval_integer_assignment(name, value, is_append);
        }
        if is_append {
            return format!(
                "{}{}",
                self.vm.state.get_var(name).unwrap_or_default(),
                value
            );
        }
        value.to_string()
    }

    fn eval_integer_assignment(&mut self, name: &str, value: &str, is_append: bool) -> String {
        let arith_input = if is_append {
            format!(
                "{}+{}",
                self.vm.state.get_var(name).unwrap_or_default(),
                value
            )
        } else {
            value.to_string()
        };
        wasmsh_expand::eval_arithmetic(&arith_input, &mut self.vm.state).to_string()
    }

    /// Assign a compound array value `(...)` to a variable.
    fn assign_compound_array(&mut self, name_str: &str, val_str: &str, is_append: bool) {
        let inner = &val_str[1..val_str.len() - 1];
        let elements = Self::parse_array_elements(inner);
        let name_key = smol_str::SmolStr::from(name_str);

        if is_append {
            self.vm.state.append_array(name_str, elements);
            return;
        }

        if Self::is_assoc_array_assignment(inner, &elements) {
            self.assign_assoc_array(&name_key, inner);
            return;
        }
        self.assign_indexed_array(&name_key, &elements);
    }

    fn is_assoc_array_assignment(inner: &str, elements: &[smol_str::SmolStr]) -> bool {
        !elements.is_empty() && inner.contains('[') && inner.contains("]=")
    }

    fn assign_assoc_array(&mut self, name_key: &smol_str::SmolStr, inner: &str) {
        self.vm.state.init_assoc_array(name_key.clone());
        for (key, value) in Self::parse_assoc_pairs(inner) {
            self.vm.state.set_array_element(
                name_key.clone(),
                &key,
                smol_str::SmolStr::from(value.as_str()),
            );
        }
    }

    fn assign_indexed_array(
        &mut self,
        name_key: &smol_str::SmolStr,
        elements: &[smol_str::SmolStr],
    ) {
        self.vm.state.init_indexed_array(name_key.clone());
        for (i, elem) in elements.iter().enumerate() {
            self.vm
                .state
                .set_array_element(name_key.clone(), &i.to_string(), elem.clone());
        }
    }

    fn push_array_element(elements: &mut Vec<smol_str::SmolStr>, current: &mut String) {
        if current.is_empty() {
            return;
        }
        elements.push(smol_str::SmolStr::from(current.as_str()));
        current.clear();
    }

    /// Parse space-separated array elements from the inner content of `(...)`.
    /// Respects quoting (single and double quotes).
    fn parse_array_elements(inner: &str) -> Vec<smol_str::SmolStr> {
        let mut elements = Vec::new();
        let mut current = String::new();
        let mut state = ArrayParseState::default();

        for ch in inner.chars() {
            match state.process_char(ch) {
                ArrayCharAction::Append(c) => current.push(c),
                ArrayCharAction::Skip => {}
                ArrayCharAction::SplitField => {
                    Self::push_array_element(&mut elements, &mut current);
                }
            }
        }
        Self::push_array_element(&mut elements, &mut current);
        elements
    }

    /// Parse `[key]=value` pairs from associative array compound assignment.
    fn parse_assoc_pairs(inner: &str) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        let mut pos = 0;
        let bytes = inner.as_bytes();

        while pos < bytes.len() {
            Self::skip_ascii_whitespace(bytes, &mut pos);
            if pos >= bytes.len() {
                break;
            }
            if let Some(key) = Self::parse_assoc_key(inner, &mut pos) {
                pairs.push((key, Self::parse_assoc_value(inner, &mut pos)));
                continue;
            }
            Self::skip_non_whitespace(bytes, &mut pos);
        }
        pairs
    }

    fn skip_ascii_whitespace(bytes: &[u8], pos: &mut usize) {
        while *pos < bytes.len() && bytes[*pos].is_ascii_whitespace() {
            *pos += 1;
        }
    }

    fn skip_non_whitespace(bytes: &[u8], pos: &mut usize) {
        while *pos < bytes.len() && !bytes[*pos].is_ascii_whitespace() {
            *pos += 1;
        }
    }

    fn parse_assoc_key(inner: &str, pos: &mut usize) -> Option<String> {
        let bytes = inner.as_bytes();
        if *pos >= bytes.len() || bytes[*pos] != b'[' {
            return None;
        }

        *pos += 1;
        let key_start = *pos;
        while *pos < bytes.len() && bytes[*pos] != b']' {
            *pos += 1;
        }
        let key = inner[key_start..*pos].to_string();
        if *pos < bytes.len() {
            *pos += 1;
        }
        if *pos < bytes.len() && bytes[*pos] == b'=' {
            *pos += 1;
        }
        Some(key)
    }

    /// Parse a single value in an associative array assignment (may be quoted).
    fn parse_assoc_value(inner: &str, pos: &mut usize) -> String {
        let bytes = inner.as_bytes();
        match bytes.get(*pos).copied() {
            Some(b'"') => Self::parse_double_quoted_assoc_value(bytes, pos),
            Some(b'\'') => Self::parse_single_quoted_assoc_value(bytes, pos),
            _ => Self::parse_unquoted_assoc_value(bytes, pos),
        }
    }

    fn parse_double_quoted_assoc_value(bytes: &[u8], pos: &mut usize) -> String {
        let mut value = String::new();
        *pos += 1;
        while *pos < bytes.len() && bytes[*pos] != b'"' {
            if bytes[*pos] == b'\\' && *pos + 1 < bytes.len() {
                *pos += 1;
            }
            value.push(bytes[*pos] as char);
            *pos += 1;
        }
        if *pos < bytes.len() {
            *pos += 1;
        }
        value
    }

    fn parse_single_quoted_assoc_value(bytes: &[u8], pos: &mut usize) -> String {
        let mut value = String::new();
        *pos += 1;
        while *pos < bytes.len() && bytes[*pos] != b'\'' {
            value.push(bytes[*pos] as char);
            *pos += 1;
        }
        if *pos < bytes.len() {
            *pos += 1;
        }
        value
    }

    fn parse_unquoted_assoc_value(bytes: &[u8], pos: &mut usize) -> String {
        let mut value = String::new();
        while *pos < bytes.len() && !bytes[*pos].is_ascii_whitespace() {
            value.push(bytes[*pos] as char);
            *pos += 1;
        }
        value
    }

    /// Maximum number of arguments after glob expansion.
    const MAX_GLOB_RESULTS: usize = 10_000;

    /// Expand glob patterns in argv against the VFS.
    /// Supports: basic glob (`*`, `?`, `[...]`), globstar (`**`), nullglob,
    /// dotglob, and extglob patterns.
    /// When `set -f` (noglob) is active, glob expansion is skipped entirely.
    /// Expand globs in argv, skipping entries tagged as quoted.
    fn expand_globs_tagged(&mut self, argv: Vec<(String, bool)>) -> Vec<String> {
        if self.vm.state.get_var("SHOPT_f").as_deref() == Some("1") {
            return argv.into_iter().map(|(s, _)| s).collect();
        }
        let nullglob = self.get_shopt_value("nullglob");
        let dotglob = self.get_shopt_value("dotglob");
        let globstar = self.get_shopt_value("globstar");
        let extglob = self.get_shopt_value("extglob");

        let mut result = Vec::new();
        for (arg, quoted) in argv {
            if quoted {
                result.push(arg);
            } else {
                result.extend(self.expand_glob_arg(arg, nullglob, dotglob, globstar, extglob));
            }
        }
        result.truncate(Self::MAX_GLOB_RESULTS);
        result
    }

    fn expand_globs(&mut self, argv: Vec<String>) -> Vec<String> {
        if self.vm.state.get_var("SHOPT_f").as_deref() == Some("1") {
            return argv;
        }
        let nullglob = self.get_shopt_value("nullglob");
        let dotglob = self.get_shopt_value("dotglob");
        let globstar = self.get_shopt_value("globstar");
        let extglob = self.get_shopt_value("extglob");

        let mut result = Vec::new();
        for arg in argv {
            result.extend(self.expand_glob_arg(arg, nullglob, dotglob, globstar, extglob));
        }
        result.truncate(Self::MAX_GLOB_RESULTS);
        result
    }

    #[allow(clippy::fn_params_excessive_bools)]
    fn expand_glob_arg(
        &self,
        arg: String,
        nullglob: bool,
        dotglob: bool,
        globstar: bool,
        extglob: bool,
    ) -> Vec<String> {
        if !Self::is_glob_pattern(&arg, extglob) {
            return vec![arg];
        }
        if globstar && arg.contains("**") {
            return self.expand_globstar_arg(arg, nullglob, dotglob, extglob);
        }
        self.expand_standard_glob_arg(arg, nullglob, dotglob, extglob)
    }

    fn is_glob_pattern(arg: &str, extglob: bool) -> bool {
        let has_bracket_class = arg.contains('[') && arg.contains(']');
        arg.contains('*')
            || arg.contains('?')
            || has_bracket_class
            || (extglob && has_extglob_pattern(arg))
    }

    fn expand_globstar_arg(
        &self,
        arg: String,
        nullglob: bool,
        dotglob: bool,
        extglob: bool,
    ) -> Vec<String> {
        let mut matches = self.expand_globstar(&arg, dotglob, extglob);
        matches.sort();
        self.finalize_glob_matches(arg, matches, nullglob)
    }

    fn expand_standard_glob_arg(
        &self,
        arg: String,
        nullglob: bool,
        dotglob: bool,
        extglob: bool,
    ) -> Vec<String> {
        let Some((dir, pattern, prefix)) = self.split_glob_search(&arg) else {
            return self.finalize_glob_matches(arg.clone(), Vec::new(), nullglob);
        };
        let matches = self.read_glob_matches(&dir, &pattern, prefix.as_deref(), dotglob, extglob);
        self.finalize_glob_matches(arg, matches, nullglob)
    }

    fn split_glob_search(&self, arg: &str) -> Option<(String, String, Option<String>)> {
        let Some(slash_pos) = arg.rfind('/') else {
            return Some((self.vm.state.cwd.clone(), arg.to_string(), None));
        };

        let dir_part = &arg[..=slash_pos];
        if Self::path_segment_has_glob(dir_part) {
            return None;
        }

        Some((
            self.resolve_cwd_path(dir_part),
            arg[slash_pos + 1..].to_string(),
            Some(dir_part.to_string()),
        ))
    }

    fn path_segment_has_glob(path: &str) -> bool {
        path.contains('*') || path.contains('?') || path.contains('[')
    }

    fn read_glob_matches(
        &self,
        dir: &str,
        pattern: &str,
        prefix: Option<&str>,
        dotglob: bool,
        extglob: bool,
    ) -> Vec<String> {
        let Ok(entries) = self.fs.read_dir(dir) else {
            return Vec::new();
        };

        let mut matches: Vec<String> = entries
            .iter()
            .filter(|e| glob_match_ext(pattern, &e.name, dotglob, extglob))
            .map(|e| match prefix {
                Some(prefix) => format!("{prefix}{}", e.name),
                None => e.name.clone(),
            })
            .collect();
        matches.sort();
        matches
    }

    #[allow(clippy::unused_self)]
    fn finalize_glob_matches(
        &self,
        arg: String,
        matches: Vec<String>,
        nullglob: bool,
    ) -> Vec<String> {
        if !matches.is_empty() {
            return matches;
        }
        if nullglob {
            Vec::new()
        } else {
            vec![arg]
        }
    }

    /// Expand a globstar (**) pattern against the VFS with recursive directory traversal.
    fn expand_globstar(&self, pattern: &str, dotglob: bool, extglob: bool) -> Vec<String> {
        // Split pattern into segments by /
        let segments: Vec<&str> = pattern.split('/').collect();
        let base_dir = self.vm.state.cwd.clone();
        let mut matches = Vec::new();
        self.globstar_walk(&base_dir, &segments, 0, "", dotglob, extglob, &mut matches);
        matches
    }

    /// Recursive walk for globstar expansion.
    fn globstar_walk(
        &self,
        dir: &str,
        segments: &[&str],
        seg_idx: usize,
        prefix: &str,
        dotglob: bool,
        extglob: bool,
        matches: &mut Vec<String>,
    ) {
        if seg_idx >= segments.len() {
            return;
        }

        let seg = segments[seg_idx];
        if seg == "**" {
            self.globstar_walk_wildcard(dir, segments, seg_idx, prefix, dotglob, extglob, matches);
            return;
        }
        self.globstar_walk_segment(
            dir, seg, segments, seg_idx, prefix, dotglob, extglob, matches,
        );
    }

    fn globstar_walk_wildcard(
        &self,
        dir: &str,
        segments: &[&str],
        seg_idx: usize,
        prefix: &str,
        dotglob: bool,
        extglob: bool,
        matches: &mut Vec<String>,
    ) {
        if seg_idx + 1 < segments.len() {
            self.globstar_walk(
                dir,
                segments,
                seg_idx + 1,
                prefix,
                dotglob,
                extglob,
                matches,
            );
        }

        let Ok(entries) = self.fs.read_dir(dir) else {
            return;
        };
        for entry in &entries {
            if !dotglob && entry.name.starts_with('.') {
                continue;
            }
            let (child_path, child_prefix) = Self::globstar_child_paths(dir, prefix, &entry.name);
            if self.fs.stat(&child_path).is_ok_and(|m| m.is_dir) {
                self.globstar_walk(
                    &child_path,
                    segments,
                    seg_idx,
                    &child_prefix,
                    dotglob,
                    extglob,
                    matches,
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn globstar_walk_segment(
        &self,
        dir: &str,
        seg: &str,
        segments: &[&str],
        seg_idx: usize,
        prefix: &str,
        dotglob: bool,
        extglob: bool,
        matches: &mut Vec<String>,
    ) {
        let Ok(entries) = self.fs.read_dir(dir) else {
            return;
        };
        let is_last = seg_idx == segments.len() - 1;

        for entry in &entries {
            if !glob_match_ext(seg, &entry.name, dotglob, extglob) {
                continue;
            }
            self.globstar_handle_matched_entry(
                dir,
                segments,
                seg_idx,
                prefix,
                dotglob,
                extglob,
                matches,
                &entry.name,
                is_last,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn globstar_handle_matched_entry(
        &self,
        dir: &str,
        segments: &[&str],
        seg_idx: usize,
        prefix: &str,
        dotglob: bool,
        extglob: bool,
        matches: &mut Vec<String>,
        name: &str,
        is_last: bool,
    ) {
        let (child_path, child_prefix) = Self::globstar_child_paths(dir, prefix, name);
        if is_last {
            matches.push(child_prefix);
            return;
        }
        let is_dir = self.fs.stat(&child_path).is_ok_and(|m| m.is_dir);
        if is_dir {
            self.globstar_walk(
                &child_path,
                segments,
                seg_idx + 1,
                &child_prefix,
                dotglob,
                extglob,
                matches,
            );
        }
    }

    fn globstar_child_paths(dir: &str, prefix: &str, name: &str) -> (String, String) {
        let child_path = if dir == "/" {
            format!("/{name}")
        } else {
            format!("{dir}/{name}")
        };
        let child_prefix = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        (child_path, child_prefix)
    }

    /// Write data to a file path, reporting errors to stderr.
    fn write_to_file(&mut self, path: &str, target: &str, data: &[u8], opts: OpenOptions) {
        match self.fs.open(path, opts) {
            Ok(h) => {
                if let Err(e) = self.fs.write_file(h, data) {
                    self.write_stderr(format!("wasmsh: write error: {e}\n").as_bytes());
                }
                self.fs.close(h);
            }
            Err(e) => {
                self.write_stderr(format!("wasmsh: {target}: {e}\n").as_bytes());
            }
        }
    }

    fn current_stdout_len(&self) -> usize {
        for capture in self.exec.output_captures.iter().rev() {
            if capture.capture_stdout {
                return capture.stdout.len();
            }
        }
        self.vm.stdout.len()
    }

    /// Capture stdout data from the given position, truncating the active stdout buffer.
    fn capture_stdout(&mut self, from: usize) -> Vec<u8> {
        for capture in self.exec.output_captures.iter_mut().rev() {
            if capture.capture_stdout {
                let data = capture.stdout[from..].to_vec();
                capture.stdout.truncate(from);
                return data;
            }
        }

        let data = self.vm.stdout[from..].to_vec();
        self.vm.stdout.truncate(from);
        data
    }

    /// Drain the active stderr buffer.
    fn take_stderr(&mut self) -> Vec<u8> {
        for capture in self.exec.output_captures.iter_mut().rev() {
            if capture.capture_stderr {
                return std::mem::take(&mut capture.stderr);
            }
        }
        std::mem::take(&mut self.vm.stderr)
    }

    fn process_subst_out_sink_mut(&mut self, path: &str) -> Option<&mut PendingProcessSubstOut> {
        for scope in self.proc_subst_out_scopes.iter_mut().rev() {
            if let Some(index) = scope.iter().position(|sink| sink.path == path) {
                return scope.get_mut(index);
            }
        }
        None
    }

    fn write_process_subst_out_with_parent(
        &mut self,
        path: &str,
        data: &[u8],
        clear: bool,
    ) -> bool {
        for scope_index in (0..self.proc_subst_out_scopes.len()).rev() {
            let maybe_index = self.proc_subst_out_scopes[scope_index]
                .iter()
                .position(|sink| sink.path == path);
            if let Some(index) = maybe_index {
                let mut sink = self.proc_subst_out_scopes[scope_index].remove(index);
                if clear {
                    sink.clear();
                }
                sink.write_with_parent(self, data);
                self.proc_subst_out_scopes[scope_index].insert(index, sink);
                return true;
            }
        }
        false
    }

    fn prepare_exec_io(&mut self, redirections: &[HirRedirection]) -> Result<Option<ExecIo>, ()> {
        let mut exec_io = self.current_exec_io.clone().unwrap_or_default();
        let mut handled_any = false;
        for redir in redirections {
            if self.apply_hir_redir(redir, &mut exec_io)? {
                handled_any = true;
            }
        }
        Ok(handled_any.then_some(exec_io))
    }

    fn apply_hir_redir(
        &mut self,
        redir: &HirRedirection,
        exec_io: &mut ExecIo,
    ) -> Result<bool, ()> {
        match redir.op {
            RedirectionOp::HereDoc | RedirectionOp::HereDocStrip => {
                self.apply_heredoc_redir(redir, exec_io);
                Ok(true)
            }
            RedirectionOp::HereString => {
                self.apply_herestring_redir(redir, exec_io);
                Ok(true)
            }
            RedirectionOp::Input => self.apply_input_redir(redir, exec_io).map(|()| true),
            RedirectionOp::Output
            | RedirectionOp::Append
            | RedirectionOp::Clobber
            | RedirectionOp::AppendBoth => self.apply_write_redir(redir, exec_io).map(|()| true),
            RedirectionOp::DupOutput => {
                self.apply_dup_output_redir(redir, exec_io);
                Ok(true)
            }
            RedirectionOp::DupInput => {
                self.apply_dup_input_redir(redir, exec_io);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn resolve_redir_target(&mut self, redir: &HirRedirection) -> String {
        let resolved = self.resolve_command_subst(std::slice::from_ref(&redir.target));
        let resolved_target = resolved.first().unwrap_or(&redir.target);
        wasmsh_expand::expand_word(resolved_target, &mut self.vm.state)
    }

    fn apply_heredoc_redir(&mut self, redir: &HirRedirection, exec_io: &mut ExecIo) {
        if let Some(body) = &redir.here_doc_body {
            let content = if body.expand {
                wasmsh_expand::expand_string(&body.content, &mut self.vm.state)
            } else {
                body.content.to_string()
            };
            exec_io
                .fds_mut()
                .set_input(InputTarget::Bytes(content.into_bytes()));
        }
    }

    fn apply_herestring_redir(&mut self, redir: &HirRedirection, exec_io: &mut ExecIo) {
        let content = self.resolve_redir_target(redir);
        let mut data = content.into_bytes();
        data.push(b'\n');
        exec_io.fds_mut().set_input(InputTarget::Bytes(data));
    }

    fn apply_input_redir(
        &mut self,
        redir: &HirRedirection,
        exec_io: &mut ExecIo,
    ) -> Result<(), ()> {
        let target = self.resolve_redir_target(redir);
        let path = self.resolve_cwd_path(&target);
        match self.fs.stat(&path) {
            Ok(metadata) if !metadata.is_dir => {
                exec_io.fds_mut().set_input(InputTarget::File {
                    path,
                    remove_after_read: false,
                });
                Ok(())
            }
            Ok(_) => self.fail_input_redir(&target, "Is a directory"),
            Err(_) => self.fail_input_redir(&target, "No such file or directory"),
        }
    }

    fn fail_input_redir(&mut self, target: &str, reason: &str) -> Result<(), ()> {
        let msg = format!("wasmsh: {target}: {reason}\n");
        self.write_stderr(msg.as_bytes());
        self.vm.state.last_status = 1;
        Err(())
    }

    fn apply_write_redir(
        &mut self,
        redir: &HirRedirection,
        exec_io: &mut ExecIo,
    ) -> Result<(), ()> {
        let target = self.resolve_redir_target(redir);
        let path = self.resolve_cwd_path(&target);
        let append = matches!(redir.op, RedirectionOp::Append | RedirectionOp::AppendBoth);
        let clear_before = matches!(redir.op, RedirectionOp::Output | RedirectionOp::Clobber);

        if matches!(redir.op, RedirectionOp::Output) && self.noclobber_rejects(&path, &target) {
            return Err(());
        }

        let destination = self.open_write_destination(path, &target, append, clear_before)?;
        Self::attach_write_destination(redir, exec_io, destination);
        Ok(())
    }

    fn open_write_destination(
        &mut self,
        path: String,
        target: &str,
        append: bool,
        clear_before: bool,
    ) -> Result<OutputTarget, ()> {
        if self.process_subst_out_sink_mut(&path).is_some() {
            if clear_before {
                if let Some(sink) = self.process_subst_out_sink_mut(&path) {
                    sink.clear();
                }
            }
            return Ok(OutputTarget::ProcessSubst { path });
        }
        match self.fs.open_write_sink(&path, append) {
            Ok(sink) => Ok(OutputTarget::File {
                path,
                append,
                sink: Rc::new(RefCell::new(sink)),
            }),
            Err(err) => {
                let msg = format!("wasmsh: {target}: {err}\n");
                self.write_stderr(msg.as_bytes());
                self.vm.state.last_status = 1;
                Err(())
            }
        }
    }

    fn attach_write_destination(
        redir: &HirRedirection,
        exec_io: &mut ExecIo,
        destination: OutputTarget,
    ) {
        let default_fd = if matches!(redir.op, RedirectionOp::AppendBoth) {
            FD_BOTH
        } else {
            1
        };
        match redir.fd.unwrap_or(default_fd) {
            FD_BOTH => {
                exec_io.fds_mut().open_output(1, destination.clone());
                exec_io.fds_mut().open_output(2, destination);
            }
            2 => exec_io.fds_mut().open_output(2, destination),
            _ => exec_io.fds_mut().open_output(1, destination),
        }
    }

    fn apply_dup_output_redir(&mut self, redir: &HirRedirection, exec_io: &mut ExecIo) {
        let target = self.resolve_redir_target(redir);
        let source_fd = redir.fd.unwrap_or(1);
        if target == "-" {
            exec_io.fds_mut().close(source_fd);
        } else if let Ok(target_fd) = target.parse() {
            exec_io.fds_mut().dup_output(source_fd, target_fd);
        }
    }

    fn apply_dup_input_redir(&mut self, redir: &HirRedirection, exec_io: &mut ExecIo) {
        let target = self.resolve_redir_target(redir);
        let source_fd = redir.fd.unwrap_or(0);
        if target == "-" {
            exec_io.fds_mut().close(source_fd);
        } else if let Ok(target_fd) = target.parse() {
            exec_io.fds_mut().dup_input(source_fd, target_fd);
        }
    }

    /// Apply redirections: for `>` and `>>`, write captured stdout/stderr to file.
    /// For `<`, read file content (handled pre-execution).
    /// Supports fd-specific redirections (2>, 2>>) and &> (both stdout and stderr).
    fn apply_redirections(&mut self, redirections: &[HirRedirection], stdout_before: usize) {
        for redir in redirections {
            if !self.apply_single_redirection(redir, stdout_before) {
                return;
            }
        }
    }

    fn apply_single_redirection(&mut self, redir: &HirRedirection, stdout_before: usize) -> bool {
        let resolved = self.resolve_command_subst(std::slice::from_ref(&redir.target));
        let resolved_target = resolved.first().unwrap_or(&redir.target);
        let target = wasmsh_expand::expand_word(resolved_target, &mut self.vm.state);
        let path = self.resolve_cwd_path(&target);
        let fd = redir.fd.unwrap_or(1);
        match redir.op {
            RedirectionOp::Output => {
                if self.noclobber_rejects(&path, &target) {
                    return false;
                }
                self.apply_output_redir(&path, &target, fd, stdout_before);
            }
            RedirectionOp::Clobber => {
                self.apply_output_redir(&path, &target, fd, stdout_before);
            }
            RedirectionOp::Append => {
                self.apply_append_redir(&path, &target, fd, stdout_before);
            }
            RedirectionOp::AppendBoth => {
                self.apply_append_redir(&path, &target, FD_BOTH, stdout_before);
            }
            RedirectionOp::DupOutput => {
                self.apply_dup_output_redir_inline(redir, &target, stdout_before);
            }
            #[allow(unreachable_patterns)]
            _ => {}
        }
        true
    }

    fn apply_dup_output_redir_inline(
        &mut self,
        redir: &HirRedirection,
        target: &str,
        stdout_before: usize,
    ) {
        let source_fd = redir.fd.unwrap_or(1);
        if target == "-" {
            if source_fd == 2 {
                self.take_stderr();
            } else {
                self.capture_stdout(stdout_before);
            }
            return;
        }
        let target_fd = target.parse::<u32>().ok();
        if target_fd == Some(1) && source_fd == 2 {
            let stderr_data = self.take_stderr();
            self.write_stdout(&stderr_data);
        } else if target_fd == Some(2) && source_fd == 1 {
            let stdout_data = self.capture_stdout(stdout_before);
            self.write_stderr(&stdout_data);
        }
    }

    /// Apply `>` output redirection for a specific fd.
    fn apply_output_redir(&mut self, path: &str, target: &str, fd: u32, stdout_before: usize) {
        let data = if fd == FD_BOTH {
            let mut combined = self.capture_stdout(stdout_before);
            combined.extend_from_slice(&self.take_stderr());
            combined
        } else if fd == 2 {
            self.take_stderr()
        } else {
            self.capture_stdout(stdout_before)
        };
        if self.write_process_subst_out_with_parent(path, &data, true) {
            return;
        }
        self.write_to_file(path, target, &data, OpenOptions::write());
    }

    /// Apply `>>` append redirection for a specific fd.
    fn apply_append_redir(&mut self, path: &str, target: &str, fd: u32, stdout_before: usize) {
        let data = if fd == FD_BOTH {
            let mut combined = self.capture_stdout(stdout_before);
            combined.extend_from_slice(&self.take_stderr());
            combined
        } else if fd == 2 {
            self.take_stderr()
        } else {
            self.capture_stdout(stdout_before)
        };
        if self.write_process_subst_out_with_parent(path, &data, false) {
            return;
        }
        self.write_to_file(path, target, &data, OpenOptions::append());
    }

    fn noclobber_rejects(&mut self, path: &str, target: &str) -> bool {
        if self.vm.state.get_var("SHOPT_C").as_deref() != Some("1") {
            return false;
        }
        if self.fs.stat(path).is_err() {
            return false;
        }
        self.write_stderr(format!("wasmsh: {target}: cannot overwrite existing file\n").as_bytes());
        self.vm.state.last_status = 1;
        true
    }
}

/// Move the `read` builtin's unconsumed stdin tail back into the active fd
/// table so later `read` calls in the same shell scope resume it. Streaming
/// readers cannot be un-read, so `read` buffers the whole source and leaves
/// the tail in `_STDIN_REMAINING`.
fn install_read_remainder(state: &mut ShellState, current_exec_io: &mut Option<ExecIo>) {
    let Some(rem) = state.get_var("_STDIN_REMAINING") else {
        return;
    };
    current_exec_io
        .get_or_insert_with(ExecIo::default)
        .fds_mut()
        .set_input(InputTarget::Bytes(rem.as_bytes().to_vec()));
}

fn default_clock_provider() -> Rc<dyn ClockProvider> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        Rc::from(Box::new(SystemClock::new()) as Box<dyn ClockProvider>)
    }
    #[cfg(target_arch = "wasm32")]
    {
        Rc::from(Box::new(UnavailableClock) as Box<dyn ClockProvider>)
    }
}

/// Convert a protocol diagnostic level to a VM diagnostic level.
fn convert_diag_level(level: DiagnosticLevel) -> wasmsh_vm::DiagLevel {
    match level {
        DiagnosticLevel::Trace => wasmsh_vm::DiagLevel::Trace,
        DiagnosticLevel::Warning => wasmsh_vm::DiagLevel::Warning,
        DiagnosticLevel::Error => wasmsh_vm::DiagLevel::Error,
        _ => wasmsh_vm::DiagLevel::Info,
    }
}

impl Default for WorkerRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use wasmsh_utils::{ClockError, FixedClock, UtcDateTime};

    struct RecordingNetworkBackend {
        requests: Rc<RefCell<Vec<String>>>,
    }

    impl NetworkBackend for RecordingNetworkBackend {
        fn fetch(
            &self,
            request: &wasmsh_utils::net_types::HttpRequest,
        ) -> Result<wasmsh_utils::net_types::HttpResponse, NetworkError> {
            self.requests.borrow_mut().push(request.url.clone());
            Ok(wasmsh_utils::net_types::HttpResponse {
                status: 200,
                ..Default::default()
            })
        }
    }

    struct SequenceClock {
        values: RefCell<Vec<i64>>,
    }

    impl SequenceClock {
        fn new(values: Vec<i64>) -> Self {
            Self {
                values: RefCell::new(values),
            }
        }
    }

    impl ClockProvider for SequenceClock {
        fn now_unix_ms(&self) -> Result<i64, ClockError> {
            let mut values = self.values.borrow_mut();
            Ok(values.pop().unwrap_or(0))
        }

        fn monotonic_now_ms(&self) -> Result<u64, ClockError> {
            Ok(0)
        }
    }

    fn first_and_or(source: &str) -> HirAndOr {
        let ast = wasmsh_parse::parse(source).unwrap();
        let hir = wasmsh_hir::lower(&ast);
        hir.items[0].list[0].clone()
    }

    fn get_stdout(events: &[WorkerEvent]) -> String {
        let mut out = Vec::new();
        for event in events {
            if let WorkerEvent::Stdout(data) = event {
                out.extend_from_slice(data);
            }
        }
        String::from_utf8(out).unwrap_or_default()
    }

    fn get_stderr(events: &[WorkerEvent]) -> String {
        let mut out = Vec::new();
        for event in events {
            if let WorkerEvent::Stderr(data) = event {
                out.extend_from_slice(data);
            }
        }
        String::from_utf8(out).unwrap_or_default()
    }

    fn get_exit(events: &[WorkerEvent]) -> i32 {
        events
            .iter()
            .find_map(|event| match event {
                WorkerEvent::Exit(status) => Some(*status),
                _ => None,
            })
            .unwrap_or(-1)
    }

    fn has_output_limit_diagnostic(events: &[WorkerEvent]) -> bool {
        events.iter().any(|event| {
            matches!(
                event,
                WorkerEvent::Diagnostic(_, message) if message.contains("output limit exceeded")
            )
        })
    }

    #[test]
    fn repeated_init_replaces_network_policy_without_stacking_old_policy() {
        let requests = Rc::new(RefCell::new(Vec::new()));
        let mut runtime = WorkerRuntime::new();
        runtime.set_network_backend(Box::new(RecordingNetworkBackend {
            requests: requests.clone(),
        }));

        let first_init = runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: Some(ProtocolNetworkPolicyConfig {
                enabled: true,
                default_action: wasmsh_protocol::NetworkDefaultAction::Deny,
                allow: vec!["first.example".into()],
                deny: vec![],
            }),
        });
        assert!(matches!(first_init.as_slice(), [WorkerEvent::Version(_)]));
        assert_eq!(
            get_exit(&runtime.handle_command(HostCommand::Run {
                input: "curl http://first.example/".into(),
            })),
            0
        );

        let second_init = runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: Some(ProtocolNetworkPolicyConfig {
                enabled: true,
                default_action: wasmsh_protocol::NetworkDefaultAction::Deny,
                allow: vec!["second.example".into()],
                deny: vec![],
            }),
        });
        assert!(matches!(second_init.as_slice(), [WorkerEvent::Version(_)]));
        assert_ne!(
            get_exit(&runtime.handle_command(HostCommand::Run {
                input: "curl http://first.example/".into(),
            })),
            0
        );
        assert_eq!(
            get_exit(&runtime.handle_command(HostCommand::Run {
                input: "curl http://second.example/".into(),
            })),
            0
        );
        assert_eq!(
            requests.borrow().as_slice(),
            ["http://first.example/", "http://second.example/"]
        );
    }

    #[test]
    fn date_samples_each_command_without_reinitializing_the_session() {
        let first = UtcDateTime::from_calendar(2025, 12, 31, 23, 59, 59, 0)
            .unwrap()
            .epoch_ms();
        let second = UtcDateTime::from_calendar(2026, 1, 1, 0, 0, 0, 0)
            .unwrap()
            .epoch_ms();
        let clock = SequenceClock::new(vec![second, first]);
        let mut runtime = WorkerRuntime::new();
        runtime.set_clock_provider(Box::new(clock));
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let same_run = runtime.handle_command(HostCommand::Run {
            input: "date '+%s'; date '+%s'".into(),
        });
        assert_eq!(get_stdout(&same_run), "1767225599\n1767225600\n");
        assert_eq!(get_exit(&same_run), 0);
    }

    #[test]
    fn fixed_clock_advances_between_runs_and_monotonic_seconds_are_separate() {
        let start = UtcDateTime::from_calendar(2026, 1, 1, 0, 0, 0, 0)
            .unwrap()
            .epoch_ms();
        let clock = FixedClock::new(start).unwrap();
        let mut runtime = WorkerRuntime::new();
        runtime.set_clock_provider(Box::new(clock.clone()));
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        let first = runtime.handle_command(HostCommand::Run {
            input: "date '+%F'; echo $SECONDS".into(),
        });
        clock.advance_ms(86_400_000).unwrap();
        let second = runtime.handle_command(HostCommand::Run {
            input: "date '+%F'; echo $SECONDS".into(),
        });
        assert_eq!(get_stdout(&first), "2026-01-01\n0\n");
        assert_eq!(get_stdout(&second), "2026-01-02\n86400\n");
        assert_eq!(get_exit(&second), 0);
    }

    #[test]
    fn command_v_discovers_bundled_utilities() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        let events = runtime.handle_command(HostCommand::Run {
            input: "command -v curl; command -v jq; type -t curl".into(),
        });
        // An AI adapter must be able to enumerate bundled utilities even
        // though they are not VFS files.
        assert_eq!(get_stdout(&events), "curl\njq\nutility\n");
        assert_eq!(get_exit(&events), 0);
    }

    #[test]
    fn init_seeds_deterministic_home_pwd_and_path() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        // `cd` with no args, `~` expansion, and `$PWD` must work without any
        // host environment, which is the AI-facing environment contract.
        let events = runtime.handle_command(HostCommand::Run {
            input: "echo $HOME; echo $PWD; cd; echo $PWD; echo ~".into(),
        });
        assert_eq!(
            get_stdout(&events),
            "/home/user\n/\n/home/user\n/home/user\n"
        );
        assert_eq!(get_exit(&events), 0);
    }

    #[test]
    fn output_limit_exposes_structured_exhaustion_reason() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.set_output_byte_limit(3);

        let events = runtime.handle_command(HostCommand::Run {
            input: "echo hello".into(),
        });

        assert_eq!(get_exit(&events), 128);
        assert!(has_output_limit_diagnostic(&events));
        assert_eq!(
            runtime.exec.stop_reason,
            Some(StopReason::Exhausted(ExhaustionReason {
                category: BudgetCategory::VisibleOutputBytes,
                used: 6,
                limit: 3,
            }))
        );
    }

    #[test]
    fn recursion_limit_exposes_structured_exhaustion_reason() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.set_recursion_limit(2);

        let events = runtime.handle_command(HostCommand::Run {
            input: "f(){ f; }\nf".into(),
        });

        assert_eq!(get_exit(&events), 128);
        assert!(get_stderr(&events).contains("maximum recursion depth exceeded"));
        assert_eq!(
            runtime.exec.stop_reason,
            Some(StopReason::Exhausted(ExhaustionReason {
                category: BudgetCategory::RecursionDepth,
                used: 3,
                limit: 2,
            }))
        );
    }

    #[test]
    fn pipe_limit_exposes_structured_exhaustion_reason() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.set_pipe_byte_limit(1);

        let events = runtime.handle_command(HostCommand::Run {
            input: "printf 'ab' | cat".into(),
        });

        assert_eq!(get_exit(&events), 128);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                WorkerEvent::Diagnostic(_, message) if message.contains("pipe buffer limit exceeded")
            )
        }));
        assert!(matches!(
            runtime.exec.stop_reason,
            Some(StopReason::Exhausted(ExhaustionReason {
                category: BudgetCategory::PipeBytes,
                ..
            }))
        ));
    }

    #[test]
    fn vm_subset_boundary_accepts_simple_builtin_and_or() {
        let runtime = WorkerRuntime::new();
        let program = runtime
            .lower_vm_subset_and_or(&first_and_or("true && echo ok"))
            .expect("simple builtin and/or should lower");
        assert!(!program.instructions.is_empty());
    }

    #[test]
    fn vm_subset_boundary_rejects_multi_stage_pipeline() {
        let runtime = WorkerRuntime::new();
        let reason = runtime
            .lower_vm_subset_and_or(&first_and_or("echo hello | cat"))
            .unwrap_err();
        assert_eq!(
            reason,
            VmSubsetFallbackReason::Lowering(LoweringError::Unsupported(
                "pipeline shape is outside the VM subset"
            ))
        );
    }

    #[test]
    fn vm_subset_boundary_rejects_alias_expansion() {
        let mut runtime = WorkerRuntime::new();
        runtime
            .vm
            .state
            .set_var("SHOPT_expand_aliases".into(), "1".into());
        runtime.aliases.insert("echo".into(), "printf".into());
        let reason = runtime
            .lower_vm_subset_and_or(&first_and_or("echo hello"))
            .unwrap_err();
        assert_eq!(reason, VmSubsetFallbackReason::AliasExpansion);
    }

    #[test]
    fn streaming_yes_head_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.vm.limits.output_byte_limit = 10;

        let events = runtime.handle_command(HostCommand::Run {
            input: "yes | head -n 5".into(),
        });

        assert_eq!(get_stdout(&events), "y\ny\ny\ny\ny\n");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn streaming_yes_cat_head_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.vm.limits.output_byte_limit = 10;

        let events = runtime.handle_command(HostCommand::Run {
            input: "yes | cat | head -n 5".into(),
        });

        assert_eq!(get_stdout(&events), "y\ny\ny\ny\ny\n");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn streaming_yes_head_wc_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.vm.limits.output_byte_limit = 8;

        let events = runtime.handle_command(HostCommand::Run {
            input: "yes | head -n 5 | wc -l".into(),
        });

        assert_eq!(get_stdout(&events), "5\n");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn streaming_cat_file_head_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.handle_command(HostCommand::WriteFile {
            path: "/big.txt".into(),
            data: b"abcdefghijklmnopqrstuvwxyz".to_vec(),
        });
        runtime.vm.limits.output_byte_limit = 10;

        let events = runtime.handle_command(HostCommand::Run {
            input: "cat /big.txt | head -c 10".into(),
        });

        assert_eq!(get_stdout(&events), "abcdefghij");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn streaming_yes_tr_head_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.vm.limits.output_byte_limit = 10;

        let events = runtime.handle_command(HostCommand::Run {
            input: "yes | tr y z | head -n 5".into(),
        });

        assert_eq!(get_stdout(&events), "z\nz\nz\nz\nz\n");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn streaming_yes_grep_head_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.vm.limits.output_byte_limit = 10;

        let events = runtime.handle_command(HostCommand::Run {
            input: "yes | grep y | head -n 5".into(),
        });

        assert_eq!(get_stdout(&events), "y\ny\ny\ny\ny\n");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn streaming_yes_tee_head_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.vm.limits.output_byte_limit = 10;

        let events = runtime.handle_command(HostCommand::Run {
            input: "yes | tee /tee.txt | head -n 5".into(),
        });

        assert_eq!(get_stdout(&events), "y\ny\ny\ny\ny\n");
        assert!(!has_output_limit_diagnostic(&events));

        let file_events = runtime.handle_command(HostCommand::ReadFile {
            path: "/tee.txt".into(),
        });
        assert_eq!(get_stdout(&file_events), "y\ny\ny\ny\ny\n");
    }

    #[test]
    fn streaming_buffered_sort_tee_cat_preserves_sorted_output() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = runtime.handle_command(HostCommand::Run {
            input: "printf 'b\\na\\n' | sort | tee /sorted.txt | cat".into(),
        });

        assert_eq!(get_stdout(&events), "a\nb\n");
        let file_events = runtime.handle_command(HostCommand::ReadFile {
            path: "/sorted.txt".into(),
        });
        assert_eq!(get_stdout(&file_events), "a\nb\n");
    }

    #[test]
    fn streaming_yes_rev_head_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.vm.limits.output_byte_limit = 10;

        let events = runtime.handle_command(HostCommand::Run {
            input: "yes | rev | head -n 5".into(),
        });

        assert_eq!(get_stdout(&events), "y\ny\ny\ny\ny\n");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn streaming_echo_cut_head_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.vm.limits.output_byte_limit = 6;

        let events = runtime.handle_command(HostCommand::Run {
            input: "echo abc:def | cut -d: -f2 | head -c 4".into(),
        });

        assert_eq!(get_stdout(&events), "def\n");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn streaming_echo_tail_head_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.vm.limits.output_byte_limit = 3;

        let events = runtime.handle_command(HostCommand::Run {
            input: "echo -e 'a\\nb\\nc' | tail -n 2 | head -n 1".into(),
        });

        assert_eq!(get_stdout(&events), "b\n");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn streaming_yes_bat_head_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        let expected = "    1   │ y\n    2   │ y\n";
        runtime.vm.limits.output_byte_limit = expected.len() as u64;

        let events = runtime.handle_command(HostCommand::Run {
            input: "yes | bat --style=numbers | head -n 2".into(),
        });

        assert_eq!(get_stdout(&events), expected);
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn streaming_yes_sed_head_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.vm.limits.output_byte_limit = 10;

        let events = runtime.handle_command(HostCommand::Run {
            input: "yes | sed 's/y/z/' | head -n 5".into(),
        });

        assert_eq!(get_stdout(&events), "z\nz\nz\nz\nz\n");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn streaming_echo_paste_serial_head_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.vm.limits.output_byte_limit = 6;

        let events = runtime.handle_command(HostCommand::Run {
            input: "echo -e 'a\\nb\\nc' | paste -s -d , | head -c 6".into(),
        });

        assert_eq!(get_stdout(&events), "a,b,c\n");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn streaming_echo_column_head_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.vm.limits.output_byte_limit = 4;

        let events = runtime.handle_command(HostCommand::Run {
            input: "echo abc | column | head -c 4".into(),
        });

        assert_eq!(get_stdout(&events), "abc\n");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn streaming_echo_uniq_head_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.vm.limits.output_byte_limit = 6;

        let events = runtime.handle_command(HostCommand::Run {
            input: "echo -e 'a\\na\\nb' | uniq | head -n 2".into(),
        });

        assert_eq!(get_stdout(&events), "a\nb\n");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn streaming_buffered_printf_sort_head_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.vm.limits.output_byte_limit = 2;

        let events = runtime.handle_command(HostCommand::Run {
            input: "printf 'b\\na\\n' | sort | head -n 1".into(),
        });

        assert_eq!(get_stdout(&events), "a\n");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn streaming_buffered_function_stage_preserves_output() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = runtime.handle_command(HostCommand::Run {
            input: "f(){ cat; }\nprintf hi | f | head -c 2".into(),
        });

        assert_eq!(get_stdout(&events), "hi");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn streaming_buffered_function_pipe_stderr_respects_visible_output_limit() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.vm.limits.output_byte_limit = 8;

        let events = runtime.handle_command(HostCommand::Run {
            input: "f(){ echo out; echo err >&2; }\nf |& head -n 2".into(),
        });

        assert_eq!(get_stdout(&events), "out\nerr\n");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn scheduled_group_stage_pipe_stderr_preserves_output() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = runtime.handle_command(HostCommand::Run {
            input: "printf x | { cat; echo err >&2; } |& cat".into(),
        });

        let stdout = get_stdout(&events);
        assert!(stdout.contains('x'));
        assert!(stdout.contains("err"));
    }

    #[test]
    fn streaming_tee_pipe_stderr_preserves_output_and_stage_status() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = runtime.handle_command(HostCommand::Run {
            input: "printf x | tee / |& cat\necho ${PIPESTATUS[*]}".into(),
        });

        let stdout = get_stdout(&events);
        assert!(stdout.contains('x'));
        assert!(stdout.contains("tee: /: is a directory: /"));
        assert!(stdout.contains("0 1 0"));
        assert_eq!(get_stderr(&events), "");
    }

    #[test]
    fn streaming_tee_pipe_stderr_respects_pipefail() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = runtime.handle_command(HostCommand::Run {
            input: "set -o pipefail\nprintf x | tee / |& cat".into(),
        });

        assert_eq!(runtime.vm.state.last_status, 1);
        let stdout = get_stdout(&events);
        assert!(stdout.contains('x'));
        assert!(stdout.contains("tee: /: is a directory: /"));
    }

    #[test]
    fn generic_pipeline_capture_does_not_count_hidden_stage_output() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        runtime.vm.limits.output_byte_limit = 2;

        let events = runtime.handle_command(HostCommand::Run {
            input: "echo -e 'a\\nb' | grep b".into(),
        });

        assert_eq!(get_stdout(&events), "b\n");
        assert!(!has_output_limit_diagnostic(&events));
    }

    #[test]
    fn generic_pipeline_file_capture_preserves_redirection_behavior() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = runtime.handle_command(HostCommand::Run {
            input: "echo -e 'a\\nb' | grep b >/filtered.txt | wc -l".into(),
        });

        assert_eq!(get_stdout(&events), "0\n");

        let file_events = runtime.handle_command(HostCommand::ReadFile {
            path: "/filtered.txt".into(),
        });
        assert_eq!(get_stdout(&file_events), "b\n");
    }

    #[test]
    fn scheduler_single_redirect_only_command_creates_target_file() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = runtime.handle_command(HostCommand::Run {
            input: "> /created.txt".into(),
        });

        assert_eq!(runtime.vm.state.last_status, 0);
        assert_eq!(get_stdout(&events), "");
        assert_eq!(get_stderr(&events), "");

        let file_events = runtime.handle_command(HostCommand::ReadFile {
            path: "/created.txt".into(),
        });
        assert_eq!(get_stdout(&file_events), "");
    }

    #[test]
    fn command_substitution_keeps_inner_stderr_visible() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = runtime.handle_command(HostCommand::Run {
            input: "echo $(printf 'hello'; echo err >&2)".into(),
        });

        assert_eq!(get_stdout(&events), "hello\n");
        assert_eq!(get_stderr(&events), "err\n");
    }

    #[test]
    fn command_substitution_isolates_shell_state() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = runtime.handle_command(HostCommand::Run {
            input: "foo=before; echo $(foo=after; printf hi); echo $foo".into(),
        });

        assert_eq!(get_stdout(&events), "hi\nbefore\n");
    }

    #[test]
    fn process_substitution_out_feeds_inner_command() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = runtime.handle_command(HostCommand::Run {
            input: "printf hi > >(cat)".into(),
        });

        assert_eq!(get_stdout(&events), "hi");
        assert_eq!(get_stderr(&events), "");
        assert_eq!(runtime.vm.state.last_status, 0);
    }

    #[test]
    fn process_substitution_out_runs_schedulable_inner_pipeline() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = runtime.handle_command(HostCommand::Run {
            input: "printf 'a\\nb\\n' > >(head -n 1 | cat)".into(),
        });

        assert_eq!(get_stdout(&events), "a\n");
        assert_eq!(get_stderr(&events), "");
        assert_eq!(runtime.vm.state.last_status, 0);
    }

    #[test]
    fn process_substitution_out_runs_live_tail_pipeline() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        runtime.proc_subst_out_scopes.push(Vec::new());
        let path = runtime.register_process_subst_out("tail -n 1 | cat");

        {
            let sink = runtime
                .process_subst_out_sink_mut(&path)
                .expect("registered process substitution sink");
            match &sink.mode {
                PendingProcessSubstOutMode::Live { .. } => {}
                PendingProcessSubstOutMode::Buffered { .. } => {
                    panic!("expected live process substitution runner")
                }
            }
            sink.write(b"a\nb\n");
        }

        let scope = runtime.proc_subst_out_scopes.pop().unwrap_or_default();
        runtime.flush_process_subst_out_scope(scope);
        assert_eq!(runtime.vm.stdout, b"b\n");
    }

    #[test]
    fn process_substitution_out_runs_live_buffered_pipeline() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        runtime.proc_subst_out_scopes.push(Vec::new());
        let path = runtime.register_process_subst_out("sort | cat");

        {
            let sink = runtime
                .process_subst_out_sink_mut(&path)
                .expect("registered process substitution sink");
            match &sink.mode {
                PendingProcessSubstOutMode::Live { runner } => {
                    assert!(runner.isolated_runtime.is_some());
                }
                PendingProcessSubstOutMode::Buffered { .. } => {
                    panic!("expected live buffered process substitution runner")
                }
            }
            sink.write(b"b\na\n");
        }

        let scope = runtime.proc_subst_out_scopes.pop().unwrap_or_default();
        runtime.flush_process_subst_out_scope(scope);
        assert_eq!(runtime.vm.stdout, b"a\nb\n");
    }

    #[test]
    fn process_substitution_in_registers_live_reader_and_cleans_up() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        runtime.proc_subst_in_scopes.push(Vec::new());
        let path = runtime
            .execute_process_subst_in("yes | head -n 2")
            .to_string();
        assert!(runtime.fs.stat(&path).is_ok());

        let file = runtime.handle_command(HostCommand::ReadFile { path: path.clone() });
        assert_eq!(get_stdout(&file), "y\ny\n");
        assert!(runtime.fs.stat(&path).is_err());

        let scope = runtime.proc_subst_in_scopes.pop().unwrap_or_default();
        runtime.flush_process_subst_in_scope(scope);
        assert!(runtime.fs.stat(&path).is_err());
    }

    #[test]
    fn process_substitution_in_registers_live_sed_reader_and_cleans_up() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        runtime.proc_subst_in_scopes.push(Vec::new());
        let path = runtime
            .execute_process_subst_in("yes | sed 's/y/z/' | head -n 2")
            .to_string();
        assert!(runtime.fs.stat(&path).is_ok());

        let file = runtime.handle_command(HostCommand::ReadFile { path: path.clone() });
        assert_eq!(get_stdout(&file), "z\nz\n");
        assert!(runtime.fs.stat(&path).is_err());

        let scope = runtime.proc_subst_in_scopes.pop().unwrap_or_default();
        runtime.flush_process_subst_in_scope(scope);
        assert!(runtime.fs.stat(&path).is_err());
    }

    #[test]
    fn process_substitution_in_runs_live_buffered_reader_and_cleans_up() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        runtime.proc_subst_in_scopes.push(Vec::new());
        let path = runtime
            .execute_process_subst_in("printf 'b\\na\\n' | sort")
            .to_string();

        assert!(runtime.fs.stat(&path).is_ok());
        let file = runtime.handle_command(HostCommand::ReadFile { path: path.clone() });
        assert_eq!(get_stdout(&file), "a\nb\n");
        assert!(runtime.fs.stat(&path).is_err());

        let scope = runtime.proc_subst_in_scopes.pop().unwrap_or_default();
        runtime.flush_process_subst_in_scope(scope);
        assert!(runtime.fs.stat(&path).is_err());
    }

    #[test]
    fn live_process_substitution_runner_consumes_before_flush() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        runtime.proc_subst_out_scopes.push(Vec::new());
        let path = runtime.register_process_subst_out("head -n 1 | cat");

        {
            let sink = runtime
                .process_subst_out_sink_mut(&path)
                .expect("registered process substitution sink");
            sink.write(b"a\nb\n");
            match &sink.mode {
                PendingProcessSubstOutMode::Live { runner } => {
                    assert_eq!(runner.captured_stdout, b"a\n");
                }
                PendingProcessSubstOutMode::Buffered { .. } => {
                    panic!("expected live process substitution runner")
                }
            }
        }

        let scope = runtime.proc_subst_out_scopes.pop().unwrap_or_default();
        runtime.flush_process_subst_out_scope(scope);
        assert_eq!(runtime.vm.stdout, b"a\n");
    }

    #[test]
    fn live_process_substitution_runner_tee_writes_before_flush() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        runtime.proc_subst_out_scopes.push(Vec::new());
        let path = runtime.register_process_subst_out("tee /tee.txt | cat");

        {
            let sink = runtime
                .process_subst_out_sink_mut(&path)
                .expect("registered process substitution sink");
            sink.write(b"a\nb\n");
            match &sink.mode {
                PendingProcessSubstOutMode::Live { runner } => {
                    assert!(runner.captured_stdout.starts_with(b"a\nb"));
                }
                PendingProcessSubstOutMode::Buffered { .. } => {
                    panic!("expected live process substitution runner")
                }
            }
        }

        let file = runtime.handle_command(HostCommand::ReadFile {
            path: "/tee.txt".into(),
        });
        assert!(get_stdout(&file).starts_with("a\nb"));

        let scope = runtime.proc_subst_out_scopes.pop().unwrap_or_default();
        runtime.flush_process_subst_out_scope(scope);
        assert_eq!(runtime.vm.stdout, b"a\nb\n");

        let file = runtime.handle_command(HostCommand::ReadFile {
            path: "/tee.txt".into(),
        });
        assert_eq!(get_stdout(&file), "a\nb\n");
    }

    #[test]
    fn exec_live_redirections_preserve_left_to_right_dup_order() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = runtime.handle_command(HostCommand::Run {
            input: "printf hi > /first.txt 1>&2\nprintf hi 1>&2 > /second.txt".into(),
        });

        assert_eq!(get_stdout(&events), "");
        assert_eq!(get_stderr(&events), "hi");

        let first = runtime.handle_command(HostCommand::ReadFile {
            path: "/first.txt".into(),
        });
        assert_eq!(get_stdout(&first), "");

        let second = runtime.handle_command(HostCommand::ReadFile {
            path: "/second.txt".into(),
        });
        assert_eq!(get_stdout(&second), "hi");
    }

    #[test]
    fn exec_process_subst_redirections_preserve_left_to_right_dup_order() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = runtime.handle_command(HostCommand::Run {
            input: "printf hi > >(cat) 1>&2\nprintf hi 1>&2 > >(cat)".into(),
        });

        assert_eq!(get_stdout(&events), "hi");
        assert_eq!(get_stderr(&events), "hi");
    }

    #[test]
    fn builtin_and_utility_redirections_write_files_during_execution() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = runtime.handle_command(HostCommand::Run {
            input: "type printf > /builtin.txt\nprintf hi > /utility.txt".into(),
        });

        let status = events
            .iter()
            .find_map(|event| {
                if let WorkerEvent::Exit(code) = event {
                    Some(*code)
                } else {
                    None
                }
            })
            .unwrap_or(-1);
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "");
        assert_eq!(get_stderr(&events), "");

        let builtin = runtime.handle_command(HostCommand::ReadFile {
            path: "/builtin.txt".into(),
        });
        assert!(get_stdout(&builtin).contains("printf"));

        let utility = runtime.handle_command(HostCommand::ReadFile {
            path: "/utility.txt".into(),
        });
        assert_eq!(get_stdout(&utility), "hi");
    }

    #[test]
    fn special_param_underscore_uses_previous_command_last_argument() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let first = runtime.handle_command(HostCommand::Run {
            input: "echo alpha beta".into(),
        });
        assert_eq!(get_stdout(&first), "alpha beta\n");
        assert_eq!(runtime.vm.state.get_var("_").as_deref(), Some("beta"));

        let events = runtime.handle_command(HostCommand::Run {
            input: "echo \"last=$_\"".into(),
        });

        assert_eq!(get_stdout(&events), "last=beta\n");
        assert_eq!(runtime.vm.state.get_var("_").as_deref(), Some("last=beta"));
    }

    #[test]
    fn amp_append_redirection_appends_stdout_and_stderr_for_simple_command() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let setup = runtime.handle_command(HostCommand::WriteFile {
            path: "/log.txt".into(),
            data: b"old\n".to_vec(),
        });
        assert_eq!(get_stderr(&setup), "");

        let events = runtime.handle_command(HostCommand::Run {
            input: "f(){ printf 'out\\n'; printf 'err\\n' >&2; }\nf &>> /log.txt\ncat /log.txt"
                .into(),
        });

        assert_eq!(get_stdout(&events), "old\nout\nerr\n");
        assert_eq!(get_stderr(&events), "");
    }

    #[test]
    fn clobber_redirection_overrides_noclobber() {
        let mut runtime = WorkerRuntime::new();
        runtime.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let setup = runtime.handle_command(HostCommand::WriteFile {
            path: "/existing.txt".into(),
            data: b"old\n".to_vec(),
        });
        assert_eq!(get_stderr(&setup), "");

        let events = runtime.handle_command(HostCommand::Run {
            input: "set -o noclobber\necho blocked > /existing.txt\ncat /existing.txt\necho force >| /existing.txt\ncat /existing.txt".into(),
        });

        assert_eq!(get_stdout(&events), "old\nforce\n");
        assert!(get_stderr(&events).contains("cannot overwrite existing file"));
    }
}
