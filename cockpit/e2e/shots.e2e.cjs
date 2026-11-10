// Screenshots of every width class, the kiosk/TV, the Mode sheet, the skill
// picker and the pre-flight checklist against a built bundle (see harness.cjs
// for how to run). A manual check for a reviewer's eye, not run in CI.

const assert = require("node:assert/strict");
const path = require("node:path");

const { BASE, PAD_INIT, SHOTS, VIEWPORTS, mockAgent, runCases } = require("./harness.cjs");

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
    // Unref'd: a closed page may never report the socket closed, and the
    // timer must not keep the run alive.
    const push = setInterval(() => ws.send(JSON.stringify({ kind: "pilot", ts_ms: Date.now(), data: state })), 100).unref();
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

runCases({ shots });
