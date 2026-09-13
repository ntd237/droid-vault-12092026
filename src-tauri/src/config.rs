// T2.2 — App config + blacklist policy (implemented by Wave 2 task T2.2)

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Packages that must never be offered for uninstall (system-critical).
pub const DEFAULT_BLACKLIST: &[&str] = &[
    "android",
    "com.android.systemui",
    "com.android.settings",
    "com.android.phone",
    "com.android.providers.telephony",
    "com.android.providers.contacts",
];

/// Used when neither the config nor PATH yields an existing adb binary.
pub const FALLBACK_ADB_PATH: &str = "C:\\platform-tools\\platform-tools\\adb.exe";

fn default_blacklist() -> Vec<String> {
    DEFAULT_BLACKLIST.iter().map(|s| s.to_string()).collect()
}

fn default_theme() -> String {
    "dark".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppConfig {
    #[serde(default)]
    pub adb_path: Option<String>,
    #[serde(default = "default_blacklist")]
    pub blacklist: Vec<String>,
    #[serde(default = "default_theme")]
    pub theme: String, // "dark" | "light"
}

impl AppConfig {
    pub fn default_config() -> Self {
        AppConfig {
            adb_path: None,
            blacklist: default_blacklist(),
            theme: default_theme(),
        }
    }
}

/// Loads `<dir>/config.json`, creating it with defaults when missing.
///
/// Parse failure of an existing file is surfaced as
/// `std::io::Error` with `ErrorKind::InvalidData`.
pub fn load_or_create(dir: &Path) -> std::io::Result<AppConfig> {
    let cfg_path = dir.join("config.json");
    if !cfg_path.exists() {
        std::fs::create_dir_all(dir)?;
        let default = AppConfig::default_config();
        let json = serde_json::to_string_pretty(&default)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&cfg_path, json)?;
        return Ok(default);
    }

    let raw = std::fs::read_to_string(&cfg_path)?;
    serde_json::from_str(&raw)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Resolves the adb binary path (requirement R1):
/// 1. `config.adb_path` when set and the file exists,
/// 2. else the first existing `adb.exe` in `path_env` (';' separated),
/// 3. else `FALLBACK_ADB_PATH` (returned regardless of existence).
pub fn resolve_adb_path(config: &AppConfig, path_env: &str) -> PathBuf {
    if let Some(cfg_path) = &config.adb_path {
        let candidate = PathBuf::from(cfg_path);
        if candidate.is_file() {
            return candidate;
        }
    }

    for entry in path_env.split(';') {
        if entry.is_empty() {
            continue;
        }
        let candidate = PathBuf::from(entry).join("adb.exe");
        if candidate.is_file() {
            return candidate;
        }
    }

    PathBuf::from(FALLBACK_ADB_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn test_load_or_create_missing_file_creates_default() {
        // First run in a fresh dir: default config is written and returned.
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.json");

        let cfg = load_or_create(dir.path()).unwrap();

        assert!(cfg_path.exists());
        assert_eq!(cfg, AppConfig::default_config());
        assert_eq!(cfg.blacklist.len(), 6);
        assert_eq!(cfg.blacklist[0], "android");
        assert_eq!(cfg.adb_path, None);
        assert_eq!(cfg.theme, "dark");
        // Written file is valid pretty JSON that parses back to the same config.
        let raw = fs::read_to_string(&cfg_path).unwrap();
        let reparsed: AppConfig = serde_json::from_str(&raw).unwrap();
        assert_eq!(reparsed, cfg);
    }

    #[test]
    fn test_load_or_create_existing_file_round_trip() {
        // Existing config.json is parsed and its values are preserved.
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.json");
        fs::write(
            &cfg_path,
            r#"{
  "adb_path": "D:\\tools\\adb.exe",
  "blacklist": ["android", "com.example.kept"],
  "theme": "light"
}"#,
        )
        .unwrap();

        let cfg = load_or_create(dir.path()).unwrap();

        assert_eq!(cfg.adb_path.as_deref(), Some("D:\\tools\\adb.exe"));
        assert_eq!(cfg.blacklist, vec!["android", "com.example.kept"]);
        assert_eq!(cfg.theme, "light");
    }

    #[test]
    fn test_load_or_create_corrupted_json_returns_invalid_data() {
        // Malformed JSON surfaces as an io::Error with ErrorKind::InvalidData.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.json"), "{ not valid json !!").unwrap();

        let err = load_or_create(dir.path()).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_resolve_adb_path_existing_config_path_wins() {
        // A configured adb path that exists takes priority over the PATH search.
        let dir = tempfile::tempdir().unwrap();
        let adb = dir.path().join("adb.exe");
        fs::write(&adb, b"stub").unwrap();

        let cfg = AppConfig {
            adb_path: Some(adb.to_string_lossy().into_owned()),
            blacklist: default_blacklist(),
            theme: default_theme().to_string(),
        };
        let fake_dir = tempfile::tempdir().unwrap();
        let path_env = format!("{};C:\\Windows", fake_dir.path().to_string_lossy());

        let resolved = resolve_adb_path(&cfg, &path_env);

        assert_eq!(resolved, adb);
    }

    #[test]
    fn test_resolve_adb_path_searches_path_env_when_config_missing() {
        // No usable config path → first existing adb.exe found in PATH entries wins.
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        fs::create_dir_all(&tools).unwrap();
        let adb = tools.join("adb.exe");
        fs::write(&adb, b"stub").unwrap();

        let cfg = AppConfig {
            adb_path: None,
            blacklist: default_blacklist(),
            theme: default_theme().to_string(),
        };
        let empty_dir = tempfile::tempdir().unwrap();
        let path_env = format!(
            "{};{}",
            empty_dir.path().to_string_lossy(),
            tools.to_string_lossy()
        );

        let resolved = resolve_adb_path(&cfg, &path_env);

        assert_eq!(resolved, adb);
    }

    #[test]
    fn test_resolve_adb_path_skips_nonexistent_config_path_and_uses_path_hit() {
        // A configured path that does not exist is ignored (falls through to PATH).
        let cfg = AppConfig {
            adb_path: Some("Z:\\nonexistent\\adb.exe".to_string()),
            blacklist: default_blacklist(),
            theme: default_theme().to_string(),
        };
        let dir = tempfile::tempdir().unwrap();
        let adb = dir.path().join("adb.exe");
        fs::write(&adb, b"stub").unwrap();

        let resolved = resolve_adb_path(&cfg, &dir.path().to_string_lossy());

        assert_eq!(resolved, adb);
    }

    #[test]
    fn test_resolve_adb_path_falls_back_when_nothing_found() {
        // Neither config nor PATH yields a binary → FALLBACK_ADB_PATH is returned as-is.
        let cfg = AppConfig {
            adb_path: None,
            blacklist: default_blacklist(),
            theme: default_theme().to_string(),
        };
        let empty_dir = tempfile::tempdir().unwrap();
        let path_env = empty_dir.path().to_string_lossy().into_owned();

        let resolved = resolve_adb_path(&cfg, &path_env);

        assert_eq!(resolved, PathBuf::from(FALLBACK_ADB_PATH));
    }
}
