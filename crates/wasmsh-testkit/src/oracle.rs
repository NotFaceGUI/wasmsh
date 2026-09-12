//! Oracle comparison: run shell scripts against local reference shells.
//!
//! Oracle mode is **on by default** whenever a reference shell can be
//! discovered. Set `WASMSH_ORACLE=0` (or `false`/`no`/`off`) to disable it
//! globally in constrained environments.
//!
//! Shell discovery:
//! - `WASMSH_ORACLE_BASH` names an explicit interpreter (used for the
//!   `bash` oracle); useful on Windows where `bash` is not on `PATH`.
//! - Otherwise the shell name is resolved on `PATH`, and on Windows a few
//!   well-known Git-for-Windows install locations are probed as a fallback.
//!
//! When a requested shell cannot be found the caller must surface a visible
//! SKIP; we never silently treat "no oracle" as a pass.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Result from an oracle shell execution.
#[derive(Debug)]
pub struct OracleResult {
    pub shell: String,
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Outcome of attempting to run a script against one reference shell.
#[derive(Debug)]
pub enum OracleOutcome {
    /// The shell ran; compare the result.
    Ran(OracleResult),
    /// Oracle comparison is globally disabled via `WASMSH_ORACLE=0`.
    Disabled,
    /// The requested shell could not be found. Callers must report a SKIP.
    Skipped { shell: String, reason: String },
}

/// Whether oracle comparison is enabled (default: yes).
#[must_use]
pub fn oracle_enabled() -> bool {
    match std::env::var("WASMSH_ORACLE") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

/// Resolve a shell name to an executable path.
///
/// `WASMSH_ORACLE_BASH` overrides the `bash` oracle only. Absolute or
/// relative paths containing a separator are used verbatim.
pub fn resolve_shell(shell: &str) -> Result<PathBuf, String> {
    if shell == "bash" {
        if let Ok(explicit) = std::env::var("WASMSH_ORACLE_BASH") {
            let explicit = explicit.trim();
            if !explicit.is_empty() {
                let p = PathBuf::from(explicit);
                if is_executable(&p) {
                    return Ok(p);
                }
                return Err(format!(
                    "WASMSH_ORACLE_BASH={explicit:?} is not an executable file"
                ));
            }
        }
    }

    if shell.contains('/') || shell.contains('\\') {
        let p = PathBuf::from(shell);
        return if is_executable(&p) {
            Ok(p)
        } else {
            Err(format!("{shell:?} is not an executable file"))
        };
    }

    if let Some(found) = search_path(shell) {
        return Ok(found);
    }

    #[cfg(windows)]
    if shell == "bash" {
        for candidate in windows_bash_candidates() {
            if is_executable(&candidate) {
                return Ok(candidate);
            }
        }
    }

    Err(format!(
        "{shell:?} not found on PATH (set WASMSH_ORACLE_BASH to point at a bash.exe)"
    ))
}

fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn search_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let direct = dir.join(name);
        if is_executable(&direct) {
            return Some(direct);
        }
        #[cfg(windows)]
        {
            for ext in ["exe", "cmd", "bat"] {
                let with_ext = dir.join(format!("{name}.{ext}"));
                if is_executable(&with_ext) {
                    return Some(with_ext);
                }
            }
        }
    }
    None
}

#[cfg(windows)]
fn windows_bash_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for base in [r"C:\Program Files\Git", r"C:\Program Files (x86)\Git"] {
        out.push(PathBuf::from(base).join("bin").join("bash.exe"));
        out.push(PathBuf::from(base).join("usr").join("bin").join("bash.exe"));
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let base = PathBuf::from(local).join("Programs").join("Git");
        out.push(base.join("bin").join("bash.exe"));
        out.push(base.join("usr").join("bin").join("bash.exe"));
    }
    out
}

/// Run a script against a reference shell.
///
/// Returns [`OracleOutcome::Disabled`] when oracle mode is off, or
/// [`OracleOutcome::Skipped`] when the shell cannot be resolved. The script
/// runs in a private temporary directory so relative-path side effects do not
/// leak into the test workspace, and so the oracle side effects are isolated
/// from any other concurrent case.
pub fn run_oracle(script: &str, shell: &str) -> OracleOutcome {
    if !oracle_enabled() {
        return OracleOutcome::Disabled;
    }

    let exe = match resolve_shell(shell) {
        Ok(p) => p,
        Err(reason) => {
            return OracleOutcome::Skipped {
                shell: shell.to_string(),
                reason,
            }
        }
    };

    let workdir = match TempDir::new() {
        Ok(d) => d,
        Err(e) => {
            return OracleOutcome::Skipped {
                shell: shell.to_string(),
                reason: format!("could not create oracle tempdir: {e}"),
            }
        }
    };

    let output = Command::new(&exe)
        .arg("-c")
        .arg(script)
        .current_dir(workdir.path())
        .envs(oracle_env(shell))
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            return OracleOutcome::Skipped {
                shell: shell.to_string(),
                reason: format!("failed to spawn {}: {e}", exe.display()),
            }
        }
    };

    OracleOutcome::Ran(OracleResult {
        shell: exe.display().to_string(),
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

/// Environment overrides for the reference shell.
///
/// On Windows, Git Bash needs `MSYS=winsymlinks:lnk` to create real symbolic
/// links instead of silently copying; without it any `ln -s` differential
/// case would compare against a copy and give a false result.
fn oracle_env(_shell: &str) -> Vec<(&'static str, &'static str)> {
    if cfg!(windows) {
        vec![("MSYS", "winsymlinks:lnk")]
    } else {
        Vec::new()
    }
}

/// Compare wasmsh output against oracle output.
pub fn compare_oracle(
    wasmsh_status: i32,
    wasmsh_stdout: &str,
    wasmsh_stderr: &str,
    oracle: &OracleResult,
    ignore_stderr: bool,
) -> Vec<String> {
    let mut diffs = Vec::new();

    if wasmsh_status != oracle.status {
        diffs.push(format!(
            "[{}] status: wasmsh={}, oracle={}",
            oracle.shell, wasmsh_status, oracle.status
        ));
    }

    if wasmsh_stdout != oracle.stdout {
        diffs.push(format!(
            "[{}] stdout differs:\n  wasmsh: {:?}\n  oracle: {:?}",
            oracle.shell, wasmsh_stdout, oracle.stdout
        ));
    }

    if !ignore_stderr && wasmsh_stderr != oracle.stderr {
        diffs.push(format!(
            "[{}] stderr differs:\n  wasmsh: {:?}\n  oracle: {:?}",
            oracle.shell, wasmsh_stderr, oracle.stderr
        ));
    }

    diffs
}

/// Minimal scoped temporary directory (no external crate).
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("wasmsh-oracle-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_detects_status_stdout_stderr() {
        let oracle = OracleResult {
            shell: "stub".into(),
            status: 1,
            stdout: "a\n".into(),
            stderr: "e\n".into(),
        };
        let diffs = compare_oracle(0, "b\n", "f\n", &oracle, false);
        assert_eq!(diffs.len(), 3, "{diffs:?}");
        let diffs = compare_oracle(0, "b\n", "f\n", &oracle, true);
        assert_eq!(diffs.len(), 2, "{diffs:?}");
    }

    #[test]
    fn resolve_missing_shell_is_error() {
        // A name that cannot plausibly exist on PATH.
        let err = resolve_shell("wasmsh-definitely-not-a-shell-xyz").unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn resolve_bash_from_env_override() {
        // Only meaningful when bash exists; assert the resolution honours the
        // override for an existing interpreter path.
        let candidate = if cfg!(windows) {
            "C:\\Windows\\System32\\cmd.exe"
        } else {
            "/bin/sh"
        };
        let p = PathBuf::from(candidate);
        if p.is_file() {
            std::env::set_var("WASMSH_ORACLE_BASH", candidate);
            let resolved = resolve_shell("bash").expect("override resolves");
            assert_eq!(resolved, p);
            std::env::remove_var("WASMSH_ORACLE_BASH");
        }
    }
}
