// T4.1 — Tauri wiring: adapters (concrete types → packages traits), AppState
// with an atomic busy guard, and the 5 Tauri commands with event emission.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tauri::{AppHandle, Emitter, Manager};

use crate::{adb, apk, cache, config, packages};

// ---- Busy guard ----

/// Pure, unit-testable busy guard: at most one holder at a time.
pub struct BusyGuard(AtomicBool);

impl BusyGuard {
    pub fn new() -> Self {
        BusyGuard(AtomicBool::new(false))
    }

    /// true ⇔ acquired; false ⇔ already busy (state unchanged).
    pub fn try_acquire(&self) -> bool {
        self.0
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn release(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Default for BusyGuard {
    fn default() -> Self {
        Self::new()
    }
}

pub struct AppState {
    pub busy: BusyGuard,
    /// Monotonic scan generation: bumped on every `get_apps` so a background
    /// hydrator from an older scan can stop itself when a newer scan starts.
    pub scan_gen: AtomicU64,
    /// Shared adb client, initialized once. One `Adb` instance means ONE
    /// internal Mutex domain, so no parallel adb client processes ever run
    /// against a device, whatever the command concurrency.
    pub adb: std::sync::RwLock<Option<std::sync::Arc<adb::Adb>>>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            busy: BusyGuard::new(),
            scan_gen: AtomicU64::new(0),
            adb: std::sync::RwLock::new(None),
        }
    }
}

/// Releases the busy guard on drop so every code path (including early
/// returns and future cancellation) leaves the guard free.
struct BusyLease<'a>(&'a BusyGuard);

impl Drop for BusyLease<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

// ---- Adapters ----

/// Monotonic suffix so concurrent temp APK pulls never collide.
static APK_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_apk_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "dv_apk_{}_{}.apk",
        std::process::id(),
        APK_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// `packages::AdbLike` impl that targets one device (`-s <serial>`) when a
/// serial is present. Holds `Arc<adb::Adb>` so the same instance is shared
/// with `ApkAdapter`; the Adb internal lock serializes invocations and is
/// only held inside `run`, never across an await.
struct DeviceAdb {
    adb: std::sync::Arc<adb::Adb>,
    serial: Option<String>,
}

impl packages::AdbLike for DeviceAdb {
    fn run(&self, args: &[&str]) -> Result<String, String> {
        match &self.serial {
            Some(serial) => {
                let mut full: Vec<&str> = vec!["-s", serial];
                full.extend_from_slice(args);
                self.adb.run(&full)
            }
            None => self.adb.run(args),
        }
    }
}

/// The DvHelper dex (built from ../helper/DvHelper.java via helper/build.sh):
/// resolves label/icon on-device through PackageManager — 0 bytes of APK
/// transferred.
const HELPER_DEX: &[u8] = include_bytes!("../assets/dv_helper.dex");
const HELPER_DEVICE_PATH: &str = "/data/local/tmp/dv_helper.dex";
/// H3: parallel pull workers. The pass is dominated by adb process spawn
/// overhead (~0.3-0.4s each on Windows), not transfer — overlay APKs are
/// tiny — so 12 concurrent spawns halve the tail vs P=6 without saturating
/// USB (benchmark: small-file throughput still climbing at P=6).
const PULL_WORKERS: usize = 12;

/// `packages::HelperRunner` impl: pushes the embedded dex to
/// /data/local/tmp (idempotent, ~5KB), then streams package names through
/// one `adb shell` pipe. Fatal = push failure, spawn failure or timeout.
struct HelperAdapter {
    adb: std::sync::Arc<adb::Adb>,
    serial: String,
}

impl packages::HelperRunner for HelperAdapter {
    fn query(&self, pkgs: &[String], on_line: &mut dyn FnMut(&str)) -> Result<(), String> {
        if pkgs.is_empty() {
            return Ok(());
        }
        // H5: skip the push when the device already holds a dex of the same
        // byte size — an adb push costs a spawn + transfer on every scan.
        let mut dex_on_device = false;
        if let Ok(out) = self.adb.run(&["-s", &self.serial, "shell", "ls", "-l", HELPER_DEVICE_PATH])
        {
            dex_on_device = packages::parse_ls_sizes(&out)
                .iter()
                .any(|(size, _)| *size == HELPER_DEX.len() as u64);
        }
        if !dex_on_device {
            let local = std::env::temp_dir().join("dv_helper.dex");
            std::fs::write(&local, HELPER_DEX)
                .map_err(|e| format!("Không ghi được helper tạm: {e}"))?;
            self.adb
                .run(&["-s", &self.serial, "push", local.to_string_lossy().as_ref(), HELPER_DEVICE_PATH])
                .map_err(|e| format!("Không đẩy helper lên thiết bị: {e}"))?;
        }

        let mut stdin = String::with_capacity(pkgs.iter().map(|p| p.len() + 1).sum());
        for pkg in pkgs {
            stdin.push_str(pkg);
            stdin.push('\n');
        }
        self.adb.run_streaming(
            &[
                "-s",
                &self.serial,
                "shell",
                "CLASSPATH=/data/local/tmp/dv_helper.dex app_process /system/bin DvHelper",
            ],
            &stdin,
            std::time::Duration::from_secs(120),
            on_line,
        )
    }
}

/// `packages::ApkLike` impl: `adb pull` the device-side APK to a unique temp
/// file, parse it locally, then always delete the temp file (success or
/// parse error). A pull error is returned as-is — packages.rs degrades
/// gracefully per package.
struct ApkAdapter {
    adb: std::sync::Arc<adb::Adb>,
    serial: String,
}

impl packages::ApkLike for ApkAdapter {
    fn parse(&self, apk_path: &str) -> Result<(String, Option<Vec<u8>>, Option<u64>), String> {
        let local = temp_apk_path();
        // Remove any stale file from a previous crash before pulling.
        let _ = std::fs::remove_file(&local);

        // Clean up the temp file on the pull-failure path too, then return
        // the error as-is — packages.rs degrades gracefully per package.
        if let Err(e) = self.pull(apk_path, &local) {
            let _ = std::fs::remove_file(&local);
            return Err(e);
        }

        let result = self.parse_with_lazy_splits(&local, apk_path);
        // Always clean up, even when parse failed.
        let _ = std::fs::remove_file(&local);
        result
    }
}

impl ApkAdapter {
    fn pull(&self, device_path: &str, local: &Path) -> Result<(), String> {
        self.adb.run(&[
            "-s",
            &self.serial,
            "pull",
            device_path,
            local.to_string_lossy().as_ref(),
        ])
        .map(|_| ())
    }

    /// Parse the pulled base APK; then decide whether density splits must be
    /// pulled lazily — one at a time, density splits first in ascending
    /// density (best density last) — re-resolving after each pull until a
    /// healthy icon is found or the splits are exhausted. Splits are tried
    /// when the base resolved NO icon at all (App Bundle installs whose icon
    /// layers live in density splits, T-D) OR when the base icon looks like a
    /// background-only/stub render (R2-F3: Gboard's base carries only a 52B
    /// foreground stub; the real foreground lives in the density split).
    /// Local split temp files are always cleaned up.
    fn parse_with_lazy_splits(
        &self,
        local_base: &Path,
        device_base: &str,
    ) -> Result<(String, Option<Vec<u8>>, Option<u64>), String> {
        let mut info = apk::parse_apk(local_base)?;
        if !should_try_splits(&info.icon_png) {
            return Ok((info.label, info.icon_png, info.version_code));
        }

        let split_paths = match std::path::Path::new(device_base).parent() {
            // `ls` runs through the same shared Adb mutex — never concurrent.
            Some(parent) => self
                .adb
                .run(&["-s", &self.serial, "shell", "ls", &parent.to_string_lossy()])
                .map(|out| order_split_paths(&out, &parent.to_string_lossy()))
                .unwrap_or_default(),
            None => Vec::new(),
        };

        let mut local_splits: Vec<PathBuf> = Vec::new();
        for device_split in split_paths {
            let local_split = temp_apk_path();
            if self.pull(&device_split, &local_split).is_err() {
                // A failed split pull is not fatal: degrade with whatever
                // the base (and any earlier splits) resolved.
                break;
            }
            local_splits.push(local_split);
            if let Ok(parsed) = apk::parse_apk_with_splits(local_base, &local_splits) {
                info = parsed;
            }
            if !should_try_splits(&info.icon_png) {
                break;
            }
        }
        for path in &local_splits {
            let _ = std::fs::remove_file(path);
        }

        Ok((info.label, info.icon_png, info.version_code))
    }
}

/// Density rank of a split APK file name (`split_config.xxxhdpi.apk` → 6);
/// `None` for splits without a density qualifier (arch/locale/feature).
fn split_density_rank(name: &str) -> Option<u8> {
    // Longest qualifier first: "xxxhdpi" contains "xxhdpi" contains "hdpi".
    for (needle, rank) in [
        ("xxxhdpi", 6),
        ("xxhdpi", 5),
        ("xhdpi", 4),
        ("hdpi", 3),
        ("mdpi", 2),
        ("ldpi", 1),
    ] {
        if name.contains(needle) {
            return Some(rank);
        }
    }
    None
}

/// Build the pull order for split APKs from `ls <base dir>` output: density
/// splits first in ASCENDING density (so the best density is pulled last and
/// wins when several are present), then the remaining splits.
fn order_split_paths(ls_output: &str, dir: &str) -> Vec<String> {
    let mut names: Vec<(u8, u8, &str)> = ls_output
        .lines()
        .map(str::trim)
        // Toybox `ls` prints bare names for a directory argument; be
        // defensive and take the last path segment of each line anyway.
        .map(|l| l.rsplit('/').next().unwrap_or(l))
        .filter(|name| name.starts_with("split_") && name.ends_with(".apk"))
        .map(|name| match split_density_rank(name) {
            Some(density) => (0, density, name),
            None => (1, 0, name),
        })
        .collect();
    names.sort_unstable_by_key(|(group, density, _)| (*group, *density));
    names
        .into_iter()
        .map(|(_, _, name)| format!("{}/{}", dir.trim_end_matches('/'), name))
        .collect()
}

/// Decide whether lazy split pulls should be attempted for a parsed base
/// icon: none at all (T-D) or a weak background-only/stub render (R2-F3).
/// Healthy icons skip the pulls.
fn should_try_splits(icon: &Option<Vec<u8>>) -> bool {
    match icon {
        None => true,
        Some(bytes) => apk::icon_looks_weak(bytes),
    }
}

/// `packages::CacheLike` impl over the on-disk cache, shared across the
/// hydration threads through `Arc<Mutex<…>>` (H3): `put_meta` loads, merges
/// and rewrites the whole map atomically, so concurrent writers MUST take
/// the lock or entries would be lost. Clones share one underlying guard.
#[derive(Clone)]
struct CacheAdapter(std::sync::Arc<std::sync::Mutex<cache::Cache>>);

impl CacheAdapter {
    fn new(cache: cache::Cache) -> Self {
        CacheAdapter(std::sync::Arc::new(std::sync::Mutex::new(cache)))
    }
}

impl packages::CacheLike for CacheAdapter {
    fn get_meta(&self, pkg: &str) -> Option<packages::CachedMeta> {
        let guard = self.0.lock().unwrap();
        guard.load_meta().get(&cache::Cache::sanitize_key(pkg)).map(|m| packages::CachedMeta {
            label: m.label.clone(),
            version_code: m.version_code,
        })
    }

    fn put_meta(&self, pkg: &str, meta: &packages::CachedMeta) {
        let cached = cache::CachedMeta {
            label: meta.label.clone(),
            version_code: meta.version_code,
        };
        let _ = self.0.lock().unwrap().put_meta(pkg, &cached);
    }

    fn put_meta_many(&self, entries: &[(String, packages::CachedMeta)]) {
        let cached: Vec<(String, cache::CachedMeta)> = entries
            .iter()
            .map(|(pkg, meta)| {
                (
                    pkg.clone(),
                    cache::CachedMeta { label: meta.label.clone(), version_code: meta.version_code },
                )
            })
            .collect();
        let _ = self.0.lock().unwrap().put_meta_many(&cached);
    }

    fn get_icon(&self, pkg: &str, version_code: u64) -> Option<Vec<u8>> {
        self.0.lock().unwrap().get_icon(pkg, version_code)
    }

    fn put_icon(&self, pkg: &str, version_code: u64, bytes: &[u8]) {
        let _ = self.0.lock().unwrap().put_icon(pkg, version_code, bytes);
    }
}

// ---- DTOs / helpers ----

#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceDto {
    pub serial: String,
    pub state: String, // "device" | "unauthorized" | "offline"
    pub model: Option<String>,
}

fn state_str(state: adb::DeviceState) -> String {
    match state {
        adb::DeviceState::Device => "device".to_string(),
        adb::DeviceState::Unauthorized => "unauthorized".to_string(),
        adb::DeviceState::Offline => "offline".to_string(),
    }
}

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|e| format!("Không lấy được thư mục dữ liệu ứng dụng: {e}"))
}

fn load_config(app: &AppHandle) -> Result<config::AppConfig, String> {
    let dir = app_data_dir(app)?;
    config::load_or_create(&dir)
        .map_err(|e| format!("Không đọc/tạo được config.json: {e}"))
}

fn resolve_adb(cfg: &config::AppConfig) -> adb::Adb {
    let path_env = std::env::var("PATH").unwrap_or_default();
    adb::Adb::new(config::resolve_adb_path(cfg, &path_env))
}

/// Returns the shared adb client, resolving config + adb path once (RwLock
/// write only on first init). All commands use this same `Arc`, so the single
/// `Adb` internal lock serializes every adb invocation process-wide.
/// Note: the resolved adb path is cached — an `adb_path` config change takes
/// effect after an app restart.
fn get_or_init_adb(app: &AppHandle) -> Result<std::sync::Arc<adb::Adb>, String> {
    let state = app.state::<AppState>();
    if let Some(adb) = state.adb.read().unwrap().clone() {
        return Ok(adb);
    }
    let cfg = load_config(app)?;
    let adb = std::sync::Arc::new(resolve_adb(&cfg));
    let mut guard = state.adb.write().unwrap();
    // Another thread may have initialized concurrently; prefer its instance.
    match guard.as_ref() {
        Some(existing) => Ok(existing.clone()),
        None => {
            *guard = Some(adb.clone());
            Ok(adb)
        }
    }
}

/// Run `adb devices` via the shared client and return the first usable device
/// serial together with the freshly loaded config (for blacklist etc.).
fn first_device_serial(app: &AppHandle) -> Result<(String, config::AppConfig), String> {
    let cfg = load_config(app)?;
    let adb = get_or_init_adb(app)?;
    let out = adb.run(&["devices"]).map_err(|e| format!("Không chạy được adb: {e}"))?;
    adb::parse_devices(&out)
        .into_iter()
        .find(|d| d.state == adb::DeviceState::Device)
        .map(|d| (d.serial, cfg))
        .ok_or_else(|| "Không tìm thấy thiết bị kết nối".to_string())
}

/// Fetch a device display name via getprop: `ro.product.marketname` first
/// (vendor marketing name), falling back to `ro.product.model`. Empty or
/// failing output → None.
fn device_model(adb: &adb::Adb, serial: &str) -> Option<String> {
    for prop in ["ro.product.marketname", "ro.product.model"] {
        let model = adb
            .run(&["-s", serial, "shell", "getprop", prop])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if model.is_some() {
            return model;
        }
    }
    None
}

// ---- Commands ----

#[tauri::command]
pub fn list_devices(app: AppHandle) -> Result<Vec<DeviceDto>, String> {
    let adb = get_or_init_adb(&app)?;
    let out = adb.run(&["devices"]).map_err(|e| {
        format!("Không chạy được adb. Hãy kiểm tra adb_path trong config.json. Chi tiết: {e}")
    })?;
    Ok(adb::parse_devices(&out)
        .into_iter()
        .map(|d| {
            // Model is only fetched for a usable device (state = "device").
            let model = match d.state {
                adb::DeviceState::Device => device_model(&adb, &d.serial),
                _ => None,
            };
            DeviceDto { serial: d.serial, state: state_str(d.state), model }
        })
        .collect())
}

/// Progressive listing (G4): return the fast roster immediately (H4 batched
/// single-spawn package lists + cache reads, never a pull) and hydrate cache
/// misses in the background (H2: helper pass ∥ overlay pull pass, both
/// streaming `app-resolved` to the UI). A newer scan bumps the generation
/// and stops emission from the stale task.
#[tauri::command]
pub async fn get_apps(app: AppHandle) -> Result<Vec<packages::AppInfo>, String> {
    let (serial, _cfg) = first_device_serial(&app)?;
    let adb = get_or_init_adb(&app)?;
    let icons_dir = app_data_dir(&app)?;
    let cache = CacheAdapter::new(
        cache::Cache::new(&icons_dir.join("icons"))
            .map_err(|e| format!("Không tạo được thư mục cache icon: {e}"))?,
    );

    let device_adb = DeviceAdb { adb: adb.clone(), serial: Some(serial.clone()) };
    let fast_adb = adb.clone();
    let fast_serial = serial.clone();
    let listing_cache = cache.clone();
    let listing = tauri::async_runtime::spawn_blocking(move || {
        use packages::AdbLike;
        let apk_adapter = ApkAdapter { adb: fast_adb, serial: fast_serial };
        // H4: one adb spawn for all 5 package listings; on any structural
        // surprise (missing markers) fall back to the 5-call sequential path.
        let batched = device_adb
            .run(&["shell", &packages::batched_package_lists_cmd()])
            .ok()
            .and_then(|out| packages::parse_batched_package_lists(&out));
        match batched {
            Some((user, system, user_all, system_all, versions)) => {
                packages::listing_from_parsed(
                    &device_adb, &listing_cache, user, system, user_all, system_all, versions,
                )
            }
            None => packages::list_apps_fast(&device_adb, &apk_adapter, &listing_cache),
        }
    })
    .await
    .map_err(|e| format!("Lỗi khi quét danh sách ứng dụng: {e}"))?;

    if listing.pending.is_empty() {
        return Ok(listing.apps);
    }

    let state = app.state::<AppState>();
    let gen = state.scan_gen.fetch_add(1, Ordering::Relaxed) + 1;
    drop(state);

    let hydration_adb = DeviceAdb { adb: adb.clone(), serial: Some(serial.clone()) };
    let hydrate_app = app.clone();
    let adb_path = adb.path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        use packages::AdbLike;

        let current_gen = |app: &AppHandle| {
            app.state::<AppState>().scan_gen.load(Ordering::Relaxed)
        };
        let paths = hydration_adb
            .run(&["shell", "pm", "list", "packages", "-f"])
            .map(|out| packages::parse_package_paths(&out))
            .unwrap_or_default();

        // H2: overlays (and pathless hidden apps) skip the helper — its
        // resource lookups always fail on them — and pull concurrently with
        // the helper pass instead of after it.
        let (helper_set, pull_set) =
            packages::split_pending_for_hydration(&listing.pending, &paths);

        // Per-worker adb pair for the parallel pull (H3): fresh Adb
        // instances bypass the shared client lock; temp APK files are
        // already unique per call.
        let spawn_pull_context = {
            let adb_path = adb_path.clone();
            let serial = serial.clone();
            move || {
                let adb = std::sync::Arc::new(adb::Adb::new(adb_path.clone()));
                (
                    Box::new(DeviceAdb { adb: adb.clone(), serial: Some(serial.clone()) })
                        as Box<dyn AdbLike + Send>,
                    Box::new(ApkAdapter { adb, serial: serial.clone() })
                        as Box<dyn packages::ApkLike + Send>,
                )
            }
        };
        let emit_resolved = |app: &AppHandle, info: packages::AppInfo| {
            if current_gen(app) != gen {
                return;
            }
            let _ = app.emit("app-resolved", &info);
        };

        let mut helper_leftover: Vec<packages::AppInfo> = Vec::new();
        std::thread::scope(|scope| {
            let paths_for_pull = paths.clone();
            let pull_cache = cache.clone();
            let pull_app = hydrate_app.clone();
            let pull_thread = scope.spawn(move || {
                packages::resolve_pending_parallel(
                    &pull_cache,
                    &spawn_pull_context,
                    &pull_set,
                    &paths_for_pull,
                    PULL_WORKERS,
                    &mut |info| emit_resolved(&pull_app, info),
                );
            });

            let helper_cache = cache.clone();
            let helper = HelperAdapter { adb: adb.clone(), serial: serial.clone() };
            let helper_emit = hydrate_app.clone();
            let helper_thread = scope.spawn(move || {
                packages::resolve_via_helper(
                    &helper_cache,
                    &helper_set,
                    &helper,
                    &mut |info| emit_resolved(&helper_emit, info),
                )
            });

            if let Ok(leftover) = helper_thread.join() {
                helper_leftover = leftover;
            }
            let _ = pull_thread.join();
        });

        // Pass 2: whatever the helper could not resolve (rare — hidden
        // packages, unusual ROM quirks) pulls through the same machinery.
        if !helper_leftover.is_empty() && current_gen(&hydrate_app) == gen {
            let spawn_pass2 = {
                let adb_path = adb_path.clone();
                let serial = serial.clone();
                move || {
                    let adb = std::sync::Arc::new(adb::Adb::new(adb_path.clone()));
                    (
                        Box::new(DeviceAdb { adb: adb.clone(), serial: Some(serial.clone()) })
                            as Box<dyn AdbLike + Send>,
                        Box::new(ApkAdapter { adb, serial: serial.clone() })
                            as Box<dyn packages::ApkLike + Send>,
                    )
                }
            };
            packages::resolve_pending_parallel(
                &cache,
                &spawn_pass2,
                &helper_leftover,
                &paths,
                PULL_WORKERS,
                &mut |info| emit_resolved(&hydrate_app, info),
            );
        }
    });

    Ok(listing.apps)
}

#[tauri::command]
pub async fn uninstall_apps(
    app: AppHandle,
    pkgs: Vec<String>,
) -> Result<Vec<packages::BatchResult>, String> {
    run_batch_cmd(app, pkgs, packages::BatchOp::Uninstall).await
}

#[tauri::command]
pub async fn restore_apps(
    app: AppHandle,
    pkgs: Vec<String>,
) -> Result<Vec<packages::BatchResult>, String> {
    run_batch_cmd(app, pkgs, packages::BatchOp::Restore).await
}

#[tauri::command]
pub fn get_config(app: AppHandle) -> Result<config::AppConfig, String> {
    load_config(&app)
}

/// Shared batch flow: busy-guard, device resolution, blacklist from config,
/// blocking `run_batch` with progress/result events. The busy guard is
/// released on every path via `BusyLease`'s Drop.
async fn run_batch_cmd(
    app: AppHandle,
    pkgs: Vec<String>,
    op: packages::BatchOp,
) -> Result<Vec<packages::BatchResult>, String> {
    if pkgs.is_empty() {
        return Ok(vec![]);
    }

    // Take the state from a cloned handle so `app` stays free to move into
    // the spawn_blocking closure while the lease still borrows state.
    let lease_app = app.clone();
    let state = lease_app.state::<AppState>();
    if !state.busy.try_acquire() {
        return Err("Đang có thao tác khác đang chạy".to_string());
    }
    let _lease = BusyLease(&state.busy);

    let (serial, cfg) = first_device_serial(&app)?;
    let device_adb = DeviceAdb { adb: get_or_init_adb(&app)?, serial: Some(serial) };
    let blacklist = cfg.blacklist;
    let emit_app = app.clone();

    tauri::async_runtime::spawn_blocking(move || {
        packages::run_batch(&device_adb, &pkgs, op, &blacklist, &mut |progress| {
            let _ = emit_app.emit("batch-progress", &progress);
        })
        .into_iter()
        .map(|result| {
            let _ = app.emit("batch-result", &result);
            result
        })
        .collect()
    })
    .await
    .map_err(|e| format!("Lỗi khi chạy thao tác hàng loạt: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_busy_guard_new_try_acquire_true_then_second_false() {
        // Fresh guard: first acquire succeeds, the second is rejected.
        let guard = BusyGuard::new();

        assert!(guard.try_acquire());
        assert!(!guard.try_acquire(), "second acquire while busy must fail");
    }

    #[test]
    fn test_busy_guard_release_allows_acquire_again() {
        // After release the guard can be acquired once more.
        let guard = BusyGuard::new();

        assert!(guard.try_acquire());
        guard.release();

        assert!(guard.try_acquire(), "acquire after release must succeed");
    }

    // ---------- split pull ordering (T-D) ----------

    #[test]
    fn test_split_density_rank_detects_all_buckets() {
        // Arrange/Act/Assert: longest qualifier first so xxxhdpi/xxhdpi/xhdpi
        // are distinguished; non-density splits have no rank.
        assert_eq!(split_density_rank("split_config.xxxhdpi.apk"), Some(6));
        assert_eq!(split_density_rank("split_config.xxhdpi.apk"), Some(5));
        assert_eq!(split_density_rank("split_config.xhdpi.apk"), Some(4));
        assert_eq!(split_density_rank("split_config.hdpi.apk"), Some(3));
        assert_eq!(split_density_rank("split_config.mdpi.apk"), Some(2));
        assert_eq!(split_density_rank("split_config.ldpi.apk"), Some(1));
        assert_eq!(split_density_rank("split_config.arm64_v8a.apk"), None);
        assert_eq!(split_density_rank("split_config.en.apk"), None);
    }

    #[test]
    fn test_order_split_paths_density_ascending_then_others() {
        // Arrange: `ls` output in arbitrary order; the best density must be
        // pulled LAST among density splits, non-density splits after them.
        let out = "split_config.en.apk\r\nsplit_config.xxxhdpi.apk\r\n\
                   base.apk\r\nAndroidManifest.txt\r\nsplit_config.arm64_v8a.apk\r\n\
                   split_config.xxhdpi.apk\r\n";

        // Act
        let ordered = order_split_paths(out, "/data/app/~~x==/com.a-1");

        // Assert: ascending density (hdpi-family), then non-density, base
        // and non-apk entries filtered out.
        assert_eq!(
            ordered,
            vec![
                "/data/app/~~x==/com.a-1/split_config.xxhdpi.apk".to_string(),
                "/data/app/~~x==/com.a-1/split_config.xxxhdpi.apk".to_string(),
                "/data/app/~~x==/com.a-1/split_config.en.apk".to_string(),
                "/data/app/~~x==/com.a-1/split_config.arm64_v8a.apk".to_string(),
            ]
        );
    }

    #[test]
    fn test_order_split_paths_handles_full_paths_and_empty_output() {
        // Arrange: some devices echo full paths; empty output is possible.
        let full = "/data/app/x/split_config.hdpi.apk\n";

        // Act/Assert
        assert_eq!(
            order_split_paths(full, "/data/app/x"),
            vec!["/data/app/x/split_config.hdpi.apk".to_string()]
        );
        assert!(order_split_paths("", "/data/app/x").is_empty());
    }

    // ---------- lazy split trigger (R2-F3) ----------

    #[test]
    fn test_should_try_splits_none_weak_and_healthy() {
        // No icon -> the docs-app flow: splits must be pulled.
        assert!(should_try_splits(&None));

        // Solid background-only render (Gboard): weak -> must try splits.
        let solid = crate::vector::render_svg(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"64\" height=\"64\">\
             <rect width=\"64\" height=\"64\" fill=\"#4285f4\"/></svg>",
            64,
        )
        .expect("svg renders");
        assert!(should_try_splits(&Some(solid)), "weak icon must trigger split pull");

        // Content-rich icon: healthy -> no split pull.
        let rich = crate::vector::render_svg(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"64\" height=\"64\">\
             <rect width=\"64\" height=\"64\" fill=\"#4285f4\"/>\
             <circle cx=\"20\" cy=\"20\" r=\"12\" fill=\"#ffffff\"/>\
             <circle cx=\"44\" cy=\"44\" r=\"12\" fill=\"#ea4335\"/>\
             <rect x=\"28\" y=\"28\" width=\"10\" height=\"10\" fill=\"#34a853\"/></svg>",
            64,
        )
        .expect("svg renders");
        assert!(!should_try_splits(&Some(rich)), "healthy icon must not trigger split pull");
    }
}
