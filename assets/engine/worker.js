// WO-017 item 2 — the engine's containment cell.
//
// Boa's epoch/fuel budget is a wasmtime host feature; it does not cross into
// the browser. The browser's equivalent is not a budget at all — it is
// `Worker.terminate()`, which is forcible and needs no cooperation from the
// code being killed. That is why the engine runs here and not on the main
// thread: THERE IS NO WAY TO INTERRUPT A SYNCHRONOUS LOOP ON THE MAIN THREAD.
// A same-thread engine is not "less contained", it is uncontainable.
//
// This file is deliberately dumb. It holds no policy, no budget, no retry
// count — all of that lives in the supervisor, which is the side that survives
// a kill. A watchdog that lives inside the thing it watches is not a watchdog.

import { _setEnv } from "./cli.js";
import { hooks } from "./script_engine.js";

self.onmessage = (e) => {
  handle(e);
};

function handle(e) {
  const m = e.data;
  switch (m.t) {
    case "init":
      // Same seam the kernel host fills (FRUST_SCRIPT), same as the
      // main-thread host used in item 1. One dialect, now three call sites.
      _setEnv({ FRUST_SCRIPT: m.script });
      self.postMessage({ t: "ready" });
      break;

    case "run": {
      const started = performance.now();
      try {
        const doc = hooks.validate(m.doc);
        self.postMessage({ t: "ok", id: m.id, doc, ms: performance.now() - started });
      } catch (err) {
        // The engine already stripped its own trace (ADR-007 hygiene), so
        // this string is user-facing as-is.
        self.postMessage({
          t: "err",
          id: m.id,
          msg: String(err && err.payload ? err.payload : err),
          ms: performance.now() - started,
        });
      }
      break;
    }

    // Hostile exports, ADR-005 spike parity. Reachable only from the test
    // hook on the supervisor — never from a user script, which can do the
    // same thing with `while (true) {}` anyway. Neither replies.
    case "spin":
      hooks.spin();
      break;
    case "hog":
      hooks.hog();
      break;
  }
}

// MUST be last. A message posted to a module worker BEFORE its module graph
// finishes evaluating is silently DROPPED, not queued (measured in Chrome:
// an immediate `init` never arrives; the same `init` after a delay does).
// So the worker announces itself and the supervisor waits for this, rather
// than sleeping — a sleep would turn a deterministic failure into a flaky one
// on slower machines, which is the same bug wearing a disguise.
self.postMessage({ t: "up" });
