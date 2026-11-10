// Shared harness for the cockpit browser checks (`*.e2e.cjs` next to this
// file): the built bundle served by `vite preview`, the agent API mocked per
// test, the Gamepad API stubbed by an init script (a standard-mapping
// DualShock 4) and the pilot stream mocked. Each case-group file hands its
// cases to `runCases`, which runs one case by name or all of them:
//
//   npx vite build && npx vite preview --port 4273 &
//   NODE_PATH=<dir containing playwright-core> CHROME=<chromium path> \
//     SHOTS=<screenshot dir> node e2e/<group>.e2e.cjs [case]

const { chromium } = require("playwright-core");
const fs = require("node:fs");
const path = require("node:path");

const BASE = process.env.BASE ?? "http://127.0.0.1:4273/cockpit/";
const SHOTS = process.env.SHOTS ?? path.join(process.cwd(), "e2e-shots");
fs.mkdirSync(SHOTS, { recursive: true });

/** A standard-mapping pad: `window.__press(i, on)` presses button `i`,
 *  `window.__pad.axes` moves the sticks. */
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

/** Standard-mapping button indices. */
const PAD = { cross: 0, circle: 1, square: 2, triangle: 3, l1: 4, r1: 5, l2: 6, r2: 7, share: 8, options: 9, ps: 16 };

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

const VIEWPORTS = [
  { name: "compact", width: 580, height: 360 },
  { name: "medium", width: 760, height: 480 },
  { name: "expanded", width: 1280, height: 800 },
  { name: "tv", width: 1920, height: 1080 },
  { name: "portrait", width: 412, height: 900 },
];

/** Where a press at the centre of `selector` lands: true when the topmost
 *  element there is (inside) `selector`. Real hit-testing in the page. */
function hits(page, selector) {
  return page.$eval(selector, (el) => {
    const r = el.getBoundingClientRect();
    const top = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
    return top !== null && el.contains(top);
  });
}

/** A pilot on the ground, ready to launch, and one flying under ADOS Pilot
 *  with Fly here offered. */
const PILOT_GROUND = {
  owner: "idle", control_mode: "ados", profile: "normal", avoid: "brake", armed: false, landed: true, vehicle_class: "multirotor", plan_ned: [],
  readiness: [{ id: "fc_profile", ok: true, severity: "block", msg: "FC failsafes set for ADOS" }],
  surface_ops: ["goto"],
};
const PILOT_AIR = { ...PILOT_GROUND, owner: "pilot", armed: true, landed: false };

function telemetryFor(airborne) {
  return {
    armed: airborne, mode: airborne ? "GUIDED" : "LOITER", autopilot: 3,
    attitude: { roll: 0, pitch: 0, yaw: 0 },
    position: { lat: 12.97, lon: 77.59, alt_rel: airborne ? 12 : 0, heading: 70 },
    velocity: { groundspeed: 0, climb: 0 }, gps: { fix_type: 3, satellites: 14 },
    home: { lat_e7: 129700000, lon_e7: 775900000, local_ned: [0, 0, 0] },
    prearm: [], last_update: new Date().toISOString(),
  };
}

/** A page with a mocked pilot stream; `sim.pilot` is what it sends (null:
 *  nothing, so the last frame goes stale), `sim.airborne` drives telemetry.
 *  `sim.sent` collects every non-input frame the cockpit sends, and
 *  `sim.commands` every `POST /api/command` body. */
async function pilotPage(browser, vp, sim, query = "") {
  const ctx = await browser.newContext({ viewport: { width: vp.width, height: vp.height } });
  const page = await ctx.newPage();
  page.on("pageerror", (e) => console.log("pageerror:", String(e).slice(0, 400)));
  await page.addInitScript(PAD_INIT);
  // The first-connect controller tour is a sheet of its own; seen already, so
  // it cannot replace the sheet a case opens.
  await page.addInitScript(() => {
    if (!localStorage.getItem("ados-cockpit-controls")) localStorage.setItem("ados-cockpit-controls", JSON.stringify({ tour_seen: true }));
  });
  await mockAgent(page, { profile: vp.profile ?? "drone" });
  await page.route("**/api/telemetry", (route) => route.fulfill({ contentType: "application/json", body: JSON.stringify(telemetryFor(sim.airborne)) }));
  sim.commands = [];
  await page.route("**/api/command", (route) => {
    sim.commands.push(route.request().postDataJSON());
    return route.fulfill({ contentType: "application/json", body: JSON.stringify({ status: "ok", result_name: "ACCEPTED" }) });
  });
  sim.sent = [];
  await page.routeWebSocket(/\/api\/v1\/pilot\/stream/, (ws) => {
    const push = setInterval(() => {
      if (sim.pilot) ws.send(JSON.stringify({ kind: "pilot", ts_ms: Date.now(), data: sim.pilot }));
    }, 100);
    // A closed page may never report the socket closed; the timer must not
    // keep the run alive.
    push.unref();
    ctx.on("close", () => clearInterval(push));
    ws.onMessage((m) => {
      const f = JSON.parse(String(m));
      if (f.op !== "input") sim.sent.push(f);
      if (f.op !== "action") return;
      // `sim.reply(frame)`: { ok, err, delayMs } or null for no answer.
      const r = sim.reply ? sim.reply(f) : { ok: true };
      if (r) setTimeout(() => ws.send(JSON.stringify({ kind: "action_result", id: f.id, ok: r.ok, err: r.err })), r.delayMs ?? 0);
    });
    ws.onClose(() => clearInterval(push));
  });
  await page.goto(`${BASE}${query}`);
  await page.waitForSelector('[data-testid="top-bar-stop"]', { timeout: 10000 });
  await page.waitForSelector('[data-testid="flight-state-chip"]', { timeout: 10000 });
  // The first pilot frame has reached the chip (it no longer reads NO PILOT).
  if (sim.pilot) await page.waitForFunction(() => !/NO PILOT/.test(document.querySelector('[data-testid="flight-state-chip"]')?.textContent ?? ""), null, { timeout: 10000 });
  const skip = page.getByRole("button", { name: "Skip" });
  if (await skip.count()) await skip.first().click();
  return { ctx, page };
}

const SHEET = '[role="dialog"][aria-modal="true"]';

/** Wait until the open sheets have finished sliding in. */
async function sheetSettled(page) {
  await page.locator(SHEET).first().waitFor({ state: "visible", timeout: 4000 });
  await page.waitForFunction((sel) => [...document.querySelectorAll(sel)].every((e) => e.getAnimations({ subtree: true }).length === 0), SHEET, { timeout: 4000 });
}

/** Hold a control with the mouse for `ms`. */
async function holdOn(page, locator, ms) {
  const b = await locator.boundingBox();
  await page.mouse.move(b.x + b.width / 2, b.y + b.height / 2);
  await page.mouse.down();
  await page.waitForTimeout(ms);
  await page.mouse.up();
}

/** The flight-screen sheets, each opened the way a pilot opens it. */
const GROUND_SHEETS = [
  ["mode", (p) => p.getByTestId("flight-state-chip").click()],
  ["launch", (p) => p.getByTestId("left-takeoff").click()],
  [
    "preflight",
    async (p) => {
      await p.getByTestId("left-takeoff").click();
      await sheetSettled(p);
      await holdOn(p, p.getByRole("button", { name: "Launch (hold)" }), 1800);
      await p.getByTestId("preflight-list").waitFor({ timeout: 3000 });
    },
  ],
  ["menu", (p) => p.click('[data-zone="menu"]')],
  ["skills", (p) => p.getByTestId("skill-picker-chip").click()],
];
const AIR_SHEETS = [
  ["mode-airborne", (p) => p.getByTestId("flight-state-chip").click()],
  ["skill-confirm", (p) => p.getByTestId("left-rth").click()],
  [
    "fly-here",
    async (p) => {
      const map = p.locator('[aria-label="Long-press the map to fly there"]').first();
      await map.waitFor({ state: "attached", timeout: 5000 });
      await holdOn(p, map, 1200);
    },
  ],
];

/** Run `fn`; on a failure save a screenshot of the page first. */
async function debugOnFail(page, name, fn) {
  try {
    await fn();
  } catch (e) {
    await page.screenshot({ path: path.join(SHOTS, `debug-${name}.png`) }).catch(() => undefined);
    throw e;
  }
}

/** Run the case named on the command line, or every case in `cases` (an
 *  object of name → async (browser) => void), in one headless Chromium. */
function runCases(cases) {
  const only = process.argv[2];
  if (only && !Object.hasOwn(cases, only)) {
    console.error(`unknown case "${only}"; this file runs: ${Object.keys(cases).join(", ")}`);
    process.exit(2);
  }
  (async () => {
    const browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME });
    try {
      for (const [name, run] of Object.entries(cases)) if (!only || only === name) await run(browser);
    } finally {
      await browser.close();
    }
  })().catch((e) => {
    console.error(e);
    process.exit(1);
  });
}

module.exports = {
  AIR_SHEETS,
  BASE,
  GROUND_SHEETS,
  PAD,
  PAD_INIT,
  PILOT_AIR,
  PILOT_GROUND,
  SHEET,
  SHOTS,
  VIEWPORTS,
  attr,
  debugOnFail,
  hits,
  holdOn,
  mockAgent,
  pilotPage,
  runCases,
  sheetSettled,
};
