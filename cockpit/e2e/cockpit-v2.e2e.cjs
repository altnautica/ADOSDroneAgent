// End-to-end checks of the cockpit v2 shell against a built bundle served by
// `vite preview`, with the agent API mocked per test and the Gamepad API
// stubbed by an init script (a standard-mapping DualShock 4). Run with a
// Playwright install on NODE_PATH and a Chromium binary:
//
//   npx vite build && npx vite preview --port 4273 &
//   NODE_PATH=<dir containing playwright-core> CHROME=<chromium path> \
//     SHOTS=<screenshot dir> node e2e/cockpit-v2.e2e.cjs
//
// Cases: STOP reachability (the first press on STOP lands on STOP with the
// status-pill list open and closed, the list drawn on top, at every width
// class and the kiosk), the gimbal pad (the recentre button takes a mouse
// click; a flick off the pad leaves no rate running), controller config (hotspot highlight, rebind persists, conflict
// blocks Save, a safety binding cannot be removed), the pilot stick stream
// (50 Hz input frames, release on a hidden tab), and screenshots of every
// width class, the kiosk/TV, the Mode sheet, the skill picker and the
// pre-flight checklist.

const { chromium } = require("playwright-core");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const BASE = process.env.BASE ?? "http://127.0.0.1:4273/cockpit/";
const SHOTS = process.env.SHOTS ?? path.join(process.cwd(), "e2e-shots");
fs.mkdirSync(SHOTS, { recursive: true });

const PAD_INIT = () => {
  const buttons = Array.from({ length: 18 }, () => ({ pressed: false, touched: false, value: 0 }));
  const pad = {
    id: "Wireless Controller (STANDARD GAMEPAD Vendor: 054c Product: 09cc)",
    index: 0,
    connected: true,
    mapping: "standard",
    axes: [0, 0, 0, 0],
    buttons,
    timestamp: performance.now(),
    vibrationActuator: null,
  };
  window.__pad = pad;
  window.__press = (i, on) => {
    buttons[i].pressed = on;
    buttons[i].value = on ? 1 : 0;
    pad.timestamp = performance.now();
  };
  // Like Chromium, `timestamp` advances only when a value changes: a pad held
  // still must keep streaming.
  navigator.getGamepads = () => [pad, null, null, null];
  window.addEventListener("load", () => window.dispatchEvent(Object.assign(new Event("gamepadconnected"), { gamepad: pad })));
};

function skill(id, over = {}) {
  return {
    id,
    label: id.replace(/-/g, " "),
    icon: "camera",
    category: "camera",
    toggle: false,
    confirm: false,
    arm_requirement: "any",
    activation: { via: "config", config_key: id.replace(/-/g, "_") },
    state: { via: "event", topic: "siyi.pod.state" },
    ...over,
  };
}

const PLUGINS = {
  "com.altnautica.siyi-pod": {
    manifest: {
      name: "SIYI pod",
      gcs: {
        contributes: {
          skills: ["siyi-record", "cycle-palette", "fire-lrf", "center", "nadir", "zoom-in", "zoom-out", "photo"].map((id) =>
            skill(id, { toggle: id === "siyi-record" }),
          ),
        },
      },
    },
  },
  "com.altnautica.follow-me": {
    manifest: {
      name: "Follow-Me",
      gcs: {
        contributes: {
          skills: [
            skill("follow-me", {
              label: "Follow-Me",
              icon: "crosshair",
              category: "behavior",
              toggle: true,
              confirm: true,
              arm_requirement: "armed",
              motion: true,
              nudge: true,
              vehicle_classes: ["multirotor", "vtol"],
              activation: { via: "config", config_key: "active" },
              state: { via: "event", topic: "follow.state" },
              params: ["follow_mode", "follow_distance_m", "orbit_radius_m"],
            }),
          ],
          parameters: [
            { key: "follow_mode", binding: "plugin.config", schema: { type: "string", enum: ["follow", "orbit"], default: "follow" }, ui: { label: "Mode" } },
            { key: "follow_distance_m", binding: "plugin.config", schema: { type: "number", minimum: 3, maximum: 30, default: 8 }, ui: { label: "Distance", visible_if: { key: "follow_mode", equals: "follow" } } },
            { key: "orbit_radius_m", binding: "plugin.config", schema: { type: "number", minimum: 3, maximum: 30, default: 8 }, ui: { label: "Orbit radius", visible_if: { key: "follow_mode", equals: "orbit" } } },
          ],
          target_actions: [{ id: "follow", label: "Follow", designate: true, config_key: "active", config_value: true }],
        },
      },
    },
  },
};

async function mockAgent(page, { profile = "drone" } = {}) {
  await page.route("**/api/**", async (route) => {
    const url = new URL(route.request().url());
    const p = url.pathname;
    const json = (body, status = 200) => route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
    if (p === "/api/pairing/info") return json({ profile, name: "bench-drone", paired: true, device_id: "drone-1" });
    if (p === "/api/config") return json({ agent: { name: "bench-drone" } });
    if (p === "/api/plugins") return json({ installs: Object.keys(PLUGINS).map((id) => ({ plugin_id: id, status: "running" })) });
    const m = /^\/api\/plugins\/([^/]+)$/.exec(p);
    if (m && PLUGINS[decodeURIComponent(m[1])]) return json({ granted_capabilities: [], ...PLUGINS[decodeURIComponent(m[1])] });
    if (/^\/api\/plugins\/[^/]+\/config$/.test(p)) return json({ values: {} });
    if (/^\/api\/plugins\/[^/]+\/state$/.test(p)) return json({});
    if (p === "/api/telemetry")
      return json({ armed: false, mode: "LOITER", autopilot: 3, attitude: { roll: 0.05, pitch: -0.03, yaw: 1.2 }, position: { alt_rel: 0, heading: 70 }, velocity: { groundspeed: 0, climb: 0 }, last_update: new Date().toISOString() });
    if (p === "/api/v1/gimbal/rate" && route.request().method() === "GET") return json({ available: true, reason: null });
    return json({ detail: "not mocked" }, 404);
  });
}

/** An attribute read in the page (the drawing re-renders at display rate). */
function attr(page, selector, name) {
  return page.$eval(selector, (e, n) => e.getAttribute(n), name);
}

async function openControls(page) {
  await page.getByRole("button", { name: "Settings" }).first().click();
  await page.getByText("Controls", { exact: true }).first().click();
  await page.waitForSelector('[data-testid="controls-settings"]', { timeout: 8000 }).catch(async (e) => {
    await page.screenshot({ path: path.join(SHOTS, "debug-controls.png") });
    throw e;
  });
}

async function controlsCases(browser) {
  const ctx = await browser.newContext({ viewport: { width: 1600, height: 1000 } });
  const page = await ctx.newPage();
  page.on("pageerror", (e) => console.log("pageerror:", String(e).slice(0, 400)));
  await page.addInitScript(PAD_INIT);
  await mockAgent(page);
  await page.goto(BASE);
  await page.evaluate(() => localStorage.removeItem("ados-cockpit-controls"));
  await page.reload();
  await openControls(page);
  // Skip the first-connect tour if it shows.
  const skip = page.getByRole("button", { name: "Skip" });
  if (await skip.isVisible().catch(() => false)) await skip.click();

  // (a) button 0 highlights the cross hotspot within a frame.
  const cross = '[data-control="cross"]';
  await page.waitForFunction((sel) => document.querySelector(sel) !== null, cross, { timeout: 10000 });
  assert.equal(await attr(page, cross, "fill"), "transparent");
  // "Press a control" mode: a press highlights and selects, never navigates.
  await page.getByRole("button", { name: "Press a control" }).click();
  await page.evaluate(() => window.__press(0, true));
  await page.waitForTimeout(50);
  assert.notEqual(await attr(page, cross, "fill"), "transparent", "cross hotspot fills when button 0 is pressed");
  await page.screenshot({ path: path.join(SHOTS, "controls-pressed-cross.png") });
  await page.evaluate(() => window.__press(0, false));
  assert.equal(await attr(page, '[data-testid="control-editor"]', "data-control"), "cross", "the pressed control is selected");
  console.log("PASS (a) button 0 highlights the cross hotspot (and selects it)");

  // (b) rebind Square tap to Record (moving Record off Share first, since one
  // action on two controls is a conflict); it persists across a reload.
  await page.$eval('[data-control="share"]', (e) => e.dispatchEvent(new MouseEvent("click", { bubbles: true })));
  await page.getByLabel("Share tap").selectOption("none");
  await page.$eval('[data-control="square"]', (e) => e.dispatchEvent(new MouseEvent("click", { bubbles: true })));
  await page.getByLabel("Square tap").selectOption("record_toggle");
  await page.getByTestId("controls-save").click();
  await page.reload();
  await openControls(page);
  const stored = await page.evaluate(() => localStorage.getItem("ados-cockpit-controls") ?? "");
  assert.match(stored, /"control":"square"[^}]*"action":"record_toggle"/, "square → record persisted");
  await page.$eval('[data-control="square"]', (e) => e.dispatchEvent(new MouseEvent("click", { bubbles: true })));
  assert.equal(await page.getByLabel("Square tap").inputValue(), "record_toggle");
  console.log("PASS (b) Square → Record persists across reload");

  // (c) a second control on the same action outlines both and blocks Save.
  await page.$eval('[data-control="triangle"]', (e) => e.dispatchEvent(new MouseEvent("click", { bubbles: true })));
  await page.getByLabel("Triangle tap").selectOption("record_toggle");
  assert.equal(await attr(page, '[data-control="square"]', "data-conflict"), "true");
  assert.equal(await attr(page, '[data-control="triangle"]', "data-conflict"), "true");
  assert.equal(await page.getByTestId("controls-save").isDisabled(), true);
  await page.screenshot({ path: path.join(SHOTS, "controls-conflict.png") });
  await page.getByRole("button", { name: "Discard" }).click();
  console.log("PASS (c) conflict outlines both and disables Save");

  // (d) removing the locked pause binding is refused.
  await page.$eval('[data-control="ps"]', (e) => e.dispatchEvent(new MouseEvent("click", { bubbles: true })));
  // PS tap carries pause (and a second row active only during RTH).
  const psRows = page.locator('select[aria-label="PS tap"]');
  const values = await psRows.evaluateAll((els) => els.map((e) => e.value));
  const pauseRow = psRows.nth(values.indexOf("pause"));
  await pauseRow.selectOption("none");
  await page.getByTestId("editor-error").waitFor();
  assert.match(await page.getByTestId("editor-error").innerText(), /locked/i);
  assert.ok((await psRows.evaluateAll((els) => els.map((e) => e.value))).includes("pause"), "pause stays bound");
  await page.screenshot({ path: path.join(SHOTS, "controls-locked.png") });
  console.log("PASS (d) removing pause is refused");
  await ctx.close();
}

async function pilotStreamCase(browser) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await ctx.newPage();
  await page.addInitScript(PAD_INIT);
  await mockAgent(page);
  const frames = [];
  await page.routeWebSocket(/\/api\/v1\/pilot\/stream/, (ws) => {
    const state = { owner: "pilot", control_mode: "ados", profile: "normal", avoid: "brake", armed: true, landed: false, vehicle_class: "multirotor", readiness: [], plan_ned: [] };
    const push = setInterval(() => ws.send(JSON.stringify({ kind: "pilot", ts_ms: Date.now(), data: state })), 100);
    ws.onMessage((m) => frames.push({ t: Date.now(), f: JSON.parse(String(m)) }));
    ws.onClose(() => clearInterval(push));
  });
  await page.goto(BASE);
  await page.waitForTimeout(1500);
  await page.evaluate(() => {
    window.__pad.axes = [0, 0, 0, -0.8];
  });
  const t0 = Date.now();
  await page.waitForTimeout(2000);
  const inputs = frames.filter((x) => x.f.op === "input" && x.t >= t0 + 200 && x.t <= t0 + 1800);
  const hz = inputs.length / 1.6;
  const last = inputs[inputs.length - 1]?.f;
  console.log(`pilot stream: ${inputs.length} input frames in 1.6 s = ${hz.toFixed(1)} Hz; last axes ${JSON.stringify(last?.axes)}`);
  assert.ok(hz >= 45 && hz <= 55, "input frames at ~50 Hz");
  assert.ok(last.axes[1] > 0.5, "right stick up maps to pitch forward (+)");
  await page.evaluate(() => {
    Object.defineProperty(document, "visibilityState", { configurable: true, get: () => "hidden" });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await page.waitForTimeout(400);
  assert.ok(frames.some((x) => x.f.op === "release"), "hidden tab sends release");
  console.log("PASS (e) 50 Hz pilot input with mapped axes; release on hidden tab");
  await ctx.close();
}

const VIEWPORTS = [
  { name: "compact", width: 580, height: 360 },
  { name: "medium", width: 760, height: 480 },
  { name: "expanded", width: 1280, height: 800 },
  { name: "tv", width: 1920, height: 1080 },
  { name: "portrait", width: 412, height: 900 },
];

async function shots(browser) {
  for (const vp of VIEWPORTS) {
    const ctx = await browser.newContext({ viewport: { width: vp.width, height: vp.height } });
    const page = await ctx.newPage();
    await page.addInitScript(PAD_INIT);
    await mockAgent(page);
    await page.goto(`${BASE}?demo=1`);
    await page.waitForTimeout(2500);
    await page.screenshot({ path: path.join(SHOTS, `feed-${vp.name}.png`) });
    await ctx.close();
  }
  const ctx = await browser.newContext({ viewport: { width: 1920, height: 1080 } });
  const page = await ctx.newPage();
  await page.addInitScript(PAD_INIT);
  await mockAgent(page, { profile: "ground_station" });
  await page.goto(`${BASE}?kiosk=1&demo=1`);
  await page.waitForTimeout(2500);
  assert.equal(await page.locator('[data-width-class="tv"][data-focus-rings="always"]').count(), 1, "kiosk is tv with focus rings");
  await page.screenshot({ path: path.join(SHOTS, "feed-kiosk-tv.png") });
  await ctx.close();

  const c2 = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const p2 = await c2.newPage();
  await p2.addInitScript(PAD_INIT);
  await mockAgent(p2);
  await p2.goto(`${BASE}?demo=1`);
  await p2.waitForTimeout(2000);
  await p2.getByTestId("flight-state-chip").click();
  await p2.waitForTimeout(300);
  await p2.screenshot({ path: path.join(SHOTS, "mode-sheet.png") });
  await p2.keyboard.press("Escape");
  await p2.getByTestId("skill-picker-chip").click();
  await p2.getByRole("tab", { name: "Camera" }).click();
  await p2.waitForTimeout(300);
  assert.equal(await p2.getByText("SIYI pod").count() >= 1, true);
  await p2.screenshot({ path: path.join(SHOTS, "skill-picker-siyi.png") });
  await p2.getByRole("tab", { name: "Behaviors" }).click();
  await p2.getByRole("button", { name: /^Follow-Me/ }).click();
  await p2.waitForTimeout(300);
  await p2.screenshot({ path: path.join(SHOTS, "skill-picker-follow-tweak.png") });
  await c2.close();

  // On the ground (pilot reports landed and disarmed): Launch sheet and the
  // pre-flight checklist it gates.
  const c3 = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const p3 = await c3.newPage();
  await p3.addInitScript(PAD_INIT);
  await mockAgent(p3);
  await p3.routeWebSocket(/\/api\/v1\/pilot\/stream/, (ws) => {
    const state = {
      owner: "idle", control_mode: "ados", profile: "normal", avoid: "brake", armed: false, landed: true, vehicle_class: "multirotor", plan_ned: [],
      readiness: [{ id: "fc_profile", ok: true, severity: "block", msg: "FC failsafes set for ADOS" }, { id: "no_agl", ok: false, severity: "warn", msg: "No height source" }],
      mission: { state: "loaded", seq: 0, total: 4, landing_seq: false },
    };
    const push = setInterval(() => ws.send(JSON.stringify({ kind: "pilot", ts_ms: Date.now(), data: state })), 100);
    ws.onMessage(() => undefined);
    ws.onClose(() => clearInterval(push));
  });
  await p3.goto(BASE);
  await p3.waitForTimeout(2000);
  await p3.screenshot({ path: path.join(SHOTS, "feed-ground-ready.png") });
  await p3.getByTestId("left-takeoff").click({ force: true });
  await p3.waitForTimeout(400);
  await p3.screenshot({ path: path.join(SHOTS, "launch-sheet.png") });
  await c3.close();
  console.log(`screenshots in ${SHOTS}`);
}

/** Where a press at the centre of `selector` lands: true when the topmost
 *  element there is (inside) `selector`. Real hit-testing in the page. */
function hits(page, selector) {
  return page.$eval(selector, (el) => {
    const r = el.getBoundingClientRect();
    const top = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
    return top !== null && el.contains(top);
  });
}

/** STOP takes the first press with the status-pill list open and closed, at
 *  every width class, and the list is drawn (in the window, on top). */
async function stopCases(browser) {
  for (const vp of [...VIEWPORTS, { name: "kiosk-tv", width: 1920, height: 1080, query: "?kiosk=1&demo=1" }]) {
    const ctx = await browser.newContext({ viewport: { width: vp.width, height: vp.height } });
    const page = await ctx.newPage();
    await page.addInitScript(PAD_INIT);
    await mockAgent(page);
    await page.goto(`${BASE}${vp.query ?? "?demo=1"}`);
    await page.waitForSelector('[data-testid="top-bar-stop"]', { timeout: 8000 });
    await page.waitForTimeout(1500);
    const skip = page.getByRole("button", { name: "Skip" });
    if (await skip.count()) await skip.first().click();
    const stop = '[data-testid="top-bar-stop"]';
    const pill = '[data-zone="status_pill"]';
    const list = '[data-testid="status-pill-list"]';
    assert.equal(await hits(page, stop), true, `${vp.name}: STOP reachable with the list closed`);
    await page.click(pill);
    await page.waitForSelector(list, { state: "visible", timeout: 2000 });
    const box = await page.locator(list).boundingBox();
    assert.ok(box && box.width > 0 && box.height > 0, `${vp.name}: the list has a size`);
    assert.ok(box.y >= 0 && box.y + Math.min(box.height, 24) <= vp.height, `${vp.name}: the list is inside the window`);
    assert.equal(await hits(page, list), true, `${vp.name}: the list is drawn on top`);
    assert.equal(await hits(page, stop), true, `${vp.name}: STOP reachable with the list open`);
    await page.screenshot({ path: path.join(SHOTS, `pill-open-${vp.name}.png`) });
    // One real press on STOP with the list open reaches STOP, and closes the list.
    await page.$eval(stop, (el) => {
      window.__stopDowns = 0;
      el.addEventListener("pointerdown", () => (window.__stopDowns += 1));
    });
    const s = await page.locator(stop).boundingBox();
    await page.mouse.move(s.x + s.width / 2, s.y + s.height / 2);
    await page.mouse.down();
    await page.mouse.up();
    assert.equal(await page.evaluate(() => window.__stopDowns), 1, `${vp.name}: STOP took the first press`);
    await page.waitForSelector(list, { state: "detached", timeout: 2000 });
    await ctx.close();
  }
  console.log("STOP reachable with the status list open and closed at every width class");
}

/** A mouse click on the gimbal pad's recentre button reaches the button (the
 *  pad does not capture the pointer until it is dragged). */
async function recentreCase(browser) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await ctx.newPage();
  await mockAgent(page);
  await page.goto(`${BASE}?demo=1`);
  const knob = page.getByRole("button", { name: "Recentre gimbal" });
  await knob.waitFor({ state: "visible", timeout: 8000 });
  await page.$eval('[data-zone="gimbal_pad"] button', (el) => {
    window.__knobClicks = 0;
    el.disabled = false;
    el.addEventListener("click", () => (window.__knobClicks += 1));
  });
  const b = await knob.boundingBox();
  await page.mouse.click(b.x + b.width / 2, b.y + b.height / 2);
  assert.equal(await page.evaluate(() => window.__knobClicks), 1, "the recentre button took the mouse click");
  await ctx.close();
  console.log("recentre button takes a mouse click");
}

/** A fast mouse flick off the gimbal pad, released elsewhere, leaves no drag
 *  behind: a later hover with no button held sends no rate, and nothing is
 *  sent once the pointer has left. Run from the knob (centre) and from the
 *  pad ring. */
async function gimbalFlickCase(browser) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await ctx.newPage();
  await mockAgent(page);
  const posts = [];
  page.on("request", (r) => {
    if (r.method() === "POST" && new URL(r.url()).pathname === "/api/v1/gimbal/rate") posts.push({ t: Date.now(), body: r.postDataJSON() });
  });
  await page.goto(`${BASE}?demo=1`);
  const pad = page.locator('[data-zone="gimbal_pad"]');
  await pad.waitFor({ state: "visible", timeout: 8000 });
  const b = await pad.boundingBox();
  const cx = b.x + b.width / 2;
  const cy = b.y + b.height / 2;
  const r = b.width / 2;
  for (const start of [0, 0.1]) {
    await page.mouse.move(cx + start * r, cy);
    await page.mouse.down();
    await page.mouse.move(cx + 2 * b.width, cy, { steps: 1 });
    await page.mouse.up();
    await page.waitForTimeout(400);
    await page.mouse.move(cx + 0.85 * r, cy, { steps: 1 });
    await page.waitForTimeout(400);
    await page.mouse.move(10, 10, { steps: 1 });
    const left = Date.now();
    await page.waitForTimeout(1500);
    const moving = posts.filter((p) => p.t >= left && (p.body.pitch_dps !== 0 || p.body.yaw_dps !== 0));
    assert.equal(moving.length, 0, `no gimbal rate after the pointer left with no button held (press at ${start} r): ${JSON.stringify(moving)}`);
    posts.length = 0;
  }
  await ctx.close();
  console.log("gimbal pad sends no rate after a flick off and a hover with no button held");
}

(async () => {
  const browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME });
  const only = process.argv[2];
  try {
    if (!only || only === "stop") await stopCases(browser);
    if (!only || only === "recentre") await recentreCase(browser);
    if (!only || only === "gimbal-flick") await gimbalFlickCase(browser);
    if (!only || only === "controls") await controlsCases(browser);
    if (!only || only === "stream") await pilotStreamCase(browser);
    if (!only || only === "shots") await shots(browser);
  } finally {
    await browser.close();
  }
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
