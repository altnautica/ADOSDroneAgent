// Screenshots of the aircraft setup wizard (the shared web-shared renderer)
// on the dashboard and on the cockpit kiosk, at the accel, outputs and
// autotune steps, with `/api/v1/fc-setup` mocked by realistic step frames.
//
//   NODE_PATH=<dir containing playwright-core> CHROME=<chromium path> \
//     DASH=http://127.0.0.1:4274/ COCKPIT=http://127.0.0.1:4273/cockpit/ \
//     SHOTS=<dir> node e2e/setup-wizard.e2e.cjs

const { chromium } = require("playwright-core");
const fs = require("node:fs");
const path = require("node:path");

const DASH = process.env.DASH ?? "http://127.0.0.1:4274/";
const COCKPIT = process.env.COCKPIT ?? "http://127.0.0.1:4273/cockpit/";
const SHOTS = process.env.SHOTS ?? path.join(process.cwd(), "e2e-shots");
fs.mkdirSync(SHOTS, { recursive: true });

const GROUND = ["detect", "frame", "accel", "level", "compass", "radio", "outputs", "battery", "initial_params", "ados_profile"];
const AIR = ["first_hover", "notch_air", "autotune", "verify"];

function step(wizard, id, state, over = {}) {
  return { wizard, step: id, title_key: `setup.${id}.title`, body_key: `setup.${id}.body`, args: {}, image: null, state, progress: null, live: {}, actions: [], blockers: [], result: null, ...over };
}

function status(focus) {
  const ground = GROUND.map((id) => {
    const at = GROUND.indexOf(id);
    const cur = GROUND.indexOf(focus.step);
    if (focus.wizard !== "ground") return step("ground", id, "done");
    if (id === focus.step) return step("ground", id, focus.state, focus.over);
    return step("ground", id, at < cur ? "done" : "pending");
  });
  const air = AIR.map((id) => {
    if (focus.wizard !== "air") return step("air", id, "pending");
    const at = AIR.indexOf(id);
    const cur = AIR.indexOf(focus.step);
    if (id === focus.step) return step("air", id, focus.state, focus.over);
    return step("air", id, at < cur ? "done" : "pending");
  });
  return {
    v: 1,
    uid: "3a0021000f51373037363437",
    firmware: "ArduCopter",
    firmware_version: "4.5.7",
    vehicle_class: "multirotor",
    running: { wizard: focus.wizard, step: focus.step },
    wizards: [
      { id: "ground", state: focus.wizard === "ground" ? "running" : "done", available: true, blockers: [], completed_at: focus.wizard === "ground" ? null : "2026-11-07T08:12:00Z", steps: ground },
      { id: "air", state: focus.wizard === "air" ? "running" : "not_started", available: focus.wizard === "air", blockers: [], completed_at: null, steps: air },
    ],
    report: null,
  };
}

const CASES = {
  accel: {
    wizard: "ground",
    step: "accel",
    state: "waiting_user",
    over: {
      image: "accel_left",
      args: { pose: "left", n: 3 },
      progress: 2 / 6,
      live: { pose: "left" },
      actions: [
        { id: "next_pose", label_key: "setup.action.next_pose", style: "primary", confirm: "none" },
        { id: "abort", label_key: "setup.action.abort", style: "secondary", confirm: "none" },
      ],
    },
  },
  outputs: {
    wizard: "ground",
    step: "outputs",
    state: "waiting_user",
    over: {
      image: "motor_order_quad_x",
      progress: 1 / 4,
      args: { n: 3 }, live: { output_kind: "motor", output_index: 2, question_key: "setup.question.motor_position" },
      actions: [
        { id: "spin", label_key: "setup.action.start", style: "primary", confirm: "hold_props_off" },
        { id: "answer_yes", label_key: "setup.action.yes", style: "primary", confirm: "none" },
        { id: "answer_no", label_key: "setup.action.no", style: "secondary", confirm: "none" },
      ],
    },
  },
  autotune: {
    wizard: "air",
    step: "autotune",
    state: "running",
    over: {
      progress: 0.42,
      live: { autotune_axis: "pitch", autotune_status: "Twitch 14", altitude_m: 8.1, battery_pct: 71, distance_from_start_m: 3.2 },
      actions: [{ id: "land", label_key: "setup.action.land", style: "danger", confirm: "hold" }],
    },
  },
};

async function mock(page, focus, profile) {
  await page.route("**/api/**", (route) => {
    const p = new URL(route.request().url()).pathname;
    const json = (b, s = 200) => route.fulfill({ status: s, contentType: "application/json", body: JSON.stringify(b) });
    if (p === "/api/v1/fc-setup") return json(status(focus));
    if (p === "/api/pairing/info") return json({ profile, name: "bench-drone", paired: true });
    if (p === "/api/config") return json({});
    return json({ detail: "not mocked" }, 404);
  });
}

(async () => {
  const browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME });
  try {
    for (const [name, focus] of Object.entries(CASES)) {
      const d = await browser.newPage({ viewport: { width: 1280, height: 800 } });
      await mock(d, focus, "drone");
      await d.goto(`${DASH}settings/aircraft-setup`);
      await d.waitForTimeout(1500);
      await d.screenshot({ path: path.join(SHOTS, `wizard-dashboard-${name}.png`) });
      await d.close();

      const k = await browser.newPage({ viewport: { width: 1920, height: 1080 } });
      await mock(k, focus, "drone");
      await k.goto(`${COCKPIT}?kiosk=1`);
      await k.waitForTimeout(1200);
      await k.getByRole("button", { name: "Settings" }).first().click();
      await k.getByText("Aircraft setup", { exact: true }).first().click();
      await k.waitForTimeout(1500);
      await k.screenshot({ path: path.join(SHOTS, `wizard-kiosk-${name}.png`) });
      await k.close();
      console.log(`shot ${name}`);
    }
  } finally {
    await browser.close();
  }
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
