use crate::{Session, kernel};
use topcoat::{context::Cx, view::{NodeViewParts, PartsWriter}};

// ── Tenant brand settings ──────────────────────────────────────────────────

pub(crate) const BRAND_SETTINGS: &str = "brand_settings";

const PRIMARY_COLORS: &[&str] = &["#171717", "#1d4ed8", "#047857", "#7c3aed", "#be123c"];
pub(crate) const ACCENT_COLORS: &[&str] = &[
    "#0d8ef8", "#2563eb", "#059669", "#7c3aed", "#db2777", "#d97706",
];
const RADIUS_SCALES: &[&str] = &["compact", "comfortable", "rounded"];
const FONT_FAMILIES: &[&str] = &["Inter", "System UI", "Georgia", "Verdana"];

/// One complete accent token family per accent option.
///
/// The static stylesheet's accent is not one variable but a family — a surface,
/// a hover/active `strong`, an `ink` for text/links, two tinted surfaces, an
/// outline, and a focus ring. Overriding only the surface and ink left the rest
/// static blue, so a pink accent rendered pink buttons with blue hovers and blue
/// focus rings. Because the accent vocabulary is CLOSED, each option carries a
/// fully precomputed family here rather than deriving shades with runtime color
/// math (a dependency, or hand-rolled arithmetic, for six known values).
///
/// Every `--fui-blue-ink` clears WCAG AA (>=4.5:1 contrast) on the default white
/// surface; the raw accent hues do not (the amber surface is only 3.19:1), which
/// is why `ink` is a separate, darker value than the surface. The contrast floor
/// and the family-completeness invariant are both asserted in the tests, which
/// recompute the ratio from relative luminance.
///
/// Position 0 of each family is `("--fui-blue", <the accent option>)`, so the
/// option a stored row selects is the family's own lookup key.
pub(crate) const ACCENT_FAMILIES: &[[(&str, &str); 7]] = &[
    [
        ("--fui-blue", "#0d8ef8"),
        ("--fui-blue-strong", "#077ddf"),
        ("--fui-blue-ink", "#0e6fbf"),
        ("--fui-blue-surface-1", "#f1f8fe"),
        ("--fui-blue-surface-2", "#e6f4ff"),
        ("--fui-blue-outline", "#b5ddfd"),
        ("--fui-focus-ring", "rgba(13,142,248,0.30)"),
    ],
    [
        ("--fui-blue", "#2563eb"),
        ("--fui-blue-strong", "#1d4ed8"),
        ("--fui-blue-ink", "#1d4ed8"),
        ("--fui-blue-surface-1", "#eff4ff"),
        ("--fui-blue-surface-2", "#dbe6fe"),
        ("--fui-blue-outline", "#bfd0fb"),
        ("--fui-focus-ring", "rgba(37,99,235,0.30)"),
    ],
    [
        ("--fui-blue", "#059669"),
        ("--fui-blue-strong", "#047857"),
        ("--fui-blue-ink", "#047857"),
        ("--fui-blue-surface-1", "#eefbf4"),
        ("--fui-blue-surface-2", "#d5f5e3"),
        ("--fui-blue-outline", "#a7e8c6"),
        ("--fui-focus-ring", "rgba(5,150,105,0.30)"),
    ],
    [
        ("--fui-blue", "#7c3aed"),
        ("--fui-blue-strong", "#6d28d9"),
        ("--fui-blue-ink", "#6d28d9"),
        ("--fui-blue-surface-1", "#f6f2fe"),
        ("--fui-blue-surface-2", "#ede4fd"),
        ("--fui-blue-outline", "#d3bdf8"),
        ("--fui-focus-ring", "rgba(124,58,237,0.30)"),
    ],
    [
        ("--fui-blue", "#db2777"),
        ("--fui-blue-strong", "#be185d"),
        ("--fui-blue-ink", "#be185d"),
        ("--fui-blue-surface-1", "#fdf2f8"),
        ("--fui-blue-surface-2", "#fbe0ee"),
        ("--fui-blue-outline", "#f6b6d6"),
        ("--fui-focus-ring", "rgba(219,39,119,0.30)"),
    ],
    [
        ("--fui-blue", "#d97706"),
        ("--fui-blue-strong", "#b45309"),
        ("--fui-blue-ink", "#b45309"),
        ("--fui-blue-surface-1", "#fdf6ec"),
        ("--fui-blue-surface-2", "#fcecd0"),
        ("--fui-blue-outline", "#f6d99a"),
        ("--fui-focus-ring", "rgba(217,119,6,0.30)"),
    ],
];

/// The accent option a stored row selected keys into its precomputed family.
pub(crate) fn accent_family(accent: &str) -> Option<&'static [(&'static str, &'static str); 7]> {
    ACCENT_FAMILIES.iter().find(|family| family[0].1 == accent)
}

#[cfg(test)]
pub(crate) const BRAND_TOKEN_NAMES: &[&str] = &[
    "--fui-primary-bg",
    "--fui-blue",
    "--fui-blue-strong",
    "--fui-blue-ink",
    "--fui-blue-surface-1",
    "--fui-blue-surface-2",
    "--fui-blue-outline",
    "--fui-focus-ring",
    "--fui-radius-sm",
    "--fui-radius",
    "--fui-radius-md",
    "--fui-radius-lg",
    "--fui-font",
    "--fui-font-sans",
    "--fui-brand-logo-image",
    "--fui-brand-logo-display",
];

fn brand_select_options(values: &[&str]) -> Vec<String> {
    std::iter::once("")
        .chain(values.iter().copied())
        .map(str::to_string)
        .collect()
}

/// This schema is installed at runtime through the ordinary metadata door.
/// The four closed choices are Select fields, so the database schema compiles
/// each vocabulary to an `ASSERT ... INSIDE [...]`. The logo is a typed Data
/// value and only enters CSS after the fixed URL mapper accepts its shape.
/// Empty means "inherit the static stylesheet" for every field.
pub(crate) fn brand_settings_meta() -> serde_json::Value {
    serde_json::json!({
        "name": BRAND_SETTINGS,
        "label": "Brand Settings",
        "issingle": true,
        "fields": [
            {
                "fieldname": "primary_color",
                "label": "Primary color",
                "fieldtype": "Select",
                "options": brand_select_options(PRIMARY_COLORS)
            },
            {
                "fieldname": "accent_color",
                "label": "Accent color",
                "fieldtype": "Select",
                "options": brand_select_options(ACCENT_COLORS)
            },
            {
                "fieldname": "corner_radius_scale",
                "label": "Corner radius scale",
                "fieldtype": "Select",
                "options": brand_select_options(RADIUS_SCALES)
            },
            {
                "fieldname": "font_family_name",
                "label": "Font family",
                "fieldtype": "Select",
                "options": brand_select_options(FONT_FAMILIES)
            },
            {
                "fieldname": "logo_url",
                "label": "Logo URL",
                "fieldtype": "Data"
            }
        ]
    })
}

fn selected<'a>(row: &'a serde_json::Value, field: &str, vocabulary: &[&str]) -> Option<&'a str> {
    let value = row.get(field)?.as_str()?;
    vocabulary.contains(&value).then_some(value)
}

fn logo_url(row: &serde_json::Value) -> Option<&str> {
    let value = row.get("logo_url")?.as_str()?;
    if value.is_empty()
        || value.len() > 2048
        || !value.is_ascii()
        || !(value.starts_with("https://") || (value.starts_with('/') && !value.starts_with("//")))
        || !value.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '-' | '.'
                        | '_'
                        | '~'
                        | ':'
                        | '/'
                        | '?'
                        | '#'
                        | '['
                        | ']'
                        | '@'
                        | '!'
                        | '$'
                        | '&'
                        | '*'
                        | '+'
                        | ','
                        | '='
                        | '%'
                )
        })
    {
        return None;
    }
    Some(value)
}

/// Opts an OWNED, kernel-validated string out of `view!`'s HTML escaping.
///
/// `view!` escapes interpolated text (`&` -> `&amp;`, `<`, `>`), so a logo URL
/// with a query string interpolated normally renders `url("…?v=2&amp;c=1")` — a
/// broken declaration the browser ignores. Unlike `frust_ui`'s compile-time
/// `Raw` (which only ever carries `&'static` SVG constants), this holds a runtime
/// `String`; the ONLY value it is handed is the output of `brand_style_from_row`,
/// and INVARIANT: every byte of that value is a closed-vocabulary token name, a
/// hex from a fixed list, a named radius or font bundle, or a logo URL the strict
/// mapper already accepted at the write door. Raw rendering of
/// write-door-validated data is therefore safe by construction — there is no
/// untrusted text on this path to escape.
pub(crate) struct RawCss(pub(crate) String);

impl NodeViewParts for RawCss {
    fn into_view_parts(self, _cx: &Cx, parts: &mut PartsWriter<'_>) {
        parts.push_str_unescaped(self.0);
    }
}

/// Map stored brand data onto the fixed token vocabulary. Colors are emitted
/// byte-for-byte, named radius and font choices select fixed bundles, and an
/// accepted logo URL can only occupy the fixed `url("...")` value slot.
pub(crate) fn brand_style_from_row(row: &serde_json::Value) -> Option<String> {
    let mut declarations = Vec::new();

    if let Some(value) = selected(row, "primary_color", PRIMARY_COLORS) {
        declarations.push(format!("--fui-primary-bg:{value};"));
    }
    if let Some(family) = selected(row, "accent_color", ACCENT_COLORS).and_then(accent_family) {
        for &(token, value) in family {
            declarations.push(format!("{token}:{value};"));
        }
    }
    match selected(row, "corner_radius_scale", RADIUS_SCALES) {
        Some("compact") => declarations.extend([
            "--fui-radius-sm:2px;".into(),
            "--fui-radius:4px;".into(),
            "--fui-radius-md:6px;".into(),
            "--fui-radius-lg:8px;".into(),
        ]),
        Some("comfortable") => declarations.extend([
            "--fui-radius-sm:4px;".into(),
            "--fui-radius:6px;".into(),
            "--fui-radius-md:8px;".into(),
            "--fui-radius-lg:12px;".into(),
        ]),
        Some("rounded") => declarations.extend([
            "--fui-radius-sm:8px;".into(),
            "--fui-radius:10px;".into(),
            "--fui-radius-md:14px;".into(),
            "--fui-radius-lg:18px;".into(),
        ]),
        _ => {}
    }
    let font = match selected(row, "font_family_name", FONT_FAMILIES) {
        Some("Inter") => Some("Inter,InterVar,system-ui,sans-serif"),
        Some("System UI") => Some("system-ui,sans-serif"),
        Some("Georgia") => Some("Georgia,serif"),
        Some("Verdana") => Some("Verdana,sans-serif"),
        _ => None,
    };
    if let Some(value) = font {
        declarations.push(format!("--fui-font:{value};"));
        declarations.push("--fui-font-sans:var(--fui-font);".into());
    }
    if let Some(value) = logo_url(row) {
        declarations.push(format!("--fui-brand-logo-image:url(\"{value}\");"));
        declarations.push("--fui-brand-logo-display:inline-block;".into());
    }

    (!declarations.is_empty()).then(|| format!(":root.fui-root{{{}}}", declarations.concat()))
}

pub(crate) async fn brand_style(s: Option<&Session>) -> Option<String> {
    // Exactly one kernel request per authenticated page. There is no Desk-side
    // cache generation to key safely, so caching here would make a saved brand
    // stale; unauthenticated pages cost zero requests.
    let s = s?;
    let _permit = kernel::admit()?;
    let (code, body) = kernel::call_async(
        Some(&s.token),
        "/single/brand_settings",
        &serde_json::json!({}),
    )
    .await;
    (code == 200)
        .then(|| brand_style_from_row(&body["row"]))
        .flatten()
}

