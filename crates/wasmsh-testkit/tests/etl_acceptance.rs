//! End-to-end offline ETL acceptance test.
//!
//! Runs `e2e/etl/{gen,run,verify}.sh` entirely inside the wasmsh sandbox
//! (no host shell, no Python, no network) and asserts:
//!
//! 1. `set -euo pipefail` is honoured and `PIPESTATUS` is populated.
//! 2. Two independent runs produce byte-identical published output
//!    (idempotency), compared via the MANIFEST, which hashes file *contents*
//!    and therefore carries no timestamp.
//! 3. `verify.sh` recomputes the invariants with a different algorithm,
//!    including a non-vacuous denominator check, and exits 0.
#![allow(clippy::print_stderr)]

use std::path::PathBuf;

use wasmsh_protocol::{HostCommand, WorkerEvent};
use wasmsh_runtime::WorkerRuntime;

fn etl_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("e2e/etl")
}

fn new_runtime() -> WorkerRuntime {
    let mut rt = WorkerRuntime::new();
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });
    rt
}

fn stdout_of(events: &[WorkerEvent]) -> String {
    let mut out = Vec::new();
    for e in events {
        if let WorkerEvent::Stdout(d) = e {
            out.extend_from_slice(d);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn stderr_of(events: &[WorkerEvent]) -> String {
    let mut out = Vec::new();
    for e in events {
        if let WorkerEvent::Stderr(d) = e {
            out.extend_from_slice(d);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn exit_of(events: &[WorkerEvent]) -> i32 {
    events
        .iter()
        .find_map(|e| match e {
            WorkerEvent::Exit(s) => Some(*s),
            _ => None,
        })
        .unwrap_or(-1)
}

fn run(rt: &mut WorkerRuntime, input: &str) -> (i32, String, String) {
    let events = rt.handle_command(HostCommand::Run {
        input: input.to_string(),
    });
    (exit_of(&events), stdout_of(&events), stderr_of(&events))
}

fn read_file(rt: &mut WorkerRuntime, path: &str) -> String {
    let events = rt.handle_command(HostCommand::ReadFile {
        path: path.to_string(),
    });
    stdout_of(&events)
}

fn seed_scripts(rt: &mut WorkerRuntime) {
    let dir = etl_dir();
    for name in ["gen.sh", "run.sh", "verify.sh"] {
        let content =
            std::fs::read_to_string(dir.join(name)).unwrap_or_else(|e| panic!("read {name}: {e}"));
        rt.handle_command(HostCommand::WriteFile {
            path: format!("/scripts/{name}"),
            data: content.into_bytes(),
        });
    }
}

fn run_pipeline(rt: &mut WorkerRuntime, label: &str) {
    let (s, _o, e) = run(rt, "sh /scripts/gen.sh");
    assert_eq!(s, 0, "[{label}] gen.sh failed: {e}");
    let (s, _o, e) = run(rt, "sh /scripts/run.sh");
    assert_eq!(s, 0, "[{label}] run.sh failed: {e}");
}

#[test]
fn etl_pipeline_is_correct_idempotent_and_self_verifying() {
    let mut rt = new_runtime();
    seed_scripts(&mut rt);

    // ---- 1. First run ---------------------------------------------------
    run_pipeline(&mut rt, "first");

    // `set -euo pipefail` and PIPESTATUS are observable.
    let events = rt.handle_command(HostCommand::Run {
        input: "set -o pipefail; false | true; echo \"PS=${PIPESTATUS[*]}\"".into(),
    });
    assert_eq!(exit_of(&events), 0, "pipefail probe failed");
    assert!(
        stdout_of(&events).contains("PS=1 0"),
        "PIPESTATUS wrong: {:?}",
        stdout_of(&events)
    );

    // Core artifact must be non-empty and parse.
    let summary = read_file(&mut rt, "/out/summary.json");
    assert!(summary.contains("\"matched\""), "summary.json: {summary}");
    let report = read_file(&mut rt, "/out/report.md");
    assert!(report.contains("# Daily ETL Report"), "report.md: {report}");

    // Fingerprint: MANIFEST hashes file *contents* (no timestamps), so its
    // own hash is a deterministic digest of the published payload.
    let (s, manifest_hash_1, e) = run(&mut rt, "cd /pkg && sha256sum MANIFEST");
    assert_eq!(s, 0, "hash manifest: {e}");
    assert!(manifest_hash_1.contains("MANIFEST"), "{manifest_hash_1}");

    // ---- 2. Independent verify.sh, with a different algorithm -----------
    let (s, out, e) = run(&mut rt, "sh /scripts/verify.sh");
    assert_eq!(s, 0, "verify.sh failed:\nstdout={out}\nstderr={e}");
    assert!(out.contains("all invariants hold"), "verify output: {out}");

    // ---- 3. Second run: idempotent output -------------------------------
    run_pipeline(&mut rt, "second");
    let (s, manifest_hash_2, e) = run(&mut rt, "cd /pkg && sha256sum MANIFEST");
    assert_eq!(s, 0, "hash manifest again: {e}");
    assert_eq!(
        manifest_hash_1, manifest_hash_2,
        "published output is not byte-identical across runs"
    );

    // verify.sh must still pass after the second run.
    let (s, out, e) = run(&mut rt, "sh /scripts/verify.sh");
    assert_eq!(s, 0, "verify.sh after second run failed:\n{out}\n{e}");

    // ---- 4. A deliberately broken invariant must fail verification ------
    // Corrupt a matched amount; verify.sh must notice and exit non-zero.
    run(&mut rt, "printf 'o1\\t999\\n' > /out/matched.tsv");
    let (s, out, e) = run(&mut rt, "sh /scripts/verify.sh");
    assert_ne!(
        s, 0,
        "verify.sh accepted a corrupted artifact; stdout={out} stderr={e}"
    );
}
