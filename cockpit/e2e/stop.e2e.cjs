// Emergency stop and the controller safety combos, against a built bundle
// (see harness.cjs for how to run). Cases:
//   stop         the first press on STOP lands on STOP with the status-pill
//                list open and closed, the list drawn on top, at every width
//                class and the kiosk;
//   stop-sheets  the same with every flight-screen sheet open, each sheet
//                starting below the top bar;
//   stop-hold    with a sheet open, STOP opens the canonical STOP sheet
//                (spec title, body and hold label): a 1 s press counts
//                down and sends nothing, a 3.2 s hold sends exactly one kill;
//   panel-stop   on the kiosk, the panel's next/previous reach STOP in one
//                press, long select twice fires it, long back never does;
//   pad-safety   with a sheet open, a pad holding L2+R2+Circle for the hold
//                time sends one kill and Options+Circle held sends one RTH.

const assert = require("node:assert/strict");
const path = require("node:path");

const { AIR_SHEETS, GROUND_SHEETS, PAD, PAD_INIT, PILOT_AIR, PILOT_GROUND, SHEET, SHOTS, VIEWPORTS, BASE, debugOnFail, hits, holdOn, mockAgent, pilotPage, runCases, sheetSettled } = require("./harness.cjs");

const STOP = '[data-testid="top-bar-stop"]';

/** STOP takes the first press with the status-pill list open and closed, at
 *  every width class, and the list is drawn (in the window, on top). */
async function stopCases(browser) {
  for (const vp of [...VIEWPORTS, { name: "kiosk-tv", width: 1920, height: 1080, query: "?kiosk=1&demo=1" }]) {
    const ctx = await browser.newContext({ viewport: { width: vp.width, height: vp.height } });
    const page = await ctx.newPage();
    await page.addInitScript(PAD_INIT);
    await mockAgent(page);
    await page.goto(`${BASE}${vp.query ?? "?demo=1"}`);
    await page.waitForSelector(STOP, { timeout: 8000 });
    await page.waitForTimeout(1500);
    const skip = page.getByRole("button", { name: "Skip" });
    if (await skip.count()) await skip.first().click();
    const pill = '[data-zone="status_pill"]';
    const list = '[data-testid="status-pill-list"]';
    assert.equal(await hits(page, STOP), true, `${vp.name}: STOP reachable with the list closed`);
    await page.click(pill);
    await page.waitForSelector(list, { state: "visible", timeout: 2000 });
    const box = await page.locator(list).boundingBox();
    assert.ok(box && box.width > 0 && box.height > 0, `${vp.name}: the list has a size`);
    assert.ok(box.y >= 0 && box.y + Math.min(box.height, 24) <= vp.height, `${vp.name}: the list is inside the window`);
    assert.equal(await hits(page, list), true, `${vp.name}: the list is drawn on top`);
    assert.equal(await hits(page, STOP), true, `${vp.name}: STOP reachable with the list open`);
    // One list layout on every surface: heading "Alerts" as written, each
    // item's text above its source label, the spec's severity glyph.
    const layout = await page.$eval(list, (el) => {
      const title = el.querySelector('[data-testid="status-pill-list-title"]');
      const items = [...el.querySelectorAll('[data-testid="alert-item"]')].map((it) => ({
        icon: it.querySelector('[data-testid="alert-item-icon"]').textContent,
        textY: it.querySelector('[data-testid="alert-item-text"]').getBoundingClientRect().top,
        sourceY: it.querySelector('[data-testid="alert-item-source"]').getBoundingClientRect().top,
      }));
      return { title: title.innerText, items };
    });
    assert.equal(layout.title, "Alerts", `${vp.name}: the list heading`);
    for (const it of layout.items) {
      assert.ok(["✕", "⚠", "ℹ"].includes(it.icon), `${vp.name}: a spec severity glyph (${it.icon})`);
      assert.ok(it.textY < it.sourceY, `${vp.name}: the text is above its source label`);
    }
    await page.screenshot({ path: path.join(SHOTS, `pill-open-${vp.name}.png`) });
    // One real press on STOP with the list open reaches STOP, and closes the list.
    await page.$eval(STOP, (el) => {
      window.__stopDowns = 0;
      el.addEventListener("pointerdown", () => (window.__stopDowns += 1));
    });
    const s = await page.locator(STOP).boundingBox();
    await page.mouse.move(s.x + s.width / 2, s.y + s.height / 2);
    await page.mouse.down();
    await page.mouse.up();
    assert.equal(await page.evaluate(() => window.__stopDowns), 1, `${vp.name}: STOP took the first press`);
    await page.waitForSelector(list, { state: "detached", timeout: 2000 });
    await ctx.close();
  }
  console.log("STOP reachable with the status list open and closed at every width class");
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
    const check = async (name) => {
      await sheetSettled(page);
      assert.equal(await hits(page, STOP), true, `${vp.name} ${name}: STOP is the topmost element at its centre`);
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
      await page.$eval(STOP, (el) => {
        window.__stopDowns = 0;
        el.addEventListener("pointerdown", () => (window.__stopDowns += 1), { once: true });
      });
      const s = await page.locator(STOP).boundingBox();
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

/** How many times the cockpit has sent `pilotAction` (on the pilot stream)
 *  or its `command` twin (`POST /api/command`). */
function sentCount(sim, pilotAction, command) {
  return sim.sent.filter((f) => f.op === "action" && f.name === pilotAction).length + sim.commands.filter((c) => c.cmd === command).length;
}

/** Wait up to `ms` for `pred`, polling the page clock. */
async function until(page, what, pred, ms) {
  const end = Date.now() + ms;
  while (!pred()) {
    if (Date.now() > end) assert.fail(`timed out waiting for ${what}`);
    await page.waitForTimeout(25);
  }
}

/** Open the Mode sheet, the sheet the safety paths must work through. */
async function openModeSheet(page) {
  await page.getByTestId("flight-state-chip").click();
  await sheetSettled(page);
}

/** With the Mode sheet open, STOP opens the canonical STOP sheet: spec
 *  title, kill warning and hold label, with no second confirm. Released after
 *  1 s it counts down and sends nothing; held for 3.2 s it sends exactly one
 *  kill and closes. */
async function stopHoldCase(browser) {
  const text = require("../src/shared/cockpit-spec/setup-strings.json").strings;
  const sim = { pilot: PILOT_AIR, airborne: true };
  const { ctx, page } = await pilotPage(browser, { width: 1280, height: 800 }, sim);
  const kills = () => sentCount(sim, "kill", "killswitch");
  await openModeSheet(page);
  await page.locator(STOP).click();
  const sheet = page.locator(SHEET, { hasText: text["cockpit.estop"] });
  await sheet.waitFor({ state: "visible", timeout: 2000 });
  await sheetSettled(page);
  assert.equal((await page.getByTestId("stop-sheet-title").textContent()).trim(), text["cockpit.estop"]);
  assert.equal((await page.getByTestId("stop-sheet-body").textContent()).trim(), text["confirm.kill.message"]);
  const hold = page.getByTestId("stop-sheet-hold");
  assert.equal((await hold.textContent()).trim(), text["stop.hold_label"]);
  assert.equal(await page.getByTestId("confirm-final").count(), 0, "no second confirm step");
  await page.screenshot({ path: path.join(SHOTS, "stop-sheet.png") });

  const b = await hold.boundingBox();
  await page.mouse.move(b.x + b.width / 2, b.y + b.height / 2);
  await page.mouse.down();
  await page.waitForTimeout(1000);
  const counting = (await hold.textContent()).trim();
  assert.match(counting, /^Keep holding · \d\.\d s$/, `the hold counts down while held: ${counting}`);
  await page.screenshot({ path: path.join(SHOTS, "stop-sheet-holding.png") });
  await page.mouse.up();
  await page.waitForTimeout(600);
  assert.equal(kills(), 0, `a 1 s press sends nothing (sent ${JSON.stringify(sim.sent)}, commands ${JSON.stringify(sim.commands)})`);

  await debugOnFail(page, "stop-hold", async () => {
    await holdOn(page, hold, 3200);
    await until(page, "the kill", () => kills() >= 1, 3000);
  });
  await page.waitForTimeout(800);
  assert.equal(kills(), 1, `exactly one kill (sent ${JSON.stringify(sim.sent)}, commands ${JSON.stringify(sim.commands)})`);
  await sheet.waitFor({ state: "detached", timeout: 2000 });
  await ctx.close();
  console.log("STOP: the spec sheet counts down, a 1 s press sends nothing; the full 3 s hold sends exactly one kill");
}

/** Hold pad buttons (modifiers first, in order) for `ms`, then release them
 *  all; `early.check()` runs `early.atMs` into the hold, to prove nothing
 *  fired before the hold time. */
async function padHold(page, buttons, ms, early) {
  for (const b of buttons) {
    await page.evaluate((i) => window.__press(i, true), b);
    await page.waitForTimeout(50);
  }
  await page.waitForTimeout(early.atMs);
  await early.check();
  await page.waitForTimeout(ms - early.atMs);
  for (const b of [...buttons].reverse()) await page.evaluate((i) => window.__press(i, false), b);
}

/** With the Mode sheet open, the pad's locked safety combos complete: L2+R2
 *  held with Circle for 3 s sends exactly one kill (nothing before the hold
 *  time), and Options+Circle held for 2 s sends exactly one RTH. */
async function padSafetyCase(browser) {
  const sim = { pilot: PILOT_AIR, airborne: true };
  const { ctx, page } = await pilotPage(browser, { width: 1280, height: 800 }, sim);
  const kills = () => sentCount(sim, "kill", "killswitch");
  const rths = () => sentCount(sim, "rth", "rtl");
  // The pad is seen once a frame of it has been read.
  await page.waitForTimeout(500);

  await openModeSheet(page);
  await debugOnFail(page, "pad-kill", async () => {
    await padHold(page, [PAD.l2, PAD.r2, PAD.circle], 3300, {
      atMs: 2400,
      check: async () => assert.equal(kills(), 0, "no kill before the 3 s hold completes"),
    });
    await until(page, "the pad kill", () => kills() >= 1, 2000);
  });
  await page.waitForTimeout(800);
  assert.equal(kills(), 1, `the kill combo sends exactly one kill (sent ${JSON.stringify(sim.sent)}, commands ${JSON.stringify(sim.commands)})`);

  if ((await page.locator(SHEET).count()) === 0) await openModeSheet(page);
  await debugOnFail(page, "pad-rth", async () => {
    await padHold(page, [PAD.options, PAD.circle], 2300, {
      atMs: 1500,
      check: async () => assert.equal(rths(), 0, "no RTH before the 2 s hold completes"),
    });
    await until(page, "the pad RTH", () => rths() >= 1, 2000);
  });
  await page.waitForTimeout(800);
  assert.equal(rths(), 1, `the RTH combo sends exactly one RTH (sent ${JSON.stringify(sim.sent)}, commands ${JSON.stringify(sim.commands)})`);
  assert.equal(kills(), 1, "the RTH combo sends no kill");
  await ctx.close();
  console.log("pad safety combos with a sheet open: L2+R2+Circle held sends one kill, Options+Circle held sends one RTH");
}

/** The ground-station panel on the kiosk, from a fresh flight screen with
 *  nothing focused: one press of next (and, separately, previous) focuses
 *  STOP; a long select opens the STOP sheet and sends nothing; a long back
 *  closes it and sends nothing; a second long select a second after the
 *  first sends exactly one kill; on a sheet a short select opened, the first
 *  long select only arms it and a second one a second later fires. */
async function panelStopCase(browser) {
  const STEP = { next: { button: "2", label: "b2" }, prev: { button: "1", label: "b1" } };
  const select = { button: "3", label: "b3", kind: "long", action: "select" };
  const back = { button: "4", label: "b4", kind: "long", action: "menu" };
  for (const step of ["next", "prev"]) {
    const sim = { pilot: PILOT_AIR, airborne: true };
    let panel = null;
    const vp = {
      width: 1920,
      height: 1080,
      profile: "ground_station",
      beforeGoto: async (page) => {
        await page.route("**/api/_ws/ticket", (r) => r.fulfill({ contentType: "application/json", body: JSON.stringify({ ticket: "t" }) }));
        await page.routeWebSocket(/\/ws\/buttons/, (ws) => {
          panel = ws;
        });
      },
    };
    const { ctx, page } = await pilotPage(browser, vp, sim, "?kiosk=1");
    const kills = () => sentCount(sim, "kill", "killswitch");
    const send = (ev) => panel.send(JSON.stringify({ ...ev, timestamp_ms: Date.now() }));
    // From nothing focused, one press of the panel's next (or previous)
    // button lands on STOP, as a pilot reaches it.
    const reachStop = async () => {
      await page.evaluate(() => document.activeElement instanceof HTMLElement && document.activeElement.blur());
      send({ ...STEP[step], kind: "short", action: step });
      await page.waitForFunction(() => document.activeElement?.closest('[data-zone="stop"]') != null, null, { timeout: 2000 });
    };
    await debugOnFail(page, `panel-stop-${step}`, async () => {
      await until(page, "the panel socket", () => panel !== null, 8000);
      await page.waitForTimeout(500);
      await reachStop();
      send(select);
      await page.getByTestId("stop-sheet-title").waitFor({ timeout: 2000 });
      assert.equal(kills(), 0, "one long select opens the STOP sheet and sends nothing");
      await page.waitForTimeout(1200);
      send(back);
      await page.locator(SHEET, { hasText: "Emergency stop" }).waitFor({ state: "detached", timeout: 2000 });
      await page.waitForTimeout(400);
      assert.equal(kills(), 0, "a long back closes the STOP sheet and sends nothing");
      await reachStop();
      send(select);
      await page.getByTestId("stop-sheet-title").waitFor({ timeout: 2000 });
      await page.waitForTimeout(1100);
      send(select);
      await until(page, "the kill", () => kills() >= 1, 3000);
      await page.waitForTimeout(500);
      assert.equal(kills(), 1, "the second long select sends exactly one kill");
      // A sheet opened by a short select is armed only by a long select:
      // one long select a second later sends nothing, a second one fires.
      await page.waitForTimeout(300);
      await reachStop();
      send({ ...select, kind: "short" });
      await page.getByTestId("stop-sheet-title").waitFor({ timeout: 2000 });
      await page.waitForTimeout(1200);
      send(select);
      await page.waitForTimeout(600);
      assert.equal(kills(), 1, "one long select on a short-select sheet sends nothing");
      await page.waitForTimeout(600);
      send(select);
      await until(page, "the second kill", () => kills() >= 2, 3000);
      await page.waitForTimeout(500);
      assert.equal(kills(), 2, "the second long select on a short-select sheet sends exactly one kill");
    });
    if (step === "next") await page.screenshot({ path: path.join(SHOTS, "panel-stop-kiosk.png") });
    await ctx.close();
  }
  console.log("Kiosk panel: next or previous reaches STOP in one press; long select twice a second apart sends one kill, long back never does");
}

runCases({
  stop: stopCases,
  "stop-sheets": stopSheetsCase,
  "stop-hold": stopHoldCase,
  "pad-safety": padSafetyCase,
  "panel-stop": panelStopCase,
});
