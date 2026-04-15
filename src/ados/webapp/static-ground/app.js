// ADOS Ground Station setup webapp.
// Vanilla JS. No framework. No build step.
// Wave D: live REST wiring to /api/v1/ground-station/*.

var API_BASE = "/api/v1/ground-station";
var REFRESH_MS = 5000;
var refreshTimer = null;

function $(id) { return document.getElementById(id); }

function goBack() {
  if (document.referrer && document.referrer.indexOf(location.host) !== -1) {
    history.back();
  } else {
    location.href = "index.html";
  }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

async function api(method, path, body) {
  showSpinner();
  try {
    var opts = { method: method, headers: {} };
    if (body !== undefined && body !== null) {
      opts.headers["Content-Type"] = "application/json";
      opts.body = JSON.stringify(body);
    }
    var res = await fetch(API_BASE + path, opts);
    var text = await res.text();
    var data = null;
    if (text) {
      try { data = JSON.parse(text); } catch (e) { data = { raw: text }; }
    }
    if (!res.ok) {
      var code = "E_HTTP_" + res.status;
      var msg = "Request failed.";
      if (data && data.detail && data.detail.error) {
        code = data.detail.error.code || code;
        msg = data.detail.error.message || msg;
      } else if (data && data.error) {
        code = data.error.code || code;
        msg = data.error.message || msg;
      }
      var err = new Error(msg);
      err.status = res.status;
      err.code = code;
      err.data = data;
      throw err;
    }
    return data;
  } finally {
    hideSpinner();
  }
}

function toast(message, kind) {
  var host = $("toast-host");
  if (!host) {
    host = document.createElement("div");
    host.id = "toast-host";
    host.className = "toast-host";
    document.body.appendChild(host);
  }
  var el = document.createElement("div");
  el.className = "toast " + (kind === "error" ? "toast-error" : "toast-ok");
  el.textContent = message;
  host.appendChild(el);
  setTimeout(function () {
    el.classList.add("fade");
    setTimeout(function () { if (el.parentNode) el.parentNode.removeChild(el); }, 300);
  }, 2600);
}

var _spinnerCount = 0;
function showSpinner() {
  _spinnerCount += 1;
  var sp = $("spinner");
  if (sp) sp.classList.remove("hidden");
}
function hideSpinner() {
  _spinnerCount = Math.max(0, _spinnerCount - 1);
  if (_spinnerCount === 0) {
    var sp = $("spinner");
    if (sp) sp.classList.add("hidden");
  }
}

function reportAuthError(err) {
  if (err && (err.status === 401 || err.status === 403)) {
    var b = $("auth-banner");
    if (b) {
      b.textContent = "Agent refused the request (HTTP " + err.status + "). Check that the ground-station profile allows captive access.";
      b.classList.remove("hidden");
    }
  }
}

function deriveShortId(apSsid) {
  if (!apSsid) return "ADOS-GS-...";
  return apSsid;
}

// ---------------------------------------------------------------------------
// index.html
// ---------------------------------------------------------------------------

async function refreshIndex() {
  try {
    var s = await api("GET", "/status");
    var apSsid = (s && s.network && s.network.ap_ssid) || null;
    var idEl = $("station-id");
    if (idEl) idEl.textContent = deriveShortId(apSsid);

    var droneId = s && s.paired_drone && s.paired_drone.device_id;
    var statusEl = $("pair-status");
    if (statusEl) {
      if (droneId) {
        statusEl.textContent = "Paired: " + droneId;
        statusEl.classList.add("ok");
      } else {
        statusEl.textContent = "Status: Not yet paired";
        statusEl.classList.remove("ok");
      }
    }
  } catch (err) {
    reportAuthError(err);
    var statusEl2 = $("pair-status");
    if (statusEl2) statusEl2.textContent = "Status: Offline (" + (err.code || "error") + ")";
  }
}

function initIndex() {
  refreshIndex();
  if (refreshTimer) clearInterval(refreshTimer);
  refreshTimer = setInterval(function () {
    if (document.visibilityState === "visible") refreshIndex();
  }, REFRESH_MS);
}

// ---------------------------------------------------------------------------
// pair.html
// ---------------------------------------------------------------------------

async function refreshPairState() {
  try {
    var s = await api("GET", "/status");
    var droneId = s && s.paired_drone && s.paired_drone.device_id;
    var already = $("pair-already");
    var form = $("pair-form");
    if (droneId && already && form) {
      var label = $("pair-already-drone");
      if (label) label.textContent = droneId;
      already.classList.remove("hidden");
      form.classList.add("hidden");
    } else if (already && form) {
      already.classList.add("hidden");
      form.classList.remove("hidden");
    }
  } catch (err) {
    reportAuthError(err);
  }
}

function initPair() {
  refreshPairState();

  var form = $("pair-form");
  if (form) {
    form.addEventListener("submit", async function (ev) {
      ev.preventDefault();
      var keyInput = $("pair-key");
      var val = (keyInput && keyInput.value || "").trim();
      var ok = $("pair-ok");
      var fail = $("pair-fail");
      ok.classList.add("hidden");
      fail.classList.add("hidden");
      if (val.length < 8) {
        fail.textContent = "Pair key looks too short.";
        fail.classList.remove("hidden");
        return;
      }
      try {
        var res = await api("POST", "/wfb/pair", { pair_key: val });
        var fp = (res && res.key_fingerprint) || "";
        ok.textContent = "Paired. Fingerprint: " + fp.slice(0, 16);
        ok.classList.remove("hidden");
        toast("Paired successfully.", "success");
        if (keyInput) keyInput.value = "";
        refreshPairState();
      } catch (err) {
        reportAuthError(err);
        if (err.status === 409) {
          refreshPairState();
          return;
        }
        if (err.status === 400) {
          fail.textContent = "Pair key invalid. Check and retry.";
        } else {
          fail.textContent = "Pair failed: " + (err.code || "error");
        }
        fail.classList.remove("hidden");
      }
    });
  }

  var unpairBtn = $("pair-unpair");
  if (unpairBtn) {
    unpairBtn.addEventListener("click", async function () {
      if (!confirm("Unpair from the current drone?")) return;
      try {
        await api("DELETE", "/wfb/pair");
        toast("Unpaired.", "success");
        refreshPairState();
      } catch (err) {
        reportAuthError(err);
        toast("Unpair failed: " + (err.code || "error"), "error");
      }
    });
  }
}

// ---------------------------------------------------------------------------
// network.html
// ---------------------------------------------------------------------------

async function initNetwork() {
  try {
    var net = await api("GET", "/network");
    var ap = net.ap || {};
    var apSsid = $("ap-ssid");
    if (apSsid) apSsid.textContent = ap.ssid || "(unknown)";
    var apChan = $("ap-channel");
    if (apChan) apChan.textContent = ap.channel != null ? String(ap.channel) : "-";

    var ssidInput = $("ap-ssid-input");
    if (ssidInput) ssidInput.value = ap.ssid || "";
    var chanInput = $("ap-channel-input");
    if (chanInput) chanInput.value = ap.channel != null ? String(ap.channel) : "";
  } catch (err) {
    reportAuthError(err);
    toast("Could not load network config.", "error");
  }

  var passEl = $("ap-pass");
  if (passEl) passEl.textContent = "(on the case label)";

  var changeBtn = $("ap-change");
  var changeForm = $("ap-change-form");
  if (changeBtn && changeForm) {
    changeBtn.addEventListener("click", function () {
      changeForm.classList.toggle("hidden");
    });
  }

  var saveBtn = $("ap-save");
  if (saveBtn) {
    saveBtn.addEventListener("click", async function () {
      var ssid = ($("ap-ssid-input").value || "").trim();
      var chanStr = ($("ap-channel-input").value || "").trim();
      var body = {};
      if (ssid) body.ssid = ssid;
      if (chanStr) {
        var n = parseInt(chanStr, 10);
        if (!isNaN(n)) body.channel = n;
      }
      if (Object.keys(body).length === 0) {
        toast("Nothing to save.", "error");
        return;
      }
      try {
        var res = await api("PUT", "/network/ap", body);
        toast("Access point updated.", "success");
        var apSsid2 = $("ap-ssid");
        if (apSsid2) apSsid2.textContent = res.ssid || ssid;
        var apChan2 = $("ap-channel");
        if (apChan2) apChan2.textContent = res.channel != null ? String(res.channel) : chanStr;
        if (changeForm) changeForm.classList.add("hidden");
      } catch (err) {
        reportAuthError(err);
        toast("Save failed: " + (err.code || "error"), "error");
      }
    });
  }
}

// ---------------------------------------------------------------------------
// display.html
// ---------------------------------------------------------------------------

async function initDisplay() {
  try {
    var ui = await api("GET", "/ui");
    var oled = (ui && ui.oled) || {};
    var slider = $("oled-brightness");
    var label = $("oled-brightness-label");
    var current = oled.brightness != null ? oled.brightness : 80;
    if (slider) {
      slider.value = String(current);
      slider.disabled = false;
    }
    if (label) label.textContent = String(current);

    if (slider) {
      slider.addEventListener("input", function () {
        if (label) label.textContent = slider.value;
      });
      slider.addEventListener("change", async function () {
        var n = parseInt(slider.value, 10);
        if (isNaN(n)) return;
        try {
          await api("PUT", "/ui/oled", { brightness: n });
          toast("Brightness saved.", "success");
        } catch (err) {
          reportAuthError(err);
          toast("Save failed: " + (err.code || "error"), "error");
        }
      });
    }
  } catch (err) {
    reportAuthError(err);
    toast("Could not load display config.", "error");
  }
}

// ---------------------------------------------------------------------------
// wfb.html
// ---------------------------------------------------------------------------

async function initWfb() {
  var chanEl = $("wfb-channel");
  var profEl = $("wfb-profile");

  async function load() {
    try {
      var w = await api("GET", "/wfb");
      if (chanEl) chanEl.textContent = String(w.channel != null ? w.channel : "-");
      if (profEl) profEl.textContent = w.bitrate_profile || "default";
      var chanSel = $("wfb-channel-input");
      if (chanSel && w.channel != null) chanSel.value = String(w.channel);
      var profSel = $("wfb-profile-input");
      if (profSel && w.bitrate_profile) profSel.value = w.bitrate_profile;
    } catch (err) {
      reportAuthError(err);
      toast("Could not load radio config.", "error");
    }
  }

  await load();

  var changeBtn = $("wfb-change");
  var form = $("wfb-form");
  if (changeBtn && form) {
    changeBtn.disabled = false;
    changeBtn.addEventListener("click", function () {
      form.classList.toggle("hidden");
    });
  }

  var saveBtn = $("wfb-save");
  if (saveBtn) {
    saveBtn.addEventListener("click", async function () {
      var chanSel = $("wfb-channel-input");
      var profSel = $("wfb-profile-input");
      var body = {};
      if (chanSel && chanSel.value) {
        var n = parseInt(chanSel.value, 10);
        if (!isNaN(n)) body.channel = n;
      }
      if (profSel && profSel.value) body.bitrate_profile = profSel.value;
      if (Object.keys(body).length === 0) {
        toast("Nothing to save.", "error");
        return;
      }
      try {
        await api("PUT", "/wfb", body);
        toast("Radio saved.", "success");
        if (form) form.classList.add("hidden");
        load();
      } catch (err) {
        reportAuthError(err);
        toast("Save failed: " + (err.code || "error"), "error");
      }
    });
  }
}

// ---------------------------------------------------------------------------
// advanced.html
// ---------------------------------------------------------------------------

function initAdvanced() {
  var reset = $("factory-reset");
  if (!reset) return;

  reset.addEventListener("click", async function () {
    var droneId = null;
    try {
      var s = await api("GET", "/status");
      droneId = s && s.paired_drone && s.paired_drone.device_id;
    } catch (err) {
      reportAuthError(err);
      toast("Could not read status: " + (err.code || "error"), "error");
      return;
    }

    var promptText;
    var expectedFallback = "factory-reset-unpaired";
    if (droneId) {
      promptText = "This will unpair from " + droneId + " and reset settings.\n\n" +
        "Type the pair key fingerprint (shown on the OLED or in Mission Control) to confirm:";
    } else {
      promptText = "This will reset all ground-station settings.\n\n" +
        "Type " + expectedFallback + " to confirm:";
    }

    var token = prompt(promptText, "");
    if (token == null) return;
    token = token.trim();
    if (!token) {
      toast("Confirmation required.", "error");
      return;
    }

    try {
      await api("POST", "/factory-reset?confirm=" + encodeURIComponent(token));
      alert("Factory reset complete. Reloading.");
      location.href = "index.html";
    } catch (err) {
      reportAuthError(err);
      if (err.status === 400) {
        toast("Confirmation did not match.", "error");
      } else {
        toast("Reset failed: " + (err.code || "error"), "error");
      }
    }
  });
}

// ---------------------------------------------------------------------------
// boot
// ---------------------------------------------------------------------------

document.addEventListener("DOMContentLoaded", function () {
  var backBtn = document.querySelector("header .back");
  if (backBtn) backBtn.addEventListener("click", goBack);

  var page = document.body.dataset.page;
  if (page === "index") initIndex();
  else if (page === "pair") initPair();
  else if (page === "network") initNetwork();
  else if (page === "display") initDisplay();
  else if (page === "wfb") initWfb();
  else if (page === "advanced") initAdvanced();
});

document.addEventListener("visibilitychange", function () {
  if (document.visibilityState === "visible" && document.body.dataset.page === "index") {
    refreshIndex();
  }
});
