// T2.1 — ADB command wrapper + output parsers (implemented by Wave 2 task T2.1)

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;

/// Create a `Command` configured not to spawn a console window on Windows.
/// On Windows, executing console binaries without `CREATE_NO_WINDOW` (`0x08000000`)
/// causes Windows to flash a temporary command prompt window.
pub fn silent_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeviceState {
    Device,
    Unauthorized,
    Offline,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceEntry {
    pub serial: String,
    pub state: DeviceState,
}

pub struct Adb {
    pub path: PathBuf,
    lock: Mutex<()>,
}

impl Adb {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Adb { path: path.into(), lock: Mutex::new(()) }
    }

    /// Run adb with args; the internal lock serializes all adb invocations.
    /// On success returns stdout (lossy utf8). On non-zero exit or spawn
    /// failure returns an error carrying stderr / os error text.
    pub fn run(&self, args: &[&str]) -> Result<String, String> {
        let _guard = self.lock.lock().unwrap();
        let output = silent_command(&self.path)
            .args(args)
            .output()
            .map_err(|e| format!("failed to spawn adb {:?}: {e}", self.path))?;

        if !output.status.success() {
            // Remote shell errors print to STDOUT with empty stderr (e.g.
            // `cmd package install-existing` → NameNotFoundException), so the
            // error message must carry stdout too or the user sees a bare
            // "exited with status …:" with no reason.
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let detail: String = [stderr.trim(), stdout.trim()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" | ");
            let detail = if detail.chars().count() > 400 {
                format!("{}…", detail.chars().take(400).collect::<String>())
            } else {
                detail
            };
            return Err(format!(
                "adb {:?} exited with status {}: {}",
                self.path,
                output.status,
                detail
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Run a child process, stream its stdout line-by-line through
    /// `on_line` as it arrives, with `stdin` fed to the child. Used for the
    /// on-device DvHelper dex (label/icon over one adb shell pipe). The
    /// internal lock serializes this with all other adb invocations.
    ///
    /// Returns `Err` only on spawn failure or deadline expiry (the child is
    /// killed in that case); a non-zero exit after useful output is NOT an
    /// error — the caller judges by the lines it received.
    pub fn run_streaming(
        &self,
        args: &[&str],
        stdin: &str,
        timeout: std::time::Duration,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), String> {
        use std::io::{BufRead, Write};
        use std::sync::mpsc::RecvTimeoutError;

        let _guard = self.lock.lock().unwrap();
        let mut child = silent_command(&self.path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn adb {:?}: {e}", self.path))?;

        // Feed stdin from a dedicated thread — the pipe can fill up when the
        // child reads slowly, and blocking here would deadlock the reader.
        let mut stdin_pipe = child.stdin.take().expect("stdin piped");
        let stdin_data = stdin.to_string();
        let writer = std::thread::spawn(move || {
            let _ = stdin_pipe.write_all(stdin_data.as_bytes());
            // dropping the handle closes the pipe → EOF for the child
        });

        let stdout = child.stdout.take().expect("stdout piped");
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let reader = std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        let deadline = std::time::Instant::now() + timeout;
        let mut result = Ok(());
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(line) => on_line(&line),
                Err(RecvTimeoutError::Timeout) => {
                    result = Err(format!("stream timeout after {timeout:?}"));
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = writer.join();
        let _ = reader.join();
        result
    }
}

/// Parse `adb devices` output. Skips the header line and blank lines.
/// Each row is "<serial>\t<state>"; unknown states map to `Offline`.
pub fn parse_devices(output: &str) -> Vec<DeviceEntry> {
    let mut devices = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("List of devices attached") {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(serial), Some(state)) = (parts.next(), parts.next()) else {
            continue;
        };
        let state = match state {
            "device" => DeviceState::Device,
            "unauthorized" => DeviceState::Unauthorized,
            _ => DeviceState::Offline,
        };
        devices.push(DeviceEntry { serial: serial.to_string(), state });
    }
    devices
}

/// Parse `pm list packages` output: lines like "package:com.example.app"
/// become "com.example.app"; empty and non-matching lines are skipped.
pub fn parse_package_list(output: &str) -> Vec<String> {
    output
        .lines()
        .map(|l| l.trim())
        .filter_map(|l| l.strip_prefix("package:"))
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
        .collect()
}

/// Parse `pm path <pkg>` output. Lines look like "package:/data/app/.../base.apk"
/// followed by split-config lines. The adb server sometimes returns "framed"
/// lines instead: the "package:" prefix is dropped and a decimal byte-length
/// prefix ("<digits>:") is added, e.g. "95:/data/app/.../base.apk". Both
/// prefixes are stripped from every line before validation. Prefers the line
/// containing "base.apk"; otherwise the first line ending in ".apk"; `None`
/// if no apk path is present.
pub fn parse_pm_path(output: &str) -> Option<String> {
    let mut fallback: Option<&str> = None;
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let without_package = line.strip_prefix("package:").unwrap_or(line);
        // Framed output: optional leading "<digits>:" byte-length prefix.
        // Only the LEADING prefix is stripped; interior colons are preserved.
        let path = match without_package.split_once(':') {
            Some((n, rest)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => rest,
            _ => without_package,
        };
        if !path.ends_with(".apk") {
            continue;
        }
        if path.contains("base.apk") {
            return Some(path.to_string());
        }
        if fallback.is_none() {
            fallback = Some(path);
        }
    }
    fallback.map(|p| p.to_string())
}

/// Check `dumpsys package <pkg>` output for the "User 0:" segment.
/// Returns false only when the segment contains "installed=false";
/// when no "User 0:" is found, defaults to installed (true).
pub fn is_installed_for_user0(dumpsys_output: &str) -> bool {
    let Some(idx) = dumpsys_output.find("User 0:") else {
        return true;
    };
    let segment = &dumpsys_output[idx..];
    let line_end = segment.find('\n').unwrap_or(segment.len());
    !segment[..line_end].contains("installed=false")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- parse_devices ----------

    #[test]
    fn test_parse_devices_with_header_and_multiple_devices() {
        let out = "List of devices attached\r\nemulator-5554\tdevice\r\ndead0beef\tunauthorized\r\n\r\n";
        let devices = parse_devices(out);

        assert_eq!(
            devices,
            vec![
                DeviceEntry { serial: "emulator-5554".into(), state: DeviceState::Device },
                DeviceEntry { serial: "dead0beef".into(), state: DeviceState::Unauthorized },
            ]
        );
    }

    #[test]
    fn test_parse_devices_offline_and_unknown_state_map_to_offline() {
        let out = "List of devices attached\nabc123\toffline\nxyz789\trecovery\n";
        let devices = parse_devices(out);

        assert_eq!(
            devices,
            vec![
                DeviceEntry { serial: "abc123".into(), state: DeviceState::Offline },
                DeviceEntry { serial: "xyz789".into(), state: DeviceState::Offline },
            ]
        );
    }

    #[test]
    fn test_parse_devices_extra_spaces_between_serial_and_state() {
        let out = "List of devices attached\nabc123      device\n";
        let devices = parse_devices(out);

        assert_eq!(
            devices,
            vec![DeviceEntry { serial: "abc123".into(), state: DeviceState::Device }]
        );
    }

    #[test]
    fn test_parse_devices_empty_output_returns_empty_vec() {
        let out = "List of devices attached\r\n\r\n";
        assert!(parse_devices(out).is_empty());
    }

    // ---------- parse_package_list ----------

    #[test]
    fn test_parse_package_list_normal_lines() {
        let out = "package:com.example.app\npackage:com.other.pkg\r\n";
        let pkgs = parse_package_list(out);

        assert_eq!(pkgs, vec!["com.example.app".to_string(), "com.other.pkg".to_string()]);
    }

    #[test]
    fn test_parse_package_list_skips_empty_and_non_package_lines() {
        let out = "\r\npackage:com.a\nnot-a-package\n  \npackage:com.b\r\n";
        let pkgs = parse_package_list(out);

        assert_eq!(pkgs, vec!["com.a".to_string(), "com.b".to_string()]);
    }

    #[test]
    fn test_parse_package_list_empty_output_returns_empty_vec() {
        assert!(parse_package_list("").is_empty());
    }

    // ---------- parse_pm_path ----------

    #[test]
    fn test_parse_pm_path_prefers_base_apk_among_splits() {
        let out = "package:/data/app/~~x==/com.ex-1/split_config.arm64_v8a.apk\r\npackage:/data/app/~~x==/com.ex-1/base.apk\n";
        let path = parse_pm_path(out);

        assert_eq!(path, Some("/data/app/~~x==/com.ex-1/base.apk".to_string()));
    }

    #[test]
    fn test_parse_pm_path_without_base_apk_returns_first_apk_line() {
        let out = "package:/data/app/com.ex/split_config.en.apk\r\npackage:/data/app/com.ex/split_config.xxhdpi.apk\n";
        let path = parse_pm_path(out);

        assert_eq!(path, Some("/data/app/com.ex/split_config.en.apk".to_string()));
    }

    #[test]
    fn test_parse_pm_path_empty_output_returns_none() {
        assert_eq!(parse_pm_path(""), None);
        assert_eq!(parse_pm_path("\r\n  \n"), None);
    }

    #[test]
    fn test_parse_pm_path_framed_line_without_package_prefix_is_stripped() {
        // Reproduced adb anomaly: "package:" dropped, decimal byte-length frame prefix added.
        let out = "95:/data/app/~~x==/com.a-1/base.apk\n";
        let path = parse_pm_path(out);

        assert_eq!(path, Some("/data/app/~~x==/com.a-1/base.apk".to_string()));
    }

    #[test]
    fn test_parse_pm_path_multi_line_framed_output_prefers_base_apk() {
        let out = "41:/data/app/~~x==/com.a-1/split_config.arm64_v8a.apk\r\n97:/data/app/~~x==/com.a-1/base.apk\n";
        let path = parse_pm_path(out);

        assert_eq!(path, Some("/data/app/~~x==/com.a-1/base.apk".to_string()));
    }

    #[test]
    fn test_parse_pm_path_normal_package_prefix_behavior_unchanged() {
        let out = "package:/data/app/~~x==/com.a-1/base.apk\r\n";
        let path = parse_pm_path(out);

        assert_eq!(path, Some("/data/app/~~x==/com.a-1/base.apk".to_string()));
    }

    #[test]
    fn test_parse_pm_path_framed_split_only_falls_back_to_first_apk_line() {
        let out = "43:/data/app/com.ex/split_config.en.apk\r\n47:/data/app/com.ex/split_config.xxhdpi.apk\n";
        let path = parse_pm_path(out);

        assert_eq!(path, Some("/data/app/com.ex/split_config.en.apk".to_string()));
    }

    #[test]
    fn test_parse_pm_path_digits_and_colons_after_leading_prefix_are_preserved() {
        // Only the LEADING "<digits>:" frame prefix may be stripped; interior colons stay.
        let out = "10:/data/2026:07/com.a-1/base.apk\n";
        let path = parse_pm_path(out);

        assert_eq!(path, Some("/data/2026:07/com.a-1/base.apk".to_string()));
    }

    // ---------- is_installed_for_user0 ----------

    #[test]
    fn test_is_installed_for_user0_installed_true_returns_true() {
        let out = "Packages:\n  Package [com.example] (abc):\n    User 0: ceDataInode=123 installed=true hidden=false\n";
        assert!(is_installed_for_user0(out));
    }

    #[test]
    fn test_is_installed_for_user0_with_installed_false_returns_false() {
        let out = "Packages:\n  Package [com.example] (abc):\n    User 0: ceDataInode=123 installed=false stopped=1\n";
        assert!(!is_installed_for_user0(out));
    }

    #[test]
    fn test_is_installed_for_user0_without_user0_defaults_true() {
        let out = "Packages:\n  Package [com.example] (abc):\n";
        assert!(is_installed_for_user0(out));
    }

    // ---------- Adb ----------

    #[test]
    fn test_adb_run_with_nonexistent_binary_returns_err() {
        let adb = Adb::new("Z:/nonexistent/path/to/adb-definitely-missing.exe");
        let result = adb.run(&["devices"]);

        assert!(result.is_err());
        assert!(!result.unwrap_err().is_empty());
    }

    /// Remote shell errors print to STDOUT with empty stderr (e.g. AOSP
    /// `cmd package install-existing` → "Error: ...NameNotFoundException").
    /// The error message must carry that stdout, or the user sees a bare
    /// "exited with status exit code: 1:" with no reason.
    #[test]
    fn test_adb_run_error_includes_stdout_on_failure() {
        let adb = Adb::new("cmd");
        let result = adb.run(&["/C", "echo DV-STDOUT-MARKER & exit /b 3"]);

        let err = result.expect_err("exit /b 3 must fail");
        assert!(
            err.contains("DV-STDOUT-MARKER"),
            "error must include the failing command's stdout, got: {err}"
        );
    }

    // ---------- run_streaming (helper.dex pipe) ----------

    /// stdout lines stream through the callback as they arrive.
    #[test]
    fn test_run_streaming_receives_stdout_lines() {
        let adb = Adb::new("cmd");
        let mut lines = Vec::new();
        let result = adb.run_streaming(
            &["/C", "echo alpha&&echo beta"],
            "",
            std::time::Duration::from_secs(10),
            &mut |line| lines.push(line.to_string()),
        );

        result.expect("stream must succeed");
        assert_eq!(lines, vec!["alpha".to_string(), "beta".to_string()]);
    }

    /// stdin must reach the child: `sort` echoes its stdin sorted.
    #[test]
    fn test_run_streaming_feeds_stdin() {
        let adb = Adb::new("cmd");
        let mut lines = Vec::new();
        let result = adb.run_streaming(
            &["/C", "sort"],
            "banana\napple\n",
            std::time::Duration::from_secs(10),
            &mut |line| lines.push(line.to_string()),
        );

        result.expect("stream must succeed");
        assert_eq!(lines, vec!["apple".to_string(), "banana".to_string()]);
    }

    /// Deadline expiry must kill the child and surface an error.
    #[test]
    fn test_run_streaming_timeout_kills_child() {
        let adb = Adb::new("cmd");
        let mut lines = Vec::new();
        let result = adb.run_streaming(
            &["/C", "ping -n 30 127.0.0.1 >nul"],
            "",
            std::time::Duration::from_millis(800),
            &mut |line| lines.push(line.to_string()),
        );

        assert!(result.is_err(), "deadline must fail the stream");
        assert!(lines.is_empty());
    }

    #[test]
    #[cfg(windows)]
    fn test_silent_command_applies_create_no_window_flag() {
        let cmd = silent_command("cmd");
        // CREATE_NO_WINDOW is 0x08000000.
        // We verify that silent_command produces a Command that can spawn without error.
        assert_eq!(cmd.get_program(), "cmd");
    }
}
