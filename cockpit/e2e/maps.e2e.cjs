// End-to-end checks of the cockpit maps and instruments against a built
// bundle served by `vite preview`, with the agent API and the pilot stream
// mocked. Run with a Playwright install on NODE_PATH and a Chromium binary:
//
//   npx vite build && npx vite preview --port 4273 &
//   NODE_PATH=<dir containing playwright-core> CHROME=<chromium path> \
//     SHOTS=<screenshot dir> node e2e/maps.e2e.cjs [case]
//
// Cases: Fly Here from a right-click and a long-press on the card map, the
// full map and the local-frame map, the ground gesture opening the Launch
// sheet, the full map's labels and north mark clear of the top bar, the
// minimap readout row, and the altitude-reference toggle clear of the ALT
// tape title at the Instruments level.

const { chromium } = require("playwright-core");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const BASE = process.env.BASE ?? "http://127.0.0.1:4273/cockpit/";
const SHOTS = process.env.SHOTS ?? path.join(process.cwd(), "e2e-shots");
fs.mkdirSync(SHOTS, { recursive: true });

const FLYING = { owner: "pilot", control_mode: "ados", profile: "normal", avoid: "brake", armed: true, landed: false, vehicle_class: "multirotor", readiness: [], plan_ned: [], surface_ops: ["goto"] };
const GROUND = { ...FLYING, owner: "idle", armed: false, landed: true };

function telemetry({ fix = 3, local = false } = {}) {
  return {
    armed: true,
    mode: "GUIDED",
    autopilot: 3,
    attitude: { roll: 0.05, pitch: -0.03, yaw: 1.2 },
    position: { lat: 12.9716, lon: 77.5946, alt_rel: 12, alt_msl: 912, heading: 70 },
    velocity: { groundspeed: 2, climb: 0.3 },
    gps: { fix_type: fix, satellites: fix >= 3 ? 14 : 0 },
    local_position: local ? { x: 20, y: -6, z: -12, vx: 0, vy: 0, vz: 0, age_ms: 100 } : null,
    last_update: new Date().toISOString(),
  };
}

async function open(browser, { pilot, fix, local, width = 1280, height = 800 }) {
  const ctx = await browser.newContext({ viewport: { width, height } });
  const page = await ctx.newPage();
  await page.route("**/api/**", async (route) => {
    const p = new URL(route.request().url()).pathname;
    const json = (body, status = 200) => route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
    if (p === "/api/pairing/info") return json({ profile: "drone", name: "bench-drone", paired: true, device_id: "drone-1" });
    if (p === "/api/config") return json({ agent: { name: "bench-drone" } });
    if (p === "/api/plugins") return json({ installs: [] });
    if (p === "/api/telemetry") return json(telemetry({ fix, local }));
    return json({ detail: "not mocked" }, 404);
  });
  const sent = [];
  await page.routeWebSocket(/\/api\/v1\/pilot\/stream/, (ws) => {
    // Closing the context does not always close the mocked socket: unref the
    // timer so it never keeps the run alive.
    const push = setInterval(() => ws.send(JSON.stringify({ kind: "pilot", ts_ms: Date.now(), data: pilot })), 100).unref();
    ws.onMessage((m) => sent.push(String(m)));
    ws.onClose(() => clearInterval(push));
  });
  await page.goto(BASE);
  await page.waitForSelector('[data-testid="minimap"]', { timeout: 10000 });
  await page.waitForTimeout(1500);
  return { ctx, page, sent };
}

async function setView(page, view) {
  for (let i = 0; i < 6 && (await page.getAttribute("[data-flight-view]", "data-flight-view")) !== view; i++) {
    await page.keyboard.press("v");
    await page.waitForTimeout(250);
  }
  assert.equal(await page.getAttribute("[data-flight-view]", "data-flight-view"), view, `view ${view}`);
}

async function box(page, selector) {
  return page.$eval(selector, (e) => {
    const r = e.getBoundingClientRect();
    return { x: r.left, y: r.top, w: r.width, h: r.height };
  });
}

const flyHereSheet = (page) => page.getByRole("dialog").filter({ hasText: "Hold to fly here" });

async function rightClickOpensFlyHere(page, selector, shot) {
  const b = await box(page, selector);
  await page.mouse.click(b.x + b.w * 0.7, b.y + b.h * 0.6, { button: "right" });
  await flyHereSheet(page).waitFor({ state: "visible", timeout: 3000 });
  if (shot) await page.screenshot({ path: path.join(SHOTS, shot) });
  await page.keyboard.press("Escape");
  await flyHereSheet(page).waitFor({ state: "hidden", timeout: 3000 });
}

async function longPressOpensFlyHere(page, selector) {
  const b = await box(page, selector);
  await page.mouse.move(b.x + b.w * 0.3, b.y + b.h * 0.4);
  await page.mouse.down();
  await page.waitForTimeout(750);
  await page.mouse.up();
  await flyHereSheet(page).waitFor({ state: "visible", timeout: 3000 });
  await page.keyboard.press("Escape");
}

async function flyHereCases(browser) {
  const { ctx, page } = await open(browser, { pilot: FLYING, fix: 3 });
  // The bottom-left card map.
  await rightClickOpensFlyHere(page, '[data-zone="bottom_left_card"] [data-testid="minimap"]', "fix-P-11a-card-rightclick.png");
  await longPressOpensFlyHere(page, '[data-zone="bottom_left_card"] [data-testid="minimap"]');
  console.log("PASS card map: right-click and long-press open the Fly Here hold");
  // The full map view: labels and north mark clear of the top bar.
  await setView(page, "map");
  await page.waitForTimeout(400);
  const bar = await box(page, '[data-zone-rect="top_bar"]');
  const header = await box(page, '[data-zone="stage"] [data-testid="minimap-header"]');
  const north = await box(page, '[data-zone="stage"] [data-testid="minimap-north"]');
  await page.screenshot({ path: path.join(SHOTS, "fix-N-AuditAgent2-10-map-view.png") });
  // Nothing drawn over the frame label (top bar, video inset, columns): the
  // topmost element there belongs to the map (the label ignores pointers).
  const labelOnTop = await page.$eval('[data-zone="stage"] [data-testid="minimap-header"]', (e) => {
    const r = e.firstElementChild.getBoundingClientRect();
    const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
    return hit !== null && e.closest('[data-testid="minimap"]').contains(hit);
  });
  assert.ok(labelOnTop, "the frame label is not covered");
  assert.ok(header.y >= bar.y + bar.h, `frame labels under the top bar: header ${JSON.stringify(header)} bar ${JSON.stringify(bar)}`);
  assert.ok(north.y >= bar.y + bar.h, `north mark under the top bar: ${JSON.stringify(north)} bar ${JSON.stringify(bar)}`);
  const northPx = north.h;
  assert.ok(northPx < 24, `north mark at chrome size, not scaled with the map: ${northPx}px`);
  console.log(`PASS map view: labels at y=${header.y}, N at y=${north.y} (${northPx.toFixed(1)} px) below the bar bottom ${bar.y + bar.h}`);
  await rightClickOpensFlyHere(page, '[data-zone="stage"] [data-testid="minimap"]', "fix-P-11a-map-rightclick.png");
  // The split pane.
  await setView(page, "split");
  await page.waitForTimeout(400);
  await rightClickOpensFlyHere(page, '[data-zone="map_pane"] [data-testid="minimap"]', "fix-P-11a-split-rightclick.png");
  console.log("PASS full map and split pane: right-click opens the Fly Here hold");
  await ctx.close();
}

async function localCase(browser) {
  const { ctx, page, sent } = await open(browser, { pilot: FLYING, fix: 0, local: true });
  await page.waitForSelector('[data-testid="local-minimap"]', { timeout: 8000 });
  await rightClickOpensFlyHere(page, '[data-testid="local-minimap"]', "fix-K-6-local-flyhere.png");
  console.log("PASS local-frame map: right-click opens the Fly Here hold (lat/lon through the FC origin)");
  void sent;
  await ctx.close();
}

async function groundCase(browser) {
  const { ctx, page } = await open(browser, { pilot: GROUND, fix: 3 });
  const b = await box(page, '[data-zone="bottom_left_card"] [data-testid="minimap"]');
  await page.mouse.click(b.x + b.w * 0.75, b.y + b.h * 0.3, { button: "right" });
  const launch = page.getByRole("dialog").filter({ hasText: "Go to a point" });
  await launch.waitFor({ state: "visible", timeout: 3000 });
  await page.screenshot({ path: path.join(SHOTS, "fix-P-11b-ground-launch.png") });
  assert.equal(await flyHereSheet(page).count(), 0, "no Fly Here on the ground");
  console.log("PASS ground: right-click on the map opens the Launch sheet");
  await ctx.close();
}

async function layoutCase(browser) {
  for (const vp of [
    { width: 1280, height: 800, name: "1280x800", scale: 1 },
    { width: 1920, height: 1080, name: "1920x1080", scale: 1.33 },
  ]) {
    const { ctx, page } = await open(browser, { pilot: FLYING, fix: 3, width: vp.width, height: vp.height });
    // The minimap's readout: one row inside the map, at chrome size.
    const readout = await box(page, '[data-zone="bottom_left_card"] [data-testid="minimap-readout"]');
    const map = await box(page, '[data-zone="bottom_left_card"] [data-testid="minimap"]');
    assert.ok(readout.y + readout.h <= map.y + map.h && readout.x >= map.x, "readout inside the map");
    const fontPx = await page.$eval('[data-zone="bottom_left_card"] [data-testid="minimap-readout"]', (e) => parseFloat(getComputedStyle(e).fontSize));
    assert.equal(fontPx, 11, "readout at the chrome label size (the zone zoom applies the class scale)");
    // Instruments: the altitude-reference toggle clear of the ALT tape title.
    for (let i = 0; i < 5 && (await page.getAttribute("[data-layer-level]", "data-layer-level")) !== "instruments"; i++) {
      await page.keyboard.press("o");
      await page.waitForTimeout(250);
    }
    assert.equal(await page.getAttribute("[data-layer-level]", "data-layer-level"), "instruments");
    const hud = await box(page, '[data-testid="hud"]');
    const toggle = await box(page, '[data-testid="alt-ref-toggle"]');
    const u = 20 * vp.scale;
    const R = Math.max(40, Math.min(hud.w * 0.26, hud.h * 0.4));
    const tapeTop = hud.y + hud.h / 2 - R * 0.9;
    const title = { x: hud.x + hud.w - u * 3.4, y: tapeTop - u * 0.8, w: 7 * 0.6 * u * 0.62, h: u * 0.6 };
    const overlap = toggle.x < title.x + title.w && title.x < toggle.x + toggle.w && toggle.y < title.y + title.h && title.y < toggle.y + toggle.h;
    await page.screenshot({ path: path.join(SHOTS, `fix-K-10-instruments-${vp.name}.png`) });
    assert.ok(!overlap, `toggle ${JSON.stringify(toggle)} overlaps the ALT title ${JSON.stringify(title)}`);
    const tFont = await page.$eval('[data-testid="alt-ref-toggle"]', (e) => parseFloat(getComputedStyle(e).fontSize));
    assert.ok(Math.abs(tFont - 11 * vp.scale) < 0.5, `toggle text ${tFont}px = 11 px × ${vp.scale}`);
    console.log(`PASS ${vp.name}: readout row in the map at 11 px; REL/MSL toggle at ${tFont}px clear of the ALT title`);
    await ctx.close();
  }
}

(async () => {
  const browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME });
  const only = process.argv[2];
  try {
    if (!only || only === "fly-here") await flyHereCases(browser);
    if (!only || only === "local") await localCase(browser);
    if (!only || only === "ground") await groundCase(browser);
    if (!only || only === "layout") await layoutCase(browser);
  } finally {
    await browser.close();
  }
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
