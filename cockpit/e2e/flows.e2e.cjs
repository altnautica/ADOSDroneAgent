// Flight-screen flows against a built bundle (see harness.cjs for how to
// run). Cases: the flow overlays clearing on a stalled pilot (`flow-stale`),
// a running launch's step and Abort (`sequence`), the Mode sheet's pending
// pick (`mode-pending`), a tap on a hold control reading "Keep holding"
// (`hold-hint`), Escape / a press outside / the controller's back closing
// the topmost sheet (`overlay-close`), the mission stall overlay from the
// golden stall frame with its actions and the left column's Return home
// reaching the pilot's `rth` (`mission-stall`), and the confined-place
// profile from the golden confined frame: the Confined badge, the cap, the
// route-check report, the stall overlay hiding its own banner and the Launch
// sheet's place picker (`confined`).

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const { GROUND_SHEETS, PILOT_AIR, PILOT_GROUND, SHEET, SHOTS, debugOnFail, holdOn, pilotPage, runCases, sheetSettled } = require("./harness.cjs");

const STALL_FRAME = JSON.parse(
  fs.readFileSync(path.join(__dirname, "../../crates/ados-control/tests/fixtures/pilot-frame-stall.json"), "utf8"),
);
const CONFINED_FRAME = JSON.parse(
  fs.readFileSync(path.join(__dirname, "../../crates/ados-control/tests/fixtures/pilot-frame-confined.json"), "utf8"),
);

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
  assert.match(await line.innerText(), /Taking off · Climb/);
  assert.match(await page.getByTestId("flight-state-chip").innerText(), /Taking off · Climb/);
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

/** The golden stall frame shows its persistent overlay next to the
 *  countdowns; Skip waypoint, Return and Land here send the pilot's
 *  `mission_skip`, `rth` and `land`; the left column's Return home (held)
 *  sends the pilot's `rth`, never a MAVLink RTL. */
async function missionStallCase(browser) {
  // The banner's slot is the frame's own past time; keep it on screen.
  const sim = { pilot: { ...STALL_FRAME, banner: { ...STALL_FRAME.banner, until_ms: null } }, airborne: true };
  const { ctx, page } = await pilotPage(browser, { width: 1280, height: 800 }, sim);
  const overlay = page.getByTestId("mission-stall-banner");
  await debugOnFail(page, "mission-stall", async () => {
    await overlay.waitFor({ state: "visible", timeout: 5000 });
  });
  assert.match(await overlay.innerText(), /No progress to waypoint 3 for 15 s · Skipping in 33 s/);
  assert.equal(await overlay.getAttribute("data-severity"), "critical");
  await page.screenshot({ path: path.join(SHOTS, "mission-stall.png") });
  const actions = () => sim.sent.filter((f) => f.op === "action").map((f) => f.name);
  for (const name of ["mission_skip", "rth", "land"]) {
    await page.getByTestId(`mission-stall-${name}`).click();
    const deadline = Date.now() + 2000;
    while (!actions().includes(name) && Date.now() < deadline) await page.waitForTimeout(50);
  }
  assert.deepEqual(actions(), ["mission_skip", "rth", "land"]);

  // A blocked return: Land here only.
  sim.pilot = { ...sim.pilot, mission_stall: { kind: "return", seq: null, wp_label: null, since_ms: 1, skip_in_s: 40.2, window_s: 20 } };
  await page.getByTestId("mission-stall-banner").filter({ hasText: "Return blocked · Landing in 41 s" }).waitFor({ timeout: 3000 });
  assert.equal(await page.getByTestId("mission-stall-mission_skip").count(), 0);
  assert.equal(await page.getByTestId("mission-stall-rth").count(), 0);
  await page.screenshot({ path: path.join(SHOTS, "return-blocked.png") });

  // The left column's Return home in ADOS control: the pilot's rth.
  sim.pilot = { ...sim.pilot, mission_stall: null };
  await page.getByTestId("mission-stall-banner").waitFor({ state: "detached", timeout: 3000 });
  const before = actions().filter((n) => n === "rth").length;
  await page.getByTestId("left-rth").click();
  await sheetSettled(page);
  await holdOn(page, page.locator(SHEET).getByRole("button", { name: /Hold to confirm/ }), 1900);
  const deadline = Date.now() + 2000;
  while (actions().filter((n) => n === "rth").length === before && Date.now() < deadline) await page.waitForTimeout(50);
  assert.equal(actions().filter((n) => n === "rth").length, before + 1, `Return home sends the pilot's rth (sent ${JSON.stringify(sim.sent)})`);
  assert.deepEqual(sim.commands, [], "no /api/command RTL beside it");
  await ctx.close();
  console.log("mission stall: overlay from the golden frame, Skip/Return/Land send mission_skip/rth/land; Return home sends the pilot's rth");
}

/** The golden confined frame in the air: the Confined badge beside the chip,
 *  the confined-near cap on its line, the too-tight route's report with its
 *  pinch, and the route banner. The golden stall frame: the overlay hides
 *  the stall's own banner. On the ground, the Launch sheet's place picker
 *  explains each class with the margin it flies and shows the route check. */
async function confinedCase(browser) {
  const config = (page) =>
    page.route("**/api/config", (route) =>
      route.fulfill({ contentType: "application/json", body: JSON.stringify({ agent: { name: "bench-drone" }, pilot: { obstacle_margin_m: 0.3, prop_tip_radius_m: 0.377 } }) }),
    );
  // The golden banner slots are the frames' own past times; keep them on screen.
  const air = { ...CONFINED_FRAME, banner: { ...CONFINED_FRAME.banner, until_ms: null } };
  const sim = { pilot: air, airborne: true };
  const { ctx, page } = await pilotPage(browser, { width: 1280, height: 800, beforeGoto: config }, sim);
  const card = page.getByTestId("route-check-card");
  await debugOnFail(page, "confined", () => card.waitFor({ state: "visible", timeout: 5000 }));
  assert.equal(await page.getByTestId("confined-chip").innerText(), "Confined");
  assert.match(await page.getByTestId("flight-state-chip").innerText(), /Near the clearance limit: 0\.5 m\/s max/);
  assert.match(await card.innerText(), /Route too tight: leg 3→4 has 0\.52 m, needs 0\.58 m/);
  assert.match(await card.innerText(), /At N 41\.3 m, E -17\.5 m, height 2\.0 m/);
  assert.match(await page.getByTestId("handover-banner").innerText(), /Route too tight at leg 3→4: 0\.52 m of 0\.58 m/);
  await page.screenshot({ path: path.join(SHOTS, "confined-air.png") });

  sim.pilot = { ...STALL_FRAME, banner: { ...STALL_FRAME.banner, until_ms: null } };
  await card.waitFor({ state: "detached", timeout: 3000 });
  await page.getByTestId("mission-stall-banner").waitFor({ state: "visible", timeout: 3000 });
  assert.equal(await page.getByTestId("handover-banner").count(), 0, "the stall's own banner hides behind the overlay");
  await page.screenshot({ path: path.join(SHOTS, "stall-banner-hidden.png") });

  sim.airborne = false;
  sim.pilot = { ...PILOT_GROUND, mission: { state: "loaded", seq: 0, total: 6 } };
  await page.getByTestId("mission-stall-banner").waitFor({ state: "detached", timeout: 3000 });
  await page.getByTestId("left-takeoff").click();
  await sheetSettled(page);
  const explain = page.getByTestId("launch-place-explain");
  assert.equal(await page.locator('[data-place="open"]').getAttribute("aria-checked"), "true");
  assert.equal(await explain.innerText(), "Open sites: 0.5 m from obstacles.");
  await page.locator('[data-place="confined"]').click();
  assert.match(await explain.innerText(), /^Mines, tunnels and rooms: 0\.48 m from walls/);
  await page.screenshot({ path: path.join(SHOTS, "launch-place.png") });
  sim.pilot = { ...sim.pilot, place_class: "confined", route_check: CONFINED_FRAME.route_check };
  await page.locator(SHEET).getByTestId("route-check-card").waitFor({ state: "visible", timeout: 3000 });
  await page.screenshot({ path: path.join(SHOTS, "launch-route-check.png") });
  await ctx.close();
  console.log("confined: badge, cap, route-check report and route banner; stall hides its banner; launch place picker and route check");
}

runCases({
  "mission-stall": missionStallCase,
  confined: confinedCase,
  "flow-stale": flowStaleCase,
  sequence: sequenceCase,
  "mode-pending": modePendingCase,
  "hold-hint": holdHintCase,
  "overlay-close": overlayCloseCase,
});
