use crate::error::{AppError, AppResult};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct RefreshStatus {
    pub status: String,
    #[serde(default)]
    pub seconds: f64,
    #[serde(default)]
    pub detail: String,
}

pub fn build_ps_args(script: &Path, workbook: &Path, timeout_s: u32, dry_run: bool) -> Vec<String> {
    let mut args = vec![
        "-NoProfile".into(),
        "-ExecutionPolicy".into(), "Bypass".into(),
        "-File".into(), script.to_string_lossy().into_owned(),
        "-WorkbookPath".into(), workbook.to_string_lossy().into_owned(),
        "-TimeoutSeconds".into(), timeout_s.to_string(),
    ];
    if dry_run {
        args.push("-DryRun".into());
    }
    args
}

pub fn parse_status(stdout: &str) -> Option<RefreshStatus> {
    stdout.lines().rev()
        .find_map(|l| serde_json::from_str::<RefreshStatus>(l.trim()).ok())
}

pub async fn run_refresh(
    script: &Path, workbook: &Path, timeout_s: u32, dry_run: bool,
) -> AppResult<RefreshStatus> {
    let out = tokio::process::Command::new("powershell.exe")
        .args(build_ps_args(script, workbook, timeout_s, dry_run))
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let code = out.status.code().unwrap_or(-1);
    if code == 0 {
        parse_status(&stdout).ok_or_else(|| AppError::Refresh {
            code: 0, detail: "exit 0 but no JSON status on stdout".into() })
    } else {
        let detail = parse_status(&stdout)
            .map(|s| s.detail)
            .filter(|d| !d.is_empty())
            .unwrap_or(stderr);
        Err(AppError::Refresh { code, detail })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn args_include_paths_timeout_and_flags() {
        let a = build_ps_args(Path::new("C:\\app\\refresh.ps1"),
                              Path::new("C:\\pending\\wb.xlsx"), 300, true);
        assert_eq!(a[0], "-NoProfile");
        assert!(a.contains(&"-ExecutionPolicy".to_string()));
        assert!(a.contains(&"-File".to_string()));
        assert!(a.contains(&"C:\\app\\refresh.ps1".to_string()));
        assert!(a.contains(&"-WorkbookPath".to_string()));
        assert!(a.contains(&"C:\\pending\\wb.xlsx".to_string()));
        assert!(a.contains(&"-TimeoutSeconds".to_string()));
        assert!(a.contains(&"300".to_string()));
        assert!(a.contains(&"-DryRun".to_string()));
        let b = build_ps_args(Path::new("s.ps1"), Path::new("w.xlsx"), 300, false);
        assert!(!b.contains(&"-DryRun".to_string()));
    }

    #[test]
    fn parses_last_stdout_line_as_status() {
        let out = "noise from addin\n{\"status\":\"ok\",\"seconds\":42.5,\"detail\":\"\"}\n";
        let s = parse_status(out).unwrap();
        assert_eq!(s.status, "ok");
        assert_eq!(s.seconds, 42.5);
        assert!(parse_status("no json here").is_none());
    }
}
