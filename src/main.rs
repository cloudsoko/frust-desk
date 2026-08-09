//! Desk v1 — the product's face on the hardened kernel.
//!
//! The Desk is a pure renderer of
//! (DocType metadata, record JSON) per the headless contract, and a
//! CLIENT of the kernel REST surface — every byte of data it shows or writes
//! travels through `frust serve` under the acting user's session token. No
//! root SQL, no direct SurrealDB, no role headers.
//!
//! v1 boundaries (standing): list refresh is SSE-driven — the browser opens a
//! kernel event stream, with a 60 s meta-refresh tag as the fallback when the
//! stream can't open; no spreadsheet grids; dependent-field logic is
//! client-side via per-field runtime signals, so a field enabled or disabled
//! by another field's value updates without a kernel round-trip.

/// The Desk's design system, wired into the real pages. Lives
/// here — Desk-local — and never in the vendored `topcoat` tree, which keeps
/// the kernel and the framework lean, and the Desk owns its skin.
mod frust_ui;
mod brand;
mod money;
mod pages;
mod realtime;
mod workspace;

use serde::Deserialize;
use topcoat::{
    Result,
    context::Cx,
    cookie::{Cookie, Cookies, RouterBuilderCookieExt, cookies},
    router::{
        IntoResponse,
        Response,
        Router,
        RouterBuilderDiscoverExt,
        // v0.5.0 namespaced the content types under `content::` (the router
        // root no longer globs them out); `Wasm` is still ours.
        content::{Form, Js, Wasm},
        error::{SeeOther, bad_request, redirect, see_other},
        layout,
        page,
        path_param,
        query_params,
        route,
    },
    runtime::{Decimal, Signal, SignalDeclaration},
    view::{component, view},
};

#[tokio::main]
async fn main() {
    // no asset bundle, no client runtime: Desk v1 is fully server-rendered
    // (zero signals/shards — the v1 field set has no dependent fields)
    topcoat::start(Router::builder().cookies().discover().build())
        .await
        .unwrap();
}

// ── Kernel client ───────────────────────────────────────────────────────────

mod kernel;

use kernel::{
    Session, admit_or_busy as admit, err500, flash, kernel_status, request_tenant, require_session,
    session, take_flash, tenant_from_host,
};
use brand::{RawCss, brand_settings_meta, brand_style};
#[cfg(test)]
use brand::{
    ACCENT_COLORS, ACCENT_FAMILIES, BRAND_TOKEN_NAMES, accent_family, brand_style_from_row,
};
use money::{MONEY_SCALE, money_sub, pad_money};
use pages::DocType;
#[cfg(test)]
use pages::today_iso;
use realtime::live_updates;
use workspace::{Workspace, workspace_links, workspace_list};
#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::{
        ACCENT_COLORS, ACCENT_FAMILIES, BRAND_TOKEN_NAMES, DocType, MONEY_SCALE, Workspace,
        accent_family, brand_settings_meta, brand_style_from_row, money_sub, pad_money,
        tenant_from_host, workspace_links,
    };
    use topcoat::cookie::RouterBuilderCookieExt;
    use topcoat::router::{Body, Request, Router, RouterBuilderDiscoverExt, to_bytes};

    /// Router-level tests share two process globals: the `FRUST_KERNEL` env var
    /// and the `OnceLock` ureq agent that reads it. `cargo test` runs them in
    /// parallel threads, so each takes this lock before pointing the Desk at its
    /// own fake kernel. A `tokio::sync::Mutex` lets the guard be held across the
    /// `.await` points inside each test without tripping `await_holding_lock`;
    /// unlike `std::sync::Mutex` it has no poisoning, so a panicking test simply
    /// releases the guard instead of cascading a poison error into every other
    /// router test.
    static KERNEL_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A fake kernel that stays up for the whole test, records the request LINE
    /// of every call, and answers each through `responder`. Unlike the brand
    /// fake kernel it serves no fixed request COUNT, so a handler that (rightly)
    /// makes *fewer* calls than a happy path — the whole point of the
    /// no-partial-write fix — can never hang the test waiting for a call that
    /// must not happen.
    fn spawn_kernel(
        responder: impl Fn(&str) -> (u16, serde_json::Value) + Send + 'static,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake kernel");
        let addr = listener.local_addr().expect("fake kernel address");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let recorder = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .expect("read timeout");
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0_u8; 1024];
                    let Ok(read) = stream.read(&mut chunk) else {
                        break;
                    };
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    let Some(head_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&request[..head_end]);
                    let content_len = head
                        .lines()
                        .find_map(|line| {
                            line.split_once(':').and_then(|(name, value)| {
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                        })
                        .unwrap_or(0);
                    if request.len() >= head_end + 4 + content_len {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&request).to_string();
                let line = request.lines().next().unwrap_or("").to_string();
                recorder
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(line);
                let (status, body) = responder(request.as_str());
                let reason = match status {
                    200 => "OK",
                    303 => "See Other",
                    500 => "Internal Server Error",
                    _ => "Status",
                };
                let body = body.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
            }
        });
        (format!("http://{addr}"), seen)
    }

    /// Drives an authenticated GET page through the router, returning its HTTP
    /// status and rendered HTML.
    async fn get_page(router: &Router, uri: &str, cookie: &str) -> (u16, String) {
        let request = Request::builder()
            .uri(uri)
            .header("cookie", cookie)
            .body(Body::empty())
            .expect("page request");
        let response = router.handle(request).await;
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("page bytes");
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    fn fake_brand_kernel(requests: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake kernel");
        let addr = listener.local_addr().expect("fake kernel address");
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().expect("accept Desk request");
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .expect("read timeout");
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0_u8; 1024];
                    let read = stream.read(&mut chunk).expect("read Desk request");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    let Some(head_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&request[..head_end]);
                    let content_len = head
                        .lines()
                        .find_map(|line| {
                            line.split_once(':').and_then(|(name, value)| {
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                        })
                        .unwrap_or(0);
                    if request.len() >= head_end + 4 + content_len {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&request).to_string();
                seen.push(request.clone());
                let row = if request.contains("acme-token") {
                    serde_json::json!({
                        "primary_color": "#1d4ed8",
                        "accent_color": "#059669",
                        "logo_url": "/assets/acme-mark.svg"
                    })
                } else if request.contains("beta-token") {
                    serde_json::json!({
                        "primary_color": "#be123c",
                        "accent_color": "#7c3aed",
                        // an `&` query string is legal for the logo mapper and is
                        // exactly the byte `view!`'s text escaping would rewrite
                        // to `&amp;` — so this URL only survives intact if the
                        // brand CSS is rendered raw
                        "logo_url": "https://cdn.example.test/beta-mark.svg?v=2&cache=1"
                    })
                } else {
                    serde_json::json!({})
                };
                let body = serde_json::json!({ "row": row }).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .expect("write fake kernel response");
            }
            seen
        });
        (format!("http://{addr}"), handle)
    }

    async fn rendered_login_page(router: &Router, token: &str) -> String {
        let request = Request::builder()
            .uri("/login")
            .header(
                "cookie",
                format!("frust_session={token}; frust_user=manager; frust_role=manager"),
            )
            .body(Body::empty())
            .expect("page request");
        let response = router.handle(request).await;
        assert_eq!(response.status().as_u16(), 200);
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("page bytes");
        String::from_utf8(bytes.to_vec()).expect("page utf8")
    }

    #[tokio::test]
    async fn two_tenant_pages_carry_their_own_style_tag_content() {
        // Serialize with the other router-level tests: they all set the
        // process-global FRUST_KERNEL endpoint the shared agent reads from.
        let _env = KERNEL_ENV.lock().await;
        let (base, server) = fake_brand_kernel(3);
        unsafe { std::env::set_var("FRUST_KERNEL", base) };
        let router = Router::builder().cookies().discover().build();

        let acme = rendered_login_page(&router, "acme-token").await;
        let beta = rendered_login_page(&router, "beta-token").await;
        let unset = rendered_login_page(&router, "unset-token").await;

        assert!(acme.contains("<link rel=\"stylesheet\" href=\"/frust-ui.css\">"));
        assert!(
            acme.contains("<style data-frust-brand=\"tenant\">:root.fui-root{"),
            "{acme}"
        );
        assert!(acme.contains("--fui-primary-bg:#1d4ed8;"));
        // The generated CSS must render RAW: an HTML-escaped `url(&quot;…&quot;)`
        // is a broken declaration, so assert the exact unescaped `url("…")`.
        assert!(
            acme.contains("--fui-brand-logo-image:url(\"/assets/acme-mark.svg\");"),
            "{acme}"
        );
        assert!(!acme.contains("#be123c"));

        assert!(beta.contains("<style data-frust-brand=\"tenant\">:root.fui-root{"));
        assert!(beta.contains("--fui-primary-bg:#be123c;"));
        // The generated, kernel-validated CSS must render RAW: interpolated as
        // escaped text the `&` becomes `&amp;` and the url() breaks. Assert the
        // EXACT unescaped url string, and that the escaped form is absent.
        assert!(
            beta.contains(
                "--fui-brand-logo-image:url(\"https://cdn.example.test/beta-mark.svg?v=2&cache=1\");"
            ),
            "{beta}"
        );
        assert!(
            !beta.contains("beta-mark.svg?v=2&amp;cache=1"),
            "brand CSS was HTML-escaped: {beta}"
        );
        assert!(!beta.contains("#1d4ed8"));
        assert!(!unset.contains("data-frust-brand"));

        let requests = server.join().expect("fake kernel thread");
        assert_eq!(requests.len(), 3);
        assert!(
            requests
                .iter()
                .all(|request| { request.starts_with("POST /single/brand_settings HTTP/1.1") })
        );
    }

    #[test]
    fn brand_settings_schema_is_a_bounded_single() {
        let meta = brand_settings_meta();
        assert_eq!(meta["name"], "brand_settings");
        assert_eq!(meta["issingle"], true);
        let fields = meta["fields"].as_array().expect("fields");
        assert_eq!(fields.len(), 5);
        assert_eq!(
            fields
                .iter()
                .map(|field| field["fieldname"].as_str().unwrap_or_default())
                .collect::<Vec<_>>(),
            [
                "primary_color",
                "accent_color",
                "corner_radius_scale",
                "font_family_name",
                "logo_url"
            ]
        );
        for field in &fields[..4] {
            assert_eq!(field["fieldtype"], "Select", "untyped brand field: {field}");
            let options = field["options"].as_array().expect("Select options");
            assert_eq!(
                options.first().and_then(serde_json::Value::as_str),
                Some("")
            );
            assert!(options.len() > 1, "brand vocabulary is empty: {field}");
        }
        assert_eq!(fields[4]["fieldtype"], "Data");
    }

    #[test]
    fn two_tenant_brand_rows_keep_content_provenance() {
        let tenant_a = serde_json::json!({
            "primary_color": "#1d4ed8",
            "accent_color": "#059669",
            "corner_radius_scale": "compact",
            "font_family_name": "Inter",
            "logo_url": "/assets/acme-mark.svg"
        });
        let tenant_b = serde_json::json!({
            "primary_color": "#be123c",
            "accent_color": "#7c3aed",
            "corner_radius_scale": "rounded",
            "font_family_name": "Georgia",
            "logo_url": "https://cdn.example.test/beta-mark.svg"
        });
        let a = brand_style_from_row(&tenant_a).expect("tenant A style");
        let b = brand_style_from_row(&tenant_b).expect("tenant B style");

        assert!(a.contains("--fui-primary-bg:#1d4ed8;"), "{a}");
        assert!(a.contains("--fui-blue:#059669;"), "{a}");
        assert!(
            a.contains("--fui-brand-logo-image:url(\"/assets/acme-mark.svg\");"),
            "{a}"
        );
        assert!(
            !a.contains("#be123c"),
            "tenant B primary leaked into A: {a}"
        );
        assert!(b.contains("--fui-primary-bg:#be123c;"), "{b}");
        assert!(b.contains("--fui-blue:#7c3aed;"), "{b}");
        assert!(
            b.contains("--fui-brand-logo-image:url(\"https://cdn.example.test/beta-mark.svg\");"),
            "{b}"
        );
        assert!(
            !b.contains("#1d4ed8"),
            "tenant A primary leaked into B: {b}"
        );
        assert_ne!(a, b);
    }

    /// WCAG 2.x relative luminance of an sRGB `#rrggbb` string. Pure arithmetic,
    /// no dependency — so the contrast floor is checked, not trusted.
    fn relative_luminance(hex: &str) -> f64 {
        let hex = hex.strip_prefix('#').expect("hex color");
        let channel = |i: usize| {
            let v = u8::from_str_radix(&hex[i..i + 2], 16).expect("hex channel") as f64 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(0) + 0.7152 * channel(2) + 0.0722 * channel(4)
    }

    fn contrast_ratio(fg: &str, bg: &str) -> f64 {
        let (a, b) = (relative_luminance(fg), relative_luminance(bg));
        let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn every_accent_family_is_complete_and_its_ink_meets_aa() {
        // ink lands on the default page background
        const SURFACE: &str = "#ffffff";
        const AA_NORMAL: f64 = 4.5;

        fn token_names(family: &[(&'static str, &'static str); 7]) -> Vec<&'static str> {
            let mut names: Vec<&'static str> = family.iter().map(|(token, _)| *token).collect();
            names.sort_unstable();
            names
        }

        // every accent option resolves to a family, and every family shares one
        // token-name set — no accent may leave a member static-blue
        let reference = token_names(accent_family(ACCENT_COLORS[0]).expect("first accent family"));
        assert_eq!(reference.len(), 7, "duplicate token in the reference family");
        for accent in ACCENT_COLORS {
            let family =
                accent_family(accent).unwrap_or_else(|| panic!("no family for accent {accent}"));
            assert_eq!(
                token_names(family),
                reference,
                "accent {accent} declares a different token set"
            );

            let ink = family
                .iter()
                .find_map(|(token, value)| (*token == "--fui-blue-ink").then_some(*value))
                .expect("family declares --fui-blue-ink");
            let ratio = contrast_ratio(ink, SURFACE);
            assert!(
                ratio >= AA_NORMAL,
                "accent {accent} ink {ink} is {ratio:.2}:1 on {SURFACE}, below AA {AA_NORMAL}:1"
            );
        }

        // the Select vocabulary and the family table are the same closed set
        assert_eq!(ACCENT_FAMILIES.len(), ACCENT_COLORS.len());
        for family in ACCENT_FAMILIES {
            assert!(
                ACCENT_COLORS.contains(&family[0].1),
                "family {} has no matching accent option",
                family[0].1
            );
        }
    }

    #[test]
    fn unset_brand_settings_emit_no_style_content() {
        assert_eq!(brand_style_from_row(&serde_json::json!({})), None);
        assert_eq!(
            brand_style_from_row(&serde_json::json!({
                "primary_color": "",
                "accent_color": "",
                "corner_radius_scale": "",
                "font_family_name": "",
                "logo_url": ""
            })),
            None
        );
    }

    #[test]
    fn out_of_vocabulary_brand_data_is_not_css() {
        let row = serde_json::json!({
            "primary_color": "</style><script>alert(1)</script>"
        });
        assert_eq!(brand_style_from_row(&row), None);
    }

    #[test]
    fn logo_url_cannot_escape_its_fixed_token_mapping() {
        for logo_url in [
            "javascript:alert(1)",
            "data:image/svg+xml,<svg onload=alert(1)>",
            "https://cdn.example/x.svg\");color:red;--owned:url(\"x",
            "//foreign.example/x.svg",
        ] {
            assert_eq!(
                brand_style_from_row(&serde_json::json!({ "logo_url": logo_url })),
                None,
                "unsafe logo entered CSS: {logo_url}"
            );
        }
    }

    #[test]
    fn tenant_hint_comes_only_from_a_real_subdomain() {
        assert_eq!(tenant_from_host("acme.frust.test"), Some("acme"));
        assert_eq!(tenant_from_host("beta.frust.test:3000"), Some("beta"));
        for host in ["127.0.0.1:3000", "localhost:3000", "frust.test", ""] {
            assert_eq!(
                tenant_from_host(host),
                None,
                "invented a tenant from {host:?}"
            );
        }
    }

    #[test]
    fn workspace_items_keep_order_and_filter_unreadable_targets() {
        let workspace: Workspace = serde_json::from_value(serde_json::json!({
            "label": "Accounting",
            "items": [
                { "label": "Sales invoices", "kind": "doctype", "target": "sales_invoice" },
                { "label": "Accounts receivable", "kind": "report", "target": "ar_outstanding" },
                { "label": "Missing", "kind": "doctype", "target": "missing" }
            ]
        }))
        .unwrap();
        let doctypes: Vec<DocType> = serde_json::from_value(serde_json::json!([
            {
                "name": "sales_invoice",
                "submittable": true,
                "can_read": true,
                "fields": [],
                "aggregates": [{ "kind": "counter", "rollup": "ar_outstanding" }]
            },
            { "name": "ar_outstanding", "can_read": false, "fields": [] }
        ]))
        .unwrap();

        let clerk = workspace_links(&workspace, &doctypes);
        assert_eq!(clerk.len(), 1);
        assert_eq!(clerk[0].label, "Sales invoices");
        assert_eq!(clerk[0].href, "/list/sales_invoice");

        let mut manager_doctypes = doctypes;
        manager_doctypes[1].can_read = true;
        let manager = workspace_links(&workspace, &manager_doctypes);
        assert_eq!(
            manager
                .iter()
                .map(|link| link.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Sales invoices", "Accounts receivable"]
        );
        assert_eq!(manager[1].href, "/report/ar_outstanding");
    }

    /// The money-formatting ruling, pinned. The interesting cases are the two
    /// at the edges: SurrealDB's stripped trailing zero (which is why this
    /// helper exists) and the over-scale value (which must SURVIVE, not round).
    #[test]
    fn money_sub_is_exact_and_never_float() {
        // the report's own question: Meridian charged 300, paid 120
        assert_eq!(money_sub("300", "120", 2).as_deref(), Some("180.00"));
        assert_eq!(money_sub("300.00", "120.00", 2).as_deref(), Some("180.00"));

        // the classic float traps — an f64 implementation fails these
        assert_eq!(money_sub("0.30", "0.10", 2).as_deref(), Some("0.20"));
        assert_eq!(money_sub("0.03", "0.01", 2).as_deref(), Some("0.02"));
        assert_eq!(money_sub("1.10", "1.00", 2).as_deref(), Some("0.10"));
        // 4.35 and 8.45 are the textbook binary-representation offenders
        assert_eq!(money_sub("8.45", "4.35", 2).as_deref(), Some("4.10"));

        // overpayment is a real accounting state, not an error
        assert_eq!(money_sub("100.00", "150.00", 2).as_deref(), Some("-50.00"));
        // nothing charged, nothing paid
        assert_eq!(money_sub("0", "0", 2).as_deref(), Some("0.00"));
        // a missing side yields nothing shown, never a wrong number
        assert_eq!(money_sub("", "120", 2), None);
        assert_eq!(money_sub("300", "", 2), None);
        // and non-decimal input is refused rather than coerced
        assert_eq!(money_sub("N/A", "1", 2), None);
        assert_eq!(money_sub("1.0.0", "1", 2), None);

        // OVER-SCALE IS REFUSED, not rounded — the same posture pad_money
        // takes. Silently dropping a place inside a money subtraction is the
        // defect this whole function exists to avoid.
        assert_eq!(money_sub("1.005", "1.00", 2), None);

        // large values stay exact (i128, not f64's 2^53 mantissa limit)
        assert_eq!(
            money_sub("99999999999.99", "0.01", 2).as_deref(),
            Some("99999999999.98")
        );
    }

    #[test]
    fn pad_money_pads_to_scale_and_never_rounds() {
        // the case that motivated the ruling: `37.50` is stored as `37.5`
        assert_eq!(pad_money("37.5", 2), "37.50");
        assert_eq!(pad_money("15", 2), "15.00");
        assert_eq!(pad_money("0", 2), "0.00");
        assert_eq!(pad_money("-3.5", 2), "-3.50");
        // already at scale: untouched
        assert_eq!(pad_money("15.00", 2), "15.00");

        // OVER scale is a DEFECT TO SURFACE, not to tidy away. Money is stored
        // AT scale, so three places in a two-place field means something
        // upstream is wrong — and a display layer that printed `1.01` here
        // would hide it at the one moment a human could still catch it.
        assert_eq!(pad_money("1.005", 2), "1.005");
        assert_eq!(pad_money("0.999", 2), "0.999");

        // not a plain decimal: passed through rather than half-formatted
        assert_eq!(pad_money("", 2), "");
        assert_eq!(pad_money("n/a", 2), "n/a");
        assert_eq!(pad_money("$5", 2), "$5");
        assert_eq!(pad_money("1e3", 2), "1e3");

        // padding must never alter the numeric VALUE — the property the ruling
        // rests on, checked rather than asserted in prose
        for raw in ["37.5", "15", "0", "-3.5", "2.1"] {
            let padded = pad_money(raw, MONEY_SCALE);
            assert_eq!(
                raw.parse::<f64>().unwrap(),
                padded.parse::<f64>().unwrap(),
                "padding changed the value of {raw}"
            );
        }
    }

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
    const MAIN_RS: &str = include_str!("main.rs");

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
            for src in [UI_RS, MAIN_RS] {
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

    #[test]
    fn today_iso_is_a_calendar_date() {
        let d = super::today_iso();
        let parts: Vec<&str> = d.split('-').collect();
        assert_eq!(parts.len(), 3, "expected YYYY-MM-DD, got {d}");
        assert_eq!(parts[0].len(), 4);
        let m: u32 = parts[1].parse().expect("month");
        let day: u32 = parts[2].parse().expect("day");
        assert!((1..=12).contains(&m), "month out of range in {d}");
        assert!((1..=31).contains(&day), "day out of range in {d}");
    }

    /// Responder for the home page: a submittable DocType that WOULD produce a
    /// starter card, and a workspace read the caller chooses to fail or empty.
    /// The layout's brand read is answered blank.
    fn home_kernel(
        workspace: (u16, serde_json::Value),
    ) -> impl Fn(&str) -> (u16, serde_json::Value) {
        move |req: &str| {
            let line = req.lines().next().unwrap_or("");
            if line.starts_with("GET /meta ") {
                (
                    200,
                    serde_json::json!({
                        "doctypes": [
                            { "name": "sales_invoice", "submittable": true, "fields": [] }
                        ]
                    }),
                )
            } else if line.starts_with("POST /read/workspace ") {
                workspace.clone()
            } else {
                // the layout's brand read — no tenant brand
                (200, serde_json::json!({ "row": {} }))
            }
        }
    }

    const HOME_COOKIE: &str = "frust_session=home-token; frust_user=manager; frust_role=manager";

    /// A workspace-read FAILURE is an outage, not an empty directory: it must
    /// surface as an error page, never be laundered into the starter-card
    /// fallback that a genuinely empty tenant sees.
    #[tokio::test]
    async fn home_workspace_read_failure_is_an_error_not_an_empty_directory() {
        let _env = KERNEL_ENV.lock().await;
        let (base, seen) = spawn_kernel(home_kernel((
            500,
            serde_json::json!({ "error": { "kind": "db", "detail": "workspace store down" } }),
        )));
        unsafe { std::env::set_var("FRUST_KERNEL", base) };
        let router = Router::builder().cookies().discover().build();

        let (status, html) = get_page(&router, "/", HOME_COOKIE).await;

        assert_eq!(
            status, 500,
            "a workspace outage must surface as an error page, not a 200 home: {html}"
        );
        assert!(
            !html.contains("Open list"),
            "the starter-card fallback rendered on a workspace FAILURE — an outage was masked as an empty directory: {html}"
        );
        let seen = seen.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            seen.iter().any(|l| l.starts_with("POST /read/workspace")),
            "the workspace read was never attempted: {seen:?}"
        );
    }

    /// The other half of the distinction: a SUCCESSFUL empty workspace read is
    /// the honest "no workspaces yet", and only that renders the starter cards.
    /// Same meta as the failure test, so the two differ only by the workspace
    /// response — error ≠ empty, proven by contrast.
    #[tokio::test]
    async fn home_empty_workspaces_falls_back_to_starter_cards() {
        let _env = KERNEL_ENV.lock().await;
        let (base, _seen) =
            spawn_kernel(home_kernel((200, serde_json::json!({ "rows": [] }))));
        unsafe { std::env::set_var("FRUST_KERNEL", base) };
        let router = Router::builder().cookies().discover().build();

        let (status, html) = get_page(&router, "/", HOME_COOKIE).await;

        assert_eq!(status, 200, "an empty workspace read is a normal home page: {html}");
        assert!(
            html.contains("Open list"),
            "a genuinely empty workspace directory must fall back to the starter cards: {html}"
        );
    }

    /// A child-table metadata failure during save must NOT be papered over by
    /// skipping the Table field and writing the parent anyway: that silently
    /// drops the rows the user typed. The whole write is refused, the failure is
    /// flashed, and the user is returned to the form.
    #[tokio::test]
    async fn submit_new_writes_nothing_when_child_meta_fails() {
        let _env = KERNEL_ENV.lock().await;
        let (base, seen) = spawn_kernel(|req: &str| {
            let line = req.lines().next().unwrap_or("");
            if line.starts_with("GET /meta/order ") {
                (
                    200,
                    serde_json::json!({
                        "doctype": {
                            "name": "order",
                            "fields": [
                                { "fieldname": "customer", "fieldtype": "Data" },
                                { "fieldname": "lines", "fieldtype": "Table", "options": ["order_line"] }
                            ]
                        }
                    }),
                )
            } else if line.starts_with("GET /meta/order_line ") {
                // the child metadata is unavailable — a transient outage
                (
                    500,
                    serde_json::json!({ "error": { "kind": "db", "detail": "child meta down" } }),
                )
            } else {
                // a /write must never be reached; answer harmlessly if it is,
                // so the assertion (not a hang) reports the regression
                (200, serde_json::json!({ "created": { "id": "order:x" } }))
            }
        });
        unsafe { std::env::set_var("FRUST_KERNEL", base) };
        let router = Router::builder().cookies().discover().build();

        let request = Request::builder()
            .method("POST")
            .uri("/submit/order")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", "frust_session=t; frust_user=clerk; frust_role=clerk")
            .body(Body::from("customer=Acme&lines.0.item=Widget&lines.0.qty=2"))
            .expect("submit request");
        let response = router.handle(request).await;

        assert_eq!(
            response.status().as_u16(),
            303,
            "a child-meta failure returns to the form, not onward to the record"
        );
        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert_eq!(location, "/form/order", "must return to the form the user was on");
        let flashed = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|c| c.contains("frust_flash="));
        assert!(flashed, "the failure must be flashed to the user");

        let seen = seen.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            seen.iter().any(|l| l.starts_with("GET /meta/order ")),
            "the parent meta should have been read: {seen:?}"
        );
        assert!(
            !seen.iter().any(|l| l.contains("/write/")),
            "a partial document was written despite the child-table failure: {seen:?}"
        );
    }
}
