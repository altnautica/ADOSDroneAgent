// Emergency stop and the controller safety combos, against a built bundle
// (see harness.cjs for how to run). Cases:
//   stop         the first press on STOP lands on STOP with the status-pill
//                list open and closed, the list drawn on top, at every width
//                class and the kiosk;
//   stop-sheets  the same with every flight-screen sheet open, each sheet
//                starting below the top bar;
//   stop-hold    with a sheet open, STOP opens the 3 s hold-to-confirm: a
//                1 s press sends nothing, a 3.2 s hold reaches the confirm
//                step and its confirm sends exactly one kill;
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

/** With the Mode sheet open, STOP opens Emergency stop's 3 s hold: released
 *  after 1 s nothing is sent and the confirm step does not show; held for
 *  3.2 s the confirm step shows, and its confirm sends exactly one kill. */
async function stopHoldCase(browser) {
  const sim = { pilot: PILOT_AIR, airborne: true };
  const { ctx, page } = await pilotPage(browser, { width: 1280, height: 800 }, sim);
  const kills = () => sentCount(sim, "kill", "killswitch");
  await openModeSheet(page);
  await page.locator(STOP).click();
  const sheet = page.locator(SHEET, { hasText: "Emergency stop" });
  await sheet.waitFor({ state: "visible", timeout: 2000 });
  await sheetSettled(page);
  const hold = sheet.getByRole("button", { name: /^Hold to confirm \(3\.0 s\)/ });
  const final = page.getByTestId("confirm-final");

  await holdOn(page, hold, 1000);
  await page.waitForTimeout(600);
  assert.equal(await final.count(), 0, "a 1 s press does not reach the confirm step");
  assert.equal(kills(), 0, `a 1 s press sends nothing (sent ${JSON.stringify(sim.sent)}, commands ${JSON.stringify(sim.commands)})`);

  await debugOnFail(page, "stop-hold", async () => {
    await holdOn(page, hold, 3200);
    await final.waitFor({ state: "visible", timeout: 1000 });
  });
  await page.screenshot({ path: path.join(SHOTS, "stop-hold-confirm.png") });
  assert.equal(kills(), 0, "the completed hold asks for the confirm before sending");
  await final.click();
  await until(page, "the kill", () => kills() >= 1, 3000);
  await page.waitForTimeout(800);
  assert.equal(kills(), 1, `exactly one kill (sent ${JSON.stringify(sim.sent)}, commands ${JSON.stringify(sim.commands)})`);
  await ctx.close();
  console.log("STOP: a 1 s press sends nothing; the full 3 s hold and its confirm send exactly one kill");
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

runCases({
  stop: stopCases,
  "stop-sheets": stopSheetsCase,
  "stop-hold": stopHoldCase,
  "pad-safety": padSafetyCase,
});
