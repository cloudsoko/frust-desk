export function log(msg) { globalThis.__frustHookLog = (globalThis.__frustHookLog || []); globalThis.__frustHookLog.push(String(msg)); }
