// ADOS Ground Station setup webapp (MSN-025 Wave A shells).
// Vanilla JS. No framework. No build step.
// Wave D wires these stubs to live REST endpoints on the agent.

const MOCK = {
  station: {
    shortId: "ADOS-GS-A17C",
    paired: false,
    pairedDroneId: null,
  },
  ap: {
    ssid: "ADOS-GS-A17C",
    passphrase: "ados-setup-1234",
    revealed: false,
  },
  wifiClient: {
    joined: false,
    ssid: null,
  },
  modem: {
    present: false,
    apn: null,
    signal: null,
  },
  display: {
    resolution: "auto",
    gamepad: null,
  },
  wfb: {
    channel: 165,
    profile: "balanced",
  },
};

function $(id) {
  return document.getElementById(id);
}

function goBack() {
  if (document.referrer && document.referrer.indexOf(location.host) !== -1) {
    history.back();
  } else {
    location.href = "index.html";
  }
}

function stub(msg) {
  alert(msg + "\n\nWires up in Wave D.");
}

function initIndex() {
  const s = $("station-id");
  if (s) s.textContent = MOCK.station.shortId;
  const status = $("pair-status");
  if (status) {
    if (MOCK.station.paired) {
      status.textContent = "Paired: " + MOCK.station.pairedDroneId;
      status.classList.add("ok");
    } else {
      status.textContent = "Status: Not yet paired";
    }
  }
}

function initPair() {
  const form = $("pair-form");
  if (!form) return;
  form.addEventListener("submit", function (ev) {
    ev.preventDefault();
    const keyInput = $("pair-key");
    const val = (keyInput && keyInput.value || "").trim();
    const ok = $("pair-ok");
    const fail = $("pair-fail");
    ok.classList.add("hidden");
    fail.classList.add("hidden");
    if (val.length < 8) {
      fail.textContent = "Pairing key looks too short. Check and retry.";
      fail.classList.remove("hidden");
      return;
    }
    alert("Pair flow wires up in Wave D.");
  });
}

function initNetwork() {
  const apSsid = $("ap-ssid");
  if (apSsid) apSsid.textContent = MOCK.ap.ssid;

  const reveal = $("ap-reveal");
  const passEl = $("ap-pass");
  if (reveal && passEl) {
    reveal.addEventListener("click", function () {
      if (MOCK.ap.revealed) {
        passEl.textContent = "(tap to reveal)";
        MOCK.ap.revealed = false;
      } else {
        passEl.textContent = MOCK.ap.passphrase;
        MOCK.ap.revealed = true;
      }
    });
  }

  const apChange = $("ap-change");
  if (apChange) apChange.addEventListener("click", function () { stub("Access point config"); });

  const wifiState = $("wifi-state");
  if (wifiState) {
    wifiState.textContent = MOCK.wifiClient.joined ? "Joined: " + MOCK.wifiClient.ssid : "Not joined";
  }
  const wifiScan = $("wifi-scan");
  if (wifiScan) wifiScan.addEventListener("click", function () { stub("WiFi scan"); });

  const modemState = $("modem-state");
  if (modemState) {
    modemState.textContent = MOCK.modem.present ? "Modem ready" : "No modem detected";
  }
}

function initDisplay() {
  const res = $("hdmi-res");
  if (res) res.value = MOCK.display.resolution;
  const btBtn = $("bt-pair");
  if (btBtn) btBtn.addEventListener("click", function () { stub("Bluetooth pairing"); });
  // All controls are disabled for Wave A per spec.
}

function initWfb() {
  const ch = $("wfb-channel");
  if (ch) ch.textContent = String(MOCK.wfb.channel);
  const prof = $("wfb-profile");
  if (prof) prof.textContent = MOCK.wfb.profile;
}

function initAdvanced() {
  const reset = $("factory-reset");
  if (!reset) return;
  reset.addEventListener("click", function () {
    const ok = confirm("Factory reset will erase all ground-station settings. Continue?");
    if (ok) {
      alert("Factory reset wires up in Wave D.");
    }
  });
}

document.addEventListener("DOMContentLoaded", function () {
  const backBtn = document.querySelector("header .back");
  if (backBtn) backBtn.addEventListener("click", goBack);

  const page = document.body.dataset.page;
  if (page === "index") initIndex();
  else if (page === "pair") initPair();
  else if (page === "network") initNetwork();
  else if (page === "display") initDisplay();
  else if (page === "wfb") initWfb();
  else if (page === "advanced") initAdvanced();
});
