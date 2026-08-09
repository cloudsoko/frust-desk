//! Frust UI — a Desk-local design system in the **frappe-ui** visual
//! language, built **Topcoat-native**: every component is a server-rendered
//! `#[component]` (Rust `view!` + a hand-authored token stylesheet). No Vue, no
//! SPA runtime, no second CSS toolchain — the Desk stays a pure renderer
//! and this layer is ours to own out-of-tree.
//!
//! This module is **additive**: it introduces a `frust-ui` component set, a
//! `/frust-ui.css` asset route, and one standalone `/ui-gallery` proof route.
//! It touches no existing Desk page handler.
//!
//! Tokens (`frust_ui.css`) are lifted verbatim from frappe-ui's published
//! design tokens (espresso-v2 primitives + semantic light/dark styles) and
//! resolved to concrete hex; see that file's header.
//!
//! Why a route (not a `#[page]`) for the gallery: layouts wrap pages by path
//! prefix, so a `#[page]` would inherit the Desk's `root_layout` chrome +
//! inline body styles. A `#[route]` is unwrapped, giving the gallery a clean
//! full-document canvas with its own `<head>` (stylesheet link, dark/light).

use topcoat::{
    Result,
    context::Cx,
    router::{content::Css, route},
    view::{NodeViewParts, PartsWriter, View, component, view},
};

/// The design-system stylesheet — tokens (light + dark) + component classes.
/// Embedded in the binary (Desk's no-external-asset posture) and
/// served at `/frust-ui.css` with `Content-Type: text/css`.
pub const FRUST_UI_CSS: &str = include_str!("frust_ui.css");

#[route(GET "/frust-ui.css")]
async fn frust_ui_css() -> Result<Css<&'static str>> {
    Ok(Css(FRUST_UI_CSS))
}

// ── Trusted raw markup (inline SVG icons) ───────────────────────────────────
//
// `view!` escapes interpolated text, so SVG source (`<`, `>`, camelCase
// `viewBox`, self-closing `<path/>`) cannot be interpolated as a string. This
// wrapper opts a value out of escaping via `push_str_unescaped`. It is ONLY
// ever handed compile-time `&'static str` constants defined below — never user
// input — so the "trusted markup" contract holds by construction.
struct Raw(&'static str);
impl NodeViewParts for Raw {
    fn into_view_parts(self, _cx: &Cx, parts: &mut PartsWriter<'_>) {
        parts.push_str_unescaped(self.0);
    }
}

/// Wraps icon body paths in the shared `<svg>` open/close (compile-time).
macro_rules! svg_icon {
    ($($body:literal),+ $(,)?) => {
        concat!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\" ",
            "viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" ",
            "stroke-width=\"2\" stroke-linecap=\"round\" stroke-linejoin=\"round\">",
            $($body),+,
            "</svg>"
        )
    };
}

/// lucide-style stroke icons (24×24, `stroke="currentColor"` so they inherit
/// the surrounding ink/feedback color). Returns `""` for an unknown name.
fn icon(name: &str) -> &'static str {
    match name {
        "plus" => svg_icon!("<path d=\"M5 12h14\"/><path d=\"M12 5v14\"/>"),
        "search" => svg_icon!("<circle cx=\"11\" cy=\"11\" r=\"8\"/><path d=\"m21 21-4.3-4.3\"/>"),
        "check" => svg_icon!("<path d=\"M20 6 9 17l-5-5\"/>"),
        "x" => svg_icon!("<path d=\"M18 6 6 18\"/><path d=\"m6 6 12 12\"/>"),
        "trash" => svg_icon!(
            "<path d=\"M3 6h18\"/><path d=\"M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6\"/>",
            "<path d=\"M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2\"/>"
        ),
        "info" => svg_icon!(
            "<circle cx=\"12\" cy=\"12\" r=\"10\"/><path d=\"M12 16v-4\"/>",
            "<path d=\"M12 8h.01\"/>"
        ),
        "success" => svg_icon!(
            "<path d=\"M22 11.08V12a10 10 0 1 1-5.93-9.14\"/><path d=\"m9 11 3 3L22 4\"/>"
        ),
        "warning" => svg_icon!(
            "<path d=\"m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3Z\"/>",
            "<path d=\"M12 9v4\"/><path d=\"M12 17h.01\"/>"
        ),
        "danger" => svg_icon!(
            "<circle cx=\"12\" cy=\"12\" r=\"10\"/><path d=\"M12 8v4\"/><path d=\"M12 16h.01\"/>"
        ),
        _ => "",
    }
}

// ── Button ──────────────────────────────────────────────────────────────────

/// `variant`: primary | secondary | ghost | accent | danger.
/// `size`: sm | md | lg. `icon`: a leading icon name (see [`icon`]) or "".
#[component]
pub async fn fui_button(
    #[into] label: String,
    #[default("secondary")] variant: &str,
    #[default("md")] size: &str,
    #[default] icon: &str,
    #[default] block: bool,
    #[default] disabled: bool,
    /// `button` | `submit`. The gallery only ever needed a dead
    /// button; a real Desk form needs to submit.
    #[default("button")]
    kind: &str,
    /// When set, renders an `<a>` styled as a button. The Desk's
    /// actions are forms and navigations — never JS click handlers. Owned,
    /// because every real destination is a `format!`ed path.
    #[default(String::new())]
    #[into]
    href: String,
    #[default] name: &str,
    #[default] value: &str,
) -> Result {
    let mut cls = format!("fui-btn fui-btn--{variant} fui-btn--{size}");
    if block {
        cls.push_str(" fui-btn--block");
    }
    let glyph = self::icon(icon);
    view! {
        if !href.is_empty() {
            <a class=(cls) href=(href)>
                if !glyph.is_empty() {
                    <span class="fui-btn__icon">(Raw(glyph))</span>
                }
                (label)
            </a>
        } else {
            <button type=(kind) class=(cls) name=(name) value=(value)
                if disabled { disabled="disabled" aria-disabled="true" }
            >
                if !glyph.is_empty() {
                    <span class="fui-btn__icon">(Raw(glyph))</span>
                }
                (label)
            </button>
        }
    }
}

// ── Inputs ──────────────────────────────────────────────────────────────────

#[component]
pub async fn fui_input(
    #[default] name: &str,
    #[default("text")] kind: &str,
    #[default] placeholder: &str,
    #[default(String::new())]
    #[into]
    value: String,
    #[default] invalid: bool,
    #[default] disabled: bool,
    #[default] required: bool,
    /// `<datalist>` id — the native typeahead affordance for
    /// the server-filtered combobox.
    #[default]
    list: &str,
    #[default] autofocus: bool,
) -> Result {
    let cls = if invalid {
        "fui-input fui-input--invalid"
    } else {
        "fui-input"
    };
    view! {
        <input class=(cls) type=(kind) name=(name) placeholder=(placeholder) value=(value)
            if !name.is_empty() { id=(name) }
            if !list.is_empty() { list=(list) }
            if required { required="required" }
            if autofocus { autofocus="autofocus" }
            if disabled { disabled="disabled" }
        >
    }
}

#[component]
pub async fn fui_textarea(
    #[default] name: &str,
    #[default] placeholder: &str,
    #[default] value: &str,
    #[default("3")] rows: &str,
    #[default] invalid: bool,
) -> Result {
    let cls = if invalid {
        "fui-textarea fui-textarea--invalid"
    } else {
        "fui-textarea"
    };
    view! {
        <textarea class=(cls) name=(name) placeholder=(placeholder) rows=(rows)
            if !name.is_empty() { id=(name) }
        >(value)</textarea>
    }
}

/// Options are supplied as child `<option>` nodes.
#[component]
pub async fn fui_select(#[default] name: &str, #[default] invalid: bool, child: View) -> Result {
    let cls = if invalid {
        "fui-select fui-select--invalid"
    } else {
        "fui-select"
    };
    view! {
        <select class=(cls) name=(name)
            if !name.is_empty() { id=(name) }
        >(child)</select>
    }
}

#[component]
pub async fn fui_checkbox(
    #[into] label: String,
    #[default] name: &str,
    #[default] checked: bool,
    #[default] disabled: bool,
) -> Result {
    view! {
        <label class="fui-check">
            <input type="checkbox" name=(name)
                if checked { checked="checked" }
                if disabled { disabled="disabled" }
            >
            <span>(label)</span>
        </label>
    }
}

// ── Badge ───────────────────────────────────────────────────────────────────

/// `color`: gray | blue | green | red | amber. `style`: subtle | solid | outline.
#[component]
pub async fn fui_badge(
    #[into] label: String,
    #[default("gray")] color: &str,
    #[default("subtle")] style: &str,
    #[default] dot: bool,
    #[default] pill: bool,
) -> Result {
    let mut cls = format!("fui-badge fui-badge--{color}");
    if style != "subtle" {
        cls.push_str(&format!(" fui-badge--{style}"));
    }
    if pill {
        cls.push_str(" fui-badge--pill");
    }
    view! {
        <span class=(cls)>
            if dot { <span class="fui-badge__dot"></span> }
            (label)
        </span>
    }
}

// ── Card / Panel ────────────────────────────────────────────────────────────

#[component]
pub async fn fui_card(
    #[into] title: String,
    #[default(View::empty())] actions: View,
    child: View,
) -> Result {
    view! {
        <div class="fui-card">
            <div class="fui-card__header">
                <span class="fui-card__title">(title)</span>
                <span class="fui-card__actions">(actions)</span>
            </div>
            <div class="fui-card__body">(child)</div>
        </div>
    }
}

// ── FormControl ─────────────────────────────────────────────────────────────

#[component]
pub async fn fui_form_control(
    #[into] label: String,
    #[default] for_id: &str,
    #[default] required: bool,
    #[default] description: &str,
    #[default] error: &str,
    child: View,
) -> Result {
    view! {
        <div class="fui-field">
            <label class="fui-field__label"
                if !for_id.is_empty() { for=(for_id) }
            >
                (label)
                if required { <span class="fui-field__req">"*"</span> }
            </label>
            (child)
            if !description.is_empty() && error.is_empty() {
                <p class="fui-field__desc">(description)</p>
            }
            if !error.is_empty() {
                <p class="fui-field__error">(error)</p>
            }
        </div>
    }
}

// ── ListRow ─────────────────────────────────────────────────────────────────

#[component]
pub async fn fui_list_row(
    #[into] title: String,
    #[default] meta: &str,
    #[default] avatar: &str,
    #[default(View::empty())] trailing: View,
) -> Result {
    view! {
        <div class="fui-listrow">
            if !avatar.is_empty() {
                <span class="fui-listrow__lead">
                    <span class="fui-listrow__avatar">(avatar)</span>
                </span>
            }
            <div class="fui-listrow__main">
                <div class="fui-listrow__title">(title)</div>
                if !meta.is_empty() {
                    <div class="fui-listrow__meta">(meta)</div>
                }
            </div>
            <div class="fui-listrow__trailing">(trailing)</div>
        </div>
    }
}

// ── Alert / Toast ───────────────────────────────────────────────────────────

/// `variant`: info | success | warning | danger.
#[component]
pub async fn fui_alert(
    #[default("info")] variant: &str,
    #[into] title: String,
    child: View,
) -> Result {
    let cls = format!("fui-alert fui-alert--{variant}");
    let glyph = icon(variant);
    view! {
        <div class=(cls) role="alert">
            <span class="fui-alert__icon">(Raw(glyph))</span>
            <div class="fui-alert__body">
                <div class="fui-alert__title">(title)</div>
                <div class="fui-alert__text">(child)</div>
            </div>
        </div>
    }
}

#[component]
pub async fn fui_toast(
    #[default("success")] variant: &str,
    #[into] title: String,
    child: View,
) -> Result {
    let cls = format!("fui-alert fui-toast fui-alert--{variant}");
    let glyph = icon(variant);
    view! {
        <div class=(cls) role="status">
            <span class="fui-alert__icon">(Raw(glyph))</span>
            <div class="fui-alert__body">
                <div class="fui-alert__title">(title)</div>
                <div class="fui-alert__text">(child)</div>
            </div>
        </div>
    }
}

// ── Dialog / Modal (surface — inline preview for the gallery) ────────────────

#[component]
pub async fn fui_dialog(#[into] title: String, #[default] message: &str, child: View) -> Result {
    view! {
        <div class="fui-dialog">
            <div class="fui-dialog__header">
                <span class="fui-dialog__title">(title)</span>
                <button type="button" class="fui-dialog__close" aria-label="Close">
                    <span class="fui-btn__icon">(Raw(icon("x")))</span>
                </button>
            </div>
            if !message.is_empty() {
                <div class="fui-dialog__body">(message)</div>
            }
            <div class="fui-dialog__footer">(child)</div>
        </div>
    }
}

// ── Gallery pane: every component, in one theme ─────────────────────────────

#[component]
async fn gallery_pane(#[default("light")] theme: &str, #[into] label: String) -> Result {
    view! {
        <div class="fui-pane fui-root" data-theme=(theme)>
            <div class="fui-pane__label">(label)</div>
            <div class="fui-pane__body">

                <div class="fui-section">
                    <h3>"Buttons"</h3>
                    <div class="fui-row">
                        fui_button(label: "Primary", variant: "primary")
                        fui_button(label: "Secondary", variant: "secondary")
                        fui_button(label: "Ghost", variant: "ghost")
                        fui_button(label: "Accent", variant: "accent")
                        fui_button(label: "Delete", variant: "danger", icon: "trash")
                    </div>
                    <div class="fui-row" style="margin-top:12px;">
                        fui_button(label: "New", variant: "primary", size: "lg", icon: "plus")
                        fui_button(label: "Search", variant: "secondary", size: "md", icon: "search")
                        fui_button(label: "Small", variant: "secondary", size: "sm")
                        fui_button(label: "Disabled", variant: "primary", disabled: true)
                    </div>
                </div>

                <div class="fui-section">
                    <h3>"Badges"</h3>
                    <div class="fui-row">
                        fui_badge(label: "Draft", color: "gray")
                        fui_badge(label: "Open", color: "blue")
                        fui_badge(label: "Approved", color: "green")
                        fui_badge(label: "Overdue", color: "red")
                        fui_badge(label: "Pending", color: "amber")
                    </div>
                    <div class="fui-row" style="margin-top:10px;">
                        fui_badge(label: "Live", color: "green", dot: true, pill: true)
                        fui_badge(label: "Solid", color: "blue", style: "solid")
                        fui_badge(label: "Outline", color: "red", style: "outline")
                        fui_badge(label: "Solid", color: "gray", style: "solid", pill: true)
                    </div>
                </div>

                <div class="fui-section">
                    <h3>"Form controls"</h3>
                    <div class="fui-stack">
                        fui_form_control(
                            label: "Full name",
                            for_id: "name",
                            required: true,
                            description: "As it appears on official documents.",
                            fui_input(name: "name", placeholder: "Jane Cooper")
                        )
                        fui_form_control(
                            label: "Priority",
                            for_id: "priority",
                            fui_select(
                                name: "priority",
                                <option>"Low"</option>
                                <option>"Medium"</option>
                                <option>"High"</option>
                            )
                        )
                        fui_form_control(
                            label: "Amount",
                            for_id: "amount",
                            error: "Enter a value greater than zero.",
                            fui_input(name: "amount", value: "0.00", invalid: true)
                        )
                        fui_form_control(
                            label: "Notes",
                            for_id: "notes",
                            fui_textarea(name: "notes", placeholder: "Add a note…", rows: "3")
                        )
                        fui_checkbox(label: "Email me a copy of this receipt", checked: true)
                        fui_checkbox(label: "Subscribe to weekly digest (disabled)", disabled: true)
                    </div>
                </div>

                <div class="fui-section">
                    <h3>"Card"</h3>
                    fui_card(
                        title: "Invoice INV-0042",
                        actions: view! { fui_button(label: "Edit", variant: "secondary", size: "sm") }?,
                        <div class="fui-row" style="justify-content:space-between;">
                            <span class="fui-muted">"Billed to Acme Corp · Due 30 Jul"</span>
                            fui_badge(label: "Unpaid", color: "amber")
                        </div>
                        <hr style="margin:14px 0;">
                        <div class="fui-row" style="justify-content:space-between;">
                            <span>"Total"</span>
                            <strong>"AR 101.96"</strong>
                        </div>
                    )
                </div>

                <div class="fui-section">
                    <h3>"List rows"</h3>
                    <div class="fui-list">
                        fui_list_row(
                            title: "Purchase Order PO-1001",
                            meta: "Raised by clerk · 2h ago",
                            avatar: "PO",
                            trailing: view! { fui_badge(label: "Draft", color: "gray") }?
                        )
                        fui_list_row(
                            title: "Travel Claim TC-2043",
                            meta: "Awaiting manager approval",
                            avatar: "TC",
                            trailing: view! { fui_badge(label: "Open", color: "blue") }?
                        )
                        fui_list_row(
                            title: "Expense EXP-9915",
                            meta: "Approved · posted to ledger",
                            avatar: "EX",
                            trailing: view! { fui_badge(label: "Done", color: "green", dot: true) }?
                        )
                    </div>
                </div>

                <div class="fui-section">
                    <h3>"Alerts & toast"</h3>
                    <div class="fui-stack fui-stack--tight">
                        fui_alert(variant: "info", title: "Heads up",
                            "This document is read-only while it is locked.")
                        fui_alert(variant: "success", title: "Saved",
                            "Your changes were written to the ledger.")
                        fui_alert(variant: "warning", title: "Rollup is stale",
                            "Figures reflect the last worker pass, not this second.")
                        fui_alert(variant: "danger", title: "Rejected by a validation rule",
                            "Amount must be greater than zero.")
                        fui_toast(variant: "success", title: "Document submitted",
                            "PO-1001 moved to Approved.")
                    </div>
                </div>

                <div class="fui-section">
                    <h3>"Dialog"</h3>
                    <div class="fui-dialog-preview">
                        fui_dialog(
                            title: "Delete this document?",
                            message: "This cannot be undone. The record and its history will be removed from the list.",
                            fui_button(label: "Cancel", variant: "secondary", size: "sm")
                            fui_button(label: "Delete", variant: "danger", size: "sm", icon: "trash")
                        )
                    </div>
                </div>

            </div>
        </div>
    }
}

// ── The gallery route (standalone full document; no Desk layout) ────────────

#[route(GET "/ui-gallery")]
async fn ui_gallery(cx: &Cx) -> Result {
    view! { cx =>
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8">
                <meta name="viewport" content="width=device-width, initial-scale=1">
                <title>"Frust UI — Gallery"</title>
                <link rel="preconnect" href="https://fonts.googleapis.com">
                <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin="crossorigin">
                <link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&display=swap">
                <link rel="stylesheet" href="/frust-ui.css">
            </head>
            <body class="fui-gallery">
                <div class="fui-gallery__bar">
                    <h1>"Frust UI"</h1>
                    <span class="fui-gallery__sub">"frappe-ui language · Topcoat-native · Frust UI foundation"</span>
                </div>
                <div class="fui-gallery__grid">
                    gallery_pane(theme: "light", label: "Light")
                    gallery_pane(theme: "dark", label: "Dark")
                </div>
            </body>
        </html>
    }
}

#[cfg(test)]
mod tests {
    use crate::brand::BRAND_TOKEN_NAMES;

    // ── The CSS-seam guard ──────────────────────────────────────────────────
    //
    // The Rust->CSS seam has NO type system: a class name or custom property
    // that does not exist compiles, renders *nearly* right, and says nothing.
    // It has bitten repeatedly — an invented spacing/font token once drew the
    // first paint in Times New Roman, and `fui-alert--error` drew every error
    // flash with no colour and no icon. So the check is wired in here where
    // it cannot be skipped, the same move the surql/tenancy monopolies made.

    const CSS: &str = include_str!("frust_ui.css");
    const UI_RS: &str = include_str!("frust_ui.rs");
    const PAGES_RS: &str = include_str!("pages.rs");

    /// Every `--fui-*` the stylesheet READS must be one it also DEFINES.
    #[test]
    fn every_custom_property_referenced_is_defined() {
        let defined = defined_properties(CSS);
        let mut missing: Vec<&str> = referenced_properties(CSS)
            .into_iter()
            .filter(|p| !defined.contains(p))
            .collect();
        missing.sort_unstable();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "custom properties referenced but never defined: {missing:?}\n\
             (a `var(--typo)` silently falls back to nothing — the Rust->CSS seam has no type system)"
        );
    }

    #[test]
    fn every_brand_override_targets_a_defined_custom_property() {
        let defined = defined_properties(CSS);
        let missing: Vec<&str> = BRAND_TOKEN_NAMES
            .iter()
            .copied()
            .filter(|token| !defined.contains(token))
            .collect();
        assert!(
            missing.is_empty(),
            "brand settings target tokens absent from the static stylesheet: {missing:?}"
        );
    }

    /// Every component VARIANT the Rust passes as a literal must be a modifier
    /// **that component** defines.
    ///
    /// This is the shape that actually bit: variants are composed at runtime
    /// (`format!("fui-btn fui-btn--{variant}")`), so no compiler and no static
    /// class scan can see them — only comparing each literal call site against
    /// its own component's defined modifiers can.
    ///
    /// **Bound to the component on purpose.** The first cut of this guard
    /// accepted a value if ANY component defined it, and that made it
    /// decorative: the real bug was `fui-btn--solid`, and `.fui-badge--solid`
    /// exists — so the permissive version passed the planted bug. A guard that
    /// cannot fail on the defect it was written for is worse than none.
    #[test]
    fn every_component_variant_literal_has_a_class() {
        let mut bad = Vec::new();
        // The Rust function name and the CSS class STEM differ (`fui_button`
        // renders `.fui-btn--*`), so the pairing is stated explicitly. Deriving
        // it — `component.replace('-', "_")` — produced `fui_btn`, which matches
        // no call site, so the button half silently checked NOTHING and passed
        // the planted bug. Found only by planting it.
        for (component, fn_name, param) in [
            ("fui-btn", "fui_button", "variant"),
            ("fui-alert", "fui_alert", "variant"),
            ("fui-badge", "fui_badge", "color"),
        ] {
            let defined = defined_modifiers(CSS, component);
            assert!(
                !defined.is_empty(),
                "no `.{component}--*` classes found — did the CSS move?"
            );
            for src in [UI_RS, PAGES_RS] {
                for value in call_site_args(src, fn_name, param) {
                    if !defined.contains(&value) {
                        bad.push(format!(
                            "{fn_name}({param}: {value:?}) — no `.{component}--{value}` in the stylesheet"
                        ));
                    }
                }
            }
        }
        bad.sort();
        bad.dedup();
        assert!(
            bad.is_empty(),
            "component variants passed from Rust with no matching CSS class:\n  {}\n\
             (these COMPILE and render nearly-right — `fui-alert--error` shipped undetected, \
             drawing every Desk error with no colour and no icon)",
            bad.join("\n  ")
        );
    }

    // ── the guard's own parsers, kept simple and testable ──

    fn defined_properties(css: &str) -> std::collections::HashSet<&str> {
        css.lines()
            .filter_map(|l| {
                let t = l.trim();
                let name = t.strip_prefix("--")?;
                let end = name.find(':')?;
                Some(&t[..end + 2])
            })
            .collect()
    }

    fn referenced_properties(css: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut rest = css;
        while let Some(i) = rest.find("var(--") {
            let after = &rest[i + 4..];
            let end = after
                .find(|c: char| c == ',' || c == ')' || c.is_whitespace())
                .unwrap_or(after.len());
            out.push(&after[..end]);
            rest = &after[end..];
        }
        out
    }

    fn selector_block<'a>(css: &'a str, selector: &str) -> &'a str {
        let selector_start = css
            .find(selector)
            .unwrap_or_else(|| panic!("missing selector {selector}"));
        let open = css[selector_start..]
            .find('{')
            .map(|offset| selector_start + offset)
            .unwrap_or_else(|| panic!("selector {selector} has no declaration block"));
        let mut depth = 0usize;
        for (offset, ch) in css[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &css[open + 1..open + offset];
                    }
                }
                _ => {}
            }
        }
        panic!("selector {selector} has an unterminated declaration block")
    }

    fn declared_properties(block: &str) -> std::collections::BTreeMap<&str, &str> {
        block
            .lines()
            .filter_map(|line| {
                let declaration = line.trim().strip_prefix("--")?;
                let (name, value) = declaration.split_once(':')?;
                Some((name, value.trim().trim_end_matches(';')))
            })
            .collect()
    }

    #[test]
    fn preferred_dark_tokens_match_explicit_dark_tokens() {
        let explicit = declared_properties(selector_block(CSS, "\n[data-theme=\"dark\"] {"));
        let preferred = declared_properties(selector_block(
            CSS,
            "\n  :root:not([data-theme=\"light\"]) {",
        ));
        assert!(
            !explicit.is_empty(),
            "explicit dark theme defines no tokens"
        );
        assert_eq!(
            preferred, explicit,
            "OS-driven dark mode must redeclare the complete explicit dark token set"
        );
    }

    fn defined_modifiers(css: &str, component: &str) -> std::collections::HashSet<String> {
        let needle = format!(".{component}--");
        let mut out = std::collections::HashSet::new();
        let mut rest = css;
        while let Some(i) = rest.find(&needle) {
            let after = &rest[i + needle.len()..];
            let end = after
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                .unwrap_or(after.len());
            if end > 0 {
                out.insert(after[..end].to_string());
            }
            rest = &after[end..];
        }
        out
    }

    /// `param: "value"` literals that appear inside a `fui_x(...)` call.
    ///
    /// Scans from each call's opening paren to its matching close, so a
    /// `variant:` belonging to a *different* component on a nearby line cannot
    /// be mistaken for this one's.
    fn call_site_args(src: &str, fn_name: &str, param: &str) -> Vec<String> {
        let mut out = Vec::new();
        let needle = format!("{fn_name}(");
        let mut from = 0;
        while let Some(i) = src[from..].find(&needle) {
            let open = from + i + needle.len();
            // find the matching close paren
            let mut depth = 1usize;
            let mut end = open;
            for (off, c) in src[open..].char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = open + off;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let args = &src[open..end.max(open)];
            let pneedle = format!("{param}: \"");
            if let Some(j) = args.find(&pneedle) {
                let after = &args[j + pneedle.len()..];
                if let Some(k) = after.find('"') {
                    out.push(after[..k].to_string());
                }
            }
            from = open;
        }
        out
    }

}
