mod common;

use common::{get_exit, get_stderr, get_stdout};
use wasmsh_protocol::HostCommand;
use wasmsh_runtime::{ExternalCommandResult, WorkerRuntime};

fn install_hostcat(rt: &mut WorkerRuntime) {
    rt.set_external_handler(Box::new(|name, argv, stdin| match name {
        "hostcat" => {
            let stdout = stdin
                .map(|mut stdin| {
                    let mut out = Vec::new();
                    let mut buffer = [0u8; 4096];
                    loop {
                        match stdin.read_chunk(&mut buffer) {
                            Ok(0) => break,
                            Ok(read) => out.extend_from_slice(&buffer[..read]),
                            Err(_) => return Vec::new(),
                        }
                    }
                    out
                })
                .unwrap_or_default();
            Some(ExternalCommandResult {
                stdout,
                stderr: Vec::new(),
                status: 0,
            })
        }
        "hostemit" => Some(ExternalCommandResult {
            stdout: b"OUT\n".to_vec(),
            stderr: b"ERR\n".to_vec(),
            status: 0,
        }),
        "hoststatus" => Some(ExternalCommandResult {
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: argv
                .get(1)
                .and_then(|value| value.parse().ok())
                .unwrap_or(1),
        }),
        _ => None,
    }));
}

#[test]
fn builtin_path_uses_same_io_redirection_model() {
    let mut rt = WorkerRuntime::new();
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });

    let events = rt.handle_command(HostCommand::Run {
        input: "printf hello > /out.txt; cat /out.txt".into(),
    });

    assert_eq!(get_stdout(&events), "hello");
}

#[test]
fn utility_path_uses_same_io_redirection_model() {
    let mut rt = WorkerRuntime::new();
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });
    rt.handle_command(HostCommand::WriteFile {
        path: "/in.txt".into(),
        data: b"input\n".to_vec(),
    });

    let events = rt.handle_command(HostCommand::Run {
        input: "cat < /in.txt".into(),
    });

    assert_eq!(get_stdout(&events), "input\n");
}

#[test]
fn external_path_uses_same_io_redirection_model() {
    let mut rt = WorkerRuntime::new();
    install_hostcat(&mut rt);
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });
    rt.handle_command(HostCommand::WriteFile {
        path: "/in.txt".into(),
        data: b"input\n".to_vec(),
    });

    let events = rt.handle_command(HostCommand::Run {
        input: "hostcat < /in.txt".into(),
    });

    assert_eq!(get_stdout(&events), "input\n");
}

#[test]
fn function_path_uses_same_io_redirection_model() {
    let mut rt = WorkerRuntime::new();
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });
    rt.handle_command(HostCommand::WriteFile {
        path: "/in.txt".into(),
        data: b"input\n".to_vec(),
    });

    let events = rt.handle_command(HostCommand::Run {
        input: "f(){ cat; }\nf < /in.txt > /out.txt; cat /out.txt".into(),
    });

    assert_eq!(get_stdout(&events), "input\n");
}

#[test]
fn mixed_pipeline_external_and_file_redirection_share_io_model() {
    let mut rt = WorkerRuntime::new();
    install_hostcat(&mut rt);
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });

    let events = rt.handle_command(HostCommand::Run {
        input: "printf hi | hostcat > /out.txt; cat /out.txt".into(),
    });

    assert_eq!(get_stdout(&events), "hi");
}

#[test]
fn three_stage_pipeline_and_pipe_statuses_include_external_stage() {
    let mut rt = WorkerRuntime::new();
    install_hostcat(&mut rt);
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });

    let events = rt.handle_command(HostCommand::Run {
        input:
            "printf hi | hostcat | wc -c; echo ${PIPESTATUS[0]} ${PIPESTATUS[1]} ${PIPESTATUS[2]}"
                .into(),
    });

    assert_eq!(get_stdout(&events), "2\n0 0 0\n");
    assert_eq!(get_exit(&events), 0);
}

#[test]
fn external_receives_here_doc_eof_and_binary_bytes_without_text_conversion() {
    let mut rt = WorkerRuntime::new();
    install_hostcat(&mut rt);
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });

    let here_doc = rt.handle_command(HostCommand::Run {
        input: "hostcat <<'EOF'\nline one\nline two\nEOF".into(),
    });
    assert_eq!(get_stdout(&here_doc), "line one\nline two\n");
    assert_eq!(get_exit(&here_doc), 0);

    rt.handle_command(HostCommand::WriteFile {
        path: "/binary".into(),
        data: vec![0, 1, 2, 0xff],
    });
    let binary = rt.handle_command(HostCommand::Run {
        input: "hostcat < /binary".into(),
    });
    let bytes: Vec<u8> = binary
        .iter()
        .filter_map(|event| match event {
            wasmsh_protocol::WorkerEvent::Stdout(data) => Some(data.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    assert_eq!(bytes, vec![0, 1, 2, 0xff]);
}

#[test]
fn external_nonzero_status_preserves_pipestatus_and_pipefail() {
    let mut rt = WorkerRuntime::new();
    install_hostcat(&mut rt);
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });

    let statuses = rt.handle_command(HostCommand::Run {
        input: "hoststatus 7 | hostcat; echo ${PIPESTATUS[0]} ${PIPESTATUS[1]}".into(),
    });
    assert_eq!(get_stdout(&statuses), "7 0\n");
    assert_eq!(get_exit(&statuses), 0);

    let pipefail = rt.handle_command(HostCommand::Run {
        input: "set -o pipefail; hoststatus 7 | hostcat".into(),
    });
    assert_eq!(get_exit(&pipefail), 7);
}

#[test]
fn external_input_limit_is_enforced_while_staging_a_pipeline() {
    let mut rt = WorkerRuntime::new();
    install_hostcat(&mut rt);
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });
    rt.set_external_input_byte_limit(3);

    let events = rt.handle_command(HostCommand::Run {
        input: "printf hello | hostcat".into(),
    });

    assert_eq!(get_stdout(&events), "");
    assert_eq!(get_exit(&events), 125);
    assert!(get_stderr(&events).contains("external stdin limit exceeded"));
}

#[test]
fn function_shadowing_utility_takes_precedence() {
    let mut rt = WorkerRuntime::new();
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });
    rt.handle_command(HostCommand::WriteFile {
        path: "/in.txt".into(),
        data: b"utility\n".to_vec(),
    });

    let events = rt.handle_command(HostCommand::Run {
        input: "cat(){ printf function; }\ncat < /in.txt > /out.txt; cat /out.txt".into(),
    });

    assert_eq!(get_stdout(&events), "function");
}

#[test]
fn builtin_keyword_bypasses_function_shadowing() {
    let mut rt = WorkerRuntime::new();
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });

    let events = rt.handle_command(HostCommand::Run {
        input: "printf(){ echo function; }\nprintf > /fn.txt; builtin printf builtin > /builtin.txt; cat /fn.txt; cat /builtin.txt".into(),
    });

    assert_eq!(get_stdout(&events), "function\nbuiltin");
}

#[test]
fn nounset_builtin_expansion_surfaces_error_through_vm_subset_path() {
    let mut rt = WorkerRuntime::new();
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });

    let events = rt.handle_command(HostCommand::Run {
        input: "set -u; echo $UNSET_VAR".into(),
    });

    assert_eq!(get_stdout(&events), "");
    assert!(
        get_stderr(&events).contains("UNSET_VAR: unbound variable"),
        "stderr = {:?}",
        get_stderr(&events)
    );
    assert_eq!(get_exit(&events), 1);
}

#[test]
fn nounset_assignment_expansion_surfaces_error_through_vm_subset_path() {
    let mut rt = WorkerRuntime::new();
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });

    let events = rt.handle_command(HostCommand::Run {
        input: "set -u; FOO=$UNSET_VAR".into(),
    });

    assert!(
        get_stderr(&events).contains("UNSET_VAR: unbound variable"),
        "stderr = {:?}",
        get_stderr(&events)
    );
    assert_eq!(get_exit(&events), 1);
}

#[test]
fn source_uses_redirected_io_and_preserves_shell_state() {
    let mut rt = WorkerRuntime::new();
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });
    rt.handle_command(HostCommand::WriteFile {
        path: "/lib.sh".into(),
        data: b"echo sourced\nX=loaded\n".to_vec(),
    });

    let events = rt.handle_command(HostCommand::Run {
        input: "source /lib.sh > /out.txt; cat /out.txt; echo $X".into(),
    });

    assert_eq!(get_stdout(&events), "sourced\nloaded\n");
}
