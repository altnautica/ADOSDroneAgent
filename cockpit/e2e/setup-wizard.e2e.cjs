// Screenshots of the aircraft setup wizard (the shared web-shared renderer)
// on the dashboard and on the cockpit kiosk, with `/api/v1/fc-setup` mocked by
// realistic step frames: the overview after an agent restart (interrupted
// banner, component cards), then the frame picture picker (reached through
// Continue setup, which resumes the interrupted run), and the accel, outputs
// and autotune steps (reached through Continue setup on a running wizard).
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

const GROUND = ["detect", "frame", "accel", "level", "compass", "radio", "outputs", "battery", "initial_params", "ados_profile", "ados_intro"];
const AIR = ["preflight", "first_hover", "notch_air", "autotune", "verify"];

function step(wizard, id, state, over = {}) {
  return { wizard, step: id, title_key: `setup.${id}.title`, body_key: `setup.${id}.body`, args: {}, image: null, state, progress: null, live: {}, actions: [], blockers: [], result: null, ...over };
}

const FRAME_INPUTS = {
  frame_class: {
    kind: "enum", value: 1, options: [1, 2, 3], label_key: "setup.input.frame_class",
    images: { 1: "motor_order_quad_x", 2: "motor_order_hexa_x", 3: "motor_order_octa_x" },
  },
  frame_type: {
    kind: "enum", value: 1, options: [0, 1, 3], label_key: "setup.input.frame_type",
    images: { 0: "motor_order_quad_plus", 1: "motor_order_quad_x", 3: "motor_order_quad_h" },
  },
  prop_in: { kind: "number", value: 10, min: 2, max: 40, step: 0.5, unit: "in", label_key: "setup.input.prop_in" },
  motor_kv: { kind: "number", value: 900, min: 50, max: 5000, step: 10, unit: "rpm/V", label_key: "setup.input.motor_kv" },
  cells: { kind: "number", value: 4, min: 1, max: 14, step: 1, unit: "S", label_key: "setup.input.cells" },
};

/** The ground and air wizards with `focus.step` in `focus.state` and the
 *  wizard of `focus` in `focus.wizardState` (default running). */
function wizards(focus) {
  const steps = (wizard, ids) =>
    ids.map((id) => {
      if (focus.wizard !== wizard) return step(wizard, id, wizard === "ground" ? "done" : "pending");
      const at = ids.indexOf(id);
      const cur = ids.indexOf(focus.step);
      if (id === focus.step) return step(wizard, id, focus.state, focus.over);
      return step(wizard, id, at < cur ? "done" : "pending", at < cur ? { result: RESULTS[id] ?? null } : {});
    });
  const state = (wizard) => (focus.wizard === wizard ? (focus.wizardState ?? "running") : wizard === "ground" ? "done" : "not_started");
  return [
    { id: "ground", state: state("ground"), available: true, blockers: [], completed_at: focus.wizard === "ground" ? null : "2026-11-07T08:12:00Z", steps: steps("ground", GROUND) },
    { id: "air", state: state("air"), available: focus.wizard === "air", blockers: [], completed_at: null, steps: steps("air", AIR) },
  ];
}

const RESULTS = {
  frame: { frame_class: 1, frame_type: 1, prop_in: 10, motor_kv: 900, cells: 4 },
  accel: { poses: 6 },
  battery: { capacity_mah: 5000 },
};

function status(focus) {
  const running = (focus.wizardState ?? "running") === "running" ? { wizard: focus.wizard, step: focus.step } : null;
  return {
    v: 1,
    uid: "3a0021000f51373037363437",
    firmware: "ArduCopter",
    firmware_version: "4.5.7",
    vehicle_class: "multirotor",
    running,
    wizards: wizards(focus),
    report: null,
  };
}

const FRAME = {
  wizard: "ground",
  step: "frame",
  state: "waiting_user",
  over: {
    image: "motor_order_quad_x",
    args: { inputs: FRAME_INPUTS },
    actions: [{ id: "apply", label_key: "setup.action.apply", style: "primary", confirm: "none" }],
  },
};

// `open`: how the shot reaches its screen. "overview" stays on the cards;
// "continue" presses Continue setup (a resume for an interrupted wizard).
const CASES = {
  overview: { ...FRAME, wizardState: "interrupted", open: "overview" },
  frame: { ...FRAME, wizardState: "interrupted", open: "continue" },
  accel: {
    wizard: "ground",
    step: "accel",
    state: "waiting_user",
    open: "continue",
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
    open: "continue",
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
  battery: {
    wizard: "ground",
    step: "battery",
    state: "waiting_user",
    open: "continue",
    over: {
      args: {
        inputs: {
          capacity_mah: { kind: "number", value: 5000, min: 100, max: 100000, step: 50, unit: "mAh", label_key: "setup.input.capacity_mah" },
          measured_v: { kind: "number", value: 16.4, min: 0, max: 70, step: 0.01, unit: "V", label_key: "setup.input.measured_v" },
        },
      },
      live: { battery_v: 16.38 },
      actions: [{ id: "apply", label_key: "setup.action.apply", style: "primary", confirm: "none" }],
    },
  },
  ados_profile: {
    wizard: "ground",
    step: "ados_profile",
    state: "waiting_user",
    open: "continue",
    over: {
      args: {
        params: [
          { name: "FS_GCS_ENABLE", value: 1 }, { name: "FS_GCS_TIMEOUT", value: 5 }, { name: "MAV_GCS_SYSID", value: 1 },
          { name: "GUID_TIMEOUT", value: 3 }, { name: "AVOID_ENABLE", value: 2 }, { name: "AVOID_MARGIN", value: 1.3 },
          { name: "PRX1_TYPE", value: 2 }, { name: "OA_TYPE", value: 1 }, { name: "BATT_FS_LOW_ACT", value: 2 },
        ],
      },
      actions: [{ id: "apply", label_key: "setup.action.apply", style: "primary", confirm: "none" }],
    },
  },
  autotune: {
    wizard: "air",
    step: "autotune",
    state: "running",
    open: "continue",
    over: {
      progress: 0.42,
      live: { autotune_axis: "pitch", autotune_status: "Twitch 14", altitude_m: 8.1, battery_pct: 71, distance_from_start_m: 3.2 },
      actions: [{ id: "land", label_key: "setup.action.land", style: "danger", confirm: "hold" }],
    },
  },
};

async function mock(page, focus, profile) {
  // Only the agent's own routes: a Vite dev server also serves source modules
  // under `/src/shared/api/`, which must load as usual.
  await page.route((url) => url.pathname.startsWith("/api/"), (route) => {
    const p = new URL(route.request().url()).pathname;
    const json = (b, s = 200) => route.fulfill({ status: s, contentType: "application/json", body: JSON.stringify(b) });
    if (p === "/api/v1/fc-setup") return json(status(focus));
    // Resuming an interrupted run: the engine answers with the wizard running again.
    const resume = /^\/api\/v1\/fc-setup\/(\w+)\/resume$/.exec(p);
    if (resume && route.request().method() === "POST") {
      const wizard = wizards({ ...focus, wizardState: "running" }).find((w) => w.id === resume[1]);
      return json({ ok: true, wizard });
    }
    if (p === "/api/pairing/info") return json({ profile, name: "bench-drone", paired: true });
    if (p === "/api/config") return json({});
    return json({ detail: "not mocked" }, 404);
  });
  // The setup stream, alive: keepalives at 1 Hz, as the agent sends them.
  await page.routeWebSocket(/\/api\/v1\/fc-setup\/stream$/, (ws) => {
    const timer = setInterval(() => ws.send(JSON.stringify({ kind: "keepalive", ts_ms: Date.now() })), 1000);
    // A closed page never closes its routed socket: never hold the process open.
    timer.unref();
    ws.onClose(() => clearInterval(timer));
  });
}

/** From the overview, reach the case's screen. */
async function open(page, focus) {
  await page.getByTestId("setup-overview").waitFor({ timeout: 10_000 });
  if (focus.open !== "continue") return;
  await page.getByRole("button", { name: "Continue setup" }).first().click();
  await page.getByTestId("setup-guided").waitFor({ timeout: 10_000 });
}

/** The step's primary action, when it has one, is on screen and not covered
 *  (by the kiosk's bottom bar or the end of the scrolling screen): the points
 *  just inside its top and bottom edges hit the button itself. */
async function primaryInView(page, name) {
  const actions = page.getByTestId("setup-actions");
  if ((await actions.count()) === 0) return;
  const visible = await actions
    .locator("button")
    .first()
    .evaluate((b) => {
      const r = b.getBoundingClientRect();
      const x = r.left + r.width / 2;
      return [r.top + 3, r.bottom - 3].every((y) => {
        const hit = document.elementFromPoint(x, y);
        return hit !== null && b.contains(hit);
      });
    });
  if (!visible) {
    const vp = page.viewportSize();
    throw new Error(`${name} at ${vp.width}x${vp.height}: the primary action is not fully in view`);
  }
}

const KIOSK_SIZES = [
  { width: 1280, height: 800 },
  { width: 1920, height: 1080 },
];

(async () => {
  const browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME });
  const misses = [];
  const check = (page, name) => primaryInView(page, name).catch((e) => misses.push(e.message));
  try {
    for (const [name, focus] of Object.entries(CASES)) {
      const d = await browser.newPage({ viewport: { width: 1280, height: 800 } });
      await mock(d, focus, "drone");
      await d.goto(`${DASH}settings/aircraft-setup`);
      await open(d, focus);
      await d.waitForTimeout(800);
      await d.screenshot({ path: path.join(SHOTS, `wizard-dashboard-${name}.png`), fullPage: true });
      await d.close();

      for (const size of KIOSK_SIZES) {
        const k = await browser.newPage({ viewport: size });
        await mock(k, focus, "drone");
        await k.goto(`${COCKPIT}?kiosk=1`);
        await k.waitForTimeout(1200);
        // Settings sits in the rail, or behind Menu when the rail is compact or
        // hidden. Exact: "Camera settings" also names a button on the Feed.
        const settings = k.getByRole("button", { name: "Settings", exact: true }).first();
        if (!(await settings.isVisible())) await k.getByRole("button", { name: "Menu", exact: true }).click();
        await settings.click();
        await k.getByText("Aircraft setup", { exact: true }).first().click();
        await open(k, focus);
        await k.waitForTimeout(800);
        await check(k, `kiosk ${name}`);
        await k.screenshot({ path: path.join(SHOTS, `wizard-kiosk-${size.width}x${size.height}-${name}.png`) });
        await k.close();
      }
      console.log(`shot ${name}`);
    }
  } finally {
    await browser.close();
  }
  if (misses.length) {
    console.error(misses.join("\n"));
    process.exit(1);
  }
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
