//! Spawns the Python BLPAPI sidecar and parses its response (spec A2 §4.2).
//!
//! Deliberately the same shape as the `refresh_driver` it replaces: spawn a
//! child, read a machine-readable exit code, take the last JSON line of stdout.
//! That pattern is the one part of the Excel design worth keeping — the risky
//! layer stays a small script a human can run by hand and watch.
//!
//! The request goes in on **stdin**, not argv: a 200-security batch would
//! exceed the Windows command-line length limit.

use crate::error::{AppError, AppResult};
use crate::fetch::{SidecarPayload, SidecarResponse};
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

/// Sidecar exit codes (see `scripts/blp_fetch.py`).
pub const EXIT_TIMEOUT: i32 = 2;
pub const EXIT_SESSION: i32 = 3;
pub const EXIT_BADINPUT: i32 = 4;

pub fn build_args(script: &Path, raw_out: Option<&Path>) -> Vec<String> {
    let mut args = vec![script.to_string_lossy().into_owned()];
    if let Some(p) = raw_out {
        args.push("--raw-out".into());
        args.push(p.to_string_lossy().into_owned());
    }
    args
}

/// The protocol is "last JSON line of stdout"; everything else on stdout is
/// noise and diagnostics belong on stderr. `log()` in the sidecar writes to
/// stderr only, so today exactly one line ever reaches stdout -- but nothing
/// enforces that upstream, and a stray future `print` must degrade into
/// "ignored noise" rather than "unparseable response" for either caller.
/// Both `run_fetch` and `run_raw` share this scan so that guarantee holds for
/// both, not just the one path someone remembered to test.
fn last_json_line<T: serde::de::DeserializeOwned>(stdout: &str) -> Option<T> {
    stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<T>(l.trim()).ok())
}

pub fn parse_response(stdout: &str) -> Option<SidecarResponse> {
    last_json_line(stdout)
}

/// Reject a response that reports success while saying nothing at all.
///
/// This is the lesson from `refresh.ps1`, which exited 0 after the refresh
/// macro threw and left a run looking healthy while ingesting nothing. A
/// well-formed request always yields either data or an explanation.
pub fn validate_response(resp: &SidecarResponse) -> Result<(), String> {
    if resp.status != "ok" {
        return Err(if resp.detail.is_empty() {
            format!("sidecar reported status '{}'", resp.status)
        } else {
            resp.detail.clone()
        });
    }
    if resp.observations.is_empty() && resp.problems.is_empty() {
        return Err("sidecar returned neither observations nor problems".into());
    }
    Ok(())
}

pub fn describe_exit(code: i32) -> &'static str {
    match code {
        EXIT_TIMEOUT => "request timed out",
        EXIT_SESSION => "session or service error (is the Terminal running and logged in?)",
        EXIT_BADINPUT => "malformed request rejected before sending",
        _ => "sidecar failed",
    }
}

/// Spawn the sidecar, write `payload` to stdin, apply the process's own exit
/// code as the success signal, and return stdout verbatim.
///
/// Shared by `run_fetch` and `run_raw`: both requests differ only in how the
/// stdout text is subsequently parsed, not in how the child process is run.
async fn run_sidecar_text(
    python: &Path,
    script: &Path,
    payload: &impl serde::Serialize,
    raw_out: Option<&Path>,
) -> AppResult<String> {
    let body = serde_json::to_vec(payload)
        .map_err(|e| AppError::Blp { code: -1, detail: format!("cannot serialize request: {e}") })?;

    let mut child = tokio::process::Command::new(python)
        .args(build_args(script, raw_out))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| AppError::Blp {
            code: -1,
            detail: format!("cannot start '{}': {e}", python.display()),
        })?;

    // Dropping stdin closes the pipe, which the sidecar needs in order for its
    // json.load(sys.stdin) to return.
    {
        let mut stdin = child.stdin.take().ok_or_else(|| AppError::Blp {
            code: -1,
            detail: "sidecar stdin unavailable".into(),
        })?;
        stdin.write_all(&body).await?;
        stdin.flush().await?;
    }

    let out = child.wait_with_output().await?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let code = out.status.code().unwrap_or(-1);

    if code != 0 {
        let detail = parse_response(&stdout)
            .map(|r| r.detail)
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| {
                let tail = stderr.trim();
                if tail.is_empty() { describe_exit(code).to_string() } else { tail.to_string() }
            });
        return Err(AppError::Blp { code, detail });
    }

    Ok(stdout)
}

pub async fn run_fetch(
    python: &Path,
    script: &Path,
    payload: &SidecarPayload,
    raw_out: Option<&Path>,
) -> AppResult<SidecarResponse> {
    let stdout = run_sidecar_text(python, script, payload, raw_out).await?;

    let resp = parse_response(&stdout).ok_or_else(|| AppError::Blp {
        code: 0,
        detail: "exit 0 but no JSON response on stdout".into(),
    })?;
    validate_response(&resp).map_err(|detail| AppError::Blp { code: 0, detail })?;
    Ok(resp)
}

/// Run the sidecar and return its response as untyped JSON.
///
/// `run_fetch` deserialises into SidecarResponse, which models observations.
/// The security-master requests return tables and search results instead, so
/// they read the JSON directly rather than widening SidecarResponse with
/// fields that mean nothing to an EOD run.
pub async fn run_raw(
    python_path: &Path,
    script_path: &Path,
    payload: &impl serde::Serialize,
) -> AppResult<serde_json::Value> {
    let text = run_sidecar_text(python_path, script_path, payload, None).await?;
    last_json_line(&text).ok_or_else(|| AppError::Sidecar(
        "sidecar returned invalid JSON: no JSON line found on stdout".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::SidecarResponse;
    use std::path::Path;

    fn resp(json: &str) -> SidecarResponse {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn args_carry_script_and_optional_raw_out() {
        let a = build_args(Path::new("C:\\app\\blp_fetch.py"),
                           Path::new("C:\\bloomdata\\archive\\r1.json").into());
        assert_eq!(a[0], "C:\\app\\blp_fetch.py");
        assert!(a.contains(&"--raw-out".to_string()));
        assert!(a.contains(&"C:\\bloomdata\\archive\\r1.json".to_string()));

        let b = build_args(Path::new("blp_fetch.py"), None);
        assert_eq!(b, vec!["blp_fetch.py".to_string()]);
    }

    #[test]
    fn parses_last_stdout_line_ignoring_noise() {
        let out = "  -> history: 2 securities x 2 fields\n\
                   {\"status\":\"ok\",\"seconds\":1.5,\"detail\":\"\",\
                   \"observations\":[{\"security\":\"AAPL US Equity\",\"field\":\"PX_LAST\",\
                   \"date\":\"2026-08-17\",\"num\":305.59}],\"problems\":[]}\n";
        let r = parse_response(out).unwrap();
        assert_eq!(r.status, "ok");
        assert_eq!(r.observations.len(), 1);
        assert_eq!(r.observations[0].num, Some(305.59));
        assert!(parse_response("no json here").is_none());
    }

    #[test]
    fn silence_is_never_success() {
        // The refresh.ps1 failure mode, translated: exit 0, status ok, and
        // absolutely nothing to show for it.
        let empty = resp(r#"{"status":"ok","observations":[],"problems":[]}"#);
        assert!(validate_response(&empty).is_err());

        // Problems alone ARE a valid outcome -- a full holiday, for instance.
        let holiday = resp(r#"{"status":"ok","observations":[],"problems":[
            {"security":"AAPL US Equity","field":"PX_LAST","date":"2026-08-17",
             "code":"no_data","detail":""}]}"#);
        assert!(validate_response(&holiday).is_ok());
    }

    #[test]
    fn non_ok_status_carries_its_detail() {
        let e = resp(r#"{"status":"session-error","detail":"session.start() failed",
                        "observations":[],"problems":[]}"#);
        assert_eq!(validate_response(&e).unwrap_err(), "session.start() failed");

        let bare = resp(r#"{"status":"timeout","observations":[],"problems":[]}"#);
        assert!(validate_response(&bare).unwrap_err().contains("timeout"));
    }

    #[test]
    fn exit_codes_are_described() {
        assert!(describe_exit(EXIT_SESSION).contains("Terminal"));
        assert!(describe_exit(EXIT_TIMEOUT).contains("timed out"));
        assert!(describe_exit(EXIT_BADINPUT).contains("malformed"));
    }
}
