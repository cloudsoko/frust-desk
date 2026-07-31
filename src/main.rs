//! Desk v1 (WO-009) — the product's face on the hardened kernel.
//!
//! Graduated from the WO-002 proof-harness. The Desk is a pure renderer of
//! (DocType metadata, record JSON) per ADR-004's headless contract, and a
//! CLIENT of the kernel REST surface — every byte of data it shows or writes
//! travels through `frust serve` under the acting user's session token. No
//! root SQL, no direct SurrealDB, no role headers (WO-009 criterion 1).
//!
//! v1 boundaries (standing): list refresh is polling (meta-refresh) until
//! Topcoat ships push; no spreadsheet grids; dependent-field logic would be
//! client-side per ADR-007 — the v1 field set has none, so no runtime
//! signals are used at all. Every interaction is one kernel round-trip.

/// **WO-042:** the Desk's design system, wired into the real pages. Lives
/// here — Desk-local — and never in the vendored `topcoat` tree: ADR-004 keeps
/// the kernel and the framework lean, and the Desk owns its skin.
mod frust_ui;

use futures_core::Stream;
use serde::Deserialize;
use topcoat::{
    Result,
    context::Cx,
    cookie::{Cookie, Cookies, RouterBuilderCookieExt, cookies},
    router::{
        IntoResponse, Response, Router, RouterBuilderDiscoverExt, layout, page, path_param,
        query_params, route,
        // v0.5.0 namespaced the content types under `content::` (the router
        // root no longer globs them out); `Wasm` is still ours.
        content::{
            Form, Js, Wasm,
            sse::{Event, KeepAlive, Sse},
        },
        error::{SeeOther, bad_request, internal_server_error, redirect, see_other, service_unavailable},
    },
    runtime::{Decimal, Signal, SignalDeclaration},
    view::{component, view},
};

#[tokio::main]
async fn main() {
    // no asset bundle, no client runtime: Desk v1 is fully server-rendered
    // (zero signals/shards — the v1 field set has no dependent fields)
    topcoat::start(Router::builder().cookies().discover().build()).await.unwrap();
}

// ── Kernel client ───────────────────────────────────────────────────────────

mod kernel {
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

    pub fn base() -> String {
        std::env::var("FRUST_KERNEL").unwrap_or_else(|_| "http://127.0.0.1:8790".into())
    }

    /// **WO-038/WO-041: ONE agent for the whole Desk.**
    ///
    /// This used to build a fresh `ureq::Agent` per call, which is the exact
    /// defect WO-041 closed in the kernel — a new TCP connection per request,
    /// so the Desk burned an ephemeral port per kernel round trip. Under the
    /// very overload this WO is about, that is a second failure source on top
    /// of the one being measured, and it would have contaminated the
    /// before/after.
    fn agent() -> &'static ureq::Agent {
        static A: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
        A.get_or_init(|| {
            // sized to the connections the Desk will actually hold open, which
            // is the admission ceiling — capped so the control run below
            // cannot ask for a pool of 100 000
            let pool = max_inflight().clamp(8, 64);
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .max_idle_connections_per_host(pool)
                .max_idle_connections(pool)
                .build()
                .into()
        })
    }

    /// **The admission ceiling**, from `FRUST_DESK_MAX_INFLIGHT`.
    ///
    /// Default 24: just above the kernel's own worker pool (`Rest::serve`
    /// clamps to 2..=16). Queueing a little keeps the kernel fed; queueing 500
    /// deep only converts a fast refusal into a slow timeout. WO-035 measured
    /// the knee at ~50 concurrent, and past it the Desk answered 500 rather
    /// than waiting — the wrong answer for a UI tier.
    ///
    /// **Configurable because it is also the control.** Setting it absurdly
    /// high disables shedding and reproduces the pre-WO-038 failure mode on
    /// demand, the same discipline as the `naive-blocking-sse` control: a
    /// guard whose failure mode cannot be reproduced decays into a number
    /// nobody trusts.
    pub fn max_inflight() -> usize {
        static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        *N.get_or_init(|| {
            std::env::var("FRUST_DESK_MAX_INFLIGHT")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|n| *n > 0)
                .unwrap_or(64)
        })
    }

    static INFLIGHT: AtomicI64 = AtomicI64::new(0);
    static SHED: AtomicU64 = AtomicU64::new(0);
    static SERVED: AtomicU64 = AtomicU64::new(0);
    /// Smoothed kernel round-trip latency, in microseconds.
    static LATENCY_US: AtomicU64 = AtomicU64::new(0);

    /// Smoothed kernel round-trip latency is still *reported* (`/admission`),
    /// because "how slow is the kernel right now" is worth an operator's
    /// glance. It is no longer *shed on*: the WO-038 rewrite proved latency
    /// cannot see the queue (the kernel stayed at 29 ms while Desk pages took
    /// 2 376 ms), and the semaphore is the bound that can. Measuring something
    /// and deciding on it are different jobs.
    /// Feed the controller. Called on every kernel round trip.
    pub fn record_latency(us: u64) {
        // EWMA, 1/8 weight on the new sample: slow enough that a single slow
        // query cannot trip the shed, fast enough to react within a second of
        // real saturation.
        let prev = LATENCY_US.load(Ordering::Relaxed);
        let next = if prev == 0 { us } else { prev - prev / 8 + us / 8 };
        LATENCY_US.store(next, Ordering::Relaxed);
    }

    /// Admission counters, for `/admission` — refusals must be a **named,
    /// attributable signal**, not a spike in 500s an operator has to guess at.
    pub fn stats() -> (i64, u64, u64, u64) {
        (
            INFLIGHT.load(Ordering::Relaxed),
            SERVED.load(Ordering::Relaxed),
            SHED.load(Ordering::Relaxed),
            LATENCY_US.load(Ordering::Relaxed) / 1000,
        )
    }

    /// A permit to serve a request. Releases on drop, so an early return or a
    /// panic cannot leak the slot — the failure mode of a hand-decremented
    /// counter is a Desk that sheds everything forever.
    pub struct Permit(#[allow(dead_code)] tokio::sync::SemaphorePermit<'static>);

    impl Drop for Permit {
        fn drop(&mut self) {
            INFLIGHT.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Take a slot, or refuse. **Refusing is the point**: past the ceiling the
    /// honest answer is "busy, come back", delivered immediately, rather than
    /// a request that queues behind a saturated database until something times
    /// out and reads as broken.
    /// The permit pool. **This is the concurrency bound the Desk actually
    /// has**, and the whole point of the rewrite.
    ///
    /// The first attempt counted in-flight kernel calls and measured **inert**:
    /// `shed` stayed 0 at 500 concurrent clients, three sweeps. The reason was
    /// structural, not a mistuned threshold — each handler made a *blocking*
    /// `ureq` call inside an `async fn`, pinning one of ~16 tokio workers for
    /// the whole round trip. So:
    ///
    /// | observation | value |
    /// |---|---|
    /// | requests inside the Desk (Little's law, 157.8 req/s × 2.476 s) | ~391 |
    /// | in-flight kernel calls a counter could ever see | ≤ 24 |
    /// | kernel round-trip latency observed | 29 ms |
    /// | Desk page p50 at that moment | 2 376 ms |
    ///
    /// ~2 350 ms of every slow request was queueing **in the tokio
    /// scheduler**, upstream of any code this crate runs — which is why the
    /// overload arrived as dropped connections rather than as a number going
    /// up. An `async fn` making a blocking call is a hidden concurrency cap
    /// *and* blinds any admission gate written inside it.
    ///
    /// The fix is the same act as the shed: `spawn_blocking` unpins the
    /// worker, and the semaphore is then the *visible* bound to refuse on. The
    /// queue moves out of the scheduler and into a counter an operator can
    /// read.
    fn permits() -> &'static tokio::sync::Semaphore {
        static S: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
        S.get_or_init(|| tokio::sync::Semaphore::new(max_inflight()))
    }

    /// Take a permit, or refuse **immediately**.
    ///
    /// `try_acquire`, never `acquire`: waiting for a permit would recreate the
    /// queue in a new place. The honest answer past the bound is "busy, come
    /// back", delivered now.
    pub fn admit() -> Option<Permit> {
        match permits().try_acquire() {
            Ok(p) => {
                INFLIGHT.fetch_add(1, Ordering::AcqRel);
                SERVED.fetch_add(1, Ordering::Relaxed);
                Some(Permit(p))
            }
            Err(_) => {
                SHED.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// **The kernel call, off the async workers.**
    ///
    /// `spawn_blocking` is the lazy-correct fix: it reuses the WO-041 shared
    /// blocking `Agent` rather than dragging in an async HTTP stack, and its
    /// default 512-thread pool sits comfortably above any sane permit count.
    /// The async worker awaits a join handle instead of sitting on a socket,
    /// so the accept loop keeps accepting and the queue stops forming where
    /// nothing can see it.
    pub async fn call_async(
        token: Option<&str>,
        route: &str,
        body: &serde_json::Value,
    ) -> (u16, serde_json::Value) {
        let (token, route, body) = (token.map(str::to_string), route.to_string(), body.clone());
        match tokio::task::spawn_blocking(move || call(token.as_deref(), &route, &body)).await {
            Ok(out) => out,
            // a panicked blocking task is our failure, not the kernel's
            Err(e) => (
                502,
                serde_json::json!({ "error": { "kind": "transport", "detail": e.to_string() } }),
            ),
        }
    }

    /// One kernel call, blocking. Non-2xx comes back as Ok((code, body)) so
    /// callers can render typed errors as user-facing messages.
    ///
    /// Still used directly from the few genuinely synchronous contexts (a
    /// `Drop` impl cannot await); every request path goes through
    /// [`call_async`].
    pub fn call(token: Option<&str>, route: &str, body: &serde_json::Value) -> (u16, serde_json::Value) {
        let started = std::time::Instant::now();
        let mut req = agent().post(format!("{}{}", base(), route));
        if let Some(t) = token {
            req = req.header("Authorization", &format!("Bearer {t}"));
        }
        let out = match req.send(body.to_string()) {
            Ok(mut r) => {
                let code = r.status().as_u16();
                let parsed = r.body_mut().read_json().unwrap_or(serde_json::json!({}));
                (code, parsed)
            }
            Err(e) => (502, serde_json::json!({ "error": { "kind": "transport", "detail": e.to_string() } })),
        };
        // feed the controller whether the call succeeded or failed: a call
        // that took four seconds and then errored is the strongest possible
        // evidence of saturation, and dropping it would blind the shed
        // exactly when it is needed
        record_latency(started.elapsed().as_micros() as u64);
        out
    }

    /// Typed kernel errors -> user-facing words. Engine internals stay out
    /// of the DOM (ADR-007 hygiene): machine codes are translated, raw DB
    /// wording is dropped.
    pub fn friendly(code: u16, body: &serde_json::Value) -> String {
        let err = body.get("error").cloned().unwrap_or_default();
        let kind = err.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        let detail = err.get("detail").and_then(|d| d.as_str()).unwrap_or("");
        match kind {
            "hook-rejected" => format!(
                "Rejected by a validation rule: {}",
                err.get("message").and_then(|m| m.as_str()).unwrap_or("the document was not accepted")
            ),
            "identity-unresolved" => "Your identity could not be resolved — contact an administrator.".into(),
            "permission-denied" if code == 401 => "Your session has expired — please log in again.".into(),
            "permission-denied" => "You don't have permission to do that.".into(),
            "field-not-readable" => "You don't have permission to see one of those fields.".into(),
            "unknown-doctype" => "That DocType does not exist.".into(),
            "invalid-value" => format!("Invalid input: {detail}"),
            "write-conflict-exhausted" => "The system is busy — please try again.".into(),
            "db" if detail.contains("FRUST:E_DOCSTATUS:RESURRECTION") => {
                "This document is cancelled; it can never be edited again.".into()
            }
            "db" if detail.contains("FRUST:E_DOCSTATUS") => {
                "That lifecycle transition is not allowed.".into()
            }
            // WO-031: the workflow judge's refusal. `detail` is already
            // user-facing prose ("'Approve' from 'Draft' requires role
            // manager; you are 'clerk'"); the FRUST:E_WORKFLOW code stays in
            // `code`, out of the message (ADR-007 hygiene).
            "workflow-denied" => detail.to_string(),
            "db" if detail.contains("FRUST:E_IDENTITY_UNRESOLVED") => {
                "Your identity could not be resolved — contact an administrator.".into()
            }
            _ => "Something went wrong on the server.".into(),
        }
    }
}

// ── Session (kernel-owned; the Desk only carries the cookie) ────────────────

#[derive(Clone)]
struct Session {
    token: String,
    user: String,
    role: String,
}

fn session(cx: &Cx) -> Option<Session> {
    let jar = cookies(cx);
    Some(Session {
        token: jar.get("frust_session")?.value().to_string(),
        user: jar.get("frust_user").map(|c| c.value().to_string()).unwrap_or_default(),
        role: jar.get("frust_role").map(|c| c.value().to_string()).unwrap_or_default(),
    })
}

fn require_session(cx: &Cx) -> Result<Session, topcoat::Error> {
    session(cx).ok_or_else(|| redirect("/login").into())
}

fn err500(msg: impl std::fmt::Display) -> topcoat::Error {
    internal_server_error(std::io::Error::other(msg.to_string())).into()
}

// ── WO-038: shed with intent ────────────────────────────────────────────────
//
// WO-035's finding: past the ~50-concurrent knee the Desk answered **HTTP 500
// for ~45% of requests at 200 concurrent and ~85% at 500**. A 500 tells a user
// (and a load balancer, and an on-call engineer) that the system is *broken*.
// It was not broken — it was busy, propagating downstream saturation upward
// with the wrong word.
//
// This is failure-MODE work, not throughput work. Nothing here makes the Desk
// faster, and it must not: the ~135 req/s ceiling is the WO-026 database
// conversation, deliberately out of scope. What changes is the answer given
// when that ceiling is exceeded.

/// A typed, honest "busy" — **503 with `Retry-After`**.
///
/// 503 rather than 429 on purpose: 429 is "*you* sent too many", which the
/// kernel already returns for WO-013's per-tenant budget. This is "*the
/// server* is at capacity", a different fact about a different subject, and
/// keeping them distinct is what lets an operator tell tenant shaping from
/// overload without guessing.
struct Busy;

impl Busy {
    fn now() -> topcoat::Error {
        // Short and honest: the shed happens because in-flight work is
        // queued, and that clears in seconds. A long Retry-After would turn a
        // momentary spike into a minute of self-inflicted downtime.
        service_unavailable(2).into()
    }
}

// NOTE: this must go through Topcoat's own error type, not a local
// `IntoResponse` impl. Topcoat maps errors to statuses by downcasting against
// a **closed list** of its own error types and falls back to 500 for anything
// else — so a bespoke `Busy` rendered as **HTTP 500**, which the first
// measured run caught: `shed: 32464` with `codes {"500": ...}`. The shed was
// firing and still telling every client "broken".
//
// The framework had no 429/503 constructor at all, so
// `ServiceUnavailableError` was added to the vendored Topcoat trunk
// (a carried patch, per the fork's practice) rather than worked around here.

/// Take an admission slot or refuse. Every kernel-calling handler passes
/// through here.
fn admit() -> std::result::Result<kernel::Permit, topcoat::Error> {
    kernel::admit().ok_or_else(Busy::now)
}

/// **Turn a kernel status into the Desk's status honestly.**
///
/// The old `err500` collapsed *every* kernel failure into 500 — including the
/// kernel's own typed capacity answers. So a kernel that correctly said "429,
/// slow down" or "503, busy" was reported to the user as "the Desk is broken".
/// A capacity answer must survive the hop.
fn kernel_status(code: u16, body: &serde_json::Value) -> topcoat::Error {
    match code {
        // the kernel's typed capacity answers (WO-013 tenant budget, live-sub
        // budget, and any 503 it sheds itself) pass through as capacity
        429 | 503 => Busy::now(),
        // 502 is our own transport failure reaching the kernel — under load
        // that is congestion, not a defect in the request
        502 => Busy::now(),
        _ => err500(kernel::friendly(code, body)),
    }
}

// ── Flash messages (one-shot, cookie-carried) ───────────────────────────────
// POST handlers answer 303 See Other (a 307 would replay the POST against a
// GET page); friendly error text rides a flash cookie the layout renders
// once and clears.

fn pct_encode(s: &str) -> String {
    s.bytes()
        .flat_map(|b| {
            if b.is_ascii_alphanumeric() {
                vec![b as char]
            } else {
                format!("%{b:02X}").chars().collect()
            }
        })
        .collect()
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn flash(cx: &Cx, msg: &str) {
    cookies(cx).add(Cookie::build(("frust_flash", pct_encode(msg))).path("/").build());
}

fn take_flash(cx: &Cx) -> Option<String> {
    let jar = cookies(cx);
    let msg = jar.get("frust_flash").map(|c| pct_decode(c.value()));
    if msg.is_some() {
        jar.remove(Cookie::build(("frust_flash", "")).path("/").build());
    }
    msg.filter(|m| !m.is_empty())
}

// ── DocType metadata (from the kernel, at request time) ─────────────────────

#[derive(Debug, Clone, Deserialize)]
struct DocType {
    name: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    submittable: bool,
    #[serde(default)]
    fields: Vec<DocField>,
    #[serde(default)]
    aggregates: Vec<Aggregate>,
    /// WO-017: Tier-2 client script, metadata like everything else. Its
    /// presence is the ONLY thing that loads the 4 MB engine — see
    /// `form_page`. Absent or blank means a scriptless form, which must
    /// never pay for the engine in requests or bytes.
    #[serde(default)]
    client_script: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct DocField {
    fieldname: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    fieldtype: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    options: Vec<String>,
    /// Editable while docstatus = 1 (the Frappe allow-on-submit convention).
    #[serde(default)]
    allow_on_submit: bool,
    // ── WO-014: declarative client behaviour (metadata, not code) ──
    #[serde(default)]
    depends_on: Option<Rule>,
    #[serde(default)]
    read_only_when: Option<Rule>,
    #[serde(default)]
    required_when: Option<Rule>,
    #[serde(default)]
    invalid_when: Option<Rule>,
    #[serde(default)]
    fetch_from: Option<FetchFrom>,
}

/// One declarative rule: `field <op> value`. Compiled at render time into a
/// runtime expression over the source field's signal.
#[derive(Debug, Clone, Deserialize)]
struct Rule {
    field: String,
    op: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

impl Rule {
    fn target(&self) -> String {
        self.value.clone().unwrap_or_default()
    }
    fn text(&self) -> String {
        self.message.clone().unwrap_or_else(|| "Not allowed".to_string())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct FetchFrom {
    source: String,
    doctype: String,
    field: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Aggregate {
    kind: String,
    rollup: String,
    #[serde(default)]
    metrics: Vec<MetricSpec>,
}

#[derive(Debug, Clone, Deserialize)]
struct MetricSpec {
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    field: String,
}

impl DocType {
    fn label_or_name(&self) -> String {
        if self.label.is_empty() { self.name.replace('_', " ") } else { self.label.clone() }
    }

    /// The lazy-load gate. A DocType with no script is indistinguishable, on
    /// the wire, from a Desk that has no script engine at all.
    fn script(&self) -> Option<&str> {
        self.client_script.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
}

impl DocField {
    fn label_or_name(&self) -> String {
        if self.label.is_empty() { self.fieldname.replace('_', " ") } else { self.label.clone() }
    }
}

// WO-038: these return the Desk's error directly rather than `anyhow`, so a
// kernel capacity answer keeps its meaning across the hop instead of being
// flattened into "something went wrong on the server".

async fn meta_list(s: &Session) -> std::result::Result<Vec<DocType>, topcoat::Error> {
    let (code, body) = kernel::call_async(Some(&s.token), "/meta", &serde_json::json!({})).await;
    if code != 200 {
        return Err(kernel_status(code, &body));
    }
    Ok(serde_json::from_value(body["doctypes"].clone()).unwrap_or_default())
}

async fn meta_one(s: &Session, name: &str) -> std::result::Result<DocType, topcoat::Error> {
    let (code, body) = kernel::call_async(Some(&s.token), &format!("/meta/{name}"), &serde_json::json!({})).await;
    if code != 200 {
        return Err(kernel_status(code, &body));
    }
    serde_json::from_value(body["doctype"].clone()).map_err(err500)
}

/// WO-015: how many blank rows a child table offers beyond what exists.
/// Rows are pre-rendered (with their signals) and REVEALED by "Add row" —
/// no client-side DOM creation, so every row is fully reactive from the
/// first paint. Deliberately small: this is a line editor, not a grid.
const SPARE_ROWS: usize = 4;

/// Reassembles `lines.{i}.{field}` form pairs into the embedded array the
/// envelope expects. Rows marked removed, or with every value blank, are
/// dropped — so "remove" and "never filled in" collapse to the same thing.
///
/// The whole array is sent on every save (ADR-008: children are embedded),
/// which is exactly what lets hooks and the Tier-2 line-differ see the full
/// document rather than a patch.
fn collect_rows(
    prefix: &str,
    child: &DocType,
    fields: &[(String, String)],
) -> Vec<serde_json::Value> {
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for i in 0.. {
        let row_prefix = format!("{prefix}.{i}.");
        let present: Vec<&(String, String)> =
            fields.iter().filter(|(k, _)| k.starts_with(&row_prefix)).collect();
        if present.is_empty() {
            break; // no more rendered rows
        }
        let get = |name: &str| -> &str {
            present
                .iter()
                .find(|(k, _)| k == &format!("{row_prefix}{name}"))
                .map(|(_, v)| v.as_str())
                .unwrap_or("")
        };
        if get("__remove") == "on" {
            continue;
        }
        let mut obj = serde_json::Map::new();
        let mut any = false;
        for f in &child.fields {
            let raw = get(&f.fieldname);
            if !raw.trim().is_empty() {
                any = true;
            }
            obj.insert(f.fieldname.clone(), typed_value(f, raw));
        }
        if any {
            rows.push(serde_json::Value::Object(obj));
        }
    }
    rows
}

/// Form value -> the REST typed-value form, per metadata. Money is DECIMAL
/// on the wire (never a float), links are records, checks are bools.
fn typed_value(f: &DocField, raw: &str) -> serde_json::Value {
    let raw = raw.trim();
    match f.fieldtype.as_str() {
        "Currency" => serde_json::json!({ "kind": "decimal", "v": if raw.is_empty() { "0" } else { raw } }),
        "Int" => serde_json::json!({ "kind": "int", "v": raw.parse::<i64>().unwrap_or(0) }),
        "Check" => serde_json::json!({ "kind": "bool", "v": raw == "on" || raw == "true" }),
        "Link" => serde_json::json!({ "kind": "record", "v": raw }),
        _ => serde_json::json!(raw),
    }
}

// ── Layout ──────────────────────────────────────────────────────────────────

/// **WO-042: the Desk wears Frust UI.**
///
/// The stylesheet and the `fui_*` set are WO-037's; what this does is make
/// them what `frust serve` actually renders, rather than what a gallery route
/// demonstrates. Everything below is presentation — no handler's data or
/// behaviour changes.
///
/// Light/dark composes with **no JS**: `frust_ui.css` carries both token sets
/// and switches on `prefers-color-scheme`, with `data-theme` as an explicit
/// override. A toggle, if one is ever wanted, is a `data-theme` attribute —
/// never a runtime (ADR-004).
///
/// **Inter is a progressive enhancement**, not a dependency: the Google Fonts
/// link is `media="print" onload=...`-free and simply absent offline, where
/// the `system-ui` fallback in the token stack takes over. Bundling it as a
/// served asset was considered and **not** done — it would add ~100 KB to the
/// binary to remove a font-swap that only shows on a first cold load, and the
/// Desk's no-external-asset posture is about *data*, not typography.
#[layout("/")]
async fn root_layout(cx: &Cx, slot: Result) -> Result {
    let sess = session(cx);
    let flash_msg = take_flash(cx);
    view! {
        <!DOCTYPE html>
        <html>
            <head>
                <title>"Frust Desk"</title>
                <meta name="viewport" content="width=device-width, initial-scale=1">
                <link rel="stylesheet" href="/frust-ui.css">
                <link rel="preconnect" href="https://fonts.googleapis.com">
                <link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&display=swap">
            </head>
            <body>
                <nav class="fui-nav">
                    <a href="/" class="fui-nav-brand">"Frust"</a>
                    if sess.is_some() {
                        <a href="/" class="fui-nav-link">"DocTypes"</a>
                        <a href="/reports" class="fui-nav-link">"Reports"</a>
                    }
                    <span class="fui-nav-spacer"></span>
                    match &sess {
                        Some(s) => {
                            <span class="fui-nav-user">
                                (&s.user)
                                frust_ui::fui_badge(label: s.role.clone(), color: "blue")
                            </span>
                            <a href="/logout" class="fui-nav-link">"Log out"</a>
                        }
                        None => {
                            <a href="/login" class="fui-nav-link">"Log in"</a>
                        }
                    }
                </nav>
                <main class="fui-main">
                    // WO-042: flash messages are the Desk's post-action
                    // feedback, so they render as a TOAST — auto-dismissing and
                    // stackable, purely in CSS (see `.fui-toast-stack`). The
                    // old inline-red-paragraph is gone.
                    if let Some(m) = &flash_msg {
                        <div class="fui-toast-stack" role="status" aria-live="polite">
                            frust_ui::fui_toast(variant: flash_variant(m), title: flash_title(m), (m))
                        </div>
                    }
                    (slot?)
                </main>
            </body>
        </html>
    }
}

/// Flash text carries no type, so the toast infers one. Deliberately crude —
/// the alternative is threading a variant through every `flash()` call site
/// for a cosmetic difference, and a wrong guess costs a colour, not a fact.
fn flash_variant(msg: &str) -> &'static str {
    let m = msg.to_ascii_lowercase();
    if m.starts_with("saved") || m.starts_with("submitted") || m.starts_with("created")
        || m.starts_with("cancelled") || m.contains("succeeded")
    {
        "success"
    } else {
        // `danger`, not `error` — the component's variant vocabulary is
        // info|success|warning|danger, and `fui-alert--error` is not a class
        // that exists. It compiled and rendered *nearly* right (base alert
        // styling, no colour, and `icon()` fell through to "" so no icon
        // either) for every error flash in the Desk. Caught by WO-047's guard.
        "danger"
    }
}

fn flash_title(msg: &str) -> &'static str {
    if flash_variant(msg) == "success" { "Done" } else { "Couldn't do that" }
}

// ── Login / logout ──────────────────────────────────────────────────────────

#[page("/login")]
async fn login_page(_cx: &Cx) -> Result {
    view! { login_form(message: None) }
}

#[component]
async fn login_form(message: Option<String>) -> Result {
    view! {
        <div style="max-width: 22rem; margin: 3rem auto;">
            frust_ui::fui_card(
                title: "Log in",
                if let Some(m) = &message {
                    frust_ui::fui_alert(variant: "danger", title: "Login failed", (m))
                }
                <form method="post" action="/login-submit">
                    frust_ui::fui_form_control(
                        label: "User", required: true,
                        frust_ui::fui_input(name: "user", required: true, autofocus: true)
                    )
                    frust_ui::fui_form_control(
                        label: "Password", required: true,
                        frust_ui::fui_input(name: "pass", kind: "password", required: true)
                    )
                    <div class="fui-form-actions">
                        frust_ui::fui_button(label: "Log in", variant: "primary", kind: "submit")
                    </div>
                </form>
            )
        </div>
    }
}

#[derive(Deserialize)]
struct LoginInput {
    user: String,
    pass: String,
}

#[route(POST "/login-submit")]
async fn login_submit(cx: &Cx, Form(input): Form<LoginInput>) -> Result<SeeOther> {
    let (code, body) = kernel::call_async(None, "/login", &serde_json::json!({ "user": input.user, "pass": input.pass })).await;
    if code != 200 {
        flash(cx, "Login failed — check your user and password.");
        return Ok(see_other("/login"));
    }
    let token = body["token"].as_str().unwrap_or_default().to_string();
    let user = body["user"].as_str().unwrap_or_default().to_string();
    let role = body["role"].as_str().unwrap_or_default().to_string();
    let jar = cookies(cx);
    jar.add(Cookie::build(("frust_session", token)).path("/").http_only(true).build());
    jar.add(Cookie::build(("frust_user", user)).path("/").build());
    jar.add(Cookie::build(("frust_role", role)).path("/").build());
    Ok(see_other("/"))
}

#[page("/logout")]
async fn logout(cx: &Cx) -> Result {
    if let Some(s) = session(cx) {
        let _ = kernel::call_async(Some(&s.token), "/logout", &serde_json::json!({})).await;
    }
    let jar = cookies(cx);
    for name in ["frust_session", "frust_user", "frust_role"] {
        jar.remove(Cookie::build((name, "")).path("/").build());
    }
    Err(redirect("/login").into())
}

// ── Home: the DocType directory ─────────────────────────────────────────────

#[page("/")]
async fn home(cx: &Cx) -> Result {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let doctypes = meta_list(&s).await?;
    let is_manager = s.role == "manager";
    view! {
        <div class="fui-page-head">
            <h1 class="fui-page-title">"DocTypes"</h1>
            if is_manager {
                <div class="fui-page-actions">
                    // WO-042 behaviour 1: CSS-only dialog. The trigger is a
                    // link to the modal's id; there is no JS anywhere in it.
                    frust_ui::fui_button(label: "New DocType", variant: "primary", icon: "plus", href: "#new-doctype")
                </div>
            }
        </div>
        <div class="fui-table-wrap">
            <table class="fui-table">
                <thead>
                    <tr>
                        <th>"DocType"</th>
                        <th>"Kind"</th>
                        <th style="width: 1%;"></th>
                        <th style="width: 1%;"></th>
                        if is_manager { <th style="width: 1%;"></th> }
                    </tr>
                </thead>
                <tbody>
                    if doctypes.is_empty() {
                        <tr><td colspan="5"><div class="fui-empty">"No DocTypes yet."</div></td></tr>
                    }
                    for dt in &doctypes {
                        <tr>
                            <td style="font-weight: 500;">(dt.label_or_name())</td>
                            <td>
                                if dt.submittable {
                                    frust_ui::fui_badge(label: "submittable", color: "blue")
                                } else {
                                    frust_ui::fui_badge(label: "record", color: "gray")
                                }
                            </td>
                            <td><a href=(format!("/list/{}", dt.name))>"List"</a></td>
                            <td><a href=(format!("/form/{}", dt.name))>"New"</a></td>
                            if is_manager {
                                <td>
                                    <a href=(format!("/script/{}", dt.name))>
                                        if dt.script().is_some() { "Script ●" } else { "Script" }
                                    </a>
                                </td>
                            }
                        </tr>
                    }
                </tbody>
            </table>
        </div>
        if is_manager {
            // ── BEHAVIOUR 1: dialog, rung (a) — CSS-only via `:target` ──
            //
            // Hidden until the URL fragment matches. `#` closes it, and so
            // does the Back button, because open-ness is a history entry.
            <div class="fui-modal" id="new-doctype">
                <a class="fui-modal__scrim" href="#" aria-label="Close"></a>
                <div class="fui-modal__panel" role="dialog" aria-modal="true" aria-labelledby="new-doctype-title">
                    <h2 class="fui-modal__title" id="new-doctype-title">"Create a DocType"</h2>
                    <p class="fui-modal__text">
                        "Metadata lands in the kernel, the schema syncs live, and the list and form render on the next request — no restarts."
                    </p>
                    <form method="post" action="/new-doctype">
                        frust_ui::fui_form_control(
                            label: "Name", required: true,
                            description: "lowercase, underscores — e.g. purchase_order",
                            frust_ui::fui_input(name: "name", placeholder: "purchase_order", required: true)
                        )
                        <div style="margin: 0.75rem 0;">
                            frust_ui::fui_checkbox(label: "Submittable (draft → submitted → cancelled)", name: "submittable")
                        </div>
                        <div class="fui-modal__actions">
                            frust_ui::fui_button(label: "Cancel", variant: "secondary", href: "#")
                            frust_ui::fui_button(label: "Create + sync", variant: "primary", kind: "submit")
                        </div>
                    </form>
                </div>
            </div>
        }
    }
}

#[derive(Deserialize)]
struct NewDoctype {
    name: String,
    #[serde(default)]
    submittable: Option<String>,
}

#[route(POST "/new-doctype")]
async fn new_doctype(cx: &Cx, Form(input): Form<NewDoctype>) -> Result<SeeOther> {
    let Some(s) = session(cx) else { return Ok(see_other("/login")) };
    let name: String =
        input.name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    if name.is_empty() {
        flash(cx, "The DocType needs a name.");
        return Ok(see_other("/"));
    }
    let meta = serde_json::json!({
        "name": name,
        "label": name.replace('_', " "),
        "submittable": input.submittable.is_some(),
        "fields": [
            { "fieldname": "title", "label": "Title", "fieldtype": "Data", "required": true },
            { "fieldname": "total", "label": "Total", "fieldtype": "Currency", "required": true },
            { "fieldname": "notes", "label": "Notes", "fieldtype": "Text", "allow_on_submit": true },
        ]
    });
    let (code, body) = kernel::call_async(Some(&s.token), "/doctype", &serde_json::json!({ "meta": meta })).await;
    if code != 200 {
        flash(cx, &kernel::friendly(code, &body));
        return Ok(see_other("/"));
    }
    Ok(see_other(&format!("/form/{name}")))
}

#[component]
async fn error_block(message: String, back: String) -> Result {
    view! {
        <h1>"Can't do that"</h1>
        <p style="color: #b00020; background: #fdecea; padding: 10px 14px; border-radius: 4px;">(&message)</p>
        <p><a href=(&back)>"← back"</a></p>
    }
}

// ── List view: Tier-0 shapes only ───────────────────────────────────────────

#[path_param(error = bad_request("bad doctype name"))]
struct DoctypeName(String);

#[derive(Default)]
struct ListQuery {
    month: Option<String>,
    field: Option<String>,
    value: Option<String>,
    sort: Option<String>,
    start: u64,
}

fn list_query(cx: &Cx) -> ListQuery {
    #[query_params(error = redirect("?"))]
    struct Raw {
        month: Option<String>,
        f: Option<String>,
        v: Option<String>,
        sort: Option<String>,
        start: Option<String>,
    }
    let raw = query_params::<Raw>(cx).ok();
    match raw {
        Some(r) => ListQuery {
            month: r.month.clone().filter(|m| !m.is_empty()),
            field: r.f.clone().filter(|f| !f.is_empty()),
            value: r.v.clone().filter(|v| !v.is_empty()),
            sort: r.sort.clone().filter(|s| !s.is_empty()),
            start: r.start.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0),
        },
        None => ListQuery::default(),
    }
}

const PAGE: u64 = 20;

/// WO-012: focus-scoped live updates (ADR-011).
///
/// The subscription is opened by the BROWSER against the kernel, carrying the
/// same session cookie every other request carries — a socket is a session.
/// It is scoped to FOCUS: opened when the list is visible, closed on
/// `pagehide`/`visibilitychange`, so the kernel's parked count tracks focused
/// views rather than open tabs (the writer tax is paid only for views someone
/// is actually looking at).
///
/// A tick carries `{action, id}` only — the client REFETCHES through the
/// normal read door, so the push path never becomes a second data path with
/// its own permission story. Budget refusal (429) is not an error here: the
/// meta-refresh above is already doing the job.
#[component]
async fn live_updates(doctype: &str) -> Result {
    // NOTE: view! escapes text, so this source contains no `<`, `>` or `&` —
    // hence `function ()` rather than arrows, and nested ifs rather than `&&`.
    // (The alternative is an asset bundle the Desk deliberately does without.)
    let script = format!(
        r#"(function () {{
  var TABLE = "{doctype}";
  var sub = null, timer = null, stopped = false;
  var es = null, sseFailed = false;

  // WO-032: SSE is the transport; polling below is the FALLBACK (REQ-6.5.2).
  // Realtime stays an enhancement — if the stream never establishes, or the
  // browser has no EventSource, the page degrades to the old poll and keeps
  // working. `onTick` is shared, so the dirty-guard contract (WO-014) is
  // identical on both paths.
  function onTick() {{
    if (window.__frustOnTick) window.__frustOnTick(); else location.reload();
  }}
  function openSse() {{
    if (stopped) return;
    if (typeof EventSource === "undefined") {{ sseFailed = true; open(); return; }}
    var opened = false;
    es = new EventSource("/live/sse/" + TABLE);
    es.addEventListener("tick", function () {{ opened = true; onTick(); }});
    es.addEventListener("idle", function () {{ opened = true; }});
    es.onopen = function () {{ opened = true; }};
    es.onerror = function () {{
      // EventSource retries on its own; only a stream that NEVER opened is
      // treated as unavailable (budget refusal, realtime off, no proxy support)
      if (opened) return;
      closeSse();
      if (!sseFailed) {{ sseFailed = true; open(); }}
    }};
  }}
  function closeSse() {{
    if (es) {{ es.close(); es = null; }}
  }}
  function open() {{
    if (sub) return;
    if (stopped) return;
    fetch("/live/subscribe/" + TABLE, {{ method: "POST" }}).then(function (r) {{
      if (!r.ok) return null;                  // 429 budget: polling covers it
      return r.json();
    }}).then(function (b) {{
      if (!b) return;
      sub = b.sub;
      timer = setInterval(poll, 3000);
    }}).catch(function () {{}});
  }}
  function poll() {{
    if (!sub) return;
    fetch("/live/events/" + sub, {{ method: "POST" }}).then(function (r) {{
      if (!r.ok) {{ close(); return null; }}
      return r.json();
    }}).then(function (b) {{
      if (!b) return;
      if (!b.alive) {{ close(); return; }}     // reconnect = refetch (ADR-011)
      // the dirty guard decides what a tick means (WO-014 criterion 5):
      // never stomp in-progress edits; a clean page still refreshes at once
      if (b.events) {{ if (b.events.length) {{ onTick(); }} }}
    }}).catch(function () {{ close(); }});
  }}
  function close() {{
    if (timer) {{ clearInterval(timer); timer = null; }}
    if (sub) {{ navigator.sendBeacon("/live/unsubscribe/" + sub); sub = null; }}
  }}
  function start() {{ if (sseFailed) open(); else openSse(); }}
  function stop() {{ closeSse(); close(); }}
  // blur stops the stream (the WO-012 writer-tax rule: pay only for views
  // someone is looking at); focus resumes on whichever transport is live
  document.addEventListener("visibilitychange", function () {{
    if (document.hidden) stop(); else start();
  }});
  window.addEventListener("pagehide", function () {{ stopped = true; stop(); }});
  if (!document.hidden) start();
}})();"#,
        doctype = doctype
    );
    view! {
        <script>(script)</script>
    }
}

/// The Tier-0 discipline (ADR-010, WO-007 rule table): period filters are
/// EQUALITY on the stored month field — this UI cannot express a raw date
/// range. Entity filters are equality on Select/Link fields.
#[page("/list/{doctype_name}")]
async fn list_page(cx: &Cx) -> Result {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let name = path_param::<DoctypeName>(cx)?.to_string();
    let dt = meta_one(&s, &name).await?;
    let q = list_query(cx);

    // build the contract filter from Tier-0 shapes only
    let mut clauses = Vec::new();
    let has_month = dt.fields.iter().any(|f| f.fieldname == "month");
    if let (true, Some(m)) = (has_month, &q.month) {
        clauses.push(serde_json::json!({ "path": "month", "op": "eq", "value": m }));
    }
    if let (Some(f), Some(v)) = (&q.field, &q.value) {
        if let Some(field) = dt.fields.iter().find(|df| &df.fieldname == f) {
            let val = typed_value(field, v);
            clauses.push(serde_json::json!({ "path": f, "op": "eq", "value": val }));
        }
    }
    let filter = match clauses.len() {
        0 => serde_json::Value::Null,
        1 => clauses[0].clone(),
        _ => serde_json::json!({ "and": clauses }),
    };
    let (sort_field, sort_dir) = match &q.sort {
        Some(spec) => match spec.split_once(':') {
            Some((f, d)) => (f.to_string(), d.to_string()),
            None => (spec.clone(), "asc".into()),
        },
        None => ("id".to_string(), "asc".to_string()),
    };
    let body = serde_json::json!({
        "filter": filter,
        "order": { "path": sort_field, "dir": sort_dir },
        "limit": PAGE,
        "start": q.start,
    });
    let (code, out) = kernel::call_async(Some(&s.token), &format!("/read/{name}"), &body).await;
    if code == 401 {
        return Err(redirect("/login").into());
    }
    if code != 200 {
        return view! { error_block(message: kernel::friendly(code, &out), back: "/".to_string()) };
    }
    let rows = out["rows"].as_array().cloned().unwrap_or_default();

    let cols: Vec<String> = dt.fields.iter().map(|f| f.fieldname.clone()).collect();
    let base_qs = |start: u64| -> String {
        let mut parts = vec![format!("start={start}")];
        if let Some(m) = &q.month { parts.push(format!("month={m}")); }
        if let (Some(f), Some(v)) = (&q.field, &q.value) { parts.push(format!("f={f}&v={v}")); }
        if let Some(so) = &q.sort { parts.push(format!("sort={so}")); }
        format!("?{}", parts.join("&"))
    };
    let months = recent_months();
    let is_manager = s.role == "manager";

    view! {
        // REQ-6.5.2: polling is the BASE, never removed — a 60 s meta-refresh
        // works with JS off, over a dead socket, past a budget refusal.
        // Realtime (below) only makes it feel instant when it is available.
        <meta http-equiv="refresh" content="60">
        live_updates(doctype: &name)
        <div class="fui-page-head">
            <h1 class="fui-page-title">(dt.label_or_name())</h1>
            frust_ui::fui_badge(label: format!("{} row(s)", rows.len()), color: "gray")
            <div class="fui-page-actions">
                frust_ui::fui_button(
                    label: format!("New {}", dt.label_or_name()),
                    variant: "primary", icon: "plus",
                    href: format!("/form/{name}")
                )
            </div>
        </div>
        <form method="get" class="fui-filters">
            if has_month {
                frust_ui::fui_form_control(
                    label: "Month",
                    frust_ui::fui_select(
                        name: "month",
                        <option value="">"All"</option>
                        for m in &months {
                            if Some(m) == q.month.as_ref() {
                                <option selected="selected">(m)</option>
                            } else {
                                <option>(m)</option>
                            }
                        }
                    )
                )
            }
            frust_ui::fui_form_control(
                label: "Field",
                frust_ui::fui_select(
                    name: "f",
                    <option value="">"(any)"</option>
                    for f in dt.fields.iter().filter(|f| matches!(f.fieldtype.as_str(), "Select" | "Link" | "Data")) {
                        if Some(&f.fieldname) == q.field.as_ref() {
                            <option value=(&f.fieldname) selected="selected">(f.label_or_name())</option>
                        } else {
                            <option value=(&f.fieldname)>(f.label_or_name())</option>
                        }
                    }
                )
            )
            frust_ui::fui_form_control(
                label: "Equals",
                frust_ui::fui_input(name: "v", value: q.value.clone().unwrap_or_default(), placeholder: "value")
            )
            <div style="display:flex; gap: 8px;">
                frust_ui::fui_button(label: "Apply", variant: "primary", kind: "submit")
                frust_ui::fui_button(label: "Clear", variant: "ghost", href: "?")
            </div>
        </form>
        <div class="fui-table-wrap">
            <table class="fui-table">
                <thead>
                    <tr>
                        <th><a href=(format!("/list/{name}{}", base_qs(0)).replace("start=0", "start=0&sort=id:asc"))>"ID"</a></th>
                        <th>"Status"</th>
                        if dt.submittable {
                            <th>"Docstatus"</th>
                        }
                        for c in &cols {
                            <th><a href=(format!("/list/{name}?sort={c}:asc"))>(c.replace('_', " "))</a></th>
                        }
                    </tr>
                </thead>
                <tbody>
                    if rows.is_empty() {
                        <tr>
                            <td colspan="12">
                                <div class="fui-empty">"Nothing here yet — nothing matches this filter."</div>
                            </td>
                        </tr>
                    }
                    for row in &rows {
                        <tr>
                            <td>
                                <a href=(format!(
                                    "/doc/{name}/{}",
                                    row["id"].as_str().unwrap_or("?").split_once(':').map(|p| p.1).unwrap_or("?")
                                ))>
                                    (row["id"].as_str().unwrap_or("?").to_string())
                                </a>
                            </td>
                            <td>(row["status"].as_str().unwrap_or("").to_string())</td>
                            if dt.submittable {
                                <td>
                                    frust_ui::fui_badge(
                                        label: docstatus_badge(row["docstatus"].as_i64().unwrap_or(0)),
                                        color: docstatus_color(row["docstatus"].as_i64().unwrap_or(0))
                                    )
                                </td>
                            }
                            for c in &cols {
                                <td>(cell(&row[c]))</td>
                            }
                        </tr>
                    }
                </tbody>
            </table>
        </div>
        <div class="fui-form-actions" style="border-top: 0;">
            if q.start >= PAGE {
                frust_ui::fui_button(label: "← Prev", variant: "secondary", href: format!("/list/{name}{}", base_qs(q.start - PAGE)))
            }
            if rows.len() as u64 == PAGE {
                frust_ui::fui_button(label: "Next →", variant: "secondary", href: format!("/list/{name}{}", base_qs(q.start + PAGE)))
            }
            if is_manager {
                <span style="color: var(--fui-ink-5); font-size: var(--fui-text-xs); align-self: center;">
                    "Open a row for its audit trail"
                </span>
            }
        </div>
    }
}

fn cell(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn docstatus_badge(ds: i64) -> String {
    match ds {
        0 => "Draft".into(),
        1 => "Submitted".into(),
        2 => "Cancelled".into(),
        n => format!("? ({n})"),
    }
}

/// WO-042: the lifecycle in colour. Draft is neutral because it is not yet a
/// claim about anything; submitted is green because it is; cancelled is red
/// because ADR-009 makes it terminal.
fn docstatus_color(ds: i64) -> &'static str {
    match ds {
        0 => "gray",
        1 => "green",
        2 => "red",
        _ => "amber",
    }
}

/// The month picker's options: last 18 months. Tier-0 by construction —
/// the UI offers equality buckets, never a range control.
/// **WO-046 / the money-formatting ruling: pad a stored decimal to scale, for
/// DISPLAY only.**
///
/// SurrealDB strips trailing zeros at write, so `37.50` is stored — and read
/// back — as `37.5`. That is faithful storage and ADR-007 is untouched by it;
/// what it is not is what a customer should read on an invoice.
///
/// Padding is not arithmetic: it appends zeros to a string. The stored value is
/// never touched, nothing is parsed into a float, and no total is recomputed —
/// which is exactly why the ruling permits it as presentation.
///
/// **A value with MORE places than the scale is returned VERBATIM, never
/// rounded.** The ruling is explicit: over-scale money is a defect to surface,
/// not to silently tidy. Money is stored *at* scale, so `1.005` in a 2-place
/// field means something upstream is wrong, and a display layer that quietly
/// printed `1.01` would hide it at the exact moment someone could still see it.
fn pad_money(raw: &str, scale: usize) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return String::new();
    }
    // Only plain decimals are padded. Anything else (a currency symbol, an
    // expression, text in a mistyped field) is passed through untouched rather
    // than half-formatted.
    let body = t.strip_prefix('-').unwrap_or(t);
    let (int, frac) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    if int.is_empty() && frac.is_empty() {
        return t.to_string();
    }
    if !int.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return t.to_string();
    }
    if frac.len() >= scale {
        return t.to_string(); // already at scale, or over it — surface, don't round
    }
    let mut out = t.to_string();
    if frac.is_empty() {
        out.push('.');
    }
    out.push_str(&"0".repeat(scale - frac.len()));
    out
}

/// The scale a Currency field displays at.
///
/// **Two, always, today** — DocType metadata carries no `precision`, so there is
/// nothing per-field to read. Named rather than hidden: a per-field scale (and
/// a currency symbol) is print-metadata vocabulary for the ADR-014 follow-on,
/// not something to hardcode per doctype here.
const MONEY_SCALE: usize = 2;

/// Today, ISO, UTC — same integers-only civil-date algorithm `recent_months`
/// already uses, so no dependency is added for a date.
fn today_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let z = (secs / 86400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mut m = mp + 3;
    if m > 12 {
        y += 1;
        m -= 12;
    }
    format!("{y:04}-{m:02}-{d:02}")
}

fn recent_months() -> Vec<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = now / 86400;
    // civil date from days since epoch (Howard Hinnant's algorithm, ints only)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let mut m = mp + 3;
    if m > 12 {
        y += 1;
        m -= 12;
    }
    let mut out = Vec::new();
    for _ in 0..18 {
        out.push(format!("{y:04}-{m:02}"));
        m -= 1;
        if m == 0 {
            m = 12;
            y -= 1;
        }
    }
    out
}

// ── Form: new record ────────────────────────────────────────────────────────

/// WO-014: the dynamic form. Every field gets its own VALUE signal, created
/// in a loop from runtime metadata (the dynamic-signals pattern), and the
/// declarative rules compile into expressions over those signals.
///
/// What that buys: `depends_on`, `read_only_when`, `required_when` and
/// `invalid_when` evaluate in the browser with ZERO round-trips. Only
/// `fetch_from` costs one, because only it needs the server.
///
/// Operators are selected at RENDER time (a `match` per rule) because `$()`
/// expressions are macro-expanded at compile time — the rule's operator is
/// data, so each arm carries its own compiled expression. That is the whole
/// trick behind "no recompile for a new rule on a new DocType".
#[page("/form/{doctype_name}")]
async fn form_page(cx: &Cx) -> Result {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let name = path_param::<DoctypeName>(cx)?.to_string();
    let dt = meta_one(&s, &name).await?;

    // one value signal per metadata field — created at run time, from data
    let values: Vec<Signal<String>> =
        dt.fields.iter().map(|_| Signal::new(String::new())).collect();
    let index_of = |fieldname: &str| dt.fields.iter().position(|f| f.fieldname == fieldname);

    // WO-042 behaviour 3: Link candidates for the new-record form, same rung
    // (b) round trip as the record page.
    let mut link_opts: Vec<(String, Vec<String>)> = Vec::new();
    for f in dt.fields.iter().filter(|f| f.fieldtype == "Link") {
        if let Some(target) = f.options.first() {
            link_opts.push((f.fieldname.clone(), link_options(&s, target).await));
        }
    }

    view! {
        <script type="module" src="/runtime.js"></script>
        <div class="fui-page-head">
            <h1 class="fui-page-title">"New " (dt.label_or_name())</h1>
            <div class="fui-page-actions">
                frust_ui::fui_button(label: "← List", variant: "ghost", href: format!("/list/{}", dt.name))
            </div>
        </div>
        <p style="color: var(--fui-ink-5); font-size: var(--fui-text-sm); margin-top: -0.5rem;">
            "Fields react to each other in the browser — no round-trips except "
            <code>"fetch_from"</code> "."
        </p>

        // declare every field signal to the browser runtime
        for sig in &values { (SignalDeclaration::new(sig)) }

        // WO-017: the lazy-load gate, and the whole of it. A scriptless
        // DocType emits nothing here, so the form costs exactly the document
        // plus the runtime — the engine is not merely deferred, it is absent.
        if dt.script().is_some() {
            <div id="script-status" style="display:none; padding:6px 10px; border-radius:4px; margin:6px 0;"></div>
            <script type="module" src=(format!("/engine-boot/{}", dt.name))></script>
        }

        <form id="doc-form" method="post" action=(format!("/submit/{}", dt.name))>
            for i in 0..dt.fields.len() {
                let field = &dt.fields[i];
                let val = &values[i];
                // depends_on: the source field's signal drives visibility
                let dep = field.depends_on.as_ref().and_then(|r| index_of(&r.field).map(|j| (r, &values[j])));
                let ro = field.read_only_when.as_ref().and_then(|r| index_of(&r.field).map(|j| (r, &values[j])));
                let req = field.required_when.as_ref().and_then(|r| index_of(&r.field).map(|j| (r, &values[j])));
                let bad = field.invalid_when.as_ref().and_then(|r| index_of(&r.field).map(|j| (r, &values[j])));
                let lopts: &[String] = link_opts
                    .iter()
                    .find(|(f, _)| f == &field.fieldname)
                    .map(|(_, o)| o.as_slice())
                    .unwrap_or(&[]);

                match dep {
                    Some((rule, src)) => {
                        let t = rule.target();
                        match rule.op.as_str() {
                            "ne" => {
                                <div :hidden=$(src.get() == t) class="fui-field">
                                    dyn_field(field: field, val: val, ro: ro, req: req, bad: bad, link_options: lopts)
                                </div>
                            }
                            "not_empty" => {
                                <div :hidden=$(src.get().is_empty()) class="fui-field">
                                    dyn_field(field: field, val: val, ro: ro, req: req, bad: bad, link_options: lopts)
                                </div>
                            }
                            "empty" => {
                                <div :hidden=$(!src.get().is_empty()) class="fui-field">
                                    dyn_field(field: field, val: val, ro: ro, req: req, bad: bad, link_options: lopts)
                                </div>
                            }
                            // default: eq
                            _ => {
                                <div :hidden=$(src.get() != t) class="fui-field">
                                    dyn_field(field: field, val: val, ro: ro, req: req, bad: bad, link_options: lopts)
                                </div>
                            }
                        }
                    }
                    None => {
                        <div class="fui-field">
                            dyn_field(field: field, val: val, ro: ro, req: req, bad: bad, link_options: lopts)
                        </div>
                    }
                }
            }
            <div class="fui-form-actions">
                frust_ui::fui_button(label: "Save draft", variant: "primary", kind: "submit")
            </div>
        </form>
    }
}

/// One dynamic field: label (with a reactive required marker), the input
/// bound to its signal, and a reactive validation message.
#[component]
async fn dyn_field<'a>(
    field: &'a DocField,
    val: &'a Signal<String>,
    ro: Option<(&'a Rule, &'a Signal<String>)>,
    req: Option<(&'a Rule, &'a Signal<String>)>,
    bad: Option<(&'a Rule, &'a Signal<String>)>,
    #[default(&[])] link_options: &'a [String],
) -> Result {
    view! {
        <label class="fui-field__label">
            (field.label_or_name())
            if field.required {
                <span class="fui-field__req">" *"</span>
            }
            // required_when: the marker itself is reactive
            match req {
                Some((rule, src)) => {
                    let t = rule.target();
                    if rule.op == "ne" {
                        <span class="fui-field__req" :hidden=$(src.get() == t)>" *"</span>
                    } else {
                        <span class="fui-field__req" :hidden=$(src.get() != t)>" *"</span>
                    }
                }
                None => {}
            }
        </label>

        // read_only_when: :disabled tracks the rule
        match ro {
            Some((rule, src)) => {
                let t = rule.target();
                // `read_only_when: f eq v` disables while the source EQUALS v
                if rule.op == "ne" {
                    field_input(field: field, val: val, disabled_when_eq: None, disabled_when_ne: Some((src, t)), link_options: link_options)
                } else {
                    field_input(field: field, val: val, disabled_when_eq: Some((src, t)), disabled_when_ne: None, link_options: link_options)
                }
            }
            None => {
                field_input(field: field, val: val, disabled_when_eq: None, disabled_when_ne: None, link_options: link_options)
            }
        }

        // invalid_when: client-side validation message, zero round-trips.
        // Money comparisons go through the Decimal surrogate — exact, and
        // structurally incapable of computing a stored value.
        match bad {
            Some((rule, src)) => {
                let t = rule.target();
                let msg = rule.text();
                // the comparison target is parsed ONCE, server-side, and
                // captured — the browser only compares, never computes
                let limit = Decimal::parse_or_zero(&t);
                match rule.op.as_str() {
                    "gt" => <span class="fui-field__error"
                        :hidden=$(!(src.get().to_decimal_or_zero() > limit))>(msg)</span>,
                    "lt" => <span class="fui-field__error"
                        :hidden=$(!(src.get().to_decimal_or_zero() < limit))>(msg)</span>,
                    "ge" => <span class="fui-field__error"
                        :hidden=$(!(src.get().to_decimal_or_zero() >= limit))>(msg)</span>,
                    "empty" => <span class="fui-field__error"
                        :hidden=$(!src.get().is_empty())>(msg)</span>,
                    _ => <span class="fui-field__error"
                        :hidden=$(src.get() != t)>(msg)</span>,
                }
            }
            None => {}
        }
    }
}

#[component]
async fn field_input<'a>(
    field: &'a DocField,
    val: &'a Signal<String>,
    disabled_when_eq: Option<(&'a Signal<String>, String)>,
    disabled_when_ne: Option<(&'a Signal<String>, String)>,
    #[default(&[])] link_options: &'a [String],
) -> Result {
    let list_id = format!("opts-{}", field.fieldname);
    view! {
        // WO-042 behaviour 3 on the dynamic form: the <datalist> is inert
        // markup, so it composes with the WO-014 signal bindings on the input
        // below without either knowing about the other.
        if !link_options.is_empty() {
            <datalist id=(&list_id)>
                for opt in link_options {
                    <option value=(opt)></option>
                }
            </datalist>
        }
        if field.fieldtype == "Select" {
            match (disabled_when_eq, disabled_when_ne) {
                (Some((src, t)), _) => {
                    <select class="fui-select" name=(&field.fieldname) :disabled=$(src.get() == t)
                        @change=$(|e: topcoat::runtime::Event| val.set(e.target.value))>
                        for opt in &field.options { <option>(opt)</option> }
                    </select>
                }
                (_, Some((src, t))) => {
                    <select class="fui-select" name=(&field.fieldname) :disabled=$(src.get() != t)
                        @change=$(|e: topcoat::runtime::Event| val.set(e.target.value))>
                        for opt in &field.options { <option>(opt)</option> }
                    </select>
                }
                _ => {
                    <select class="fui-select" name=(&field.fieldname)
                        @change=$(|e: topcoat::runtime::Event| val.set(e.target.value))>
                        for opt in &field.options { <option>(opt)</option> }
                    </select>
                }
            }
        } else if field.fieldtype == "Text" {
            <textarea class="fui-textarea" name=(&field.fieldname) rows="3" cols="48"
                @input=$(|e: topcoat::runtime::Event| val.set(e.target.value))></textarea>
        } else {
            // Currency renders as text + inputmode: decimal in, decimal out
            match (disabled_when_eq, disabled_when_ne) {
                (Some((src, t)), _) => {
                    <input class="fui-input" type="text" name=(&field.fieldname) list=(&list_id) :disabled=$(src.get() == t)
                        @input=$(|e: topcoat::runtime::Event| val.set(e.target.value))>
                }
                (_, Some((src, t))) => {
                    <input class="fui-input" type="text" name=(&field.fieldname) list=(&list_id) :disabled=$(src.get() != t)
                        @input=$(|e: topcoat::runtime::Event| val.set(e.target.value))>
                }
                _ => {
                    <input class="fui-input" type="text" name=(&field.fieldname) list=(&list_id)
                        @input=$(|e: topcoat::runtime::Event| val.set(e.target.value))>
                }
            }
        }
    }
}

/// WO-015 criterion 6: the per-line diff, computed presentation-side.
///
/// ADR-008 A4 recorded that embedded children make the changefeed store
/// whole-document entries, and promised the per-line view as a *computation*
/// over before/after rather than a storage change. This is that computation,
/// minimally: pair rows by index, report added / removed / changed fields.
///
/// A feed entry is either `{update: <doc>}` (create) or `{current: <after>,
/// update: [json-patch undo ops]}`; for the changed case the "before" lines
/// are reconstructed from the patch ops that target `/lines`.
fn line_diffs(entry: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let empty = Vec::new();
    for ch in entry.get("changes").and_then(|c| c.as_array()).unwrap_or(&empty) {
        let Some(after) = ch.get("current").or_else(|| ch.get("update")).filter(|v| v.is_object())
        else {
            continue;
        };
        let after_lines = after.get("lines").and_then(|l| l.as_array()).cloned().unwrap_or_default();

        // undo-patch ops rebuild the previous rows for a changed document
        let mut before_lines = after_lines.clone();
        let mut touched = false;
        if let Some(ops) = ch.get("update").and_then(|u| u.as_array()) {
            for op in ops {
                let Some(path) = op.get("path").and_then(|p| p.as_str()) else { continue };
                if !path.starts_with("/lines") {
                    continue;
                }
                touched = true;
                let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
                match (op.get("op").and_then(|o| o.as_str()), parts.as_slice()) {
                    (Some("add"), ["lines", i]) => {
                        if let (Ok(i), Some(v)) = (i.parse::<usize>(), op.get("value")) {
                            if i <= before_lines.len() {
                                before_lines.insert(i, v.clone());
                            }
                        }
                    }
                    (Some("remove"), ["lines", i]) => {
                        if let Ok(i) = i.parse::<usize>() {
                            if i < before_lines.len() {
                                before_lines.remove(i);
                            }
                        }
                    }
                    (Some("replace"), ["lines", i, field]) => {
                        if let (Ok(i), Some(v)) = (i.parse::<usize>(), op.get("value")) {
                            if let Some(row) = before_lines.get_mut(i) {
                                row[*field] = v.clone();
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if !touched && ch.get("current").is_some() {
            continue; // an update that did not touch lines
        }

        let label = |row: &serde_json::Value| -> String {
            row.get("item").and_then(|i| i.as_str()).unwrap_or("(row)").to_string()
        };
        let n = before_lines.len().max(after_lines.len());
        for i in 0..n {
            match (before_lines.get(i), after_lines.get(i)) {
                (None, Some(a)) => out.push(format!("+ added   {}", label(a))),
                (Some(b), None) => out.push(format!("- removed {}", label(b))),
                (Some(b), Some(a)) if b != a => {
                    let fields = a.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
                    for f in fields {
                        if b.get(&f) != a.get(&f) {
                            out.push(format!(
                                "~ {} · {}: {} -> {}",
                                label(a),
                                f,
                                cell(&b[&f]),
                                cell(&a[&f])
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// WO-015: the child-table line editor.
///
/// Rows render from the CHILD DocType's own metadata (Tier-1: a child type
/// created at runtime gets an editor with no recompile). Existing rows plus
/// `SPARE_ROWS` blanks are rendered up front, each with its own signals; the
/// "Add row" button reveals the next blank by bumping one counter signal, so
/// adding a row costs no DOM creation and no round-trip.
///
/// Deliberately NOT a grid: no per-cell reactivity, no bulk edit, no ranges.
/// That boundary is ADR-004 revisit-trigger #1 and it stays armed.
#[component]
async fn line_editor<'a>(
    parent_field: &'a str,
    child: &'a DocType,
    rows: &'a [serde_json::Value],
    shown: &'a Signal<f64>,
    editable: bool,
    row_money: &'a [Signal<String>],
) -> Result {
    let existing = rows.len();
    let total_rows = existing + if editable { SPARE_ROWS } else { 0 };
    view! {
        <fieldset style="border:1px solid #ddd; border-radius:4px; padding:10px; margin:12px 0;">
            <legend style="font-weight:600;">(child.label_or_name()) " lines"</legend>
            <table style="width:100%; border-collapse:collapse;">
                <tr style="background:#f7f7f7;">
                    for cf in &child.fields {
                        <th style="text-align:left; padding:4px 6px; font-size:0.85rem;">(cf.label_or_name())</th>
                    }
                    if editable {
                        <th style="width:4rem; font-size:0.85rem;">"remove"</th>
                    }
                </tr>
                for i in 0..total_rows {
                    let idx = i as f64;
                    let existing_row = rows.get(i);
                    // rows beyond the existing ones stay hidden until revealed
                    let always = i < existing;
                    if always {
                        <tr>
                            line_cells(prefix: parent_field, i: i, child: child, row: existing_row,
                                       editable: editable, money: row_money.get(i))
                        </tr>
                    } else {
                        <tr :hidden=$(!(shown.get() > idx))>
                            line_cells(prefix: parent_field, i: i, child: child, row: existing_row,
                                       editable: editable, money: row_money.get(i))
                        </tr>
                    }
                }
            </table>
            if editable {
                <button type="button" style="margin-top:8px;"
                    @click=$(|_e| shown.set(shown.get() + 1.0))>"+ Add row"</button>
            }
        </fieldset>
    }
}

#[component]
async fn line_cells<'a>(
    prefix: &'a str,
    i: usize,
    child: &'a DocType,
    row: Option<&'a serde_json::Value>,
    editable: bool,
    money: Option<&'a Signal<String>>,
) -> Result {
    view! {
        for cf in &child.fields {
            let name = format!("{prefix}.{i}.{}", cf.fieldname);
            let value = row.map(|r| cell(&r[&cf.fieldname])).unwrap_or_default();
            <td style="padding:3px 6px;">
                if !editable {
                    // criterion 5: at Submitted rows freeze with the form
                    if value.is_empty() { <i style="color:#999;">"—"</i> } else { (&value) }
                } else if cf.fieldtype == "Currency" {
                    // criterion 4: decimals in/out; comparisons only — the
                    // line total is the server's job (see the total row)
                    match money {
                        Some(sig) => {
                            <input type="text" inputmode="decimal" name=(&name) value=(&value) size="10"
                                @input=$(|e: topcoat::runtime::Event| sig.set(e.target.value))>
                        }
                        None => {
                            <input type="text" inputmode="decimal" name=(&name) value=(&value) size="10">
                        }
                    }
                } else if cf.fieldtype == "Select" {
                    <select name=(&name)>
                        for opt in &cf.options {
                            if opt == &value { <option selected="selected">(opt)</option> }
                            else { <option>(opt)</option> }
                        }
                    </select>
                } else {
                    <input type="text" name=(&name) value=(&value) size="14">
                }
            </td>
        }
        if editable {
            <td style="text-align:center;">
                <input type="checkbox" name=(format!("{prefix}.{i}.__remove"))>
            </td>
        }
    }
}

/// WO-014 criterion 5: the dirty-field guard.
///
/// **Reconciliation rule:** a live tick may never discard un-saved input.
/// While the form is dirty, the realtime reload is suppressed and the tick
/// becomes a visible "changed elsewhere" banner; the user resolves it by
/// saving (their write wins, and the lattice/hooks still judge it) or by
/// reloading deliberately (they discard their own edits, knowingly). A clean
/// form still refreshes instantly, so the common case keeps its liveness.
#[component]
async fn dirty_guard() -> Result {
    // Delegated from `document`, so this does not depend on the form
    // existing yet (the guard renders above it).
    let script = r##"(function () {
  window.__frustDirty = false;
  document.addEventListener("input", function (e) {
    if (!e.target.closest) return;
    if (!e.target.closest("#doc-form")) return;
    window.__frustDirty = true;
    var b = document.getElementById("dirty-note");
    if (b) b.style.display = "block";
  });
  window.__frustOnTick = function () {
    if (!window.__frustDirty) { location.reload(); return; }
    var s = document.getElementById("stale-note");
    if (s) s.style.display = "block";
  };
})();"##;
    view! {
        <div id="dirty-note" style="display:none; color:#856404; background:#fff3cd; padding:6px 10px; border-radius:4px; margin:6px 0;">
            "Unsaved changes — live refresh is paused while you type."
        </div>
        <div id="stale-note" style="display:none; color:#721c24; background:#f8d7da; padding:6px 10px; border-radius:4px; margin:6px 0;">
            "This document changed elsewhere. Save to keep your edits, or reload to see theirs."
        </div>
        <script>(script)</script>
    }
}

/// Serves the vendored runtime script. Embedded at build time, so the Desk
/// stays a single binary with no asset bundle (WO-009 posture) while still
/// getting real signals.
/// **WO-038: admission is an attributable signal, not a mystery.**
///
/// The house rule is that silent misbehaviour is the enemy, and that applies
/// to overload too: an operator must be able to see *"the Desk shed 412
/// requests"* as its own named number, rather than inferring it from a spike
/// in 500s whose cause they have to guess. Deliberately unauthenticated and
/// dependency-free — it must answer while the Desk is too busy to serve a page,
/// which is exactly when it is worth reading.
#[route(GET "/admission")]
async fn admission_stats() -> Result<Js<String>> {
    let (inflight, served, shed, latency_ms) = kernel::stats();
    Ok(Js(format!(
        r#"{{"inflight":{inflight},"max_inflight":{},"served":{served},"shed":{shed},"kernel_latency_ms":{latency_ms}}}"#,
        kernel::max_inflight()
    )))
}

#[route(GET "/runtime.js")]
async fn runtime_js() -> Result<Js<&'static str>> {
    Ok(Js(include_str!(
        "../../topcoat/crates/topcoat-runtime/browser/dist/index.js"
    )))
}

// ── WO-017: the Tier-2 script engine, served lazily ─────────────────────────
//
// The engine is the SAME `script_engine.wasm` the kernel runs under wasmtime,
// put through `jco transpile` — one artifact, two hosts (ADR-007). Assets are
// embedded in the binary (the WO-009 no-asset-bundle posture holds); "lazy"
// is a statement about REQUESTS, not about binary size. Nothing below is
// fetched unless a DocType actually carries a script.

#[path_param(error = bad_request("bad engine file"))]
struct EngineFile(String);

/// Serves one engine asset. The transpiler emits Node-shaped specifiers, so
/// they are rewritten to sibling URLs on the way out — this keeps the served
/// tree flat and, more importantly, means the page needs no import map (an
/// import map would have to survive `view!`'s HTML escaping, which is a trap
/// we already paid for once in WO-014).
#[route(GET "/engine/{engine_file}")]
async fn engine_asset(cx: &Cx) -> Result<Response> {
    let file = path_param::<EngineFile>(cx)?.to_string();
    if file == "script_engine.core.wasm" {
        return Wasm(include_bytes!("../assets/engine/script_engine.core.wasm").as_slice())
            .into_response(cx);
    }
    let js: String = match file.as_str() {
        "script_engine.js" => include_str!("../assets/engine/script_engine.js")
            .replace("'../host-api.js'", "'./host-api.js'")
            .replace("'@bytecodealliance/preview2-shim/cli'", "'./cli.js'")
            .replace("'@bytecodealliance/preview2-shim/clocks'", "'./clocks.js'")
            .replace("'@bytecodealliance/preview2-shim/io'", "'./io.js'")
            .replace("'@bytecodealliance/preview2-shim/random'", "'./random.js'"),
        "worker.js" => include_str!("../assets/engine/worker.js").into(),
        "host-api.js" => include_str!("../assets/engine/host-api.js").into(),
        "cli.js" => include_str!("../assets/engine/cli.js").into(),
        "clocks.js" => include_str!("../assets/engine/clocks.js").into(),
        "io.js" => include_str!("../assets/engine/io.js").into(),
        "random.js" => include_str!("../assets/engine/random.js").into(),
        "config.js" => include_str!("../assets/engine/config.js").into(),
        "environment.js" => include_str!("../assets/engine/environment.js").into(),
        _ => return Err(bad_request("no such engine file").into()),
    };
    Js(js).into_response(cx)
}

/// The per-DocType boot module: the loader plus that DocType's script, in one
/// request. Generated rather than static because the script text has to reach
/// the engine somehow, and embedding it in the page would put user-authored
/// JS through `view!`'s escaper.
///
/// `_setEnv` is the seam the engine already reads (`FRUST_SCRIPT`), so the
/// browser host feeds the script exactly the way the kernel host does.
#[route(GET "/engine-boot/{doctype_name}")]
async fn engine_boot(cx: &Cx) -> Result<Js<String>> {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let name = path_param::<DoctypeName>(cx)?.to_string();
    let dt = meta_one(&s, &name).await?;
    let Some(script) = dt.script() else {
        // No script: hand back an inert module rather than an error, so a
        // race against a just-edited DocType degrades to "does nothing".
        return Ok(Js("export {};".to_string()));
    };

    // fieldname -> WIT variant tag, so the doc crosses the boundary typed.
    // Currency is `decimal-v`: money stays a string on both sides (REQ-6.2.1).
    let kinds: serde_json::Map<String, serde_json::Value> = dt
        .fields
        .iter()
        .map(|f| {
            let tag = match f.fieldtype.as_str() {
                "Currency" => "decimal-v",
                "Int" => "int-v",
                "Check" => "bool-v",
                _ => "text-v",
            };
            (f.fieldname.clone(), serde_json::json!(tag))
        })
        .collect();

    // `to_string` on a JSON string yields a safe JS literal, which is why the
    // script never needs escaping by hand.
    let script_lit = serde_json::json!(script).to_string();
    let kinds_lit = serde_json::Value::Object(kinds).to_string();
    Ok(Js(format!(
        "{}\nconst SCRIPT = {script_lit};\nconst KINDS = {kinds_lit};\nboot(SCRIPT, KINDS);\n",
        include_str!("../assets/engine/boot.js")
    )))
}

// ── WO-017 item 4: client-script authoring (manager tier) ───────────────────
//
// The Frappe client-script UX with real isolation underneath: attach a script
// to a DocType through the UI, and the NEXT form load runs it — through the
// same lazy-load gate (item 1), under the same watchdog (item 2), behind the
// same decimal catch (item 3). The Desk renders role-appropriately, but the
// KERNEL's require_manager on the script endpoint is the enforcement; hiding
// the page is courtesy, not security (WO-008 posture).

#[page("/script/{doctype_name}")]
async fn script_page(cx: &Cx) -> Result {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let name = path_param::<DoctypeName>(cx)?.to_string();
    let dt = meta_one(&s, &name).await?;
    let current = dt.script().unwrap_or_default().to_string();
    let has_script = !current.is_empty();
    #[query_params(error = redirect("?"))]
    struct ScriptQuery {
        saved: Option<String>,
    }
    let saved = query_params::<ScriptQuery>(cx).ok().is_some_and(|q| q.saved.is_some());

    view! {
        <h1>"Client script — " (dt.label_or_name())</h1>
        if saved {
            <p style="color:#155724; background:#d4edda; padding:6px 10px; border-radius:4px;">
                "Saved. The next form load runs this version — nothing restarts."
            </p>
        }
        <p style="color:#666; max-width: 46rem;">
            "Runs in the form, sandboxed: no network, no DOM, no page access. "
            "It sees one object, " <code>"doc"</code> ", holding the form's fields. "
            "Mutate " <code>"doc"</code> " to change fields; " <code>"throw"</code>
            " to reject with a message. A script that loops or allocates is "
            "killed and the form keeps working."
        </p>
        <form method="post" action=(format!("/script-save/{}", dt.name))>
            <textarea name="script" rows="16" cols="90" spellcheck="false"
                style="font-family: ui-monospace, monospace; font-size: 0.9rem; width: 100%; max-width: 52rem;"
                placeholder="// e.g.\n// if (doc.status === &quot;Draft&quot; && Number(doc.total) > 10000) {\n//   doc.status = &quot;Needs Approval&quot;;\n// }">(&current)</textarea>
            <div style="margin-top: 8px; display: flex; gap: 0.8rem; align-items: baseline;">
                <button type="submit">"Save script"</button>
                if has_script {
                    <button type="submit" name="clear" value="1"
                        style="color:#b00020;">"Remove script"</button>
                }
                <a href=(format!("/form/{}", dt.name))>"open the form"</a>
                <a href="/">"back"</a>
            </div>
        </form>
        <h2 style="margin-top: 2rem; font-size: 1rem;">"Money in scripts"</h2>
        <p style="color:#666; max-width: 46rem;">
            "Currency fields arrive as exact decimal " <b>"strings"</b> " and must leave the same way. "
            "Bare arithmetic on one (" <code>"doc.total + 1"</code> ") concatenates or produces a float, "
            "and a float on a Currency field is " <b>"rejected, not stored"</b> ". The safe path:"
        </p>
        <pre style="background:#f6f6f6; padding: 10px 12px; border-radius: 4px; max-width: 46rem; overflow-x: auto;"><code>"var v = Number(doc.amount) * 3;   // compute
doc.amount = v.toFixed(2);        // round explicitly, write back a string"</code></pre>
        <p style="color:#666; max-width: 46rem;">
            <small>"Exact money arithmetic belongs on the server; a client script should route, flag and label money, not compute it."</small>
        </p>
    }
}

#[route(POST "/script-save/{doctype_name}")]
async fn script_save(cx: &Cx, Form(fields): Form<Vec<(String, String)>>) -> Result<SeeOther> {
    let Some(s) = session(cx) else { return Ok(see_other("/login")) };
    let name = path_param::<DoctypeName>(cx)?.to_string();
    let clear = fields.iter().any(|(k, _)| k == "clear");
    let script = if clear {
        String::new()
    } else {
        fields.iter().find(|(k, _)| k == "script").map(|(_, v)| v.clone()).unwrap_or_default()
    };
    let (code, out) = kernel::call_async(
        Some(&s.token),
        &format!("/doctype/{name}/script"),
        &serde_json::json!({ "script": script }),
    ).await;
    if code != 200 {
        flash(cx, &kernel::friendly(code, &out));
        return Ok(see_other(&format!("/script/{name}")));
    }
    Ok(see_other(&format!("/script/{name}?saved=1")))
}

/// **WO-042: one field, rendered in Frust UI.**
///
/// `link_options` carries the candidate values for a `Link` field, fetched from
/// the kernel by the page handler — **behaviour 3, rung (b)**. See
/// [`link_options`] for why the round trip is the right rung here.
#[component]
async fn field_row(
    field: &DocField,
    value: String,
    editable: bool,
    #[default(&[])] link_options: &[String],
) -> Result {
    let list_id = format!("opts-{}", field.fieldname);
    view! {
        frust_ui::fui_form_control(
            label: field.label_or_name(),
            required: field.required,
            description: if field.fieldtype == "Link" && !link_options.is_empty() {
                "type to search"
            } else { "" },
            if !editable {
                // Read-only is a *state*, not a disabled input: the lattice
                // froze this field, and a greyed-out box that looks editable
                // would be lying about that.
                <div class="fui-readonly">
                    if value.is_empty() { <span class="fui-muted">"—"</span> } else { (&value) }
                </div>
            } else if field.fieldtype == "Select" {
                frust_ui::fui_select(
                    name: &field.fieldname,
                    for opt in &field.options {
                        if opt == &value {
                            <option selected="selected">(opt)</option>
                        } else {
                            <option>(opt)</option>
                        }
                    }
                )
            } else if field.fieldtype == "Link" {
                // ── BEHAVIOUR 3: combobox, rung (b) ──
                // The options came from the kernel on this render; `<datalist>`
                // gives native typeahead over them with no JS. Where the set is
                // capped, the hint says so rather than silently truncating.
                <div class="fui-combobox">
                    frust_ui::fui_input(
                        name: &field.fieldname, value: value.clone(),
                        list: &list_id, placeholder: "start typing…"
                    )
                </div>
                <datalist id=(&list_id)>
                    for opt in link_options {
                        <option value=(opt)></option>
                    }
                </datalist>
                if link_options.len() >= LINK_OPTION_CAP as usize {
                    <p class="fui-combobox__hint">
                        "showing the first " (LINK_OPTION_CAP) " — type a full value if yours is not listed"
                    </p>
                }
            } else if field.fieldtype == "Text" {
                frust_ui::fui_textarea(name: &field.fieldname, value: &value, rows: "3")
            } else if field.fieldtype == "Currency" {
                // decimal in, decimal out — money never becomes a float
                frust_ui::fui_input(
                    name: &field.fieldname, value: value.clone(),
                    kind: "text", placeholder: "0.00"
                )
            } else if field.fieldtype == "Check" {
                frust_ui::fui_checkbox(
                    label: field.label_or_name(), name: &field.fieldname,
                    checked: value == "true"
                )
            } else {
                frust_ui::fui_input(name: &field.fieldname, value: value.clone())
            }
        )
    }
}

/// How many `Link` candidates a page ships inline for native typeahead.
///
/// Deliberately capped and deliberately *stated in the UI* when hit: a
/// silently-truncated option list is a combobox that lies about what exists.
const LINK_OPTION_CAP: u64 = 50;

/// **Behaviour 3's server round trip** — the kernel holds the options, so the
/// Desk asks it.
///
/// Rung (a) is impossible here: CSS cannot filter on typed text. Rung (c) — the
/// six-verb bridge — would trade one round trip for a client runtime the Desk
/// does not otherwise need, so the ranked order stops at (b), exactly where
/// WO-037 predicted it would for link fields.
///
/// A failure returns no options, which degrades to a plain text input. That is
/// the right failure: the field still submits, it just loses the affordance.
async fn link_options(s: &Session, target: &str) -> Vec<String> {
    // **The label field comes from METADATA, not from a guessed name.** The
    // first attempt probed `name` / `title` and fell back to the record id —
    // which on the seeded `customer` doctype (whose field is `cust_name`) would
    // have offered `customer:qfmpdzax2shlte1flelt` in the dropdown: technically
    // correct and useless, exactly what the fallback comment warned about.
    // Asking the DocType which field is its first `Data` field is the same
    // metadata-driven move the rest of the Desk makes.
    let label_field = match meta_one(s, target).await {
        Ok(meta) => meta
            .fields
            .iter()
            .find(|f| f.fieldtype == "Data")
            .map(|f| f.fieldname.clone()),
        Err(_) => None,
    };

    let body = serde_json::json!({ "limit": LINK_OPTION_CAP, "order": { "path": "id", "dir": "asc" } });
    let (code, out) = kernel::call_async(Some(&s.token), &format!("/read/{target}"), &body).await;
    if code != 200 {
        return Vec::new();
    }
    out["rows"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    label_field
                        .as_deref()
                        .and_then(|f| r[f].as_str())
                        .or_else(|| r["id"].as_str())
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[route(POST "/submit/{doctype_name}")]
async fn submit_new(cx: &Cx, Form(fields): Form<Vec<(String, String)>>) -> Result<SeeOther> {
    let Some(s) = session(cx) else { return Ok(see_other("/login")) };
    let name = path_param::<DoctypeName>(cx)?.to_string();
    let Ok(dt) = meta_one(&s, &name).await else {
        flash(cx, "That DocType does not exist.");
        return Ok(see_other("/"));
    };

    let mut doc = serde_json::Map::new();
    for f in &dt.fields {
        let raw = fields.iter().find(|(k, _)| k == &f.fieldname).map(|(_, v)| v.as_str()).unwrap_or("");
        doc.insert(f.fieldname.clone(), typed_value(f, raw));
    }
    let (code, body) = kernel::call_async(
        Some(&s.token),
        &format!("/write/{name}"),
        &serde_json::json!({ "doc": doc }),
    ).await;
    if code == 401 {
        return Ok(see_other("/login"));
    }
    if code != 200 {
        flash(cx, &kernel::friendly(code, &body));
        return Ok(see_other(&format!("/form/{name}")));
    }
    let id = body["created"]["id"].as_str().unwrap_or("?");
    let key = id.split_once(':').map(|p| p.1).unwrap_or("?");
    Ok(see_other(&format!("/doc/{name}/{key}")))
}

// ── Record view: the docstatus lifecycle as affordances ─────────────────────

#[path_param(error = bad_request("bad record key"))]
struct RecordKey(String);

#[page("/doc/{doctype_name}/{record_key}")]
async fn doc_page(cx: &Cx) -> Result {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let name = path_param::<DoctypeName>(cx)?.to_string();
    let key = path_param::<RecordKey>(cx)?.to_string();
    let dt = meta_one(&s, &name).await?;

    let body = serde_json::json!({
        "filter": { "path": "id", "op": "eq", "value": { "kind": "record", "v": format!("{name}:{key}") } }
    });
    let (code, out) = kernel::call_async(Some(&s.token), &format!("/read/{name}"), &body).await;
    if code == 401 {
        return Err(redirect("/login").into());
    }
    if code != 200 {
        return view! { error_block(message: kernel::friendly(code, &out), back: format!("/list/{name}")) };
    }
    let Some(row) = out["rows"].as_array().and_then(|a| a.first()).cloned() else {
        return view! { error_block(
            message: "No such record — or it isn't yours to see.".to_string(),
            back: format!("/list/{name}"),
        ) };
    };

    let ds = row["docstatus"].as_i64().unwrap_or(0);
    // the lifecycle as affordances; the lattice EVENT is the floor beneath
    let (ds_state, editable_all, editable_allowed, ds_can_submit, can_cancel) = if !dt.submittable {
        ("".to_string(), true, true, false, false)
    } else {
        match ds {
            0 => ("Draft".to_string(), true, true, true, false),
            1 => ("Submitted".to_string(), false, true, false, true),
            _ => ("Cancelled".to_string(), false, false, false, false),
        }
    };
    let is_manager = s.role == "manager";

    // ── WO-031: workflow transition buttons ──
    // Ask the kernel what THIS user's role may do from THIS document's state.
    // The kernel computes it from `available(state, role)` — the same data its
    // judge uses — so a button that renders is a transition that will be
    // allowed. No workflow → `workflow: null`, and the raw docstatus
    // affordances below stand unchanged (criterion 1).
    let (wf_code, wf_out) = kernel::call_async(Some(&s.token), &format!("/workflow/{name}/{key}"), &serde_json::json!({})).await;
    let under_workflow = wf_code == 200 && wf_out["workflow"].is_string();
    let wf_state = wf_out["state"].as_str().unwrap_or("").to_string();
    let wf_actions: Vec<String> = wf_out["actions"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t["action"].as_str().map(String::from)).collect())
        .unwrap_or_default();
    // Under a workflow the transition buttons are the ONLY lifecycle
    // affordance: the raw docstatus submit/cancel bypass the judge, so they
    // are suppressed and the workflow drives docstatus through `/transition`.
    let can_submit = ds_can_submit && !under_workflow;
    let can_cancel = can_cancel && !under_workflow;
    // The badge shows the workflow state when there is one, else the docstatus.
    let state = if under_workflow { wf_state.clone() } else { ds_state };

    // ── WO-015: child tables ──
    // Each Table field names its child DocType in `options`; the child's own
    // metadata drives the row columns, so a child type created at runtime
    // gets a working editor with no recompile.
    let mut child_meta: Vec<(String, DocType)> = Vec::new();
    let mut row_arrays: Vec<(String, Vec<serde_json::Value>)> = Vec::new();
    for f in dt.fields.iter().filter(|f| f.fieldtype == "Table") {
        if let Some(child_name) = f.options.first() {
            if let Ok(child) = meta_one(&s, child_name).await {
                child_meta.push((f.fieldname.clone(), child));
            }
        }
        let rows = row[&f.fieldname].as_array().cloned().unwrap_or_default();
        row_arrays.push((f.fieldname.clone(), rows));
    }
    // ── WO-042 behaviour 3: Link options, fetched from the kernel ──
    // One read per Link field on this document. Done here rather than inside
    // the field component because a component that reaches for the network is
    // a component you cannot render in a test.
    let mut link_opts: Vec<(String, Vec<String>)> = Vec::new();
    for f in dt.fields.iter().filter(|f| f.fieldtype == "Link") {
        if let Some(target) = f.options.first() {
            link_opts.push((f.fieldname.clone(), link_options(&s, target).await));
        }
    }

    // one reveal counter per table, and one money signal per rendered row
    // (row-scoped, so WO-014 rules can apply per line — criterion 3)
    let table_shown: Vec<Signal<f64>> =
        child_meta.iter().map(|_| Signal::new(0.0)).collect();
    let max_rows = row_arrays.iter().map(|(_, r)| r.len()).max().unwrap_or(0) + SPARE_ROWS;
    let line_money: Vec<Signal<String>> =
        (0..max_rows).map(|_| Signal::new(String::new())).collect();

    view! {
        <div class="fui-page-head">
            <h1 class="fui-page-title">(dt.label_or_name()) " " (&key)</h1>
            if dt.submittable || under_workflow {
                // Under a workflow the badge shows the WORKFLOW state, else the
                // docstatus — the same choice the buttons below are made from,
                // so the badge can never disagree with the affordances.
                frust_ui::fui_badge(
                    label: state.clone(),
                    color: if under_workflow { "blue" } else { docstatus_color(ds) },
                    dot: true
                )
            }
            <div class="fui-page-actions">
                frust_ui::fui_button(label: "← List", variant: "ghost", href: format!("/list/{name}"))
                // WO-046: the way to the document view. Every role gets it —
                // it reads through the same door under the same session, so it
                // can show nothing this page could not already show.
                frust_ui::fui_button(label: "Print", variant: "secondary", href: format!("/print/{name}/{key}"))
                if is_manager {
                    frust_ui::fui_button(label: "Audit trail", variant: "secondary", href: format!("/audit/{name}/{key}"))
                }
            </div>
        </div>
        if ds == 2 {
            frust_ui::fui_alert(
                variant: "danger", title: "Cancelled",
                "This document is cancelled. It is permanently read-only."
            )
        }
        // WO-014 criterion 5 — the reconciliation rule, stated:
        // a realtime tick NEVER overwrites a form the user is editing. The
        // page marks itself dirty on first input; the live script's reload
        // is suppressed while dirty, and the tick surfaces as a banner the
        // user dismisses by saving or reloading. Staleness is visible;
        // typing is never stomped (the classic Frappe annoyance, ruled).
        dirty_guard()
        <script type="module" src="/runtime.js"></script>
        // one reveal-counter signal per child table (WO-015)
        for sig in &table_shown { (SignalDeclaration::new(sig)) }
        for sig in &line_money { (SignalDeclaration::new(sig)) }
        <form id="doc-form" method="post" action=(format!("/save/{name}/{key}"))>
            for field in &dt.fields {
                if field.fieldtype == "Table" {
                    match child_meta.iter().find(|(fname, _)| fname == &field.fieldname) {
                        Some((_, child)) => {
                            let rows = row_arrays
                                .iter()
                                .find(|(fname, _)| fname == &field.fieldname)
                                .map(|(_, r)| r.as_slice())
                                .unwrap_or(&[]);
                            let shown = &table_shown[0];
                            line_editor(
                                parent_field: &field.fieldname,
                                child: child,
                                rows: rows,
                                shown: shown,
                                editable: editable_all,
                                row_money: &line_money,
                            )
                            // criterion 4, stated honestly in the UI: the
                            // total is computed by the server's hooks on
                            // save — never faked client-side
                            <p style="margin:4px 0 12px; color:#444;">
                                <b>"Total: "</b>
                                (cell(&row["total"]))
                                <small style="color:#888;">" (computed on save)"</small>
                            </p>
                        }
                        None => {}
                    }
                } else {
                    field_row(
                        field: field,
                        // criterion 3: the LATTICE still governs. A field shown
                        // by depends_on is still frozen at Submitted unless it
                        // is allow_on_submit — dynamism composes with the floor,
                        // it does not outrank it.
                        value: cell(&row[&field.fieldname]),
                        editable: editable_all || (editable_allowed && field.allow_on_submit),
                        link_options: link_opts
                            .iter()
                            .find(|(f, _)| f == &field.fieldname)
                            .map(|(_, o)| o.as_slice())
                            .unwrap_or(&[]),
                    )
                }
            }
            <div class="fui-form-actions">
                if editable_all || (editable_allowed && dt.fields.iter().any(|f| f.allow_on_submit)) {
                    frust_ui::fui_button(label: "Save", variant: "primary", kind: "submit", name: "action", value: "save")
                }
                if can_submit {
                    frust_ui::fui_button(label: "Submit", variant: "primary", kind: "submit", name: "action", value: "submit")
                }
                if can_cancel {
                    // A cancel is terminal under ADR-009's lattice, so it gets
                    // the destructive variant — the affordance should look like
                    // what it does.
                    frust_ui::fui_button(label: "Cancel document", variant: "danger", kind: "submit", name: "action", value: "cancel")
                }
            </div>
        </form>
        // WO-031: the workflow's transition buttons, in their own form so a
        // transition carries ONLY the action (the kernel judges on action +
        // current state, never on edited fields — approving is not saving).
        // These are the sole lifecycle affordance under a workflow; the raw
        // docstatus submit/cancel are suppressed above.
        if under_workflow && !wf_actions.is_empty() {
            <div style="margin-top: var(--fui-space-5);">
                frust_ui::fui_card(
                    title: format!("Workflow — {wf_state}"),
                    <form method="post" action=(format!("/transition/{name}/{key}")) style="display:flex; gap:8px; flex-wrap:wrap;">
                        for act in &wf_actions {
                            frust_ui::fui_button(
                                label: act.clone(),
                                variant: "primary", kind: "submit",
                                name: "action", value: act.as_str()
                            )
                        }
                    </form>
                )
            </div>
        }
    }
}

/// **WO-046: the document view — the artifact WO-045 found missing.**
///
/// The spike's finding was that browser print does not lack an engine, it lacks
/// a *document*: the record page is an editing form, and print CSS can make a
/// form tidy but it cannot make it an invoice. A printed form says `customer *`
/// and `Total: 15 (computed on save)` to a customer no matter how clean the
/// stylesheet is.
///
/// This is that document, and it is **generic** — DocType metadata plus record
/// JSON, exactly the ADR-004 contract the form already honours. No per-doctype
/// code exists here and none may be added; a new DocType created at runtime
/// gets a printable document with no recompile, which is the same claim the
/// list and the form already make.
///
/// It is also **the same HTML a PDF engine would consume**, which is the
/// one-dialect payoff ADR-014 banked when it deferred the engine.
#[page("/print/{doctype_name}/{record_key}")]
async fn print_page(cx: &Cx) -> Result {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let name = path_param::<DoctypeName>(cx)?.to_string();
    let key = path_param::<RecordKey>(cx)?.to_string();
    let dt = meta_one(&s, &name).await?;

    // **One door.** The read goes through the kernel under the CALLER'S OWN
    // session — the identical call the record page makes. Row and field
    // permissions therefore apply to the printed document by construction, not
    // by a second check that could drift: a clerk prints exactly what a clerk
    // can see, and a record they may not read is not printable either.
    let body = serde_json::json!({
        "filter": { "path": "id", "op": "eq", "value": { "kind": "record", "v": format!("{name}:{key}") } }
    });
    let (code, out) = kernel::call_async(Some(&s.token), &format!("/read/{name}"), &body).await;
    if code == 401 {
        return Err(redirect("/login").into());
    }
    if code != 200 {
        return view! { error_block(message: kernel::friendly(code, &out), back: format!("/doc/{name}/{key}")) };
    }
    let Some(row) = out["rows"].as_array().and_then(|a| a.first()).cloned() else {
        return view! { error_block(
            message: "No such record — or it isn't yours to see.".to_string(),
            back: format!("/list/{name}"),
        ) };
    };

    // child metadata drives the line columns, same as the editor
    let mut child_meta: Vec<(String, DocType)> = Vec::new();
    for f in dt.fields.iter().filter(|f| f.fieldtype == "Table") {
        if let Some(child_name) = f.options.first() {
            if let Ok(child) = meta_one(&s, child_name).await {
                child_meta.push((f.fieldname.clone(), child));
            }
        }
    }

    let ds = row["docstatus"].as_i64().unwrap_or(0);
    // The stored workflow state travels on the record itself (the kernel's
    // `workflow::STATE_FIELD`), so the document needs no extra round trip to
    // the `/workflow` endpoint — it is reporting state, not offering actions.
    let wf_state = row["workflow_state"].as_str().unwrap_or("").to_string();
    let state = if !wf_state.is_empty() {
        wf_state
    } else if dt.submittable {
        match ds {
            0 => "Draft".into(),
            1 => "Submitted".into(),
            _ => "Cancelled".into(),
        }
    } else {
        String::new()
    };

    // Scalar fields, already display-formatted. Table fields are rendered
    // below; engine fields are not document content.
    let scalars: Vec<(String, String)> = dt
        .fields
        .iter()
        .filter(|f| f.fieldtype != "Table")
        .map(|f| {
            let raw = cell(&row[&f.fieldname]);
            let shown = if f.fieldtype == "Currency" { pad_money(&raw, MONEY_SCALE) } else { raw };
            (f.label_or_name(), shown)
        })
        .filter(|(_, v)| !v.is_empty())
        .collect();

    view! {
        <article class="fui-doc">
            <header class="fui-doc__head">
                <div>
                    <div class="fui-doc__title">(dt.label_or_name())</div>
                    <div class="fui-doc__id">(&key)</div>
                </div>
                <div class="fui-doc__meta">
                    if !state.is_empty() {
                        <div>(state.clone())</div>
                    }
                    // Labelled "Printed", never bare. An unlabelled date on an
                    // invoice reads as the ISSUE date, and the record carries
                    // no issue date to print (see the build log's named gap) —
                    // so this says exactly what it is and claims nothing more.
                    <div>"Printed " (today_iso())</div>
                </div>
            </header>

            <dl class="fui-doc__fields">
                for (label, value) in &scalars {
                    <dt>(label)</dt>
                    <dd>(value)</dd>
                }
            </dl>

            for f in dt.fields.iter().filter(|f| f.fieldtype == "Table") {
                match child_meta.iter().find(|(fname, _)| fname == &f.fieldname) {
                    Some((_, child)) => {
                        let rows = row[&f.fieldname].as_array().cloned().unwrap_or_default();
                        <table class="fui-doc__lines">
                            <tr>
                                for cf in &child.fields {
                                    <th class=(if cf.fieldtype == "Currency" { "n" } else { "" })>
                                        (cf.label_or_name())
                                    </th>
                                }
                            </tr>
                            for line in &rows {
                                <tr>
                                    for cf in &child.fields {
                                        let raw = cell(&line[&cf.fieldname]);
                                        <td class=(if cf.fieldtype == "Currency" { "n" } else { "" })>
                                            (if cf.fieldtype == "Currency" { pad_money(&raw, MONEY_SCALE) } else { raw })
                                        </td>
                                    }
                                </tr>
                            }
                        </table>
                    }
                    None => {}
                }
            }

            <footer class="fui-doc__foot">
                // Screen-only: the affordance to leave. `.fui-btn` is already
                // hidden by the WO-045 print block, so paper never shows it.
                frust_ui::fui_button(label: "← Back to record", variant: "ghost",
                    href: format!("/doc/{name}/{key}"))
            </footer>
        </article>
    }
}

#[route(POST "/save/{doctype_name}/{record_key}")]
async fn save_doc(cx: &Cx, Form(fields): Form<Vec<(String, String)>>) -> Result<SeeOther> {
    let Some(s) = session(cx) else { return Ok(see_other("/login")) };
    let name = path_param::<DoctypeName>(cx)?.to_string();
    let key = path_param::<RecordKey>(cx)?.to_string();
    let Ok(dt) = meta_one(&s, &name).await else {
        flash(cx, "That DocType does not exist.");
        return Ok(see_other("/"));
    };
    let action = fields
        .iter()
        .find(|(k, _)| k == "action")
        .map(|(_, v)| v.as_str())
        .unwrap_or("save");

    let mut doc = serde_json::Map::new();
    match action {
        // lifecycle transitions carry ONLY docstatus — the lattice EVENT
        // under the write is the enforcement, this is just the affordance
        "submit" => {
            doc.insert("docstatus".into(), serde_json::json!({ "kind": "int", "v": 1 }));
        }
        "cancel" => {
            doc.insert("docstatus".into(), serde_json::json!({ "kind": "int", "v": 2 }));
        }
        _ => {
            for f in &dt.fields {
                // WO-015: a child table submits as the WHOLE embedded array,
                // so hooks and the Tier-2 line-differ see the full document
                // rather than a patch (ADR-008: children are embedded).
                if f.fieldtype == "Table" {
                    let Some(child_name) = f.options.first() else { continue };
                    let Ok(child) = meta_one(&s, child_name).await else { continue };
                    let rows = collect_rows(&f.fieldname, &child, &fields);
                    doc.insert(f.fieldname.clone(), serde_json::Value::Array(rows));
                } else if let Some((_, raw)) = fields.iter().find(|(k, _)| k == &f.fieldname) {
                    doc.insert(f.fieldname.clone(), typed_value(f, raw));
                }
            }
        }
    }
    let (code, body) = kernel::call_async(
        Some(&s.token),
        &format!("/write/{name}"),
        &serde_json::json!({ "doc": doc, "record": key }),
    ).await;
    if code == 401 {
        return Ok(see_other("/login"));
    }
    if code != 200 {
        flash(cx, &kernel::friendly(code, &body));
    }
    Ok(see_other(&format!("/doc/{name}/{key}")))
}

/// WO-031: drive a workflow transition. A UI affordance over the proven
/// `/transition` endpoint (WO-018) — the button carries an action, the kernel
/// judges it and the lattice EVENT backstops. The Desk reimplements no
/// transition logic; it posts the action and surfaces the typed refusal.
#[route(POST "/transition/{doctype_name}/{record_key}")]
async fn transition_doc(cx: &Cx, Form(fields): Form<Vec<(String, String)>>) -> Result<SeeOther> {
    let Some(s) = session(cx) else { return Ok(see_other("/login")) };
    let name = path_param::<DoctypeName>(cx)?.to_string();
    let key = path_param::<RecordKey>(cx)?.to_string();
    let Some((_, action)) = fields.iter().find(|(k, _)| k == "action") else {
        flash(cx, "No workflow action was given.");
        return Ok(see_other(&format!("/doc/{name}/{key}")));
    };
    let (code, body) = kernel::call_async(
        Some(&s.token),
        &format!("/transition/{name}/{key}"),
        &serde_json::json!({ "action": action }),
    ).await;
    if code == 401 {
        return Ok(see_other("/login"));
    }
    if code != 200 {
        // criterion 3: the typed E_WORKFLOW/E_DOCSTATUS refusal, as prose
        flash(cx, &kernel::friendly(code, &body));
    }
    Ok(see_other(&format!("/doc/{name}/{key}")))
}

// ── Live updates: session-scoped proxy to the kernel (WO-012) ───────────────
// The browser never holds a kernel token. These routes attach the session's
// bearer server-side, so a live subscription is exactly as authenticated as
// every other call — a socket is a session (kernel owns sessions, 🚫 bucket).

#[path_param(error = bad_request("bad name"))]
struct LiveName(String);

#[route(POST "/live/subscribe/{live_name}")]
async fn live_subscribe(cx: &Cx) -> Result<String> {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let table = path_param::<LiveName>(cx)?.to_string();
    let (code, body) = kernel::call_async(Some(&s.token), &format!("/subscribe/{table}"), &serde_json::json!({})).await;
    if code != 200 {
        // budget refusal / disabled realtime: the list keeps polling
        return Err(topcoat::router::error::bad_request("live unavailable").into());
    }
    Ok(body.to_string())
}

#[route(POST "/live/events/{live_name}")]
async fn live_events(cx: &Cx) -> Result<String> {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let sub = path_param::<LiveName>(cx)?.to_string();
    let (code, body) = kernel::call_async(Some(&s.token), &format!("/events/{sub}"), &serde_json::json!({})).await;
    if code != 200 {
        return Err(topcoat::router::error::bad_request("subscription gone").into());
    }
    Ok(body.to_string())
}

/// WO-032: the push transport — one long-lived SSE stream per focused view,
/// replacing the browser's 3 s poll.
///
/// **Why this does not pin a thread per subscriber (criterion 2).** The
/// kernel's `/events/{sub}` is a NON-BLOCKING drain — it locks, empties a Vec
/// and returns (`realtime.rs::drain`), so there is no long-poll to hold open.
/// The stream therefore sleeps with `tokio::time::sleep` (which releases the
/// worker) and only occupies a thread for the ~1 ms drain call itself. A
/// `std::thread::sleep` here — the naive shape — would pin one OS thread for
/// the whole subscription and cap the Desk at core-count viewers.
///
/// **Honest bound:** SSE removes *browser* polling, not Desk→kernel polling.
/// The kernel realtime API is drain-based, so the Desk still polls it on the
/// user's behalf; eliminating that needs a streaming kernel endpoint, which is
/// a kernel change and out of this WO's boundary (reported, not built).
///
/// Ticks carry `{action, id}` only (ADR-011) — the browser refetches through
/// the read door, so the push path never becomes a second data path with its
/// own permission story. The subscription runs under the SUBSCRIBER'S session,
/// so a clerk's stream carries only a clerk's events.
#[route(GET "/live/sse/{live_name}")]
async fn live_sse(cx: &Cx) -> Result<Sse<impl Stream<Item = Result<Event>> + use<>>> {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let table = path_param::<LiveName>(cx)?.to_string();
    let (code, body) = kernel::call_async(Some(&s.token), &format!("/subscribe/{table}"), &serde_json::json!({})).await;
    if code != 200 {
        // budget refusal (429) / realtime disabled: refuse the stream so the
        // client falls back to polling (REQ-6.5.2) rather than holding a
        // connection that will never tick.
        return Err(topcoat::router::error::bad_request("live unavailable").into());
    }
    let sub = body["sub"].as_str().unwrap_or_default().to_string();

    // The guard unsubscribes when the stream is dropped (browser closed the
    // connection), keeping the WO-012 per-table budget honest instead of
    // waiting out the kernel's 30 s idle reaper.
    let guard = SubGuard { token: s.token.clone(), sub: sub.clone() };

    let events = futures_util::stream::unfold(Some(guard), move |guard| {
        let sub = sub.clone();
        async move {
            let guard = guard?;
            // ASYNC sleep: the worker thread is released between drains.
            #[cfg(not(feature = "naive-blocking-sse"))]
            tokio::time::sleep(std::time::Duration::from_millis(LIVE_DRAIN_MS)).await;
            // WO-032 CONTROL (never shipped): the naive shape. Blocks the tokio
            // worker for the subscription's lifetime — build with
            // `--features naive-blocking-sse` to reproduce the stall this
            // design avoids. Exists so the measurement can be shown to FAIL.
            #[cfg(feature = "naive-blocking-sse")]
            std::thread::sleep(std::time::Duration::from_millis(LIVE_DRAIN_MS));
            let (code, body) =
                kernel::call_async(Some(&guard.token), &format!("/events/{sub}"), &serde_json::json!({})).await;
            if code != 200 || !body["alive"].as_bool().unwrap_or(false) {
                return None; // ends the stream; the guard drops and unsubscribes
            }
            let n = body["events"].as_array().map_or(0, |a| a.len());
            // One event per tick BATCH, not per row: the client's response is
            // "refetch", so N ticks and one tick mean the same thing.
            let ev = Event::new().event(if n > 0 { "tick" } else { "idle" }).data(n.to_string());
            Some((Ok(ev), Some(guard)))
        }
    });

    Ok(Sse::new(events).keep_alive(KeepAlive::new()))
}

/// How often the Desk drains the kernel on a subscriber's behalf. Matches the
/// old browser poll interval's responsiveness without the browser round trip.
const LIVE_DRAIN_MS: u64 = 1000;

/// Unsubscribes on drop, so a closed browser connection returns its slot to the
/// per-table budget immediately.
struct SubGuard {
    token: String,
    sub: String,
}

impl Drop for SubGuard {
    fn drop(&mut self) {
        // One short call; the kernel's idle reaper is the backstop if it fails.
        let _ = kernel::call(Some(&self.token), &format!("/unsubscribe/{}", self.sub), &serde_json::json!({}));
    }
}

#[route(POST "/live/unsubscribe/{live_name}")]
async fn live_unsubscribe(cx: &Cx) -> Result<String> {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let sub = path_param::<LiveName>(cx)?.to_string();
    let _ = kernel::call_async(Some(&s.token), &format!("/unsubscribe/{sub}"), &serde_json::json!({})).await;
    Ok(r#"{"ok":true}"#.to_string())
}

// ── Reports: rollup-backed, staleness visible ───────────────────────────────

#[page("/reports")]
async fn reports_index(cx: &Cx) -> Result {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let doctypes = meta_list(&s).await?;
    let mut entries: Vec<(String, String, String)> = Vec::new(); // (rollup, tier, source)
    for dt in &doctypes {
        for a in &dt.aggregates {
            let tier = if a.kind == "counter" { "Tier 1 · exact" } else { "Tier 2 · eventually consistent" };
            entries.push((a.rollup.clone(), tier.to_string(), dt.name.clone()));
        }
    }
    view! {
        <h1>"Reports"</h1>
        if entries.is_empty() {
            <p style="color:#666;">"No aggregates are declared yet. Declare one on a DocType's metadata and it appears here."</p>
        }
        <table border="0" cellpadding="6" style="border-collapse: collapse;">
            for (rollup, tier, source) in &entries {
                <tr style="border-bottom: 1px solid #eee;">
                    <td><a href=(format!("/report/{rollup}"))><b>(rollup.replace('_', " "))</b></a></td>
                    <td><small style="color:#666;">(tier)</small></td>
                    <td><small style="color:#666;">"from " (source)</small></td>
                </tr>
            }
        </table>
    }
}

#[path_param(error = bad_request("bad rollup name"))]
struct RollupName(String);

#[page("/report/{rollup_name}")]
async fn report_page(cx: &Cx) -> Result {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let rollup = path_param::<RollupName>(cx)?.to_string();
    let doctypes = meta_list(&s).await?;
    let agg = doctypes
        .iter()
        .flat_map(|dt| dt.aggregates.iter().map(move |a| (dt, a)))
        .find(|(_, a)| a.rollup == rollup);
    let Some((source_dt, agg)) = agg else {
        return view! { error_block(message: "No such report.".to_string(), back: "/reports".to_string()) };
    };
    let is_tier2 = agg.kind == "worker";
    let metric_names: Vec<String> = agg.metrics.iter().map(|m| m.name.clone()).collect();

    // the rollup is a DocType: read through the same contract as any record
    let body = serde_json::json!({ "order": { "path": "k", "dir": "asc" }, "limit": 200 });
    let (code, out) = kernel::call_async(Some(&s.token), &format!("/read/{rollup}"), &body).await;
    if code == 401 {
        return Err(redirect("/login").into());
    }
    if code != 200 {
        return view! { error_block(message: kernel::friendly(code, &out), back: "/reports".to_string()) };
    }
    let rows = out["rows"].as_array().cloned().unwrap_or_default();

    // Tier-2: staleness is data, shown, never hidden (ADR-010)
    let lag = if is_tier2 {
        let (lc, lb) = kernel::call_async(Some(&s.token), &format!("/lag/{rollup}"), &serde_json::json!({})).await;
        if lc == 200 { Some(lb) } else { None }
    } else {
        None
    };

    view! {
        <meta http-equiv="refresh" content="60">
        <h1>(rollup.replace('_', " "))</h1>
        <p style="color:#666;">
            if is_tier2 {
                "Tier-2 rollup (worker-maintained, eventually consistent) from "
            } else {
                "Tier-1 rollup (transaction-exact) from "
            }
            <b>(&source_dt.name)</b>
        </p>
        if let Some(l) = &lag {
            <p style=(format!(
                "padding: 6px 12px; border-radius: 4px; background: {};",
                if l["pending"].as_u64().unwrap_or(0) == 0 { "#d4edda" } else { "#fff3cd" }
            ))>
                "Staleness: " <b>(l["pending"].as_u64().unwrap_or(0))</b> " pending change(s)"
                if let Some(at) = l["cursor"]["updated_at"].as_str() {
                    " · cursor advanced " (at)
                }
            </p>
        }
        <table border="1" cellpadding="6" style="border-collapse: collapse; min-width: 32rem;">
            <tr style="background:#f7f7f7;">
                <th>"bucket"</th>
                <th>"count"</th>
                for m in &metric_names {
                    <th>(m)</th>
                }
                if metric_names.is_empty() {
                    <th>"qty"</th>
                    <th>"amount"</th>
                }
            </tr>
            for row in &rows {
                <tr>
                    <td>(row["k"].as_str().unwrap_or("?").to_string())</td>
                    <td style="text-align: right;">(cell(&row["n"]))</td>
                    for m in &metric_names {
                        <td style="text-align: right;">(cell(&row[m]))</td>
                    }
                    if metric_names.is_empty() {
                        <td style="text-align: right;">(cell(&row["qty"]))</td>
                        <td style="text-align: right;">(cell(&row["amount"]))</td>
                    }
                </tr>
            }
        </table>
    }
}

// ── Audit trail (manager surface) ───────────────────────────────────────────

#[page("/audit/{doctype_name}/{record_key}")]
async fn audit_page(cx: &Cx) -> Result {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let name = path_param::<DoctypeName>(cx)?.to_string();
    let key = path_param::<RecordKey>(cx)?.to_string();
    let (code, out) = kernel::call_async(Some(&s.token), &format!("/audit/{name}/{key}"), &serde_json::json!({})).await;
    if code == 401 {
        return Err(redirect("/login").into());
    }
    if code != 200 {
        return view! { error_block(message: kernel::friendly(code, &out), back: format!("/doc/{name}/{key}")) };
    }
    let entries = out["entries"].as_array().cloned().unwrap_or_default();
    view! {
        <h1>"Audit trail — " (&name) ":" (&key)</h1>
        <p><small style="color:#666;">
            "Storage-layer history from the table changefeed (REQ-3.2.1) — "
            (entries.len()) " of " (out["total"].as_u64().unwrap_or(0)) " feed entries touch this record."
        </small></p>
        // WO-015 criterion 6 — ADR-008 A4's promissory note, cashed.
        // The changefeed stores WHOLE-DOCUMENT entries, so a per-line diff
        // is a PRESENTATION-layer computation over the before/after arrays.
        // Here it is: the audit UI stops owing that answer.
        for entry in &entries {
            let diffs = line_diffs(entry);
            if !diffs.is_empty() {
                <div style="background:#eef7ee; border-left:3px solid #4a4; padding:6px 10px; margin:6px 0;">
                    <b style="font-size:0.85rem;">"Line changes"</b>
                    for d in &diffs {
                        <div style="font-size:0.85rem; font-family:monospace;">(d)</div>
                    }
                </div>
            }
            <pre style="background: #f4f4f4; padding: 8px; overflow-x: auto; border-radius: 4px;">(
                serde_json::to_string_pretty(entry).unwrap_or_default()
            )</pre>
        }
        <p><a href=(format!("/doc/{name}/{key}"))>"← back to the document"</a></p>
    }
}

#[cfg(test)]
mod tests {
    use super::{pad_money, MONEY_SCALE};

    /// The money-formatting ruling, pinned. The interesting cases are the two
    /// at the edges: SurrealDB's stripped trailing zero (which is why this
    /// helper exists) and the over-scale value (which must SURVIVE, not round).
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

    // ── WO-047 item 2: the CSS-seam guard ───────────────────────────────────
    //
    // The Rust->CSS seam has NO type system: a class name or custom property
    // that does not exist compiles, renders *nearly* right, and says nothing.
    // It has now caught its author three times — `--fui-space-*`/`--fui-font-sans`
    // (WO-042, first render in Times New Roman), `--fui-outline` (WO-046), and
    // `fui-alert--error` (this WO: every error flash in the Desk drew with no
    // colour and no icon). WO-042's own caveat set the promotion criterion —
    // "grep referenced-vs-defined if it recurs" — so it is wired in here where
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
             (a `var(--typo)` silently falls back to nothing — this is the WO-042 class)"
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
    /// decorative: WO-042's real bug was `fui-btn--solid`, and `.fui-badge--solid`
    /// exists — so the permissive version passed the planted bug. A guard that
    /// cannot fail on the defect it was written for is worse than none.
    #[test]
    fn every_component_variant_literal_has_a_class() {
        let mut bad = Vec::new();
        // The Rust function name and the CSS class STEM differ (`fui_button`
        // renders `.fui-btn--*`), so the pairing is stated explicitly. Deriving
        // it — `component.replace('-', "_")` — produced `fui_btn`, which matches
        // no call site, so the button half silently checked NOTHING and passed
        // the planted bug. Found only by planting it (WO-032's rule).
        for (component, fn_name, param) in [
            ("fui-btn", "fui_button", "variant"),
            ("fui-alert", "fui_alert", "variant"),
            ("fui-badge", "fui_badge", "color"),
        ] {
            let defined = defined_modifiers(CSS, component);
            assert!(!defined.is_empty(), "no `.{component}--*` classes found — did the CSS move?");
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
             (these COMPILE and render nearly-right — `fui-alert--error` shipped for two WOs, \
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
}
