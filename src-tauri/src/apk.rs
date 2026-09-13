//! APK metadata extraction.
//!
//! Parses an `.apk` file and extracts the app display label and the launcher
//! icon bytes, using the `apk-info` crate (AXML + ARSC parsing).

use std::path::{Path, PathBuf};

use crate::vector;

pub struct ApkInfo {
    /// App display name (default-locale label from the APK).
    pub label: String,
    /// Rendered 192x192 icon PNG bytes (adaptive/vector pipeline, or a
    /// normalized plain raster); `None` if the icon is missing or cannot be
    /// read/rendered.
    pub icon_png: Option<Vec<u8>>,
    /// `versionCode` from the manifest; `None` when absent or not a u64.
    pub version_code: Option<u64>,
}

/// Density rank of an `anydpi` directory (adaptive-icon XML): outranks every
/// density bucket (T-E).
const ANYDPI_DENSITY_RANK: u8 = 7;

/// Minimum number of distinct opaque colors for a rendered adaptive/vector
/// icon to be accepted by the manifest-anchored step (T-A degeneracy guard):
/// stub templates (the shared green-robot, black/white partial renders)
/// produce fewer, while real rendered logos/wordmarks/gradients produce
/// many.
const MIN_ANCHORED_OPAQUE_COLORS: usize = 8;

/// Maximum share of the opaque canvas a SINGLE color may cover in an
/// accepted XML render (T-A degeneracy guard, second axis): a missing
/// adaptive layer renders a near-monochrome canvas that can still carry many
/// anti-aliasing shades (Telegram's background-only render is 95.8% white
/// with 51 distinct colors). Tune floor: real flat-design icons stay well
/// below (Drive 72%, Gorio/GreenSM measured on fixtures).
const MAX_ANCHORED_DOMINANT_FRACTION: f32 = 0.90;

/// A rendered XML icon is degenerate when it is color-poor (stub template)
/// or mono-dominant (missing adaptive layer). Such renders are skipped while
/// raster variants remain to try.
fn anchored_render_degenerate(png: &[u8]) -> bool {
    vector::distinct_opaque_colors(png) < MIN_ANCHORED_OPAQUE_COLORS
        || vector::dominant_opaque_fraction(png) > MAX_ANCHORED_DOMINANT_FRACTION
}

/// True when a rendered PNG icon looks like a background-only/stub render
/// (the T-A degeneracy axes: color-poor or mono-dominant). Non-PNG bytes
/// (WebP rasters) and empty bytes cannot be judged and classify healthy —
/// this only decides whether callers should try harder (density splits,
/// R2-F3), never whether to drop an icon.
pub fn icon_looks_weak(png: &[u8]) -> bool {
    if tiny_skia::Pixmap::decode_png(png).is_err() {
        return false;
    }
    anchored_render_degenerate(png)
}

/// A parsed manifest icon reference: a raw resource id (hex token form) or a
/// `type/entry` name.
#[derive(Debug, PartialEq)]
enum IconRef {
    Id(u32),
    Name(String),
}

/// Parse a raw manifest reference token: `@7f0d0002` (the hex fallback the
/// AXML decoder emits without an ARSC), `@0x7f0d0002`, or a named
/// `@type/entry` / `@package:type/entry` form. Non-`@` tokens are not
/// references.
fn parse_icon_ref_token(raw: &str) -> Option<IconRef> {
    let rest = raw.strip_prefix('@')?;
    let rest = rest.rsplit(':').next().unwrap_or(rest);
    if rest.contains('/') {
        return Some(IconRef::Name(rest.to_string()));
    }
    u32::from_str_radix(rest.strip_prefix("0x").unwrap_or(rest), 16).ok().map(IconRef::Id)
}

/// Manifest-anchored icon variants (T-A step 0 source): resolve the
/// manifest's `android:icon` resource across ALL configs and sources and
/// return the ranked variant entry paths (anydpi adaptive XMLs first, then
/// rasters by descending density). The manifest is parsed WITHOUT an ARSC so
/// the reference survives as a raw token instead of being resolved through
/// the default config only (which dead-ends on density-only entries, C4).
/// `None` when the manifest or the icon reference is missing/unparsable.
fn manifest_icon_variants(multi: &crate::arsc_resolve::MultiSource) -> Option<Vec<String>> {
    let bytes = multi.base().read("AndroidManifest.xml").ok()?.0;
    let axml = apk_info::AXML::new(&mut &bytes[..], None).ok()?;
    let raw = axml.get_attribute_value("application", "icon", None)?;
    let variants = match parse_icon_ref_token(&raw) {
        Some(IconRef::Id(id)) => multi.get_resource_variants(id),
        Some(IconRef::Name(name)) => multi.get_resource_variants_by_name(&name),
        None => Vec::new(),
    };
    (!variants.is_empty()).then_some(variants)
}

/// Render the ranked manifest-icon variants (T-A step 0): adaptive/vector
/// XMLs render through the vector pipeline, rasters normalize like the
/// legacy path. A raster variant is accepted when its bytes are non-empty;
/// an XML render must pass the degeneracy color guard UNLESS no raster
/// variant remains to try (adaptive-only apps must not lose their icon).
fn render_anchored_variants(
    variants: &[String],
    multi: &crate::arsc_resolve::MultiSource,
    read_entry: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    let raster_remaining =
        |from: usize| variants[from..].iter().any(|v| !v.ends_with(".xml"));
    for (i, entry) in variants.iter().enumerate() {
        let bytes = read_entry(entry)?;
        if bytes.is_empty() {
            continue;
        }
        if entry.ends_with(".xml") {
            let Some(png) = vector::adaptive_icon_png_res(&bytes, read_entry, multi) else {
                continue;
            };
            if !anchored_render_degenerate(&png) || !raster_remaining(i + 1) {
                return Some(png);
            }
            // Degenerate adaptive render (stub/black/white partial) with
            // raster alternatives left: try the next variant.
        } else {
            return Some(vector::normalize_raster_any(&bytes).unwrap_or(bytes));
        }
    }
    None
}

/// Score a zip entry as a launcher-icon fallback candidate.
///
/// Higher tuples win; components ordered by priority:
/// 1. launcher-likeness: 2 = base name contains "ic_launcher",
///    1 = under a `mipmap*` directory, 0 = under a `drawable*` directory.
/// 2. density bucket: anydpi(7, adaptive-icon XML) > xxxhdpi(6) > xxhdpi(5)
///    > xhdpi(4) > hdpi(3) > mdpi(2) > ldpi(1) > no density qualifier(0).
/// 3. extension: png(4) > webp(3) > jpg(2) > xml(1, adaptive/vector icon).
///
/// `None` when the entry is not a res/ icon candidate.
fn icon_entry_score(entry: &str) -> Option<(u8, u8, u8)> {
    let rest = entry.strip_prefix("res/")?;
    let (dir, file) = rest.split_once('/')?;
    if file.is_empty() {
        return None;
    }

    let ext_rank = match file.rsplit_once('.')?.1.to_ascii_lowercase().as_str() {
        "png" => 4,
        "webp" => 3,
        "jpg" => 2,
        "xml" => 1,
        _ => return None,
    };

    let base_name = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    let launcher_class = if base_name.contains("ic_launcher") {
        2
    } else if dir.starts_with("mipmap") {
        1
    } else if dir.starts_with("drawable") {
        0
    } else {
        return None;
    };

    let density_rank = dir
        .split('-')
        .find_map(|q| match q {
            "anydpi" => Some(ANYDPI_DENSITY_RANK),
            "xxxhdpi" => Some(6),
            "xxhdpi" => Some(5),
            "xhdpi" => Some(4),
            "hdpi" => Some(3),
            "mdpi" => Some(2),
            "ldpi" => Some(1),
            _ => None,
        })
        .unwrap_or(0);

    Some((launcher_class, density_rank, ext_rank))
}

/// Pick the best adaptive-icon XML entry (`anydpi` directory) from the APK
/// zip namelist, if any (T-E).
fn choose_adaptive_icon_entry<'a>(entries: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    entries
        .into_iter()
        .filter(|e| e.ends_with(".xml"))
        .filter_map(|e| icon_entry_score(e).map(|score| (score, e)))
        .filter(|(score, _)| score.1 == ANYDPI_DENSITY_RANK)
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, e)| e)
}

/// Pick the best launcher-icon entry from the APK zip namelist.
///
/// Used when the ARSC-resolved `applicationIcon` is missing or empty (the
/// apk-info crate only resolves the default config, so density-only icons
/// like `res/drawable-xxhdpi-v4/ic_launcher.webp` resolve to "").
/// Adaptive `anydpi` XML entries are excluded — they are handled by the
/// dedicated adaptive-first step, so a failed adaptive render does not
/// shadow the legacy raster candidates here (T-E).
fn choose_fallback_icon_entry<'a>(entries: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    entries
        .into_iter()
        .filter_map(|e| icon_entry_score(e).map(|score| (score, e)))
        .filter(|(score, _)| score.1 != ANYDPI_DENSITY_RANK)
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, e)| e)
}

/// Parse label + icon + version from the APK at `path`.
/// Errors as String when the file cannot be read/parsed.
pub fn parse_apk(path: &Path) -> Result<ApkInfo, String> {
    parse_apk_inner(path, &[])
}

/// Parse label + icon + version from a base APK plus optional split APKs
/// (App Bundle installs, T-D). The single-APK pipeline runs first; only when
/// it cannot produce an icon is the config-aware base+split resolver engaged,
/// so single-APK outcomes are unchanged.
pub fn parse_apk_with_splits(base: &Path, splits: &[PathBuf]) -> Result<ApkInfo, String> {
    parse_apk_inner(base, splits)
}

fn parse_apk_inner(path: &Path, splits: &[PathBuf]) -> Result<ApkInfo, String> {
    let apk = apk_info::Apk::new(path)
        .map_err(|e| format!("failed to parse APK {}: {e}", path.display()))?;

    // B1: system APKs (framework-res, RRO overlays, priv-apps) may have no
    // resolvable static label. Hard-failing here prevented meta caching and
    // forced a re-pull of these packages on EVERY refresh (~10s listing).
    // Degrade to an empty label instead; resolve_app_info falls back to the
    // package name and caches the meta as usual.
    let label = apk.get_application_label().unwrap_or_default();

    let version_code = apk.get_version_code().and_then(|s| s.parse::<u64>().ok());

    let icon_png = single_apk_icon(apk).or_else(|| {
        if splits.is_empty() {
            return None;
        }
        // The base APK was moved into the base-only resolver above; re-parse
        // it for the base+split pipeline (fallback path only).
        // Skippable split parse failures: the base alone still resolves.
        let base = apk_info::Apk::new(path).ok()?;
        let split_apks: Vec<apk_info::Apk> =
            splits.iter().filter_map(|p| apk_info::Apk::new(p).ok()).collect();
        multi_apk_icon(base, split_apks)
    });

    Ok(ApkInfo { label, icon_png, version_code })
}

/// Single-APK icon pipeline: adaptive-first (T-E), then the ARSC-resolved
/// `applicationIcon` entry, then a namelist scan for the best legacy raster.
/// Resolution is config-aware (T-H): the base-only `MultiSource` resolver
/// decodes density-only `Compact` entries and ranks densities, so adaptive
/// layers that exist ONLY as density buckets (e.g. Gorio's
/// `mipmap-*-v4/ic_launcher_foreground.webp`) resolve instead of vanishing
/// (the crate's default-config resolution returned `None` for them, leaving
/// only the background rendered).
fn single_apk_icon(apk: apk_info::Apk) -> Option<Vec<u8>> {
    let multi = crate::arsc_resolve::MultiSource::new(apk, Vec::new());
    icon_pipeline(&multi)
}

/// Base+split icon pipeline (T-D): resolve icon entries config-aware across
/// base + split APKs (`MultiSource`), reading layer files from whichever
/// source zip actually contains them. This handles App Bundle apps whose
/// adaptive-icon layers are density-only entries stored in
/// `split_config.*.apk` under obfuscated names. Adaptive-first per T-E.
fn multi_apk_icon(base: apk_info::Apk, splits: Vec<apk_info::Apk>) -> Option<Vec<u8>> {
    let multi = crate::arsc_resolve::MultiSource::new(base, splits);
    icon_pipeline(&multi)
}

/// Shared selection order over a config-aware resolver (T-A unified the
/// single-APK path with the base+split pipeline):
/// 0. Manifest-anchored (T-A): resolve the manifest `android:icon` resource
///    across all configs/splits, ranked anydpi-XML-first, with a degeneracy
///    color guard — this is the launcher's own source of truth.
/// 1. Adaptive-first namelist entry (T-E) -> 2. ARSC-resolved
///    `applicationIcon` entry -> 3. best raster namelist candidate (the
///    legacy heuristics, unchanged, as fallback).
fn icon_pipeline(multi: &crate::arsc_resolve::MultiSource) -> Option<Vec<u8>> {
    let read_entry = |name: &str| multi.read_entry(name);
    let read_icon = |entry: &str| -> Option<Vec<u8>> {
        let bytes = read_entry(entry)?;
        if entry.ends_with(".xml") {
            vector::adaptive_icon_png_res(&bytes, &read_entry, multi)
        } else {
            // Plain raster: normalize padded legacy icons (crop transparent
            // or uniform-opaque margins, scale the content to fill the
            // canvas). Icons whose bytes cannot be decoded (e.g. WebP) are
            // pre-rendered through the SVG pipeline; undecodable bytes pass
            // through unchanged.
            Some(vector::normalize_raster_any(&bytes).unwrap_or(bytes))
        }
    };

    // 0. Manifest-anchored: the launcher resolves `android:icon`, not a
    //    namelist heuristic; try every config variant of that resource
    //    (cross-split reads included) before any name-based guess.
    if let Some(variants) = manifest_icon_variants(multi) {
        if let Some(png) = render_anchored_variants(&variants, multi, &read_entry) {
            return Some(png);
        }
    }

    // 1. Adaptive-first: an `anydpi` adaptive XML renders full-bleed and
    //    looks uniform; prefer it over the ARSC-resolved legacy raster,
    //    which may be a low-density entry with baked transparent padding.
    if let Some(entry) = choose_adaptive_icon_entry(multi.base().namelist()) {
        if let Some(png) = read_icon(entry) {
            return Some(png);
        }
    }
    // 2. ARSC-resolved default-config entry.
    if let Some(png) = multi
        .base()
        .get_application_icon()
        .filter(|entry| !entry.is_empty())
        .and_then(|entry| read_icon(&entry))
    {
        return Some(png);
    }
    // 3. Best remaining raster candidate from the namelist.
    let entry = choose_fallback_icon_entry(multi.base().namelist())?;
    read_icon(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arsc_resolve::ResourceResolver;

    #[test]
    fn fallback_prefers_ic_launcher_over_other_drawables() {
        // ic_launcher base name (at any density) beats unrelated drawables.
        let entries = [
            "res/drawable-xxhdpi-v4/ic_launcher.webp",
            "res/drawable-hdpi-v4/abc_btn.png",
            "AndroidManifest.xml",
            "res/drawable/ic_launcher.xml",
        ];
        // Arrange / Act / Assert
        assert_eq!(
            choose_fallback_icon_entry(entries),
            Some("res/drawable-xxhdpi-v4/ic_launcher.webp")
        );
    }

    #[test]
    fn fallback_prefers_higher_density_over_extension() {
        // A higher-density webp wins over a lower-density png of the same class.
        let entries = [
            "res/drawable-hdpi-v4/ic_launcher.png",
            "res/drawable-xxhdpi-v4/ic_launcher.webp",
        ];
        // Arrange / Act / Assert
        assert_eq!(
            choose_fallback_icon_entry(entries),
            Some("res/drawable-xxhdpi-v4/ic_launcher.webp")
        );
    }

    #[test]
    fn fallback_returns_none_without_candidates() {
        // No res/ raster-or-xml icon candidates -> no fallback entry.
        let entries = ["AndroidManifest.xml", "classes.dex", "res/layout/main.xml"];
        // Arrange / Act / Assert
        assert_eq!(choose_fallback_icon_entry(entries), None);
    }

    #[test]
    fn adaptive_chooser_prefers_anydpi_xml_over_every_density_raster() {
        // T-E: an anydpi adaptive XML outranks even an xxxhdpi raster and is
        // picked by the adaptive-first chooser...
        let entries = [
            "res/mipmap-xxxhdpi-v4/ic_launcher.png",
            "res/mipmap-anydpi-v26/ic_launcher.xml",
            "res/mipmap-mdpi-v4/ic_launcher.webp",
        ];
        // Arrange / Act / Assert
        assert_eq!(
            choose_adaptive_icon_entry(entries),
            Some("res/mipmap-anydpi-v26/ic_launcher.xml")
        );
        // ...but excluded from the raster fallback so a failed adaptive
        // render does not shadow the legacy rasters.
        assert_eq!(
            choose_fallback_icon_entry(entries),
            Some("res/mipmap-xxxhdpi-v4/ic_launcher.png")
        );
    }

    #[test]
    fn adaptive_chooser_returns_none_without_anydpi_entries() {
        // No anydpi XML candidate -> the adaptive-first step yields nothing.
        let entries = [
            "res/mipmap-xxxhdpi-v4/ic_launcher.png",
            "res/mipmap-anydpi-v26/ic_launcher.webp",
            "res/drawable/ic_launcher.xml",
        ];
        // Arrange / Act / Assert
        assert_eq!(choose_adaptive_icon_entry(entries), None);
    }

    /// TEMP diagnostics (bugfinder tier-1): dump raw icon resolution per APK.
    #[test]
    #[ignore = "diagnostics; set DV_DIAG_APKS (semicolon-separated) to run"]
    fn diag_multi_apk_icon_dump() {
        let paths = std::env::var("DV_DIAG_APKS").expect("DV_DIAG_APKS must be set");
        for p in paths.split(';') {
            let apk = match apk_info::Apk::new(Path::new(p)) {
                Ok(a) => a,
                Err(e) => {
                    println!("DIAG {p}: parse error {e}");
                    continue;
                }
            };
            println!(
                "DIAG {p}\n  label={:?}\n  icon={:?}",
                apk.get_application_label(),
                apk.get_application_icon()
            );
            let fb = choose_fallback_icon_entry(apk.namelist());
            println!("  fallback_choice={fb:?}");
        }
    }

    /// T-D: App Bundle apps (base.apk + split_config.*.apk, e.g. Google
    /// Sheets `com.google.android.apps.docs.editors.sheets`) must render
    /// their REAL icon: the adaptive-icon XML lives in base.apk but its
    /// layer resources are density-only entries whose files live in the
    /// density split. `parse_apk_with_splits` must resolve across both.
    /// Set `DV_SHEETS_BASE` (base apk) and `DV_SHEETS_SPLIT` (density split).
    #[test]
    #[ignore = "requires the Sheets fixtures; set DV_SHEETS_BASE + DV_SHEETS_SPLIT to run"]
    fn parses_real_apk_with_splits_renders_split_density_icon() {
        let base = std::env::var("DV_SHEETS_BASE").expect("DV_SHEETS_BASE must be set");
        let split = std::env::var("DV_SHEETS_SPLIT").expect("DV_SHEETS_SPLIT must be set");
        let info =
            parse_apk_with_splits(Path::new(&base), &[PathBuf::from(&split)]).expect("parse ok");

        assert_eq!(info.label, "Sheets", "label must come from the base apk");
        let icon = info
            .icon_png
            .expect("base+split adaptive icon must render (no letter fallback)");
        assert!(&icon[..4] == b"\x89PNG", "icon must be PNG bytes");

        let pixmap = tiny_skia::Pixmap::decode_png(&icon).expect("icon must decode as PNG");
        assert_eq!((pixmap.width(), pixmap.height()), (192, 192), "rendered at 192x192");
        // Non-trivial painted content: the bitmap layers actually rendered.
        let painted = pixmap.data().chunks_exact(4).filter(|px| px[3] > 0).count();
        assert!(painted > 1000, "icon must have painted pixels, got {painted}");
        println!("SPIKE: Sheets icon rendered, {} bytes", icon.len());
    }

    /// T-D regression contract: single-APK outcomes are unchanged by the
    /// split-resolution work. Gorio (adaptive/vector icon) and ORoaming
    /// (density-only namelist fallback icon) must still yield Some.
    /// Set `DV_GORIO_APK` and/or `DV_ROAMING_APK`; at least one is required.
    #[test]
    #[ignore = "requires fixtures; set DV_GORIO_APK and/or DV_ROAMING_APK to run"]
    fn parses_real_apk_with_splits_single_apk_regression() {
        let gorio = std::env::var("DV_GORIO_APK").ok();
        let roaming = std::env::var("DV_ROAMING_APK").ok();
        assert!(gorio.is_some() || roaming.is_some(), "set DV_GORIO_APK or DV_ROAMING_APK");

        for (path, name) in gorio
            .map(|p| (p, "Gorio"))
            .into_iter()
            .chain(roaming.map(|p| (p, "ORoaming")))
        {
            let info =
                parse_apk_with_splits(Path::new(&path), &[]).expect("single-apk parse must work");
            let icon = info.icon_png.expect("{name} icon must stay Some");
            assert!(!icon.is_empty(), "{name} icon must be non-empty");
            println!("SPIKE: {name} regression ok, icon {} bytes", icon.len());
        }
    }

    #[test]
    fn nonexistent_file_returns_err() {
        let result = parse_apk(Path::new("Z:/definitely/not/a/real/file.apk"));
        assert!(result.is_err());
    }

    /// Spike validation against a real APK. Set `DV_TEST_APK` to an APK path.
    #[test]
    #[ignore = "requires a real APK; set DV_TEST_APK to run"]
    fn parses_real_apk_label_and_icon() {
        let path = std::env::var("DV_TEST_APK").expect("DV_TEST_APK must be set");
        let info = parse_apk(Path::new(&path)).expect("parse should succeed");
        assert!(!info.label.is_empty(), "label must be non-empty");
        if let Some(icon) = &info.icon_png {
            assert!(!icon.is_empty(), "icon bytes must be non-empty");
        }
        if let Some(version) = info.version_code {
            assert!(version > 0, "version_code must be positive when present");
        }
        println!(
            "SPIKE: label={:?}, icon={}, version_code={:?}",
            info.label,
            match &info.icon_png {
                Some(bytes) => format!("Some({} bytes)", bytes.len()),
                None => "None".to_string(),
            },
            info.version_code
        );
    }

    /// T-C: an APK whose launcher icon only exists in a density bucket
    /// (ORoaming `com.redteamobile.roaming`, icon at
    /// `res/drawable-xxhdpi-v4/ic_launcher.webp` with no default-config
    /// entry) must fall back to a namelist scan and yield real icon bytes
    /// instead of None. Set `DV_TEST_APK` to the ORoaming APK.
    #[test]
    #[ignore = "requires the ORoaming APK; set DV_TEST_APK to run"]
    fn parses_real_apk_density_only_icon_via_namelist_fallback() {
        let path = std::env::var("DV_TEST_APK").expect("DV_TEST_APK must be set");
        let info = parse_apk(Path::new(&path)).expect("parse should succeed");

        assert_eq!(info.label, "ORoaming", "label must be resolved");
        let icon = info.icon_png.expect("namelist fallback icon must be Some");
        assert!(!icon.is_empty(), "icon bytes must be non-empty");
        assert!(
            icon.starts_with(b"RIFF") || icon.starts_with(b"\x89PNG"),
            "icon must be RIFF/WebP or PNG bytes, got {:?}",
            &icon[..4.min(icon.len())]
        );
        println!(
            "SPIKE: label={:?}, icon={} bytes",
            info.label,
            icon.len()
        );
    }

    /// Adaptive-icon acceptance: an APK with ONLY an adaptive icon XML
    /// (e.g. GitHub `com.github.android`) must now yield a rendered 192x192
    /// PNG instead of None. Set `DV_TEST_APK` to such an APK.
    #[test]
    #[ignore = "requires an adaptive-icon-only APK; set DV_TEST_APK to run"]
    fn parses_real_apk_adaptive_icon_to_rendered_png() {
        let path = std::env::var("DV_TEST_APK").expect("DV_TEST_APK must be set");
        let info = parse_apk(Path::new(&path)).expect("parse should succeed");

        assert_eq!(info.label, "GitHub", "label must be resolved");
        let icon = info.icon_png.expect("adaptive icon must render to PNG");
        assert!(&icon[..4] == b"\x89PNG", "icon must be PNG bytes");
        assert!(icon.len() > 512, "icon must be non-trivial, got {} bytes", icon.len());

        let pixmap = tiny_skia::Pixmap::decode_png(&icon).expect("icon must decode as PNG");
        assert_eq!(
            (pixmap.width(), pixmap.height()),
            (192, 192),
            "icon must be rendered at 192x192"
        );
        // Non-trivial painted content: not a fully transparent image.
        let painted = pixmap
            .data()
            .chunks_exact(4)
            .filter(|px| px[3] > 0)
            .count();
        assert!(painted > 64, "icon must have painted pixels, got {painted}");
        // The white octocat foreground must actually render (T-B: the zip
        // entry path bug previously dropped all XML layers).
        let white = pixmap
            .data()
            .chunks_exact(4)
            .filter(|px| px[3] > 200 && px[0] > 240 && px[1] > 240 && px[2] > 240)
            .count();
        assert!(white > 100, "white foreground pixels must render, got {white}");
    }

    /// Regression guard: an APK with a plain raster launcher icon still
    /// yields its original PNG bytes. Set `DV_TEST_APK2` to such an APK
    /// (e.g. Reddit `com.reddit.frontpage`); the test is skipped when unset.
    #[test]
    #[ignore = "requires a PNG-icon APK; set DV_TEST_APK2 to run"]
    fn parses_real_apk_raster_icon_still_works() {
        let path = std::env::var("DV_TEST_APK2")
            .unwrap_or_else(|_| std::env::var("DV_TEST_APK").expect("DV_TEST_APK2 or DV_TEST_APK"));
        let info = parse_apk(Path::new(&path)).expect("parse should succeed");

        assert!(!info.label.is_empty(), "label must be non-empty");
        let icon = info.icon_png.expect("raster icon must be Some");
        assert!(!icon.is_empty(), "icon bytes must be non-empty");
    }

    /// T-B: gradient-filled adaptive icon (Teams `com.microsoft.teams`):
    /// foreground paths use `@drawable/$name` aapt inline gradient
    /// sub-documents; the icon must render instead of falling back to None.
    #[test]
    #[ignore = "requires the Teams APK; set DV_TEST_APK3 to run"]
    fn parses_real_apk_gradient_adaptive_icon_renders() {
        let path = std::env::var("DV_TEST_APK3").expect("DV_TEST_APK3 must be set");
        let info = parse_apk(Path::new(&path)).expect("parse should succeed");

        assert_eq!(info.label, "Teams", "label must be resolved");
        let icon = info.icon_png.expect("gradient adaptive icon must render");
        assert!(&icon[..4] == b"\x89PNG", "icon must be PNG bytes");

        let pixmap = tiny_skia::Pixmap::decode_png(&icon).expect("icon must decode as PNG");
        // Distinct opaque colors: gradients must produce multi-color output.
        let mut colors = std::collections::HashSet::new();
        for px in pixmap.data().chunks_exact(4) {
            if px[3] > 200 {
                colors.insert((px[0], px[1], px[2]));
            }
        }
        assert!(
            colors.len() > 8,
            "gradient icon must have color diversity, got {} colors",
            colors.len()
        );
    }

    /// T-E: apps that ship BOTH a legacy padded raster icon and an adaptive
    /// icon (Gorio `com.hong.gorio`, Green SM `com.gsm.customer`) must render
    /// the ADAPTIVE icon full-bleed: the rendered icon's alpha bounding box
    /// must cover >= 85% of both canvas dimensions (the legacy mdpi raster
    /// with baked transparent padding would fail this). Set `DV_GORIO_APK`
    /// and/or `DV_GSM_APK`; at least one is required.
    #[test]
    #[ignore = "requires fixtures; set DV_GORIO_APK and/or DV_GSM_APK to run"]
    fn parses_real_apk_adaptive_first_icon_fills_canvas() {
        let gorio = std::env::var("DV_GORIO_APK").ok();
        let gsm = std::env::var("DV_GSM_APK").ok();
        assert!(gorio.is_some() || gsm.is_some(), "set DV_GORIO_APK or DV_GSM_APK");

        for (path, name) in gorio
            .map(|p| (p, "Gorio"))
            .into_iter()
            .chain(gsm.map(|p| (p, "GreenSM")))
        {
            let info = parse_apk(Path::new(&path)).expect("parse must work");
            let icon = info.icon_png.expect("{name} icon must stay Some");
            assert!(&icon[..4] == b"\x89PNG", "{name} icon must be PNG bytes");

            let pixmap = tiny_skia::Pixmap::decode_png(&icon).expect("icon must decode as PNG");
            assert_eq!((pixmap.width(), pixmap.height()), (192, 192));
            let (bw, bh) = alpha_bbox_ratio(&pixmap);
            println!("SPIKE: {name} alpha bbox ratio = {bw:.3} x {bh:.3}");
            assert!(
                bw >= 0.85 && bh >= 0.85,
                "{name} icon content must fill the canvas (>=85%), got {bw:.3} x {bh:.3}"
            );
        }
    }

    /// T-H: an adaptive icon whose FOREGROUND layer exists only as
    /// density-config entries (Gorio `com.hong.gorio`, Green SM
    /// `com.gsm.customer`) must render the foreground logo, not just the
    /// solid background. A background-only render is a solid fill with ~1-3
    /// colors; a rendered logo/wordmark yields many distinct opaque colors.
    /// Set `DV_GORIO_APK` and/or `DV_GREENSM_APK`; at least one is required.
    #[test]
    #[ignore = "requires fixtures; set DV_GORIO_APK and/or DV_GREENSM_APK to run"]
    fn parses_real_apk_adaptive_foreground_renders_color_diversity() {
        let gorio = std::env::var("DV_GORIO_APK").ok();
        let greensm = std::env::var("DV_GREENSM_APK").ok();
        assert!(gorio.is_some() || greensm.is_some(), "set DV_GORIO_APK or DV_GREENSM_APK");

        for (path, name) in gorio
            .map(|p| (p, "Gorio"))
            .into_iter()
            .chain(greensm.map(|p| (p, "GreenSM")))
        {
            let info = parse_apk(Path::new(&path)).expect("parse must work");
            let icon = info.icon_png.expect("{name} icon must be Some");
            assert!(&icon[..4] == b"\x89PNG", "{name} icon must be PNG bytes");

            let pixmap = tiny_skia::Pixmap::decode_png(&icon).expect("icon must decode as PNG");
            assert_eq!((pixmap.width(), pixmap.height()), (192, 192));
            let mut colors = std::collections::HashSet::new();
            for px in pixmap.data().chunks_exact(4) {
                if px[3] > 200 {
                    colors.insert((px[0], px[1], px[2]));
                }
            }
            println!("SPIKE: {name} distinct opaque colors = {}", colors.len());
            assert!(
                colors.len() >= 8,
                "{name} icon must contain rendered foreground content (>=8 colors), got {}",
                colors.len()
            );
        }
    }

    /// Fraction of the canvas covered by the opaque-content bounding box.
    fn alpha_bbox_ratio(pixmap: &tiny_skia::Pixmap) -> (f32, f32) {
        let (w, h) = (pixmap.width() as usize, pixmap.height() as usize);
        let mut min_x = w;
        let mut min_y = h;
        let mut max_x = 0usize;
        let mut max_y = 0usize;
        for (i, px) in pixmap.data().chunks_exact(4).enumerate() {
            if px[3] > 8 {
                let (x, y) = (i % w, i / w);
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
        if max_x < min_x {
            return (0.0, 0.0);
        }
        (
            (max_x - min_x + 1) as f32 / w as f32,
            (max_y - min_y + 1) as f32 / h as f32,
        )
    }

    /// T-E: a plain raster launcher icon (PNG) is served normalized: the
    /// rendered icon's content fills the canvas even when the source raster
    /// carries baked transparent padding. WebP rasters pass through
    /// unchanged (cannot be decoded as PNG) — this test asserts the PNG
    /// case. Set `DV_RASTER_APK` to an APK whose launcher icon is a plain
    /// PNG raster (none of the current local fixtures qualifies; the
    /// behavior itself is covered by the vector.rs normalize unit tests).
    #[test]
    #[ignore = "requires a PNG-icon APK; set DV_RASTER_APK to run"]
    fn parses_real_apk_plain_raster_icon_normalized_full_bleed() {
        let path = std::env::var("DV_RASTER_APK").expect("DV_RASTER_APK must be set");
        let info = parse_apk(Path::new(&path)).expect("parse must work");

        let icon = info.icon_png.expect("raster icon must stay Some");
        assert!(&icon[..4] == b"\x89PNG", "icon must be PNG bytes");

        let pixmap = tiny_skia::Pixmap::decode_png(&icon).expect("icon must decode as PNG");
        assert_eq!((pixmap.width(), pixmap.height()), (192, 192));
        let (bw, bh) = alpha_bbox_ratio(&pixmap);
        println!("SPIKE: raster icon alpha bbox ratio = {bw:.3} x {bh:.3}");
        assert!(
            bw >= 0.85 && bh >= 0.85,
            "normalized raster content must fill the canvas (>=85%), got {bw:.3} x {bh:.3}"
        );
    }

    /// T-B: `<bitmap>` launcher drawable (Shopee `com.shopee.vn`) resolves
    /// its `android:src` to the raster mipmaps and renders.
    #[test]
    #[ignore = "requires the Shopee APK; set DV_TEST_APK4 to run"]
    fn parses_real_apk_bitmap_drawable_icon_renders() {
        let path = std::env::var("DV_TEST_APK4").expect("DV_TEST_APK4 must be set");
        let info = parse_apk(Path::new(&path)).expect("parse should succeed");

        assert_eq!(info.label, "Shopee", "label must be resolved");
        let icon = info.icon_png.expect("bitmap drawable icon must render");
        assert!(&icon[..4] == b"\x89PNG", "icon must be PNG bytes");
        assert!(icon.len() > 512, "icon must be non-trivial, got {} bytes", icon.len());
    }

    // ---------- T-A: manifest-anchored icon resolution ----------

    #[test]
    fn parse_icon_ref_token_parses_hex_id_named_and_rejects_non_refs() {
        // Arrange / Act / Assert: the hex fallback the AXML decoder emits
        // without an ARSC, the 0x form, named forms, and non-references.
        assert_eq!(parse_icon_ref_token("@7f0d0002"), Some(IconRef::Id(0x7f0d0002)));
        assert_eq!(parse_icon_ref_token("@0x7f0d0002"), Some(IconRef::Id(0x7f0d0002)));
        assert_eq!(
            parse_icon_ref_token("@mipmap/ic_launcher"),
            Some(IconRef::Name("mipmap/ic_launcher".to_string()))
        );
        assert_eq!(
            parse_icon_ref_token("@android:drawable/ic_launcher"),
            Some(IconRef::Name("drawable/ic_launcher".to_string()))
        );
        assert_eq!(parse_icon_ref_token("res/ic_launcher.png"), None);
        assert_eq!(parse_icon_ref_token(""), None);
        assert_eq!(parse_icon_ref_token("@zzzz"), None);
    }

    /// Fraction of opaque pixels that are near-black (all channels < 24).
    fn near_black_fraction(png: &[u8]) -> f32 {
        let pm = tiny_skia::Pixmap::decode_png(png).expect("icon must decode as PNG");
        let total = pm.data().chunks_exact(4).filter(|px| px[3] > 200).count();
        if total == 0 {
            return 0.0;
        }
        let black = pm
            .data()
            .chunks_exact(4)
            .filter(|px| px[3] > 200 && px[0] < 24 && px[1] < 24 && px[2] < 24)
            .count();
        black as f32 / total as f32
    }

    /// Fraction of opaque pixels that are near-white (all channels > 232).
    fn near_white_fraction(png: &[u8]) -> f32 {
        let pm = tiny_skia::Pixmap::decode_png(png).expect("icon must decode as PNG");
        let total = pm.data().chunks_exact(4).filter(|px| px[3] > 200).count();
        if total == 0 {
            return 0.0;
        }
        let white = pm
            .data()
            .chunks_exact(4)
            .filter(|px| px[3] > 200 && px[0] > 232 && px[1] > 232 && px[2] > 232)
            .count();
        white as f32 / total as f32
    }

    /// C1 (stub preemption): Shopee / VNeID / TopDev / Theme Store / realme
    /// Link currently ALL render the identical 3898-byte green-robot stub
    /// because the adaptive-first namelist scan picks a shared template
    /// instead of the manifest icon resource. Each app must render its own
    /// real icon (pairwise DISTINCT bytes). Set `DV_T1_C1_APKS`
    /// (semicolon-separated base apk paths).
    #[test]
    #[ignore = "requires dv_tier1 fixtures; set DV_T1_C1_APKS (semicolon-separated) to run"]
    fn manifest_anchor_c1_stub_apps_render_distinct_icons() {
        let paths = std::env::var("DV_T1_C1_APKS").expect("DV_T1_C1_APKS must be set");
        let apps: Vec<String> = paths.split(';').map(str::to_string).collect();
        assert!(apps.len() >= 2, "need at least 2 C1 fixtures for byte-distinctness");

        let mut icons: Vec<(String, Vec<u8>)> = Vec::new();
        for path in &apps {
            let info = parse_apk(Path::new(path)).expect("base-only parse must work");
            let icon = info
                .icon_png
                .unwrap_or_else(|| panic!("{path}: icon must be Some"));
            assert!(!icon.is_empty(), "{path}: icon must be non-empty");
            icons.push((path.clone(), icon));
        }

        for (i, (pa, ia)) in icons.iter().enumerate() {
            for (pb, ib) in icons.iter().skip(i + 1) {
                assert!(
                    ia.as_slice() != ib.as_slice(),
                    "{pa} and {pb} must NOT render identical bytes (shared stub signature): \
                     both rendered {} bytes",
                    ia.len()
                );
            }
        }
        for (p, icon) in &icons {
            println!("SPIKE: C1 {p}: {} bytes", icon.len());
        }
    }

    /// C2 (anydpi overreach): Weather currently renders a fully-black 2088B
    /// icon from an unrelated `ic_call_answer` anydpi drawable. The manifest
    /// icon resource (`ic_launcher_weather`, density rasters) must win
    /// instead — its xxxhdpi entry is WebP, which passes through un-decoded,
    /// so the black-PNG signature is asserted negatively (a decoded PNG must
    /// not be near-black). Set `DV_T1_WEATHER_APK`.
    #[test]
    #[ignore = "requires the Weather fixture; set DV_T1_WEATHER_APK to run"]
    fn manifest_anchor_c2_weather_icon_not_near_black() {
        let path = std::env::var("DV_T1_WEATHER_APK").expect("DV_T1_WEATHER_APK must be set");
        let info = parse_apk(Path::new(&path)).expect("parse must work");

        let icon = info.icon_png.expect("weather icon must be Some");
        assert!(
            icon.starts_with(b"RIFF") || icon.starts_with(b"\x89PNG"),
            "icon must be WebP/RIFF or PNG bytes, got {:?}",
            &icon[..4.min(icon.len())]
        );
        assert_ne!(
            icon.len(),
            2088,
            "weather icon must not be the old black 2088B render (bug C2)"
        );
        if icon.starts_with(b"\x89PNG") {
            let frac = near_black_fraction(&icon);
            println!("SPIKE: weather near-black fraction = {frac:.3}");
            assert!(
                frac <= 0.95,
                "weather icon must not be near-black (bug C2), got {frac:.3}"
            );
        }
        println!("SPIKE: weather icon {} bytes", icon.len());
    }

    /// C3a (Telegram): the adaptive `ic_launcher.xml` render lacks the
    /// background and yields a black 2-color image; the density rasters in
    /// base must be used instead. Set `DV_T1_TELEGRAM_APK`.
    #[test]
    #[ignore = "requires the Telegram fixture; set DV_T1_TELEGRAM_APK to run"]
    fn manifest_anchor_c3_telegram_not_two_color_black_white() {
        let path = std::env::var("DV_T1_TELEGRAM_APK").expect("DV_T1_TELEGRAM_APK must be set");
        let info = parse_apk(Path::new(&path)).expect("parse must work");

        let icon = info.icon_png.expect("telegram icon must be Some");
        assert!(&icon[..4] == b"\x89PNG", "icon must be PNG bytes");
        let colors = vector::distinct_opaque_colors(&icon);
        let white = near_white_fraction(&icon);
        println!("SPIKE: telegram colors = {colors}, white fraction = {white:.3}");
        assert!(
            colors >= 3 && white <= 0.90,
            "telegram icon must not be a 2-color black+white render (bug C3), \
             got {colors} colors, white fraction {white:.3}"
        );
    }

    /// C3b (APKPure): the ARSC default-config entry is the mdpi back-arrow
    /// `res/mipmap-mdpi/ic_launcher.png`; the xxxhdpi raster must win
    /// instead. Rendered bytes must differ from the (normalized) mdpi
    /// entry render. Set `DV_T1_APKPURE_APK`.
    #[test]
    #[ignore = "requires the APKPure fixture; set DV_T1_APKPURE_APK to run"]
    fn manifest_anchor_c3_apkpure_not_mdpi_arrow() {
        let path = std::env::var("DV_T1_APKPURE_APK").expect("DV_T1_APKPURE_APK must be set");
        let info = parse_apk(Path::new(&path)).expect("parse must work");

        let icon = info.icon_png.expect("apkpure icon must be Some");
        assert!(&icon[..4] == b"\x89PNG", "icon must be PNG bytes");

        let apk = apk_info::Apk::new(Path::new(&path)).expect("re-parse for mdpi bytes");
        let mdpi = apk
            .read("res/mipmap-mdpi/ic_launcher.png")
            .expect("mdpi arrow entry must exist")
            .0;
        let mdpi_render = vector::normalize_raster_png(&mdpi).unwrap_or(mdpi);
        assert!(
            icon.as_slice() != mdpi_render.as_slice(),
            "apkpure must not render the mdpi arrow (bug C3): \
             rendered {} bytes == mdpi render {} bytes",
            icon.len(),
            mdpi_render.len()
        );
        // The current wrong render is a near-empty white image; the real
        // xxxhdpi icon is the green "A" mark filling the frame.
        let white = near_white_fraction(&icon);
        println!("SPIKE: apkpure white fraction = {white:.3}");
        assert!(
            white <= 0.90,
            "apkpure icon must not be a near-white stub render (bug C3), got {white:.3}"
        );
        println!("SPIKE: apkpure icon {} bytes (mdpi render {} bytes)", icon.len(), mdpi_render.len());
    }

    /// C3c (Google Wallet): the adaptive MP.xml render misses a layer and
    /// yields a white 563B partial image; with splits the real colored icon
    /// must render. Set `DV_T1_WALLET_APK` and `DV_T1_WALLET_SPLITS`
    /// (semicolon-separated split paths).
    #[test]
    #[ignore = "requires the Wallet fixtures; set DV_T1_WALLET_APK + DV_T1_WALLET_SPLITS to run"]
    fn manifest_anchor_c3_wallet_renders_colored_icon() {
        let path = std::env::var("DV_T1_WALLET_APK").expect("DV_T1_WALLET_APK must be set");
        let splits = std::env::var("DV_T1_WALLET_SPLITS").unwrap_or_default();
        let splits: Vec<PathBuf> =
            splits.split(';').filter(|s| !s.is_empty()).map(PathBuf::from).collect();
        let info = parse_apk_with_splits(Path::new(&path), &splits).expect("parse must work");

        let icon = info.icon_png.expect("wallet icon must be Some");
        assert!(&icon[..4] == b"\x89PNG", "icon must be PNG bytes");
        let colors = vector::distinct_opaque_colors(&icon);
        let white = near_white_fraction(&icon);
        println!("SPIKE: wallet colors = {colors}, white fraction = {white:.3}");
        assert!(
            colors >= 4,
            "wallet icon must not be a white partial render (bug C3), got {colors} colors"
        );
        assert!(
            white <= 0.95,
            "wallet icon must not be near-white (bug C3), got {white:.3}"
        );
    }

    /// C4 (dead-end): Google Drive's base APK has NO drawable/mipmap
    /// directories at all; the manifest icon is density-only and its files
    /// live in the config splits. With splits supplied, the icon must render
    /// (currently None). Set `DV_T1_DOCS_BASE` and `DV_T1_DOCS_SPLITS`
    /// (semicolon-separated split paths).
    #[test]
    #[ignore = "requires the Drive fixtures; set DV_T1_DOCS_BASE + DV_T1_DOCS_SPLITS to run"]
    fn manifest_anchor_c4_docs_with_splits_renders_icon() {
        let base = std::env::var("DV_T1_DOCS_BASE").expect("DV_T1_DOCS_BASE must be set");
        let splits = std::env::var("DV_T1_DOCS_SPLITS").expect("DV_T1_DOCS_SPLITS must be set");
        let splits: Vec<PathBuf> =
            splits.split(';').filter(|s| !s.is_empty()).map(PathBuf::from).collect();
        let info = parse_apk_with_splits(Path::new(&base), &splits).expect("parse must work");

        let icon = info.icon_png.expect("drive icon must render with splits (bug C4: was None)");
        assert!(!icon.is_empty(), "icon must be non-empty");
        println!("SPIKE: drive icon {} bytes", icon.len());
    }

    /// C4 (dead-end): OPPO My Files (`com.coloros.filemanager`, no splits)
    /// has an obfuscated flat `res/` layout; the density-only manifest icon
    /// must resolve through the config-aware resolver (currently None).
    /// Set `DV_T1_FILEMANAGER_APK`.
    #[test]
    #[ignore = "requires the My Files fixture; set DV_T1_FILEMANAGER_APK to run"]
    fn manifest_anchor_c4_filemanager_base_only_renders_icon() {
        let path = std::env::var("DV_T1_FILEMANAGER_APK").expect("DV_T1_FILEMANAGER_APK must be set");
        let info = parse_apk(Path::new(&path)).expect("parse must work");

        let icon = info
            .icon_png
            .expect("filemanager icon must render from base alone (bug C4: was None)");
        assert!(!icon.is_empty(), "icon must be non-empty");
        println!("SPIKE: filemanager icon {} bytes", icon.len());
    }

    /// TEMP diag helper: recursively resolve and dump a reference token's
    /// target AXML structure (WALLET deep-dump).
    fn dump_ref_deep(
        multi: &crate::arsc_resolve::MultiSource,
        raw: &str,
        depth: usize,
    ) {
        if depth > 3 {
            return;
        }
        let resolved = vector::resolve_ref_token(raw, multi);
        println!("      {} {raw} -> {resolved:?}", "  ".repeat(depth));
        let Some(value) = resolved else { return };
        if !value.starts_with("res/") {
            return;
        }
        let Some(bytes) = multi.read_entry(&value) else {
            println!("      {} entry {value} UNREADABLE", "  ".repeat(depth));
            return;
        };
        let Ok(axml) = apk_info::AXML::new(&mut &bytes[..], multi.axml_arsc()) else {
            println!("      {} {value}: not AXML ({} bytes)", "  ".repeat(depth), bytes.len());
            return;
        };
        println!(
            "      {} <{}> attrs={:?}",
            "  ".repeat(depth),
            axml.root.name(),
            axml.root
                .attributes()
                .map(|a| format!("{}={}", a.name(), a.value()))
                .collect::<Vec<_>>()
        );
        for child in axml.root.childrens() {
            println!(
                "      {} <{}> drawable={:?}",
                "  ".repeat(depth + 1),
                child.name(),
                child.attr("drawable")
            );
            if let Some(inner) = child.attr("drawable") {
                dump_ref_deep(multi, inner, depth + 2);
            }
        }
    }

    /// TEMP diagnostics (T-A): dump the raw manifest icon token, the ranked
    /// variants, and the per-variant render/guard outcome for each APK.
    /// Set `DV_T1_DIAG_APKS` (semicolon-separated apk paths).
    #[test]
    #[ignore = "diagnostics; set DV_T1_DIAG_APKS (semicolon-separated) to run"]
    fn diag_manifest_icon_anchor_dump() {        let paths = std::env::var("DV_T1_DIAG_APKS").expect("DV_T1_DIAG_APKS must be set");
        for p in paths.split(';') {
            let apk = match apk_info::Apk::new(Path::new(p)) {
                Ok(a) => a,
                Err(e) => {
                    println!("DIAG {p}: parse error {e}");
                    continue;
                }
            };
            let manifest = match apk.read("AndroidManifest.xml") {
                Ok((b, _)) => b,
                Err(e) => {
                    println!("DIAG {p}: no manifest: {e:?}");
                    continue;
                }
            };
            let raw = apk_info::AXML::new(&mut &manifest[..], None)
                .ok()
                .and_then(|axml| axml.get_attribute_value("application", "icon", None));
            println!("DIAG {p}\n  raw_icon_token={raw:?}");
            // Anchored variants + per-variant render/guard outcomes.
            let split_paths: Vec<PathBuf> = std::env::var("DV_T1_DIAG_SPLITS")
                .unwrap_or_default()
                .split(';')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect();
            let multi = crate::arsc_resolve::MultiSource::new(
                apk_info::Apk::new(Path::new(p)).expect("re-parse"),
                split_paths
                    .iter()
                    .filter_map(|sp| apk_info::Apk::new(sp).ok())
                    .collect(),
            );
            if let Some(variants) = manifest_icon_variants(&multi) {
                for v in &variants {
                    let read = multi.read_entry(v).map(|b| b.len());
                    let outcome = match read {
                        None => "unreadable".to_string(),
                        Some(n) if n == 0 => "empty".to_string(),
                        Some(n) => {
                            if v.ends_with(".xml") {
                                match multi.read_entry(v).map(|b| {
                                    vector::adaptive_icon_png_res(&b, &|name: &str| {
                                        multi.read_entry(name)
                                    }, &multi)
                                }) {
                                    Some(Some(png)) => format!(
                                        "xml render {}B colors={}",
                                        png.len(),
                                        vector::distinct_opaque_colors(&png)
                                    ),
                                    Some(None) => "xml render None".to_string(),
                                    None => "read err".to_string(),
                                }
                            } else {
                                format!("raster {n}B")
                            }
                        }
                    };
                    println!("  variant {v} -> {outcome}");
                }
                // Dump the first XML variant's decoded structure.
                if let Some(v) = variants.iter().find(|v| v.ends_with(".xml")) {
                    if let Some(bytes) = multi.read_entry(v) {
                        if let Ok(axml) =
                            apk_info::AXML::new(&mut &bytes[..], multi.axml_arsc())
                        {
                            println!("  xml root={}", axml.root.name());
                            for child in axml.root.childrens() {
                                println!(
                                    "    layer <{}> drawable={:?} children={:?}",
                                    child.name(),
                                    child.attr("drawable"),
                                    child.childrens().map(|c| c.name()).collect::<Vec<_>>()
                                );
                            }
                            if let Ok(a) = apk_info::AXML::new(&mut &bytes[..], None) {
                                for child in a.root.childrens() {
                                    println!(
                                        "    raw layer <{}> drawable={:?}",
                                        child.name(),
                                        child.attr("drawable")
                                    );
                                }
                            }
                            // Deep-dump a resolved layer reference (WALLET diag).
                            for layer_name in ["background", "foreground"] {
                                if let Some(layer) =
                                    axml.root.childrens().find(|c| c.name() == layer_name)
                                {
                                    if let Some(raw) = layer.attr("drawable") {
                                        println!("    deep-dump {layer_name} raw={raw:?}");
                                        dump_ref_deep(&multi, raw, 0);
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                println!("  variants=None");
            }
            // Current-pipeline render color histogram (top 5 opaque colors).
            if let Ok(info) = parse_apk_with_splits(
                Path::new(p),
                &std::env::var("DV_T1_DIAG_SPLITS")
                    .unwrap_or_default()
                    .split(';')
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .collect::<Vec<_>>(),
            ) {
                if let Some(icon) = &info.icon_png {
                    if let Ok(pm) = tiny_skia::Pixmap::decode_png(icon) {
                        let total = pm.data().chunks_exact(4).filter(|px| px[3] > 200).count();
                        let mut hist: std::collections::HashMap<(u8, u8, u8), usize> =
                            std::collections::HashMap::new();
                        for px in pm.data().chunks_exact(4) {
                            if px[3] > 200 {
                                *hist.entry((px[0], px[1], px[2])).or_insert(0) += 1;
                            }
                        }
                        let mut top: Vec<_> = hist.into_iter().collect();
                        top.sort_by(|a, b| b.1.cmp(&a.1));
                        let tops: Vec<String> = top
                            .into_iter()
                            .take(5)
                            .map(|(c, n)| format!("#{:02x}{:02x}{:02x}={:.1}%", c.0, c.1, c.2, n as f32 * 100.0 / total.max(1) as f32))
                            .collect();
                        println!(
                            "  current_render={} bytes, distinct={}, top: {}",
                            icon.len(),
                            "see top",
                            tops.join(", ")
                        );
                    } else {
                        println!("  current_render={} bytes (not decodable PNG)", icon.len());
                    }
                } else {
                    println!("  current_render=None");
                }
            }
        }
    }

    // ---------- R2 (Gboard / Cốc Cốc / OneDrive) regression spikes ----------

    /// TEMP diagnostics: dump the config variants of specific resource ids
    /// across base + splits. Set `DV_T1_DIAG2_APK` (+ optional
    /// `DV_T1_DIAG2_SPLITS`, semicolon-separated) and `DV_T1_DIAG2_IDS`.
    #[test]
    #[ignore = "diagnostics; set DV_T1_DIAG2_APK, DV_T1_DIAG2_SPLITS, DV_T1_DIAG2_IDS"]
    fn diag_split_resource_variants_dump() {
        let base = std::env::var("DV_T1_DIAG2_APK").expect("DV_T1_DIAG2_APKS must be set");
        let splits: Vec<apk_info::Apk> = std::env::var("DV_T1_DIAG2_SPLITS")
            .unwrap_or_default()
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|p| apk_info::Apk::new(Path::new(p)).expect("split parse"))
            .collect();
        let apk = apk_info::Apk::new(Path::new(&base)).expect("base parse");
        let multi = crate::arsc_resolve::MultiSource::new(apk, splits);
        for id in std::env::var("DV_T1_DIAG2_IDS")
            .expect("DV_T1_DIAG2_IDS must be set")
            .split(';')
        {
            let id = u32::from_str_radix(id.trim().trim_start_matches("0x"), 16)
                .expect("hex resource id");
            println!("DIAG2 id {id:#010x}: variants = {:?}", multi.get_resource_variants(id));
        }
    }

    /// Render a trivial SVG body to a 64x64 PNG (test helper).
    fn svg_png(body: &str) -> Vec<u8> {
        vector::render_svg(
            &format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"64\" height=\"64\">{body}</svg>"
            ),
            64,
        )
        .expect("svg renders")
    }

    #[test]
    fn icon_looks_weak_flags_solid_accepts_rich_and_skips_non_png() {
        // Solid background-only render (Gboard symptom) is weak.
        let solid = svg_png("<rect width=\"64\" height=\"64\" fill=\"#4285f4\"/>");
        assert!(icon_looks_weak(&solid), "solid-color PNG must classify weak");

        // Content-rich render is healthy.
        let rich = svg_png(
            "<rect width=\"64\" height=\"64\" fill=\"#4285f4\"/>\
             <circle cx=\"20\" cy=\"20\" r=\"12\" fill=\"#ffffff\"/>\
             <circle cx=\"44\" cy=\"44\" r=\"12\" fill=\"#ea4335\"/>\
             <rect x=\"28\" y=\"28\" width=\"10\" height=\"10\" fill=\"#34a853\"/>",
        );
        assert!(!icon_looks_weak(&rich), "multi-color PNG must classify healthy");

        // Non-PNG bytes (WebP rasters) and empty bytes cannot be judged.
        assert!(!icon_looks_weak(b"RIFF\x00\x00\x00\x00WEBPVP8 junk"), "webp must not be weak");
        assert!(!icon_looks_weak(&[]), "empty bytes must not be weak");
    }

    /// R2-Gboard: the obfuscated adaptive layers share one resource NAME
    /// (`mipmap/0_resource_name_obfuscated`) but have different ids; the
    /// raw-id layer resolution must render the real foreground (base-only
    /// and with the density split loaded). Set `DV_T1_RES_GBOARD_BASE`
    /// (+ optional `DV_T1_RES_GBOARD_SPLIT`).
    #[test]
    #[ignore = "requires fixtures; set DV_T1_RES_GBOARD_BASE (+ DV_T1_RES_GBOARD_SPLIT) to run"]
    fn residual_gboard_icon_decodes_rich() {
        let base = std::env::var("DV_T1_RES_GBOARD_BASE").expect("DV_T1_RES_GBOARD_BASE");

        let assert_rich = |icon: &[u8], what: &str| {
            assert_eq!(&icon[..4], b"\x89PNG", "{what}: must be a rendered PNG");
            let colors = vector::distinct_opaque_colors(icon);
            let dominant = vector::dominant_opaque_fraction(icon);
            println!("SPIKE gboard {what}: {} bytes colors={colors} dominant={dominant:.3}", icon.len());
            assert!(colors >= 8, "{what}: foreground must add content colors, got {colors}");
            assert!(dominant <= 0.95, "{what}: one color must not dominate, got {dominant:.3}");
        };

        let base_info = parse_apk(Path::new(&base)).expect("base-only parse must work");
        assert_rich(
            &base_info.icon_png.expect("base-only icon must be Some"),
            "base-only",
        );

        if let Ok(split) = std::env::var("DV_T1_RES_GBOARD_SPLIT") {
            let info = parse_apk_with_splits(Path::new(&base), &[PathBuf::from(&split)])
                .expect("parse with split must work");
            assert_rich(&info.icon_png.expect("split icon must be Some"), "with-split");
        }
    }

    /// R2-Cốc Cốc: extension-less obfuscated layers must resolve, so the
    /// final icon is a decoded content-rich PNG (not a raw white-bg WebP).
    /// Set `DV_T1_RES_COCCOC`.
    #[test]
    #[ignore = "requires the Cốc Cốc fixture; set DV_T1_RES_COCCOC to run"]
    fn residual_coccoc_icon_decodes_rich() {
        let path = std::env::var("DV_T1_RES_COCCOC").expect("DV_T1_RES_COCCOC");
        let info = parse_apk(Path::new(&path)).expect("parse must work");

        let icon = info.icon_png.expect("icon must be Some");
        assert_eq!(&icon[..4], b"\x89PNG", "must be PNG bytes, got {:?}", &icon[..4]);
        let colors = vector::distinct_opaque_colors(&icon);
        println!("SPIKE coccoc: {} bytes colors={colors}", icon.len());
        assert!(colors >= 8, "icon must be content-rich, got {colors}");
    }

    /// R2-OneDrive: the layer-list foreground must compose so the final icon
    /// is a decoded, content-rich, non-dominated PNG. Set
    /// `DV_T1_RES_ONEDRIVE`.
    #[test]
    #[ignore = "requires the OneDrive fixture; set DV_T1_RES_ONEDRIVE to run"]
    fn residual_onedrive_icon_decodes_rich() {
        let path = std::env::var("DV_T1_RES_ONEDRIVE").expect("DV_T1_RES_ONEDRIVE");
        let info = parse_apk(Path::new(&path)).expect("parse must work");

        let icon = info.icon_png.expect("icon must be Some");
        assert_eq!(&icon[..4], b"\x89PNG", "must be PNG bytes, got {:?}", &icon[..4]);
        let colors = vector::distinct_opaque_colors(&icon);
        let dominant = vector::dominant_opaque_fraction(&icon);
        println!("SPIKE onedrive: {} bytes colors={colors} dominant={dominant:.3}", icon.len());
        assert!(colors >= 8, "icon must be content-rich, got {colors}");
        assert!(dominant <= 0.95, "one color must not dominate, got {dominant:.3}");
    }

    /// R2-F4: a white-bg WebP raster fallback must pre-render through the SVG
    /// pipeline and be background-keyed cropped to a full-bleed PNG. Set
    /// `DV_T1_RES_COCCOC` (extracts `res/TBQ` from the fixture).
    #[test]
    #[ignore = "requires the Cốc Cốc fixture; set DV_T1_RES_COCCOC to run"]
    fn residual_webp_raster_fallback_normalizes_to_full_bleed() {
        let path = std::env::var("DV_T1_RES_COCCOC").expect("DV_T1_RES_COCCOC");
        let apk = apk_info::Apk::new(Path::new(&path)).expect("open fixture");
        let (tbq, _) = apk.read("res/TBQ").expect("res/TBQ present in fixture");

        // Act
        let out = vector::normalize_raster_any(&tbq).expect("webp must normalize");

        // Assert
        assert_eq!(&out[..4], b"\x89PNG", "output must be PNG bytes");
        let pm = tiny_skia::Pixmap::decode_png(&out).expect("decodes");
        let px = pm.pixel(4, 4).expect("corner pixel exists");
        assert!(
            !(px.red() > 240 && px.green() > 240 && px.blue() > 240),
            "corner must be content, not background white, got {px:?}"
        );
    }

    /// B1 (latency): system APKs whose static label cannot be resolved
    /// (framework-res, RRO overlays, priv-apps like Tag) previously hard-
    /// failed `parse_apk` → no meta cached → re-pulled on EVERY refresh
    /// (~10s for 149 apps). Parse must now succeed with a degraded (possibly
    /// empty) label and the real versionCode so the meta is cached.
    /// Set `DV_T2_DIAG_TAG_APK` and `DV_T2_DIAG_MISS_APK` (framework-res).
    #[test]
    #[ignore = "requires fixtures; set DV_T2_DIAG_TAG_APK + DV_T2_DIAG_MISS_APK to run"]
    fn residual_labelless_apk_parses_ok_with_version() {
        for (name, var) in [
            ("tag", "DV_T2_DIAG_TAG_APK"),
            ("framework-res", "DV_T2_DIAG_MISS_APK"),
        ] {
            let path = std::env::var(var).unwrap_or_else(|_| panic!("{var} must be set"));
            let info = parse_apk(Path::new(&path))
                .unwrap_or_else(|e| panic!("parse {name} must not fail: {e}"));
            assert!(
                info.version_code.is_some(),
                "{name} must expose a versionCode so meta is cached"
            );
            println!("SPIKE {name}: label={:?} version={:?}", info.label, info.version_code);
        }
    }
}
