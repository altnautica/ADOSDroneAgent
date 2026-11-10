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
// class and the kiosk; `stop-sheets`: the same with every flight-screen sheet
// open, each sheet starting below the top bar), the flow overlays clearing on
// a stalled pilot (`flow-stale`), a running launch's step and Abort
// (`sequence`), the Mode sheet's pending pick (`mode-pending`), Escape / a
// press outside / the controller's back closing the topmost sheet
// (`overlay-close`), the gimbal pad (the recentre button takes a mouse
// click; a flick off the pad leaves no rate running), the kiosk alert column
// (banners, alert cards and layer chips never overlap), controller config
// (hotspot highlight, rebind persists, conflict blocks Save, a safety binding
// cannot be removed), the pilot stick stream (50 Hz input frames, release on
// a hidden tab), and screenshots of every width class, the kiosk/TV, the Mode
// sheet, the skill picker and the pre-flight checklist (`shots`, manual).

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
  // From the flight screen Settings is a tab of the Menu ("Go to"); from any
  // other screen it is in the side navigation.
  await page.waitForSelector('[data-zone="menu"], nav', { timeout: 10000 });
  if (await page.locator('[data-zone="menu"]').count()) {
    await page.locator('[data-zone="menu"]').click();
    await page.locator('[role="dialog"]').getByRole("button", { name: "Settings", exact: true }).click();
  } else {
    await page.getByRole("button", { name: "Settings", exact: true }).first().click();
  }
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
  // Factory controls, with the first-connect tour already seen (a sheet of
  // its own that would otherwise open over the editor).
  await page.evaluate(() => localStorage.setItem("ados-cockpit-controls", JSON.stringify({ tour_seen: true })));
  await page.reload();
  await openControls(page);

  // (a) button 0 highlights the cross hotspot within a frame.
  const cross = '[data-control="cross"]';
  await page.waitForFunction((sel) => document.querySelector(sel) !== null, cross, { timeout: 10000 });
  assert.equal(await attr(page, cross, "fill"), "transparent");
  // "Press a control" mode: a press highlights and selects, never navigates.
  await page.getByRole("button", { name: "Press a control" }).click();
  await page.evaluate(() => window.__press(0, true));
  await page.waitForFunction((sel) => document.querySelector(sel)?.getAttribute("fill") !== "transparent", cross, { timeout: 2000 });
  assert.notEqual(await attr(page, cross, "fill"), "transparent", "cross hotspot fills when button 0 is pressed");
  await page.screenshot({ path: path.join(SHOTS, "controls-pressed-cross.png") });
  await page.evaluate(() => window.__press(0, false));
  await page.waitForFunction(() => document.querySelector('[data-testid="control-editor"]')?.getAttribute("data-control") === "cross", null, { timeout: 2000 });
  assert.equal(await attr(page, '[data-testid="control-editor"]', "data-control"), "cross", "the pressed control is selected");
  console.log("PASS (a) button 0 highlights the cross hotspot (and selects it)");

  // (b) rebind Square tap to Record (moving Record off Share first, since one
  // action on two controls is a conflict); it persists across a reload.
  await page.$eval('[data-control="share"]', (e) => e.dispatchEvent(new MouseEvent("click", { bubbles: true })));
  await page.getByLabel("Share tap").selectOption("none");
  await page.$eval('[data-control="square"]', (e) => e.dispatchEvent(new MouseEvent("click", { bubbles: true })));
  await page.getByLabel("Square tap").selectOption("record_toggle");
  await page.getByTestId("controls-save").click();
  await page.waitForFunction(() => /"action":"record_toggle"/.test(localStorage.getItem("ados-cockpit-controls") ?? ""), null, { timeout: 4000 });
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
  await page.waitForFunction(() => document.querySelector('[data-control="triangle"]')?.getAttribute("data-conflict") === "true", null, { timeout: 4000 });
  assert.equal(await attr(page, '[data-control="square"]', "data-conflict"), "true");
  assert.equal(await attr(page, '[data-control="triangle"]', "data-conflict"), "true");
  assert.equal(await page.getByTestId("controls-save").isDisabled(), true);
  await page.screenshot({ path: path.join(SHOTS, "controls-conflict.png") });
  await page.getByRole("button", { name: "Discard" }).click();
  console.log("PASS (c) conflict outlines both and disables Save");

  // (d) removing the locked pause binding is refused.
  await page.$eval('[data-control="ps"]', (e) => e.dispatchEvent(new MouseEvent("click", { bubbles: true })));
  await page.waitForFunction(() => document.querySelector('[data-testid="control-editor"]')?.getAttribute("data-control") === "ps", null, { timeout: 4000 });
  // PS press carries pause (outside RTH); the locked row refuses removal.
  const psRows = page.locator('select[aria-label="PS press"]');
  const values = await psRows.evaluateAll((els) => els.map((e) => e.value));
  assert.ok(values.includes("pause"), `PS press carries pause (got ${JSON.stringify(values)})`);
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
  // Every mock socket's push timer, cleared before the context closes: a
  // socket the page never closed would keep its timer (and node) alive.
  const pushes = [];
  await page.routeWebSocket(/\/api\/v1\/pilot\/stream/, (ws) => {
    const state = { owner: "pilot", control_mode: "ados", profile: "normal", avoid: "brake", armed: true, landed: false, vehicle_class: "multirotor", readiness: [], plan_ned: [] };
    const push = setInterval(() => ws.send(JSON.stringify({ kind: "pilot", ts_ms: Date.now(), data: state })), 100);
    pushes.push(push);
    ws.onMessage((m) => frames.push({ t: Date.now(), f: JSON.parse(String(m)) }));
    ws.onClose(() => clearInterval(push));
  });
  await page.goto(BASE);
  // The stick stream runs once the pilot link is open and the pad is seen.
  const until = async (what, pred, ms) => {
    const end = Date.now() + ms;
    while (!pred()) {
      if (Date.now() > end) assert.fail(`timed out waiting for ${what}`);
      await page.waitForTimeout(25);
    }
  };
  await until("the first input frame", () => frames.some((x) => x.f.op === "input"), 10000);
  await page.evaluate(() => {
    window.__pad.axes = [0, 0, 0, -0.8];
  });
  await until("the stick deflection in a frame", () => frames.some((x) => x.f.op === "input" && x.f.axes[1] > 0.5), 3000);
  const t0 = Date.now();
  await until("1.6 s of frames", () => frames.some((x) => x.f.op === "input" && x.t >= t0 + 1600), 5000);
  // The rate on the cockpit's own clock (each frame's t_ms), so a busy test
  // host delaying delivery does not skew it.
  const inputs = frames.filter((x) => x.f.op === "input" && x.t >= t0).map((x) => x.f);
  const span = (inputs[inputs.length - 1].t_ms - inputs[0].t_ms) / 1000;
  const hz = (inputs.length - 1) / span;
  const last = inputs[inputs.length - 1];
  console.log(`pilot stream: ${inputs.length} input frames in ${span.toFixed(2)} s = ${hz.toFixed(1)} Hz; last axes ${JSON.stringify(last?.axes)}`);
  assert.ok(hz >= 45 && hz <= 55, "input frames at ~50 Hz");
  assert.ok(last.axes[1] > 0.5, "right stick up maps to pitch forward (+)");
  await page.evaluate(() => {
    Object.defineProperty(document, "visibilityState", { configurable: true, get: () => "hidden" });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await until("the release frame", () => frames.some((x) => x.f.op === "release"), 2000);
  console.log("PASS (e) 50 Hz pilot input with mapped axes; release on hidden tab");
  pushes.forEach(clearInterval);
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
    prearm: [], last_update: new Date().toISOString(),
  };
}

/** A page with a mocked pilot stream; `sim.pilot` is what it sends (null:
 *  nothing, so the last frame goes stale), `sim.airborne` drives telemetry. */
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

/** The kiosk and every width class, with each flight-screen sheet open:
 *  the topmost element at STOP's centre is STOP, the sheet starts below the
 *  top bar, and a press on STOP reaches STOP (and its confirmation replaces
 *  the sheet). */
async function stopSheetsCase(browser) {
  const viewports = [...VIEWPORTS, { name: "kiosk-tv", width: 1920, height: 1080, query: "?kiosk=1", profile: "ground_station" }];
  for (const vp of viewports) {
    const sim = { pilot: PILOT_GROUND, airborne: false };
    const { ctx, page } = await pilotPage(browser, vp, sim, vp.query ?? "");
    const stop = '[data-testid="top-bar-stop"]';
    const check = async (name) => {
      await sheetSettled(page);
      assert.equal(await hits(page, stop), true, `${vp.name} ${name}: STOP is the topmost element at its centre`);
      const bar = await page.locator('[data-zone="top_bar"]').boundingBox();
      const panel = await page.locator(SHEET).first().boundingBox();
      assert.ok(panel.y >= bar.y + bar.height - 1, `${vp.name} ${name}: the sheet starts below the top bar (${panel.y} < ${bar.y + bar.height})`);
    };
    for (const [name, openIt] of GROUND_SHEETS) {
      await debugOnFail(page, `${vp.name}-${name}`, async () => {
        await openIt(page);
        await check(name);
      });
      if (name === "launch") await page.screenshot({ path: path.join(SHOTS, `stop-sheets-${vp.name}-launch.png`) });
      await page.keyboard.press("Escape");
      await page.locator(SHEET).waitFor({ state: "detached", timeout: 2000 });
    }
    sim.pilot = PILOT_AIR;
    sim.airborne = true;
    await page.getByTestId("left-land").waitFor({ state: "attached", timeout: 5000 });
    for (const [name, openIt] of AIR_SHEETS) {
      if (name === "fly-here" && (await page.locator('[aria-label="Long-press the map to fly there"]').count()) === 0) {
        console.log(`${vp.name}: no map on screen at this class, Fly here not offered`);
        continue;
      }
      await debugOnFail(page, `${vp.name}-${name}`, async () => {
        await openIt(page);
        await check(name);
      });
      if (name === "mode-airborne") await page.screenshot({ path: path.join(SHOTS, `stop-sheets-${vp.name}-mode.png`) });
      // One real press on STOP reaches STOP; its confirmation replaces the sheet.
      await page.$eval(stop, (el) => {
        window.__stopDowns = 0;
        el.addEventListener("pointerdown", () => (window.__stopDowns += 1), { once: true });
      });
      const s = await page.locator(stop).boundingBox();
      await page.mouse.click(s.x + s.width / 2, s.y + s.height / 2);
      assert.equal(await page.evaluate(() => window.__stopDowns), 1, `${vp.name} ${name}: STOP took the first press`);
      await page.locator(SHEET, { hasText: "Emergency stop" }).waitFor({ timeout: 2000 });
      const open = await page.$$eval(SHEET, (els) => els.map((e) => e.textContent.slice(0, 40)));
      assert.equal(open.length, 1, `${vp.name} ${name}: one sheet at a time: ${JSON.stringify(open)}`);
      await page.keyboard.press("Escape");
      await page.locator(SHEET).waitFor({ state: "detached", timeout: 2000 });
    }
    await ctx.close();
  }
  console.log("STOP is on top and takes the first press with every flight-screen sheet open, at every width class and the kiosk");
}

/** A stalled pilot (no frame for more than `stale_after_ms`, 2 s) clears
 *  every overlay drawn from its last frame: the low-battery and link-loss
 *  countdowns (with Cancel), the gate ring and "Sticks are with …". */
async function flowStaleCase(browser) {
  const sim = {
    pilot: {
      ...PILOT_AIR,
      rth_countdown_s: 9,
      link_loss_in_s: 5,
      link_loss_action: "rth",
      gate: { pending: true, progress: 0.4 },
      input_session: { src: "bench", label: "Bench", session: 7 },
      banner: { text_key: "pilot.banner.ados_control", args: { profile: "NORMAL" }, until_ms: null },
    },
    airborne: true,
  };
  const { ctx, page } = await pilotPage(browser, { width: 1280, height: 800 }, sim);
  const live = ['[data-testid="low-battery-banner"]', '[data-testid="link-loss-banner"]', '[data-testid="gate-ring"]', '[data-testid="sticks-elsewhere"]'];
  for (const sel of [...live, '[data-testid="handover-banner"]']) await page.locator(sel).waitFor({ state: "visible", timeout: 5000 });
  sim.pilot = null;
  const t0 = Date.now();
  await debugOnFail(page, "flow-stale", async () => {
    for (const sel of live) await page.locator(sel).waitFor({ state: "detached", timeout: 3200 });
  });
  const gone = Date.now() - t0;
  assert.ok(gone >= 1500, `the overlays stay while the frame is fresh (gone after ${gone} ms)`);
  await page.screenshot({ path: path.join(SHOTS, "flow-stale.png") });
  await ctx.close();
  console.log(`stalled pilot: countdowns, gate ring and sticks-with gone ${gone} ms after the last frame`);
}

/** A running launch sequence shows its step and Abort in the top-centre
 *  stack with no sheet open and with another sheet open; Abort sends the
 *  pilot's pause; the line goes once the sequence is done. */
async function sequenceCase(browser) {
  const sim = { pilot: { ...PILOT_AIR, sequence: { step: "takeoff", after: "hold", error: null } }, airborne: true };
  const { ctx, page } = await pilotPage(browser, { width: 1280, height: 800 }, sim);
  const line = page.getByTestId("launch-sequence");
  await line.waitFor({ state: "visible", timeout: 5000 });
  // The same line the chip's launching rule writes.
  assert.match(await line.innerText(), /TAKING OFF · TAKE OFF/);
  await page.getByTestId("flight-state-chip").click();
  await sheetSettled(page);
  assert.ok(await line.isVisible(), "the step and Abort stay on screen with the Mode sheet open");
  await page.screenshot({ path: path.join(SHOTS, "sequence-mode-sheet.png") });
  await page.keyboard.press("Escape");
  await page.locator(SHEET).waitFor({ state: "detached", timeout: 2000 });
  await page.getByTestId("launch-abort").click();
  const deadline = Date.now() + 2000;
  while (!sim.sent.some((f) => f.op === "action" && f.name === "pause") && Date.now() < deadline) await page.waitForTimeout(50);
  assert.ok(sim.sent.some((f) => f.op === "action" && f.name === "pause"), "Abort sends the pilot's pause");
  sim.pilot = { ...PILOT_AIR, sequence: { step: "done", after: "hold", error: null } };
  await line.waitFor({ state: "detached", timeout: 3000 });
  await ctx.close();
  console.log("launch sequence: step and Abort in the top-centre stack, with or without a sheet; Abort sends pause");
}

/** A pick in the Mode sheet turns pending at once (outlined, busy) and holds
 *  the row until the pilot answers, then the result line shows. */
async function modePendingCase(browser) {
  const sim = { pilot: PILOT_AIR, airborne: true, reply: (f) => (f.name.startsWith("profile:") ? { ok: true, delayMs: 1500 } : { ok: true }) };
  const { ctx, page } = await pilotPage(browser, { width: 1280, height: 800 }, sim);
  await debugOnFail(page, "mode-pending-open", async () => {
    await page.getByTestId("flight-state-chip").click();
    await sheetSettled(page);
  });
  const row = page.locator('[data-row="profile"]');
  await row.getByRole("radio", { name: "Cine" }).click();
  await debugOnFail(page, "mode-pending", async () => {
    await row.locator('[data-pending="true"]').waitFor({ state: "visible", timeout: 150 });
  });
  assert.equal(await row.getAttribute("aria-busy"), "true", "the row is busy");
  assert.equal(await row.getByRole("radio", { name: "Sport" }).isDisabled(), true, "the row waits for the answer");
  await page.screenshot({ path: path.join(SHOTS, "mode-pending.png") });
  await page.getByTestId("mode-result").waitFor({ timeout: 4000 });
  assert.equal(await page.getByTestId("mode-result").innerText(), "Profile: done");
  assert.equal(await row.locator('[data-pending="true"]').count(), 0, "pending clears with the answer");
  assert.equal(await row.getByRole("radio", { name: "Sport" }).isDisabled(), false);
  await ctx.close();
  console.log("Mode sheet: the pick is pending at once and the row waits for the answer");
}

/** A tap on a hold control does not act: it reads "Keep holding · <s> s"
 *  for 1.5 s, then its label again. */
async function holdHintCase(browser) {
  const sim = { pilot: PILOT_AIR, airborne: true };
  const { ctx, page } = await pilotPage(browser, { width: 1280, height: 800 }, sim);
  await page.getByTestId("flight-state-chip").click();
  await sheetSettled(page);
  const off = page.locator('[data-row="avoid"]').getByRole("button", { name: "Hold to turn avoidance off" });
  await off.click();
  const hint = off.getByTestId("keep-holding");
  await debugOnFail(page, "hold-hint", async () => {
    await hint.waitFor({ state: "visible", timeout: 500 });
  });
  assert.match(await hint.innerText(), /^Keep holding · \d\.\d s$/);
  await page.screenshot({ path: path.join(SHOTS, "hold-hint.png") });
  const shownAt = Date.now();
  await hint.waitFor({ state: "detached", timeout: 3000 });
  const shownMs = Date.now() - shownAt;
  assert.ok(shownMs > 1000, `the hint stays about 1.5 s (gone after ${shownMs} ms)`);
  assert.ok(!sim.sent.some((f) => f.op === "action" && String(f.name).startsWith("avoid:")), "a tap sends nothing");
  await ctx.close();
  console.log(`hold hint: a tap reads Keep holding, gone after ${shownMs} ms, nothing sent`);
}

/** Escape, a press outside and the controller's back close the topmost
 *  overlay first: a press on the chip with a sheet open closes the sheet and
 *  does not open the Mode sheet. */
async function overlayCloseCase(browser) {
  const sim = { pilot: PILOT_GROUND, airborne: false };
  const { ctx, page } = await pilotPage(browser, { width: 1280, height: 800 }, sim);
  const openers = GROUND_SHEETS.filter(([name]) => name !== "preflight");
  for (const [name, openIt] of openers) {
    await openIt(page);
    await sheetSettled(page);
    await page.keyboard.press("Escape");
    await page.locator(SHEET).waitFor({ state: "detached", timeout: 1000 }).catch(() => assert.fail(`${name}: Escape closes it`));
    await openIt(page);
    await sheetSettled(page);
    await page.getByTestId("flight-state-chip").click();
    await page.locator(SHEET).waitFor({ state: "detached", timeout: 1000 }).catch(() => assert.fail(`${name}: a press outside closes it`));
    await page.waitForTimeout(300);
    assert.equal(await page.locator(SHEET).count(), 0, `${name}: the press that closed it did nothing else`);
    await openIt(page);
    await sheetSettled(page);
    await page.evaluate(() => window.__press(1, true));
    await page.waitForTimeout(100);
    await page.evaluate(() => window.__press(1, false));
    await page.locator(SHEET).waitFor({ state: "detached", timeout: 1000 }).catch(() => assert.fail(`${name}: the controller's back closes it`));
  }
  await ctx.close();
  console.log("Escape, a press outside and the controller's back close the topmost sheet first");
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

/** The alert column on the kiosk: a GPS alert card, the handover banner,
 *  "Sticks are with <surface>" and the layer chips ("Obstacles
 *  unavailable" once the demo layer feed stops for more than 2 s) all show
 *  at once, and no two of them overlap. */
async function chipsCase(browser) {
  const ctx = await browser.newContext({ viewport: { width: 1920, height: 1080 } });
  const page = await ctx.newPage();
  await page.addInitScript(PAD_INIT);
  // The demo layer feed ticks every 50 ms: record those timers to stop them.
  await page.addInitScript(() => {
    const set = window.setInterval.bind(window);
    window.__feedTimers = [];
    window.setInterval = (fn, ms, ...rest) => {
      const id = set(fn, ms, ...rest);
      if (ms === 50) window.__feedTimers.push(id);
      return id;
    };
  });
  await mockAgent(page, { profile: "ground_station" });
  await page.route("**/api/telemetry", (route) =>
    route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({ armed: true, mode: "GUIDED", autopilot: 3, attitude: { roll: 0, pitch: 0, yaw: 0 }, position: { alt_rel: 3, heading: 0 }, velocity: { groundspeed: 0, climb: 0 }, gps: { fix_type: 1, satellites: 0 }, last_update: new Date().toISOString() }),
    }),
  );
  const pushes = [];
  await page.routeWebSocket(/\/api\/v1\/pilot\/stream/, (ws) => {
    const state = {
      owner: "pilot", control_mode: "ados", profile: "normal", avoid: "brake", armed: true, landed: false, vehicle_class: "multirotor", readiness: [], plan_ned: [],
      input_session: { src: "bench", label: "Bench", session: 7 },
      banner: { text_key: "pilot.banner.ados_control", args: { profile: "NORMAL" }, until_ms: null },
    };
    const push = setInterval(() => ws.send(JSON.stringify({ kind: "pilot", ts_ms: Date.now(), data: state })), 100);
    pushes.push(push);
    ws.onMessage(() => undefined);
    ws.onClose(() => clearInterval(push));
  });
  await page.goto(`${BASE}?kiosk=1&demo=1`);
  await page.waitForSelector('[data-testid="sticks-elsewhere"]', { state: "visible", timeout: 10000 });
  assert.equal(await page.locator('[data-testid="layer-chip"]').count(), 0, "no chip while the layers are fresh");
  await page.evaluate(() => window.__feedTimers.forEach((id) => clearInterval(id)));
  await page.waitForSelector('[data-testid="layer-chip"]', { state: "visible", timeout: 10000 });
  assert.ok((await page.locator('[data-testid="layer-chip"]').allInnerTexts()).includes("Obstacles unavailable"), "the obstacles chip shows");
  await page.waitForTimeout(500);
  await page.screenshot({ path: path.join(SHOTS, "kiosk-alert-column.png") });
  const boxes = await page.$$eval('[data-testid="feed-notices"] > *, [data-testid="sticks-elsewhere"], [data-testid="handover-banner"], [data-testid="layer-chip"], [role="alert"] > div, [role="status"] > div.border-2', (els) =>
    els.map((e) => {
      const r = e.getBoundingClientRect();
      return { what: (e.getAttribute("data-testid") ?? e.textContent ?? "").slice(0, 40), x: r.left, y: r.top, w: r.width, h: r.height };
    }),
  );
  const chip = boxes.find((b) => b.what === "layer-chip");
  assert.ok(chip && chip.w > 0, "the layer chip is drawn");
  assert.ok(boxes.length >= 3, `banners, cards and chip present: ${JSON.stringify(boxes)}`);
  for (let i = 0; i < boxes.length; i++) {
    for (let j = i + 1; j < boxes.length; j++) {
      const a = boxes[i];
      const b = boxes[j];
      const overlap = a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
      const nested = (a.x >= b.x && a.y >= b.y && a.x + a.w <= b.x + b.w && a.y + a.h <= b.y + b.h) || (b.x >= a.x && b.y >= a.y && b.x + b.w <= a.x + a.w && b.y + b.h <= a.y + a.h);
      assert.ok(!overlap || nested, `"${a.what}" overlaps "${b.what}": ${JSON.stringify(a)} ${JSON.stringify(b)}`);
    }
  }
  pushes.forEach((p) => clearInterval(p));
  await ctx.close();
  console.log(`kiosk alert column: ${boxes.map((b) => b.what).join(" | ")}; no overlaps`);
}

(async () => {
  const browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME });
  const only = process.argv[2];
  try {
    if (!only || only === "stop") await stopCases(browser);
    if (!only || only === "stop-sheets") await stopSheetsCase(browser);
    if (!only || only === "flow-stale") await flowStaleCase(browser);
    if (!only || only === "sequence") await sequenceCase(browser);
    if (!only || only === "mode-pending") await modePendingCase(browser);
    if (!only || only === "hold-hint") await holdHintCase(browser);
    if (!only || only === "overlay-close") await overlayCloseCase(browser);
    if (!only || only === "recentre") await recentreCase(browser);
    if (!only || only === "gimbal-flick") await gimbalFlickCase(browser);
    if (!only || only === "chips") await chipsCase(browser);
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
