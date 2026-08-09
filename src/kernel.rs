use topcoat::{
    Result,
    context::Cx,
    cookie::{Cookie, Cookies, cookies},
    router::{error::{internal_server_error, redirect, service_unavailable}, headers},
};

mod client {
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

    pub fn base() -> String {
        std::env::var("FRUST_KERNEL").unwrap_or_else(|_| "http://127.0.0.1:8790".into())
    }

    /// **One agent for the whole Desk.**
    ///
    /// This used to build a fresh `ureq::Agent` per call, which is the exact
    /// defect the kernel closed — a new TCP connection per request,
    /// so the Desk burned an ephemeral port per kernel round trip. Under
    /// overload, that is a second failure source on top
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
                // Browser SSE is Desk-generated; this agent carries only bounded kernel calls.
                .timeout_connect(Some(std::time::Duration::from_secs(10)))
                .timeout_global(Some(std::time::Duration::from_secs(60)))
                .max_idle_connections_per_host(pool)
                .max_idle_connections(pool)
                .build()
                .into()
        })
    }

    /// **The admission ceiling**, from `FRUST_DESK_MAX_INFLIGHT`.
    ///
    /// Default 64: load testing measured the throughput plateau at about 50
    /// concurrent calls, so 64 leaves modest scheduling headroom while keeping
    /// the queue bounded. Queueing hundreds deep only converts a fast refusal
    /// into a slow timeout.
    ///
    /// **Configurable because it is also the control.** Setting it absurdly
    /// high disables shedding and reproduces the earlier overload failure mode on
    /// demand, the same discipline as the `naive-blocking-sse` control: a
    /// guard whose failure mode cannot be reproduced decays into a number
    /// nobody trusts.
    pub fn max_inflight() -> usize {
        static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        *N.get_or_init(|| {
            bounded_max_inflight(
                std::env::var("FRUST_DESK_MAX_INFLIGHT")
                    .ok()
                    .as_deref(),
            )
        })
    }

    pub(super) fn bounded_max_inflight(raw: Option<&str>) -> usize {
        raw.and_then(|v| v.parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(64)
            .min(tokio::sync::Semaphore::MAX_PERMITS)
    }

    static INFLIGHT: AtomicI64 = AtomicI64::new(0);
    static SHED: AtomicU64 = AtomicU64::new(0);
    static SERVED: AtomicU64 = AtomicU64::new(0);
    /// Smoothed kernel round-trip latency, in microseconds.
    static LATENCY_US: AtomicU64 = AtomicU64::new(0);

    /// Smoothed kernel round-trip latency is still *reported* (`/admission`),
    /// because "how slow is the kernel right now" is worth an operator's
    /// glance. It is no longer *shed on*: the admission rewrite proved latency
    /// cannot see the queue (the kernel stayed at 29 ms while Desk pages took
    /// 2 376 ms), and the semaphore is the bound that can. Measuring something
    /// and deciding on it are different jobs.
    /// Feed the controller. Called on every kernel round trip.
    pub fn record_latency(us: u64) {
        // EWMA, 1/8 weight on the new sample: slow enough that a single slow
        // query cannot trip the shed, fast enough to react within a second of
        // real saturation.
        let prev = LATENCY_US.load(Ordering::Relaxed);
        let next = if prev == 0 {
            us
        } else {
            prev - prev / 8 + us / 8
        };
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
    /// `spawn_blocking` is the lazy-correct fix: it reuses the Desk's shared
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

    pub async fn get_async(token: Option<&str>, route: &str) -> (u16, serde_json::Value) {
        let (token, route) = (token.map(str::to_string), route.to_string());
        match tokio::task::spawn_blocking(move || get(token.as_deref(), &route)).await {
            Ok(out) => out,
            Err(e) => (
                502,
                serde_json::json!({ "error": { "kind": "transport", "detail": e.to_string() } }),
            ),
        }
    }

    pub fn get(token: Option<&str>, route: &str) -> (u16, serde_json::Value) {
        let started = std::time::Instant::now();
        let mut req = agent().get(format!("{}{}", base(), route));
        if let Some(t) = token {
            req = req.header("Authorization", &format!("Bearer {t}"));
        }
        let out = match req.call() {
            Ok(mut r) => {
                let code = r.status().as_u16();
                let parsed = r.body_mut().read_json().unwrap_or(serde_json::json!({}));
                (code, parsed)
            }
            Err(e) => (
                502,
                serde_json::json!({ "error": { "kind": "transport", "detail": e.to_string() } }),
            ),
        };
        record_latency(started.elapsed().as_micros() as u64);
        out
    }

    /// One kernel call, blocking. Non-2xx comes back as Ok((code, body)) so
    /// callers can render typed errors as user-facing messages.
    ///
    /// Still used directly from the few genuinely synchronous contexts (a
    /// `Drop` impl cannot await); every request path goes through
    /// [`call_async`].
    pub fn call(
        token: Option<&str>,
        route: &str,
        body: &serde_json::Value,
    ) -> (u16, serde_json::Value) {
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
            Err(e) => (
                502,
                serde_json::json!({ "error": { "kind": "transport", "detail": e.to_string() } }),
            ),
        };
        // feed the controller whether the call succeeded or failed: a call
        // that took four seconds and then errored is the strongest possible
        // evidence of saturation, and dropping it would blind the shed
        // exactly when it is needed
        record_latency(started.elapsed().as_micros() as u64);
        out
    }

    /// Typed kernel errors -> user-facing words. Engine internals stay out
    /// of the DOM: machine codes are translated, raw DB
    /// wording is dropped.
    pub fn friendly(code: u16, body: &serde_json::Value) -> String {
        let err = body.get("error").cloned().unwrap_or_default();
        let kind = err.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        let detail = err.get("detail").and_then(|d| d.as_str()).unwrap_or("");
        match kind {
            "hook-rejected" => format!(
                "Rejected by a validation rule: {}",
                err.get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("the document was not accepted")
            ),
            "identity-unresolved" => {
                "Your identity could not be resolved — contact an administrator.".into()
            }
            "permission-denied" if code == 401 => {
                "Your session has expired — please log in again.".into()
            }
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
            // The workflow judge's refusal. `detail` is already
            // user-facing prose ("'Approve' from 'Draft' requires role
            // manager; you are 'clerk'"), so this branch returns it verbatim.
            // The FRUST:E_WORKFLOW machine code never reaches here — `code` is
            // the HTTP status and `kind` is the discriminant — so engine
            // internals stay out of the UI.
            "workflow-denied" => detail.to_string(),
            "db" if detail.contains("FRUST:E_IDENTITY_UNRESOLVED") => {
                "Your identity could not be resolved — contact an administrator.".into()
            }
            _ => "Something went wrong on the server.".into(),
        }
    }
}

pub(crate) use client::{admit, call, call_async, friendly, get_async, max_inflight, stats};

// ── Session (kernel-owned; the Desk only carries the cookie) ────────────────

#[derive(Clone)]
pub(crate) struct Session {
    pub(crate) token: String,
    pub(crate) user: String,
    pub(crate) role: String,
}

pub(crate) fn session(cx: &Cx) -> Option<Session> {
    let jar = cookies(cx);
    Some(Session {
        token: jar.get("frust_session")?.value().to_string(),
        user: jar
            .get("frust_user")
            .map(|c| c.value().to_string())
            .unwrap_or_default(),
        role: jar
            .get("frust_role")
            .map(|c| c.value().to_string())
            .unwrap_or_default(),
    })
}

pub(crate) fn require_session(cx: &Cx) -> Result<Session, topcoat::Error> {
    session(cx).ok_or_else(|| redirect("/login").into())
}

fn tenant_from_host(host: &str) -> Option<&str> {
    let host = host.split(':').next()?.trim();
    if host.is_empty() || host.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    let labels: Vec<&str> = host.split('.').collect();
    (labels.len() >= 3 && !labels[0].is_empty()).then_some(labels[0])
}

pub(crate) fn request_tenant(cx: &Cx) -> Option<&str> {
    headers(cx)
        .get("host")
        .and_then(|value| value.to_str().ok())
        .and_then(tenant_from_host)
}

pub(crate) fn err500(msg: impl std::fmt::Display) -> topcoat::Error {
    internal_server_error(std::io::Error::other(msg.to_string())).into()
}

// ── Shed with intent ────────────────────────────────────────────────────────
//
// Past the ~50-concurrent knee the Desk answered **HTTP 500
// for ~45% of requests at 200 concurrent and ~85% at 500**. A 500 tells a user
// (and a load balancer, and an on-call engineer) that the system is *broken*.
// It was not broken — it was busy, propagating downstream saturation upward
// with the wrong word.
//
// This is failure-MODE work, not throughput work. Nothing here makes the Desk
// faster, and it must not: the ~135 req/s ceiling is a database
// conversation, deliberately out of scope. What changes is the answer given
// when that ceiling is exceeded.

/// A typed, honest "busy" — **503 with `Retry-After`**.
///
/// 503 rather than 429 on purpose: 429 is "*you* sent too many", which the
/// kernel already returns for its per-tenant budget. This is "*the
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
pub(crate) fn admit_or_busy() -> std::result::Result<client::Permit, topcoat::Error> {
    client::admit().ok_or_else(Busy::now)
}

/// **Turn a kernel status into the Desk's status honestly.**
///
/// The old `err500` collapsed *every* kernel failure into 500 — including the
/// kernel's own typed capacity answers. So a kernel that correctly said "429,
/// slow down" or "503, busy" was reported to the user as "the Desk is broken".
/// A capacity answer must survive the hop.
pub(crate) fn kernel_status(code: u16, body: &serde_json::Value) -> topcoat::Error {
    match code {
        401 => redirect("/login").into(),
        // the kernel's typed capacity answers (tenant budget, live-sub
        // budget, and any 503 it sheds itself) pass through as capacity
        429 | 503 => Busy::now(),
        // 502 is our own transport failure reaching the kernel — under load
        // that is congestion, not a defect in the request
        502 => Busy::now(),
        _ => err500(client::friendly(code, body)),
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
            if let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
            {
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

pub(crate) fn flash(cx: &Cx, msg: &str) {
    cookies(cx).add(
        Cookie::build(("frust_flash", pct_encode(msg)))
            .path("/")
            .build(),
    );
}

pub(crate) fn take_flash(cx: &Cx) -> Option<String> {
    let jar = cookies(cx);
    let msg = jar.get("frust_flash").map(|c| pct_decode(c.value()));
    if msg.is_some() {
        jar.remove(Cookie::build(("frust_flash", "")).path("/").build());
    }
    msg.filter(|m| !m.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{client::bounded_max_inflight, kernel_status, tenant_from_host};
    use topcoat::{context::Cx, router::{IntoResponse, StatusCode}};

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
    fn max_inflight_is_bounded_by_the_semaphore_limit() {
        assert_eq!(bounded_max_inflight(None), 64);
        assert_eq!(bounded_max_inflight(Some("0")), 64);
        assert_eq!(bounded_max_inflight(Some("17")), 17);
        assert_eq!(
            bounded_max_inflight(Some(&usize::MAX.to_string())),
            tokio::sync::Semaphore::MAX_PERMITS
        );
    }

    #[test]
    fn kernel_unauthorized_status_redirects_to_login() {
        let error = kernel_status(
            401,
            &serde_json::json!({ "error": { "kind": "permission-denied" } }),
        );
        let response = error
            .into_response(&Cx::default())
            .expect("redirect response");
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.headers()["location"], "/login");
    }

}
