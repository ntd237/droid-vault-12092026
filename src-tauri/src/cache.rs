// T2.3 — Icon/metadata cache (implemented by Wave 2 task T2.3)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const META_FILE: &str = "meta.json";

/// Meta.json schema version. Bumped to 7 when the icon pipeline gained
/// extension-less layer sniffing, layer-list foregrounds, split pulls for
/// weak base renders and background-keyed raster cropping (R2): loaders
/// treat a missing/mismatched marker as "no meta", forcing one re-parse of
/// every cached app so the corrected icons replace the wrong ones.
const SCHEMA: i64 = 7;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CachedMeta {
    pub label: String,
    pub version_code: u64,
}

pub struct Cache {
    dir: PathBuf,
}

/// Monotonic suffix so concurrent temp writes never collide.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn atomic_write(dir: &Path, target: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = dir.join(format!(
        ".tmp_{}_{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, target) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Best-effort cleanup so failed writes do not litter the cache dir.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

impl Cache {
    /// Creates dir (and parents) if missing.
    pub fn new(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Cache { dir: dir.to_path_buf() })
    }

    /// Sanitize a package key: replace every char not in [a-zA-Z0-9._-] with '_'.
    pub fn sanitize_key(pkg: &str) -> String {
        pkg.chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            })
            .collect()
    }

    /// Path of "<sanitized>_<version_code>.png" inside dir (path computed even if file absent).
    pub fn icon_path(&self, pkg: &str, version_code: u64) -> PathBuf {
        self.dir
            .join(format!("{}_{}.png", Self::sanitize_key(pkg), version_code))
    }

    /// Atomic write: write bytes to a temp file in dir then rename to icon_path. Returns final path.
    pub fn put_icon(&self, pkg: &str, version_code: u64, bytes: &[u8]) -> io::Result<PathBuf> {
        let target = self.icon_path(pkg, version_code);
        atomic_write(&self.dir, &target, bytes)?;
        Ok(target)
    }

    /// Some(bytes) if the icon file exists, else None.
    pub fn get_icon(&self, pkg: &str, version_code: u64) -> Option<Vec<u8>> {
        std::fs::read(self.icon_path(pkg, version_code)).ok()
    }

    /// meta.json in dir: a JSON object with a `_schema` marker plus a mapping
    /// sanitized package key → CachedMeta.
    /// Missing/corrupted file, or a missing/mismatched `_schema` marker
    /// (pre-schema-2 cache without rendered icons) → empty map. The cache is
    /// an optimization, never fatal.
    pub fn load_meta(&self) -> HashMap<String, CachedMeta> {
        let bytes = match std::fs::read(self.dir.join(META_FILE)) {
            Ok(bytes) => bytes,
            Err(_) => return HashMap::new(),
        };
        let value: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => return HashMap::new(),
        };
        if value.get("_schema").and_then(serde_json::Value::as_i64) != Some(SCHEMA) {
            return HashMap::new();
        }
        let Some(obj) = value.as_object() else {
            return HashMap::new();
        };
        let mut map = HashMap::new();
        for (key, entry) in obj {
            if key == "_schema" {
                continue;
            }
            // Skip (rather than fail) entries that do not deserialize.
            if let Ok(meta) = serde_json::from_value::<CachedMeta>(entry.clone()) {
                map.insert(key.clone(), meta);
            }
        }
        map
    }

    /// Merge this entry into meta.json and atomically rewrite the whole file.
    /// The written file always carries the current `_schema` marker.
    pub fn put_meta(&self, pkg: &str, meta: &CachedMeta) -> io::Result<()> {
        self.put_meta_many(std::slice::from_ref(&(pkg.to_string(), meta.clone())))
    }

    /// H3b: merge MANY entries with a single load + single atomic rewrite —
    /// the per-app `put_meta` re-read/re-writes the whole map, which made
    /// cold-scan hydration spend seconds on 459 consecutive rewrites.
    pub fn put_meta_many(&self, entries: &[(String, CachedMeta)]) -> io::Result<()> {
        let mut map = self.load_meta();
        for (pkg, meta) in entries {
            map.insert(Self::sanitize_key(pkg), meta.clone());
        }
        let mut obj = serde_json::Map::new();
        obj.insert("_schema".to_string(), serde_json::json!(SCHEMA));
        for (key, entry) in map {
            obj.insert(
                key,
                serde_json::to_value(entry).expect("CachedMeta is always JSON-serializable"),
            );
        }
        let json = serde_json::to_vec_pretty(&serde_json::Value::Object(obj))
            .expect("JSON object is always serializable");
        atomic_write(&self.dir, &self.dir.join(META_FILE), &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_meta(label: &str, version_code: u64) -> CachedMeta {
        CachedMeta {
            label: label.to_string(),
            version_code,
        }
    }

    // sanitize_key tests

    #[test]
    fn test_sanitize_key_normal_package_is_unchanged() {
        // Normal package names with allowed chars stay untouched
        let pkg = "com.example.my_app-2";
        let sanitized = Cache::sanitize_key(pkg);
        assert_eq!(sanitized, "com.example.my_app-2");
    }

    #[test]
    fn test_sanitize_key_weird_chars_replaced_with_underscore() {
        // Every char not in [a-zA-Z0-9._-] must become '_'
        let pkg = "com/example:weird name";
        let sanitized = Cache::sanitize_key(pkg);
        assert_eq!(sanitized, "com_example_weird_name");
    }

    #[test]
    fn test_sanitize_key_empty_string_stays_empty() {
        // Empty-ish input yields empty key, no panic
        let sanitized = Cache::sanitize_key("");
        assert_eq!(sanitized, "");
    }

    // icon roundtrip tests

    #[test]
    fn test_put_icon_then_get_icon_roundtrips_same_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();

        let bytes = vec![1u8, 2, 3, 250, 42];
        let path = cache.put_icon("com.example.app", 7, &bytes).unwrap();

        assert_eq!(path, cache.icon_path("com.example.app", 7));
        assert_eq!(cache.get_icon("com.example.app", 7), Some(bytes));
    }

    #[test]
    fn test_get_icon_with_wrong_version_code_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();

        cache
            .put_icon("com.example.app", 7, &[9, 9, 9])
            .unwrap();

        assert_eq!(cache.get_icon("com.example.app", 8), None);
    }

    #[test]
    fn test_get_icon_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();

        assert_eq!(cache.get_icon("never.cached.app", 1), None);
    }

    #[test]
    fn test_put_icon_twice_overwrites_with_new_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();

        cache
            .put_icon("com.example.app", 7, &[1, 1, 1])
            .unwrap();
        let path = cache
            .put_icon("com.example.app", 7, &[5, 5, 5, 5])
            .unwrap();

        assert_eq!(fs::read(&path).unwrap(), vec![5, 5, 5, 5]);
        assert_eq!(cache.get_icon("com.example.app", 7), Some(vec![5, 5, 5, 5]));
    }

    // meta tests

    #[test]
    fn test_load_meta_missing_file_returns_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();

        let meta = cache.load_meta();
        assert!(meta.is_empty());
    }

    #[test]
    fn test_put_meta_then_load_meta_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();

        cache
            .put_meta("com.example.app", &test_meta("Example", 7))
            .unwrap();

        let meta = cache.load_meta();
        assert_eq!(meta.len(), 1);
        assert_eq!(
            meta.get("com.example.app"),
            Some(&test_meta("Example", 7))
        );
    }

    #[test]
    fn test_put_meta_second_call_merges_both_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();

        cache
            .put_meta("com.example.app", &test_meta("Example", 7))
            .unwrap();
        cache
            .put_meta("org.other.app", &test_meta("Other", 12))
            .unwrap();

        let meta = cache.load_meta();
        assert_eq!(meta.len(), 2);
        assert_eq!(
            meta.get("com.example.app"),
            Some(&test_meta("Example", 7))
        );
        assert_eq!(meta.get("org.other.app"), Some(&test_meta("Other", 12)));
    }

    #[test]
    fn test_put_meta_same_key_second_call_overwrites_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();

        cache
            .put_meta("com.example.app", &test_meta("Old", 7))
            .unwrap();
        cache
            .put_meta("com.example.app", &test_meta("New", 9))
            .unwrap();

        let meta = cache.load_meta();
        assert_eq!(meta.len(), 1);
        assert_eq!(meta.get("com.example.app"), Some(&test_meta("New", 9)));
    }

    #[test]
    fn test_load_meta_corrupted_file_returns_empty_map_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();
        fs::write(cache.dir.join("meta.json"), "{not valid json!!").unwrap();

        let meta = cache.load_meta();
        assert!(meta.is_empty());
    }

    // schema-marker tests (schema 4: adaptive-first selection + raster
    // padding normalization changed every icon, force-reparses old caches)

    #[test]
    fn test_load_meta_without_schema_marker_returns_empty_map() {
        // Arrange: legacy meta.json written before the schema marker existed.
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();
        let legacy = serde_json::json!({
            "com.example.app": {"label": "Example", "version_code": 7}
        });
        fs::write(
            cache.dir.join("meta.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        // Act
        let meta = cache.load_meta();

        // Assert: legacy caches are treated as "no meta" → force re-parse.
        assert!(meta.is_empty(), "missing schema marker must invalidate all meta");
    }

    #[test]
    fn test_load_meta_with_wrong_schema_returns_empty_map() {
        // Arrange: future/unknown schema version.
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();
        let future = serde_json::json!({
            "_schema": 99,
            "com.example.app": {"label": "Example", "version_code": 7}
        });
        fs::write(
            cache.dir.join("meta.json"),
            serde_json::to_vec(&future).unwrap(),
        )
        .unwrap();

        // Act
        let meta = cache.load_meta();

        // Assert
        assert!(meta.is_empty(), "mismatched schema must invalidate all meta");
    }

    #[test]
    fn test_load_meta_with_stale_schema_2_and_3_returns_empty_map() {
        // Arrange: caches written before the adaptive-first + raster
        // normalization pipeline; every rendered icon changed, so old meta
        // must be invalidated.
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();
        for stale_schema in [2, 3] {
            let stale = serde_json::json!({
                "_schema": stale_schema,
                "com.example.app": {"label": "Example", "version_code": 7}
            });
            fs::write(
                cache.dir.join("meta.json"),
                serde_json::to_vec(&stale).unwrap(),
            )
            .unwrap();

            // Act
            let meta = cache.load_meta();

            // Assert
            assert!(
                meta.is_empty(),
                "stale schema-{stale_schema} meta must force re-parse"
            );
        }
    }

    #[test]
    fn test_put_meta_writes_schema_7_and_roundtrips() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();

        // Act
        cache
            .put_meta("com.example.app", &test_meta("Example", 7))
            .unwrap();

        // Assert: on-disk file carries the schema marker; load skips it.
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(cache.dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(raw.get("_schema"), Some(&serde_json::json!(7)));

        let meta = cache.load_meta();
        assert_eq!(
            meta.get("com.example.app"),
            Some(&test_meta("Example", 7))
        );
    }

    #[test]
    fn test_put_meta_second_call_keeps_schema_marker() {
        // Arrange: first write sets the marker, a merge must keep it.
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();

        // Act
        cache
            .put_meta("com.example.app", &test_meta("Example", 7))
            .unwrap();
        cache
            .put_meta("org.other.app", &test_meta("Other", 12))
            .unwrap();

        // Assert
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(cache.dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(raw.get("_schema"), Some(&serde_json::json!(7)));
        let meta = cache.load_meta();
        assert_eq!(meta.len(), 2);
        assert_eq!(meta.get("com.example.app"), Some(&test_meta("Example", 7)));
        assert_eq!(meta.get("org.other.app"), Some(&test_meta("Other", 12)));
    }

    #[test]
    fn test_icon_path_uses_sanitized_key_and_version() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();

        let path = cache.icon_path("com/example app", 42);
        assert_eq!(
            path,
            dir.path().join("com_example_app_42.png")
        );
        // Path is computed even if the file does not exist
        assert!(!path.exists());
    }

    #[test]
    fn test_put_meta_many_writes_all_entries_in_one_pass() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();
        let entries = vec![
            ("com.a".to_string(), CachedMeta { label: "A".into(), version_code: 1 }),
            ("com.b".to_string(), CachedMeta { label: "B".into(), version_code: 2 }),
            ("com.c".to_string(), CachedMeta { label: "C".into(), version_code: 3 }),
        ];

        // Act
        cache.put_meta_many(&entries).unwrap();

        // Assert: all present after one merged rewrite; schema marker intact.
        let map = cache.load_meta();
        assert_eq!(map.get("com.a").map(|m| m.label.as_str()), Some("A"));
        assert_eq!(map.get("com.b").map(|m| m.label.as_str()), Some("B"));
        assert_eq!(map.get("com.c").map(|m| m.label.as_str()), Some("C"));
    }

    #[test]
    fn test_put_meta_many_merges_with_existing_entries() {
        // Arrange: one entry already cached, bulk adds two more.
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path()).unwrap();
        cache
            .put_meta("com.old", &CachedMeta { label: "Old".into(), version_code: 9 })
            .unwrap();

        // Act
        cache
            .put_meta_many(&[(
                "com.new".to_string(),
                CachedMeta { label: "New".into(), version_code: 10 },
            )])
            .unwrap();

        // Assert
        let map = cache.load_meta();
        assert_eq!(map.get("com.old").map(|m| m.version_code), Some(9));
        assert_eq!(map.get("com.new").map(|m| m.version_code), Some(10));
    }
}
