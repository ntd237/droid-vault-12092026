// T3.2 — packages service layer (Wave 3).
// Lists apps (user/system + restorable badge, label/icon via cache-first) and
// runs sequential batch operations (uninstall/restore) with blacklist
// enforcement and continue-on-error. All external dependencies are injected
// via traits so the module is unit-testable with mocks and compiles
// standalone (lib.rs wiring is owned by Wave 4).

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

// ---- Injected dependencies ----

/// ADB command runner abstraction (see `adb::Adb::run` for the real impl).
pub trait AdbLike {
    /// Run one adb command, e.g. args = ["shell", "pm", "list", "packages", "-3"].
    /// Returns stdout or error text.
    fn run(&self, args: &[&str]) -> Result<String, String>;
}

/// Parses an APK file → (label, icon bytes, version_code when known).
pub trait ApkLike {
    fn parse(&self, apk_path: &str) -> Result<(String, Option<Vec<u8>>, Option<u64>), String>;
}

/// Cache-first store for per-package metadata and icons.
pub trait CacheLike {
    fn get_meta(&self, pkg: &str) -> Option<CachedMeta>;
    fn put_meta(&self, pkg: &str, meta: &CachedMeta);
    /// Bulk variant (H3b) — default loops `put_meta`; real adapters override
    /// with a single merged meta.json rewrite.
    fn put_meta_many(&self, entries: &[(String, CachedMeta)]) {
        for (pkg, meta) in entries {
            self.put_meta(pkg, meta);
        }
    }
    fn get_icon(&self, pkg: &str, version_code: u64) -> Option<Vec<u8>>;
    fn put_icon(&self, pkg: &str, version_code: u64, bytes: &[u8]);
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CachedMeta {
    pub label: String,
    pub version_code: u64,
}

// ---- Domain ----

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub enum AppKind {
    User,
    System,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AppInfo {
    pub package: String,
    pub label: String,
    pub kind: AppKind,
    /// false ⇔ uninstalled for user 0 but restorable
    pub installed: bool,
    /// True ⇔ `cmd package install-existing` can restore this package
    /// (system kind only — data apps uninstalled for user 0 cannot be
    /// restored via adb, they only vanish from the -u enumeration).
    pub restorable: bool,
    pub icon_base64: Option<String>,
    pub version_code: Option<u64>,
}

/// Pure: restorable set = the `-u` enumeration of one kind (which includes
/// packages uninstalled for user 0) minus the matching installed-only list.
pub fn restorable_set(all_with_u: &[String], installed: &[String]) -> HashSet<String> {
    let installed: HashSet<&String> = installed.iter().collect();
    all_with_u.iter().filter(|p| !installed.contains(p)).cloned().collect()
}

/// Parse `pm list packages` output (adb.rs convention): lines "package:<name>".
fn parse_package_list(output: &str) -> Vec<String> {
    output
        .lines()
        .map(|l| l.trim())
        .filter_map(|l| l.strip_prefix("package:"))
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
        .collect()
}

/// Marker separating the 5 segments of the batched package-list command —
/// distinctive enough that no `pm list` line can ever contain it.
pub const BATCHED_LIST_SEP: &str = "@@DV1@@";

/// Parses the output of one batched `adb shell` call that runs all 5 package
/// listings separated by `echo BATCHED_LIST_SEP` (H4 — a single adb spawn
/// instead of 5). Segments in order: user(-3), system(-s), user_all(-u -3),
/// system_all(-u -s), versions(--show-versioncode). `None` when the marker
/// count is wrong (shell mangled the batch) so the caller can fall back to
/// the sequential per-call path.
pub fn parse_batched_package_lists(
    output: &str,
) -> Option<(
    Vec<String>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    HashMap<String, u64>,
)> {
    let segments: Vec<&str> = output.split(BATCHED_LIST_SEP).collect();
    if segments.len() != 5 {
        return None;
    }
    Some((
        parse_package_list(segments[0]),
        parse_package_list(segments[1]),
        parse_package_list(segments[2]),
        parse_package_list(segments[3]),
        parse_package_versions(segments[4]),
    ))
}

/// Pick the apk path from `pm path <pkg>` output (adb.rs convention): prefer
/// the line containing "base.apk"; otherwise the first line ending in ".apk".
fn pick_base_apk(output: &str) -> Option<String> {
    let mut fallback: Option<&str> = None;
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let path = line.strip_prefix("package:").unwrap_or(line);
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

/// Parse `pm list packages --show-versioncode` output (Android 12+): lines
/// like "package:com.example versionCode:1234" → map pkg → versionCode.
/// Lines without a parsable versionCode (or unsupported-flag junk output)
/// are skipped; an empty map means the flag is unsupported.
fn parse_package_versions(output: &str) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    for line in output.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("package:") else {
            continue;
        };
        let mut tokens = rest.split_whitespace();
        let Some(pkg) = tokens.next() else {
            continue;
        };
        if pkg.is_empty() {
            continue;
        }
        for token in tokens {
            if let Some(v) = token.strip_prefix("versionCode:") {
                if let Ok(version) = v.parse::<u64>() {
                    map.insert(pkg.to_string(), version);
                }
                break;
            }
        }
    }
    map
}

/// Parse `dumpsys package <pkg>` output for the versionCode: the line may
/// carry suffixes like "versionCode=1234 minSdk=21". Returns the first
/// parsable value, `None` when absent (older devices / parse failure).
fn parse_dumpsys_version_code(output: &str) -> Option<u64> {
    for line in output.lines() {
        for token in line.split_whitespace() {
            if let Some(v) = token.strip_prefix("versionCode=") {
                let digits: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(version) = digits.parse::<u64>() {
                    return Some(version);
                }
            }
        }
    }
    None
}

/// pkg → device APK path from `pm list packages -f` output
/// ("package:<path>=<pkg>"). Split at the LAST '=' — device paths may
/// contain '=' themselves (e.g. /data/app/~~x==/pkg-1/base.apk).
pub fn parse_package_paths(output: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in output.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("package:") else { continue };
        let Some((path, pkg)) = rest.rsplit_once('=') else { continue };
        if path.is_empty() || pkg.is_empty() || !path.ends_with(".apk") {
            continue;
        }
        map.insert(pkg.to_string(), path.to_string());
    }
    map
}

/// Splits pending cache-misses into (helper set, parallel pull set) — H2.
/// Measured on-device: EVERY package with a known APK path resolves through
/// the helper (393/393 OK), while ALL 66 helper failures are pathless
/// (hidden packages `pm list -f` never lists). Pathless apps therefore go
/// straight to the pull pass — where `pm path` fails fast into the negative
/// cache — and run concurrently with the helper instead of trailing it.
pub fn split_pending_for_hydration(
    pending: &[AppInfo],
    paths: &HashMap<String, String>,
) -> (Vec<AppInfo>, Vec<AppInfo>) {
    pending
        .iter()
        .cloned()
        .partition(|a| paths.contains_key(&a.package))
}

/// (size, path) pairs from `ls -l` output (toybox shape:
/// perms links owner group size date time path). Unparsable and error
/// lines are skipped; the path side keeps any inner spaces.
pub fn parse_ls_sizes(output: &str) -> Vec<(u64, String)> {
    let mut sizes = Vec::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 {
            continue;
        }
        let Ok(size) = fields[4].parse::<u64>() else { continue };
        sizes.push((size, fields[7..].join(" ")));
    }
    sizes
}

/// Result of the non-blocking fast listing: the full roster (with cache
/// misses degraded to pkg-name entries) plus the degraded subset the
/// background hydrator must resolve.
#[derive(Debug, Clone)]
pub struct Listing {
    pub apps: Vec<AppInfo>,
    pub pending: Vec<AppInfo>,
}

/// Fast listing pass (G3): 5 `pm list` calls + cache reads only — NEVER
/// `pm path`, never a pull, never a parse. Cache misses degrade in place
/// (pkg-name label, no icon, device version when known) and are collected
/// in `pending` for `resolve_pending`.
pub fn list_apps_fast(adb: &dyn AdbLike, apk: &dyn ApkLike, cache: &dyn CacheLike) -> Listing {
    let _ = apk; // parity with list_apps signature; fast pass never parses.

    // Installed-only lists (source of truth for installed=true), pinned to
    // user 0.
    let user = parse_package_list(
        &adb.run(&["shell", "pm", "list", "packages", "-3", "--user", "0"]).unwrap_or_default(),
    );
    let system = parse_package_list(
        &adb.run(&["shell", "pm", "list", "packages", "-s", "--user", "0"]).unwrap_or_default(),
    );
    // The `-u` variants additionally enumerate packages uninstalled for
    // user 0. If a ROM rejects `-u` (run error), fall back to the installed
    // list so every entry stays installed=true.
    let user_all = adb
        .run(&["shell", "pm", "list", "packages", "-u", "-3", "--user", "0"])
        .map(|out| parse_package_list(&out))
        .unwrap_or_else(|_| user.clone());
    let system_all = adb
        .run(&["shell", "pm", "list", "packages", "-u", "-s", "--user", "0"])
        .map(|out| parse_package_list(&out))
        .unwrap_or_else(|_| system.clone());

    let version_map = parse_package_versions(
        &adb
            .run(&["shell", "pm", "list", "packages", "--show-versioncode"])
            .unwrap_or_default(),
    );

    listing_from_parsed(adb, cache, user, system, user_all, system_all, version_map)
}

/// Shell command (H4) that produces all 5 package listings in ONE adb spawn,
/// segments separated by `echo BATCHED_LIST_SEP`. Parsed by
/// `parse_batched_package_lists`; feed its output to `listing_from_parsed`.
pub fn batched_package_lists_cmd() -> String {
    format!(
        "pm list packages -3 --user 0; echo {sep}; \
         pm list packages -s --user 0; echo {sep}; \
         pm list packages -u -3 --user 0; echo {sep}; \
         pm list packages -u -s --user 0; echo {sep}; \
         pm list packages --show-versioncode",
        sep = BATCHED_LIST_SEP
    )
}

/// Listing construction from pre-parsed package lists — the shared body of
/// the sequential fast pass and the H4 batched single-spawn path. Runs the
/// pre-Android-12 dumpsys version fallback for cached packages when the
/// version list is empty.
pub fn listing_from_parsed(
    adb: &dyn AdbLike,
    cache: &dyn CacheLike,
    user: Vec<String>,
    system: Vec<String>,
    user_all: Vec<String>,
    system_all: Vec<String>,
    version_map: HashMap<String, u64>,
) -> Listing {
    let mut listing = Listing { apps: Vec::new(), pending: Vec::new() };

    let restorable_user = restorable_set(&user_all, &user);
    let restorable_system: Vec<String> = restorable_set(&system_all, &system)
        .into_iter()
        .filter(|p| !restorable_user.contains(p))
        .collect();

    let entries: Vec<(String, AppKind, bool)> = user
        .iter()
        .map(|p| (p.clone(), AppKind::User, true))
        .chain(system.iter().map(|p| (p.clone(), AppKind::System, true)))
        .chain(restorable_user.iter().map(|p| (p.clone(), AppKind::User, false)))
        .chain(restorable_system.iter().map(|p| (p.clone(), AppKind::System, false)))
        .collect();

    // Fallback for pre-Android-12 devices: probe dumpsys only for packages
    // that would otherwise be served from cache.
    let dumpsys_versions: HashMap<String, Option<u64>> = if version_map.is_empty() {
        entries
            .iter()
            .filter(|(pkg, _, _)| cache.get_meta(pkg).is_some())
            .map(|(pkg, _, _)| {
                let v = adb
                    .run(&["shell", "dumpsys", "package", pkg])
                    .ok()
                    .and_then(|out| parse_dumpsys_version_code(&out));
                (pkg.clone(), v)
            })
            .collect()
    } else {
        HashMap::new()
    };

    for (pkg, kind, installed) in entries {
        let device_version = version_map
            .get(&pkg)
            .copied()
            .or_else(|| dumpsys_versions.get(&pkg).copied().flatten());
        let restorable = kind == AppKind::System;
        match cached_app_info(cache, &pkg, kind, installed, restorable, device_version) {
            Some(info) => listing.apps.push(info),
            None => {
                let degraded = AppInfo {
                    package: pkg.clone(),
                    label: pkg,
                    kind,
                    installed,
                    restorable,
                    icon_base64: None,
                    version_code: device_version,
                };
                listing.apps.push(degraded.clone());
                listing.pending.push(degraded);
            }
        }
    }
    listing
}

/// Background hydration (G3): resolve each pending (degraded) entry via
/// pull + parse, caching as usual, and fire `on_resolved` once per
/// successfully re-resolved app in pending order. Packages already cached
/// (overlapping newer scan) are skipped silently. `paths` (from
/// `parse_package_paths`) replaces the per-app `pm path` call when the
/// package is covered; otherwise the old `pm path` fallback applies.
pub fn resolve_pending(
    adb: &dyn AdbLike,
    apk: &dyn ApkLike,
    cache: &dyn CacheLike,
    pending: &[AppInfo],
    paths: &HashMap<String, String>,
    on_resolved: &mut dyn FnMut(AppInfo),
) {
    for degraded in pending {
        let pkg = &degraded.package;
        if cached_app_info(
            cache,
            pkg,
            degraded.kind,
            degraded.installed,
            degraded.restorable,
            degraded.version_code,
        )
        .is_some()
        {
            continue;
        }
        let mapped = paths.get(pkg);
        let info = resolve_miss(
            adb, apk, cache, pkg, degraded.kind, degraded.installed, degraded.version_code, mapped,
        );
        on_resolved(info);
    }
}

/// Cache-hit resolution: `None` when the entry is missing or provably stale.
/// When the device version is unknown (both detection methods failed) the
/// cache is served as-is — we cannot prove staleness.
fn cached_app_info(
    cache: &dyn CacheLike,
    pkg: &str,
    kind: AppKind,
    installed: bool,
    restorable: bool,
    device_version: Option<u64>,
) -> Option<AppInfo> {
    let meta = cache.get_meta(pkg)?;
    if device_version.map_or(false, |v| v != meta.version_code) {
        return None;
    }
    let icon_base64 = cache.get_icon(pkg, meta.version_code).map(|bytes| base64_encode(&bytes));
    Some(AppInfo {
        package: pkg.to_string(),
        label: meta.label,
        kind,
        installed,
        restorable,
        icon_base64,
        version_code: Some(meta.version_code),
    })
}

/// Cache-miss resolution (the old `resolve_app_info` miss path): prefer the
/// `-f`-provided path, else `pm path`; parse, cache (positive or negative),
/// degrade to pkg name. Never fails the listing.
fn resolve_miss(
    adb: &dyn AdbLike,
    apk: &dyn ApkLike,
    cache: &dyn CacheLike,
    pkg: &str,
    kind: AppKind,
    installed: bool,
    device_version: Option<u64>,
    mapped_path: Option<&String>,
) -> AppInfo {
    let degraded = AppInfo {
        package: pkg.to_string(),
        label: pkg.to_string(),
        kind,
        installed,
        restorable: kind == AppKind::System,
        icon_base64: None,
        version_code: device_version,
    };
    let outcome = compute_miss(adb, apk, &degraded, mapped_path);
    let mut batch = MetaBatch::new(cache);
    commit_miss(cache, &mut batch, &degraded, outcome)
}

/// Worker half of a cache miss (H3): resolve the APK (pull + parse) WITHOUT
/// touching the cache — safe to run on parallel workers. `negative` marks
/// pull-denied/unparseable/no-path APKs for the negative-cache commit.
pub(crate) struct MissOutcome {
    pub label: String,
    pub icon_png: Option<Vec<u8>>,
    pub version_code: Option<u64>,
    pub negative: bool,
}

fn compute_miss(
    adb: &dyn AdbLike,
    apk: &dyn ApkLike,
    degraded: &AppInfo,
    mapped_path: Option<&String>,
) -> MissOutcome {
    let apk_path = match mapped_path {
        Some(path) => Some(path.clone()),
        None => adb
            .run(&["shell", "pm", "path", &degraded.package])
            .ok()
            .and_then(|out| pick_base_apk(&out)),
    };

    // A parse outcome of Err covers pull-denied APKs (RRO overlays),
    // unparseable APKs and adb failures — all degrade to label = package
    // name.
    let outcome = match apk_path {
        Some(path) => apk.parse(&path).map_err(|_| ()),
        None => Err(()),
    };

    match outcome {
        Ok((label, icon, version)) => {
            // B1: a parse that yields no static label (system
            // APKs) degrades to the package name — and still
            // caches — so the entry is not re-pulled forever.
            let label = if label.is_empty() { degraded.package.clone() } else { label };
            MissOutcome { label, icon_png: icon, version_code: version, negative: false }
        }
        Err(()) => MissOutcome {
            label: degraded.package.clone(),
            icon_png: None,
            version_code: None,
            negative: true,
        },
    }
}

/// H3b: buffers meta writes in the collector and flushes them in bulk — one
/// merged meta.json rewrite per `META_FLUSH_EVERY` results instead of one
/// per app (459 per-app rewrites cost seconds on a cold cache). Icons stay
/// immediate per-file atomic writes. Drop flushes the remainder, so the
/// cache is complete whenever the resolve function returns.
const META_FLUSH_EVERY: usize = 16;

struct MetaBatch<'a> {
    cache: &'a dyn CacheLike,
    metas: Vec<(String, CachedMeta)>,
}

impl<'a> MetaBatch<'a> {
    fn new(cache: &'a dyn CacheLike) -> Self {
        MetaBatch { cache, metas: Vec::new() }
    }

    fn put(&mut self, pkg: &str, meta: CachedMeta) {
        self.metas.push((pkg.to_string(), meta));
        if self.metas.len() >= META_FLUSH_EVERY {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.metas.is_empty() {
            return;
        }
        let entries = std::mem::take(&mut self.metas);
        self.cache.put_meta_many(&entries);
    }
}

impl Drop for MetaBatch<'_> {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Collector half of a cache miss (H3): write positive/negative cache and
/// build the final AppInfo. Only ever called from the single collector
/// thread, so meta.json writes never race — and they go through `batch`
/// (H3b) so they land as merged bulk rewrites.
fn commit_miss(
    cache: &dyn CacheLike,
    batch: &mut MetaBatch,
    degraded: &AppInfo,
    outcome: MissOutcome,
) -> AppInfo {
    let pkg = degraded.package.as_str();
    if outcome.negative {
        // B1b: the failure is deterministic (pull denied for /product/overlay
        // APKs, corrupt APK). Negative-cache pkg-name meta when the device
        // version is known so later listings hit the cache instead of
        // re-pulling every refresh. Without a known version nothing is
        // stored — hidden (uninstalled) packages are absent from the version
        // list and must stay re-resolvable.
        if let Some(v) = degraded.version_code {
            batch.put(pkg, CachedMeta { label: pkg.to_string(), version_code: v });
        }
        return AppInfo {
            package: degraded.package.clone(),
            label: pkg.to_string(),
            kind: degraded.kind,
            installed: degraded.installed,
            restorable: degraded.restorable,
            icon_base64: None,
            version_code: None,
        };
    }
    if let Some(v) = outcome.version_code {
        // Meta (and icon) only cached when the version is known.
        batch.put(pkg, CachedMeta { label: outcome.label.clone(), version_code: v });
        if let Some(bytes) = &outcome.icon_png {
            cache.put_icon(pkg, v, bytes);
        }
    }
    AppInfo {
        package: degraded.package.clone(),
        label: outcome.label,
        kind: degraded.kind,
        installed: degraded.installed,
        restorable: degraded.restorable,
        icon_base64: outcome.icon_png.as_ref().map(|bytes| base64_encode(bytes)),
        version_code: outcome.version_code,
    }
}

/// Parallel pull fallback (H3): resolves each pending cache-miss on
/// `workers` threads, each owning its own adb pair (no shared client lock,
/// no shared temp file). Results flow over a channel to the CALLING thread,
/// which commits cache writes and fires `on_resolved` — so meta.json
/// read-merge-writes and event emission stay serialized and ordered-safe.
pub fn resolve_pending_parallel(
    cache: &dyn CacheLike,
    spawn_context: &(dyn Fn() -> (Box<dyn AdbLike + Send>, Box<dyn ApkLike + Send>) + Sync),
    pending: &[AppInfo],
    paths: &HashMap<String, String>,
    workers: usize,
    on_resolved: &mut dyn FnMut(AppInfo),
) {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::mpsc;

    if pending.is_empty() {
        return;
    }
    let worker_count = workers.clamp(1, pending.len());
    let next = std::sync::Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel::<(usize, MissOutcome)>();

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let tx = tx.clone();
            let next = std::sync::Arc::clone(&next);
            scope.spawn(move || {
                let (adb, apk) = spawn_context();
                loop {
                    let i = next.fetch_add(1, AtomicOrdering::Relaxed);
                    if i >= pending.len() {
                        break;
                    }
                    let item = &pending[i];
                    let mapped = paths.get(&item.package);
                    let outcome = compute_miss(adb.as_ref(), apk.as_ref(), item, mapped);
                    if tx.send((i, outcome)).is_err() {
                        break;
                    }
                }
            });
        }
        // Drop our sender so recv ends when all workers are done.
        drop(tx);
        let mut batch = MetaBatch::new(cache);
        while let Ok((i, outcome)) = rx.recv() {
            on_resolved(commit_miss(cache, &mut batch, &pending[i], outcome));
        }
    });
}

/// Blocking full listing: fast pass + inline hydration. Kept for callers
/// (and tests) that want the complete roster in one call.
pub fn list_apps(adb: &dyn AdbLike, apk: &dyn ApkLike, cache: &dyn CacheLike) -> Vec<AppInfo> {
    let mut listing = list_apps_fast(adb, apk, cache);
    let mut resolved: Vec<AppInfo> = Vec::new();
    resolve_pending(adb, apk, cache, &listing.pending, &HashMap::new(), &mut |info| {
        resolved.push(info);
    });
    for info in resolved {
        if let Some(slot) = listing.apps.iter_mut().find(|a| a.package == info.package) {
            *slot = info;
        }
    }
    listing.apps
}

// ---- Batch ----

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BatchOp {
    Uninstall,
    Restore,
}

#[derive(Debug, Serialize)]
pub struct BatchResult {
    pub package: String,
    pub success: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BatchProgress {
    pub index: usize,
    pub total: usize,
    pub package: String,
}

const BLACKLIST_BLOCKED_MSG: &str = "Blocked: package thuộc danh sách cấm gỡ";

/// Sequential batch (NEVER parallel — one device). Blacklisted packages are
/// blocked BEFORE any adb call. On adb error the batch records the failure and
/// continues with the next package. `on_progress` fires right before each adb
/// attempt (after the blacklist check).
pub fn run_batch(
    adb: &dyn AdbLike,
    pkgs: &[String],
    op: BatchOp,
    blacklist: &[String],
    on_progress: &mut dyn FnMut(BatchProgress),
) -> Vec<BatchResult> {
    let blocked: HashSet<&String> = blacklist.iter().collect();
    let mut results = Vec::with_capacity(pkgs.len());
    for (index, pkg) in pkgs.iter().enumerate() {
        if blocked.contains(pkg) {
            results.push(BatchResult {
                package: pkg.clone(),
                success: false,
                message: BLACKLIST_BLOCKED_MSG.to_string(),
            });
            continue;
        }
        on_progress(BatchProgress { index, total: pkgs.len(), package: pkg.clone() });
        let args: Vec<&str> = match op {
            BatchOp::Uninstall => vec!["shell", "pm", "uninstall", "-k", "--user", "0", pkg],
            BatchOp::Restore => vec!["shell", "cmd", "package", "install-existing", pkg],
        };
        let result = match adb.run(&args) {
            Ok(stdout) => {
                BatchResult { package: pkg.clone(), success: true, message: stdout.trim().to_string() }
            }
            Err(e) => BatchResult { package: pkg.clone(), success: false, message: e },
        };
        results.push(result);
    }
    results
}

/// Minimal standard-alphabet base64 encoder (no external crates allowed).
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b1 = chunk[0] as u32;
        let b2 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b3 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b1 << 16) | (b2 << 8) | b3;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Standard-alphabet base64 decoder (no external crates allowed). Whitespace
/// is skipped; `None` on any other invalid input.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let unpadded: Vec<u8> = bytes.iter().copied().take_while(|&b| b != b'=').collect();
    if bytes.len() - unpadded.len() > 2 {
        return None;
    }
    if !bytes[unpadded.len()..].iter().all(|&b| b == b'=') {
        return None;
    }
    let mut out = Vec::with_capacity(unpadded.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &b in &unpadded {
        buf = (buf << 6) | val(b)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

// ---- On-device helper (hướng 2): label/icon qua PackageManager, 0 byte APK ----

/// One line of DvHelper stdout.
#[derive(Debug, Clone, PartialEq)]
pub enum HelperOutcome {
    Ok { package: String, label: String, icon_png: Option<Vec<u8>> },
    Err { package: String },
}

/// Parse one `OK\t<pkg>\t<label_b64>\t<icon_b64 | ->` / `ERR\t<pkg>\t<reason_b64>`
/// line. `None` for anything malformed (skipped by the caller).
pub fn parse_helper_line(line: &str) -> Option<HelperOutcome> {
    let parts: Vec<&str> = line.split('\t').collect();
    let decoded = |s: &str| -> Option<String> {
        base64_decode(s).and_then(|b| String::from_utf8(b).ok())
    };
    match parts.first().copied()? {
        "OK" if parts.len() == 4 => {
            let package = parts[1].to_string();
            if package.is_empty() {
                return None;
            }
            let label = decoded(parts[2])?;
            if parts[3] == "-" {
                Some(HelperOutcome::Ok { package, label, icon_png: None })
            } else {
                let icon_png = base64_decode(parts[3])?;
                Some(HelperOutcome::Ok { package, label, icon_png: Some(icon_png) })
            }
        }
        "ERR" if parts.len() == 3 => {
            let package = parts[1].to_string();
            if package.is_empty() {
                return None;
            }
            decoded(parts[2])?;
            Some(HelperOutcome::Err { package })
        }
        _ => None,
    }
}

/// Runs the DvHelper dex on the device and streams its output lines.
/// Implemented in commands.rs over `adb push` + `adb shell` with stdin.
pub trait HelperRunner {
    /// Feed `pkgs` (one per line) to the helper and invoke `on_line` per
    /// stdout line. `Err` = fatal: the helper cannot produce results at all
    /// (blocked ROM, spawn failure, timeout with no output). Per-package
    /// problems arrive as parsed `ERR` lines instead — not fatal.
    fn query(&self, pkgs: &[String], on_line: &mut dyn FnMut(&str)) -> Result<(), String>;
}

/// Resolve pending apps through the on-device helper. Returns the apps the
/// helper did NOT resolve (per-package ERR lines, unknown packages, or — on
/// a fatal failure — the ENTIRE pending list) so the caller falls back to
/// pulling APKs for exactly those.
pub fn resolve_via_helper(
    cache: &dyn CacheLike,
    pending: &[AppInfo],
    runner: &dyn HelperRunner,
    on_resolved: &mut dyn FnMut(AppInfo),
) -> Vec<AppInfo> {
    let pkgs: Vec<String> = pending.iter().map(|a| a.package.clone()).collect();
    let mut resolved: HashMap<String, AppInfo> = HashMap::new();
    let mut batch = MetaBatch::new(cache);
    let result = runner.query(&pkgs, &mut |line| {
        let Some(HelperOutcome::Ok { package, label, icon_png }) = parse_helper_line(line)
        else {
            return; // ERR lines / malformed lines stay leftover by absence
        };
        let Some(degraded) = pending.iter().find(|a| a.package == package) else {
            return;
        };
        let version_code = degraded.version_code;
        if let Some(v) = version_code {
            batch.put(&package, CachedMeta { label: label.clone(), version_code: v });
            if let Some(bytes) = &icon_png {
                cache.put_icon(&package, v, bytes);
            }
        }
        resolved.insert(
            package.clone(),
            AppInfo {
                package,
                label,
                kind: degraded.kind,
                installed: degraded.installed,
                restorable: degraded.restorable,
                icon_base64: icon_png.as_ref().map(|bytes| base64_encode(bytes)),
                version_code,
            },
        );
        if let Some(info) = resolved.get(&degraded.package) {
            on_resolved(info.clone());
        }
    });
    match result {
        Ok(()) => pending
            .iter()
            .filter(|a| !resolved.contains_key(&a.package))
            .cloned()
            .collect(),
        // Fatal: the helper produced nothing usable — hand everything over.
        Err(_) => pending.to_vec(),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    // ---------- Mocks ----------

    /// Mock adb: records every call; serves canned responses for the
    /// `pm list packages ...` variants (installed-only and `-u` per kind),
    /// per-package `pm path`, per-package `dumpsys package`, and batch ops.
    /// `fail_u` simulates a ROM rejecting the `-u` flag.
    struct MockAdb {
        calls: RefCell<Vec<Vec<String>>>,
        list_user: String,
        list_system: String,
        list_user_u: String,
        list_system_u: String,
        fail_u: bool,
        show_versioncode: String,
        dumpsys: HashMap<String, String>,
        pm_path: HashMap<String, Result<String, String>>,
        batch_out: HashMap<String, Result<String, String>>,
    }

    impl Default for MockAdb {
        fn default() -> Self {
            MockAdb {
                calls: RefCell::new(Vec::new()),
                list_user: String::new(),
                list_system: String::new(),
                list_user_u: String::new(),
                list_system_u: String::new(),
                fail_u: false,
                show_versioncode: String::new(),
                dumpsys: HashMap::new(),
                pm_path: HashMap::new(),
                batch_out: HashMap::new(),
            }
        }
    }

    impl MockAdb {
        fn batch_out(mut self, pkg: &str, res: Result<&str, &str>) -> Self {
            self.batch_out
                .insert(pkg.to_string(), res.map(|s| s.to_string()).map_err(|e| e.to_string()));
            self
        }

        fn with_versioncode(mut self, out: &str) -> Self {
            self.show_versioncode = out.to_string();
            self
        }

        fn with_dumpsys(mut self, pkg: &str, out: &str) -> Self {
            self.dumpsys.insert(pkg.to_string(), out.to_string());
            self
        }

        fn calls_snapshot(&self) -> Vec<Vec<String>> {
            self.calls.borrow().clone()
        }
    }

    impl AdbLike for MockAdb {
        fn run(&self, args: &[&str]) -> Result<String, String> {
            self.calls.borrow_mut().push(args.iter().map(|s| s.to_string()).collect());
            if args.len() >= 5 && args[1] == "pm" && args[2] == "list" && args[3] == "packages" {
                if args[4] == "-u" {
                    if self.fail_u {
                        return Err("pm list packages: unsupported flag -u".to_string());
                    }
                    return match args.get(5).copied() {
                        Some("-3") => Ok(self.list_user_u.clone()),
                        Some("-s") => Ok(self.list_system_u.clone()),
                        _ => Ok(String::new()),
                    };
                }
                if args[4] == "-3" {
                    return Ok(self.list_user.clone());
                }
                if args[4] == "-s" {
                    return Ok(self.list_system.clone());
                }
                if args[4] == "--show-versioncode" {
                    return Ok(self.show_versioncode.clone());
                }
            }
            if args.len() == 4 && args[1] == "dumpsys" && args[2] == "package" {
                return Ok(self.dumpsys.get(args[3]).cloned().unwrap_or_default());
            }
            if args.len() == 4 && args[1] == "pm" && args[2] == "path" {
                return self
                    .pm_path
                    .get(args[3])
                    .cloned()
                    .unwrap_or_else(|| Ok(String::new()));
            }
            if let Some(res) = self.batch_out.get(args[args.len() - 1]) {
                return res.clone();
            }
            Ok(String::from("Success\n"))
        }
    }

    /// Mock apk parser: records parsed paths; serves canned parse results per path.
    struct MockApk {
        calls: RefCell<Vec<String>>,
        results: HashMap<String, Result<(String, Option<Vec<u8>>, Option<u64>), String>>,
    }

    impl Default for MockApk {
        fn default() -> Self {
            MockApk { calls: RefCell::new(Vec::new()), results: HashMap::new() }
        }
    }

    impl MockApk {
        fn result(mut self, path: &str, res: Result<(&str, Option<Vec<u8>>, Option<u64>), &str>) -> Self {
            self.results.insert(
                path.to_string(),
                res.map(|(l, i, v)| (l.to_string(), i, v)).map_err(|e| e.to_string()),
            );
            self
        }

        fn calls_snapshot(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl ApkLike for MockApk {
        fn parse(&self, apk_path: &str) -> Result<(String, Option<Vec<u8>>, Option<u64>), String> {
            self.calls.borrow_mut().push(apk_path.to_string());
            self.results
                .get(apk_path)
                .cloned()
                .unwrap_or_else(|| Ok((String::from("Default"), None, None)))
        }
    }

    /// Mock cache: in-memory meta and icon maps.
    struct MockCache {
        meta: RefCell<HashMap<String, CachedMeta>>,
        icons: RefCell<HashMap<(String, u64), Vec<u8>>>,
    }

    impl Default for MockCache {
        fn default() -> Self {
            MockCache {
                meta: RefCell::new(HashMap::new()),
                icons: RefCell::new(HashMap::new()),
            }
        }
    }

    impl MockCache {
        fn with_meta(self, pkg: &str, label: &str, version_code: u64) -> Self {
            self.meta
                .borrow_mut()
                .insert(pkg.to_string(), CachedMeta { label: label.to_string(), version_code });
            self
        }

        fn with_icon(self, pkg: &str, version_code: u64, bytes: Vec<u8>) -> Self {
            self.icons.borrow_mut().insert((pkg.to_string(), version_code), bytes);
            self
        }

        fn meta_of(&self, pkg: &str) -> Option<CachedMeta> {
            self.meta.borrow().get(pkg).cloned()
        }

        fn icon_of(&self, pkg: &str, version_code: u64) -> Option<Vec<u8>> {
            self.icons.borrow().get(&(pkg.to_string(), version_code)).cloned()
        }
    }

    impl CacheLike for MockCache {
        fn get_meta(&self, pkg: &str) -> Option<CachedMeta> {
            self.meta.borrow().get(pkg).cloned()
        }

        fn put_meta(&self, pkg: &str, meta: &CachedMeta) {
            self.meta.borrow_mut().insert(pkg.to_string(), meta.clone());
        }

        fn get_icon(&self, pkg: &str, version_code: u64) -> Option<Vec<u8>> {
            self.icons.borrow().get(&(pkg.to_string(), version_code)).cloned()
        }

        fn put_icon(&self, pkg: &str, version_code: u64, bytes: &[u8]) {
            self.icons.borrow_mut().insert((pkg.to_string(), version_code), bytes.to_vec());
        }
    }

    fn find<'a>(apps: &'a [AppInfo], pkg: &str) -> &'a AppInfo {
        apps.iter().find(|a| a.package == pkg).unwrap_or_else(|| panic!("missing app {pkg}"))
    }

    fn degraded(pkg: &str, kind: AppKind, installed: bool, version: Option<u64>) -> AppInfo {
        AppInfo {
            package: pkg.to_string(),
            label: pkg.to_string(),
            kind,
            installed,
            restorable: kind == AppKind::System,
            icon_base64: None,
            version_code: version,
        }
    }

    /// Mock helper runner: replays canned output lines; `failing` simulates
    /// a fatal helper failure (blocked ROM / spawn failure / timeout).
    struct MockRunner {
        lines: Vec<String>,
        fail: bool,
    }

    impl MockRunner {
        fn new(lines: Vec<String>) -> Self {
            MockRunner { lines, fail: false }
        }

        fn failing() -> Self {
            MockRunner { lines: Vec::new(), fail: true }
        }
    }

    impl HelperRunner for MockRunner {
        fn query(&self, _pkgs: &[String], on_line: &mut dyn FnMut(&str)) -> Result<(), String> {
            if self.fail {
                return Err("helper dead".to_string());
            }
            for line in &self.lines {
                on_line(line);
            }
            Ok(())
        }
    }

    // ---------- restorable_set ----------

    #[test]
    fn test_restorable_set_basic_diff() {
        // Arrange
        let all_with_u = vec![
            "com.a".to_string(),
            "com.b".to_string(),
            "com.c".to_string(),
        ];
        let installed = vec!["com.a".to_string(), "com.c".to_string()];

        // Act
        let restorable = restorable_set(&all_with_u, &installed);

        // Assert
        let expected: HashSet<String> = ["com.b".to_string()].into_iter().collect();
        assert_eq!(restorable, expected);
    }

    #[test]
    fn test_restorable_set_all_installed_returns_empty() {
        // Arrange
        let all_with_u = vec!["com.a".to_string()];
        let installed = vec!["com.a".to_string()];

        // Act
        let restorable = restorable_set(&all_with_u, &installed);

        // Assert
        assert!(restorable.is_empty());
    }

    // ---------- list_apps ----------

    /// Shared fixture: 2 user + 1 system installed + 1 uninstalled-but-restorable
    /// user package (com.gone.app appears in `-u -3` but not `-3`).
    fn listing_fixture() -> (MockAdb, MockApk, MockCache) {
        let adb = MockAdb {
            list_user: "package:com.user.one\npackage:com.user.two\r\n".to_string(),
            list_system: "package:com.sys.app\n".to_string(),
            list_user_u: "package:com.user.one\npackage:com.user.two\npackage:com.gone.app\n"
                .to_string(),
            list_system_u: "package:com.sys.app\n".to_string(),
            pm_path: HashMap::from([
                (
                    "com.user.one".to_string(),
                    Ok("package:/data/app/~~x==/com.user.one-1/split_config.arm64_v8a.apk\npackage:/data/app/~~x==/com.user.one-1/base.apk\n".to_string()),
                ),
                (
                    "com.user.two".to_string(),
                    Ok("package:/data/app/com.user.two/base.apk\n".to_string()),
                ),
                (
                    "com.sys.app".to_string(),
                    Ok("package:/system/app/SysApp/SysApp.apk\n".to_string()),
                ),
                (
                    "com.gone.app".to_string(),
                    Ok("package:/data/app/com.gone.app/base.apk\n".to_string()),
                ),
            ]),
            ..MockAdb::default()
        };
        let apk = MockApk::default()
            .result(
                "/data/app/~~x==/com.user.one-1/base.apk",
                Ok(("User One", Some(vec![1, 2, 3]), Some(15))),
            )
            .result("/data/app/com.user.two/base.apk", Ok(("User Two", None, None)))
            .result("/system/app/SysApp/SysApp.apk", Ok(("Sys App", None, Some(7))))
            .result("/data/app/com.gone.app/base.apk", Err("corrupt apk"));
        (adb, apk, MockCache::default())
    }

    #[test]
    fn test_list_apps_classifies_user_system_and_restorable() {
        // Arrange
        let (adb, apk, cache) = listing_fixture();

        // Act
        let apps = list_apps(&adb, &apk, &cache);

        // Assert
        assert_eq!(apps.len(), 4);

        let one = find(&apps, "com.user.one");
        assert_eq!(one.kind, AppKind::User);
        assert!(one.installed);
        assert_eq!(one.label, "User One");
        assert_eq!(one.version_code, Some(15));
        assert_eq!(one.icon_base64.as_deref(), Some("AQID"));

        let two = find(&apps, "com.user.two");
        assert_eq!(two.kind, AppKind::User);
        assert!(two.installed);
        assert_eq!(two.label, "User Two");
        assert_eq!(two.version_code, None);
        assert_eq!(two.icon_base64, None);

        let sys = find(&apps, "com.sys.app");
        assert_eq!(sys.kind, AppKind::System);
        assert!(sys.installed);
        assert_eq!(sys.label, "Sys App");
        assert_eq!(sys.version_code, Some(7));

        let gone = find(&apps, "com.gone.app");
        assert_eq!(gone.kind, AppKind::User, "uninstalled user package keeps the User kind");
        assert_eq!(gone.installed, false, "restorable package must be marked not installed");
        assert_eq!(gone.label, "com.gone.app", "parse error degrades label to package name");
        assert_eq!(gone.icon_base64, None);
    }

    #[test]
    fn test_list_apps_cache_miss_parses_and_stores_meta_and_icon() {
        // Arrange
        let (adb, apk, cache) = listing_fixture();

        // Act
        let _ = list_apps(&adb, &apk, &cache);

        // Assert: base.apk preferred over split apk; meta + icon persisted for
        // packages with known version_code.
        let apk_calls = apk.calls_snapshot();
        assert!(apk_calls.contains(&"/data/app/~~x==/com.user.one-1/base.apk".to_string()));
        assert_eq!(
            cache.meta_of("com.user.one"),
            Some(CachedMeta { label: "User One".to_string(), version_code: 15 })
        );
        assert_eq!(cache.icon_of("com.user.one", 15), Some(vec![1, 2, 3]));
        assert_eq!(cache.meta_of("com.sys.app"), Some(CachedMeta { label: "Sys App".to_string(), version_code: 7 }));

        // version unknown → meta NOT stored
        assert_eq!(cache.meta_of("com.user.two"), None);
    }

    #[test]
    fn test_list_apps_cache_hit_skips_pm_path_and_apk_parse() {
        // Arrange: com.user.one fully cached with label/version/icon.
        let (adb, apk, _cache_unused) = listing_fixture();
        let cache = MockCache::default()
            .with_meta("com.user.one", "Cached Label", 9)
            .with_icon("com.user.one", 9, vec![9, 9, 9]);

        // Act
        let apps = list_apps(&adb, &apk, &cache);

        // Assert
        let one = find(&apps, "com.user.one");
        assert_eq!(one.label, "Cached Label");
        assert_eq!(one.version_code, Some(9));
        assert_eq!(one.icon_base64.as_deref(), Some("CQkJ"));

        let calls = adb.calls_snapshot();
        let pm_path_calls: Vec<&Vec<String>> =
            calls.iter().filter(|c| c.len() == 4 && c[2] == "path").collect();
        assert!(
            !pm_path_calls.iter().any(|c| c[3] == "com.user.one"),
            "cache hit must NOT call `pm path` for the cached package"
        );
        assert!(
            !apk.calls_snapshot().iter().any(|p| p.contains("com.user.one")),
            "cache hit must NOT parse the apk for the cached package"
        );
    }

    #[test]
    fn test_list_apps_parse_error_still_lists_every_package() {
        // Arrange: parse error for com.gone.app must not fail the whole listing.
        let (adb, apk, cache) = listing_fixture();

        // Act
        let apps = list_apps(&adb, &apk, &cache);

        // Assert: all 4 packages listed despite the parse error.
        assert_eq!(apps.len(), 4);
        assert_eq!(find(&apps, "com.gone.app").label, "com.gone.app");
        assert!(apk.calls_snapshot().len() == 4, "every pm-path hit is attempted via apk.parse");
    }

    /// B1 (latency): a parse that succeeds with an EMPTY label (system APKs
    /// without a resolvable static label) must degrade to the package name
    /// AND still store meta — an empty-label cache entry is useless and the
    /// previous hard-fail left these packages re-pulled on every refresh.
    #[test]
    fn test_list_apps_empty_label_degrades_to_package_name_and_still_caches() {
        // Arrange: com.user.two parses OK but with an empty label.
        let (adb, _apk_unused, cache) = listing_fixture();
        let apk = MockApk::default()
            .result("/data/app/com.user.two/base.apk", Ok(("", None, Some(5))));

        // Act
        let apps = list_apps(&adb, &apk, &cache);

        // Assert
        let two = find(&apps, "com.user.two");
        assert_eq!(two.label, "com.user.two", "empty label must degrade to package name");
        assert_eq!(two.version_code, Some(5));
        assert_eq!(
            cache.meta_of("com.user.two"),
            Some(CachedMeta { label: "com.user.two".to_string(), version_code: 5 }),
            "meta must be cached so the package is not re-pulled every refresh"
        );
    }

    /// B1b (latency): pull-denied APKs (RRO overlays under /product/overlay
    /// are not readable via `adb pull` on some ROMs) and unparseable APKs
    /// fail deterministically. When the device version is known they must be
    /// NEGATIVE-cached (pkg-name label, no icon) so later listings hit the
    /// cache instead of re-pulling every refresh.
    #[test]
    fn test_list_apps_unresolvable_apk_negative_cached_when_version_known() {
        // Arrange: device reports com.gone.app's version, but its APK can
        // never be pulled/parsed.
        let (adb, _apk_unused, cache) = listing_fixture();
        let adb = adb.with_versioncode("package:com.gone.app versionCode:3\n");
        let apk = MockApk::default()
            .result("/data/app/com.gone.app/base.apk", Err("pull failed: permission denied"));

        // Act
        let apps = list_apps(&adb, &apk, &cache);

        // Assert
        let gone = find(&apps, "com.gone.app");
        assert_eq!(gone.label, "com.gone.app");
        assert_eq!(
            cache.meta_of("com.gone.app"),
            Some(CachedMeta { label: "com.gone.app".to_string(), version_code: 3 }),
            "deterministic parse failure must be negative-cached with the device version"
        );
    }

    /// Without a known device version the failure must NOT be cached — the
    /// entry would never be re-validated (hidden packages are absent from
    /// the version list, so this keeps uninstall-then-restore listings sane).
    #[test]
    fn test_list_apps_unresolvable_apk_not_cached_without_device_version() {
        // Arrange: empty --show-versioncode output → device version unknown.
        let (adb, _apk_unused, cache) = listing_fixture();
        let apk = MockApk::default()
            .result("/data/app/com.gone.app/base.apk", Err("pull failed: permission denied"));

        // Act
        let _ = list_apps(&adb, &apk, &cache);

        // Assert
        assert_eq!(
            cache.meta_of("com.gone.app"),
            None,
            "unknown device version must not produce a negative cache entry"
        );
    }

    // ---------- version-aware cache invalidation ----------

    #[test]
    fn test_parse_package_versions_extracts_pkg_versioncode_pairs() {
        // Arrange: mixed lines (CRLF, no-versionCode, non-package).
        let out = "package:com.a versionCode:10\r\npackage:com.b\npackage:com.c versionCode:42\n";

        // Act
        let map = parse_package_versions(out);

        // Assert
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("com.a"), Some(&10));
        assert_eq!(map.get("com.c"), Some(&42));
        assert!(!map.contains_key("com.b"), "lines without versionCode are skipped");
    }

    #[test]
    fn test_parse_package_versions_empty_or_unsupported_output_returns_empty() {
        // Arrange/Act/Assert: flag unsupported → adb returns junk or empty.
        assert!(parse_package_versions("").is_empty());
        assert!(parse_package_versions("Success\n").is_empty());
        assert!(parse_package_versions("package:com.a versionCode:notanumber\n").is_empty());
    }

    #[test]
    fn test_parse_dumpsys_version_code_extracts_first_versioncode() {
        // Arrange: real dumpsys shape, versionCode may carry suffixes.
        let out = "Packages:\n  Package [com.a] (abc):\n    versionCode=1234 minSdk=21 targetSdk=33\n";

        // Act/Assert
        assert_eq!(parse_dumpsys_version_code(out), Some(1234));
    }

    #[test]
    fn test_parse_dumpsys_version_code_missing_returns_none() {
        // Arrange/Act/Assert
        assert_eq!(parse_dumpsys_version_code("Packages:\n  Package [com.a] (abc):\n"), None);
        assert_eq!(parse_dumpsys_version_code(""), None);
    }

    #[test]
    fn test_list_apps_version_mismatch_on_cache_hit_reparses_and_updates_meta() {
        // Arrange: cache says version 9, device reports 15 → stale cache.
        let (adb, apk, _cache_unused) = listing_fixture();
        let adb = adb.with_versioncode(
            "package:com.user.one versionCode:15\npackage:com.sys.app versionCode:7\n",
        );
        let cache = MockCache::default().with_meta("com.user.one", "Cached Label", 9);

        // Act
        let apps = list_apps(&adb, &apk, &cache);

        // Assert: cache miss path re-resolves, new version is stored.
        let one = find(&apps, "com.user.one");
        assert_eq!(one.label, "User One");
        assert_eq!(one.version_code, Some(15));
        assert!(apk.calls_snapshot().iter().any(|p| p.contains("com.user.one")),
            "version mismatch must re-parse the apk");
        assert_eq!(
            cache.meta_of("com.user.one"),
            Some(CachedMeta { label: "User One".to_string(), version_code: 15 })
        );
    }

    #[test]
    fn test_list_apps_version_match_uses_cache_without_pm_path() {
        // Arrange: cache version matches device version.
        let (adb, apk, _cache_unused) = listing_fixture();
        let adb = adb.with_versioncode("package:com.user.one versionCode:15\n");
        let cache = MockCache::default()
            .with_meta("com.user.one", "Cached Label", 15)
            .with_icon("com.user.one", 15, vec![7, 7, 7]);

        // Act
        let apps = list_apps(&adb, &apk, &cache);

        // Assert
        let one = find(&apps, "com.user.one");
        assert_eq!(one.label, "Cached Label");
        assert_eq!(one.version_code, Some(15));
        assert_eq!(one.icon_base64.as_deref(), Some("BwcH"));
        let calls = adb.calls_snapshot();
        assert!(
            !calls.iter().any(|c| c.len() == 4 && c[2] == "path" && c[3] == "com.user.one"),
            "version match must NOT call `pm path`"
        );
        assert!(
            !apk.calls_snapshot().iter().any(|p| p.contains("com.user.one")),
            "version match must NOT parse the apk"
        );
    }

    #[test]
    fn test_list_apps_unsupported_versioncode_flag_falls_back_to_dumpsys() {
        // Arrange: --show-versioncode unsupported (no versionCode in output),
        // dumpsys reports the real version for the cached package only.
        let (adb, apk, _cache_unused) = listing_fixture();
        let adb = adb.with_dumpsys("com.user.one", "Packages:\n  versionCode=15 minSdk=21\n");
        let cache = MockCache::default()
            .with_meta("com.user.one", "Cached Label", 9)
            .with_meta("com.sys.app", "Sys App", 7);

        // Act
        let apps = list_apps(&adb, &apk, &cache);

        // Assert: mismatch detected via dumpsys → re-parse + updated meta.
        let one = find(&apps, "com.user.one");
        assert_eq!(one.label, "User One");
        assert_eq!(one.version_code, Some(15));
        assert_eq!(
            cache.meta_of("com.user.one"),
            Some(CachedMeta { label: "User One".to_string(), version_code: 15 })
        );

        // dumpsys only runs for cache-hit packages (both are cached here, but
        // the uncached com.user.two must not be probed).
        let calls = adb.calls_snapshot();
        let dumpsys_calls: Vec<&Vec<String>> =
            calls.iter().filter(|c| c.len() == 4 && c[1] == "dumpsys").collect();
        assert!(dumpsys_calls.iter().any(|c| c[3] == "com.user.one"));
        assert!(!dumpsys_calls.iter().any(|c| c[3] == "com.user.two"));
    }

    #[test]
    fn test_list_apps_version_unknown_keeps_cache() {
        // Arrange: neither --show-versioncode nor dumpsys yields a version.
        let (adb, apk, _cache_unused) = listing_fixture();
        let cache = MockCache::default()
            .with_meta("com.user.one", "Cached Label", 9)
            .with_icon("com.user.one", 9, vec![9, 9, 9]);

        // Act
        let apps = list_apps(&adb, &apk, &cache);

        // Assert: documented degrade — serve cache when the device version
        // cannot be determined.
        let one = find(&apps, "com.user.one");
        assert_eq!(one.label, "Cached Label");
        assert_eq!(one.version_code, Some(9));
        let calls = adb.calls_snapshot();
        assert!(
            !calls.iter().any(|c| c.len() == 4 && c[2] == "path" && c[3] == "com.user.one"),
            "version unknown must keep the cache (no pm path)"
        );
        assert!(
            !apk.calls_snapshot().iter().any(|p| p.contains("com.user.one")),
            "version unknown must keep the cache (no apk parse)"
        );
    }

    // ---------- `-u` enumeration (uninstalled-for-user-0 packages) ----------

    #[test]
    fn test_list_apps_installed_only_lists_marks_all_installed() {
        // Arrange: `-u` enumerations match the installed lists (no package
        // uninstalled for user 0).
        let (adb, apk, cache) = listing_fixture();
        let adb = MockAdb {
            list_user_u: "package:com.user.one\npackage:com.user.two\r\n".to_string(),
            list_system_u: "package:com.sys.app\n".to_string(),
            ..adb
        };

        // Act
        let apps = list_apps(&adb, &apk, &cache);

        // Assert
        assert_eq!(apps.len(), 3);
        assert!(apps.iter().all(|a| a.installed), "no uninstalled package exists");
    }

    #[test]
    fn test_list_apps_system_uninstalled_keeps_system_kind() {
        // Arrange: a system package uninstalled for user 0 appears in `-u -s`
        // but not in `-s`.
        let (adb, apk, cache) = listing_fixture();
        let adb = MockAdb {
            list_system_u: "package:com.sys.app\npackage:com.sys.gone\n".to_string(),
            ..adb
        };

        // Act
        let apps = list_apps(&adb, &apk, &cache);

        // Assert
        assert_eq!(apps.len(), 5);
        let gone = find(&apps, "com.sys.gone");
        assert_eq!(gone.kind, AppKind::System);
        assert!(!gone.installed);
        assert_eq!(gone.label, "com.sys.gone", "no apk path → label degrades to package name");
        assert_eq!(gone.icon_base64, None);
    }

    #[test]
    fn test_list_apps_u_flag_error_falls_back_to_installed_listing() {
        // Arrange: the ROM rejects `-u` on both per-kind enumerations.
        let (adb, apk, cache) = listing_fixture();
        let adb = MockAdb { fail_u: true, ..adb };

        // Act
        let apps = list_apps(&adb, &apk, &cache);

        // Assert: only the installed packages are listed, all installed=true;
        // the listing never fails.
        assert_eq!(apps.len(), 3);
        assert!(apps.iter().all(|a| a.installed));
        assert!(
            !apps.iter().any(|a| a.package == "com.gone.app"),
            "packages only known via `-u` must not appear in the fallback"
        );
    }

    // ---------- restorable flag (B2: install-existing only works on system) ----------

    /// AOSP `cmd package install-existing` only restores SYSTEM packages.
    /// An uninstalled USER-kind package (e.g. Compass, a market-updated data
    /// app) must be flagged restorable=false so the UI never promises a
    /// restore it cannot deliver.
    #[test]
    fn test_list_apps_uninstalled_user_app_is_not_restorable() {
        // Arrange
        let (adb, apk, cache) = listing_fixture();

        // Act
        let apps = list_apps(&adb, &apk, &cache);

        // Assert
        assert!(!find(&apps, "com.gone.app").restorable);
    }

    /// Uninstalled SYSTEM-kind packages ARE restorable via install-existing
    /// (and installed system packages accept a no-op restore).
    #[test]
    fn test_list_apps_system_apps_are_restorable() {
        // Arrange: com.sys.gone uninstalled system + com.sys.app installed system.
        let (adb, apk, cache) = listing_fixture();
        let adb = MockAdb {
            list_system_u: "package:com.sys.app\npackage:com.sys.gone\n".to_string(),
            ..adb
        };

        // Act
        let apps = list_apps(&adb, &apk, &cache);

        // Assert
        assert!(find(&apps, "com.sys.gone").restorable, "uninstalled system must be restorable");
        assert!(find(&apps, "com.sys.app").restorable, "installed system must be restorable");
        assert!(
            !find(&apps, "com.user.one").restorable,
            "user-kind packages are never install-existing-restorable"
        );
    }

    #[test]
    fn test_list_apps_uninstalled_package_served_from_cache() {
        // Arrange: com.gone.app was uninstalled for user 0 AFTER being cached
        // (meta + icon present). The plain --show-versioncode list does not
        // report it → device version unknown → cache served as-is.
        let (adb, apk, _cache_unused) = listing_fixture();
        let cache = MockCache::default()
            .with_meta("com.gone.app", "Gone Label", 4)
            .with_icon("com.gone.app", 4, vec![4, 4, 4]);

        // Act
        let apps = list_apps(&adb, &apk, &cache);

        // Assert: cached label/icon/version kept, installed=false, and no
        // `pm path` / apk parse for the uninstalled package.
        let gone = find(&apps, "com.gone.app");
        assert!(!gone.installed);
        assert_eq!(gone.kind, AppKind::User);
        assert_eq!(gone.label, "Gone Label");
        assert_eq!(gone.version_code, Some(4));
        assert_eq!(gone.icon_base64.as_deref(), Some("BAQE"));
        let calls = adb.calls_snapshot();
        assert!(
            !calls.iter().any(|c| c.len() == 4 && c[2] == "path" && c[3] == "com.gone.app"),
            "cache hit must NOT call `pm path` for the uninstalled package"
        );
        assert!(
            !apk.calls_snapshot().iter().any(|p| p.contains("com.gone.app")),
            "cache hit must NOT parse the apk for the uninstalled package"
        );
    }

    // ---------- progressive hydration (G1-G3) ----------

    #[test]
    fn test_parse_package_paths_handles_equals_in_apk_dir() {
        // Arrange: `pm list packages -f` lines; device paths contain '='.
        let out = "package:/data/app/~~x==/com.a-1/base.apk=com.a\n\
                   package:/product/overlay/o.apk=com.o\n\
                   package:/system/app/Sys/Sys.apk=com.sys.app\r\n";

        // Act
        let map = parse_package_paths(out);

        // Assert: split at the LAST '=' — the path side keeps its '=' chars.
        assert_eq!(map.get("com.a"), Some(&"/data/app/~~x==/com.a-1/base.apk".to_string()));
        assert_eq!(map.get("com.o"), Some(&"/product/overlay/o.apk".to_string()));
        assert_eq!(map.get("com.sys.app"), Some(&"/system/app/Sys/Sys.apk".to_string()));
    }

    #[test]
    fn test_parse_package_paths_skips_malformed_lines() {
        // Arrange/Act
        let map = parse_package_paths("package:no-equals-sign\nnot-package:x=y\n\npackage:=\n");

        // Assert
        assert!(map.is_empty());
    }

    #[test]
    fn test_parse_ls_sizes_parses_size_and_path() {
        // Arrange: `ls -l` output (toybox shape), size then date then path.
        let out = "-rw-r--r-- 1 root root 12345 2026-01-01 00:00 /system/app/Foo/Foo.apk\n\
                   -rw-r--r-- 1 root root 675000 2025-12-31 23:59 /product/overlay/o.apk\r\n\
                   ls: /gone: Permission denied\n";

        // Act
        let sizes = parse_ls_sizes(out);

        // Assert
        assert_eq!(
            sizes,
            vec![
                (12345, "/system/app/Foo/Foo.apk".to_string()),
                (675000, "/product/overlay/o.apk".to_string()),
            ]
        );
    }

    /// G3: fast listing must NEVER touch `pm path` or the apk parser —
    /// cache misses degrade in place (pkg label, no icon) and are reported
    /// as pending so the background hydrator can resolve them.
    #[test]
    fn test_list_apps_fast_degrades_misses_without_any_pull() {
        // Arrange: empty cache, device reports com.gone.app's version.
        let (adb, apk, cache) = listing_fixture();
        let adb = adb.with_versioncode("package:com.gone.app versionCode:3\n");

        // Act
        let listing = list_apps_fast(&adb, &apk, &cache);

        // Assert: full roster present immediately, misses degraded.
        assert_eq!(listing.apps.len(), 4);
        let gone = find(&listing.apps, "com.gone.app");
        assert_eq!(gone.label, "com.gone.app");
        assert_eq!(gone.icon_base64, None);
        assert_eq!(gone.version_code, Some(3), "degraded entry keeps the device version");
        assert!(!gone.installed);

        let pending: Vec<&str> = listing.pending.iter().map(|a| a.package.as_str()).collect();
        assert_eq!(pending.len(), 4, "all 4 packages are cache misses");
        assert!(pending.contains(&"com.gone.app"));

        let calls = adb.calls_snapshot();
        assert!(
            !calls.iter().any(|c| c.len() == 4 && c[2] == "path"),
            "fast listing must NOT call `pm path`"
        );
        assert!(
            apk.calls_snapshot().is_empty(),
            "fast listing must NOT parse any apk"
        );
    }

    /// The background hydrator resolves pending packages using the provided
    /// path map (no per-app `pm path`), caches meta/icon, and fires the
    /// callback once per resolved app in pending order.
    #[test]
    fn test_resolve_pending_resolves_via_path_map_and_caches() {
        // Arrange
        let adb = MockAdb::default();
        let apk = MockApk::default().result(
            "/system/app/SysApp/SysApp.apk",
            Ok(("Sys App", Some(vec![1, 2, 3]), Some(7))),
        );
        let cache = MockCache::default();
        let pending = vec![degraded("com.sys.app", AppKind::System, true, Some(7))];
        let paths = HashMap::from([(
            "com.sys.app".to_string(),
            "/system/app/SysApp/SysApp.apk".to_string(),
        )]);
        let mut seen = Vec::new();

        // Act
        resolve_pending(&adb, &apk, &cache, &pending, &paths, &mut |info| {
            seen.push(info);
        });

        // Assert
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].label, "Sys App");
        assert_eq!(seen[0].icon_base64.as_deref(), Some("AQID"));
        assert_eq!(seen[0].version_code, Some(7));
        assert_eq!(
            cache.meta_of("com.sys.app"),
            Some(CachedMeta { label: "Sys App".to_string(), version_code: 7 })
        );
        assert_eq!(cache.icon_of("com.sys.app", 7), Some(vec![1, 2, 3]));
        assert!(
            !adb.calls_snapshot().iter().any(|c| c.len() == 4 && c[2] == "path"),
            "path map must replace per-app `pm path` calls"
        );
    }

    /// Packages that became cached between the fast listing and hydration
    /// (e.g. an overlapping newer scan) must be skipped without any parse
    /// and without firing the callback.
    #[test]
    fn test_resolve_pending_skips_already_cached_packages() {
        // Arrange
        let adb = MockAdb::default();
        let apk = MockApk::default();
        let cache = MockCache::default().with_meta("com.sys.app", "Cached", 7);
        let pending = vec![degraded("com.sys.app", AppKind::System, true, Some(7))];

        // Act
        let mut seen = Vec::new();
        resolve_pending(&adb, &apk, &cache, &pending, &HashMap::new(), &mut |info| {
            seen.push(info);
        });

        // Assert
        assert!(seen.is_empty(), "cached package must not fire the callback");
        assert!(apk.calls_snapshot().is_empty(), "cached package must not be parsed");
    }

    // ---------- helper.dex (T2: parse + resolve_via_helper) ----------

    #[test]
    fn test_base64_decode_roundtrip() {
        // Arrange
        let original = "Compass / Cài đặt 🎉";

        // Act
        let encoded = base64_encode(original.as_bytes());
        let decoded = base64_decode(&encoded).unwrap();

        // Assert
        assert_eq!(String::from_utf8(decoded).unwrap(), original);
    }

    #[test]
    fn test_parse_helper_line_ok_with_icon() {
        // Arrange: real helper shape.
        let label = base64_encode("Cài đặt".as_bytes());
        let icon = base64_encode(&[0x89, b'P', b'N', b'G', 1, 2, 3]);
        let line = format!("OK\tcom.android.settings\t{label}\t{icon}");

        // Act
        let out = parse_helper_line(&line);

        // Assert
        match out {
            Some(HelperOutcome::Ok { package, label, icon_png }) => {
                assert_eq!(package, "com.android.settings");
                assert_eq!(label, "Cài đặt");
                assert_eq!(icon_png, Some(vec![0x89, b'P', b'N', b'G', 1, 2, 3]));
            }
            other => panic!("expected Ok outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_helper_line_ok_without_icon() {
        // Arrange
        let label = base64_encode("Some App".as_bytes());
        let line = format!("OK\tcom.some.app\t{label}\t-");

        // Act/Assert
        match parse_helper_line(&line) {
            Some(HelperOutcome::Ok { package, icon_png, .. }) => {
                assert_eq!(package, "com.some.app");
                assert_eq!(icon_png, None, "'-' means no icon");
            }
            other => panic!("expected Ok outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_helper_line_err() {
        // Arrange
        let reason = base64_encode("NameNotFoundException: com.x".as_bytes());
        let line = format!("ERR\tcom.x\t{reason}");

        // Act/Assert
        match parse_helper_line(&line) {
            Some(HelperOutcome::Err { package }) => assert_eq!(package, "com.x"),
            other => panic!("expected Err outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_helper_line_malformed_returns_none() {
        // Arrange/Act/Assert: short lines, bad base64, wrong tag, non-UTF label.
        assert_eq!(parse_helper_line(""), None);
        assert_eq!(parse_helper_line("OK\tcom.x"), None);
        assert_eq!(parse_helper_line("WEIRD\tcom.x\ta\t-"), None);
        assert_eq!(parse_helper_line("OK\tcom.x\tnot!base64!\t-"), None);
    }

    /// Happy path: OK lines populate cache + fire the callback; ERR lines and
    /// packages the helper never mentioned become leftover for pull fallback.
    #[test]
    fn test_resolve_via_helper_caches_and_collects_leftover() {
        // Arrange
        let cache = MockCache::default();
        let label_a = base64_encode("App A".as_bytes());
        let label_b = base64_encode("App B".as_bytes());
        let icon_b = base64_encode(&[9, 9, 9]);
        let reason_c = base64_encode("not found".as_bytes());
        let runner = MockRunner::new(vec![
            format!("OK\tcom.a\t{label_a}\t-"),
            format!("OK\tcom.b\t{label_b}\t{icon_b}"),
            format!("ERR\tcom.c\t{reason_c}"),
        ]);
        let pending = vec![
            degraded("com.a", AppKind::User, true, Some(5)),
            degraded("com.b", AppKind::System, true, Some(7)),
            degraded("com.c", AppKind::User, true, Some(3)),
        ];
        let mut seen = Vec::new();

        // Act
        let leftover = resolve_via_helper(&cache, &pending, &runner, &mut |info| {
            seen.push(info);
        });

        // Assert: 2 resolved via callback, com.c is the only leftover.
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].package, "com.a");
        assert_eq!(seen[0].label, "App A");
        assert_eq!(seen[0].icon_base64, None);
        assert_eq!(seen[1].package, "com.b");
        assert_eq!(seen[1].icon_base64.as_deref(), Some("CQkJ"));
        assert_eq!(leftover.iter().map(|a| a.package.as_str()).collect::<Vec<_>>(), ["com.c"]);

        assert_eq!(
            cache.meta_of("com.a"),
            Some(CachedMeta { label: "App A".to_string(), version_code: 5 })
        );
        assert_eq!(cache.icon_of("com.b", 7), Some(vec![9, 9, 9]));
        assert_eq!(cache.meta_of("com.c"), None, "ERR packages are not cached");
    }

    /// Fatal helper failure (blocked ROM, spawn failure, timeout) must hand
    /// the ENTIRE pending list to the pull fallback untouched.
    #[test]
    fn test_resolve_via_helper_fatal_returns_all_pending() {
        // Arrange
        let cache = MockCache::default();
        let runner = MockRunner::failing();
        let pending = vec![
            degraded("com.a", AppKind::User, true, Some(5)),
            degraded("com.b", AppKind::System, true, Some(7)),
        ];
        let mut seen = Vec::new();

        // Act
        let leftover = resolve_via_helper(&cache, &pending, &runner, &mut |info| {
            seen.push(info);
        });

        // Assert
        assert!(seen.is_empty(), "fatal helper must not fire the callback");
        assert_eq!(
            leftover.iter().map(|a| a.package.as_str()).collect::<Vec<_>>(),
            ["com.a", "com.b"]
        );
        assert_eq!(cache.meta_of("com.a"), None, "nothing cached from a dead helper");
    }

    // ---------- run_batch ----------

    #[test]
    fn test_run_batch_uninstall_uses_exact_args() {
        // Arrange
        let adb = MockAdb::default().batch_out("com.a", Ok("Success\n"));
        let pkgs = vec!["com.a".to_string()];
        let mut progress: Vec<BatchProgress> = Vec::new();

        // Act
        let results = run_batch(&adb, &pkgs, BatchOp::Uninstall, &[], &mut |p| progress.push(p));

        // Assert
        assert_eq!(results.len(), 1);
        assert!(results[0].success);
        assert_eq!(results[0].package, "com.a");
        assert_eq!(results[0].message, "Success");
        let expected: Vec<String> = ["shell", "pm", "uninstall", "-k", "--user", "0", "com.a"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(adb.calls_snapshot()[0], expected);
        assert_eq!(progress, vec![BatchProgress { index: 0, total: 1, package: "com.a".to_string() }]);
    }

    #[test]
    fn test_run_batch_restore_uses_exact_args() {
        // Arrange
        let adb = MockAdb::default().batch_out("com.a", Ok("Success\n"));
        let pkgs = vec!["com.a".to_string()];
        let mut progress: Vec<BatchProgress> = Vec::new();

        // Act
        let results = run_batch(&adb, &pkgs, BatchOp::Restore, &[], &mut |p| progress.push(p));

        // Assert
        assert!(results[0].success);
        let expected: Vec<String> = ["shell", "cmd", "package", "install-existing", "com.a"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(adb.calls_snapshot()[0], expected);
    }

    #[test]
    fn test_run_batch_blacklist_blocks_without_adb_call() {
        // Arrange
        let adb = MockAdb::default().batch_out("com.good", Ok("Success\n"));
        let pkgs = vec!["com.bad".to_string(), "com.good".to_string()];
        let blacklist = vec!["com.bad".to_string()];
        let mut progress: Vec<BatchProgress> = Vec::new();

        // Act
        let results = run_batch(&adb, &pkgs, BatchOp::Uninstall, &blacklist, &mut |p| progress.push(p));

        // Assert: blocked entry fails without any adb attempt; the rest proceeds.
        assert_eq!(results.len(), 2);
        assert!(!results[0].success);
        assert_eq!(results[0].package, "com.bad");
        assert_eq!(results[0].message, "Blocked: package thuộc danh sách cấm gỡ");
        assert!(results[1].success);
        assert_eq!(results[1].package, "com.good");

        let calls = adb.calls_snapshot();
        assert_eq!(calls.len(), 1, "blacklisted package must not reach adb");
        assert!(!calls[0].contains(&"com.bad".to_string()));

        // Progress only fires right before adb attempts (after blacklist check).
        assert_eq!(
            progress,
            vec![BatchProgress { index: 1, total: 2, package: "com.good".to_string() }]
        );
    }

    #[test]
    fn test_run_batch_continues_after_mid_batch_failure() {
        // Arrange
        let adb = MockAdb::default()
            .batch_out("com.ok1", Ok("Success\n"))
            .batch_out("com.fail", Err("adb exited with status 1: Failure"))
            .batch_out("com.ok2", Ok("Success\n"));
        let pkgs = vec![
            "com.ok1".to_string(),
            "com.fail".to_string(),
            "com.ok2".to_string(),
        ];
        let mut progress: Vec<BatchProgress> = Vec::new();

        // Act
        let results = run_batch(&adb, &pkgs, BatchOp::Uninstall, &[], &mut |p| progress.push(p));

        // Assert
        assert_eq!(results.len(), 3);
        assert!(results[0].success);
        assert!(!results[1].success);
        assert_eq!(results[1].message, "adb exited with status 1: Failure");
        assert!(results[2].success, "batch must continue after a failure");
        assert_eq!(adb.calls_snapshot().len(), 3, "all packages attempted");
        assert_eq!(
            progress,
            vec![
                BatchProgress { index: 0, total: 3, package: "com.ok1".to_string() },
                BatchProgress { index: 1, total: 3, package: "com.fail".to_string() },
                BatchProgress { index: 2, total: 3, package: "com.ok2".to_string() },
            ]
        );
    }

    #[test]
    fn test_run_batch_empty_selection_returns_empty() {
        // Arrange
        let adb = MockAdb::default();
        let pkgs: Vec<String> = Vec::new();
        let mut progress: Vec<BatchProgress> = Vec::new();

        // Act
        let results = run_batch(&adb, &pkgs, BatchOp::Uninstall, &[], &mut |p| progress.push(p));

        // Assert
        assert!(results.is_empty());
        assert!(progress.is_empty());
        assert!(adb.calls_snapshot().is_empty());
    }

    // ---------- parse_batched_package_lists ----------

    #[test]
    fn test_parse_batched_package_lists_splits_five_segments() {
        // Arrange: batched output with 4 markers separating 5 segments
        let out = format!(
            "package:com.a\npackage:com.b\n{sep}\npackage:com.sys\n{sep}\npackage:com.a\npackage:com.hidden\n{sep}\npackage:com.sys\n{sep}\npackage:com.a versionCode:42\n",
            sep = BATCHED_LIST_SEP
        );

        // Act
        let parsed = parse_batched_package_lists(&out);

        // Assert
        let (user, system, user_all, system_all, versions) = parsed.expect("well-formed batch");
        assert_eq!(user, vec!["com.a".to_string(), "com.b".to_string()]);
        assert_eq!(system, vec!["com.sys".to_string()]);
        assert_eq!(user_all, vec!["com.a".to_string(), "com.hidden".to_string()]);
        assert_eq!(system_all, vec!["com.sys".to_string()]);
        assert_eq!(versions.get("com.a"), Some(&42u64));
    }

    #[test]
    fn test_parse_batched_package_lists_returns_none_on_missing_marker() {
        // Arrange: a segment missing means the shell mangled the batch —
        // the caller must fall back to the sequential per-call path.
        let out = format!(
            "package:com.a\n{sep}\npackage:com.sys\n",
            sep = BATCHED_LIST_SEP
        );

        // Act
        let parsed = parse_batched_package_lists(&out);

        // Assert
        assert!(parsed.is_none());
    }

    #[test]
    fn test_parse_batched_package_lists_tolerates_crlf_and_empty_segments() {
        // Arrange: adb emits \r\n line endings; an empty segment (command
        // produced nothing) is valid and parses to an empty list.
        let out = format!(
            "package:com.a\r\n{sep}\r\n{sep}\r\npackage:com.h\r\n{sep}\r\n{sep}\r\n",
            sep = BATCHED_LIST_SEP
        );

        // Act
        let parsed = parse_batched_package_lists(&out);

        // Assert
        let (user, system, user_all, system_all, _versions) = parsed.expect("well-formed batch");
        assert_eq!(user, vec!["com.a".to_string()]);
        assert!(system.is_empty());
        assert_eq!(user_all, vec!["com.h".to_string()]);
        assert!(system_all.is_empty());
    }

    // ---------- hydration routing (split_pending_for_hydration) ----------

    #[test]
    fn test_split_pending_for_hydration_routes_pathless_to_pull() {
        // Arrange: hidden packages have no `pm list -f` path — the helper
        // cannot see them, so they pull (fast negative-cache) concurrently.
        let paths = HashMap::from([
            ("com.normal".to_string(), "/data/app/~~x==/com.normal-1==/base.apk".to_string()),
            ("com.sysui.overlay.anim".to_string(), "/product/overlay/SystemUIAnim.apk".to_string()),
            // com.hidden has no path at all (uninstalled for user 0)
        ]);
        let pending = vec![
            degraded("com.normal", AppKind::System, true, Some(1)),
            degraded("com.sysui.overlay.anim", AppKind::System, true, Some(1)),
            degraded("com.hidden", AppKind::User, false, Some(1)),
        ];

        // Act
        let (helper_set, pull_set) = split_pending_for_hydration(&pending, &paths);

        // Assert: path presence decides — overlay in the PATH still goes to
        // the helper (it resolves those fine).
        assert_eq!(
            helper_set.iter().map(|a| a.package.as_str()).collect::<Vec<_>>(),
            ["com.normal", "com.sysui.overlay.anim"]
        );
        assert_eq!(
            pull_set.iter().map(|a| a.package.as_str()).collect::<Vec<_>>(),
            ["com.hidden"]
        );
    }

    #[test]
    fn test_split_pending_for_hydration_keeps_order_within_sets() {
        // Arrange
        let paths = HashMap::from([
            ("com.a".to_string(), "/data/app/a.apk".to_string()),
            ("com.b".to_string(), "/data/app/b.apk".to_string()),
            // com.h1 / com.h2 are pathless
        ]);
        let pending = vec![
            degraded("com.a", AppKind::System, true, Some(1)),
            degraded("com.h1", AppKind::User, false, Some(1)),
            degraded("com.b", AppKind::User, true, Some(1)),
            degraded("com.h2", AppKind::System, true, Some(1)),
        ];

        // Act
        let (helper_set, pull_set) = split_pending_for_hydration(&pending, &paths);

        // Assert
        assert_eq!(
            helper_set.iter().map(|a| a.package.as_str()).collect::<Vec<_>>(),
            ["com.a", "com.b"]
        );
        assert_eq!(
            pull_set.iter().map(|a| a.package.as_str()).collect::<Vec<_>>(),
            ["com.h1", "com.h2"]
        );
    }

    // ---------- resolve_pending_parallel ----------

    fn mk_apk_ok(path: &str, label: &str) -> MockApk {
        MockApk::default().result(path, Ok((label, Some(vec![1, 2, 3]), Some(5))))
    }

    #[test]
    fn test_resolve_pending_parallel_resolves_all_and_caches_once_each() {
        // Arrange: 4 apps across 2 workers; every parse succeeds.
        let paths = HashMap::from([
            ("com.a".to_string(), "/data/app/a.apk".to_string()),
            ("com.b".to_string(), "/data/app/b.apk".to_string()),
            ("com.c".to_string(), "/data/app/c.apk".to_string()),
            ("com.d".to_string(), "/data/app/d.apk".to_string()),
        ]);
        let pending = vec![
            degraded("com.a", AppKind::System, true, Some(1)),
            degraded("com.b", AppKind::User, true, Some(2)),
            degraded("com.c", AppKind::System, true, Some(3)),
            degraded("com.d", AppKind::User, true, Some(4)),
        ];
        let cache = MockCache::default();
        let spawn_context = || {
            let mut apk = MockApk::default();
            for (pkg, path) in &paths {
                let label = format!("Label {}", pkg);
                apk = apk.result(path, Ok((label.as_str(), Some(vec![9, 9]), Some(5))));
            }
            (Box::new(MockAdb::default()) as Box<dyn AdbLike + Send>, Box::new(apk) as Box<dyn ApkLike + Send>)
        };
        let mut resolved: Vec<AppInfo> = Vec::new();

        // Act
        resolve_pending_parallel(
            &cache,
            &spawn_context,
            &pending,
            &paths,
            2,
            &mut |info| resolved.push(info),
        );

        // Assert: every app resolved exactly once with parse output; cache
        // written once per app (collector thread, never concurrent).
        assert_eq!(resolved.len(), 4);
        let labels: Vec<&str> = resolved.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"Label com.a"));
        assert!(labels.contains(&"Label com.d"));
        assert!(resolved.iter().all(|i| i.version_code == Some(5)));
        assert!(resolved.iter().all(|i| i.icon_base64.is_some()));
        let meta = cache.meta_of("com.b").expect("meta cached");
        assert_eq!(meta.label, "Label com.b");
        assert_eq!(meta.version_code, 5);
        assert!(cache.icon_of("com.b", 5).is_some());
    }

    #[test]
    fn test_resolve_pending_parallel_negative_caches_parse_failures() {
        // Arrange: parse fails (pull denied / corrupt APK) → negative cache
        // with the device version, label degrades to package name.
        let paths = HashMap::from([
            ("com.x".to_string(), "/product/overlay/x.apk".to_string()),
            ("com.y".to_string(), "/vendor/overlay/y.apk".to_string()),
        ]);
        let pending = vec![
            degraded("com.x", AppKind::System, true, Some(9)),
            degraded("com.y", AppKind::System, true, Some(9)),
        ];
        let cache = MockCache::default();
        let spawn_context = || {
            let mut apk = MockApk::default();
            apk = apk.result("/product/overlay/x.apk", Err("pull denied"));
            apk = apk.result("/vendor/overlay/y.apk", Err("pull denied"));
            (Box::new(MockAdb::default()) as Box<dyn AdbLike + Send>, Box::new(apk) as Box<dyn ApkLike + Send>)
        };
        let mut resolved: Vec<AppInfo> = Vec::new();

        // Act
        resolve_pending_parallel(
            &cache,
            &spawn_context,
            &pending,
            &paths,
            2,
            &mut |info| resolved.push(info),
        );

        // Assert
        assert_eq!(resolved.len(), 2);
        assert!(resolved.iter().all(|i| i.label == i.package));
        assert!(resolved.iter().all(|i| i.icon_base64.is_none()));
        assert!(resolved.iter().all(|i| i.version_code.is_none()));
        let meta = cache.meta_of("com.x").expect("negatively cached");
        assert_eq!(meta.label, "com.x");
        assert_eq!(meta.version_code, 9);
        assert!(cache.icon_of("com.x", 9).is_none());
    }

    #[test]
    fn test_resolve_pending_parallel_empty_pending_is_noop() {
        // Arrange
        let paths = HashMap::new();
        let pending: Vec<AppInfo> = Vec::new();
        let cache = MockCache::default();
        let spawn_context = || {
            (Box::new(MockAdb::default()) as Box<dyn AdbLike + Send>, Box::new(MockApk::default()) as Box<dyn ApkLike + Send>)
        };
        let mut resolved: Vec<AppInfo> = Vec::new();

        // Act
        resolve_pending_parallel(
            &cache,
            &spawn_context,
            &pending,
            &paths,
            6,
            &mut |info| resolved.push(info),
        );

        // Assert
        assert!(resolved.is_empty());
    }

    // ---------- listing_from_parsed + batched command (H4) ----------

    fn app_key(info: &AppInfo) -> (String, String, bool, bool, Option<u64>, Option<String>) {
        (
            info.package.clone(),
            info.label.clone(),
            info.installed,
            info.restorable,
            info.version_code,
            info.icon_base64.clone(),
        )
    }

    #[test]
    fn test_listing_from_parsed_matches_list_apps_fast() {
        // Arrange: identical device state, once through the sequential 5-call
        // path and once through pre-parsed lists.
        let versioncode_out = "package:com.user.one versionCode:11\n\
                               package:com.user.two versionCode:12\n\
                               package:com.sys.app versionCode:13\n\
                               package:com.gone.app versionCode:14\n";
        let (adb, apk, cache) = listing_fixture();
        let adb = adb.with_versioncode(versioncode_out);
        let via_fast = list_apps_fast(&adb, &apk, &cache);

        let (adb2, _apk2, cache2) = listing_fixture();
        let adb2 = adb2.with_versioncode(versioncode_out);
        let via_parsed = listing_from_parsed(
            &adb2,
            &cache2,
            vec!["com.user.one".to_string(), "com.user.two".to_string()],
            vec!["com.sys.app".to_string()],
            vec![
                "com.user.one".to_string(),
                "com.user.two".to_string(),
                "com.gone.app".to_string(),
            ],
            vec!["com.sys.app".to_string()],
            parse_package_versions(versioncode_out),
        );

        // Assert
        let a: Vec<_> = via_fast.apps.iter().map(app_key).collect();
        let b: Vec<_> = via_parsed.apps.iter().map(app_key).collect();
        assert_eq!(a, b);
        let pa: Vec<_> = via_fast.pending.iter().map(|x| x.package.clone()).collect();
        let pb: Vec<_> = via_parsed.pending.iter().map(|x| x.package.clone()).collect();
        assert_eq!(pa, pb);
    }

    #[test]
    fn test_batched_package_lists_cmd_composes_five_calls() {
        // Arrange / Act
        let cmd = batched_package_lists_cmd();

        // Assert: every pm variant present, exactly 4 separators for 5 segments.
        assert!(cmd.contains("pm list packages -3 --user 0"));
        assert!(cmd.contains("pm list packages -s --user 0"));
        assert!(cmd.contains("pm list packages -u -3 --user 0"));
        assert!(cmd.contains("pm list packages -u -s --user 0"));
        assert!(cmd.contains("pm list packages --show-versioncode"));
        assert_eq!(cmd.matches(BATCHED_LIST_SEP).count(), 4);
    }
}
