// Tiny dirty-field tracker shared by every settings section.
//
// Usage:
//   const tracker = createDirtyTracker({ wifi_ssid: "" }, () => onChange());
//   tracker.set("wifi_ssid", "skynet");
//   tracker.isDirty();   // true
//   tracker.payload();   // { wifi_ssid: "skynet" }  (only changed fields)
//   tracker.read("wifi_ssid"); // current value (initial or updated)
//
// payload() returns ONLY fields whose current value differs from the
// initial snapshot. Equality is shallow: scalars compare by ===, arrays
// and objects compare by JSON shape so callers don't need to deep-clone
// before reading. Booleans, numbers, strings, and null all flow through
// untouched.

function shallowEqual(a, b) {
  if (a === b) return true;
  if (a == null || b == null) return a === b;
  if (typeof a !== typeof b) return false;
  if (typeof a === "object") {
    try {
      return JSON.stringify(a) === JSON.stringify(b);
    } catch {
      return false;
    }
  }
  return false;
}

export function createDirtyTracker(initial, onChange) {
  const initialSnapshot = { ...(initial || {}) };
  const current = { ...initialSnapshot };
  const notify = typeof onChange === "function" ? onChange : () => {};

  return {
    read(key) {
      return current[key];
    },
    set(key, value) {
      if (current[key] === value) return;
      current[key] = value;
      notify();
    },
    reset() {
      for (const k of Object.keys(current)) delete current[k];
      Object.assign(current, initialSnapshot);
      notify();
    },
    isDirty() {
      for (const k of Object.keys(current)) {
        if (!shallowEqual(current[k], initialSnapshot[k])) return true;
      }
      // Detect keys present in the initial snapshot but cleared.
      for (const k of Object.keys(initialSnapshot)) {
        if (!(k in current)) return true;
      }
      return false;
    },
    dirtyCount() {
      let n = 0;
      for (const k of Object.keys(current)) {
        if (!shallowEqual(current[k], initialSnapshot[k])) n += 1;
      }
      return n;
    },
    payload() {
      const out = {};
      for (const k of Object.keys(current)) {
        if (!shallowEqual(current[k], initialSnapshot[k])) {
          out[k] = current[k];
        }
      }
      return out;
    },
  };
}
