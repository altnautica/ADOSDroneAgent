// End-to-end check of the flight screen's contextual tips (cockpit spec
// `tips.json`) against a built bundle served by `vite preview`, with the agent
// API mocked and the pilot stream reporting a vehicle on the ground. Run with
// a Playwright install on NODE_PATH and a Chromium binary:
//
//   npx vite build && npx vite preview --port 4286 &
//   BASE=http://127.0.0.1:4286/cockpit/ NODE_PATH=<dir containing playwright-core> \
//     CHROME=<chromium path> SHOTS=<screenshot dir> node e2e/tips.e2e.cjs
//
// On a fresh localStorage: the status-pill tip shows after the first delay
// beside the pill; STOP stays the topmost element at its centre; "Got it"
// brings the flight-state chip tip after the gap; arming hides the tip and no
// tip shows for 30 s; a reload shows none of the seen tips; Settings › Reset
// tips brings the first tip back on the next Feed open.

const { chromium } = require("playwright-core");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const BASE = process.env.BASE ?? "http://127.0.0.1:4286/cockpit/";
const SHOTS = process.env.SHOTS ?? path.join(process.cwd(), "e2e-shots");
fs.mkdirSync(SHOTS, { recursive: true });

const SPEC_DIR = path.join(__dirname, "..", "src", "shared", "cockpit-spec");
const STRINGS = JSON.parse(fs.readFileSync(path.join(SPEC_DIR, "setup-strings.json"), "utf8")).strings;
const RULES = JSON.parse(fs.readFileSync(path.join(SPEC_DIR, "tips.json"), "utf8")).rules;

const TIP = '[data-testid="cockpit-tip"]';

/** The vehicle the mocks report; flipped by the test. */
const vehicle = { armed: false };
/** The mocked pilot streams' timers, cleared when the run ends. */
const pushes = [];

async function mockAgent(page) {
  await page.route("**/api/**", async (route) => {
    const p = new URL(route.request().url()).pathname;
    const json = (body, status = 200) => route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
    if (p === "/api/pairing/info") return json({ profile: "drone", name: "bench-drone", paired: true, device_id: "drone-1" });
    if (p === "/api/config") return json({ agent: { name: "bench-drone" } });
    if (p === "/api/plugins") return json({ installs: [] });
    // The status poll answers, so the agent-offline banner (which holds tips)
    // stays down.
    if (p === "/api/status/full") return json({ health: {}, telemetry: {} });
    if (p === "/api/telemetry")
      return json({
        armed: vehicle.armed,
        mode: "LOITER",
        autopilot: 3,
        attitude: { roll: 0, pitch: 0, yaw: 0 },
        position: { alt_rel: 0, heading: 70 },
        velocity: { groundspeed: 0, climb: 0 },
        last_update: new Date().toISOString(),
      });
    return json({ detail: "not mocked" }, 404);
  });
  await page.routeWebSocket(/\/api\/v1\/pilot\/stream/, (ws) => {
    const push = setInterval(() => {
      const data = {
        owner: "idle",
        control_mode: "ados",
        profile: "normal",
        avoid: "brake",
        armed: vehicle.armed,
        landed: !vehicle.armed,
        vehicle_class: "multirotor",
        plan_ned: [],
        readiness: [],
      };
      ws.send(JSON.stringify({ kind: "pilot", ts_ms: Date.now(), data }));
    }, 100);
    ws.onMessage(() => undefined);
    pushes.push(push);
    ws.onClose(() => clearInterval(push));
  });
}

/** The tip on screen now: its id, text and rect, or null. */
function tipNow(page) {
  return page.evaluate((sel) => {
    const el = document.querySelector(sel);
    if (!el) return null;
    const r = el.getBoundingClientRect();
    if (getComputedStyle(el).visibility === "hidden") return null;
    return { id: el.getAttribute("data-tip"), text: el.textContent, x: r.left, y: r.top, w: r.width, h: r.height };
  }, TIP);
}

function rectOf(page, selector) {
  return page.$eval(selector, (el) => {
    const r = el.getBoundingClientRect();
    return { x: r.left, y: r.top, w: r.width, h: r.height };
  });
}

/** Wait up to `ms` for a tip with `id`; returns when it showed, ms after `from`. */
async function waitTip(page, id, ms) {
  const start = Date.now();
  while (Date.now() - start < ms) {
    const t = await tipNow(page);
    if (t && t.id === id) return t;
    await page.waitForTimeout(100);
  }
  throw new Error(`tip ${id} did not show within ${ms} ms`);
}

/** Every tip id seen over `ms`. */
async function tipsOver(page, ms) {
  const seen = new Set();
  const start = Date.now();
  while (Date.now() - start < ms) {
    const t = await tipNow(page);
    if (t) seen.add(t.id);
    await page.waitForTimeout(250);
  }
  return seen;
}

function gap(a, b) {
  const dx = Math.max(0, a.x - (b.x + b.w), b.x - (a.x + a.w));
  const dy = Math.max(0, a.y - (b.y + b.h), b.y - (a.y + a.h));
  return Math.hypot(dx, dy);
}

function overlaps(a, b) {
  return a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
}

async function tipsCase(browser) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  ctx.setDefaultTimeout(15000);
  const page = await ctx.newPage();
  page.on("pageerror", (e) => console.log("pageerror:", String(e).slice(0, 400)));
  // The ground mock: the demo's scripted layer feed (a 50 ms timer) flies an
  // armed mission; keep it off so the pilot stream below is the only pilot.
  await page.addInitScript(() => {
    const set = window.setInterval.bind(window);
    window.setInterval = (fn, ms, ...rest) => (ms === 50 ? 0 : set(fn, ms, ...rest));
  });
  await mockAgent(page);
  await page.goto(`${BASE}?demo=1`);
  await page.waitForSelector("[data-zone=status_pill]", { timeout: 10000 });
  const opened = Date.now();

  // (1) The first tip shows after the first delay, beside the status pill.
  assert.equal(await tipNow(page), null, "no tip before the first delay");
  const first = await waitTip(page, "status_pill", RULES.first_delay_ms + 3000);
  const firstAfter = Date.now() - opened;
  assert.ok(firstAfter >= RULES.first_delay_ms - 300, `first tip after ${firstAfter} ms`);
  assert.ok(first.text.includes(STRINGS["tips.status_pill"]), first.text);
  assert.equal(STRINGS["tips.status_pill"], "Warnings collect here. Select to see them.");
  const pill = await rectOf(page, "[data-zone=status_pill]");
  assert.ok(gap(first, pill) <= 32, `tip ${JSON.stringify(first)} is beside the pill ${JSON.stringify(pill)}`);
  for (const zone of ["[data-zone=stop]", "[data-zone=left_column]", '[data-testid="flight-state-chip"]', "[data-zone=top_bar]"]) {
    assert.ok(!overlaps(first, await rectOf(page, zone)), `tip covers ${zone}`);
  }
  await page.screenshot({ path: path.join(SHOTS, "fix-TIPS.png") });

  console.log("tips: first tip beside the pill");
  // (2) STOP stays the topmost element at its centre.
  const stopHit = await page.$eval("[data-zone=stop]", (el) => {
    const r = el.getBoundingClientRect();
    const top = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
    return top === el || el.contains(top);
  });
  assert.ok(stopHit, "elementFromPoint at STOP's centre is STOP");

  console.log("tips: STOP on top");
  // (3) "Got it" brings the flight-state chip tip after the gap.
  await page.getByRole("button", { name: STRINGS["tips.dismiss"] }).click();
  const dismissed = Date.now();
  assert.equal(await tipNow(page), null, "the tip hides on Got it");
  await page.waitForTimeout(RULES.gap_between_ms - 600);
  assert.equal(await tipNow(page), null, "no tip inside the gap");
  const chipTip = await waitTip(page, "flight_state_chip", 2500);
  const gapMs = Date.now() - dismissed;
  assert.ok(gapMs >= RULES.gap_between_ms - 300, `second tip after ${gapMs} ms`);
  assert.ok(chipTip.text.includes(STRINGS["tips.flight_state_chip"]), chipTip.text);
  assert.ok(!overlaps(chipTip, await rectOf(page, '[data-testid="flight-state-chip"]')), "the chip tip covers the chip");

  console.log("tips: next tip after the gap");
  // (4) Arming hides the tip; no tip shows for 30 s.
  vehicle.armed = true;
  await page.waitForFunction((sel) => document.querySelector(sel) === null, TIP, { timeout: 2000 });
  const armedTips = await tipsOver(page, 30000);
  assert.equal(armedTips.size, 0, `tips while armed: ${[...armedTips].join(", ")}`);
  vehicle.armed = false;

  console.log("tips: none while armed");
  // (5) A reload shows none of the seen tips.
  await page.reload();
  await page.waitForSelector("[data-zone=status_pill]", { timeout: 10000 });
  const afterReload = await tipsOver(page, RULES.first_delay_ms + 2000);
  assert.ok(!afterReload.has("status_pill") && !afterReload.has("flight_state_chip"), `seen tips shown again: ${[...afterReload].join(", ")}`);
  const saved = await page.evaluate(() => JSON.parse(localStorage.getItem("ados-cockpit-tips") ?? "{}"));
  assert.deepEqual(saved.seen.slice(0, 2), ["status_pill", "flight_state_chip"]);

  console.log("tips: seen tips stay seen on reload");
  // (6) Settings › Reset tips, then the Feed shows the first tip again.
  await page.getByRole("button", { name: STRINGS["cockpit.menu"] }).click();
  await page.getByRole("dialog").getByRole("button", { name: /Settings/ }).first().click();
  await page.getByTestId("settings-reset-tips").click();
  assert.deepEqual(await page.evaluate(() => JSON.parse(localStorage.getItem("ados-cockpit-tips") ?? "{}")), { seen: [], open_count: 0 });
  await page.getByRole("button", { name: "Feed" }).first().click();
  await page.waitForSelector("[data-zone=status_pill]", { timeout: 10000 });
  await waitTip(page, "status_pill", RULES.first_delay_ms + 3000);
  // (7) A banner in the flight screen's notices hides the tip.
  await page.evaluate(() => {
    const b = document.createElement("div");
    b.setAttribute("role", "status");
    b.id = "e2e-banner";
    b.textContent = "banner";
    b.style.height = "24px";
    document.querySelector('[data-testid="feed-notices"]').appendChild(b);
  });
  const hideBy = Date.now() + 1500;
  while ((await tipNow(page)) !== null) {
    assert.ok(Date.now() < hideBy, "tip still shown beside an alert banner");
    await page.waitForTimeout(100);
  }
  await page.evaluate(() => document.getElementById("e2e-banner").remove());
  await ctx.close();
  console.log(`tips: first after ${firstAfter} ms, next after ${gapMs} ms, none while armed, seen kept on reload, reset brings them back, hidden under a banner`);
}

(async () => {
  const browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME });
  try {
    await tipsCase(browser);
  } finally {
    await browser.close();
    pushes.forEach((p) => clearInterval(p));
  }
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
