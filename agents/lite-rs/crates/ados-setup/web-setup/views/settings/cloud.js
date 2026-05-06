// Cloud section. Mode radio + (when self-hosted) URL/MQTT/key inputs.
// Pairing code is read-only display; pairing is owned by the wizard.

import { el } from "../../components.js";
import { createDirtyTracker } from "./_dirty.js";

const MODES = [
  { value: "cloud", label: "altnautica cloud" },
  { value: "self_hosted", label: "self-hosted" },
  { value: "local", label: "local only" },
];

function readCloud(status) {
  const cloud = status.cloud_choice || {};
  return {
    mode: cloud.mode || "cloud",
    url: cloud.backend_url || "",
    mqtt_broker: cloud.mqtt_broker || "",
    mqtt_port: typeof cloud.mqtt_port === "number" ? cloud.mqtt_port : 8883,
    api_key: "",
  };
}

export function renderCloudSection({ store, onChange }) {
  const status = store.get().status || {};
  const initial = readCloud(status);
  const tracker = createDirtyTracker(initial, () => {
    notify();
    repaint();
  });
  const notify = typeof onChange === "function" ? onChange : () => {};

  const pair = status.pairing_code || status.pair || "-";

  const root = el("div", { className: "section-body" });

  const repaint = () => {
    const mode = tracker.read("mode");
    const isSelfHosted = mode === "self_hosted";

    const modeRadio = (value, label) => el("label", { className: "radio-row" },
      el("input", {
        type: "radio",
        name: "cloud-mode",
        value,
        checked: value === mode,
        onchange: (ev) => {
          if (ev.currentTarget.checked) tracker.set("mode", value);
        },
      }),
      el("span", { className: "radio-label", text: label }),
    );

    const urlInput = el("input", {
      type: "url",
      className: "input",
      value: tracker.read("url") || "",
      placeholder: "https://convex.example.com",
      disabled: !isSelfHosted,
      oninput: (ev) => tracker.set("url", ev.currentTarget.value),
    });

    const brokerInput = el("input", {
      type: "text",
      className: "input",
      value: tracker.read("mqtt_broker") || "",
      placeholder: "mqtt.example.com",
      disabled: !isSelfHosted,
      oninput: (ev) => tracker.set("mqtt_broker", ev.currentTarget.value),
    });

    const portInput = el("input", {
      type: "number",
      className: "input",
      value: tracker.read("mqtt_port"),
      min: 1,
      max: 65535,
      disabled: !isSelfHosted,
      oninput: (ev) => {
        const n = Number.parseInt(ev.currentTarget.value, 10);
        tracker.set("mqtt_port", Number.isFinite(n) ? n : 0);
      },
    });

    const apiKeyInput = el("input", {
      type: "password",
      className: "input",
      autocomplete: "new-password",
      value: tracker.read("api_key") || "",
      placeholder: "api key",
      disabled: !isSelfHosted,
      oninput: (ev) => tracker.set("api_key", ev.currentTarget.value),
    });

    root.replaceChildren(
      el("p", { className: "section-hint text-faint", text: "cloud relay" }),
      el("div", { className: "radio-group" },
        ...MODES.map((m) => modeRadio(m.value, m.label)),
      ),
      el("label", { className: "field" },
        el("span", { className: "field-label", text: "self-hosted url" }),
        urlInput,
      ),
      el("label", { className: "field" },
        el("span", { className: "field-label", text: "mqtt broker" }),
        brokerInput,
      ),
      el("label", { className: "field" },
        el("span", { className: "field-label", text: "mqtt port" }),
        portInput,
      ),
      el("label", { className: "field" },
        el("span", { className: "field-label", text: "api key" }),
        apiKeyInput,
      ),
      el("dl", { className: "kv" },
        el("dt", { text: "pairing" }),
        el("dd", { className: "mono", text: pair }),
      ),
    );
  };

  repaint();

  return {
    node: root,
    tracker,
    payload: () => {
      const diff = tracker.payload();
      if (!Object.keys(diff).length) return null;
      const mode = tracker.read("mode");
      const out = { mode };
      if (mode === "self_hosted") {
        out.self_hosted = {
          url: tracker.read("url") || "",
          mqtt_broker: tracker.read("mqtt_broker") || "",
          mqtt_port: tracker.read("mqtt_port") || 8883,
        };
        const key = tracker.read("api_key") || "";
        if (key) out.self_hosted.api_key = key;
      }
      return out;
    },
    reset: () => {
      tracker.reset();
      repaint();
      notify();
    },
  };
}
