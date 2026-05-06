// Network section. WiFi SSID + password + hotspot toggle. The full
// uplink matrix lands in a later iteration; this slice covers the
// fields the apply route ships in v0.13.1.

import { el } from "../../components.js";
import { createDirtyTracker } from "./_dirty.js";

function readNetwork(status) {
  const network = status.network || {};
  return {
    wifi_ssid: network.wifi_ssid || "",
    wifi_password: "",
    hotspot_enabled: !!network.hotspot_enabled,
  };
}

export function renderNetworkSection({ store, onChange }) {
  const status = store.get().status || {};
  const initial = readNetwork(status);
  const tracker = createDirtyTracker(initial, () => {
    notify();
    repaint();
  });
  const notify = typeof onChange === "function" ? onChange : () => {};

  const root = el("div", { className: "section-body" });

  const repaint = () => {
    const ssidInput = el("input", {
      type: "text",
      className: "input",
      value: tracker.read("wifi_ssid") || "",
      placeholder: "wifi ssid",
      autocomplete: "off",
      oninput: (ev) => tracker.set("wifi_ssid", ev.currentTarget.value),
    });

    const pwdInput = el("input", {
      type: "password",
      className: "input",
      value: tracker.read("wifi_password") || "",
      placeholder: "wifi password",
      autocomplete: "new-password",
      oninput: (ev) => tracker.set("wifi_password", ev.currentTarget.value),
    });

    const apToggle = el("label", { className: "toggle-row" },
      el("input", {
        type: "checkbox",
        checked: !!tracker.read("hotspot_enabled"),
        onchange: (ev) => tracker.set("hotspot_enabled", !!ev.currentTarget.checked),
      }),
      el("span", { className: "toggle-label", text: "hotspot enabled" }),
    );

    root.replaceChildren(
      el("p", { className: "section-hint text-faint", text: "wifi client + ap" }),
      el("label", { className: "field" },
        el("span", { className: "field-label", text: "ssid" }),
        ssidInput,
      ),
      el("label", { className: "field" },
        el("span", { className: "field-label", text: "password" }),
        pwdInput,
      ),
      apToggle,
    );
  };

  repaint();

  return {
    node: root,
    tracker,
    payload: () => {
      const diff = tracker.payload();
      if (!Object.keys(diff).length) return null;
      const out = {};
      if ("wifi_ssid" in diff) out.wifi_ssid = diff.wifi_ssid;
      if ("wifi_password" in diff) out.wifi_password = diff.wifi_password;
      if ("hotspot_enabled" in diff) out.hotspot_enabled = !!diff.hotspot_enabled;
      return out;
    },
    reset: () => {
      tracker.reset();
      repaint();
      notify();
    },
  };
}
