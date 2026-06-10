//! Launch the `zed` CLI — the I/O backend for the `openFile` MCP tool.
//!
//! Layer position: between `mcp` and `transport`; depends only on
//! `protocol`. All process-spawning I/O for driving the Zed editor lives
//! here and nowhere else.
//!
//! Capability notes (Zed 1.5 CLI): `zed -e <path>[:line:col]` opens a file
//! in an existing window and positions the cursor (1-indexed). It cannot
//! set selections or open without focusing — see `OpenFileArgs`' docs for
//! how the unsupported wire fields degrade.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;

use crate::protocol::{CallToolResult, OpenFileArgs, ToolContent};

/// Default editor binary, resolved via `PATH`.
pub const DEFAULT_ZED_BIN: &str = "zed";

/// How long we wait for the `zed` CLI to exit (it hands off to the app and
/// returns quickly; 5s is generous).
const ZED_SPAWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors from driving the `zed` CLI.
#[derive(Debug, thiserror::Error)]
pub enum ZedCliError {
    /// The binary could not be spawned (missing from PATH, not executable).
    #[error("failed to spawn {bin}: {source}")]
    Spawn {
        /// Binary name or path we tried to run.
        bin: String,
        /// Underlying OS error.
        source: std::io::Error,
    },
    /// The CLI ran but exited non-zero.
    #[error("{bin} exited with status {status}")]
    NonZero {
        /// Binary name or path.
        bin: String,
        /// The non-success exit status.
        status: std::process::ExitStatus,
    },
    /// The CLI did not exit within [`ZED_SPAWN_TIMEOUT`].
    #[error("{bin} timed out after {timeout_ms} ms")]
    Timeout {
        /// Binary name or path.
        bin: String,
        /// The timeout that elapsed, in milliseconds.
        timeout_ms: u64,
    },
}

/// Locate `needle` in `haystack`; return the 1-indexed (line, byte-column)
/// of the match START. Byte columns are exact for ASCII and a close
/// approximation otherwise (Zed treats the column as a character offset).
pub fn locate_text(haystack: &str, needle: &str) -> Option<(u32, u32)> {
    let idx = haystack.find(needle)?;
    let before = &haystack[..idx];
    let line = before.bytes().filter(|b| *b == b'\n').count() as u32 + 1;
    let line_start = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
    let col = (idx - line_start) as u32 + 1;
    Some((line, col))
}

/// Build the `zed -e` path-spec argument: `path[:line:col]`.
pub fn path_spec(path: &Path, position: Option<(u32, u32)>) -> String {
    match position {
        Some((line, col)) => format!("{}:{line}:{col}", path.display()),
        None => path.display().to_string(),
    }
}

/// Resolve `file_path` against `base` when relative (VSCode parity: the
/// base is the first workspace folder).
pub fn resolve_path(file_path: &str, base: Option<&Path>) -> PathBuf {
    let p = Path::new(file_path);
    if p.is_absolute() {
        p.to_path_buf()
    } else if let Some(b) = base {
        b.join(p)
    } else {
        p.to_path_buf()
    }
}

/// Execute an `openFile` request end to end: resolve the path, check
/// existence, locate `startText` (if any), spawn `bin -e <spec>`, and build
/// a [`CallToolResult`] mirroring the VSCode extension's response shapes.
pub async fn open_file(
    bin: &str,
    args: &OpenFileArgs,
    workspace_base: Option<&Path>,
) -> CallToolResult {
    let path = resolve_path(&args.file_path, workspace_base);
    if !path.is_file() {
        return text_result(
            json!({
                "success": false,
                "message": format!("File not found: {}", path.display()),
            })
            .to_string(),
        );
    }

    // startText → 1-indexed cursor position. Read failures degrade to a
    // plain open at the path (tracked so the message stays honest).
    let mut text_missing = false;
    let position = match &args.start_text {
        Some(t) => match tokio::fs::read_to_string(&path).await {
            Ok(contents) => {
                let pos = locate_text(&contents, t);
                text_missing = pos.is_none();
                pos
            }
            Err(_) => {
                text_missing = true;
                None
            }
        },
        None => None,
    };

    let spec = path_spec(&path, position);
    if let Err(e) = run_zed(bin, &spec).await {
        return text_result(
            json!({
                "success": false,
                "message": format!("Failed to launch zed: {e}"),
            })
            .to_string(),
        );
    }

    let message = match (&args.start_text, position) {
        (Some(t), Some(_)) => format!("Opened file and positioned at \"{t}\""),
        (Some(t), None) if text_missing => {
            format!("Opened file, but text \"{t}\" not found")
        }
        (Some(t), None) => format!("Opened file, but text \"{t}\" could not be located"),
        (None, _) => json!({
            "success": true,
            "filePath": path.display().to_string(),
            "fileUrl": format!("file://{}", path.display()),
            "message": format!("Opened file: {}", path.display()),
        })
        .to_string(),
    };
    text_result(message)
}

/// Spawn `bin -e <spec>` and wait (bounded) for it to exit successfully.
async fn run_zed(bin: &str, spec: &str) -> Result<(), ZedCliError> {
    let mut child = tokio::process::Command::new(bin)
        .arg("-e")
        .arg(spec)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| ZedCliError::Spawn {
            bin: bin.to_string(),
            source: e,
        })?;
    match tokio::time::timeout(ZED_SPAWN_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(ZedCliError::NonZero {
            bin: bin.to_string(),
            status,
        }),
        Ok(Err(e)) => Err(ZedCliError::Spawn {
            bin: bin.to_string(),
            source: e,
        }),
        Err(_) => {
            let _ = child.start_kill();
            Err(ZedCliError::Timeout {
                bin: bin.to_string(),
                timeout_ms: ZED_SPAWN_TIMEOUT.as_millis() as u64,
            })
        }
    }
}

fn text_result(text: String) -> CallToolResult {
    CallToolResult {
        content: vec![ToolContent::Text { text }],
        is_error: None,
    }
}

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    // ----- locate_text ----------------------------------------------------

    #[test]
    fn locate_text_first_line() {
        assert_eq!(locate_text("fn main() {}", "fn main"), Some((1, 1)));
    }

    #[test]
    fn locate_text_later_line_and_column() {
        let s = "fn a() {}\nfn main() {}\n";
        assert_eq!(locate_text(s, "fn main"), Some((2, 1)));
        assert_eq!(locate_text(s, "main"), Some((2, 4)));
    }

    #[test]
    fn locate_text_not_found_and_empty() {
        assert_eq!(locate_text("abc", "zzz"), None);
        assert_eq!(locate_text("", "x"), None);
    }

    // ----- path_spec / resolve_path ----------------------------------------

    #[test]
    fn path_spec_with_and_without_position() {
        let p = Path::new("/a/b.rs");
        assert_eq!(path_spec(p, Some((2, 4))), "/a/b.rs:2:4");
        assert_eq!(path_spec(p, None), "/a/b.rs");
    }

    #[test]
    fn resolve_path_relative_joins_base_absolute_passes_through() {
        assert_eq!(
            resolve_path("src/x.rs", Some(Path::new("/w"))),
            PathBuf::from("/w/src/x.rs")
        );
        assert_eq!(
            resolve_path("/abs/x.rs", Some(Path::new("/w"))),
            PathBuf::from("/abs/x.rs")
        );
        assert_eq!(resolve_path("rel.rs", None), PathBuf::from("rel.rs"));
    }

    // ----- open_file with a fake binary ------------------------------------

    /// Write an executable shell script that records its argv and exits 0.
    fn fake_zed(dir: &Path, capture: &Path, exit_code: i32) -> PathBuf {
        let script = dir.join("fake-zed.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" > {}\nexit {exit_code}\n",
                capture.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn args_for(path: &str, start_text: Option<&str>) -> OpenFileArgs {
        OpenFileArgs {
            file_path: path.to_string(),
            preview: false,
            start_text: start_text.map(String::from),
            end_text: None,
            select_to_end_of_line: false,
            make_frontmost: true,
        }
    }

    fn result_text(r: &CallToolResult) -> &str {
        let ToolContent::Text { text } = &r.content[0];
        text
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_file_positions_at_start_text() {
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("argv.txt");
        let bin = fake_zed(tmp.path(), &capture, 0);
        let target = tmp.path().join("main.rs");
        std::fs::write(&target, "fn a() {}\nfn main() {}\n").unwrap();

        let r = open_file(
            bin.to_str().unwrap(),
            &args_for(target.to_str().unwrap(), Some("fn main")),
            None,
        )
        .await;
        assert_eq!(result_text(&r), "Opened file and positioned at \"fn main\"");
        let argv = std::fs::read_to_string(&capture).unwrap();
        assert_eq!(argv.trim(), format!("-e {}:2:1", target.display()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_file_without_start_text_returns_success_json() {
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("argv.txt");
        let bin = fake_zed(tmp.path(), &capture, 0);
        let target = tmp.path().join("x.rs");
        std::fs::write(&target, "x").unwrap();

        let r = open_file(
            bin.to_str().unwrap(),
            &args_for(target.to_str().unwrap(), None),
            None,
        )
        .await;
        let body: serde_json::Value = serde_json::from_str(result_text(&r)).unwrap();
        assert_eq!(body["success"], true);
        assert_eq!(body["filePath"], target.to_str().unwrap());
        let argv = std::fs::read_to_string(&capture).unwrap();
        assert_eq!(argv.trim(), format!("-e {}", target.display()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_file_start_text_not_found_still_opens() {
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("argv.txt");
        let bin = fake_zed(tmp.path(), &capture, 0);
        let target = tmp.path().join("x.rs");
        std::fs::write(&target, "nothing here").unwrap();

        let r = open_file(
            bin.to_str().unwrap(),
            &args_for(target.to_str().unwrap(), Some("absent")),
            None,
        )
        .await;
        assert_eq!(
            result_text(&r),
            "Opened file, but text \"absent\" not found"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_file_missing_file_does_not_spawn() {
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("argv.txt");
        let bin = fake_zed(tmp.path(), &capture, 0);

        let r = open_file(
            bin.to_str().unwrap(),
            &args_for("/no/such/file.rs", None),
            None,
        )
        .await;
        let body: serde_json::Value = serde_json::from_str(result_text(&r)).unwrap();
        assert_eq!(body["success"], false);
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .starts_with("File not found")
        );
        assert!(!capture.exists(), "fake zed must not have been spawned");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_file_nonzero_exit_reports_launch_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("argv.txt");
        let bin = fake_zed(tmp.path(), &capture, 3);
        let target = tmp.path().join("x.rs");
        std::fs::write(&target, "x").unwrap();

        let r = open_file(
            bin.to_str().unwrap(),
            &args_for(target.to_str().unwrap(), None),
            None,
        )
        .await;
        let body: serde_json::Value = serde_json::from_str(result_text(&r)).unwrap();
        assert_eq!(body["success"], false);
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .starts_with("Failed to launch zed")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_file_relative_path_resolves_against_base() {
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("argv.txt");
        let bin = fake_zed(tmp.path(), &capture, 0);
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub fn x() {}").unwrap();

        let r = open_file(
            bin.to_str().unwrap(),
            &args_for("src/lib.rs", None),
            Some(tmp.path()),
        )
        .await;
        let body: serde_json::Value = serde_json::from_str(result_text(&r)).unwrap();
        assert_eq!(body["success"], true);
        assert!(
            body["filePath"].as_str().unwrap().ends_with("/src/lib.rs"),
            "relative path resolved against base"
        );
    }
}
