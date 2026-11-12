// Flight-screen flows against a built bundle (see harness.cjs for how to
// run). Cases: the flow overlays clearing on a stalled pilot (`flow-stale`),
// a running launch's step and Abort (`sequence`), the Mode sheet's pending
// pick (`mode-pending`), a tap on a hold control reading "Keep holding"
// (`hold-hint`), and Escape / a press outside / the controller's back
// closing the topmost sheet (`overlay-close`).

const assert = require("node:assert/strict");
const path = require("node:path");

const { GROUND_SHEETS, PILOT_AIR, PILOT_GROUND, SHEET, SHOTS, debugOnFail, pilotPage, runCases, sheetSettled } = require("./harness.cjs");

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
  // The same line the chip's launching rule writes, word for word.
  assert.match(await line.innerText(), /Taking off · CLIMB/);
  assert.match(await page.getByTestId("flight-state-chip").innerText(), /Taking off · CLIMB/);
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

runCases({
  "flow-stale": flowStaleCase,
  sequence: sequenceCase,
  "mode-pending": modePendingCase,
  "hold-hint": holdHintCase,
  "overlay-close": overlayCloseCase,
});
