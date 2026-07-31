// WO-017 — browser host for the Tier-2 script engine (items 1 + 2).
//
// This module is never fetched for a scriptless DocType; `form_page` emits the
// tag that pulls it only when metadata carries a script. Everything expensive
// (the 4 MB core module) hangs off the worker below, so the gate is a single
// decision made server-side and the client cannot accidentally defeat it.
//
// Item 2 moved the engine into a Worker. This file is the SUPERVISOR: it owns
// the budgets, the kill, the restart, and the circuit breaker — everything
// that must outlive a hostile script. It never calls the engine directly.

const WORKER_URL = "/engine/worker.js";

// Budgets. Init is generous because it covers compiling 4 MB of WebAssembly on
// whatever machine the user has; the per-call budget is tight because a
// validate hook that takes a quarter second is already broken. Both are
// measured on the happy path (see the build log) and set with margin, not
// guessed.
const INIT_BUDGET_MS = 15000;
const CALL_BUDGET_MS = 250;

// After this many kills the script is switched off for the page. Without it
// the watchdog becomes its own denial of service: a script that always spins
// would spawn, load 4 MB, and die on every keystroke, forever.
const MAX_STRIKES = 3;

// A kill notice is STICKY. The engine restarts after a kill, and the restarted
// engine usually succeeds a second later — which would quietly erase the only
// evidence that a script was forcibly stopped. "Terminated silently and
// recovered" is exactly the silent misbehaviour this project treats as the
// enemy, so a kill notice survives subsequent successes and only a newer
// message of its own severity replaces it.
let sticky = null;

function statusEl() {
  return document.getElementById("script-status");
}

function paint(text, bad) {
  const el = statusEl();
  if (!el) return;
  el.textContent = text;
  el.style.display = text ? "block" : "none";
  el.style.color = bad ? "#721c24" : "#155724";
  el.style.background = bad ? "#f8d7da" : "#d4edda";
}

function say(text, bad) {
  if (text) {
    paint(text, bad);
  } else if (sticky) {
    paint(sticky, true); // success does not erase a kill
  } else {
    paint("", false);
  }
}

function sayKilled(text) {
  sticky = text;
  paint(text, true);
}

// The form's inputs -> the WIT entry list. Only fields the engine was told
// about are sent; anything else is not the script's business.
function readDoc(form, kinds) {
  const doc = [];
  for (const name of Object.keys(kinds)) {
    const input = form.elements[name];
    if (!input) continue;
    let v = input.type === "checkbox" ? input.checked : input.value;
    const tag = kinds[name];
    if (tag === "int-v") v = BigInt(parseInt(v, 10) || 0);
    else if (tag === "bool-v") v = Boolean(v);
    else v = String(v);
    doc.push({ key: name, val: { tag: tag, val: v } });
  }
  return doc;
}

// True while the engine's own output is being written back into the form.
//
// `applyDoc` dispatches `input` so Topcoat's signals stay in step with what
// the script decided — which also re-triggers the run listener. For an
// idempotent script that merely converges; for a script that DERIVES a field
// from itself it is a feedback loop that compounds every pass (a x3 rule took
// `10.10` to `3.7e20` in about a dozen cycles before the decimal catch stopped
// it). The engine must never be re-triggered by its own writes; user edits
// still are.
let applying = false;

// Writes the engine's doc back. Only changed fields are touched so the caret
// does not jump while someone is typing in a field the script did not alter.
function applyDoc(form, entries) {
  for (const e of entries) {
    const input = form.elements[e.key];
    if (!input) continue;
    const next = e.val.val;
    if (input.type === "checkbox") {
      if (input.checked !== Boolean(next)) input.checked = Boolean(next);
    } else if (String(input.value) !== String(next)) {
      input.value = String(next);
      // keep Topcoat's signals in step with what the engine decided
      input.dispatchEvent(new Event("input", { bubbles: true }));
    }
  }
}

export function boot(script, kinds) {
  const form = document.getElementById("doc-form");
  if (!form) return;

  let worker = null;
  let pending = null; // { id, timer }
  let seq = 0;
  let strikes = 0;
  let disabled = false;
  let queued = false;
  let lastKill = null;

  const kill = (why) => {
    if (worker) {
      worker.terminate(); // forcible; needs no cooperation from the engine
      worker = null;
    }
    if (pending) {
      clearTimeout(pending.timer);
      pending = null;
    }
    strikes += 1;
    lastKill = why;
    if (strikes >= MAX_STRIKES) {
      disabled = true;
      sayKilled(
        "This form's script was stopped " + strikes + " times and has been " +
        "disabled for this page. The form still works; the script does not. " +
        "Reload to try again."
      );
      return;
    }
    sayKilled(why);
    spawn();
  };

  const spawn = () => {
    if (disabled) return;
    worker = new Worker(WORKER_URL, { type: "module" });
    // A worker that dies on its own (an allocation bomb the browser reaps
    // before our timer fires) must land in the same place as one we killed.
    worker.onerror = () => kill("The form script failed and was stopped.");
    worker.onmessage = (e) => {
      const m = e.data;
      if (m.t === "up") {
        // The worker's module graph has evaluated and it is listening. Only
        // now is postMessage actually delivered rather than dropped.
        worker.postMessage({ t: "init", script: script });
        return;
      }
      if (m.t === "ready") {
        if (pending) {
          clearTimeout(pending.timer);
          pending = null;
        }
        run();
        return;
      }
      if (!pending || m.id !== pending.id) return; // stale reply from a killed run
      clearTimeout(pending.timer);
      pending = null;
      window.__frustLastMs = m.ms;
      if (m.t === "ok") {
        applying = true;
        try {
          applyDoc(form, m.doc);
        } finally {
          applying = false;
        }
        say("", false);
      } else {
        say(m.msg, true);
      }
      if (queued) {
        queued = false;
        run();
      }
    };

    // One budget covers the whole startup: module evaluation (which includes
    // compiling 4 MB of WebAssembly), the handshake, and engine construction.
    pending = {
      id: -1,
      timer: setTimeout(
        () => kill("The form script did not start in time and was stopped."),
        INIT_BUDGET_MS
      ),
    };
  };

  const run = () => {
    if (disabled || !worker || applying) return;
    if (pending) {
      // one in flight: coalesce, do not pile up
      queued = true;
      return;
    }
    const id = ++seq;
    pending = {
      id,
      timer: setTimeout(
        () => kill("The form script took too long and was stopped."),
        CALL_BUDGET_MS
      ),
    };
    worker.postMessage({ t: "run", id, doc: readDoc(form, kinds) });
  };

  form.addEventListener("input", run);
  form.addEventListener("change", run);
  spawn();

  // Observable surface for the containment proof. `hostile` reaches the
  // engine's ADR-005 spike exports; it is a test seam, not a capability —
  // a user script can spin with `while (true) {}` and gets killed the same way.
  window.__frustEngine = {
    hostile(kind) {
      if (!worker) return "no worker";
      pending = {
        id: -2,
        timer: setTimeout(
          () => kill("The form script took too long and was stopped."),
          CALL_BUDGET_MS
        ),
      };
      worker.postMessage({ t: kind });
      return "sent";
    },
    get strikes() {
      return strikes;
    },
    get lastKill() {
      return lastKill;
    },
    get disabled() {
      return disabled;
    },
    get alive() {
      return worker !== null;
    },
  };
}
