//! Adaptive/vector icon rendering (T-VEC-1).
//!
//! Modern apps often ship ONLY an adaptive icon XML (`res/mipmap-anydpi-v21/
//! ic_launcher.xml`) with no raster launcher PNG in the APK. This module
//! decodes that binary AXML, resolves the foreground/background references
//! through `resources.arsc`, converts the supported Android VectorDrawable
//! subset to SVG and rasterizes it to a 192x192 PNG with resvg/tiny-skia.
//!
//! Supported subset:
//! - `<adaptive-icon>` with `<background>`/`<foreground android:drawable=...>`
//! - `<vector>` with `<path android:pathData android:fillColor android:fillAlpha>`
//!   and nested `<group android:translateX/Y android:scaleX/Y android:rotation
//!   android:pivotX/Y android:clip-path via <clip-path>>`
//! - `fillColor` as `#RGB/#ARGB/#RRGGBB/#AARRGGBB`, `@color/...`/`@drawable/$ref`
//!   resolved via resources.arsc (aapt inline gradient sub-documents included),
//!   or `?<attr id in hex>` resolved through the ARSC when possible
//! - gradient fills (`<gradient>` linear/radial with `<item>` stops or
//!   start/center/endColor) — sweep gradients degrade
//! - `<shape android:shape="rectangle|oval">` with `<solid>`/`<corners>`
//! - `<bitmap android:src="...">` referencing a raster resource
//!
//! Adaptive-icon layers are composited on the 108x108dp canvas but only the
//! launcher-visible center 72dp safe zone is rasterized (center crop).
//!
//! Anything unsupported (sweep gradients, animated drawables, unresolvable
//! references) degrades to `None` — the caller falls back to the letter icon.
//! Rendering NEVER fails the APK parse.

use apk_info::{ARSC, AXML};
use apk_info_xml::Element;

use crate::arsc_resolve::{NullResolver, ResourceResolver};

/// Adaptive icon layers live on a 108x108dp canvas (72dp safe zone inside).
pub const ADAPTIVE_VIEWPORT: f32 = 108.0;

/// Launcher-visible center safe zone of the adaptive canvas; only this
/// region is rasterized (launchers crop the 108dp layers to 72dp).
pub const ADAPTIVE_SAFE_ZONE: f32 = 72.0;

/// Output raster size in pixels.
pub const OUTPUT_SIZE: u32 = 192;

/// Max recursion depth when an adaptive layer points at another XML drawable.
const MAX_XML_DEPTH: usize = 2;

/// One drawable node of a parsed vector drawable tree.
#[derive(Debug, Clone, PartialEq)]
pub enum VNode {
    Group {
        translate: (f32, f32),
        rotation: f32,
        pivot: (f32, f32),
        scale: (f32, f32),
        /// `android:pathData` of a `<clip-path>` inside this group, clipping
        /// its content (first one wins).
        clip: Option<String>,
        children: Vec<VNode>,
    },
    Path {
        path_data: String,
        /// Solid color or gradient fill.
        fill: VFill,
        fill_alpha: f32,
    },
}

/// Fill of a vector path.
#[derive(Debug, Clone, PartialEq)]
pub enum VFill {
    /// Solid RGBA color.
    Color([u8; 4]),
    /// Multi-stop gradient in viewport space.
    Gradient(VGradient),
}

/// One gradient color stop: RGBA + offset in [0, 1].
pub type VStop = ([u8; 4], f32);

/// A decoded gradient fill (Android `<gradient>`).
#[derive(Debug, Clone, PartialEq)]
pub struct VGradient {
    pub stops: Vec<VStop>,
    pub kind: VGradientKind,
}

/// Gradient geometry in the vector's viewport coordinate space.
#[derive(Debug, Clone, PartialEq)]
pub enum VGradientKind {
    Linear { x1: f32, y1: f32, x2: f32, y2: f32 },
    Radial { cx: f32, cy: f32, r: f32 },
}

/// A decoded `<vector>` drawable.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorDrawable {
    pub viewport: (f32, f32),
    pub nodes: Vec<VNode>,
}

/// A resolved adaptive-icon layer.
#[derive(Debug, Clone, PartialEq)]
pub enum Layer {
    /// Solid color, RGBA.
    Color([u8; 4]),
    /// Raster image bytes (PNG/WebP/...); embedded as data URI.
    Raster(Vec<u8>),
    /// Vector drawable.
    Vector(VectorDrawable),
}

/// Background + foreground layers of an adaptive icon.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AdaptiveIcon {
    pub background: Option<Layer>,
    pub foreground: Option<Layer>,
}

/// Parse an Android color literal (`#RGB`, `#ARGB`, `#RRGGBB`, `#AARRGGBB`)
/// into RGBA bytes.
pub fn parse_color(s: &str) -> Option<[u8; 4]> {
    let hex = s.strip_prefix('#')?;
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let doubled = |c: u8| -> u8 {
        let v = (c as char).to_digit(16).expect("checked hexdigit") as u8;
        v * 16 + v
    };
    let pair = |i: usize| -> u8 {
        u8::from_str_radix(&hex[i..i + 2], 16).expect("checked hexdigits")
    };
    match hex.len() {
        3 => {
            let b = hex.as_bytes();
            Some([doubled(b[0]), doubled(b[1]), doubled(b[2]), 255])
        }
        4 => {
            let b = hex.as_bytes();
            Some([doubled(b[1]), doubled(b[2]), doubled(b[3]), doubled(b[0])])
        }
        6 => Some([pair(0), pair(2), pair(4), 255]),
        8 => Some([pair(2), pair(4), pair(6), pair(0)]),
        _ => None,
    }
}

/// Decode a `<vector>` AXML element tree into a [`VectorDrawable`].
/// `res` resolves `@color/...` fill references; `read_entry`
/// (zip entry name -> bytes) loads aapt inline gradient sub-documents.
/// Returns `None` for any unsupported construct (theme refs that cannot be
/// resolved, unresolvable refs, missing viewport, paths without
/// pathData/fillColor).
pub fn decode_vector(
    root: &Element,
    res: &dyn ResourceResolver,
    read_entry: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> Option<VectorDrawable> {
    if root.name() != "vector" {
        return None;
    }
    let vw = root.attr("viewportWidth")?.parse::<f32>().ok()?;
    let vh = root.attr("viewportHeight")?.parse::<f32>().ok()?;
    if !(vw > 0.0 && vh > 0.0) {
        return None;
    }
    let mut top_clip = None;
    let nodes = decode_nodes(root, res, read_entry, (vw, vh), &mut top_clip)?;
    if nodes.is_empty() {
        return None;
    }
    Some(VectorDrawable { viewport: (vw, vh), nodes })
}

fn decode_nodes(
    el: &Element,
    res: &dyn ResourceResolver,
    read_entry: &dyn Fn(&str) -> Option<Vec<u8>>,
    viewport: (f32, f32),
    clip: &mut Option<String>,
) -> Option<Vec<VNode>> {
    let mut nodes = Vec::new();
    for child in el.childrens() {
        match child.name() {
            "path" => {
                let path_data = child.attr("pathData")?.to_string();
                if path_data.is_empty() {
                    continue;
                }
                let fill = decode_path_fill(child, res, read_entry, viewport)?;
                let fill_alpha = child
                    .attr("fillAlpha")
                    .and_then(|v| v.parse::<f32>().ok())
                    .unwrap_or(1.0);
                nodes.push(VNode::Path { path_data, fill, fill_alpha });
            }
            "group" => {
                let num = |name: &str, default: f32| {
                    child.attr(name).and_then(|v| v.parse::<f32>().ok()).unwrap_or(default)
                };
                let mut group_clip = None;
                let children =
                    decode_nodes(child, res, read_entry, viewport, &mut group_clip)?;
                nodes.push(VNode::Group {
                    translate: (num("translateX", 0.0), num("translateY", 0.0)),
                    rotation: num("rotation", 0.0),
                    pivot: (num("pivotX", 0.0), num("pivotY", 0.0)),
                    scale: (num("scaleX", 1.0), num("scaleY", 1.0)),
                    clip: group_clip,
                    children,
                });
            }
            "clip-path" => {
                // Clips the containing group's content; first one wins.
                if clip.is_none() {
                    let d = child.attr("pathData")?.to_string();
                    if !d.is_empty() {
                        *clip = Some(d);
                    }
                }
            }
            // Unknown children (animated, aapt:attr without a gradient, ...)
            // are ignored; a vector made only of them yields no nodes → None
            // upstream.
            _ => {}
        }
    }
    Some(nodes)
}

/// Decode a path's fill: an inline `<aapt:attr name="android:fillColor">`
/// gradient child wins, else the `android:fillColor` attribute.
fn decode_path_fill(
    el: &Element,
    res: &dyn ResourceResolver,
    read_entry: &dyn Fn(&str) -> Option<Vec<u8>>,
    viewport: (f32, f32),
) -> Option<VFill> {
    for child in el.childrens() {
        if child.name() == "attr" && child.attr("name") == Some("android:fillColor") {
            return match child.childrens().find(|c| c.name() == "gradient") {
                Some(g) => decode_gradient(g, res, viewport).map(VFill::Gradient),
                // Inline aapt:attr without a supported gradient child.
                None => None,
            };
        }
    }
    decode_color_ref(el.attr("fillColor")?, res, read_entry, viewport)
}

/// Resolve a fill-color value: literal hex, `@color|drawable` reference
/// (which may point at an aapt inline gradient sub-document), or a theme
/// attribute reference `?<hex resource id>` resolved via the resolver.
fn decode_color_ref(
    raw: &str,
    res: &dyn ResourceResolver,
    read_entry: &dyn Fn(&str) -> Option<Vec<u8>>,
    viewport: (f32, f32),
) -> Option<VFill> {
    if raw.starts_with('#') {
        return parse_color(raw).map(VFill::Color);
    }
    if raw.starts_with('@') {
        let resolved = resolve_ref_token(raw, res)?;
        return resolved_value_as_fill(&resolved, res, read_entry, viewport);
    }
    if let Some(id) = parse_theme_attr_id(raw) {
        // Theme attrs have no ambient theme here; try the attr resource's
        // value from the resolver, else degrade.
        let resolved = res.get_resource_value(id)?;
        return parse_color(&resolved).map(VFill::Color);
    }
    None
}

/// Interpret a resolved resource value as a fill: a color literal, or an XML
/// file (zip entry) holding a `<gradient>` sub-document.
fn resolved_value_as_fill(
    value: &str,
    res: &dyn ResourceResolver,
    read_entry: &dyn Fn(&str) -> Option<Vec<u8>>,
    viewport: (f32, f32),
) -> Option<VFill> {
    if value.starts_with('#') {
        return parse_color(value).map(VFill::Color);
    }
    if value.starts_with("res/") && value.ends_with(".xml") {
        let bytes = read_entry(value)?;
        let axml = AXML::new(&mut &bytes[..], res.axml_arsc()).ok()?;
        if axml.root.name() == "gradient" {
            return decode_gradient(&axml.root, res, viewport).map(VFill::Gradient);
        }
    }
    None
}

/// Resolve a raw AXML reference token (`@type/name`, `@<hex resource id>`,
/// `@0x<id>`, `@ref/0x<id>` forms emitted by the AXML decoder) through the
/// resolver. Plain `res/...` paths are NOT tokens — they pass through the
/// caller untouched.
pub fn resolve_ref_token(raw: &str, res: &dyn ResourceResolver) -> Option<String> {
    let rest = raw.strip_prefix('@')?;
    let rest = rest.rsplit(':').next().unwrap_or(rest);
    // Numeric id forms: `@7f0d0002` (AXML string-render fallback),
    // `@0x7f0d0002` or `@ref/0x7f0d0002` (defensive; not emitted today).
    let rest = rest.strip_prefix("ref/").unwrap_or(rest);
    let hex = rest.strip_prefix("0x").unwrap_or(rest);
    if !rest.contains('/') {
        if let Ok(id) = u32::from_str_radix(hex, 16) {
            return res.get_resource_value(id);
        }
    }
    // Named form: "type/entry" (e.g. `@mipmap/ic_launcher`; a package
    // prefix like `@android:color/white` is stripped above).
    res.get_resource_value_by_name(rest)
}

/// Parse a theme attribute reference: AXML emits `?attr` refs as `?<hex
/// resource id>` (e.g. `?7f040123`); named forms cannot be resolved here.
pub fn parse_theme_attr_id(raw: &str) -> Option<u32> {
    u32::from_str_radix(raw.strip_prefix('?')?, 16).ok()
}

/// Decode an Android `<gradient>` element into a [`VGradient`].
/// `viewport` supplies defaults/geometry for angle-based linear and
/// fraction-defaulted radial gradients.
fn decode_gradient(root: &Element, res: &dyn ResourceResolver, viewport: (f32, f32)) -> Option<VGradient> {
    let stops = gradient_stops(root, res)?;
    let num = |name: &str| root.attr(name).and_then(|v| v.parse::<f32>().ok());
    let kind = match root.attr("type").unwrap_or("linear") {
        "linear" => match (num("startX"), num("startY"), num("endX"), num("endY")) {
            (Some(x1), Some(y1), Some(x2), Some(y2)) => VGradientKind::Linear { x1, y1, x2, y2 },
            // Angle-based: 0 = left-to-right, counterclockwise, spanning the
            // viewport through its center.
            _ => {
                let angle = num("angle").unwrap_or(0.0).to_radians();
                let (w, h) = viewport;
                let (cx, cy, r) = (w / 2.0, h / 2.0, w.max(h) / 2.0);
                let (dx, dy) = (angle.cos(), -angle.sin());
                VGradientKind::Linear {
                    x1: cx - dx * r,
                    y1: cy - dy * r,
                    x2: cx + dx * r,
                    y2: cy + dy * r,
                }
            }
        },
        "radial" => {
            let r = num("gradientRadius")?;
            let cx = num("centerX").unwrap_or(viewport.0 / 2.0);
            let cy = num("centerY").unwrap_or(viewport.1 / 2.0);
            VGradientKind::Radial { cx, cy, r }
        }
        // Sweep and unknown types are unsupported.
        _ => return None,
    };
    if stops.is_empty() {
        return None;
    }
    Some(VGradient { stops, kind })
}

/// Gradient color stops: `<item color offset>` children when present, else
/// the required start/endColor (plus optional centerColor at 0.5).
fn gradient_stops(root: &Element, res: &dyn ResourceResolver) -> Option<Vec<VStop>> {
    let mut stops = Vec::new();
    for item in root.childrens() {
        if item.name() != "item" {
            continue;
        }
        let color = gradient_color(item.attr("color")?, res)?;
        let offset: f32 = item.attr("offset")?.parse().ok()?;
        stops.push((color, offset));
    }
    if !stops.is_empty() {
        return Some(stops);
    }
    let start = gradient_color(root.attr("startColor")?, res)?;
    let end = gradient_color(root.attr("endColor")?, res)?;
    let center = root.attr("centerColor").and_then(|c| gradient_color(c, res));
    Some(match center {
        Some(c) => vec![(start, 0.0), (c, 0.5), (end, 1.0)],
        None => vec![(start, 0.0), (end, 1.0)],
    })
}

/// Resolve a gradient color value: literal hex or `@color/...` reference.
fn gradient_color(raw: &str, res: &dyn ResourceResolver) -> Option<[u8; 4]> {
    if raw.starts_with('#') {
        return parse_color(raw);
    }
    let resolved = resolve_ref_token(raw, res)?;
    parse_color(&resolved)
}

/// Decode a `<shape>` drawable (rectangle/oval with solid fill and optional
/// corner radius) into a full-canvas [`VectorDrawable`]. Other shapes
/// (ring/line), gradient fills, or a missing `<solid>` degrade to `None`.
pub fn decode_shape(root: &Element, res: &dyn ResourceResolver) -> Option<VectorDrawable> {
    if root.name() != "shape" {
        return None;
    }
    let kind = root.attr("shape").unwrap_or("rectangle");
    if kind != "rectangle" && kind != "oval" {
        return None;
    }
    // Gradient-filled shapes are an unsupported construct.
    if root.childrens().any(|c| c.name() == "gradient") {
        return None;
    }
    let solid_color = root.childrens().find(|c| c.name() == "solid")?.attr("color")?;
    let fill = VFill::Color(gradient_color(solid_color, res)?);
    let radius: f32 = root
        .childrens()
        .find(|c| c.name() == "corners")
        .and_then(|c| c.attr("radius"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);

    let v = ADAPTIVE_VIEWPORT;
    let path_data = if kind == "oval" {
        let r = v / 2.0;
        format!("M0,{} A{r},{r} 0 1 1 {v},{} A{r},{r} 0 1 1 0,{} Z", fmt_num(r), fmt_num(r), fmt_num(r))
    } else if radius > 0.0 {
        let r = radius.min(v / 2.0);
        let (rr, xr, yr) = (fmt_num(r), fmt_num(v - r), fmt_num(v));
        format!("M{rr},0 H{xr} A{rr},{rr} 0 0 1 {yr},{rr} V{xr} A{rr},{rr} 0 0 1 {xr},{yr} H{rr} A{rr},{rr} 0 0 1 0,{xr} V{rr} A{rr},{rr} 0 0 1 {rr},0 Z")
    } else {
        format!("M0,0 H{} V{} H0 Z", fmt_num(v), fmt_num(v))
    };
    Some(VectorDrawable {
        viewport: (v, v),
        nodes: vec![VNode::Path { path_data, fill, fill_alpha: 1.0 }],
    })
}

/// Decode a `<bitmap android:src="...">` drawable into a raster [`Layer`]
/// by resolving the src reference through the resolver.
pub fn decode_bitmap(
    root: &Element,
    res: &dyn ResourceResolver,
    read_entry: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> Option<Layer> {
    if root.name() != "bitmap" {
        return None;
    }
    let raw = root.attr("src")?;
    let value = resolve_ref_token(raw, res)?;
    value_to_layer(&value, read_entry, res, 0)
}

/// Extract the raw `android:drawable` reference strings of the
/// `<background>`/`<foreground>` children of an `<adaptive-icon>` root.
pub fn decode_adaptive_refs(root: &Element) -> (Option<String>, Option<String>) {
    let layer = |name: &str| {
        root.childrens()
            .find(|c| c.name() == name)
            .and_then(|c| c.attr("drawable"))
            .map(str::to_string)
    };
    (layer("background"), layer("foreground"))
}

/// Compose background + foreground into an SVG document sized `OUTPUT_SIZE`
/// with the full 108x108 viewport (used for direct vector launcher icons,
/// which already fill their canvas).
pub fn compose_svg(icon: &AdaptiveIcon) -> String {
    compose_svg_impl(icon, false)
}

/// Compose background + foreground into an SVG document sized `OUTPUT_SIZE`
/// cropped to the launcher-visible center 72dp safe zone of the 108dp
/// adaptive canvas (used for `<adaptive-icon>` compositing).
pub fn compose_adaptive_svg(icon: &AdaptiveIcon) -> String {
    compose_svg_impl(icon, true)
}

fn compose_svg_impl(icon: &AdaptiveIcon, crop: bool) -> String {
    let mut body = String::new();
    let mut defs = String::new();
    let mut next_id = 0u32;
    if let Some(bg) = &icon.background {
        render_layer(bg, &mut defs, &mut next_id, &mut body);
    }
    if let Some(fg) = &icon.foreground {
        render_layer(fg, &mut defs, &mut next_id, &mut body);
    }
    let viewbox = if crop {
        let off = fmt_num((ADAPTIVE_VIEWPORT - ADAPTIVE_SAFE_ZONE) / 2.0);
        format!("{off} {off} {} {}", fmt_num(ADAPTIVE_SAFE_ZONE), fmt_num(ADAPTIVE_SAFE_ZONE))
    } else {
        let v = fmt_num(ADAPTIVE_VIEWPORT);
        format!("0 0 {v} {v}")
    };
    let defs = if defs.is_empty() { String::new() } else { format!("<defs>{defs}</defs>") };
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" \
             xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
             width=\"{OUTPUT_SIZE}\" height=\"{OUTPUT_SIZE}\" \
             viewBox=\"{viewbox}\">{defs}{body}</svg>"
    )
}

fn render_layer(layer: &Layer, defs: &mut String, next_id: &mut u32, out: &mut String) {
    let v = fmt_num(ADAPTIVE_VIEWPORT);
    match layer {
        Layer::Color(c) => {
            out.push_str(&format!(
                "<rect x=\"0\" y=\"0\" width=\"{v}\" height=\"{v}\" fill=\"{}\"{}",
                color_css(c),
                opacity_attr("fill-opacity", c[3] as f32 / 255.0),
            ));
            out.push_str("/>");
        }
        Layer::Raster(bytes) => {
            // The data-URI MIME must match the actual byte format: usvg
            // silently drops the image on a mismatch, so WebP layers
            // embedded with the old hardcoded `image/png` vanished from
            // the render (background-only icons, T-H).
            let mime = if bytes.starts_with(b"RIFF") && bytes.len() > 11 && &bytes[8..12] == b"WEBP" {
                "image/webp"
            } else {
                "image/png"
            };
            out.push_str(&format!(
                "<image x=\"0\" y=\"0\" width=\"{v}\" height=\"{v}\" \
                     preserveAspectRatio=\"xMidYMid slice\" \
                     xlink:href=\"data:{mime};base64,{}\"/>",
                base64_encode(bytes),
            ));
        }
        Layer::Vector(vd) => {
            let sx = ADAPTIVE_VIEWPORT / vd.viewport.0;
            let sy = ADAPTIVE_VIEWPORT / vd.viewport.1;
            out.push_str(&format!(
                "<g transform=\"scale({},{})\">",
                fmt_num(sx),
                fmt_num(sy)
            ));
            render_nodes(&vd.nodes, defs, next_id, out);
            out.push_str("</g>");
        }
    }
}

fn render_nodes(nodes: &[VNode], defs: &mut String, next_id: &mut u32, out: &mut String) {
    for node in nodes {
        match node {
            VNode::Path { path_data, fill, fill_alpha } => {
                let (fill_attr, base_alpha) = match fill {
                    VFill::Color(c) => (format!("fill=\"{}\"", color_css(c)), c[3] as f32 / 255.0),
                    VFill::Gradient(g) => {
                        let gid = format!("g{next_id}");
                        *next_id += 1;
                        render_gradient_def(g, &gid, defs);
                        (format!("fill=\"url(#{gid})\""), 1.0)
                    }
                };
                let opacity = base_alpha * fill_alpha;
                out.push_str(&format!(
                    "<path d=\"{}\" {}",
                    escape_attr(path_data),
                    fill_attr,
                ));
                out.push_str(&opacity_attr("fill-opacity", opacity));
                out.push_str("/>");
            }
            VNode::Group { translate, rotation, pivot, scale, clip, children } => {
                // Android group matrix:
                // M = translate(t + p) · rotate(r) · scale(s) · translate(-p).
                out.push_str(&format!(
                    "<g transform=\"translate({},{}) rotate({}) scale({},{}) translate(-{},-{})\">",
                    fmt_num(translate.0 + pivot.0),
                    fmt_num(translate.1 + pivot.1),
                    fmt_num(*rotation),
                    fmt_num(scale.0),
                    fmt_num(scale.1),
                    fmt_num(pivot.0),
                    fmt_num(pivot.1),
                ));
                match clip {
                    Some(d) => {
                        // The clip path shares the group's coordinate system,
                        // so it wraps the children in an inner <g> living in
                        // the transformed space.
                        let cid = format!("c{next_id}");
                        *next_id += 1;
                        defs.push_str(&format!(
                            "<clipPath id=\"{cid}\"><path d=\"{}\"/></clipPath>",
                            escape_attr(d),
                        ));
                        out.push_str(&format!("<g clip-path=\"url(#{cid})\">"));
                        render_nodes(children, defs, next_id, out);
                        out.push_str("</g>");
                    }
                    None => render_nodes(children, defs, next_id, out),
                }
                out.push_str("</g>");
            }
        }
    }
}

/// Emit an SVG gradient definition (userSpaceOnUse) with alpha-carrying
/// stops.
fn render_gradient_def(g: &VGradient, id: &str, defs: &mut String) {
    match &g.kind {
        VGradientKind::Linear { x1, y1, x2, y2 } => defs.push_str(&format!(
            "<linearGradient id=\"{id}\" gradientUnits=\"userSpaceOnUse\" \
                 x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\">",
            fmt_num(*x1),
            fmt_num(*y1),
            fmt_num(*x2),
            fmt_num(*y2),
        )),
        VGradientKind::Radial { cx, cy, r } => defs.push_str(&format!(
            "<radialGradient id=\"{id}\" gradientUnits=\"userSpaceOnUse\" \
                 cx=\"{}\" cy=\"{}\" r=\"{}\">",
            fmt_num(*cx),
            fmt_num(*cy),
            fmt_num(*r),
        )),
    }
    for (color, offset) in &g.stops {
        let alpha = color[3] as f32 / 255.0;
        defs.push_str(&format!(
            "<stop offset=\"{}\" stop-color=\"{}\"{}",
            fmt_num(*offset),
            color_css(color),
            opacity_attr("stop-opacity", alpha),
        ));
        defs.push_str("/>");
    }
    defs.push_str(match &g.kind {
        VGradientKind::Linear { .. } => "</linearGradient>",
        VGradientKind::Radial { .. } => "</radialGradient>",
    });
}

fn color_css(c: &[u8; 4]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn opacity_attr(name: &str, opacity: f32) -> String {
    if (0.0..1.0).contains(&opacity) {
        format!(" {name}=\"{}\"", fmt_num(opacity.clamp(0.0, 1.0)))
    } else {
        String::new()
    }
}

/// Minimal float formatting: `Display` already trims trailing zeros.
fn fmt_num(v: f32) -> String {
    format!("{v}")
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('"', "&quot;")
}

/// Minimal standard-alphabet base64 encoder (raster layers are embedded as
/// data URIs; no base64 crate is pulled in for this single use).
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

/// Render an SVG string to PNG bytes of `size` x `size`.
pub fn render_svg(svg: &str, size: u32) -> Option<Vec<u8>> {
    let tree = resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    resvg::render(&tree, tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    pixmap.encode_png().ok()
}

/// Minimum alpha for a pixel to count as painted content when locating the
/// opaque bounding box of a raster icon.
const NORMALIZE_ALPHA_THRESHOLD: u8 = 8;

/// A raster whose opaque bounding box already covers at least this fraction
/// of both canvas dimensions is considered full-bleed and passed through
/// unchanged.
const NORMALIZE_FULLBLEED_RATIO: f32 = 0.95;

/// Normalize a plain raster launcher icon for uniform visual size (T-E).
///
/// Legacy rasters often carry baked-in padding, making their content render
/// visibly smaller than full-bleed icons. This decodes the raster (PNG),
/// finds the content bounding box, and — unless the box already (nearly)
/// fills the canvas — re-renders the content cropped to that box into a full
/// 192x192 PNG (aspect-preserving, centered) by embedding the original bytes
/// in an SVG `<image>` with the box as the viewBox.
///
/// Two box strategies (R2-F4): the alpha bounding box (transparent padding),
/// and — when the canvas is a fully-opaque uniform background (white margins
/// are baked in, OneDrive/Cốc Cốc) — a background-keyed box of pixels that
/// differ from the corner color.
///
/// Returns the input unchanged for already-full-bleed content, `None` when
/// the bytes cannot be decoded as PNG (e.g. WebP — see `normalize_raster_any`).
/// Never panics.
pub fn normalize_raster_png(png_bytes: &[u8]) -> Option<Vec<u8>> {
    let pixmap = tiny_skia::Pixmap::decode_png(png_bytes).ok()?;
    let (w, h) = (pixmap.width() as usize, pixmap.height() as usize);
    if w == 0 || h == 0 {
        return None;
    }

    // Opaque-content bounding box (inclusive, in pixel coordinates).
    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    for (i, px) in pixmap.data().chunks_exact(4).enumerate() {
        if px[3] > NORMALIZE_ALPHA_THRESHOLD {
            let (x, y) = (i % w, i / w);
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    if max_x < min_x {
        // Fully transparent: nothing to crop or scale; keep as-is.
        return Some(png_bytes.to_vec());
    }
    let (bw, bh) = (max_x - min_x + 1, max_y - min_y + 1);
    if (bw as f32 / w as f32) < NORMALIZE_FULLBLEED_RATIO
        || (bh as f32 / h as f32) < NORMALIZE_FULLBLEED_RATIO
    {
        return render_pixmap_cropped(&pixmap, min_x, min_y, bw, bh);
    }

    // Alpha bbox covers the canvas: the padding (if any) is opaque. Key on
    // the background color and crop to the content that differs from it.
    if let Some((bx, by, bbw, bbh)) = background_keyed_bbox(&pixmap) {
        if (bbw as f32 / w as f32) < NORMALIZE_BG_FULLBLEED_RATIO
            || (bbh as f32 / h as f32) < NORMALIZE_BG_FULLBLEED_RATIO
        {
            return render_pixmap_cropped(&pixmap, bx, by, bbw, bbh);
        }
    }

    // Full-bleed content (or heterogeneous canvas): pass through unchanged.
    Some(png_bytes.to_vec())
}

/// A raster whose background-keyed content box must cover at least this
/// fraction of both canvas dimensions to count as full-bleed.
const NORMALIZE_BG_FULLBLEED_RATIO: f32 = 0.92;

/// Maximum per-channel deviation between the four corner pixels for the
/// canvas to have a uniform keyable background.
const NORMALIZE_BG_TOLERANCE: i16 = 12;

/// Bounding box of pixels that differ from the uniform corner color
/// (inclusive pixel coordinates). `None` when the corners disagree, are
/// transparent, or nothing differs from the keyed background.
fn background_keyed_bbox(pm: &tiny_skia::Pixmap) -> Option<(usize, usize, usize, usize)> {
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    let corners = [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)];
    let mut bg = [0i32; 3];
    for &(x, y) in &corners {
        let px = pm.pixel(x as u32, y as u32)?;
        if px.alpha() < 255 {
            return None;
        }
        bg[0] += px.red() as i32;
        bg[1] += px.green() as i32;
        bg[2] += px.blue() as i32;
    }
    // Premultiplied channel values equal straight values for opaque pixels,
    // which the alpha check above guarantees for all four corners.
    let bg = [bg[0] / 4, bg[1] / 4, bg[2] / 4];
    for &(x, y) in &corners {
        let px = pm.pixel(x as u32, y as u32)?;
        let c = [px.red() as i32, px.green() as i32, px.blue() as i32];
        if c.iter().zip(bg.iter()).any(|(v, b)| (v - b).abs() > NORMALIZE_BG_TOLERANCE as i32) {
            return None;
        }
    }

    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    for (i, px) in pm.data().chunks_exact(4).enumerate() {
        let differs = px[3] > NORMALIZE_ALPHA_THRESHOLD
            && [(px[0], bg[0]), (px[1], bg[1]), (px[2], bg[2])]
                .iter()
                .any(|(v, b)| (i32::from(*v) - b).abs() > NORMALIZE_BG_TOLERANCE as i32);
        if differs {
            let (x, y) = (i % w, i / w);
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    if max_x < min_x {
        return None;
    }
    Some((min_x, min_y, max_x - min_x + 1, max_y - min_y + 1))
}

/// Re-render `pixmap` cropped to the given box into a full 192x192 PNG via
/// the SVG viewBox (aspect-preserving, centered).
fn render_pixmap_cropped(
    pm: &tiny_skia::Pixmap,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
) -> Option<Vec<u8>> {
    let full = pm.encode_png().ok()?;
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" \
             xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
             width=\"{OUTPUT_SIZE}\" height=\"{OUTPUT_SIZE}\" \
             viewBox=\"{x} {y} {w} {h}\" \
             preserveAspectRatio=\"xMidYMid meet\">\
             <image x=\"0\" y=\"0\" width=\"{}\" height=\"{}\" \
             xlink:href=\"data:image/png;base64,{}\"/></svg>",
        pm.width(),
        pm.height(),
        base64_encode(&full),
    );
    render_svg(&svg, OUTPUT_SIZE)
}

/// Normalize any raster launcher icon (PNG or WebP, R2-F4).
///
/// WebP bytes cannot be decoded here directly, but the SVG renderer decodes
/// WebP data URIs (the adaptive-layer path relies on this), so WebP is
/// pre-rendered to PNG pixels first and then normalized like any PNG. PNG
/// input goes straight to `normalize_raster_png`. `None` when the input is
/// undecodable as either format.
pub fn normalize_raster_any(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.starts_with(b"RIFF") && bytes.len() > 11 && &bytes[8..12] == b"WEBP" {
        let svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" \
                 xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
                 width=\"{OUTPUT_SIZE}\" height=\"{OUTPUT_SIZE}\" \
                 preserveAspectRatio=\"xMidYMid meet\">\
                 <image width=\"{OUTPUT_SIZE}\" height=\"{OUTPUT_SIZE}\" \
                 xlink:href=\"data:image/webp;base64,{}\"/></svg>",
            base64_encode(bytes),
        );
        let png = render_svg(&svg, OUTPUT_SIZE)?;
        return normalize_raster_png(&png);
    }
    normalize_raster_png(bytes)
}

/// Count DISTINCT fully-opaque RGB colors in rendered PNG bytes (T-A).
///
/// Degeneracy guard for the manifest-anchored icon pipeline: stub templates
/// (the shared green-robot, black/white partial adaptive renders) produce
/// very few distinct colors, while a real rendered icon (logo, wordmark,
/// gradient) produces many. Returns 0 when the bytes cannot be decoded as
/// PNG. Never panics.
pub fn distinct_opaque_colors(png_bytes: &[u8]) -> usize {
    let Ok(pixmap) = tiny_skia::Pixmap::decode_png(png_bytes) else {
        return 0;
    };
    let mut colors = std::collections::HashSet::new();
    for px in pixmap.data().chunks_exact(4) {
        if px[3] > 200 {
            colors.insert((px[0], px[1], px[2]));
        }
    }
    colors.len()
}

/// Fraction of the opaque pixels sharing the single most common RGB color
/// (T-A). A second degeneracy axis: a missing adaptive layer renders a
/// near-monochrome canvas (Telegram's white 95.8% background-only render)
/// that can still carry many anti-aliasing shades. A fully transparent
/// render counts as 1.0 (nothing painted); undecodable bytes as 0.0.
pub fn dominant_opaque_fraction(png_bytes: &[u8]) -> f32 {
    let Ok(pixmap) = tiny_skia::Pixmap::decode_png(png_bytes) else {
        return 0.0;
    };
    let mut counts: std::collections::HashMap<(u8, u8, u8), u32> =
        std::collections::HashMap::new();
    let mut total = 0u32;
    for px in pixmap.data().chunks_exact(4) {
        if px[3] > 200 {
            *counts.entry((px[0], px[1], px[2])).or_insert(0) += 1;
            total += 1;
        }
    }
    if total == 0 {
        return 1.0;
    }
    counts.values().copied().max().unwrap_or(0) as f32 / total as f32
}

/// Full adaptive-icon pipeline: decode the adaptive-icon AXML at `xml_bytes`,
/// resolve layer references through `read_entry` (zip entry name -> bytes)
/// and `arsc_bytes` (raw resources.arsc), then render to a 192x192 PNG.
/// Any failure yields `None`.
pub fn adaptive_icon_png(
    xml_bytes: &[u8],
    read_entry: &dyn Fn(&str) -> Option<Vec<u8>>,
    arsc_bytes: Option<&[u8]>,
) -> Option<Vec<u8>> {
    let arsc = arsc_bytes.and_then(|b| ARSC::new(&mut &b[..]).ok());
    match arsc.as_ref() {
        Some(a) => adaptive_icon_png_res(xml_bytes, read_entry, a),
        None => adaptive_icon_png_res(xml_bytes, read_entry, &NullResolver),
    }
}

/// Resolver-based variant of [`adaptive_icon_png`]: layer references resolve
/// through `res` (config-aware, possibly across base + split APKs).
pub fn adaptive_icon_png_res(
    xml_bytes: &[u8],
    read_entry: &dyn Fn(&str) -> Option<Vec<u8>>,
    res: &dyn ResourceResolver,
) -> Option<Vec<u8>> {
    let axml = AXML::new(&mut &xml_bytes[..], res.axml_arsc()).ok()?;

    let icon = match axml.root.name() {
        "adaptive-icon" => {
            // Obfuscated apps reuse ONE resource name across different ids
            // (Gboard's background/foreground are both
            // `mipmap/0_resource_name_obfuscated`), so ARSC-rendered name
            // references resolve ambiguously to the first matching id. The
            // binary XML stores layer refs as resource IDS: re-parse without
            // the ARSC so refs survive as raw `@<hex id>` tokens and resolve
            // config-aware (cross-split) by id.
            let raw_root = AXML::new(&mut &xml_bytes[..], None)
                .ok()
                .filter(|raw| raw.root.name() == "adaptive-icon")
                .map(|raw| raw.root);
            let layer_root = raw_root.as_ref().unwrap_or(&axml.root);
            let background = adaptive_layer(layer_root, "background", res, read_entry, 0);
            let foreground = adaptive_layer(layer_root, "foreground", res, read_entry, 0);
            if background.is_none() && foreground.is_none() {
                return None;
            }
            AdaptiveIcon { background, foreground }
        }
        // Plain vector drawable used directly as the launcher icon.
        "vector" => {
            let vector = decode_vector(&axml.root, res, read_entry)?;
            AdaptiveIcon { background: None, foreground: Some(Layer::Vector(vector)) }
        }
        // Shape drawables (rectangle/oval) used directly as the launcher icon.
        "shape" => {
            let vector = decode_shape(&axml.root, res)?;
            AdaptiveIcon { background: None, foreground: Some(Layer::Vector(vector)) }
        }
        // Bitmap drawables reference a raster resource via android:src.
        "bitmap" => {
            let layer = decode_bitmap(&axml.root, res, read_entry)?;
            AdaptiveIcon { background: None, foreground: Some(layer) }
        }
        _ => return None,
    };

    if icon.background.is_none() && icon.foreground.is_none() {
        return None;
    }
    // Adaptive compositing rasterizes only the center safe zone; a direct
    // vector/shape/bitmap icon fills its canvas and is not cropped.
    let svg = match axml.root.name() {
        "adaptive-icon" => compose_adaptive_svg(&icon),
        _ => compose_svg(&icon),
    };
    render_svg(&svg, OUTPUT_SIZE)
}

/// Resolve one adaptive-icon layer (`<background>`/`<foreground>`) of an
/// `<adaptive-icon>` root. Two shapes are supported (T-D): the classic
/// `android:drawable` reference attribute, and an inline child drawable
/// element (e.g. `<background><bitmap android:src="@mipmap/..."/></background>`,
/// as emitted by AAB apps like Google Sheets).
fn adaptive_layer(
    root: &Element,
    tag: &str,
    res: &dyn ResourceResolver,
    read_entry: &dyn Fn(&str) -> Option<Vec<u8>>,
    depth: usize,
) -> Option<Layer> {
    let el = root.childrens().find(|c| c.name() == tag)?;
    // 1. `android:drawable` reference token.
    if let Some(raw) = el.attr("drawable") {
        let value = resolve_ref_token(&raw, res)?;
        return value_to_layer(&value, read_entry, res, depth);
    }
    // 2. Inline child drawable element.
    let child = el.childrens().next()?;
    match child.name() {
        "vector" => decode_vector(child, res, read_entry).map(Layer::Vector),
        "shape" => decode_shape(child, res).map(Layer::Vector),
        "bitmap" => decode_bitmap(child, res, read_entry),
        _ => None,
    }
}

/// Map a resolved resource value (a file path, a color literal, or a further
/// reference) to a layer.
fn value_to_layer(
    value: &str,
    read_entry: &dyn Fn(&str) -> Option<Vec<u8>>,
    res: &dyn ResourceResolver,
    depth: usize,
) -> Option<Layer> {
    if depth > MAX_XML_DEPTH {
        return None;
    }
    if value.starts_with('#') {
        return parse_color(value).map(Layer::Color);
    }
    if value.starts_with("res/") {
        // The value IS the zip entry path ("res/..."), exactly what the
        // zip central-directory lookup needs.
        let bytes = read_entry(value)?;
        if value.ends_with(".xml") {
            return xml_bytes_to_layer(&bytes, read_entry, res, depth);
        }
        if value.ends_with(".png")
            || value.ends_with(".webp")
            || value.ends_with(".jpg")
            || value.ends_with(".jpeg")
        {
            return Some(Layer::Raster(bytes));
        }
        // Obfuscated APKs (Cốc Cốc) strip resource extensions; sniff the
        // byte magic instead of trusting the name, and fall through to an
        // AXML attempt for anything else.
        if bytes.starts_with(b"\x89PNG")
            || (bytes.starts_with(b"RIFF") && bytes.len() > 11 && &bytes[8..12] == b"WEBP")
        {
            return Some(Layer::Raster(bytes));
        }
        return xml_bytes_to_layer(&bytes, read_entry, res, depth);
    }
    // Unresolved references (e.g. redacted `@0x…` ids) are unsupported.
    None
}

fn xml_bytes_to_layer(
    bytes: &[u8],
    read_entry: &dyn Fn(&str) -> Option<Vec<u8>>,
    res: &dyn ResourceResolver,
    depth: usize,
) -> Option<Layer> {
    let axml = AXML::new(&mut &bytes[..], res.axml_arsc()).ok()?;
    match axml.root.name() {
        "vector" => decode_vector(&axml.root, res, read_entry).map(Layer::Vector),
        "shape" => decode_shape(&axml.root, res).map(Layer::Vector),
        // Bitmap drawables embed their referenced raster directly.
        "bitmap" => decode_bitmap(&axml.root, res, read_entry),
        // `<layer-list>` stacks its `<item>` drawables (OneDrive's adaptive
        // foreground, R2-F2).
        "layer-list" if depth < MAX_XML_DEPTH => {
            decode_layer_list(&axml.root, res, read_entry, depth)
        }
        // `<inset android:drawable=... android:inset*="f%">` wraps another
        // drawable with margins (Wallet's launcher foreground, T-A). The
        // layer canvas IS the 108dp adaptive canvas, so percentage insets
        // apply directly as fractions; non-percentage dimensions degrade to
        // 0 (full-bleed inner drawable).
        "inset" if depth < MAX_XML_DEPTH => {
            let raw = axml.root.attr("drawable")?;
            let resolved = resolve_ref_token(raw, res)?;
            let inner = value_to_layer(&resolved, read_entry, res, depth + 1)?;
            let pct = |name: &str| -> f32 {
                axml.root
                    .attr(name)
                    .and_then(|v| v.strip_suffix('%'))
                    .and_then(|v| v.parse::<f32>().ok())
                    .map_or(0.0, |p| (p / 100.0).clamp(0.0, 1.0))
            };
            let (l, t, r, b) =
                (pct("insetLeft"), pct("insetTop"), pct("insetRight"), pct("insetBottom"));
            let v = ADAPTIVE_VIEWPORT;
            let (iw, ih) = (v * (1.0 - l - r), v * (1.0 - t - b));
            if iw <= 0.0 || ih <= 0.0 {
                return None;
            }
            let mut defs = String::new();
            let mut body = String::new();
            let mut next_id = 0u32;
            render_layer(&inner, &mut defs, &mut next_id, &mut body);
            let defs =
                if defs.is_empty() { String::new() } else { format!("<defs>{defs}</defs>") };
            let svg = format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" \
                     xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
                     width=\"{}\" height=\"{}\" viewBox=\"0 0 {v} {v}\">{defs}\
                     <g transform=\"translate({},{}) scale({},{})\">{body}</g></svg>",
                fmt_num(v),
                fmt_num(v),
                fmt_num(v * l),
                fmt_num(v * t),
                fmt_num(iw / v),
                fmt_num(ih / v),
            );
            render_svg(&svg, ADAPTIVE_VIEWPORT as u32).map(Layer::Raster)
        }
        // Nested adaptive icon (e.g. foreground points at another one):
        // resolve its layers and rasterize at layer resolution.
        "adaptive-icon" if depth < MAX_XML_DEPTH => {
            // Raw re-parse for layer refs (same obfuscated-name-collision
            // reasoning as `adaptive_icon_png_res`).
            let raw_root = AXML::new(&mut &bytes[..], None)
                .ok()
                .filter(|raw| raw.root.name() == "adaptive-icon")
                .map(|raw| raw.root);
            let layer_root = raw_root.as_ref().unwrap_or(&axml.root);
            let background = adaptive_layer(layer_root, "background", res, read_entry, depth + 1);
            let foreground = adaptive_layer(layer_root, "foreground", res, read_entry, depth + 1);
            if background.is_none() && foreground.is_none() {
                return None;
            }
            let inner = AdaptiveIcon { background, foreground };
            // Rendered at the 108dp layer size (safe-zone cropped), embedded
            // 1:1 by the parent.
            render_svg(&compose_adaptive_svg(&inner), ADAPTIVE_VIEWPORT as u32).map(Layer::Raster)
        }
        _ => None,
    }
}

/// Compose a `<layer-list>` drawable (OneDrive's adaptive foreground,
/// R2-F2): each `<item>` resolves to a layer and renders onto the shared
/// 108dp adaptive canvas in document order (later items paint on top).
/// Items that fail to resolve are skipped; `None` when no item resolves.
fn decode_layer_list(
    root: &Element,
    res: &dyn ResourceResolver,
    read_entry: &dyn Fn(&str) -> Option<Vec<u8>>,
    depth: usize,
) -> Option<Layer> {
    let mut defs = String::new();
    let mut body = String::new();
    let mut next_id = 0u32;
    let mut any = false;
    for item in root.childrens().filter(|c| c.name() == "item") {
        let layer = match item.attr("drawable") {
            Some(raw) => resolve_ref_token(&raw, res)
                .and_then(|value| value_to_layer(&value, read_entry, res, depth + 1)),
            None => item.childrens().next().and_then(|child| match child.name() {
                "vector" => decode_vector(child, res, read_entry).map(Layer::Vector),
                "shape" => decode_shape(child, res).map(Layer::Vector),
                "bitmap" => decode_bitmap(child, res, read_entry),
                _ => None,
            }),
        };
        if let Some(layer) = layer {
            render_layer(&layer, &mut defs, &mut next_id, &mut body);
            any = true;
        }
    }
    if !any {
        return None;
    }
    let v = ADAPTIVE_VIEWPORT;
    let defs = if defs.is_empty() { String::new() } else { format!("<defs>{defs}</defs>") };
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" \
             xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
             width=\"{v}\" height=\"{v}\" viewBox=\"0 0 {v} {v}\">{defs}{body}</svg>"
    );
    render_svg(&svg, ADAPTIVE_VIEWPORT as u32).map(Layer::Raster)
}

#[cfg(test)]
mod tests {
    use super::*;

    use apk_info_xml::Element;

    /// No-op zip reader for degraded paths.
    fn no_read_entry(_name: &str) -> Option<Vec<u8>> {
        None
    }

    /// Build an element with android-namespaced attributes.
    fn el(name: &str, attrs: &[(&str, &str)]) -> Element {
        let mut e = Element::new(name);
        for (k, v) in attrs {
            e.set_attribute_with_prefix(Some("android"), k, v);
        }
        e
    }

    // ---------- parse_color ----------

    #[test]
    fn test_parse_color_handles_rgb_and_rrggbb() {
        // Arrange/Act/Assert: #RGB expands, #RRGGBB is opaque.
        assert_eq!(parse_color("#F00"), Some([255, 0, 0, 255]));
        assert_eq!(parse_color("#FF0000"), Some([255, 0, 0, 255]));
        assert_eq!(parse_color("#336699"), Some([0x33, 0x66, 0x99, 255]));
    }

    #[test]
    fn test_parse_color_handles_argb_and_aarrggbb() {
        // Arrange/Act/Assert: 4/8-digit forms carry alpha first; digits are doubled.
        assert_eq!(parse_color("#8F00"), Some([255, 0, 0, 0x88]));
        assert_eq!(parse_color("#80FF0000"), Some([255, 0, 0, 0x80]));
    }

    #[test]
    fn test_parse_color_invalid_returns_none() {
        // Arrange/Act/Assert: bad lengths / junk / empty are rejected.
        assert_eq!(parse_color(""), None);
        assert_eq!(parse_color("#"), None);
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("#GGHHII"), None);
        assert_eq!(parse_color("red"), None);
    }

    // ---------- decode_vector ----------

    #[test]
    fn test_decode_vector_parses_paths_and_nested_groups() {
        // Arrange: vector with one path and a group holding another path.
        let mut group = el("group", &[("translateX", "10.5"), ("scaleY", "2")]);
        group.append_child(el(
            "path",
            &[("pathData", "M20,20 L30,20"), ("fillColor", "#FF00FF")],
        ));

        let mut root = el(
            "vector",
            &[("viewportWidth", "24"), ("viewportHeight", "24")],
        );
        root.append_child(el(
            "path",
            &[
                ("pathData", "M10,10 L20,10 L20,20 Z"),
                ("fillColor", "#FF3366CC"),
                ("fillAlpha", "0.5"),
            ],
        ));
        root.append_child(group);

        // Act
        let vd = decode_vector(&root, &NullResolver, &no_read_entry).expect("vector must decode");

        // Assert
        assert_eq!(vd.viewport, (24.0, 24.0));
        assert_eq!(vd.nodes.len(), 2);
        match &vd.nodes[0] {
            VNode::Path { path_data, fill, fill_alpha } => {
                assert_eq!(path_data, "M10,10 L20,10 L20,20 Z");
                assert_eq!(*fill, VFill::Color([0x33, 0x66, 0xCC, 0xFF]));
                assert_eq!(*fill_alpha, 0.5);
            }
            other => panic!("expected path, got {other:?}"),
        }
        match &vd.nodes[1] {
            VNode::Group { translate, scale, children, .. } => {
                assert_eq!(*translate, (10.5, 0.0));
                assert_eq!(*scale, (1.0, 2.0));
                assert_eq!(children.len(), 1);
            }
            other => panic!("expected group, got {other:?}"),
        }
    }

    #[test]
    fn test_decode_vector_theme_fillcolor_returns_none() {
        // Arrange: theme attr refs cannot be resolved from AXML alone.
        let mut root = el(
            "vector",
            &[("viewportWidth", "24"), ("viewportHeight", "24")],
        );
        root.append_child(el(
            "path",
            &[("pathData", "M0,0 L1,1"), ("fillColor", "?attr/colorPrimary")],
        ));

        // Act/Assert: unsupported construct degrades to None.
        assert_eq!(decode_vector(&root, &NullResolver, &no_read_entry), None);
    }

    #[test]
    fn test_decode_vector_color_ref_without_arsc_returns_none() {
        // Arrange: @color ref is unresolvable without a resource table.
        let mut root = el(
            "vector",
            &[("viewportWidth", "24"), ("viewportHeight", "24")],
        );
        root.append_child(el(
            "path",
            &[("pathData", "M0,0 L1,1"), ("fillColor", "@color/white")],
        ));

        // Act/Assert
        assert_eq!(decode_vector(&root, &NullResolver, &no_read_entry), None);
    }

    #[test]
    fn test_decode_vector_missing_viewport_returns_none() {
        // Arrange/Act/Assert: viewportWidth/Height are required.
        let root = el("vector", &[]);
        assert_eq!(decode_vector(&root, &NullResolver, &no_read_entry), None);
    }

    // ---------- decode_adaptive_refs ----------

    #[test]
    fn test_decode_adaptive_refs_extracts_background_and_foreground() {
        // Arrange: standard adaptive-icon shape.
        let mut root = el("adaptive-icon", &[]);
        root.append_child(el("background", &[("drawable", "@color/ic_launcher_background")]));
        root.append_child(el("foreground", &[("drawable", "@mipmap/ic_launcher_foreground")]));

        // Act
        let (bg, fg) = decode_adaptive_refs(&root);

        // Assert
        assert_eq!(bg.as_deref(), Some("@color/ic_launcher_background"));
        assert_eq!(fg.as_deref(), Some("@mipmap/ic_launcher_foreground"));
    }

    #[test]
    fn test_decode_adaptive_refs_missing_children_yields_nones() {
        // Arrange/Act/Assert: root without layer children degrades cleanly.
        let root = el("adaptive-icon", &[]);
        let (bg, fg) = decode_adaptive_refs(&root);
        assert_eq!(bg, None);
        assert_eq!(fg, None);
    }

    // ---------- compose_svg ----------

    #[test]
    fn test_compose_svg_color_background_under_vector_foreground() {
        // Arrange: solid background + vector foreground on a 24x24 viewport.
        let icon = AdaptiveIcon {
            background: Some(Layer::Color([255, 0, 0, 255])),
            foreground: Some(Layer::Vector(VectorDrawable {
                viewport: (24.0, 24.0),
                nodes: vec![VNode::Path {
                    path_data: "M10,10 L20,10 L20,20 Z".to_string(),
                    fill: VFill::Color([0x33, 0x66, 0xCC, 0xFF]),
                    fill_alpha: 1.0,
                }],
            })),
        };

        // Act
        let svg = compose_svg(&icon);

        // Assert: root geometry, background rect below foreground path.
        assert!(svg.contains("width=\"192\""), "svg must be 192px wide: {svg}");
        assert!(svg.contains("height=\"192\""), "svg must be 192px tall: {svg}");
        assert!(svg.contains("viewBox=\"0 0 108 108\""), "adaptive viewport: {svg}");
        let rect_pos = svg.find("<rect").expect("background rect");
        let scale_pos = svg.find("scale(4.5,4.5)").expect("vector scaled to 108 canvas");
        let path_pos = svg.find("d=\"M10,10 L20,10 L20,20 Z\"").expect("path data");
        assert!(rect_pos < scale_pos && scale_pos < path_pos, "z-order bg<fg: {svg}");
        assert!(svg.contains("fill=\"#3366cc\""), "path fill: {svg}");
    }

    #[test]
    fn test_compose_svg_raster_layer_embeds_base64_image() {
        // Arrange: PNG-ish bytes on the background layer.
        let icon = AdaptiveIcon {
            background: Some(Layer::Raster(vec![0x89, b'P', b'N', b'G'])),
            foreground: None,
        };

        // Act
        let svg = compose_svg(&icon);

        // Assert: data URI embedded image, standard base64 of the bytes.
        assert!(
            svg.contains("xlink:href=\"data:image/png;base64,iVBORw"),
            "embedded raster as data uri: {svg}"
        );
        assert!(svg.contains("<image"), "image element: {svg}");
    }

    #[test]
    fn test_compose_svg_group_transform_matches_android_matrix() {
        // Arrange: group with pivot+rotation+translate+scale.
        let icon = AdaptiveIcon {
            background: None,
            foreground: Some(Layer::Vector(VectorDrawable {
                viewport: (108.0, 108.0),
                nodes: vec![VNode::Group {
                    translate: (2.0, 3.0),
                    rotation: 45.0,
                    pivot: (10.0, 20.0),
                    scale: (1.5, 1.5),
                    clip: None,
                    children: vec![VNode::Path {
                        path_data: "M0,0 L1,1".to_string(),
                        fill: VFill::Color([0, 0, 0, 255]),
                        fill_alpha: 1.0,
                    }],
                }],
            })),
        };

        // Act
        let svg = compose_svg(&icon);

        // Assert: M = translate(t+p) rotate scale translate(-p).
        assert!(
            svg.contains("transform=\"translate(12,23) rotate(45) scale(1.5,1.5) translate(-10,-20)\""),
            "android group matrix order: {svg}"
        );
    }

    #[test]
    fn test_compose_svg_alpha_becomes_fill_opacity() {
        // Arrange: translucent path.
        let icon = AdaptiveIcon {
            background: None,
            foreground: Some(Layer::Vector(VectorDrawable {
                viewport: (108.0, 108.0),
                nodes: vec![VNode::Path {
                    path_data: "M0,0 L1,1".to_string(),
                    fill: VFill::Color([255, 255, 255, 128]),
                    fill_alpha: 0.5,
                }],
            })),
        };

        // Act
        let svg = compose_svg(&icon);

        // Assert: combined alpha = 128/255 * 0.5, in range (0,1).
        let pos = svg.find("fill-opacity=\"").expect("opacity emitted");
        let rest = &svg[pos + "fill-opacity=\"".len()..];
        let val: f32 = rest.split('"').next().unwrap().parse().unwrap();
        assert!((0.2..0.26).contains(&val), "combined alpha ~0.251, got {val}");
    }

    // ---------- render_svg ----------

    #[test]
    fn test_render_svg_produces_png_at_requested_size() {
        // Arrange: a trivial full-canvas red rect.
        let svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{size}\" height=\"{size}\" \
             viewBox=\"0 0 108 108\"><rect width=\"108\" height=\"108\" fill=\"#ff0000\"/></svg>",
            size = OUTPUT_SIZE
        );

        // Act
        let png = render_svg(&svg, OUTPUT_SIZE).expect("must render");

        // Assert: PNG magic + exact dimensions via tiny-skia decode.
        assert_eq!(&png[..4], b"\x89PNG");
        let pixmap = tiny_skia::Pixmap::decode_png(&png).expect("decodes as png");
        assert_eq!((pixmap.width(), pixmap.height()), (OUTPUT_SIZE, OUTPUT_SIZE));
        // Center pixel must be red (non-trivial content).
        let px = pixmap.pixel(96, 96).expect("pixel exists");
        assert_eq!((px.red(), px.green(), px.blue()), (255, 0, 0));
    }

    #[test]
    fn test_render_svg_invalid_svg_returns_none() {
        // Arrange/Act/Assert: garbage degrades to None, never panics.
        assert_eq!(render_svg("not an svg at all", 64), None);
        assert_eq!(render_svg("", 64), None);
    }

    // ---------- adaptive_icon_png (full pipeline) ----------

    #[test]
    fn test_adaptive_icon_png_unparsable_bytes_return_none() {
        // Arrange: a no-op reader; junk bytes cannot be AXML.
        let read = |_name: &str| -> Option<Vec<u8>> { None };

        // Act/Assert: silent degrade.
        assert_eq!(adaptive_icon_png(b"garbage", &read, None), None);
    }

    // ---------- adaptive center crop (T-B) ----------

    /// Foreground-only adaptive icon helper.
    fn fg_vector(viewport: (f32, f32), nodes: Vec<VNode>) -> AdaptiveIcon {
        AdaptiveIcon {
            background: None,
            foreground: Some(Layer::Vector(VectorDrawable { viewport, nodes })),
        }
    }

    fn painted_pixels(png: &[u8]) -> u32 {
        tiny_skia::Pixmap::decode_png(png)
            .expect("png decodes")
            .data()
            .chunks_exact(4)
            .filter(|px| px[3] > 0)
            .count() as u32
    }

    #[test]
    fn test_compose_adaptive_svg_crops_viewbox_to_center_safe_zone() {
        // Arrange: adaptive compositing must show only the launcher-visible
        // center 72dp of the 108dp canvas.
        let icon = fg_vector(
            (108.0, 108.0),
            vec![VNode::Path {
                path_data: "M40,40 L60,40 L60,60 Z".to_string(),
                fill: VFill::Color([255, 0, 0, 255]),
                fill_alpha: 1.0,
            }],
        );

        // Act
        let svg = compose_adaptive_svg(&icon);

        // Assert
        assert!(svg.contains("viewBox=\"18 18 72 72\""), "center crop: {svg}");
        assert!(svg.contains("width=\"192\""), "still 192px wide: {svg}");
        assert!(svg.contains("height=\"192\""), "still 192px tall: {svg}");
    }

    #[test]
    fn test_compose_adaptive_svg_rasterizes_only_safe_zone() {
        // Arrange: a path fully inside the outer 18dp margin must vanish
        // after the crop; a centered path must remain visible.
        let margin_icon = fg_vector(
            (108.0, 108.0),
            vec![VNode::Path {
                path_data: "M0,0 L10,0 L10,10 Z".to_string(),
                fill: VFill::Color([255, 0, 0, 255]),
                fill_alpha: 1.0,
            }],
        );
        let center_icon = fg_vector(
            (108.0, 108.0),
            vec![VNode::Path {
                path_data: "M40,40 L68,40 L68,68 Z".to_string(),
                fill: VFill::Color([255, 0, 0, 255]),
                fill_alpha: 1.0,
            }],
        );

        // Act
        let margin_png =
            render_svg(&compose_adaptive_svg(&margin_icon), OUTPUT_SIZE).expect("renders");
        let center_png =
            render_svg(&compose_adaptive_svg(&center_icon), OUTPUT_SIZE).expect("renders");

        // Assert
        assert_eq!(
            painted_pixels(&margin_png),
            0,
            "content outside the 72dp safe zone must be cropped away"
        );
        assert!(
            painted_pixels(&center_png) > 1000,
            "centered content must remain visible"
        );
    }

    // ---------- raster normalization (T-E) ----------

    /// PNG of a `size`x`size` transparent canvas with a centered opaque red
    /// square of side `square` (transparent padding when square < size).
    fn padded_square_png(size: u32, square: u32) -> Vec<u8> {
        let mut pm = tiny_skia::Pixmap::new(size, size).expect("pixmap allocates");
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(255, 0, 0, 255);
        let off = ((size - square) / 2) as f32;
        let rect = tiny_skia::Rect::from_xywh(off, off, square as f32, square as f32)
            .expect("square fits canvas");
        let path = tiny_skia::PathBuilder::from_rect(rect);
        pm.fill_path(
            &path,
            &paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
        pm.encode_png().expect("encoding a valid pixmap works")
    }

    /// Fraction of the canvas covered by the opaque-content bounding box.
    fn bbox_ratio(pm: &tiny_skia::Pixmap) -> (f32, f32) {
        let (w, h) = (pm.width() as usize, pm.height() as usize);
        let mut min_x = w;
        let mut min_y = h;
        let mut max_x = 0usize;
        let mut max_y = 0usize;
        for (i, px) in pm.data().chunks_exact(4).enumerate() {
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

    #[test]
    fn test_normalize_raster_png_padded_icon_content_fills_canvas() {
        // Arrange: 192x192 PNG whose content occupies only ~50% (baked
        // transparent padding, the Gorio/Green-SM legacy-raster symptom).
        let png = padded_square_png(192, 96);

        // Act
        let out = normalize_raster_png(&png).expect("padded icon must normalize");

        // Assert: 192x192 output whose content fills the canvas.
        let pm = tiny_skia::Pixmap::decode_png(&out).expect("normalized output decodes");
        assert_eq!((pm.width(), pm.height()), (192, 192));
        let (bw, bh) = bbox_ratio(&pm);
        assert!(bw >= 0.85 && bh >= 0.85, "content must fill canvas, got {bw:.3}x{bh:.3}");
    }

    #[test]
    fn test_normalize_raster_png_small_canvas_renders_at_192() {
        // Arrange: a smaller NxN padded raster (e.g. a 64px legacy icon).
        let png = padded_square_png(64, 32);

        // Act
        let out = normalize_raster_png(&png).expect("padded small icon must normalize");

        // Assert
        let pm = tiny_skia::Pixmap::decode_png(&out).expect("normalized output decodes");
        assert_eq!((pm.width(), pm.height()), (192, 192));
        let (bw, bh) = bbox_ratio(&pm);
        assert!(bw >= 0.85 && bh >= 0.85, "content must fill canvas, got {bw:.3}x{bh:.3}");
    }

    #[test]
    fn test_normalize_raster_png_full_bleed_unchanged() {
        // Arrange: content already covers the full canvas — must not be
        // re-scaled (assert dimensions + bbox, not byte equality).
        let png = padded_square_png(192, 192);

        // Act
        let out = normalize_raster_png(&png).expect("full-bleed icon must pass through");

        // Assert
        let pm = tiny_skia::Pixmap::decode_png(&out).expect("output decodes");
        assert_eq!((pm.width(), pm.height()), (192, 192));
        let (bw, bh) = bbox_ratio(&pm);
        assert!(bw >= 0.99 && bh >= 0.99, "content must stay full-bleed, got {bw:.3}x{bh:.3}");
    }

    #[test]
    fn test_normalize_raster_png_garbage_returns_none() {
        // Arrange/Act/Assert: decode failures degrade to None (caller keeps
        // the un-normalized bytes); never panics.
        assert_eq!(normalize_raster_png(b"not a png at all"), None);
        assert_eq!(normalize_raster_png(&[]), None);
    }

    // ---------- extension-less layer refs (R2-F1), layer-list (R2-F2),
    // ---------- opaque-background normalization (R2-F4) ----------

    /// Fully-opaque `size`x`size` PNG: uniform `bg` canvas with a centered
    /// `fg` square of side `square` (baked-in opaque margins).
    fn opaque_bg_square_png(size: u32, square: u32, bg: [u8; 4], fg: [u8; 4]) -> Vec<u8> {
        let mut pm = tiny_skia::Pixmap::new(size, size).expect("pixmap allocates");
        pm.fill(tiny_skia::Color::from_rgba8(bg[0], bg[1], bg[2], bg[3]));
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(fg[0], fg[1], fg[2], fg[3]);
        let off = ((size - square) / 2) as f32;
        let rect = tiny_skia::Rect::from_xywh(off, off, square as f32, square as f32)
            .expect("square fits canvas");
        pm.fill_path(
            &tiny_skia::PathBuilder::from_rect(rect),
            &paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
        pm.encode_png().expect("encoding a valid pixmap works")
    }

    #[test]
    fn test_value_to_layer_extensionless_webp_entry_sniffs_magic() {
        // Obfuscated APKs (Cốc Cốc) ship layer resources with NO extension;
        // the layer loader must sniff RIFF/WEBP magic, not gate on the name.
        let webp = b"RIFF\x24\x00\x00\x00WEBPVP8L-stub-bytes".to_vec();
        let read = move |_: &str| Some(webp.clone());

        // Act
        let layer = value_to_layer("res/tdA", &read, &NullResolver, 0);

        // Assert
        assert!(matches!(layer, Some(Layer::Raster(_))), "webp magic must map to a raster layer");
    }

    #[test]
    fn test_value_to_layer_extensionless_png_entry_sniffs_magic() {
        // Arrange: PNG bytes under an extension-less resource name.
        let png = b"\x89PNG\r\n\x1a\nfake-idat".to_vec();
        let read = move |_: &str| Some(png.clone());

        // Act
        let layer = value_to_layer("res/ZAU", &read, &NullResolver, 0);

        // Assert
        assert!(matches!(layer, Some(Layer::Raster(_))), "png magic must map to a raster layer");
    }

    #[test]
    fn test_value_to_layer_extensionless_garbage_entry_returns_none() {
        // Arrange/Act/Assert: unrecognizable bytes under any name stay None.
        let read = |_: &str| Some(b"junk-junk".to_vec());
        assert_eq!(value_to_layer("res/xxx", &read, &NullResolver, 0), None);
    }

    /// Minimal `<vector>` element with one full-canvas path of `color`.
    fn full_vector_el(color: &str) -> Element {
        let mut v = el("vector", &[("viewportWidth", "24"), ("viewportHeight", "24")]);
        v.append_child(el("path", &[("pathData", "M0,0 L24,24 L24,0 Z"), ("fillColor", color)]));
        v
    }

    #[test]
    fn test_decode_layer_list_stacks_inline_items() {
        // OneDrive's adaptive foreground is a layer-list of stacked items;
        // the layer loader must compose them instead of returning None.
        let mut root = el("layer-list", &[]);
        let mut item1 = el("item", &[]);
        item1.append_child(full_vector_el("#FF0000"));
        let mut item2 = el("item", &[]);
        item2.append_child(full_vector_el("#00FF00"));
        root.append_child(item1);
        root.append_child(item2);

        // Act
        let out = decode_layer_list(&root, &NullResolver, &no_read_entry, 0);

        // Assert
        assert!(
            matches!(out, Some(Layer::Raster(_))),
            "layer-list must compose its items into one layer, got {out:?}"
        );
    }

    #[test]
    fn test_decode_layer_list_without_resolvable_items_returns_none() {
        // Arrange: empty layer-list and one with an unresolvable @ref.
        let empty = el("layer-list", &[]);
        let mut unresolvable = el("layer-list", &[]);
        unresolvable.append_child(el("item", &[("drawable", "@mipmap/does_not_exist")]));

        // Act/Assert
        assert_eq!(decode_layer_list(&empty, &NullResolver, &no_read_entry, 0), None);
        assert_eq!(decode_layer_list(&unresolvable, &NullResolver, &no_read_entry, 0), None);
    }

    #[test]
    fn test_normalize_raster_png_opaque_bg_content_cropped_to_fill() {
        // White opaque canvas with a centered colored square (the OneDrive /
        // Cốc Cốc baked-margin symptom): the alpha bbox cannot crop opaque
        // margins, so the background-keyed box must drive the crop.
        let png = opaque_bg_square_png(64, 40, [255, 255, 255, 255], [0x1A, 0x73, 0xE8, 255]);

        // Act
        let out = normalize_raster_png(&png).expect("opaque-bg icon must normalize");

        // Assert: content now fills the 192x192 canvas (corner = content).
        let pm = tiny_skia::Pixmap::decode_png(&out).expect("normalized output decodes");
        assert_eq!((pm.width(), pm.height()), (192, 192));
        let (bw, bh) = bbox_ratio(&pm);
        assert!(bw >= 0.85 && bh >= 0.85, "content must fill canvas, got {bw:.3}x{bh:.3}");
        let px = pm.pixel(6, 6).expect("corner pixel exists");
        assert!(
            !(px.red() > 240 && px.green() > 240 && px.blue() > 240),
            "corner must be content, not the old background, got {px:?}"
        );
    }

    #[test]
    fn test_normalize_raster_png_opaque_fullbleed_photo_unchanged() {
        // Quadrant-colored canvas: corners disagree, so there is no uniform
        // background to key on — the bytes must pass through unchanged.
        let mut pm = tiny_skia::Pixmap::new(64, 64).expect("pixmap allocates");
        let quads = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]];
        for (i, px) in pm.data_mut().chunks_exact_mut(4).enumerate() {
            let (x, y) = (i % 64, i / 64);
            let q = quads[usize::from(x >= 32) * 2 + usize::from(y >= 32)];
            px[..3].copy_from_slice(&q);
            px[3] = 255;
        }
        let png = pm.encode_png().expect("encoding works");

        // Act
        let out = normalize_raster_png(&png).expect("decodes");

        // Assert
        assert_eq!(out, png, "corner-heterogeneous opaque canvas must pass through");
    }

    #[test]
    fn test_normalize_raster_png_opaque_bg_small_margin_unchanged() {
        // Content covers ~97% — inside the full-bleed tolerance, no rescale.
        let png = opaque_bg_square_png(64, 62, [255, 255, 255, 255], [0x1A, 0x73, 0xE8, 255]);

        // Act
        let out = normalize_raster_png(&png).expect("decodes");

        // Assert
        assert_eq!(out, png, "near-full-bleed opaque content must pass through");
    }

    #[test]
    fn test_normalize_raster_any_png_input_normalizes_like_png() {
        // Arrange: padded transparent raster (the legacy T-E symptom).
        let png = padded_square_png(64, 32);

        // Act
        let out = normalize_raster_any(&png).expect("png input must normalize");

        // Assert
        let pm = tiny_skia::Pixmap::decode_png(&out).expect("decodes");
        let (bw, bh) = bbox_ratio(&pm);
        assert!(bw >= 0.85 && bh >= 0.85, "content must fill canvas, got {bw:.3}x{bh:.3}");
    }

    #[test]
    fn test_normalize_raster_any_garbage_returns_none() {
        // Arrange/Act/Assert: undecodable input degrades to None.
        assert_eq!(normalize_raster_any(b"junk-junk"), None);
        assert_eq!(normalize_raster_any(&[]), None);
    }

    // ---------- distinct opaque colors (T-A degeneracy guard) ----------

    #[test]
    fn test_distinct_opaque_colors_solid_and_garbage() {
        // Arrange: a solid single-color PNG and undecodable bytes.
        let mut pm = tiny_skia::Pixmap::new(8, 8).expect("pixmap allocates");
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(255, 0, 0, 255);
        let rect = tiny_skia::Rect::from_xywh(0.0, 0.0, 8.0, 8.0).expect("rect fits");
        pm.fill_path(
            &tiny_skia::PathBuilder::from_rect(rect),
            &paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
        let png = pm.encode_png().expect("encoding works");

        // Act / Assert
        assert_eq!(distinct_opaque_colors(&png), 1, "solid color = 1 distinct");
        assert_eq!(distinct_opaque_colors(b"not a png"), 0, "garbage = 0");
        assert_eq!(distinct_opaque_colors(&[]), 0, "empty = 0");
    }

    #[test]
    fn test_distinct_opaque_colors_gradient_renders_many() {
        // Arrange: a two-stop linear gradient fill renders many shades.
        let icon = AdaptiveIcon {
            background: None,
            foreground: Some(Layer::Vector(VectorDrawable {
                viewport: (108.0, 108.0),
                nodes: vec![VNode::Path {
                    path_data: "M0,0 H108 V108 H0 Z".to_string(),
                    fill: VFill::Gradient(VGradient {
                        stops: vec![([255, 0, 0, 255], 0.0), ([0, 0, 255, 255], 1.0)],
                        kind: VGradientKind::Linear { x1: 0.0, y1: 0.0, x2: 108.0, y2: 0.0 },
                    }),
                    fill_alpha: 1.0,
                }],
            })),
        };
        let png = render_svg(&compose_svg(&icon), OUTPUT_SIZE).expect("renders");

        // Act / Assert
        let colors = distinct_opaque_colors(&png);
        assert!(colors >= 8, "gradient must yield many colors, got {colors}");
        let dominant = dominant_opaque_fraction(&png);
        assert!(
            dominant <= 0.90,
            "gradient must not be mono-dominant, got {dominant:.3}"
        );
    }

    #[test]
    fn test_dominant_opaque_fraction_mono_and_garbage() {
        // Arrange: solid-color canvas = 1.0; empty canvas = 1.0 (degenerate);
        // undecodable bytes = 0.0.
        let mut pm = tiny_skia::Pixmap::new(8, 8).expect("pixmap allocates");
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(255, 0, 0, 255);
        let rect = tiny_skia::Rect::from_xywh(0.0, 0.0, 8.0, 8.0).expect("rect fits");
        pm.fill_path(
            &tiny_skia::PathBuilder::from_rect(rect),
            &paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
        let solid = pm.encode_png().expect("encoding works");

        // Act / Assert
        assert!(
            (dominant_opaque_fraction(&solid) - 1.0).abs() < 1e-6,
            "solid canvas is fully mono-dominant"
        );
        let empty = tiny_skia::Pixmap::new(8, 8).expect("pixmap allocates");
        let empty_png = empty.encode_png().expect("encoding works");
        assert!(
            (dominant_opaque_fraction(&empty_png) - 1.0).abs() < 1e-6,
            "fully transparent canvas counts as degenerate"
        );
        assert_eq!(dominant_opaque_fraction(b"junk"), 0.0);
    }

    // ---------- theme attribute references (T-B) ----------

    #[test]
    fn test_parse_theme_attr_id_parses_numeric_and_rejects_named() {
        // Arrange/Act/Assert: AXML emits `?attr` refs as `?<hex id>`;
        // named forms (no numeric id) cannot be resolved and are rejected.
        assert_eq!(parse_theme_attr_id("?7f040123"), Some(0x7f040123));
        assert_eq!(parse_theme_attr_id("?01010033"), Some(0x01010033));
        assert_eq!(parse_theme_attr_id("?android:attr/colorPrimary"), None);
        assert_eq!(parse_theme_attr_id("?attr/colorPrimary"), None);
        assert_eq!(parse_theme_attr_id("#ffffff"), None);
        assert_eq!(parse_theme_attr_id(""), None);
    }

    // ---------- gradients (T-B) ----------

    fn gradient_el(attrs: &[(&str, &str)], items: &[(&str, &str)]) -> Element {
        let mut g = el("gradient", attrs);
        for (color, offset) in items {
            g.append_child(el("item", &[("color", color), ("offset", offset)]));
        }
        g
    }

    #[test]
    fn test_decode_gradient_linear_attrs_produce_stops_and_coords() {
        // Arrange: classic linear gradient with explicit viewport coordinates.
        let g = gradient_el(
            &[
                ("type", "linear"),
                ("startColor", "#ffff0000"),
                ("endColor", "#8000ff00"),
                ("startX", "0"),
                ("startY", "0"),
                ("endX", "108"),
                ("endY", "0"),
            ],
            &[],
        );

        // Act
        let grad = decode_gradient(&g, &NullResolver, (108.0, 108.0)).expect("linear gradient decodes");

        // Assert: stops are start@0 / end@1, coordinates preserved.
        match grad.kind {
            VGradientKind::Linear { x1, y1, x2, y2 } => {
                assert_eq!((x1, y1, x2, y2), (0.0, 0.0, 108.0, 0.0));
            }
            other => panic!("expected linear, got {other:?}"),
        }
        assert_eq!(grad.stops.len(), 2);
        assert_eq!(grad.stops[0], ([255, 0, 0, 255], 0.0));
        assert_eq!(grad.stops[1], ([0, 255, 0, 128], 1.0));
    }

    #[test]
    fn test_decode_gradient_item_children_become_stops() {
        // Arrange: aapt inline gradients use <item color offset> children.
        let g = gradient_el(
            &[("type", "linear"), ("angle", "0")],
            &[("#ff112233", "0"), ("#ff445566", "0.5"), ("#ffffffff", "1")],
        );

        // Act
        let grad = decode_gradient(&g, &NullResolver, (108.0, 108.0)).expect("item gradient decodes");

        // Assert
        assert_eq!(grad.stops.len(), 3);
        assert_eq!(grad.stops[0], ([0x11, 0x22, 0x33, 0xff], 0.0));
        assert_eq!(grad.stops[2], ([0xff, 0xff, 0xff, 0xff], 1.0));
    }

    #[test]
    fn test_decode_gradient_radial_uses_center_and_radius() {
        // Arrange: radial gradients (Teams-style) carry center + radius.
        let g = gradient_el(
            &[
                ("type", "radial"),
                ("centerX", "75.5"),
                ("centerY", "55"),
                ("gradientRadius", "103"),
                ("startColor", "#ff000000"),
                ("endColor", "#ffffffff"),
            ],
            &[],
        );

        // Act
        let grad = decode_gradient(&g, &NullResolver, (108.0, 108.0)).expect("radial gradient decodes");

        // Assert
        match grad.kind {
            VGradientKind::Radial { cx, cy, r } => assert_eq!((cx, cy, r), (75.5, 55.0, 103.0)),
            other => panic!("expected radial, got {other:?}"),
        }
    }

    #[test]
    fn test_decode_gradient_unsupported_variants_degrade_to_none() {
        // Arrange/Act/Assert: sweep type, radial without radius, and
        // gradients without any color source all degrade to None.
        let sweep = gradient_el(
            &[("type", "sweep"), ("startColor", "#ff000000"), ("endColor", "#ffffffff")],
            &[],
        );
        assert_eq!(decode_gradient(&sweep, &NullResolver, (108.0, 108.0)), None);

        let no_radius = gradient_el(
            &[("type", "radial"), ("startColor", "#ff000000"), ("endColor", "#ffffffff")],
            &[],
        );
        assert_eq!(decode_gradient(&no_radius, &NullResolver, (108.0, 108.0)), None);

        let no_colors = gradient_el(&[("type", "linear"), ("angle", "0")], &[]);
        assert_eq!(decode_gradient(&no_colors, &NullResolver, (108.0, 108.0)), None);
    }

    #[test]
    fn test_decode_path_gradient_fill_via_ref_resolves_sub_xml() {
        // Arrange: Teams-style fill ref `@drawable/$name` resolves through the
        // ARSC to a `res/*.xml` gradient sub-document; the reader provides its
        // serialized AXML bytes. A path element referencing it must decode to
        // a gradient fill.
        let mut root = el(
            "vector",
            &[("viewportWidth", "108"), ("viewportHeight", "108")],
        );
        root.append_child(el(
            "path",
            &[
                ("pathData", "M0,0 L10,10"),
                ("fillColor", "@drawable/$ic_launcher_foreground__0"),
            ],
        ));

        // A real AXML cannot be synthesized in-process; the fake reader
        // returns None, so the path must degrade (coverage of the ref
        // plumbing). The success path is covered by the Teams fixture test.
        let read = |_name: &str| -> Option<Vec<u8>> { None };

        // Act/Assert: unresolvable sub-document degrades the layer.
        assert_eq!(decode_vector(&root, &NullResolver, &read), None);
    }

    #[test]
    fn test_compose_svg_gradient_fill_emits_defs_and_url() {
        // Arrange: path with a linear gradient fill.
        let icon = fg_vector(
            (108.0, 108.0),
            vec![VNode::Path {
                path_data: "M0,0 L10,10".to_string(),
                fill: VFill::Gradient(VGradient {
                    stops: vec![([255, 0, 0, 255], 0.0), ([0, 0, 255, 255], 1.0)],
                    kind: VGradientKind::Linear { x1: 0.0, y1: 0.0, x2: 108.0, y2: 0.0 },
                }),
                fill_alpha: 1.0,
            }],
        );

        // Act
        let svg = compose_adaptive_svg(&icon);

        // Assert: gradient def in user space + url() fill on the path.
        assert!(svg.contains("<linearGradient"), "def emitted: {svg}");
        assert!(
            svg.contains("gradientUnits=\"userSpaceOnUse\""),
            "gradient in user space: {svg}"
        );
        assert!(svg.contains("x1=\"0\""), "start x: {svg}");
        assert!(svg.contains("x2=\"108\""), "end x: {svg}");
        assert!(svg.contains("fill=\"url(#g0)\""), "path references def: {svg}");
        assert!(
            svg.contains("<stop offset=\"0\" stop-color=\"#ff0000\""),
            "stop 0: {svg}"
        );
        assert!(
            svg.contains("<stop offset=\"1\" stop-color=\"#0000ff\""),
            "stop 1: {svg}"
        );
    }

    #[test]
    fn test_compose_svg_radial_gradient_fill_emits_radial_gradient() {
        // Arrange: path with a radial gradient fill (Teams case).
        let icon = fg_vector(
            (108.0, 108.0),
            vec![VNode::Path {
                path_data: "M0,0 L10,10".to_string(),
                fill: VFill::Gradient(VGradient {
                    stops: vec![([255, 0, 0, 255], 0.0), ([0, 0, 255, 255], 1.0)],
                    kind: VGradientKind::Radial { cx: 75.5, cy: 55.0, r: 103.0 },
                }),
                fill_alpha: 1.0,
            }],
        );

        // Act
        let svg = compose_adaptive_svg(&icon);

        // Assert
        assert!(svg.contains("<radialGradient"), "def emitted: {svg}");
        assert!(svg.contains("cx=\"75.5\""), "center x: {svg}");
        assert!(svg.contains("r=\"103\""), "radius: {svg}");
        assert!(svg.contains("fill=\"url(#g0)\""), "path references def: {svg}");
    }

    // ---------- clip-path (T-B) ----------

    #[test]
    fn test_decode_group_clip_path_is_captured() {
        // Arrange: clip-path inside a group clips that group's content.
        let mut group = el("group", &[]);
        group.append_child(el("clip-path", &[("pathData", "M0,0 L10,10")]));
        group.append_child(el(
            "path",
            &[("pathData", "M1,1 L2,2"), ("fillColor", "#ff000000")],
        ));
        let mut root = el(
            "vector",
            &[("viewportWidth", "24"), ("viewportHeight", "24")],
        );
        root.append_child(group);

        // Act
        let vd = decode_vector(&root, &NullResolver, &no_read_entry).expect("vector decodes");

        // Assert
        match &vd.nodes[0] {
            VNode::Group { clip, children, .. } => {
                assert_eq!(clip.as_deref(), Some("M0,0 L10,10"));
                assert_eq!(children.len(), 1);
            }
            other => panic!("expected group, got {other:?}"),
        }
    }

    #[test]
    fn test_compose_svg_group_clip_emits_clippath_def_and_ref() {
        // Arrange
        let icon = fg_vector(
            (108.0, 108.0),
            vec![VNode::Group {
                translate: (0.0, 0.0),
                rotation: 0.0,
                pivot: (0.0, 0.0),
                scale: (1.0, 1.0),
                clip: Some("M0,0 L10,10".to_string()),
                children: vec![VNode::Path {
                    path_data: "M1,1 L2,2".to_string(),
                    fill: VFill::Color([0, 0, 0, 255]),
                    fill_alpha: 1.0,
                }],
            }],
        );

        // Act
        let svg = compose_adaptive_svg(&icon);

        // Assert: clip def + group reference.
        assert!(
            svg.contains("<clipPath id=\"c0\"><path d=\"M0,0 L10,10\"/></clipPath>"),
            "clip def: {svg}"
        );
        assert!(svg.contains("clip-path=\"url(#c0)\""), "group ref: {svg}");
    }

    // ---------- shape drawables (T-B) ----------

    #[test]
    fn test_decode_shape_rounded_rectangle_becomes_vector() {
        // Arrange: launcher backgrounds are often <shape> rectangles.
        let mut root = el("shape", &[("shape", "rectangle")]);
        root.append_child(el("solid", &[("color", "#ff3366cc")]));
        root.append_child(el("corners", &[("radius", "12")]));

        // Act
        let vd = decode_shape(&root, &NullResolver).expect("shape decodes");

        // Assert: full-canvas 108 viewport, one filled path with rounded corners.
        assert_eq!(vd.viewport, (108.0, 108.0));
        assert_eq!(vd.nodes.len(), 1);
        match &vd.nodes[0] {
            VNode::Path { path_data, fill, .. } => {
                assert!(path_data.starts_with("M12,0"), "rounded rect path: {path_data}");
                assert!(path_data.contains("A12,12"), "arc corners: {path_data}");
                assert_eq!(*fill, VFill::Color([0x33, 0x66, 0xcc, 0xff]));
            }
            other => panic!("expected path, got {other:?}"),
        }
    }

    #[test]
    fn test_decode_shape_oval_becomes_ellipse_path() {
        // Arrange
        let mut root = el("shape", &[("shape", "oval")]);
        root.append_child(el("solid", &[("color", "#ffff0000")]));
        root.append_child(el("corners", &[("radius", "8")]));

        // Act
        let vd = decode_shape(&root, &NullResolver).expect("oval decodes");

        // Assert: two arcs spanning the full canvas; corners ignored.
        match &vd.nodes[0] {
            VNode::Path { path_data, .. } => {
                assert!(path_data.contains("A54,54"), "ellipse arcs: {path_data}");
                assert!(path_data.starts_with("M0,54"), "left-most point: {path_data}");
            }
            other => panic!("expected path, got {other:?}"),
        }
    }

    #[test]
    fn test_decode_shape_unsupported_variants_degrade_to_none() {
        // Arrange/Act/Assert: ring/line shapes and shapes without a solid
        // fill degrade to None.
        let ring = el("shape", &[("shape", "ring")]);
        assert_eq!(decode_shape(&ring, &NullResolver), None);

        let line = el("shape", &[("shape", "line")]);
        assert_eq!(decode_shape(&line, &NullResolver), None);

        let no_solid = el("shape", &[("shape", "rectangle")]);
        assert_eq!(decode_shape(&no_solid, &NullResolver), None);
    }

    // ---------- bitmap drawables + zip entry paths (T-B) ----------

    #[test]
    fn test_decode_bitmap_without_arsc_degrades() {
        // Arrange: <bitmap android:src="@mipmap/..."> needs the ARSC to
        // resolve the src reference; without it the layer degrades.
        let root = el("bitmap", &[("src", "@mipmap/ic_launcher")]);
        let read = |_name: &str| -> Option<Vec<u8>> { None };

        // Act/Assert
        assert_eq!(decode_bitmap(&root, &NullResolver, &read), None);
    }

    #[test]
    fn test_value_to_layer_reads_zip_entry_with_full_res_path() {
        // Arrange: ARSC values carry the full zip entry path ("res/..."),
        // which is exactly what the zip central-directory lookup needs.
        let read = |name: &str| {
            if name == "res/drawable/foo.png" {
                Some(vec![1u8, 2, 3])
            } else {
                None
            }
        };

        // Act/Assert
        assert_eq!(
            value_to_layer("res/drawable/foo.png", &read, &NullResolver, 0),
            Some(Layer::Raster(vec![1, 2, 3]))
        );
    }

    // ---------- reference-token resolution (T-D) ----------

    /// Mock resolver: by-id and by-name tables.
    struct MockResolver;
    impl ResourceResolver for MockResolver {
        fn get_resource_value(&self, id: u32) -> Option<String> {
            match id {
                0x7f0d0002 => Some("res/0pB.png".to_string()),
                _ => None,
            }
        }
        fn get_resource_value_by_name(&self, name: &str) -> Option<String> {
            match name {
                "mipmap/ic_launcher_fg" => Some("#ff112233".to_string()),
                _ => None,
            }
        }
        fn axml_arsc(&self) -> Option<&apk_info::ARSC> {
            None
        }
    }

    #[test]
    fn test_resolve_ref_token_named_form_resolves_by_name() {
        // Arrange/Act/Assert: `@type/name` (with optional package prefix).
        assert_eq!(
            resolve_ref_token("@mipmap/ic_launcher_fg", &MockResolver),
            Some("#ff112233".to_string())
        );
        assert_eq!(
            resolve_ref_token("@android:mipmap/ic_launcher_fg", &MockResolver),
            Some("#ff112233".to_string())
        );
    }

    #[test]
    fn test_resolve_ref_token_numeric_forms_resolve_by_id() {
        // Arrange/Act/Assert: hex-id fallbacks emitted by the AXML decoder.
        assert_eq!(
            resolve_ref_token("@7f0d0002", &MockResolver),
            Some("res/0pB.png".to_string())
        );
        assert_eq!(
            resolve_ref_token("@0x7f0d0002", &MockResolver),
            Some("res/0pB.png".to_string())
        );
        assert_eq!(
            resolve_ref_token("@ref/0x7f0d0002", &MockResolver),
            Some("res/0pB.png".to_string())
        );
    }

    #[test]
    fn test_resolve_ref_token_unresolvable_or_non_reference_returns_none() {
        // Arrange/Act/Assert: unknown names/ids and non-'@' tokens degrade.
        assert_eq!(resolve_ref_token("@mipmap/missing", &MockResolver), None);
        assert_eq!(resolve_ref_token("@7fdeadbeef", &MockResolver), None);
        assert_eq!(resolve_ref_token("res/no_prefix.png", &MockResolver), None);
        assert_eq!(resolve_ref_token("", &MockResolver), None);
    }

    // ---------- inline adaptive layers (T-D) ----------

    #[test]
    fn test_adaptive_layer_inline_bitmap_child_resolves_via_resolver() {
        // Arrange: Sheets-style `<background><bitmap android:src="@mipmap/x"/>
        // </background>` with no android:drawable attribute at all.
        let mut bg = el("background", &[]);
        bg.append_child(el("bitmap", &[("src", "@mipmap/ic_launcher_fg")]));
        let mut root = el("adaptive-icon", &[]);
        root.append_child(bg);
        let read = |_name: &str| -> Option<Vec<u8>> { None };

        // Act
        let layer = adaptive_layer(&root, "background", &MockResolver, &read, 0);

        // Assert: the src name resolves to a color literal in the mock.
        assert_eq!(layer, Some(Layer::Color([0x11, 0x22, 0x33, 0xff])));
    }

    #[test]
    fn test_adaptive_layer_drawable_attr_still_wins() {
        // Arrange: classic `android:drawable` reference form.
        let mut root = el("adaptive-icon", &[]);
        root.append_child(el("background", &[("drawable", "@mipmap/ic_launcher_fg")]));

        // Act
        let layer = adaptive_layer(&root, "background", &MockResolver, &no_read_entry, 0);

        // Assert
        assert_eq!(layer, Some(Layer::Color([0x11, 0x22, 0x33, 0xff])));
    }

    #[test]
    fn test_adaptive_layer_missing_tag_or_unsupported_child_degrades() {
        // Arrange/Act/Assert
        let root = el("adaptive-icon", &[]);
        assert_eq!(adaptive_layer(&root, "foreground", &MockResolver, &no_read_entry, 0), None);

        let mut bg = el("background", &[]);
        bg.append_child(el("animated-selector", &[]));
        let mut root2 = el("adaptive-icon", &[]);
        root2.append_child(bg);
        assert_eq!(adaptive_layer(&root2, "background", &MockResolver, &no_read_entry, 0), None);
    }
}
