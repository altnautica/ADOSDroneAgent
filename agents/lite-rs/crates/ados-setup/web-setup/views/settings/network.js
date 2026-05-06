// Network section placeholder. WiFi client + AP + Ethernet + USB tether
// + 4G modem tabs land in a later iteration.

import { el } from "../../components.js";

const TABS = ["wifi client", "ap", "ethernet", "usb tether", "4g"];

export function renderNetworkSection() {
  const tabs = el("div", { className: "tab-strip" });
  TABS.forEach((label, i) => {
    tabs.appendChild(el("button", {
      type: "button",
      className: `tab ${i === 0 ? "is-active" : ""}`.trim(),
      text: label,
      disabled: true,
    }));
  });

  return el("div", { className: "section-body" },
    el("p", { className: "section-hint text-faint", text: "uplink tabs" }),
    tabs,
    el("p", { className: "section-note text-faint", text: "tab content + scan + apply lands in a later iteration." }),
  );
}
