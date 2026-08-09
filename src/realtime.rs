use crate::{admit, kernel, require_session};
use futures_core::Stream;
use topcoat::{
    Result,
    context::Cx,
    router::{
        content::sse::{Event, KeepAlive, Sse},
        path_param, route,
    },
    view::{component, view},
};

/// Visibility-scoped live updates.
///
/// The subscription is opened by the BROWSER against the kernel, carrying the
/// same session cookie every other request carries — a socket is a session.
/// It is scoped to VISIBILITY: opened when the page is visible, torn down when
/// `visibilitychange` reports `document.hidden` (and on `pagehide`), so the
/// kernel's parked count tracks visible views rather than open tabs (the
/// writer tax is paid only for views someone is actually looking at).
///
/// A tick carries `{action, id}` only — the client REFETCHES through the
/// normal read door, so the push path never becomes a second data path with
/// its own permission story. Budget refusal (429) is not an error here: the
/// meta-refresh above is already doing the job.
#[component]
pub(crate) async fn live_updates(doctype: &str) -> Result {
    // NOTE: view! escapes text, so this source contains no `<`, `>` or `&` —
    // hence `function ()` rather than arrows, and nested ifs rather than `&&`.
    // (The alternative is an asset bundle the Desk deliberately does without.)
    let script = format!(
        r#"(function () {{
  var TABLE = "{doctype}";
  var sub = null, timer = null, stopped = false;
  var es = null, sseFailed = false;

  // SSE is the transport; polling below is the FALLBACK.
  // Realtime stays an enhancement — if the stream never establishes, or the
  // browser has no EventSource, the page degrades to the old poll and keeps
  // working. `onTick` is shared, so the dirty-guard contract is
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
      // dead subscription: close() stops the poll timer and unsubscribes.
      // recovery is NOT immediate — the list comes back on the next
      // visibilitychange (start reopens the stream) or the 60 s meta-refresh
      // fallback, whichever fires first.
      if (!b.alive) {{ close(); return; }}
      // the dirty guard decides what a tick means:
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
  // becoming hidden stops the stream (the writer-tax rule: pay only for views
  // someone is looking at); becoming visible resumes whichever transport is live
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

// ── Live updates: session-scoped proxy to the kernel ────────────────────────
// The browser never holds a kernel token. These routes attach the session's
// bearer server-side, so a live subscription is exactly as authenticated as
// every other call — a socket is a session (the kernel owns sessions).

#[path_param(error = bad_request("bad name"))]
struct LiveName(String);

#[route(POST "/live/subscribe/{live_name}")]
async fn live_subscribe(cx: &Cx) -> Result<String> {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let table = path_param::<LiveName>(cx)?.to_string();
    let (code, body) = kernel::call_async(
        Some(&s.token),
        &format!("/subscribe/{table}"),
        &serde_json::json!({}),
    )
    .await;
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
    let (code, body) = kernel::call_async(
        Some(&s.token),
        &format!("/events/{sub}"),
        &serde_json::json!({}),
    )
    .await;
    if code != 200 {
        return Err(topcoat::router::error::bad_request("subscription gone").into());
    }
    Ok(body.to_string())
}

/// The push transport — one long-lived SSE stream per focused view,
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
/// a kernel change and out of scope here (reported, not built).
///
/// Ticks carry `{action, id}` only — the browser refetches through
/// the read door, so the push path never becomes a second data path with its
/// own permission story. The subscription runs under the SUBSCRIBER'S session,
/// so a clerk's stream carries only a clerk's events.
#[route(GET "/live/sse/{live_name}")]
async fn live_sse(cx: &Cx) -> Result<Sse<impl Stream<Item = Result<Event>> + use<>>> {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let table = path_param::<LiveName>(cx)?.to_string();
    let (code, body) = kernel::call_async(
        Some(&s.token),
        &format!("/subscribe/{table}"),
        &serde_json::json!({}),
    )
    .await;
    if code != 200 {
        // budget refusal (429) / realtime disabled: refuse the stream so the
        // client falls back to polling rather than holding a
        // connection that will never tick.
        return Err(topcoat::router::error::bad_request("live unavailable").into());
    }
    let sub = body["sub"].as_str().unwrap_or_default().to_string();

    // The guard unsubscribes when the stream is dropped (browser closed the
    // connection), keeping the per-table budget honest instead of
    // waiting out the kernel's 30 s idle reaper.
    let guard = SubGuard {
        token: s.token.clone(),
        sub: sub.clone(),
    };

    let events = futures_util::stream::unfold(Some(guard), move |guard| {
        let sub = sub.clone();
        async move {
            let guard = guard?;
            // ASYNC sleep: the worker thread is released between drains.
            #[cfg(not(feature = "naive-blocking-sse"))]
            tokio::time::sleep(std::time::Duration::from_millis(LIVE_DRAIN_MS)).await;
            // CONTROL (never shipped): the naive shape. Blocks the tokio
            // worker for the subscription's lifetime — build with
            // `--features naive-blocking-sse` to reproduce the stall this
            // design avoids. Exists so the measurement can be shown to FAIL.
            #[cfg(feature = "naive-blocking-sse")]
            std::thread::sleep(std::time::Duration::from_millis(LIVE_DRAIN_MS));
            let (code, body) = kernel::call_async(
                Some(&guard.token),
                &format!("/events/{sub}"),
                &serde_json::json!({}),
            )
            .await;
            if code != 200 || !body["alive"].as_bool().unwrap_or(false) {
                return None; // ends the stream; the guard drops and unsubscribes
            }
            let n = body["events"].as_array().map_or(0, |a| a.len());
            // One event per tick BATCH, not per row: the client's response is
            // "refetch", so N ticks and one tick mean the same thing.
            let ev = Event::new()
                .event(if n > 0 { "tick" } else { "idle" })
                .data(n.to_string());
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
        let _ = kernel::call(
            Some(&self.token),
            &format!("/unsubscribe/{}", self.sub),
            &serde_json::json!({}),
        );
    }
}

#[route(POST "/live/unsubscribe/{live_name}")]
async fn live_unsubscribe(cx: &Cx) -> Result<String> {
    let s = require_session(cx)?;
    let _permit = admit()?;
    let sub = path_param::<LiveName>(cx)?.to_string();
    let _ = kernel::call_async(
        Some(&s.token),
        &format!("/unsubscribe/{sub}"),
        &serde_json::json!({}),
    )
    .await;
    Ok(r#"{"ok":true}"#.to_string())
}

