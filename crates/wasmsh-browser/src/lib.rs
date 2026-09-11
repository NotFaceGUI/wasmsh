//! Browser Web Worker integration for wasmsh.
//!
//! Thin adapter around [`wasmsh_runtime::WorkerRuntime`] that adds
//! `wasm-bindgen` entry points for the browser worker.

// Re-export the runtime so downstream consumers (testkit, benches) work unchanged.
pub use wasmsh_runtime::{extglob_match, BrowserConfig, WorkerRuntime};

// Protocol types used in tests (via `use super::*`) and wasm_bindings.
#[cfg(test)]
use wasmsh_protocol::{DiagnosticLevel, HostCommand, WorkerEvent, PROTOCOL_VERSION};

#[cfg(test)]
mod tests {
    use super::*;

    fn run_shell(input: &str) -> (Vec<WorkerEvent>, i32) {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        let events = rt.handle_command(HostCommand::Run {
            input: input.into(),
        });
        let status = events
            .iter()
            .find_map(|e| {
                if let WorkerEvent::Exit(s) = e {
                    Some(*s)
                } else {
                    None
                }
            })
            .unwrap_or(-1);
        (events, status)
    }

    fn get_stdout(events: &[WorkerEvent]) -> String {
        let mut out = Vec::new();
        for e in events {
            if let WorkerEvent::Stdout(data) = e {
                out.extend_from_slice(data);
            }
        }
        String::from_utf8(out).unwrap_or_default()
    }

    fn get_stderr(events: &[WorkerEvent]) -> String {
        let mut out = Vec::new();
        for e in events {
            if let WorkerEvent::Stderr(data) = e {
                out.extend_from_slice(data);
            }
        }
        String::from_utf8(out).unwrap_or_default()
    }

    #[test]
    fn init_returns_version() {
        let mut rt = WorkerRuntime::new();
        let events = rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        assert!(matches!(&events[0], WorkerEvent::Version(v) if v == PROTOCOL_VERSION));
    }

    #[test]
    fn run_before_init_errors() {
        let mut rt = WorkerRuntime::new();
        let events = rt.handle_command(HostCommand::Run {
            input: "echo hi".into(),
        });
        assert!(matches!(
            &events[0],
            WorkerEvent::Diagnostic(DiagnosticLevel::Error, _)
        ));
    }

    #[test]
    fn echo_hello() {
        let (events, status) = run_shell("echo hello");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hello\n");
    }

    #[test]
    fn true_false() {
        let (_, status) = run_shell("true");
        assert_eq!(status, 0);
        let (_, status) = run_shell("false");
        assert_eq!(status, 1);
    }

    #[test]
    fn variable_assignment_and_echo() {
        let (events, status) = run_shell("X=hello; echo $X");
        assert_eq!(status, 0);
        // Note: variable expansion happens through the word parser + expand
        // The parser produces WordPart::Parameter("X"), expand resolves it
        assert_eq!(get_stdout(&events), "hello\n");
    }

    #[test]
    fn and_or_chain() {
        let (events, _) = run_shell("true && echo yes");
        assert_eq!(get_stdout(&events), "yes\n");

        let (events, _) = run_shell("false && echo no");
        assert_eq!(get_stdout(&events), "");

        let (events, _) = run_shell("false || echo fallback");
        assert_eq!(get_stdout(&events), "fallback\n");
    }

    #[test]
    fn if_then_fi() {
        let (events, status) = run_shell("if true; then echo yes; fi");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "yes\n");
    }

    #[test]
    fn if_else() {
        let (events, _) = run_shell("if false; then echo no; else echo yes; fi");
        assert_eq!(get_stdout(&events), "yes\n");
    }

    #[test]
    fn for_loop() {
        let (events, _) = run_shell("for x in a b c; do echo $x; done");
        assert_eq!(get_stdout(&events), "a\nb\nc\n");
    }

    #[test]
    fn parse_error_reported() {
        let (events, status) = run_shell("|");
        assert_eq!(status, 2);
        assert!(events.iter().any(|e| matches!(e, WorkerEvent::Stderr(_))));
    }

    #[test]
    fn negated_pipeline() {
        let (_, status) = run_shell("! true");
        assert_eq!(status, 1);
        let (_, status) = run_shell("! false");
        assert_eq!(status, 0);
    }

    #[test]
    fn cancel_command() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        let events = rt.handle_command(HostCommand::Cancel);
        assert!(matches!(
            &events[0],
            WorkerEvent::Diagnostic(DiagnosticLevel::Info, _)
        ));
    }

    // ---- Utility dispatch ----

    #[test]
    fn touch_and_cat_via_shell() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        // touch creates a file, then we write via protocol and cat it
        rt.handle_command(HostCommand::Run {
            input: "touch /hello.txt".into(),
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/hello.txt".into(),
            data: b"hello world".to_vec(),
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "cat /hello.txt".into(),
        });
        assert_eq!(get_stdout(&events), "hello world");
    }

    #[test]
    fn mkdir_and_ls_via_shell() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "mkdir /mydir".into(),
        });
        rt.handle_command(HostCommand::Run {
            input: "touch /mydir/a.txt".into(),
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "ls /mydir".into(),
        });
        assert_eq!(get_stdout(&events), "a.txt\n");
    }

    #[test]
    fn unknown_command_reports_error() {
        let (events, status) = run_shell("nonexistent_cmd");
        assert_eq!(status, 127);
        // Check stderr contains "command not found"
        let stderr: String = events
            .iter()
            .filter_map(|e| {
                if let WorkerEvent::Stderr(data) = e {
                    Some(String::from_utf8_lossy(data).to_string())
                } else {
                    None
                }
            })
            .collect();
        assert!(stderr.contains("command not found"));
    }

    // ---- Protocol file operations ----

    #[test]
    fn protocol_write_and_read_file() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        let write_events = rt.handle_command(HostCommand::WriteFile {
            path: "/test.txt".into(),
            data: b"content".to_vec(),
        });
        assert!(write_events
            .iter()
            .any(|e| matches!(e, WorkerEvent::FsChanged(_))));

        let read_events = rt.handle_command(HostCommand::ReadFile {
            path: "/test.txt".into(),
        });
        assert_eq!(read_events, vec![WorkerEvent::Stdout(b"content".to_vec())]);
    }

    #[test]
    fn protocol_list_dir() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/a.txt".into(),
            data: vec![],
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/b.txt".into(),
            data: vec![],
        });
        let events = rt.handle_command(HostCommand::ListDir { path: "/".into() });
        let stdout = get_stdout(&events);
        assert!(stdout.contains("a.txt"));
        assert!(stdout.contains("b.txt"));
    }

    // ---- Redirections ----

    #[test]
    fn output_redirection_to_file() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        // echo hello > /out.txt should write to file, not stdout
        let events = rt.handle_command(HostCommand::Run {
            input: "echo hello > /out.txt".into(),
        });
        // stdout should be empty (redirected to file)
        assert_eq!(get_stdout(&events), "");
        // File should contain the output
        let read_events = rt.handle_command(HostCommand::ReadFile {
            path: "/out.txt".into(),
        });
        assert_eq!(get_stdout(&read_events), "hello\n");
    }

    #[test]
    fn append_redirection() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "echo line1 > /log.txt".into(),
        });
        rt.handle_command(HostCommand::Run {
            input: "echo line2 >> /log.txt".into(),
        });
        let read_events = rt.handle_command(HostCommand::ReadFile {
            path: "/log.txt".into(),
        });
        assert_eq!(get_stdout(&read_events), "line1\nline2\n");
    }

    #[test]
    fn redirect_only_creates_file() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "> /empty.txt".into(),
        });
        let read_events = rt.handle_command(HostCommand::ReadFile {
            path: "/empty.txt".into(),
        });
        assert_eq!(get_stdout(&read_events), "");
    }

    // ---- Diagnostics surfaced as events ----

    #[test]
    fn vm_diagnostics_surfaced() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        // Running an unknown command triggers a diagnostic in the VM
        let events = rt.handle_command(HostCommand::Run {
            input: "unknown_cmd_xyz".into(),
        });
        // The "command not found" goes to stderr, not diagnostics,
        // but the VM emits a diagnostic when CallBuiltin fails for unknown builtins.
        // Since we dispatch unknown commands before IR, it goes to stderr.
        // Let's test that stderr events are present.
        assert!(events.iter().any(|e| matches!(e, WorkerEvent::Stderr(_))));
    }

    // ---- Integration: unset + default expansion ----

    #[test]
    fn unset_then_default_expansion() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "X=hello".into(),
        });
        rt.handle_command(HostCommand::Run {
            input: "unset X".into(),
        });
        // After unset, ${X:-default} should use the default
        let events = rt.handle_command(HostCommand::Run {
            input: "echo ${X:-default}".into(),
        });
        assert_eq!(get_stdout(&events), "default\n");
    }

    #[test]
    fn readonly_prevents_reassignment() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "readonly X=locked".into(),
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "echo $X".into(),
        });
        assert_eq!(get_stdout(&events), "locked\n");
    }

    #[test]
    fn pipeline_last_status() {
        // Pipeline exit status should be the last command's status
        let (_, status) = run_shell("true | false");
        assert_eq!(status, 1);
        let (_, status) = run_shell("false | true");
        assert_eq!(status, 0);
    }

    #[test]
    fn pipe_data_flows_through() {
        let (events, status) = run_shell("echo hello | cat");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hello\n");
    }

    #[test]
    fn command_substitution_captures_stdout_without_leak() {
        let (events, status) = run_shell("echo $(printf 'hello')");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hello\n");
    }

    #[test]
    fn command_substitution_preserves_inner_stderr_visibility() {
        let (events, status) = run_shell("echo $(printf 'hello'; echo err >&2)");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hello\n");
        assert_eq!(get_stderr(&events), "err\n");
    }

    #[test]
    fn command_substitution_isolates_shell_state() {
        let (events, status) = run_shell("foo=before; echo $(foo=after; printf hi); echo $foo");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hi\nbefore\n");
    }

    #[test]
    fn scheduler_executes_single_redirect_only_command() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "> /created.txt".into(),
        });
        let status = events
            .iter()
            .find_map(|e| {
                if let WorkerEvent::Exit(s) = e {
                    Some(*s)
                } else {
                    None
                }
            })
            .unwrap_or(-1);
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "");
        assert_eq!(get_stderr(&events), "");
        let read_events = rt.handle_command(HostCommand::ReadFile {
            path: "/created.txt".into(),
        });
        assert_eq!(get_stdout(&read_events), "");
    }

    #[test]
    fn pipe_three_stages() {
        let (events, status) = run_shell("echo hello world | cat | cat");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hello world\n");
    }

    #[test]
    fn pipe_echo_to_wc() {
        let (events, status) = run_shell("echo hello world | wc");
        assert_eq!(status, 0);
        let stdout = get_stdout(&events);
        assert!(stdout.contains('1')); // 1 line
        assert!(stdout.contains('2')); // 2 words
    }

    #[test]
    fn streaming_yes_head_stops_after_requested_lines() {
        let (events, status) = run_shell("yes | head -n 5");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "y\ny\ny\ny\ny\n");
    }

    #[test]
    fn streaming_yes_cat_head_stops_after_requested_lines() {
        let (events, status) = run_shell("yes | cat | head -n 5");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "y\ny\ny\ny\ny\n");
    }

    #[test]
    fn streaming_yes_head_wc_counts_lines() {
        let (events, status) = run_shell("yes | head -n 5 | wc -l");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "5\n");
    }

    #[test]
    fn streaming_cat_file_head_stops_at_requested_bytes() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/big.txt".into(),
            data: b"abcdefghijklmnopqrstuvwxyz".to_vec(),
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "cat /big.txt | head -c 10".into(),
        });
        let status = events
            .iter()
            .find_map(|e| {
                if let WorkerEvent::Exit(s) = e {
                    Some(*s)
                } else {
                    None
                }
            })
            .unwrap_or(-1);
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "abcdefghij");
    }

    #[test]
    fn streaming_yes_tr_head_transforms_lines() {
        let (events, status) = run_shell("yes | tr y z | head -n 5");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "z\nz\nz\nz\nz\n");
    }

    #[test]
    fn streaming_yes_grep_head_stops_after_requested_lines() {
        let (events, status) = run_shell("yes | grep y | head -n 5");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "y\ny\ny\ny\ny\n");
    }

    #[test]
    fn streaming_yes_tee_head_writes_only_pulled_output() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "yes | tee /tee.txt | head -n 5".into(),
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
        assert_eq!(get_stdout(&events), "y\ny\ny\ny\ny\n");

        let file_events = rt.handle_command(HostCommand::ReadFile {
            path: "/tee.txt".into(),
        });
        assert_eq!(get_stdout(&file_events), "y\ny\ny\ny\ny\n");
    }

    #[test]
    fn streaming_buffered_sort_tee_cat_preserves_sorted_output() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "printf 'b\\na\\n' | sort | tee /sorted.txt | cat".into(),
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
        assert_eq!(get_stdout(&events), "a\nb\n");

        let file_events = rt.handle_command(HostCommand::ReadFile {
            path: "/sorted.txt".into(),
        });
        assert_eq!(get_stdout(&file_events), "a\nb\n");
    }

    #[test]
    fn streaming_yes_rev_head_stops_after_requested_lines() {
        let (events, status) = run_shell("yes | rev | head -n 5");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "y\ny\ny\ny\ny\n");
    }

    #[test]
    fn streaming_echo_cut_selects_field() {
        let (events, status) = run_shell("echo abc:def | cut -d: -f2 | head -c 4");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "def\n");
    }

    #[test]
    fn streaming_echo_tail_head_selects_last_lines() {
        let (events, status) = run_shell("echo -e 'a\\nb\\nc' | tail -n 2 | head -n 1");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "b\n");
    }

    #[test]
    fn streaming_buffered_printf_sort_head_outputs_sorted_first_line() {
        let (events, status) = run_shell("printf 'b\\na\\n' | sort | head -n 1");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "a\n");
    }

    #[test]
    fn streaming_buffered_function_stage_preserves_output() {
        let (events, status) = run_shell("f(){ cat; }\nprintf hi | f | head -c 2");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hi");
    }

    #[test]
    fn streaming_buffered_function_pipe_stderr_preserves_output() {
        let (events, status) = run_shell("f(){ echo out; echo err >&2; }\nf |& head -n 2");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "out\nerr\n");
    }

    #[test]
    fn scheduled_group_stage_pipe_stderr_preserves_output() {
        let (events, status) = run_shell("printf x | { cat; echo err >&2; } |& cat");
        assert_eq!(status, 0);
        let stdout = get_stdout(&events);
        assert!(stdout.contains('x'));
        assert!(stdout.contains("err"));
    }

    #[test]
    fn streaming_tee_pipe_stderr_preserves_output() {
        let (events, status) = run_shell("printf x | tee / |& cat");
        assert_eq!(status, 0);
        let stdout = get_stdout(&events);
        assert!(stdout.contains('x'));
        assert!(stdout.contains("tee: /: is a directory: /"));
        assert_eq!(get_stderr(&events), "");
    }

    #[test]
    fn streaming_tee_pipe_stderr_respects_pipefail_status() {
        let (events, status) = run_shell("set -o pipefail\nprintf x | tee / |& cat");
        assert_eq!(status, 1);
        let stdout = get_stdout(&events);
        assert!(stdout.contains('x'));
        assert!(stdout.contains("tee: /: is a directory: /"));
        assert_eq!(get_stderr(&events), "");
    }

    #[test]
    fn streaming_yes_bat_head_formats_numbered_lines() {
        let (events, status) = run_shell("yes | bat --style=numbers | head -n 2");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "    1   │ y\n    2   │ y\n");
    }

    #[test]
    fn streaming_yes_sed_head_rewrites_lines() {
        let (events, status) = run_shell("yes | sed 's/y/z/' | head -n 5");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "z\nz\nz\nz\nz\n");
    }

    #[test]
    fn streaming_echo_paste_serial_joins_lines() {
        let (events, status) = run_shell("echo -e 'a\\nb\\nc' | paste -s -d , | head -c 6");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "a,b,c\n");
    }

    #[test]
    fn streaming_echo_column_preserves_plain_output() {
        let (events, status) = run_shell("echo abc | column | head -c 4");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "abc\n");
    }

    #[test]
    fn streaming_echo_uniq_deduplicates_lines() {
        let (events, status) = run_shell("echo -e 'a\\na\\nb' | uniq | head -n 2");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "a\nb\n");
    }

    #[test]
    fn generic_pipeline_grep_preserves_visible_output_budget_behavior() {
        let (events, status) = run_shell("echo -e 'a\\nb' | grep b");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "b\n");
    }

    #[test]
    fn while_loop_with_counter() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 10000,
            allowed_hosts: vec![],
            network_policy: None,
        });
        // Simple loop that echoes 3 times using a counter variable
        let events = rt.handle_command(HostCommand::Run {
            input: "for i in 1 2 3; do echo line; done".into(),
        });
        assert_eq!(get_stdout(&events), "line\nline\nline\n");
    }

    #[test]
    fn heredoc_with_cat() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "cat <<EOF\nhello world\nEOF\n".into(),
        });
        assert_eq!(get_stdout(&events), "hello world\n");
    }

    #[test]
    fn string_length_expansion() {
        let (events, status) = run_shell("X=hello; echo ${#X}");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "5\n");
    }

    // ---- Functions ----

    #[test]
    fn function_define_and_call() {
        let (events, status) = run_shell("greet() { echo hello; }; greet");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hello\n");
    }

    #[test]
    fn function_with_args() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "greet() { echo hello $1; }".into(),
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "greet world".into(),
        });
        assert_eq!(get_stdout(&events), "hello world\n");
    }

    #[test]
    fn function_modifies_parent_scope() {
        // Bash behavior: functions share parent scope (no isolation by default)
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "X=outer".into(),
        });
        rt.handle_command(HostCommand::Run {
            input: "f() { X=inner; }".into(),
        });
        rt.handle_command(HostCommand::Run { input: "f".into() });
        let events = rt.handle_command(HostCommand::Run {
            input: "echo $X".into(),
        });
        assert_eq!(get_stdout(&events), "inner\n");
    }

    #[test]
    fn local_isolates_in_function() {
        // `local` creates a variable that is restored after function returns
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "X=outer".into(),
        });
        rt.handle_command(HostCommand::Run {
            input: "f() { local X=inner; echo $X; }".into(),
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "f; echo $X".into(),
        });
        assert_eq!(get_stdout(&events), "inner\nouter\n");
    }

    // ---- Case ----

    #[test]
    fn case_basic() {
        let source = "case hello in\nhello) echo matched;;\nworld) echo no;;\nesac";
        let (events, status) = run_shell(source);
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "matched\n");
    }

    #[test]
    fn case_wildcard() {
        let source = "case anything in\n*) echo default;;\nesac";
        let (events, _) = run_shell(source);
        assert_eq!(get_stdout(&events), "default\n");
    }

    #[test]
    fn case_no_match() {
        let source = "case hello in\nworld) echo no;;\nesac";
        let (events, _) = run_shell(source);
        assert_eq!(get_stdout(&events), "");
    }

    // ---- Subshell scope isolation ----

    #[test]
    fn subshell_scope_isolation() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "X=outer".into(),
        });
        rt.handle_command(HostCommand::Run {
            input: "(X=inner)".into(),
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "echo $X".into(),
        });
        assert_eq!(get_stdout(&events), "outer\n");
    }

    // ---- Assign-default expansion ----

    #[test]
    fn assign_default_expansion() {
        let (events, _) = run_shell("echo ${X:=fallback}; echo $X");
        assert_eq!(get_stdout(&events), "fallback\nfallback\n");
    }

    // ---- Glob expansion ----

    #[test]
    fn glob_star_matches_files() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "touch /a.txt".into(),
        });
        rt.handle_command(HostCommand::Run {
            input: "touch /b.txt".into(),
        });
        rt.handle_command(HostCommand::Run {
            input: "touch /c.log".into(),
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "echo /*.txt".into(),
        });
        let stdout = get_stdout(&events);
        assert!(stdout.contains("/a.txt"));
        assert!(stdout.contains("/b.txt"));
        assert!(!stdout.contains("c.log"));
    }

    #[test]
    fn glob_no_match_keeps_literal() {
        let (events, _) = run_shell("echo /no_such_*.xyz");
        assert_eq!(get_stdout(&events), "/no_such_*.xyz\n");
    }

    #[test]
    fn glob_question_mark() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "touch /ab".into(),
        });
        rt.handle_command(HostCommand::Run {
            input: "touch /ac".into(),
        });
        rt.handle_command(HostCommand::Run {
            input: "touch /abc".into(),
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "echo /a?".into(),
        });
        let stdout = get_stdout(&events);
        assert!(stdout.contains("/ab"));
        assert!(stdout.contains("/ac"));
        assert!(!stdout.contains("/abc"));
    }

    // ---- Brace expansion ----

    #[test]
    fn brace_comma_expansion() {
        let (events, _) = run_shell("echo {a,b,c}");
        assert_eq!(get_stdout(&events), "a b c\n");
    }

    #[test]
    fn brace_range_expansion() {
        let (events, _) = run_shell("echo {1..5}");
        assert_eq!(get_stdout(&events), "1 2 3 4 5\n");
    }

    #[test]
    fn brace_prefix_suffix() {
        let (events, _) = run_shell("echo file{1,2,3}.txt");
        assert_eq!(get_stdout(&events), "file1.txt file2.txt file3.txt\n");
    }

    // ---- Here-string ----

    #[test]
    fn here_string_basic() {
        let (events, status) = run_shell("cat <<< hello");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hello\n");
    }

    #[test]
    fn here_string_with_variable() {
        let (events, status) = run_shell("X=world; cat <<< $X");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "world\n");
    }

    // ---- ANSI-C quoting ----

    #[test]
    fn ansi_c_quoting_newline() {
        let (events, status) = run_shell("echo $'hello\\nworld'");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hello\nworld\n");
    }

    #[test]
    fn ansi_c_quoting_tab() {
        let (events, status) = run_shell("echo $'a\\tb'");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "a\tb\n");
    }

    #[test]
    fn ansi_c_quoting_hex() {
        let (events, status) = run_shell("echo $'\\x41'");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "A\n");
    }

    // ---- Stderr redirection ----

    #[test]
    fn stderr_redirect_to_file() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        // Running a command that doesn't exist produces stderr
        let _events = rt.handle_command(HostCommand::Run {
            input: "nonexistent_cmd 2> /err.txt".into(),
        });
        // stderr should have been captured to file
        let read_events = rt.handle_command(HostCommand::ReadFile {
            path: "/err.txt".into(),
        });
        let err_content = get_stdout(&read_events);
        assert!(err_content.contains("command not found"));
    }

    #[test]
    fn stderr_merge_into_stdout() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        // Redirections are applied left-to-right: stderr duplicates the original
        // stdout, then stdout is redirected to the file. The error stays visible.
        let events = rt.handle_command(HostCommand::Run {
            input: "nonexistent_cmd 2>&1 > /out.txt".into(),
        });
        let read_events = rt.handle_command(HostCommand::ReadFile {
            path: "/out.txt".into(),
        });
        let content = get_stdout(&read_events);
        assert_eq!(content, "");
        assert!(get_stdout(&events).contains("command not found"));
        assert_eq!(get_stderr(&events), "");
    }

    #[test]
    fn amp_greater_both_to_file() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        let _events = rt.handle_command(HostCommand::Run {
            input: "nonexistent_cmd &> /all.txt".into(),
        });
        let read_events = rt.handle_command(HostCommand::ReadFile {
            path: "/all.txt".into(),
        });
        let content = get_stdout(&read_events);
        assert!(content.contains("command not found"));
    }

    // ---- [[ ]] extended test ----

    #[test]
    fn dbl_bracket_string_equality() {
        let (_, status) = run_shell("[[ hello == hello ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("[[ hello == world ]]");
        assert_eq!(status, 1);
    }

    #[test]
    fn dbl_bracket_string_inequality() {
        let (_, status) = run_shell("[[ hello != world ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("[[ hello != hello ]]");
        assert_eq!(status, 1);
    }

    #[test]
    fn dbl_bracket_glob_match() {
        let (_, status) = run_shell("[[ hello == hel* ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("[[ hello == wor* ]]");
        assert_eq!(status, 1);
    }

    #[test]
    fn dbl_bracket_string_ordering() {
        let (_, status) = run_shell("[[ abc < def ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("[[ def < abc ]]");
        assert_eq!(status, 1);
        let (_, status) = run_shell("[[ def > abc ]]");
        assert_eq!(status, 0);
    }

    #[test]
    fn dbl_bracket_integer_comparison() {
        let (_, status) = run_shell("[[ 5 -eq 5 ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("[[ 5 -ne 3 ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("[[ 3 -lt 5 ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("[[ 5 -le 5 ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("[[ 7 -gt 3 ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("[[ 5 -ge 5 ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("[[ 5 -lt 3 ]]");
        assert_eq!(status, 1);
    }

    #[test]
    fn dbl_bracket_string_tests() {
        let (_, status) = run_shell("[[ -z \"\" ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("[[ -z hello ]]");
        assert_eq!(status, 1);
        let (_, status) = run_shell("[[ -n hello ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("[[ -n \"\" ]]");
        assert_eq!(status, 1);
    }

    #[test]
    fn dbl_bracket_logical_and() {
        let (_, status) = run_shell("[[ hello == hello && world == world ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("[[ hello == hello && world == nope ]]");
        assert_eq!(status, 1);
    }

    #[test]
    fn dbl_bracket_logical_or() {
        let (_, status) = run_shell("[[ hello == nope || world == world ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("[[ hello == nope || world == nope ]]");
        assert_eq!(status, 1);
    }

    #[test]
    fn dbl_bracket_logical_not() {
        let (_, status) = run_shell("[[ ! hello == world ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("[[ ! hello == hello ]]");
        assert_eq!(status, 1);
    }

    #[test]
    fn dbl_bracket_variable_expansion() {
        let (_, status) = run_shell("X=hello; [[ $X == hello ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("X=hello; [[ $X == world ]]");
        assert_eq!(status, 1);
    }

    #[test]
    fn dbl_bracket_no_word_splitting() {
        // In [[ ]], variables with spaces should NOT be word-split
        let (_, status) = run_shell("X=\"hello world\"; [[ $X == \"hello world\" ]]");
        assert_eq!(status, 0);
    }

    #[test]
    fn dbl_bracket_file_tests() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        // Create a file
        rt.handle_command(HostCommand::Run {
            input: "touch /testfile".into(),
        });
        // -e: file exists
        let events = rt.handle_command(HostCommand::Run {
            input: "[[ -e /testfile ]]".into(),
        });
        let status = events
            .iter()
            .find_map(|e| {
                if let WorkerEvent::Exit(s) = e {
                    Some(*s)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(status, 0);

        // -f: is a regular file
        let events = rt.handle_command(HostCommand::Run {
            input: "[[ -f /testfile ]]".into(),
        });
        let status = events
            .iter()
            .find_map(|e| {
                if let WorkerEvent::Exit(s) = e {
                    Some(*s)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(status, 0);

        // -d: is a directory (should fail for a file)
        let events = rt.handle_command(HostCommand::Run {
            input: "[[ -d /testfile ]]".into(),
        });
        let status = events
            .iter()
            .find_map(|e| {
                if let WorkerEvent::Exit(s) = e {
                    Some(*s)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(status, 1);

        // -e: non-existent file
        let events = rt.handle_command(HostCommand::Run {
            input: "[[ -e /nonexistent ]]".into(),
        });
        let status = events
            .iter()
            .find_map(|e| {
                if let WorkerEvent::Exit(s) = e {
                    Some(*s)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(status, 1);
    }

    #[test]
    fn dbl_bracket_dir_test() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "mkdir /testdir".into(),
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "[[ -d /testdir ]]".into(),
        });
        let status = events
            .iter()
            .find_map(|e| {
                if let WorkerEvent::Exit(s) = e {
                    Some(*s)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(status, 0);
    }

    #[test]
    fn dbl_bracket_regex_match() {
        let (_, status) = run_shell("[[ hello =~ ^hel ]]");
        assert_eq!(status, 0);
        let (_, status) = run_shell("[[ hello =~ world ]]");
        assert_eq!(status, 1);
        let (_, status) = run_shell("[[ hello =~ ^hello$ ]]");
        assert_eq!(status, 0);
    }

    #[test]
    fn dbl_bracket_in_if() {
        let (events, status) = run_shell("if [[ 1 -eq 1 ]]; then echo yes; fi");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "yes\n");
    }

    #[test]
    fn dbl_bracket_in_and_or() {
        let (events, _) = run_shell("[[ hello == hello ]] && echo matched");
        assert_eq!(get_stdout(&events), "matched\n");
        let (events, _) = run_shell("[[ hello == nope ]] || echo fallback");
        assert_eq!(get_stdout(&events), "fallback\n");
    }

    #[test]
    fn dbl_bracket_grouping() {
        let (_, status) = run_shell("[[ ( hello == hello ) ]]");
        assert_eq!(status, 0);
        // Grouping with || inside ()
        let (_, status) = run_shell("[[ ( a == b || a == a ) && x == x ]]");
        assert_eq!(status, 0);
    }

    #[test]
    fn dbl_bracket_single_string() {
        // Non-empty string is true
        let (_, status) = run_shell("[[ hello ]]");
        assert_eq!(status, 0);
        // Empty string is false
        let (_, status) = run_shell("[[ \"\" ]]");
        assert_eq!(status, 1);
    }

    // ---- (( )) arithmetic command ----

    #[test]
    fn arith_command_nonzero_is_success() {
        // (( 1 )) → non-zero result → exit 0
        let (_, status) = run_shell("(( 1 ))");
        assert_eq!(status, 0);
    }

    #[test]
    fn arith_command_zero_is_failure() {
        // (( 0 )) → zero result → exit 1
        let (_, status) = run_shell("(( 0 ))");
        assert_eq!(status, 1);
    }

    #[test]
    fn arith_command_expression() {
        let (_, status) = run_shell("(( 2 + 3 ))");
        assert_eq!(status, 0); // result 5 → non-zero → success
    }

    #[test]
    fn arith_command_assignment() {
        let (events, _) = run_shell("(( x = 42 )); echo $x");
        assert_eq!(get_stdout(&events), "42\n");
    }

    #[test]
    fn arith_command_in_if() {
        let (events, _) = run_shell("if (( 1 + 1 )); then echo yes; fi");
        assert_eq!(get_stdout(&events), "yes\n");
    }

    #[test]
    fn arith_command_in_and_or() {
        let (events, _) = run_shell("(( 1 )) && echo ok");
        assert_eq!(get_stdout(&events), "ok\n");
        let (events, _) = run_shell("(( 0 )) || echo fallback");
        assert_eq!(get_stdout(&events), "fallback\n");
    }

    #[test]
    fn arith_command_increment() {
        let (events, _) = run_shell("x=5; (( x++ )); echo $x");
        assert_eq!(get_stdout(&events), "6\n");
    }

    // ---- C-style for (( )) loop ----

    #[test]
    fn arith_for_basic() {
        let (events, status) = run_shell("for ((i=0; i<5; i++)) do echo $i; done");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "0\n1\n2\n3\n4\n");
    }

    #[test]
    fn arith_for_with_spaces() {
        let (events, _) = run_shell("for (( i = 0; i < 3; i++ )) do echo $i; done");
        assert_eq!(get_stdout(&events), "0\n1\n2\n");
    }

    #[test]
    fn arith_for_sum() {
        let (events, _) =
            run_shell("sum=0; for ((i=1; i<=10; i++)) do (( sum += i )); done; echo $sum");
        assert_eq!(get_stdout(&events), "55\n");
    }

    #[test]
    fn arith_for_break() {
        let (events, _) =
            run_shell("for ((i=0; i<100; i++)) do if (( i == 3 )); then break; fi; echo $i; done");
        assert_eq!(get_stdout(&events), "0\n1\n2\n");
    }

    #[test]
    fn arith_for_continue() {
        let (events, _) =
            run_shell("for ((i=0; i<5; i++)) do if (( i == 2 )); then continue; fi; echo $i; done");
        assert_eq!(get_stdout(&events), "0\n1\n3\n4\n");
    }

    // ---- let builtin ----

    #[test]
    fn let_basic_assignment() {
        let (events, _) = run_shell("let x=5; echo $x");
        assert_eq!(get_stdout(&events), "5\n");
    }

    #[test]
    fn let_arithmetic() {
        let (events, _) = run_shell("let x=2+3; echo $x");
        assert_eq!(get_stdout(&events), "5\n");
    }

    #[test]
    fn let_returns_zero_for_nonzero() {
        // let returns 0 when last expression is non-zero
        let (_, status) = run_shell("let 1+1");
        assert_eq!(status, 0);
    }

    #[test]
    fn let_returns_one_for_zero() {
        // let returns 1 when last expression is zero
        let (_, status) = run_shell("let 0");
        assert_eq!(status, 1);
    }

    #[test]
    fn let_multiple_expressions() {
        let (events, status) = run_shell("let a=1 b=2 c=a+b; echo $c");
        assert_eq!(status, 0); // last expr (a+b=3) is non-zero → 0
        assert_eq!(get_stdout(&events), "3\n");
    }

    #[test]
    fn let_no_args_fails() {
        let (_, status) = run_shell("let");
        assert_eq!(status, 1);
    }

    // ---- declare/typeset ----

    #[test]
    fn declare_basic_variable() {
        let (events, _) = run_shell("declare x=hello; echo $x");
        assert_eq!(get_stdout(&events), "hello\n");
    }

    #[test]
    fn declare_integer_flag() {
        let (events, _) = run_shell("declare -i x=2+3; echo $x");
        assert_eq!(get_stdout(&events), "5\n");
    }

    #[test]
    fn declare_export_flag() {
        let (events, _) = run_shell("declare -x MYVAR=exported; echo $MYVAR");
        assert_eq!(get_stdout(&events), "exported\n");
    }

    #[test]
    fn declare_readonly_flag() {
        // After declare -r, re-assignment should be silently ignored
        let (events, _) = run_shell("declare -r X=locked; X=new; echo $X");
        assert_eq!(get_stdout(&events), "locked\n");
    }

    #[test]
    fn declare_lowercase_flag() {
        let (events, _) = run_shell("declare -l x=HELLO; echo $x");
        assert_eq!(get_stdout(&events), "hello\n");
    }

    #[test]
    fn declare_uppercase_flag() {
        let (events, _) = run_shell("declare -u x=hello; echo $x");
        assert_eq!(get_stdout(&events), "HELLO\n");
    }

    #[test]
    fn declare_indexed_array() {
        let (events, _) = run_shell("declare -a arr; arr[0]=x; arr[1]=y; echo ${arr[0]} ${arr[1]}");
        assert_eq!(get_stdout(&events), "x y\n");
    }

    #[test]
    fn declare_assoc_array() {
        let (events, _) = run_shell("declare -A map; map[key]=val; echo ${map[key]}");
        assert_eq!(get_stdout(&events), "val\n");
    }

    #[test]
    fn typeset_is_alias_for_declare() {
        let (events, _) = run_shell("typeset -i x=3+4; echo $x");
        assert_eq!(get_stdout(&events), "7\n");
    }

    #[test]
    fn declare_print_specific_var() {
        let (events, _) = run_shell("x=hello; declare -p x");
        let out = get_stdout(&events);
        assert!(out.contains("x="));
        assert!(out.contains("hello"));
    }

    // ---- set -o / shell option enforcement tests ----

    #[test]
    fn set_o_pipefail_enable_disable() {
        // set -o pipefail stores SHOPT_o_pipefail=1
        let (events, status) = run_shell("set -o pipefail; echo $SHOPT_o_pipefail");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "1\n");

        // set +o pipefail stores SHOPT_o_pipefail=0
        let (events, _) = run_shell("set -o pipefail; set +o pipefail; echo $SHOPT_o_pipefail");
        assert_eq!(get_stdout(&events), "0\n");
    }

    #[test]
    fn pipefail_uses_rightmost_failure() {
        // Without pipefail: last command determines status
        let (_, status) = run_shell("false | true");
        assert_eq!(status, 0);

        // With pipefail: rightmost non-zero status is used
        let (_, status) = run_shell("set -o pipefail; false | true");
        assert_eq!(status, 1);
    }

    #[test]
    fn pipefail_all_succeed_is_zero() {
        let (_, status) = run_shell("set -o pipefail; true | true | true");
        assert_eq!(status, 0);
    }

    #[test]
    fn pipefail_rightmost_nonzero() {
        // The rightmost non-zero should be chosen
        let (_, status) = run_shell("set -o pipefail; false | true | false");
        assert_eq!(status, 1);
    }

    #[test]
    fn nounset_unset_var_errors() {
        let (events, status) = run_shell("set -u; echo $UNSET_VAR");
        assert_eq!(status, 1);
        let stderr = get_stderr(&events);
        assert!(stderr.contains("UNSET_VAR"));
        assert!(stderr.contains("unbound variable"));
    }

    #[test]
    fn nounset_set_var_ok() {
        // set -u should not trigger for defined variables
        let (events, status) = run_shell("set -u; X=hello; echo $X");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hello\n");
    }

    #[test]
    fn nounset_special_params_ok() {
        // $? and $# should not trigger nounset
        let (events, status) = run_shell("set -u; echo $? $#");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "0 0\n");
    }

    #[test]
    fn nounset_with_default_operator() {
        // ${var:-default} should not trigger nounset even when var is unset
        let (events, status) = run_shell("set -u; echo ${UNSET:-fallback}");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "fallback\n");
    }

    #[test]
    fn nounset_long_option_alias() {
        // set -o nounset should be equivalent to set -u
        let (events, status) = run_shell("set -o nounset; echo $UNSET_VAR");
        assert_eq!(status, 1);
        let stderr = get_stderr(&events);
        assert!(stderr.contains("unbound variable"));
    }

    #[test]
    fn xtrace_outputs_commands() {
        let (events, status) = run_shell("set -x; echo hello");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hello\n");
        let stderr = get_stderr(&events);
        // xtrace should produce "+ echo hello" on stderr
        assert!(stderr.contains("+ echo hello"));
    }

    #[test]
    fn xtrace_custom_ps4() {
        let (events, _) = run_shell("PS4='>> '; set -x; echo test");
        let stderr = get_stderr(&events);
        assert!(stderr.contains(">> echo test"));
    }

    #[test]
    fn xtrace_disabled_with_plus_x() {
        let (events, _) = run_shell("set -x; set +x; echo quiet");
        let stderr = get_stderr(&events);
        // The "set +x" itself is traced, but "echo quiet" should not be
        assert!(stderr.contains("+ set +x"));
        assert!(!stderr.contains("+ echo quiet"));
    }

    #[test]
    fn noglob_skips_expansion() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        // Create a file that would match *.txt
        rt.handle_command(HostCommand::Run {
            input: "touch /hello.txt".into(),
        });
        // With noglob, the * should be literal
        let events = rt.handle_command(HostCommand::Run {
            input: "set -f; echo /*.txt".into(),
        });
        let stdout = get_stdout(&events);
        assert_eq!(stdout, "/*.txt\n");
    }

    #[test]
    fn noglob_disabled_allows_expansion() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "touch /abc.txt".into(),
        });
        // Enable then disable noglob: globs should work again
        let events = rt.handle_command(HostCommand::Run {
            input: "set -f; set +f; echo /*.txt".into(),
        });
        let stdout = get_stdout(&events);
        assert_eq!(stdout, "/abc.txt\n");
    }

    #[test]
    fn allexport_auto_exports() {
        let (events, status) = run_shell("set -a; MYVAR=hello; echo $MYVAR");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hello\n");
        // We can't directly test export flag from shell, but we can verify
        // via declare -p which shows flags. Or we simply verify the variable is set.
    }

    #[test]
    fn set_long_options_errexit() {
        // set -o errexit should be same as set -e
        let (events, status) = run_shell("set -o errexit; echo $SHOPT_e");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "1\n");
    }

    #[test]
    fn set_long_options_xtrace() {
        let (events, _) = run_shell("set -o xtrace; echo $SHOPT_x");
        assert_eq!(get_stdout(&events), "1\n");
    }

    #[test]
    fn set_long_options_allexport() {
        let (events, _) = run_shell("set -o allexport; echo $SHOPT_a");
        assert_eq!(get_stdout(&events), "1\n");
    }

    #[test]
    fn set_long_options_noglob() {
        let (events, _) = run_shell("set -o noglob; echo $SHOPT_f");
        assert_eq!(get_stdout(&events), "1\n");
    }

    #[test]
    fn set_long_options_noclobber() {
        let (events, _) = run_shell("set -o noclobber; echo $SHOPT_C");
        assert_eq!(get_stdout(&events), "1\n");
    }

    #[test]
    fn set_dash_o_lists_known_options() {
        let (events, status) = run_shell("set -o errexit; set -o");
        assert_eq!(status, 0);
        let out = get_stdout(&events);
        assert!(out.contains("errexit"));
        assert!(out.contains("pipefail"));
        assert!(out.contains("verbose"));
        assert!(out
            .lines()
            .any(|line| line.starts_with("errexit") && line.ends_with("on")));
    }

    #[test]
    fn set_plus_o_prints_recreatable_commands() {
        let (events, status) = run_shell("set -o errexit; set +o");
        assert_eq!(status, 0);
        let out = get_stdout(&events);
        assert!(out.contains("set -o errexit"));
        assert!(out.contains("set +o nounset"));
    }

    #[test]
    fn set_updates_special_dash_flags() {
        let (events, status) = run_shell("set -E -T -p -v; echo $-");
        assert_eq!(status, 0);
        let out = get_stdout(&events);
        assert!(out.contains('E'));
        assert!(out.contains('T'));
        assert!(out.contains('p'));
        assert!(out.contains('v'));
    }

    #[test]
    fn noexec_skips_subsequent_runs() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        let first = rt.handle_command(HostCommand::Run {
            input: "set -n".into(),
        });
        let first_status = first
            .iter()
            .find_map(|e| {
                if let WorkerEvent::Exit(s) = e {
                    Some(*s)
                } else {
                    None
                }
            })
            .unwrap_or(-1);
        assert_eq!(first_status, 0);

        let second = rt.handle_command(HostCommand::Run {
            input: "echo skipped".into(),
        });
        let second_status = second
            .iter()
            .find_map(|e| {
                if let WorkerEvent::Exit(s) = e {
                    Some(*s)
                } else {
                    None
                }
            })
            .unwrap_or(-1);
        assert_eq!(second_status, 0);
        assert_eq!(get_stdout(&second), "");
    }

    #[test]
    fn verbose_echoes_subsequent_runs() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        let first = rt.handle_command(HostCommand::Run {
            input: "set -v".into(),
        });
        let first_status = first
            .iter()
            .find_map(|e| {
                if let WorkerEvent::Exit(s) = e {
                    Some(*s)
                } else {
                    None
                }
            })
            .unwrap_or(-1);
        assert_eq!(first_status, 0);

        let events = rt.handle_command(HostCommand::Run {
            input: "echo hello".into(),
        });
        let status = events
            .iter()
            .find_map(|e| {
                if let WorkerEvent::Exit(s) = e {
                    Some(*s)
                } else {
                    None
                }
            })
            .unwrap_or(-1);
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hello\n");
        let stderr = get_stderr(&events);
        assert!(stderr.contains("echo hello"));
    }

    // ---- shopt builtin tests ----

    #[test]
    fn shopt_list_all() {
        let (events, status) = run_shell("shopt");
        assert_eq!(status, 0);
        let out = get_stdout(&events);
        assert!(out.contains("extglob"));
        assert!(out.contains("nullglob"));
        assert!(out.contains("dotglob"));
        assert!(out.contains("globstar"));
        assert!(out.contains("sourcepath"));
        assert!(out.contains("off"));
    }

    #[test]
    fn shopt_enable_option() {
        let (events, status) = run_shell("shopt -s extglob; shopt extglob");
        assert_eq!(status, 0);
        let out = get_stdout(&events);
        assert!(out.contains("extglob\ton"));
    }

    #[test]
    fn shopt_disable_option() {
        let (events, status) = run_shell("shopt -s extglob; shopt -u extglob; shopt extglob");
        assert_eq!(status, 0);
        let out = get_stdout(&events);
        assert!(out.contains("extglob\toff"));
    }

    #[test]
    fn shopt_invalid_option() {
        let (events, status) = run_shell("shopt -s nonexistent");
        assert_eq!(status, 1);
        let stderr = get_stderr(&events);
        assert!(stderr.contains("invalid shell option name"));
    }

    #[test]
    fn shopt_query_specific() {
        let (events, status) = run_shell("shopt nullglob");
        assert_eq!(status, 0);
        let out = get_stdout(&events);
        assert!(out.contains("nullglob\toff"));
    }

    #[test]
    fn shopt_sourcepath_defaults_on() {
        let (events, status) = run_shell("shopt sourcepath");
        assert_eq!(status, 0);
        let out = get_stdout(&events);
        assert!(out.contains("sourcepath\ton"));
    }

    #[test]
    fn source_uses_path_when_sourcepath_is_on() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "mkdir -p /lib".into(),
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/lib/defs.sh".into(),
            data: b"X=from-path\n".to_vec(),
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "PATH=/lib; source defs.sh; echo $X".into(),
        });
        let status = events
            .iter()
            .find_map(|e| {
                if let WorkerEvent::Exit(s) = e {
                    Some(*s)
                } else {
                    None
                }
            })
            .unwrap_or(-1);
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "from-path\n");
    }

    #[test]
    fn sourcepath_can_disable_path_lookup_for_source() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "mkdir -p /lib".into(),
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/lib/defs.sh".into(),
            data: b"X=from-path\n".to_vec(),
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "PATH=/lib; shopt -u sourcepath; source defs.sh".into(),
        });
        let status = events
            .iter()
            .find_map(|e| {
                if let WorkerEvent::Exit(s) = e {
                    Some(*s)
                } else {
                    None
                }
            })
            .unwrap_or(-1);
        assert_eq!(status, 1);
        assert!(get_stderr(&events).contains("source: defs.sh: not found"));
    }

    // ---- Dynamic variables ----

    #[test]
    fn dynamic_random() {
        let (events, status) = run_shell("echo $RANDOM");
        assert_eq!(status, 0);
        let out = get_stdout(&events);
        let val: u32 = out.trim().parse().unwrap();
        assert!(val < 32768);
    }

    #[test]
    fn dynamic_random_changes() {
        // Two calls should produce different values
        let (events, _) = run_shell("echo $RANDOM; echo $RANDOM");
        let out = get_stdout(&events);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_ne!(lines[0], lines[1]);
    }

    #[test]
    fn dynamic_lineno() {
        let (events, status) = run_shell("echo $LINENO");
        assert_eq!(status, 0);
        let out = get_stdout(&events);
        // LINENO should be a number
        let _val: u32 = out.trim().parse().unwrap();
    }

    #[test]
    fn dynamic_seconds() {
        let (events, status) = run_shell("echo $SECONDS");
        assert_eq!(status, 0);
        let out = get_stdout(&events);
        let val: u64 = out.trim().parse().unwrap();
        assert!(val < 60);
    }

    #[test]
    fn dynamic_funcname() {
        let (events, status) = run_shell("myfn() { echo $FUNCNAME; }; myfn");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "myfn\n");
    }

    #[test]
    fn dynamic_pipestatus() {
        let (events, status) = run_shell("true | false; echo ${PIPESTATUS[0]} ${PIPESTATUS[1]}");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "0 1\n");
    }

    #[test]
    fn streaming_grep_no_match_returns_failure() {
        let (events, status) = run_shell("echo a | grep b");
        assert_eq!(status, 1);
        assert_eq!(get_stdout(&events), "");
    }

    #[test]
    fn streaming_grep_updates_pipestatus() {
        let (events, status) = run_shell(
            "echo a | grep b | cat; echo ${PIPESTATUS[0]} ${PIPESTATUS[1]} ${PIPESTATUS[2]}",
        );
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "0 1 0\n");
    }

    #[test]
    fn streaming_grep_respects_pipefail_status() {
        let (_, status) = run_shell("set -o pipefail; echo a | grep b | cat");
        assert_eq!(status, 1);
    }

    #[test]
    fn dynamic_bash_source() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/test.sh".into(),
            data: b"echo $BASH_SOURCE".to_vec(),
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "source /test.sh".into(),
        });
        assert_eq!(get_stdout(&events), "/test.sh\n");
    }

    // ---- Alias/unalias ----

    #[test]
    fn alias_basic() {
        let (events, status) = run_shell("alias ll='echo listing'; ll");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "listing\n");
    }

    #[test]
    fn alias_with_args() {
        let (events, status) = run_shell("alias greet='echo hello'; greet world");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hello world\n");
    }

    #[test]
    fn shopt_expand_aliases_can_disable_alias_expansion() {
        let (events, status) = run_shell("alias ll='echo listing'; shopt -u expand_aliases; ll");
        assert_eq!(status, 127);
        assert!(get_stderr(&events).contains("command not found"));
    }

    #[test]
    fn shopt_expand_aliases_can_reenable_alias_expansion() {
        let (events, status) = run_shell(
            "alias ll='echo listing'; shopt -u expand_aliases; shopt -s expand_aliases; ll",
        );
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "listing\n");
    }

    #[test]
    fn alias_list_all() {
        let (events, status) = run_shell("alias ll='ls -la'; alias g='grep'; alias");
        assert_eq!(status, 0);
        let out = get_stdout(&events);
        assert!(out.contains("alias ll='ls -la'"));
        assert!(out.contains("alias g='grep'"));
    }

    #[test]
    fn alias_show_specific() {
        let (events, status) = run_shell("alias ll='ls -la'; alias ll");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "alias ll='ls -la'\n");
    }

    #[test]
    fn unalias_removes() {
        let (events, status) = run_shell("alias ll='echo hi'; unalias ll; ll");
        assert_eq!(status, 127); // command not found
        let stderr = get_stderr(&events);
        assert!(stderr.contains("command not found"));
    }

    #[test]
    fn unalias_all() {
        let (events, status) = run_shell("alias a='echo a'; alias b='echo b'; unalias -a; alias");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "");
    }

    // ---- Enhanced printf ----

    #[test]
    fn printf_hex() {
        let (events, _) = run_shell("printf '%x' 255");
        assert_eq!(get_stdout(&events), "ff");
    }

    #[test]
    fn printf_octal() {
        let (events, _) = run_shell("printf '%o' 8");
        assert_eq!(get_stdout(&events), "10");
    }

    #[test]
    fn printf_float() {
        let (events, _) = run_shell("printf '%.2f' 3.14159");
        assert_eq!(get_stdout(&events), "3.14");
    }

    #[test]
    fn printf_char() {
        let (events, _) = run_shell("printf '%c' A");
        assert_eq!(get_stdout(&events), "A");
    }

    #[test]
    fn printf_width_right_align() {
        let (events, _) = run_shell("printf '%10s' hello");
        assert_eq!(get_stdout(&events), "     hello");
    }

    #[test]
    fn printf_width_left_align() {
        let (events, _) = run_shell("printf '%-10s|' hello");
        assert_eq!(get_stdout(&events), "hello     |");
    }

    #[test]
    fn printf_zero_pad() {
        let (events, _) = run_shell("printf '%05d' 42");
        assert_eq!(get_stdout(&events), "00042");
    }

    #[test]
    fn printf_backslash_b() {
        let (events, _) = run_shell("printf '%b' 'hello\\nworld'");
        assert_eq!(get_stdout(&events), "hello\nworld");
    }

    #[test]
    fn printf_shell_quote_q() {
        let (events, _) = run_shell("printf '%q' 'hello world'");
        let out = get_stdout(&events);
        // Should be quoted with $'...' or similar
        assert!(out.contains("hello") && out.contains("world"));
    }

    #[test]
    fn printf_precision_string() {
        let (events, _) = run_shell("printf '%.3s' abcdef");
        assert_eq!(get_stdout(&events), "abc");
    }

    // ---- Enhanced read ----

    #[test]
    fn read_prompt() {
        let (events, _) = run_shell("echo hello | read -p 'Enter: ' VAR; echo done");
        let stderr = get_stderr(&events);
        assert!(stderr.contains("Enter: "));
    }

    #[test]
    fn read_delimiter() {
        let (events, status) = run_shell("printf 'a:b:c' | read -d ':' VAR; echo $VAR");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "a\n");
    }

    #[test]
    fn read_nchars() {
        let (events, status) = run_shell("echo 'hello' | read -n 3 VAR; echo $VAR");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hel\n");
    }

    #[test]
    fn read_exact_nchars() {
        let (events, status) = run_shell("printf 'ab\\ncd' | read -N 4 VAR; echo \"$VAR\"");
        assert_eq!(status, 0);
        // -N reads exactly 4 chars, ignoring delimiter
        let out = get_stdout(&events);
        assert!(out.starts_with("ab"));
    }

    #[test]
    fn read_into_array() {
        let (events, status) =
            run_shell("echo 'one two three' | read -a arr; echo ${arr[0]} ${arr[1]} ${arr[2]}");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "one two three\n");
    }

    // ---- builtin keyword ----

    #[test]
    fn builtin_keyword_invokes_builtin() {
        let (events, status) = run_shell("builtin echo hello");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hello\n");
    }

    #[test]
    fn builtin_keyword_skips_function() {
        let (events, status) =
            run_shell("echo() { printf 'FUNC: %s\\n' \"$1\"; }; builtin echo direct");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "direct\n");
    }

    #[test]
    fn builtin_keyword_not_builtin_errors() {
        let (events, status) = run_shell("builtin nonexistent");
        assert_eq!(status, 1);
        let stderr = get_stderr(&events);
        assert!(stderr.contains("not a shell builtin"));
    }

    #[test]
    fn builtin_keyword_inside_function_uses_real_builtin() {
        let (events, status) = run_shell(
            "echo() { builtin echo \"wrapped: $@\"; }\n\
             echo hello",
        );
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "wrapped: hello\n");
    }

    // ---- source PATH search ----

    #[test]
    fn source_path_search() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        // Create /bin directory and a script in it
        rt.handle_command(HostCommand::Run {
            input: "mkdir /bin".into(),
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/bin/helpers.sh".into(),
            data: b"LOADED=yes".to_vec(),
        });
        // Set PATH and source without slash
        let events = rt.handle_command(HostCommand::Run {
            input: "PATH=/bin; source helpers.sh; echo $LOADED".into(),
        });
        assert_eq!(get_stdout(&events), "yes\n");
    }

    // ---- mapfile/readarray ----

    #[test]
    fn mapfile_basic() {
        let (events, status) =
            run_shell("printf 'a\\nb\\nc\\n' | mapfile arr; echo ${arr[0]} ${arr[1]} ${arr[2]}");
        assert_eq!(status, 0);
        let out = get_stdout(&events);
        // Each element includes trailing newline by default
        assert!(out.contains('a'));
        assert!(out.contains('b'));
        assert!(out.contains('c'));
    }

    #[test]
    fn mapfile_strip_newline() {
        let (events, status) = run_shell(
            "printf 'x\\ny\\nz\\n' | mapfile -t arr; echo \"${arr[0]}${arr[1]}${arr[2]}\"",
        );
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "xyz\n");
    }

    #[test]
    fn mapfile_default_name() {
        let (events, status) = run_shell("printf 'hello\\nworld\\n' | mapfile; echo ${MAPFILE[0]}");
        assert_eq!(status, 0);
        let out = get_stdout(&events);
        assert!(out.contains("hello"));
    }

    #[test]
    fn readarray_is_alias_for_mapfile() {
        let (events, status) =
            run_shell("printf 'a\\nb\\n' | readarray -t arr; echo ${arr[0]} ${arr[1]}");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "a b\n");
    }

    #[test]
    fn process_subst_out_feeds_inner_command() {
        let (events, status) = run_shell("printf hi > >(cat)");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hi");
    }

    #[test]
    fn process_subst_out_runs_schedulable_inner_pipeline() {
        let (events, status) = run_shell("printf 'a\\nb\\n' > >(head -n 1 | cat)");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "a\n");
    }

    #[test]
    fn process_subst_out_runs_live_tail_pipeline() {
        let (events, status) = run_shell("printf 'a\\nb\\n' > >(tail -n 1 | cat)");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "b\n");
    }

    #[test]
    fn process_subst_out_runs_live_buffered_pipeline() {
        let (events, status) = run_shell("printf 'b\\na\\n' > >(sort | cat)");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "a\nb\n");
    }

    #[test]
    fn process_subst_out_isolates_shell_state() {
        let (events, status) =
            run_shell("foo=before; printf hi > >(foo=after; wc -c >/count.txt); echo $foo");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "before\n");
    }

    // ---- Pipe-ampersand (|&) ----

    #[test]
    fn pipe_amp_captures_stderr() {
        let (events, status) = run_shell("echo error >&2 |& cat");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "error\n");
    }

    #[test]
    fn plain_pipeline_leaves_stderr_unpiped() {
        let (events, status) = run_shell("echo error >&2 | cat");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "");
        assert_eq!(get_stderr(&events), "error\n");
    }

    #[test]
    fn pipe_amp_captures_both_stdout_and_stderr() {
        let (events, status) = run_shell("{ echo out; echo err >&2; } |& cat");
        assert_eq!(status, 0);
        let stdout = get_stdout(&events);
        assert!(stdout.contains("out"));
        assert!(stdout.contains("err"));
    }

    // ---- Case fall-through (;&) ----

    #[test]
    fn case_fallthrough() {
        let (events, status) = run_shell(
            "X=a\ncase $X in\n  a) echo one ;&\n  b) echo two ;;\n  c) echo three ;;\nesac",
        );
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "one\ntwo\n");
    }

    // ---- Case continue-testing (;;&) ----

    #[test]
    fn case_continue_testing() {
        let (events, status) = run_shell(
            "X=abc\ncase $X in\n  a*) echo starts-a ;;&\n  *b*) echo contains-b ;;&\n  *c) echo ends-c ;;\nesac",
        );
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "starts-a\ncontains-b\nends-c\n");
    }

    // ---- Case glob matching ----

    #[test]
    fn case_glob_pattern() {
        let (events, status) =
            run_shell("case hello in\n  h*) echo matched ;;\n  *) echo nope ;;\nesac");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "matched\n");
    }

    // ---- Select ----

    #[test]
    fn select_basic() {
        // Use echo pipe to provide stdin to select
        let (events, status) = run_shell(
            "echo 2 | select item in apple banana cherry; do\n  echo \"chose: $item\"\n  break\ndone",
        );
        assert_eq!(status, 0);
        let stdout = get_stdout(&events);
        assert!(stdout.contains("chose: banana"), "got: {stdout}");
    }

    // ---- $"..." locale quoting ----

    #[test]
    fn locale_quoting_basic() {
        let (events, status) = run_shell("echo $\"hello\"");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hello\n");
    }

    #[test]
    fn locale_quoting_with_variable() {
        let (events, status) = run_shell("X=world; echo $\"hello $X\"");
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "hello world\n");
    }

    // ---- nullglob ----

    #[test]
    fn nullglob_empty_on_no_match() {
        let (events, status) = run_shell(
            "shopt -s nullglob\nresult=$(echo /nonexistent/*.xyz)\nif test -z \"$result\"; then\n  echo empty\nfi",
        );
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "empty\n");
    }

    // ---- dotglob ----

    #[test]
    fn dotglob_matches_hidden() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "mkdir /tmp2".into(),
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/tmp2/.hidden".into(),
            data: vec![],
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/tmp2/visible".into(),
            data: vec![],
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "cd /tmp2; shopt -s dotglob; echo * | tr ' ' '\\n' | sort".into(),
        });
        let stdout = get_stdout(&events);
        assert!(stdout.contains(".hidden"), "got: {stdout}");
        assert!(stdout.contains("visible"), "got: {stdout}");
    }

    // ---- nocasematch ----

    #[test]
    fn nocasematch_case_statement() {
        let (events, status) = run_shell(
            "shopt -s nocasematch\nX=Hello\ncase $X in\n  hello) echo matched ;;\n  *) echo no-match ;;\nesac",
        );
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "matched\n");
    }

    #[test]
    fn nocasematch_double_bracket() {
        let (events, status) = run_shell(
            "shopt -s nocasematch\nif [[ HELLO == hello ]]; then echo yes; else echo no; fi",
        );
        assert_eq!(status, 0);
        assert_eq!(get_stdout(&events), "yes\n");
    }

    // ---- extglob matching ----

    #[test]
    fn extglob_match_at_basic() {
        assert!(extglob_match("@(jpg|png)", "jpg"));
        assert!(extglob_match("@(jpg|png)", "png"));
        assert!(!extglob_match("@(jpg|png)", "txt"));
    }

    #[test]
    fn extglob_match_star_suffix() {
        assert!(extglob_match("*.@(jpg|png)", "file.jpg"));
        assert!(extglob_match("*.@(jpg|png)", "file.png"));
        assert!(!extglob_match("*.@(jpg|png)", "file.txt"));
    }

    #[test]
    fn extglob_match_not() {
        assert!(!extglob_match("!(*.log)", "b.log"));
        assert!(extglob_match("!(*.log)", "a.txt"));
    }

    #[test]
    fn extglob_match_optional() {
        assert!(extglob_match("colo?(u)r", "color"));
        assert!(extglob_match("colo?(u)r", "colour"));
        assert!(!extglob_match("colo?(u)r", "colouur"));
    }

    // ---- extglob (integration) ----

    #[test]
    fn extglob_at_pattern() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/tmp3/file.jpg".into(),
            data: vec![],
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/tmp3/file.png".into(),
            data: vec![],
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/tmp3/file.txt".into(),
            data: vec![],
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "cd /tmp3; shopt -s extglob; for f in *.@(jpg|png); do echo $f; done | sort"
                .into(),
        });
        let stdout = get_stdout(&events);
        assert!(stdout.contains("file.jpg"), "got: {stdout}");
        assert!(stdout.contains("file.png"), "got: {stdout}");
        assert!(!stdout.contains("file.txt"), "got: {stdout}");
    }

    #[test]
    fn extglob_not_pattern() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/tmp4/a.txt".into(),
            data: vec![],
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/tmp4/b.log".into(),
            data: vec![],
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/tmp4/c.txt".into(),
            data: vec![],
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "cd /tmp4; shopt -s extglob; for f in !(*.log); do echo $f; done | sort".into(),
        });
        let stdout = get_stdout(&events);
        assert!(stdout.contains("a.txt"), "got: {stdout}");
        assert!(stdout.contains("c.txt"), "got: {stdout}");
        assert!(!stdout.contains("b.log"), "got: {stdout}");
    }

    #[test]
    fn extglob_optional_pattern() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/tmp5/color".into(),
            data: vec![],
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/tmp5/colour".into(),
            data: vec![],
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "cd /tmp5; shopt -s extglob; for f in colo?(u)r; do echo $f; done | sort".into(),
        });
        let stdout = get_stdout(&events);
        assert!(stdout.contains("color"), "got: {stdout}");
        assert!(stdout.contains("colour"), "got: {stdout}");
    }

    // ---- globstar ----

    #[test]
    fn globstar_recursive() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/project/a.txt".into(),
            data: vec![],
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/project/sub/b.txt".into(),
            data: vec![],
        });
        rt.handle_command(HostCommand::WriteFile {
            path: "/project/sub/deep/c.txt".into(),
            data: vec![],
        });
        let events = rt.handle_command(HostCommand::Run {
            input: "cd /project; shopt -s globstar; for f in **/*.txt; do echo $f; done | sort"
                .into(),
        });
        let stdout = get_stdout(&events);
        assert!(stdout.contains("a.txt"), "got: {stdout}");
        assert!(stdout.contains("sub/b.txt"), "got: {stdout}");
        assert!(stdout.contains("sub/deep/c.txt"), "got: {stdout}");
    }

    #[test]
    fn exec_live_redirections_preserve_left_to_right_dup_order() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = rt.handle_command(HostCommand::Run {
            input: "printf hi > /first.txt 1>&2\nprintf hi 1>&2 > /second.txt".into(),
        });

        assert_eq!(get_stdout(&events), "");
        assert_eq!(get_stderr(&events), "hi");

        let first = rt.handle_command(HostCommand::ReadFile {
            path: "/first.txt".into(),
        });
        assert_eq!(get_stdout(&first), "");

        let second = rt.handle_command(HostCommand::ReadFile {
            path: "/second.txt".into(),
        });
        assert_eq!(get_stdout(&second), "hi");
    }

    #[test]
    fn exec_process_subst_redirections_preserve_left_to_right_dup_order() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = rt.handle_command(HostCommand::Run {
            input: "printf hi > >(cat) 1>&2\nprintf hi 1>&2 > >(cat)".into(),
        });

        assert_eq!(get_stdout(&events), "hi");
        assert_eq!(get_stderr(&events), "hi");
    }

    #[test]
    fn process_subst_in_streams_native_pipeline() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = rt.handle_command(HostCommand::Run {
            input: "cat <(yes | head -n 5)".into(),
        });

        assert_eq!(get_stdout(&events), "y\ny\ny\ny\ny\n");
        assert_eq!(get_stderr(&events), "");
    }

    #[test]
    fn process_subst_in_streams_native_sed_pipeline() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = rt.handle_command(HostCommand::Run {
            input: "cat <(yes | sed 's/y/z/' | head -n 3)".into(),
        });

        assert_eq!(get_stdout(&events), "z\nz\nz\n");
        assert_eq!(get_stderr(&events), "");
    }

    #[test]
    fn process_subst_in_buffered_pipeline_still_works() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = rt.handle_command(HostCommand::Run {
            input: "cat <(printf 'b\\na\\n' | sort)".into(),
        });

        assert_eq!(get_stdout(&events), "a\nb\n");
        assert_eq!(get_stderr(&events), "");
    }

    #[test]
    fn process_subst_out_runs_live_tee_pipeline() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = rt.handle_command(HostCommand::Run {
            input: "printf 'a\\nb\\n' > >(tee /tee.txt | cat)".into(),
        });

        assert_eq!(get_stdout(&events), "a\nb\n");
        assert_eq!(get_stderr(&events), "");

        let file = rt.handle_command(HostCommand::ReadFile {
            path: "/tee.txt".into(),
        });
        assert_eq!(get_stdout(&file), "a\nb\n");
    }

    #[test]
    fn builtin_and_utility_redirections_write_files_during_execution() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });

        let events = rt.handle_command(HostCommand::Run {
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

        let builtin = rt.handle_command(HostCommand::ReadFile {
            path: "/builtin.txt".into(),
        });
        assert!(get_stdout(&builtin).contains("printf"));

        let utility = rt.handle_command(HostCommand::ReadFile {
            path: "/utility.txt".into(),
        });
        assert_eq!(get_stdout(&utility), "hi");
    }

    // ── Coverage gap tests ─────────────────────────────────────────────

    #[test]
    fn pushd_popd_dirs_manage_directory_stack() {
        let (events, _) = run_shell("mkdir -p /a /b; cd /a; pushd /b; popd; pwd");
        // pushd prints "/b /a", popd prints "/a", pwd prints "/a"
        assert_eq!(get_stdout(&events), "/b /a\n/a\n/a\n");
    }

    #[test]
    fn pushd_no_arg_swaps_top_two_dirs() {
        let (events, _) = run_shell("mkdir -p /a /b; cd /a; pushd /b; pushd; pwd");
        // pushd /b → cwd=/b, stack=[/a] → prints "/b /a"
        // pushd (no arg) → swaps to /a (top of stack), pushes /b → prints "/a /b /a"
        // pwd → "/a"
        assert_eq!(get_stdout(&events), "/b /a\n/a /b /a\n/a\n");
    }

    #[test]
    fn popd_empty_stack_errors() {
        let (events, _) = run_shell("popd");
        assert!(get_stderr(&events).contains("directory stack empty"));
    }

    #[test]
    fn test_unary_file_operators_extended() {
        let (events, _) = run_shell(
            "echo data > /tmp/f; [ -O /tmp/f ] && echo O_ok; [ -G /tmp/f ] && echo G_ok; [ -N /tmp/f ] && echo N_ok",
        );
        assert_eq!(get_stdout(&events), "O_ok\nG_ok\nN_ok\n");
    }

    #[test]
    fn test_binary_ef_operator() {
        let (events, _) =
            run_shell("echo x > /tmp/same; [ /tmp/same -ef /tmp/same ] && echo ef_ok");
        assert_eq!(get_stdout(&events), "ef_ok\n");
    }

    #[test]
    fn test_symlink_operators_return_false() {
        let (events, _) = run_shell(
            "echo x > /tmp/f; [ -L /tmp/f ] || echo L_ok; [ -S /tmp/f ] || echo S_ok; [ -p /tmp/f ] || echo p_ok",
        );
        assert_eq!(get_stdout(&events), "L_ok\nS_ok\np_ok\n");
    }

    #[test]
    fn export_dash_p_lists_exported_vars() {
        let (events, _) = run_shell("export MY_VAR=hello; export -p | grep MY_VAR");
        assert_eq!(get_stdout(&events), "declare -x MY_VAR=\"hello\"\n");
    }

    #[test]
    fn export_dash_n_unexports_variable() {
        let (events, _) =
            run_shell("export FOO=bar; export -n FOO; export -p | grep FOO; echo done");
        // After unexport, grep finds nothing, so only "done" shows
        assert_eq!(get_stdout(&events), "done\n");
    }

    #[test]
    fn readonly_dash_p_lists_readonly_vars() {
        let (events, _) = run_shell("readonly CONST=42; readonly -p | grep CONST");
        assert_eq!(get_stdout(&events), "declare -r CONST=\"42\"\n");
    }

    #[test]
    fn declare_dash_f_lists_function_bodies() {
        let (events, _) = run_shell("myfn() { echo hello; }; declare -f myfn");
        assert!(get_stdout(&events).contains("myfn"));
    }

    #[test]
    fn declare_dash_cap_f_lists_function_names() {
        let (events, _) = run_shell("aaa() { :; }; bbb() { :; }; declare -F | sort");
        assert_eq!(get_stdout(&events), "declare -f aaa\ndeclare -f bbb\n");
    }

    #[test]
    fn trap_reentry_does_not_recurse() {
        // DEBUG trap should not trigger recursively inside itself
        let (events, status) = run_shell("trap 'echo trapped' DEBUG; echo one; echo two");
        let stdout = get_stdout(&events);
        // Each command triggers DEBUG once; no infinite recursion
        assert!(stdout.contains("trapped"));
        assert!(stdout.contains("one"));
        assert!(stdout.contains("two"));
        assert_eq!(status, 0);
    }

    #[test]
    fn signal_with_sig_prefix_is_accepted() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "trap 'echo caught' TERM".into(),
        });
        let events = rt.handle_command(HostCommand::Signal {
            signal: "SIGTERM".into(),
        });
        assert_eq!(get_stdout(&events), "caught\n");
    }

    #[test]
    fn signal_by_number_is_accepted() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "trap 'echo got15' TERM".into(),
        });
        let events = rt.handle_command(HostCommand::Signal {
            signal: "15".into(),
        });
        assert_eq!(get_stdout(&events), "got15\n");
    }

    #[test]
    fn signal_case_insensitive() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "trap 'echo ok' TERM".into(),
        });
        let events = rt.handle_command(HostCommand::Signal {
            signal: "term".into(),
        });
        assert_eq!(get_stdout(&events), "ok\n");
    }

    #[test]
    fn ignored_signal_produces_no_output() {
        let mut rt = WorkerRuntime::new();
        rt.handle_command(HostCommand::Init {
            step_budget: 0,
            allowed_hosts: vec![],
            network_policy: None,
        });
        rt.handle_command(HostCommand::Run {
            input: "trap '' TERM".into(),
        });
        let events = rt.handle_command(HostCommand::Signal {
            signal: "TERM".into(),
        });
        assert_eq!(get_stdout(&events), "");
        // Should not exit
        assert!(!events.iter().any(|e| matches!(e, WorkerEvent::Exit(_))));
    }
}
// ── wasm-bindgen entry points (wasm32 only) ────────────────────────

#[cfg(target_arch = "wasm32")]
mod wasm_bindings {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use wasm_bindgen::prelude::*;
    use wasmsh_protocol::{
        HostCommand, NetworkDefaultAction, NetworkPolicyConfig as ProtocolNetworkPolicyConfig,
    };
    use wasmsh_runtime::{
        ExternalCommandOptions, ExternalCommandResult, ExternalCommandSpec,
        ExternalCommandSpecHandler, ExternalProcess, ExternalProcessPoll, ExternalProcessWrite,
        ExternalStreamHandler,
    };
    use wasmsh_utils::net_types::{
        validate_http_url, HttpRequest, HttpResponse, NetworkBackend, NetworkError,
    };
    use wasmsh_utils::{ClockError, ClockProvider, FixedClock, UnavailableClock, UtcDateTime};

    use crate::WorkerRuntime;

    fn external_failure(status: i32, message: impl Into<String>) -> ExternalCommandResult {
        ExternalCommandResult {
            stdout: Vec::new(),
            stderr: format!("wasmsh: {}\n", message.into()).into_bytes(),
            status,
        }
    }

    fn js_bytes(value: JsValue, field: &str) -> Result<Vec<u8>, String> {
        if value.is_undefined() || value.is_null() {
            return Ok(Vec::new());
        }
        if value.is_instance_of::<js_sys::Uint8Array>() {
            return Ok(js_sys::Uint8Array::from(value).to_vec());
        }
        if value.is_instance_of::<js_sys::Array>() {
            let array = js_sys::Array::from(&value);
            let mut bytes = Vec::with_capacity(array.length() as usize);
            for item in array.iter() {
                let number = item
                    .as_f64()
                    .ok_or_else(|| format!("external {field} must contain byte numbers"))?;
                if !number.is_finite() || number.fract() != 0.0 || !(0.0..=255.0).contains(&number)
                {
                    return Err(format!("external {field} contains an invalid byte"));
                }
                bytes.push(number as u8);
            }
            return Ok(bytes);
        }
        Err(format!("external {field} must be a Uint8Array"))
    }

    fn read_stdin(
        mut stdin: Option<wasmsh_runtime::ExternalCommandStdin<'_>>,
        max_bytes: u64,
    ) -> Result<Vec<u8>, String> {
        let Some(mut stdin) = stdin.take() else {
            return Ok(Vec::new());
        };
        let mut data = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            let read = stdin
                .read_chunk(&mut buffer)
                .map_err(|error| format!("external stdin read failed: {error}"))?;
            if read == 0 {
                return Ok(data);
            }
            if data.len() as u64 + read as u64 > max_bytes {
                return Err(format!(
                    "external stdin limit exceeded (limit {max_bytes} bytes)"
                ));
            }
            data.extend_from_slice(&buffer[..read]);
        }
    }

    fn js_external_handler(
        executor: &Rc<RefCell<Option<js_sys::Function>>>,
        spec: &ExternalCommandSpec,
        argv: &[String],
        stdin: Option<wasmsh_runtime::ExternalCommandStdin<'_>>,
    ) -> ExternalCommandResult {
        let Some(callback) = executor.borrow().clone() else {
            return external_failure(
                126,
                "native external processes are not supported by this host",
            );
        };
        let input = match read_stdin(stdin, spec.options.max_input_bytes) {
            Ok(input) => input,
            Err(error) => return external_failure(125, error),
        };
        let argv_js = js_sys::Array::new();
        for arg in argv {
            argv_js.push(&JsValue::from_str(arg));
        }
        let options_json = match serde_json::to_string(&spec.options) {
            Ok(json) => json,
            Err(error) => {
                return external_failure(126, format!("invalid external options: {error}"))
            }
        };
        let input_js = js_sys::Uint8Array::from(input.as_slice());
        let result = match callback.call5(
            &JsValue::UNDEFINED,
            &JsValue::from_str(&spec.name),
            &JsValue::from_str(&spec.executable),
            argv_js.as_ref(),
            input_js.as_ref(),
            &JsValue::from_str(&options_json),
        ) {
            Ok(result) => result,
            Err(error) => {
                let message = error
                    .as_string()
                    .unwrap_or_else(|| "JavaScript external executor threw".into());
                return external_failure(126, format!("external host error: {message}"));
            }
        };
        if !result.is_object() {
            return external_failure(126, "external executor must return an object");
        }
        let status = match js_sys::Reflect::get(&result, &JsValue::from_str("status"))
            .ok()
            .and_then(|value| value.as_f64())
        {
            Some(value) if value.is_finite() && value.fract() == 0.0 => {
                if value < i32::MIN as f64 || value > i32::MAX as f64 {
                    return external_failure(126, "external status is outside the i32 range");
                }
                value as i32
            }
            _ => return external_failure(126, "external executor returned an invalid status"),
        };
        let stdout = match js_sys::Reflect::get(&result, &JsValue::from_str("stdout"))
            .map_err(|_| "external executor result has no readable stdout".to_string())
            .and_then(|value| js_bytes(value, "stdout"))
        {
            Ok(bytes) => bytes,
            Err(error) => return external_failure(126, error),
        };
        let stderr = match js_sys::Reflect::get(&result, &JsValue::from_str("stderr"))
            .map_err(|_| "external executor result has no readable stderr".to_string())
            .and_then(|value| js_bytes(value, "stderr"))
        {
            Ok(bytes) => bytes,
            Err(error) => return external_failure(126, error),
        };
        ExternalCommandResult {
            stdout,
            stderr,
            status,
        }
    }

    fn stream_request(
        operation: &str,
        process_id: &str,
        spec: Option<&ExternalCommandSpec>,
        argv: &[String],
        data: &[u8],
    ) -> Result<JsValue, String> {
        let request = js_sys::Object::new();
        js_sys::Reflect::set(
            &request,
            &JsValue::from_str("operation"),
            &JsValue::from_str(operation),
        )
        .map_err(|_| "could not build external stream request".to_string())?;
        js_sys::Reflect::set(
            &request,
            &JsValue::from_str("process_id"),
            &JsValue::from_str(process_id),
        )
        .map_err(|_| "could not build external stream request".to_string())?;
        let command_name = spec.map_or("", |spec| spec.name.as_str());
        let executable = spec.map_or("", |spec| spec.executable.as_str());
        let options_json = spec
            .map(|spec| serde_json::to_string(&spec.options))
            .transpose()
            .map_err(|error| format!("invalid external options: {error}"))?
            .unwrap_or_else(|| "{}".into());
        js_sys::Reflect::set(
            &request,
            &JsValue::from_str("command_name"),
            &JsValue::from_str(command_name),
        )
        .map_err(|_| "could not build external stream request".to_string())?;
        js_sys::Reflect::set(
            &request,
            &JsValue::from_str("executable"),
            &JsValue::from_str(executable),
        )
        .map_err(|_| "could not build external stream request".to_string())?;
        let argv_js = js_sys::Array::new();
        for arg in argv {
            argv_js.push(&JsValue::from_str(arg));
        }
        js_sys::Reflect::set(&request, &JsValue::from_str("argv"), argv_js.as_ref())
            .map_err(|_| "could not build external stream request".to_string())?;
        let data_js = js_sys::Uint8Array::from(data);
        js_sys::Reflect::set(&request, &JsValue::from_str("data"), data_js.as_ref())
            .map_err(|_| "could not build external stream request".to_string())?;
        js_sys::Reflect::set(
            &request,
            &JsValue::from_str("options_json"),
            &JsValue::from_str(&options_json),
        )
        .map_err(|_| "could not build external stream request".to_string())?;
        Ok(request.into())
    }

    struct JsExternalProcess {
        executor: Rc<RefCell<Option<js_sys::Function>>>,
        process_id: String,
    }

    impl JsExternalProcess {
        fn callback(&self) -> Result<js_sys::Function, String> {
            self.executor
                .borrow()
                .clone()
                .ok_or_else(|| "streaming external executor was cleared".into())
        }

        fn call(&self, operation: &str, data: &[u8]) -> Result<JsValue, String> {
            let callback = self.callback()?;
            let request = stream_request(operation, &self.process_id, None, &[], data)?;
            callback
                .call1(&JsValue::UNDEFINED, &request)
                .map_err(|error| {
                    error
                        .as_string()
                        .unwrap_or_else(|| "JavaScript streaming executor threw".into())
                })
        }

        #[allow(clippy::needless_pass_by_value)] // JS handles are passed by value
        fn parse_number(value: JsValue, field: &str) -> Result<f64, String> {
            value
                .as_f64()
                .filter(|value| value.is_finite() && value.fract() == 0.0)
                .ok_or_else(|| format!("external stream {field} must be an integer"))
        }

        fn start(
            executor: &Rc<RefCell<Option<js_sys::Function>>>,
            spec: &ExternalCommandSpec,
            argv: &[String],
        ) -> Result<Box<dyn ExternalProcess>, String> {
            let callback = executor
                .borrow()
                .clone()
                .ok_or_else(|| "streaming external executor is unavailable".to_string())?;
            let request = stream_request("start", "", Some(spec), argv, &[])?;
            let response = callback
                .call1(&JsValue::UNDEFINED, &request)
                .map_err(|error| {
                    error
                        .as_string()
                        .unwrap_or_else(|| "JavaScript streaming executor threw".into())
                })?;
            let process_id = js_sys::Reflect::get(&response, &JsValue::from_str("process_id"))
                .ok()
                .and_then(|value| value.as_string())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    "streaming external executor must synchronously return process_id".to_string()
                })?;
            Ok(Box::new(Self {
                executor: executor.clone(),
                process_id,
            }))
        }
    }

    impl ExternalProcess for JsExternalProcess {
        fn write_stdin(&mut self, data: &[u8]) -> ExternalProcessWrite {
            let Ok(response) = self.call("write_stdin", data) else {
                return ExternalProcessWrite {
                    accepted: 0,
                    would_block: false,
                    closed: true,
                };
            };
            let accepted = js_sys::Reflect::get(&response, &JsValue::from_str("accepted"))
                .ok()
                .and_then(|value| Self::parse_number(value, "accepted").ok())
                .map_or(0, |value| value.max(0.0) as usize);
            let would_block = js_sys::Reflect::get(&response, &JsValue::from_str("would_block"))
                .ok()
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let closed = js_sys::Reflect::get(&response, &JsValue::from_str("closed"))
                .ok()
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            ExternalProcessWrite {
                accepted,
                would_block,
                closed,
            }
        }

        fn close_stdin(&mut self) {
            let _ = self.call("close_stdin", &[]);
        }

        fn poll(&mut self) -> ExternalProcessPoll {
            let response = match self.call("poll", &[]) {
                Ok(response) => response,
                Err(error) => {
                    return ExternalProcessPoll {
                        error: Some(error),
                        ..ExternalProcessPoll::default()
                    }
                }
            };
            let stdout = js_sys::Reflect::get(&response, &JsValue::from_str("stdout"))
                .ok()
                .and_then(|value| js_bytes(value, "stdout").ok())
                .unwrap_or_default();
            let stderr = js_sys::Reflect::get(&response, &JsValue::from_str("stderr"))
                .ok()
                .and_then(|value| js_bytes(value, "stderr").ok())
                .unwrap_or_default();
            let optional_status = js_sys::Reflect::get(&response, &JsValue::from_str("status"))
                .ok()
                .filter(|value| !value.is_null() && !value.is_undefined())
                .and_then(|value| Self::parse_number(value, "status").ok())
                .and_then(|value| {
                    (i32::MIN as f64..=i32::MAX as f64)
                        .contains(&value)
                        .then_some(value as i32)
                });
            let error = js_sys::Reflect::get(&response, &JsValue::from_str("error"))
                .ok()
                .and_then(|value| value.as_string());
            ExternalProcessPoll {
                stdout,
                stderr,
                stdout_eof: js_sys::Reflect::get(&response, &JsValue::from_str("stdout_eof"))
                    .ok()
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
                stderr_eof: js_sys::Reflect::get(&response, &JsValue::from_str("stderr_eof"))
                    .ok()
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
                status: optional_status,
                stdin_writable: js_sys::Reflect::get(
                    &response,
                    &JsValue::from_str("stdin_writable"),
                )
                .ok()
                .and_then(|value| value.as_bool())
                .unwrap_or(true),
                error,
            }
        }

        fn cancel(&mut self) {
            let _ = self.call("cancel", &[]);
        }
    }

    struct JsClockProvider {
        callback: js_sys::Function,
    }

    impl JsClockProvider {
        #[allow(clippy::needless_pass_by_value)] // JS handles are passed by value
        fn callback_error(value: JsValue) -> ClockError {
            ClockError::Callback(
                value
                    .as_string()
                    .unwrap_or_else(|| "JavaScript exception".into()),
            )
        }

        #[allow(clippy::needless_pass_by_value)] // JS handles are passed by value
        fn number(value: JsValue, label: &str) -> Result<f64, ClockError> {
            let value = value
                .as_f64()
                .ok_or_else(|| ClockError::Callback(format!("{label} must return a number")))?;
            if !value.is_finite() || value.fract() != 0.0 {
                return Err(ClockError::Callback(format!(
                    "{label} must return a finite integer"
                )));
            }
            Ok(value)
        }

        /// Monotonic readings (`performance.now()`) are legitimately
        /// fractional, unlike the integer Unix-millisecond wall clock.
        #[allow(clippy::needless_pass_by_value)] // JS handles are passed by value
        fn monotonic_number(value: JsValue, label: &str) -> Result<f64, ClockError> {
            let value = value
                .as_f64()
                .ok_or_else(|| ClockError::Callback(format!("{label} must return a number")))?;
            if !value.is_finite() || value < 0.0 {
                return Err(ClockError::Callback(format!(
                    "{label} must return a finite non-negative number"
                )));
            }
            Ok(value)
        }
    }

    impl ClockProvider for JsClockProvider {
        fn now_unix_ms(&self) -> Result<i64, ClockError> {
            let value = self
                .callback
                .call0(&JsValue::UNDEFINED)
                .map_err(Self::callback_error)?;
            let value = Self::number(value, "clock callback")?;
            if value.abs() > 9_007_199_254_740_991.0 {
                return Err(ClockError::Callback(
                    "clock callback result exceeds JavaScript safe integer range".into(),
                ));
            }
            let value = value as i64;
            UtcDateTime::from_unix_ms(value)
                .map(|_| value)
                .map_err(|error| ClockError::Callback(error.to_string()))
        }

        fn monotonic_now_ms(&self) -> Result<u64, ClockError> {
            let global = js_sys::global();
            let performance = js_sys::Reflect::get(&global, &JsValue::from_str("performance"))
                .map_err(Self::callback_error)?;
            let now = js_sys::Reflect::get(&performance, &JsValue::from_str("now"))
                .map_err(Self::callback_error)?
                .dyn_into::<js_sys::Function>()
                .map_err(|_| ClockError::Callback("performance.now is unavailable".into()))?;
            let value = now.call0(&performance).map_err(Self::callback_error)?;
            let value = Self::monotonic_number(value, "performance.now")?;
            if value > u64::MAX as f64 {
                return Err(ClockError::Callback(
                    "performance.now returned an unsupported value".into(),
                ));
            }
            // Truncate sub-millisecond precision instead of rejecting the
            // reading; browsers and Node both return fractional values.
            Ok(value.floor() as u64)
        }
    }

    // JS function provided by the worker scope for synchronous HTTP.
    #[wasm_bindgen]
    extern "C" {
        /// Synchronous HTTP fetch implemented in JavaScript (Web Worker).
        /// Returns a JS object: `{ status: number, headers_json: string, body: Uint8Array }`.
        fn wasmsh_http_fetch(
            url: &str,
            method: &str,
            headers_json: &str,
            body: &[u8],
            body_len: u32,
            follow_redirects: bool,
            options_json: &str,
        ) -> JsValue;
    }

    /// Browser backend. Synchronous XHR is deliberately not treated as a
    /// trusted broker because it cannot disable automatic redirects.
    struct BrowserNetworkBackend {
        trusted_broker: bool,
        /// Shared with `WasmShell`. Only set once a configuration has been
        /// validated and accepted by the runtime. While false every request is
        /// refused, so a rejected or mis-ordered initialization can never
        /// leave an allow-all transport reachable.
        policy_ready: Rc<Cell<bool>>,
    }

    impl NetworkBackend for BrowserNetworkBackend {
        fn fetch(&self, request: &HttpRequest) -> Result<HttpResponse, NetworkError> {
            if !self.policy_ready.get() {
                return Err(NetworkError::HostDenied(
                    "network policy was not initialized; refusing request".into(),
                ));
            }
            if !self.trusted_broker {
                return Err(NetworkError::Other(
                    "standalone browser network requires a trusted redirect-aware broker; synchronous XHR is refused".into(),
                ));
            }

            let headers_json =
                serde_json::to_string(&request.headers).unwrap_or_else(|_| "[]".into());
            let options_json = serde_json::json!({
                "timeout_ms": request.timeout_ms,
                "connect_timeout_ms": request.connect_timeout_ms,
                "max_redirs": request.max_redirs,
                "max_response_bytes": request.max_response_bytes,
            })
            .to_string();
            let body = request.body.as_deref().unwrap_or(&[]);
            let body_len = body.len() as u32;

            let result = wasmsh_http_fetch(
                &request.url,
                &request.method,
                &headers_json,
                body,
                body_len,
                request.follow_redirects,
                &options_json,
            );

            // Parse the JS result object.
            let status = js_sys::Reflect::get(&result, &"status".into())
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as u16;

            let headers_str = js_sys::Reflect::get(&result, &"headers_json".into())
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "[]".into());
            let headers: Vec<(String, String)> =
                serde_json::from_str(&headers_str).unwrap_or_default();

            let body_val = js_sys::Reflect::get(&result, &"body".into())
                .ok()
                .unwrap_or(JsValue::NULL);
            let body_bytes = if body_val.is_instance_of::<js_sys::Uint8Array>() {
                js_sys::Uint8Array::from(body_val).to_vec()
            } else {
                Vec::new()
            };

            // Check for error field (connection failure, etc.)
            if let Ok(err_val) = js_sys::Reflect::get(&result, &"error".into()) {
                if let Some(err_msg) = err_val.as_string() {
                    let reason = js_sys::Reflect::get(&result, &"error_reason".into())
                        .ok()
                        .and_then(|value| value.as_string())
                        .unwrap_or_default();
                    return Err(match reason.as_str() {
                        "timeout" => NetworkError::Timeout(err_msg),
                        "response_too_large" | "response_overflow" | "payload_too_large" => {
                            NetworkError::ResponseTooLarge(err_msg)
                        }
                        "invalid_url" => NetworkError::InvalidUrl(err_msg),
                        "host_denied" => NetworkError::HostDenied(err_msg),
                        "too_many_redirects" => NetworkError::TooManyRedirects(err_msg),
                        _ if err_msg.to_ascii_lowercase().contains("timeout") => {
                            NetworkError::Timeout(err_msg)
                        }
                        _ => NetworkError::ConnectionFailed(err_msg),
                    });
                }
            }

            if let Some(limit) = request.max_response_bytes {
                if body_bytes.len() as u64 > limit {
                    return Err(NetworkError::ResponseTooLarge(format!(
                        "response has {} bytes, limit is {limit}",
                        body_bytes.len()
                    )));
                }
            }

            Ok(HttpResponse {
                status,
                headers,
                body: body_bytes,
            })
        }

        fn check_url(&self, url: &str) -> Result<(), NetworkError> {
            if !self.policy_ready.get() {
                return Err(NetworkError::HostDenied(
                    "network policy was not initialized; refusing request".into(),
                ));
            }
            // The runtime's policy wrapper owns concrete rule matching; this
            // backend only gates on a validated policy being present.
            validate_http_url(url)
        }
    }

    /// Browser-facing shell instance exposed via `wasm-bindgen`.
    #[wasm_bindgen]
    #[allow(missing_debug_implementations)]
    pub struct WasmShell {
        runtime: WorkerRuntime,
        trusted_network_broker: bool,
        /// True only after `init` has validated a network configuration and
        /// the runtime accepted it. Gates the browser transport so a rejected
        /// or absent configuration fails closed.
        network_policy_ready: Rc<Cell<bool>>,
        external_executor: Rc<RefCell<Option<js_sys::Function>>>,
        external_stream_executor: Rc<RefCell<Option<js_sys::Function>>>,
    }

    #[wasm_bindgen]
    impl WasmShell {
        /// Create a new shell instance.
        #[wasm_bindgen(constructor)]
        pub fn new() -> Self {
            console_error_panic_hook::set_once();
            let external_executor = Rc::new(RefCell::new(None));
            let external_stream_executor = Rc::new(RefCell::new(None));
            let mut runtime = WorkerRuntime::new();
            let executor_state = external_executor.clone();
            let handler: ExternalCommandSpecHandler = Box::new(move |spec, argv, stdin| {
                Some(js_external_handler(&executor_state, spec, argv, stdin))
            });
            runtime.set_external_spec_handler(handler);
            let stream_executor_state = external_stream_executor.clone();
            let stream_handler: ExternalStreamHandler = Box::new(move |spec, argv| {
                JsExternalProcess::start(&stream_executor_state, spec, argv)
            });
            runtime.set_external_stream_handler(stream_handler);
            Self {
                runtime,
                trusted_network_broker: false,
                network_policy_ready: Rc::new(Cell::new(false)),
                external_executor,
                external_stream_executor,
            }
        }

        /// Mark the installed host fetch function as a trusted broker.
        ///
        /// The caller must provide a broker that disables automatic
        /// redirects, applies policy before every hop, enforces time and
        /// response limits while reading, and strips cross-origin secrets.
        /// The default is false because browser XHR cannot satisfy that
        /// contract. The standalone XHR fixture intentionally remains
        /// refused.
        pub fn set_trusted_network_broker(&mut self, trusted: bool) {
            self.trusted_network_broker = trusted;
        }

        /// Register a synchronous JavaScript wall-clock callback returning
        /// integer Unix milliseconds. The callback is owned by this shell.
        pub fn set_clock_callback(&mut self, callback: js_sys::Function) {
            self.runtime
                .set_clock_provider(Box::new(JsClockProvider { callback }));
        }

        /// Remove the clock capability. `Date` and `SigV4` then fail rather than
        /// falling back to a startup timestamp or a fabricated default.
        pub fn clear_clock_callback(&mut self) {
            self.runtime.set_clock_provider(Box::new(UnavailableClock));
        }

        /// Install an explicit fixed wall clock for deterministic embedding
        /// tests. This is not used by the default worker bootstrap.
        pub fn set_fixed_time_ms(&mut self, unix_ms: i64) -> Result<(), JsValue> {
            let clock =
                FixedClock::new(unix_ms).map_err(|error| JsValue::from_str(&error.to_string()))?;
            self.runtime.set_clock_provider(Box::new(clock));
            Ok(())
        }

        /// Install a synchronous external executor. The callback receives
        /// `(command_name, fixed_executable, argv, stdin_bytes, options_json)`
        /// and must return `{status, stdout, stderr}` with byte arrays. It
        /// must not invoke a shell or return a Promise.
        pub fn set_external_executor(&mut self, callback: js_sys::Function) {
            *self.external_executor.borrow_mut() = Some(callback);
        }

        /// Remove the external executor. Registered commands then fail with
        /// status 126 because this host cannot start native processes.
        pub fn clear_external_executor(&mut self) {
            *self.external_executor.borrow_mut() = None;
        }

        /// Install a synchronous operation callback for progressive external
        /// processes. It receives one request object with `operation` equal
        /// to `start`, `write_stdin`, `close_stdin`, `poll`, or `cancel`.
        /// The callback must return immediately; it must never return a Promise.
        pub fn set_external_stream_executor(&mut self, callback: js_sys::Function) {
            *self.external_stream_executor.borrow_mut() = Some(callback);
        }

        /// Remove the progressive external process callback.
        pub fn clear_external_stream_executor(&mut self) {
            *self.external_stream_executor.borrow_mut() = None;
        }

        /// Register or replace a fixed executable external command.
        /// `options_json` must be an object matching `ExternalCommandOptions`.
        pub fn register_external(
            &mut self,
            name: &str,
            executable: &str,
            options_json: &str,
        ) -> Result<(), JsValue> {
            let options: ExternalCommandOptions =
                serde_json::from_str(options_json).map_err(|error| {
                    JsValue::from_str(&format!("invalid external options: {error}"))
                })?;
            self.runtime
                .register_external(name, executable, options)
                .map_err(|error| JsValue::from_str(&error))
        }

        /// Unregister an external command and return whether it existed.
        pub fn unregister_external(&mut self, name: &str) -> bool {
            self.runtime.unregister_external(name)
        }

        /// Return registered external command names as a JSON array.
        pub fn external_commands(&self) -> String {
            serde_json::to_string(&self.runtime.external_command_names())
                .unwrap_or_else(|_| "[]".into())
        }

        /// Initialize the shell with a step budget and a network allowlist.
        /// `allowed_hosts_json` is a JSON array of host patterns (default `"[]"`).
        /// An empty allowlist creates a backend that denies every host, so
        /// callers get a `host denied` error instead of `network access not
        /// available`.  Returns a JSON array of events.
        pub fn init(&mut self, step_budget: u64, network_config_json: &str) -> String {
            // Re-initialization must never leave a previously accepted policy
            // reachable if the new configuration is rejected.
            self.network_policy_ready.set(false);
            let backend = BrowserNetworkBackend {
                trusted_broker: self.trusted_network_broker,
                policy_ready: self.network_policy_ready.clone(),
            };
            self.runtime.set_network_backend(Box::new(backend));

            let (allowed_hosts, network_policy) = match parse_network_config(network_config_json) {
                Ok(config) => config,
                Err(error) => {
                    return serde_json::to_string(&[wasmsh_protocol::WorkerEvent::Diagnostic(
                        wasmsh_protocol::DiagnosticLevel::Error,
                        format!("invalid network configuration: {error}"),
                    )])
                    .unwrap_or_else(|_| "[]".into());
                }
            };

            // Validate rules here as well as in the runtime so an invalid
            // pattern is rejected before any transport is marked usable.
            let resolved = network_policy
                .clone()
                .unwrap_or_else(|| ProtocolNetworkPolicyConfig {
                    enabled: !allowed_hosts.is_empty(),
                    default_action: NetworkDefaultAction::Deny,
                    allow: allowed_hosts.clone(),
                    deny: Vec::new(),
                });
            if let Err(error) = wasmsh_utils::NetworkPolicy::try_from_config(resolved) {
                return serde_json::to_string(&[wasmsh_protocol::WorkerEvent::Diagnostic(
                    wasmsh_protocol::DiagnosticLevel::Error,
                    format!("invalid network policy: {error}"),
                )])
                .unwrap_or_else(|_| "[]".into());
            }

            let events = self.runtime.handle_command(HostCommand::Init {
                step_budget,
                allowed_hosts,
                network_policy,
            });
            let rejected = events.iter().any(|event| {
                matches!(
                    event,
                    wasmsh_protocol::WorkerEvent::Diagnostic(
                        wasmsh_protocol::DiagnosticLevel::Error,
                        _
                    )
                )
            });
            if !rejected {
                self.network_policy_ready.set(true);
            }
            serde_json::to_string(&events).unwrap_or_default()
        }

        /// Execute a shell command.  Returns a JSON array of events.
        #[wasm_bindgen(js_name = "exec")]
        pub fn run(&mut self, input: &str) -> String {
            let events = self.runtime.handle_command(HostCommand::Run {
                input: input.to_string(),
            });
            serde_json::to_string(&events).unwrap_or_default()
        }

        /// Start a progressive execution. Poll with `poll_run` until an Exit
        /// event is returned. This is required for streaming external commands.
        pub fn start_run(&mut self, input: &str) -> String {
            let events = self.runtime.handle_command(HostCommand::StartRun {
                input: input.to_string(),
            });
            serde_json::to_string(&events).unwrap_or_default()
        }

        /// Advance a progressive execution without blocking for external I/O.
        pub fn poll_run(&mut self) -> String {
            let events = self.runtime.handle_command(HostCommand::PollRun);
            serde_json::to_string(&events).unwrap_or_default()
        }

        /// Write a file to the VFS.  Returns a JSON array of events.
        pub fn write_file(&mut self, path: &str, data: &[u8]) -> String {
            let events = self.runtime.handle_command(HostCommand::WriteFile {
                path: path.to_string(),
                data: data.to_vec(),
            });
            serde_json::to_string(&events).unwrap_or_default()
        }

        /// Read a file from the VFS.  Returns a JSON array of events.
        pub fn read_file(&mut self, path: &str) -> String {
            let events = self.runtime.handle_command(HostCommand::ReadFile {
                path: path.to_string(),
            });
            serde_json::to_string(&events).unwrap_or_default()
        }

        /// List a directory.  Returns a JSON array of events.
        pub fn list_dir(&mut self, path: &str) -> String {
            let events = self.runtime.handle_command(HostCommand::ListDir {
                path: path.to_string(),
            });
            serde_json::to_string(&events).unwrap_or_default()
        }

        /// Cancel the currently running execution.  Returns a JSON array of events.
        pub fn cancel(&mut self) -> String {
            let events = self.runtime.handle_command(HostCommand::Cancel);
            serde_json::to_string(&events).unwrap_or_default()
        }

        /// Deliver a POSIX signal name or number.  Returns a JSON array of events.
        pub fn signal(&mut self, signal: &str) -> String {
            let events = self.runtime.handle_command(HostCommand::Signal {
                signal: signal.to_string(),
            });
            serde_json::to_string(&events).unwrap_or_default()
        }
    }

    fn parse_network_config(
        raw: &str,
    ) -> Result<(Vec<String>, Option<ProtocolNetworkPolicyConfig>), String> {
        let value: serde_json::Value = serde_json::from_str(raw)
            .map_err(|error| format!("JSON must be an array or object: {error}"))?;
        let object = match value {
            serde_json::Value::Array(_) => {
                let allowed_hosts = serde_json::from_value(value)
                    .map_err(|error| format!("allowed_hosts must be a string array: {error}"))?;
                return Ok((allowed_hosts, None));
            }
            serde_json::Value::Object(object) => object,
            _ => return Err("network configuration must be an array or object".into()),
        };
        let has_old = object.contains_key("allowed_hosts");
        let has_nested = object.contains_key("network_policy");
        if has_old && has_nested {
            return Err("network_policy and allowed_hosts cannot both be configured".into());
        }
        if let Some(policy) = object.get("network_policy") {
            let policy = serde_json::from_value(policy.clone())
                .map_err(|error| format!("invalid network_policy: {error}"))?;
            return Ok((Vec::new(), Some(policy)));
        }
        if let Some(allowed_hosts) = object.get("allowed_hosts") {
            let allowed_hosts = serde_json::from_value(allowed_hosts.clone())
                .map_err(|error| format!("allowed_hosts must be a string array: {error}"))?;
            return Ok((allowed_hosts, None));
        }
        let policy = serde_json::from_value(serde_json::Value::Object(object))
            .map_err(|error| format!("invalid network_policy: {error}"))?;
        Ok((Vec::new(), Some(policy)))
    }
}
