//! Config-aware resource resolution across base + split APKs (T-D).
//!
//! The `apk-info` crate resolves resource ids through the DEFAULT config
//! only (`ARSC::get_resource_value` hardcodes `ResTableConfig::default()`
//! and only decodes `ResTableEntry::Default`). App Bundle apps store their
//! launcher layers as density-only entries (decoded as `ResTableEntry::Compact`)
//! whose files live in `split_config.*.apk` splits — so single-APK, default-
//! config resolution returns `None` and the UI falls back to a letter icon.
//!
//! This module provides:
//! - [`ResourceResolver`]: the reference-resolution surface `vector.rs` needs,
//!   implemented for `ARSC` (delegates to crate behavior, so single-APK usage
//!   is unchanged) and [`MultiSource`] (base-first, config-aware across all
//!   sources, preferring file values that can actually be read).
//! - [`MultiSource`]: an ordered list of parsed APKs (base first, then
//!   splits), each contributing its `resources.arsc` and a zip reader.

use apk_info::structs::{ResTableConfig, ResTableEntry, ResourceValueType};
use apk_info::{ARSC, Apk};

/// Max recursion depth when a resource references another resource.
const MAX_REF_DEPTH: usize = 5;

/// Resource-id resolution surface used by the icon pipeline.
pub trait ResourceResolver {
    /// Resolve a resource id (e.g. `0x7f0d0002`) to its value string
    /// (a file path like `res/0pB.png`, a color literal, ...).
    fn get_resource_value(&self, id: u32) -> Option<String>;

    /// Resolve a `type/entry` resource name (e.g. `mipmap/ic_launcher_fg`).
    fn get_resource_value_by_name(&self, name: &str) -> Option<String>;

    /// ARSC used for AXML-internal string rendering (raw reference tokens).
    /// Single-source resolvers return themselves; `None` means no table.
    fn axml_arsc(&self) -> Option<&ARSC>;
}

impl ResourceResolver for ARSC {
    fn get_resource_value(&self, id: u32) -> Option<String> {
        // Delegate to the crate's inherent method (default-config behavior,
        // unchanged for the single-APK path).
        ARSC::get_resource_value(self, id)
    }

    fn get_resource_value_by_name(&self, name: &str) -> Option<String> {
        ARSC::get_resource_value_by_name(self, name)
    }

    fn axml_arsc(&self) -> Option<&ARSC> {
        Some(self)
    }
}

/// Resolver that never resolves (replaces the old `Option<&ARSC>` = `None`).
pub struct NullResolver;

impl ResourceResolver for NullResolver {
    fn get_resource_value(&self, _id: u32) -> Option<String> {
        None
    }

    fn get_resource_value_by_name(&self, _name: &str) -> Option<String> {
        None
    }

    fn axml_arsc(&self) -> Option<&ARSC> {
        None
    }
}

/// An ordered set of APK sources (base first, then splits) with config-aware
/// resource resolution across all of them.
pub struct MultiSource {
    /// Base APK first, then splits in the order given by the caller.
    sources: Vec<Apk>,
    /// Parsed `resources.arsc` per source (parallel to `sources`).
    arscs: Vec<Option<ARSC>>,
}

impl MultiSource {
    /// Build a resolver from a base APK plus its split APKs. Split parse
    /// failures are skipped (the base still resolves alone).
    pub fn new(base: Apk, splits: Vec<Apk>) -> Self {
        let mut sources = Vec::with_capacity(1 + splits.len());
        sources.push(base);
        sources.extend(splits);
        let arscs = sources
            .iter()
            .map(|apk| {
                apk.read("resources.arsc")
                    .ok()
                    .and_then(|(data, _)| ARSC::new(&mut &data[..]).ok())
            })
            .collect();
        MultiSource { sources, arscs }
    }

    /// The base (first) APK — its manifest provides the icon entry.
    pub fn base(&self) -> &Apk {
        &self.sources[0]
    }

    /// Composite zip read: try each source in order (base first).
    pub fn read_entry(&self, name: &str) -> Option<Vec<u8>> {
        self.sources
            .iter()
            .find_map(|apk| apk.read(name).ok().map(|(data, _)| data))
    }

    /// ALL usable config variants of the resource `id`, ranked best-first
    /// (T-A): `anydpi` adaptive-icon XMLs first, then every other value by
    /// DESCENDING density with the default config / unknown-density configs
    /// last (their value may be a low-density entry). Only variants whose
    /// file is actually readable from one of the sources are returned, so
    /// this works across base + split APKs.
    pub fn get_resource_variants(&self, id: u32) -> Vec<String> {
        self.variants_for_ids(std::slice::from_ref(&id))
    }

    /// Like [`MultiSource::get_resource_variants`], starting from a
    /// `type/entry` resource name (e.g. `mipmap/ic_launcher`).
    pub fn get_resource_variants_by_name(&self, name: &str) -> Vec<String> {
        self.variants_for_ids(&self.ids_matching_full_name(name))
    }

    /// Collect, rank and dedupe the variants of the given resource ids.
    fn variants_for_ids(&self, ids: &[u32]) -> Vec<String> {
        let mut candidates: Vec<(u8, u64, usize, String)> = Vec::new();
        for id in ids {
            self.collect_variants(*id, 0, &mut candidates);
        }
        candidates.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
        let mut seen = std::collections::HashSet::new();
        let mut variants = Vec::new();
        for (_, _, _, value) in candidates {
            if seen.insert(value.clone()) {
                variants.push(value);
            }
        }
        variants
    }

    /// Gather every config's value of `id` across all sources into `out` as
    /// `(rank group, config rank, source index, value)` tuples, following
    /// reference entries to their target resource. Only usable values (file
    /// present in a source zip, or non-file values) are collected.
    fn collect_variants(&self, id: u32, depth: usize, out: &mut Vec<(u8, u64, usize, String)>) {
        if depth > MAX_REF_DEPTH {
            return;
        }
        let pkg_id = id >> 24;
        let type_id = ((id >> 16) & 0xff) as u8;
        let entry_id = (id & 0xffff) as u16;
        for (sidx, arsc) in self.arscs.iter().enumerate() {
            let Some(arsc) = arsc else { continue };
            for pkg in arsc.packages() {
                if pkg.header.id != pkg_id {
                    continue;
                }
                for (cfg, type_map) in pkg.resources.iter() {
                    let Some(entries) = type_map.get(&type_id) else { continue };
                    let Some(entry) = entries.find(entry_id) else { continue };
                    let value = match decode_entry(entry) {
                        Decoded::Ref(ref_id) => {
                            if ref_id != id {
                                self.collect_variants(ref_id, depth + 1, out);
                            }
                            continue;
                        }
                        Decoded::Borrowed(v) => arsc.value_to_string(v),
                        Decoded::Owned { data_type, data } => {
                            arsc.value_to_string(&apk_info::structs::ResourceValue {
                                size: 8,
                                res: 0,
                                data_type,
                                data,
                            })
                        }
                        Decoded::None => continue,
                    };
                    if self.usable(&value) {
                        out.push((variant_group(&value), variant_cfg_rank(cfg), sidx, value));
                    }
                }
            }
        }
    }

    /// Collect the resource ids whose entry full name (`type/key`) matches.
    fn ids_matching_full_name(&self, name: &str) -> Vec<u32> {
        let mut ids: Vec<u32> = Vec::new();
        for arsc in &self.arscs {
            let Some(arsc) = arsc else { continue };
            for pkg in arsc.packages() {
                for type_map in pkg.resources.values() {
                    for (type_id, entries) in type_map {
                        for (pos, entry) in entries.entries.iter().enumerate() {
                            let entry_id = entries
                                .entry_ids
                                .as_ref()
                                .map_or(pos as u16, |ids| ids[pos]);
                            let Some(full) = pkg.get_entry_full_name(entry, *type_id) else {
                                continue;
                            };
                            if full != name {
                                continue;
                            }
                            let id = (pkg.header.id << 24)
                                | (u32::from(*type_id) << 16)
                                | u32::from(entry_id);
                            if !ids.contains(&id) {
                                ids.push(id);
                            }
                        }
                    }
                }
            }
        }
        ids
    }

    /// Resolve a resource id by scanning ALL sources' packages/entries with
    /// config-aware best-density selection, preferring candidates whose file
    /// values can actually be read from one of the sources' zips.
    fn resolve_id(&self, id: u32, depth: usize) -> Option<String> {
        if depth > MAX_REF_DEPTH {
            return None;
        }
        let pkg_id = id >> 24;
        let type_id = ((id >> 16) & 0xff) as u8;
        let entry_id = (id & 0xffff) as u16;

        // Candidates: (source index, config rank, value). Lower rank wins;
        // sort is stable, so equal-rank configs keep deterministic BTreeMap
        // order within a source and source order breaks ties across sources.
        let mut candidates: Vec<(usize, u64, String)> = Vec::new();
        for (sidx, arsc) in self.arscs.iter().enumerate() {
            let Some(arsc) = arsc else { continue };
            for pkg in arsc.packages() {
                if pkg.header.id != pkg_id {
                    continue;
                }
                let mut configs: Vec<&ResTableConfig> = pkg.resources.keys().collect();
                configs.sort_by_key(|cfg| config_rank(cfg));
                for cfg in configs {
                    let Some(type_map) = pkg.resources.get(cfg) else { continue };
                    let Some(entries) = type_map.get(&type_id) else { continue };
                    let Some(entry) = entries.find(entry_id) else { continue };
                    let value = match decode_entry(entry) {
                        Decoded::Ref(ref_id) => {
                            if ref_id == id {
                                continue;
                            }
                            match self.resolve_id(ref_id, depth + 1) {
                                Some(v) => v,
                                None => continue,
                            }
                        }
                        Decoded::Borrowed(v) => arsc.value_to_string(v),
                        Decoded::Owned { data_type, data } => {
                            arsc.value_to_string(&apk_info::structs::ResourceValue {
                                size: 8,
                                res: 0,
                                data_type,
                                data,
                            })
                        }
                        Decoded::None => continue,
                    };
                    candidates.push((sidx, config_rank(cfg), value));
                }
            }
        }

        candidates.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        candidates
            .into_iter()
            .find(|(_, _, value)| self.usable(value))
            .map(|(_, _, value)| value)
    }

    /// A candidate is usable when it is not a file path, or its file exists
    /// in one of the sources' zips. This is what lets the resolver skip
    /// base-ARSC density entries whose files were moved into a split.
    fn usable(&self, value: &str) -> bool {
        !value.starts_with("res/") || self.read_entry(value).is_some()
    }
}

impl ResourceResolver for MultiSource {
    fn get_resource_value(&self, id: u32) -> Option<String> {
        self.resolve_id(id, 0)
    }

    fn get_resource_value_by_name(&self, name: &str) -> Option<String> {
        // Collect candidate ids whose entry full name matches, then resolve
        // config-aware. `get_entry_full_name` yields `type/key` strings, the
        // same format the icon pipeline passes in.
        self.ids_matching_full_name(name)
            .into_iter()
            .find_map(|id| self.resolve_id(id, 0))
    }

    fn axml_arsc(&self) -> Option<&ARSC> {
        self.arscs.first().and_then(|a| a.as_ref())
    }
}

/// A decoded entry value: a reference to another resource, a borrowed parsed
/// value (from a `Default` entry), an owned type+payload pair (from a
/// `Compact` entry, synthesized from `flags >> 8` / `data`), or nothing.
#[derive(Debug)]
enum Decoded<'a> {
    Ref(u32),
    Borrowed(&'a apk_info::structs::ResourceValue),
    Owned { data_type: ResourceValueType, data: u32 },
    None,
}

/// Decode a table entry, handling BOTH storage forms: `Default` (full entry
/// with a parsed `ResourceValue`) and `Compact` (AOSP compact layout: the
/// value TYPE is the high byte of `flags`, `data` is the payload) — the
/// crate's default-config path never decodes Compact, which is exactly what
/// density-only entries use.
fn decode_entry(entry: &ResTableEntry) -> Decoded<'_> {
    match entry {
        ResTableEntry::Default(e) => match e.value.data_type {
            ResourceValueType::Reference | ResourceValueType::DynamicReference => {
                Decoded::Ref(e.value.data)
            }
            _ => Decoded::Borrowed(&e.value),
        },
        ResTableEntry::Compact(e) => match ResourceValueType::from((e.flags >> 8) as u8) {
            ResourceValueType::Reference | ResourceValueType::DynamicReference => {
                Decoded::Ref(e.data)
            }
            data_type => Decoded::Owned { data_type, data: e.data },
        },
        _ => Decoded::None,
    }
}

/// Config ranking (lower wins): the default config first, then non-default
/// configs by DESCENDING density; density 0 (anydpi-style or unknown) is the
/// worst non-default option. Ties keep deterministic BTreeMap order.
fn config_rank(cfg: &ResTableConfig) -> u64 {
    if *cfg == ResTableConfig::default() {
        return 0;
    }
    let (_orientation, _touchscreen, density) = cfg.get_orientation_touchscreen_density();
    if density == 0 {
        (1 << 32) + u64::from(u16::MAX)
    } else {
        (1 << 32) + u64::from(u16::MAX - density)
    }
}

/// Variant rank group (lower wins, T-A): an `anydpi` adaptive-icon XML (the
/// launcher's own pick) outranks every other value.
fn variant_group(value: &str) -> u8 {
    let dir = value
        .strip_prefix("res/")
        .and_then(|rest| rest.split_once('/'))
        .map(|(dir, _)| dir);
    if value.ends_with(".xml") && dir.is_some_and(|d| d.split('-').any(|q| q == "anydpi")) {
        0
    } else {
        1
    }
}

/// Variant rank within a group (lower wins, T-A): higher density first.
/// Unlike [`config_rank`], the DEFAULT config ranks last among rasters —
/// its value may be a low-density entry (APKPure's default-config value is
/// the mdpi icon while an xxxhdpi exists), and the manifest-anchored step
/// wants the highest-density variant first. Unknown-density non-default
/// configs tie with the default config at the end.
fn variant_cfg_rank(cfg: &ResTableConfig) -> u64 {
    if *cfg == ResTableConfig::default() {
        return u64::from(u16::MAX);
    }
    // Reuse config_rank's density component (u16::MAX - density; density 0
    // maps to u16::MAX) without its default-config special case.
    config_rank(cfg) - (1 << 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_rank_default_first_then_higher_density_then_zero() {
        // Arrange
        let default = ResTableConfig::default();
        let mut d120 = ResTableConfig::default();
        d120.screen_type = 120 << 16;
        let mut d640 = ResTableConfig::default();
        d640.screen_type = 640 << 16;
        let mut d0 = ResTableConfig::default();
        d0.screen_type = 0x0000_0001; // non-default (orientation set), density 0

        // Act / Assert
        assert_eq!(config_rank(&default), 0, "default config must win");
        assert!(config_rank(&d640) < config_rank(&d120), "higher density wins");
        assert!(config_rank(&d120) < config_rank(&d0), "density-0 non-default is worst");
    }

    #[test]
    fn test_decode_entry_compact_reference_uses_flags_high_byte() {
        // Arrange: AOSP compact entry, type in flags >> 8 (0x01 = Reference).
        let entry = ResTableEntry::Compact(apk_info::structs::ResTableEntryCompact {
            key: 0,
            flags: 0x0108, // FLAG_COMPACT (0x08) | Reference (0x01) << 8
            data: 0x7f0d0002,
        });

        // Act
        let decoded = decode_entry(&entry);

        // Assert: the value payload must surface as a reference id.
        assert!(matches!(decoded, Decoded::Ref(0x7f0d0002)), "got {decoded:?}");
    }

    #[test]
    fn test_decode_entry_compact_string_keeps_payload() {
        // Arrange: compact String entry (type 0x03).
        let entry = ResTableEntry::Compact(apk_info::structs::ResTableEntryCompact {
            key: 0,
            flags: 0x0308,
            data: 42,
        });

        // Act
        let decoded = decode_entry(&entry);

        // Assert: payload preserved for the caller's string-pool lookup.
        assert!(
            matches!(decoded, Decoded::Owned { data_type: ResourceValueType::String, data: 42 }),
            "got {decoded:?}"
        );
    }

    #[test]
    fn test_null_resolver_never_resolves() {
        // Arrange/Act/Assert: the degraded resolver mirrors the old `None` ARSC.
        assert_eq!(NullResolver.get_resource_value(0x7f0d0002), None);
        assert_eq!(NullResolver.get_resource_value_by_name("mipmap/ic_launcher"), None);
        assert!(NullResolver.axml_arsc().is_none());
    }

    #[test]
    fn test_variant_group_anydpi_xml_first() {
        // Arrange / Act / Assert: only anydpi adaptive XMLs rank group 0.
        assert_eq!(variant_group("res/mipmap-anydpi-v26/ic_launcher.xml"), 0);
        assert_eq!(variant_group("res/mipmap-anydpi/ic_launcher.xml"), 0);
        assert_eq!(variant_group("res/mipmap-xxxhdpi-v4/ic_launcher.png"), 1);
        assert_eq!(variant_group("res/mipmap-mdpi/ic_launcher.png"), 1);
        assert_eq!(variant_group("res/drawable-xhdpi/icon.xml"), 1, "density xml is not anydpi");
        assert_eq!(variant_group("res/mipmap-anydpi/ic_launcher.webp"), 1, "raster is not xml");
    }

    #[test]
    fn test_variant_cfg_rank_density_desc_default_last() {
        // Arrange: default config, density buckets, unknown density.
        let default = ResTableConfig::default();
        let mut d120 = ResTableConfig::default();
        d120.screen_type = 120 << 16;
        let mut d480 = ResTableConfig::default();
        d480.screen_type = 480 << 16;
        let mut d0 = ResTableConfig::default();
        d0.screen_type = 0x0000_0001; // non-default (orientation set), density 0

        // Act / Assert: higher density first; default and unknown density last.
        assert!(variant_cfg_rank(&d480) < variant_cfg_rank(&d120), "higher density wins");
        assert_eq!(variant_cfg_rank(&d120), u64::from(u16::MAX) - 120);
        assert_eq!(
            variant_cfg_rank(&default),
            u64::from(u16::MAX),
            "default config ranks last (may be a low-density entry)"
        );
        assert_eq!(variant_cfg_rank(&d0), u64::from(u16::MAX), "unknown density ties last");
    }

    /// T-A spike: the ranked variants of a real manifest icon resource must
    /// put the anydpi adaptive XML first and the density rasters in
    /// descending-density order, without duplicates. Set `DV_T1_RANK_APK`
    /// (e.g. the Telegram fixture) and `DV_T1_RANK_RES` (e.g.
    /// `mipmap/ic_launcher`).
    #[test]
    #[ignore = "requires a fixture; set DV_T1_RANK_APK + DV_T1_RANK_RES to run"]
    fn variants_rank_anydpi_first_then_density_descending() {
        let path = std::env::var("DV_T1_RANK_APK").expect("DV_T1_RANK_APK must be set");
        let res = std::env::var("DV_T1_RANK_RES").expect("DV_T1_RANK_RES must be set");
        let apk = Apk::new(std::path::Path::new(&path)).expect("fixture parses");
        let multi = MultiSource::new(apk, Vec::new());

        let variants = multi.get_resource_variants_by_name(&res);
        assert!(!variants.is_empty(), "must resolve at least one variant");
        println!("SPIKE: variants of {res}: {variants:?}");

        // Dedupe: every variant appears exactly once.
        let mut sorted = variants.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), variants.len(), "variants must be deduped");

        // All anydpi XMLs before all other variants.
        let last_anydpi = variants
            .iter()
            .rposition(|v| v.ends_with(".xml") && v.contains("anydpi"));
        let first_other = variants
            .iter()
            .position(|v| !(v.ends_with(".xml") && v.contains("anydpi")));
        if let (Some(l), Some(o)) = (last_anydpi, first_other) {
            assert!(l < o, "anydpi xmls must precede all other variants: {variants:?}");
        }
        // Higher-density rasters must precede lower-density ones (the
        // default-config value — whatever path it points at — sorts last).
        fn dir_of(v: &str) -> &str {
            v.strip_prefix("res/")
                .and_then(|rest| rest.split_once('/'))
                .map(|(dir, _)| dir)
                .unwrap_or("")
        }
        let has_q = |v: &str, q: &str| dir_of(v).split('-').any(|seg| seg == q);
        let pos = |q: &str| {
            variants.iter().position(|v| has_q(v, q)).expect("fixture entry missing")
        };
        let xxx = pos("xxxhdpi");
        for lower in ["xhdpi", "hdpi", "mdpi", "ldpi"] {
            if let Some(p) = variants.iter().position(|v| has_q(v, lower)) {
                assert!(
                    xxx < p,
                    "xxxhdpi must precede {lower} (bug C3): {variants:?}"
                );
            }
        }
    }
}
