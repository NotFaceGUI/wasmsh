//! Regression coverage for `>(cmd)` output process substitution.
//!
//! A reproduction reported that `tee >(wc -c > file) <<< hi` panicked with
//! `unreachable!("buffered pipeline stage requires runtime access")` and then
//! permanently poisoned the `WasmShell` object. The trigger is a pipeline whose
//! stage is runtime-driven (a redirect, a compound command, or an external
//! command) while no isolated process-substitution runtime can be cloned — which
//! is exactly the standalone WASM configuration, because `WasmShell::new`
//! always installs an external spec handler.
//!
//! These tests run under that configuration: an external handler is registered,
//! so `clone_for_isolated_process_subst` returns `None`.

mod common;

use common::{get_exit, get_stdout};
use wasmsh_protocol::HostCommand;
use wasmsh_runtime::WorkerRuntime;

/// A shell configured like the standalone WASM `WasmShell`: an external spec
/// handler is always installed, which makes an isolated process-substitution
/// runtime unavailable.
fn shell_with_external_host() -> WorkerRuntime {
    let mut rt = WorkerRuntime::new();
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });
    rt.set_external_handler(Box::new(|_name, _argv, _stdin| None));
    rt
}

fn run(rt: &mut WorkerRuntime, script: &str) -> (String, i32) {
    let events = rt.handle_command(HostCommand::Run {
        input: script.to_string(),
    });
    (get_stdout(&events), get_exit(&events))
}

// --- the three reported panic triggers ---

#[test]
fn tee_with_inner_redirect_does_not_panic() {
    let mut rt = shell_with_external_host();
    let (out, status) = run(&mut rt, "tee >(wc -c > /tmp/_ps.txt) <<< hi\n");
    assert_eq!(status, 0, "stdout={out:?}");
}

#[test]
fn tee_with_inner_file_redirect_does_not_panic() {
    let mut rt = shell_with_external_host();
    let (out, status) = run(&mut rt, "tee >(cat > /tmp/_o.txt) <<< hi\n");
    assert_eq!(status, 0, "stdout={out:?}");
}

#[test]
fn inner_redirect_consumer_writes_its_file() {
    let mut rt = shell_with_external_host();
    let (out, status) = run(
        &mut rt,
        "echo hi > >(wc -c > /tmp/_count.txt)\necho \"F=[$(cat /tmp/_count.txt)]\"\n",
    );
    assert_eq!(status, 0, "stdout={out:?}");
    assert_eq!(
        out, "F=[3]\n",
        "the consumer's redirect must reach its file"
    );
}

// --- the cases that already worked must keep working ---

#[test]
fn input_process_substitution_still_works() {
    let mut rt = shell_with_external_host();
    let (out, status) = run(&mut rt, "cat <(echo ok)\n");
    assert_eq!(status, 0, "stdout={out:?}");
    assert_eq!(out, "ok\n");
}

#[test]
fn plain_output_process_substitution_still_works() {
    let mut rt = shell_with_external_host();
    let (out, status) = run(&mut rt, "echo hi > >(cat)\n");
    assert_eq!(status, 0, "stdout={out:?}");
    assert_eq!(out, "hi\n");
}

#[test]
fn tee_streaming_consumer_receives_both_copies() {
    let mut rt = shell_with_external_host();
    let (out, status) = run(&mut rt, "tee >(cat) <<< data\n");
    assert_eq!(status, 0, "stdout={out:?}");
    // tee echoes once, and the consumer `cat` writes the second copy.
    assert_eq!(out, "data\ndata\n");
}

// --- a utility that opens the substitution path directly must still feed it ---

#[test]
fn tee_inner_redirect_consumer_writes_its_file() {
    let mut rt = shell_with_external_host();
    let (out, status) = run(
        &mut rt,
        "tee >(wc -c > /tmp/_t1.txt) <<< hi\necho \"t1=[$(cat /tmp/_t1.txt)]\"\n",
    );
    assert_eq!(status, 0, "stdout={out:?}");
    assert_eq!(out, "hi\nt1=[3]\n");
}

#[test]
fn tee_inner_cat_redirect_consumer_writes_its_file() {
    let mut rt = shell_with_external_host();
    let (out, status) = run(
        &mut rt,
        "tee >(cat > /tmp/_t2.txt) <<< hi\necho \"t2=[$(cat /tmp/_t2.txt)]\"\n",
    );
    assert_eq!(status, 0, "stdout={out:?}");
    assert_eq!(out, "hi\nt2=[hi]\n");
}

// --- the instance must stay usable after every trigger ---

#[test]
fn instance_is_reusable_after_output_process_substitution() {
    let mut rt = shell_with_external_host();
    for script in [
        "tee >(wc -c > /tmp/_ps1.txt) <<< hi\n",
        "tee >(cat > /tmp/_ps2.txt) <<< hi\n",
        "echo hi > >(wc -c > /tmp/_ps3.txt)\n",
    ] {
        let (_, status) = run(&mut rt, script);
        assert_eq!(status, 0, "script={script:?}");
    }
    let (out, status) = run(&mut rt, "echo alive\n");
    assert_eq!(status, 0);
    assert_eq!(out, "alive\n");
}

#[test]
fn input_procsub_non_trivial_first_stage() {
    let mut rt = shell_with_external_host();
    for (script, want) in [
        (
            "printf 'a\nb\n' > /tmp/i.txt
cat <(sed 's/a/A/' /tmp/i.txt)
",
            "A
b
",
        ),
        (
            "printf 'a\nb\n' > /tmp/i2.txt
cat <(grep a /tmp/i2.txt)
",
            "a
",
        ),
        (
            "printf 'b\na\n' > /tmp/i3.txt
cat <(sort /tmp/i3.txt)
",
            "a
b
",
        ),
        (
            "printf 'a\nb\n' > /tmp/i4.txt
cat <(head -1 /tmp/i4.txt)
",
            "a
",
        ),
    ] {
        let (out, status) = run(&mut rt, script);
        assert_eq!(status, 0, "script={script:?}");
        assert_eq!(out, want, "script={script:?}");
    }
}
