// The document an extension's GCS bundle runs in: a sandboxed
// (`allow-scripts`, null-origin) iframe whose head starts with a policy that
// denies every network path (`connect-src 'none'`) and a guard script that
// removes the WebRTC interfaces CSP cannot govern and refuses nested frames.
// The bundle is single-file ESM, so it is wrapped in `<script type="module">`;
// all I/O goes through the postMessage bridge, where the host enforces the
// extension's granted capabilities. This is the same frame contract Mission
// Control uses, so one bundle runs unchanged on both hosts.

export const PLUGIN_FRAME_CSP = [
  "default-src 'none'",
  "script-src 'unsafe-inline'",
  "style-src 'unsafe-inline'",
  "img-src data: blob:",
  "font-src data:",
  "connect-src 'none'",
  "webrtc 'block'",
  "form-action 'none'",
  "base-uri 'none'",
].join("; ");

/** Runs before the bundle: deletes every `RTC*` interface from the frame's
 *  global and removes any nested browsing context (tearing the document down
 *  if one survives), so a plugin never reaches a child global with WebRTC. */
export const PLUGIN_FRAME_GUARD_SCRIPT = `(() => {
"use strict";
const w = window;
for (const n of Object.getOwnPropertyNames(w)) {
  if (/^(webkit)?RTC/.test(n)) { try { delete w[n]; } catch (e) {} }
}
const FRAMES = "iframe,frame,object,embed,fencedframe,portal";
const roots = [document];
const observer = new MutationObserver(() => purge());
const purge = () => {
  if (!(w.length > 0)) return;
  for (const r of roots) for (const f of r.querySelectorAll(FRAMES)) f.remove();
  if (w.length > 0) {
    document.documentElement.remove();
    throw new Error("nested frames are not permitted in a plugin frame");
  }
};
const attach = Element.prototype.attachShadow;
Element.prototype.attachShadow = function (init) {
  const root = attach.call(this, init);
  roots.push(root);
  observer.observe(root, { childList: true, subtree: true });
  return root;
};
const wrap = (proto, name) => {
  const d = Object.getOwnPropertyDescriptor(proto, name);
  if (!d) return;
  if (typeof d.value === "function") {
    const f = d.value;
    d.value = function (...a) { const r = f.apply(this, a); purge(); return r; };
  } else if (d.set) {
    const s = d.set;
    d.set = function (v) { s.call(this, v); purge(); };
  } else return;
  Object.defineProperty(proto, name, d);
};
const SINKS = [
  [Node.prototype, ["appendChild", "insertBefore", "replaceChild"]],
  [Element.prototype, ["append", "prepend", "before", "after", "replaceWith", "replaceChildren", "moveBefore", "insertAdjacentElement", "insertAdjacentHTML", "setHTMLUnsafe", "innerHTML", "outerHTML"]],
  [CharacterData.prototype, ["before", "after", "replaceWith"]],
  [DocumentFragment.prototype, ["append", "prepend", "replaceChildren", "moveBefore"]],
  [ShadowRoot.prototype, ["innerHTML", "setHTMLUnsafe"]],
  [Document.prototype, ["append", "prepend", "replaceChildren", "moveBefore", "write", "writeln", "execCommand", "body"]],
  [Range.prototype, ["insertNode", "surroundContents"]],
];
for (const [proto, names] of SINKS) for (const n of names) wrap(proto, n);
observer.observe(document, { childList: true, subtree: true });
})();`;

export const PLUGIN_FRAME_HEAD = `<meta http-equiv="Content-Security-Policy" content="${PLUGIN_FRAME_CSP}"><script>${PLUGIN_FRAME_GUARD_SCRIPT}</script>`;

/** Wrap an ESM bundle in the frame document. A `</script` inside the bundle
 *  (in a string literal) is escaped so it cannot close the module early. */
export function buildPluginFrameHtml(bundleJs: string): string {
  const safe = bundleJs.replace(/<\/(script)/gi, "<\\/$1");
  return [
    "<!doctype html>",
    '<html lang="en">',
    "<head>",
    PLUGIN_FRAME_HEAD,
    '<meta charset="utf-8">',
    '<meta name="color-scheme" content="dark light">',
    "<style>html,body{margin:0;padding:0;height:100%;background:transparent;overflow:hidden}</style>",
    "</head>",
    "<body>",
    `<script type="module">\n${safe}\n</script>`,
    "</body>",
    "</html>",
  ].join("");
}
