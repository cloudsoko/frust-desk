//! Frust UI (WO-037) — a Desk-local design system in the **frappe-ui** visual
//! language, built **Topcoat-native**: every component is a server-rendered
//! `#[component]` (Rust `view!` + a hand-authored token stylesheet). No Vue, no
//! SPA runtime, no second CSS toolchain — the Desk stays a pure renderer
//! (ADR-004) and this layer is ours to own out-of-tree (WO-012).
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
/// Embedded in the binary (Desk's no-external-asset posture, WO-009) and
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
    /// WO-042: `button` | `submit`. The gallery only ever needed a dead
    /// button; a real Desk form needs to submit.
    #[default("button")] kind: &str,
    /// WO-042: when set, renders an `<a>` styled as a button. The Desk's
    /// actions are forms and navigations — never JS click handlers. Owned,
    /// because every real destination is a `format!`ed path.
    #[default(String::new())] #[into] href: String,
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
    #[default(String::new())] #[into] value: String,
    #[default] invalid: bool,
    #[default] disabled: bool,
    #[default] required: bool,
    /// WO-042: `<datalist>` id — the native typeahead affordance for
    /// behaviour 3's server-filtered combobox.
    #[default] list: &str,
    #[default] autofocus: bool,
) -> Result {
    let cls = if invalid { "fui-input fui-input--invalid" } else { "fui-input" };
    view! {
        <input class=(cls) type=(kind) name=(name) placeholder=(placeholder) value=(value)
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
    let cls = if invalid { "fui-textarea fui-textarea--invalid" } else { "fui-textarea" };
    view! {
        <textarea class=(cls) name=(name) placeholder=(placeholder) rows=(rows)>(value)</textarea>
    }
}

/// Options are supplied as child `<option>` nodes.
#[component]
pub async fn fui_select(#[default] name: &str, #[default] invalid: bool, child: View) -> Result {
    let cls = if invalid { "fui-select fui-select--invalid" } else { "fui-select" };
    view! {
        <select class=(cls) name=(name)>(child)</select>
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
    #[default] required: bool,
    #[default] description: &str,
    #[default] error: &str,
    child: View,
) -> Result {
    view! {
        <div class="fui-field">
            <label class="fui-field__label">
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
pub async fn fui_alert(#[default("info")] variant: &str, #[into] title: String, child: View) -> Result {
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
pub async fn fui_toast(#[default("success")] variant: &str, #[into] title: String, child: View) -> Result {
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
                            required: true,
                            description: "As it appears on official documents.",
                            fui_input(name: "name", placeholder: "Jane Cooper")
                        )
                        fui_form_control(
                            label: "Priority",
                            fui_select(
                                name: "priority",
                                <option>"Low"</option>
                                <option>"Medium"</option>
                                <option>"High"</option>
                            )
                        )
                        fui_form_control(
                            label: "Amount",
                            error: "Enter a value greater than zero.",
                            fui_input(name: "amount", value: "0.00", invalid: true)
                        )
                        fui_form_control(
                            label: "Notes",
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
                    <span class="fui-gallery__sub">"frappe-ui language · Topcoat-native · WO-037 foundation"</span>
                </div>
                <div class="fui-gallery__grid">
                    gallery_pane(theme: "light", label: "Light")
                    gallery_pane(theme: "dark", label: "Dark")
                </div>
            </body>
        </html>
    }
}
