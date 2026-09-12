//! Development-only harness: run a shell script through the in-process
//! wasmsh runtime and mirror stdout/stderr/exit status to the host process.
//!
//! Usage:
//!   wasmsh-dev <script-file> [--dump]
//!   wasmsh-dev -            (read script from stdin)
//!
//! `--dump` prints every VFS file (recursively) to stderr after execution,
//! which makes it easy to inspect redirection / symlink / archive effects.
//!
//! This binary is a developer aid for the differential test workflow; it is
//! not part of the sandbox's public surface.
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::exit)]

use std::io::Read;
use std::io::Write as _;

use wasmsh_protocol::{HostCommand, WorkerEvent};
use wasmsh_runtime::WorkerRuntime;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut path: Option<String> = None;
    let mut dump = false;
    let mut seed_dir: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dump" => dump = true,
            "--seed-dir" => seed_dir = args.next(),
            other => path = Some(other.to_string()),
        }
    }

    let Some(path) = path else {
        eprintln!("usage: wasmsh-dev <script-file|-> [--dump] [--seed-dir DIR]");
        std::process::exit(2);
    };

    let script = if path == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .expect("read stdin");
        buf
    } else {
        std::fs::read_to_string(&path).expect("read script file")
    };

    let mut rt = WorkerRuntime::new();
    rt.handle_command(HostCommand::Init {
        step_budget: 0,
        allowed_hosts: vec![],
        network_policy: None,
    });

    if let Some(dir) = seed_dir {
        seed_dir_into(&mut rt, std::path::Path::new(&dir));
    }

    let events = rt.handle_command(HostCommand::Run { input: script });

    let mut status = 0i32;
    for event in &events {
        match event {
            WorkerEvent::Stdout(data) => {
                std::io::stdout().write_all(data).expect("write stdout");
            }
            WorkerEvent::Stderr(data) => {
                std::io::stderr().write_all(data).expect("write stderr");
            }
            WorkerEvent::Exit(code) => status = *code,
            _ => {}
        }
    }
    std::io::stdout().flush().ok();

    if dump {
        eprintln!("=== VFS DUMP ===");
        dump_dir(&mut rt, "/");
    }

    std::process::exit(status);
}

fn seed_dir_into(rt: &mut WorkerRuntime, dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(data) = std::fs::read(&p) else {
            continue;
        };
        rt.handle_command(HostCommand::WriteFile {
            path: format!("/scripts/{name}"),
            data,
        });
    }
}

fn dump_dir(rt: &mut WorkerRuntime, dir: &str) {
    let events = rt.handle_command(HostCommand::ListDir {
        path: dir.to_string(),
    });
    let mut entries: Vec<String> = Vec::new();
    for event in &events {
        if let WorkerEvent::Stdout(data) = event {
            for line in String::from_utf8_lossy(data).lines() {
                entries.push(line.to_string());
            }
        }
    }
    for entry in entries {
        if entry.is_empty() {
            continue;
        }
        let full = if dir == "/" {
            format!("/{entry}")
        } else {
            format!("{dir}/{entry}")
        };
        let probe = rt.handle_command(HostCommand::ReadFile { path: full.clone() });
        let mut is_dir = false;
        let mut content = Vec::new();
        for event in &probe {
            match event {
                WorkerEvent::Stdout(data) => content.extend_from_slice(data),
                WorkerEvent::Diagnostic(_, _) => is_dir = true,
                _ => {}
            }
        }
        if is_dir {
            eprintln!("[dir] {full}");
            dump_dir(rt, &full);
        } else {
            eprintln!("[file] {full} ({} bytes)", content.len());
            eprintln!("{}", String::from_utf8_lossy(&content));
        }
    }
}
