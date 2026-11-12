// Controls, the pilot stick stream, the gimbal pad and the kiosk alert column
// against a built bundle (see harness.cjs for how to run). Cases: controller
// config (`controls`: hotspot highlight, rebind persists, conflict blocks
// Save, a safety binding cannot be removed), the pilot stick stream
// (`stream`: 50 Hz input frames, release on a hidden tab), the gimbal pad
// (`recentre`: the recentre button takes a mouse click; `gimbal-flick`: a
// flick off the pad leaves no rate running) and the kiosk alert column
// (`chips`: banners, alert cards and layer chips never overlap).

const assert = require("node:assert/strict");
const path = require("node:path");

const { BASE, PAD_INIT, PILOT_AIR, SHOTS, VIEWPORTS, attr, mockAgent, pilotPage, runCases } = require("./harness.cjs");

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
 *  unavailable" once the layer feed stops for more than 2 s) all show at
 *  once, and no two of them overlap. On the demo (whose video never reads
 *  lost, so the layers draw): its pins put the sticks with another surface
 *  and a handover banner on the demo pilot, then stall its layer frames
 *  while the pilot keeps reporting. */
async function chipsCase(browser) {
  const ctx = await browser.newContext({ viewport: { width: 1920, height: 1080 } });
  const page = await ctx.newPage();
  page.on("pageerror", (e) => console.log("pageerror:", String(e).slice(0, 400)));
  await page.addInitScript(PAD_INIT);
  await mockAgent(page, { profile: "ground_station" });
  await page.route("**/api/telemetry", (route) =>
    route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({ armed: true, mode: "GUIDED", autopilot: 3, attitude: { roll: 0, pitch: 0, yaw: 0 }, position: { alt_rel: 3, heading: 0 }, velocity: { groundspeed: 0, climb: 0 }, gps: { fix_type: 1, satellites: 0 }, last_update: new Date().toISOString() }),
    }),
  );
  await page.goto(`${BASE}?kiosk=1&demo=1`);
  await page.waitForFunction(() => window.__adosDemoPilot !== undefined, null, { timeout: 10000 });
  await page.waitForTimeout(500);
  assert.equal(await page.locator('[data-testid="layer-chip"]').count(), 0, "no chip while the layers are fresh");
  await page.evaluate(() => {
    window.__adosDemoPilot.pin({
      owner: "pilot",
      input_session: { src: "bench", label: "Bench", session: 7 },
      banner: { text_key: "pilot.banner.ados_control", args: { profile: "NORMAL" }, until_ms: null },
    });
    window.__adosDemoPilot.stallLayers(true);
  });
  await page.waitForSelector('[data-testid="sticks-elsewhere"]', { state: "visible", timeout: 10000 });
  await page.waitForSelector('[data-testid="layer-chip"]', { state: "visible", timeout: 10000 });
  assert.ok((await page.locator('[data-testid="layer-chip"]').allInnerTexts()).includes("Obstacles unavailable"), "the obstacles chip shows");
  await page.waitForTimeout(500);
  await page.screenshot({ path: path.join(SHOTS, "kiosk-alert-column.png") });
  const boxes = await page.$$eval('[data-testid="feed-notices"] > *, [data-testid="sticks-elsewhere"], [data-testid="handover-banner"], [data-testid="layer-chip"], [role="alert"] > div, [data-testid="alert-row"]', (els) =>
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
  await ctx.close();
  console.log(`kiosk alert column: ${boxes.map((b) => b.what).join(" | ")}; no overlaps`);
}

/** Where the shell puts the Feed, and the top bar's room. A browser load is
 *  framed beside the menu rail; the Full screen button draws it full bleed;
 *  the kiosk is full bleed with no such button. At every size the chip keeps
 *  its whole text until every movable item is in the Menu: it never
 *  truncates to make room for an item the bar could have moved. */
async function shellCase(browser) {
  const shell = (page) => page.getAttribute("[data-shell]", "data-shell");
  const sim = { pilot: PILOT_AIR, airborne: true };
  {
    const { ctx, page } = await pilotPage(browser, { width: 1920, height: 1080 }, sim);
    assert.equal(await shell(page), "flight-framed", "a browser load frames the Feed");
    assert.equal(await page.locator('[data-shell="flight-framed"] > nav').count(), 1, "the menu rail sits beside it");
    await page.screenshot({ path: path.join(SHOTS, "shell-framed-1920x1080.png") });
    // Mark the Feed's top bar: the same node after the toggles proves the Feed
    // was not remounted (a remount releases the stick session).
    await page.$eval('[data-zone="top_bar"]', (el) => (el.dataset.e2eMark = "kept"));
    const kept = '[data-zone="top_bar"][data-e2e-mark="kept"]';
    await page.getByTestId("top-bar-fullscreen").click();
    await page.waitForSelector('[data-shell="flight"]', { timeout: 3000 });
    assert.equal(await page.locator("nav").count(), 0, "full screen draws no menu rail");
    assert.equal(await page.locator(kept).count(), 1, "full screen keeps the same Feed");
    await page.waitForTimeout(400);
    await page.screenshot({ path: path.join(SHOTS, "shell-fullscreen-1920x1080.png") });
    await page.getByTestId("top-bar-fullscreen").click();
    await page.waitForSelector('[data-shell="flight-framed"]', { timeout: 3000 });
    assert.equal(await page.locator(kept).count(), 1, "leaving full screen keeps the same Feed");
    await ctx.close();
  }
  {
    const { ctx, page } = await pilotPage(browser, { width: 1920, height: 1080 }, sim, "?kiosk=1");
    assert.equal(await shell(page), "flight", "the kiosk is full bleed");
    assert.equal(await page.getByTestId("top-bar-fullscreen").count(), 0, "the kiosk offers no Full screen button");
    await page.waitForTimeout(400);
    await page.screenshot({ path: path.join(SHOTS, "shell-kiosk-1920x1080.png") });
    await ctx.close();
  }
  for (const vp of VIEWPORTS) {
    const { ctx, page } = await pilotPage(browser, vp, sim, "?kiosk=1");
    // Let the bar settle: it moves one item a frame while it overflows.
    await page.waitForTimeout(1500);
    const bar = await page.$eval('[data-zone="top_bar"]', (el) => {
      const text = el.querySelector('[data-testid="flight-state-chip"] .truncate');
      return { moved: (el.getAttribute("data-moved") ?? "").split(" ").filter(Boolean).length, truncated: text ? text.scrollWidth > text.clientWidth + 1 : false };
    });
    if (bar.truncated) {
      await page.screenshot({ path: path.join(SHOTS, `debug-chip-${vp.name}.png`) });
      assert.equal(bar.moved, 5, `${vp.name}: the chip truncates only once every movable item is in the Menu (moved ${bar.moved})`);
    }
    if (vp.name === "compact") await page.screenshot({ path: path.join(SHOTS, "top-bar-compact-580x360.png") });
    console.log(`${vp.name}: ${bar.moved} items in the Menu, chip ${bar.truncated ? "truncated (bar full)" : "whole"}`);
    await ctx.close();
  }
  console.log("shell: framed by default, full bleed on Full screen and on the kiosk");
}

runCases({
  controls: controlsCases,
  stream: pilotStreamCase,
  recentre: recentreCase,
  "gimbal-flick": gimbalFlickCase,
  chips: chipsCase,
  shell: shellCase,
});
