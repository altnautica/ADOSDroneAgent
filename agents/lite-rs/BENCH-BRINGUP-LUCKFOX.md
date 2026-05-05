# Lite agent bench bringup — Luckfox Pico Zero

This runbook walks through the end-to-end validation gate for Phase 1 of
the lite-agent track. Goal: prove that a stock Luckfox Pico Zero,
flashed with the vendor SDK Buildroot image, completes the full DEC-141
setup wizard locally — no Python full agent on the rootfs, no manual
edits, no operator intervention beyond the wizard form itself.

Estimated time on the bench: 60–90 minutes the first time, 15 minutes
on a re-run.

---

## 0. What you need on the desk

| | |
|---|---|
| Board | Luckfox Pico Zero (RV1106G3, 256 MB, ARMv7 single-core) |
| Storage | microSD card, 8 GB or larger, formatted with the Luckfox SDK image |
| Cable | USB-C cable (carries power + serial console + Ethernet-over-USB) |
| Host | Mac or Linux with a working browser, on the same Wi-Fi as the board |
| Optional FC | Any ArduPilot/PX4 board over UART or USB CDC — required to validate the FC serial probe in /hardware-check |

The agent itself does not need the FC connected to walk through the
wizard. Step 5 of the wizard reports `state: "missing"` for the FC
component when there is no serial device, which is the correct outcome.

---

## 1. Image flash (operator side, not us)

This is on you. We do not ship a flashable image at Phase 1 (that lands
in MSN-058). For now, flash the Luckfox SDK Buildroot image using
Luckfox's documented flow:

- Windows: SocToolKit (Luckfox's flashing utility)
- Linux/Mac: `dd if=luckfox-pico-zero.img of=/dev/sdX bs=4M conv=fsync`

Vendor docs: <https://wiki.luckfox.com/Luckfox-Pico/Luckfox-Pico-quick-start>

Confirm the board boots: connect USB-C, watch for the device to enumerate
as a serial console at `/dev/cu.usbmodem*` (Mac) or `/dev/ttyACM*`
(Linux). Default user: `root`, no password.

## 2. Networking the board (operator side)

Bring the board onto your Wi-Fi using whichever method you prefer —
console + `wpa_supplicant` config, or Ethernet over USB (the Luckfox
exposes a CDC-NCM Ethernet device automatically when plugged in).

Sanity check from the board's shell:

```sh
ip addr show           # confirm an IP is bound
ping -c 1 github.com   # confirm outbound DNS + reachability
```

If `ping github.com` fails, fix the network before proceeding. The
installer requires reachability to GitHub Releases.

## 3. Install (one curl command)

From the board's shell:

```sh
curl -sSL https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/scripts/install.sh | sudo bash
```

That's it. No env var, no flag, no pair code. The script:

1. Reads `/proc/device-tree/model` → matches `Luckfox Pico Zero` against
   the lite-eligible board manifest → picks `lite-rs`
2. Detects `armv7l` + `uclibc` → resolves the `armv7-unknown-linux-musleabihf`
   binary URL from the latest GitHub Release
3. Downloads the tarball + `.sha256` + `.minisig`
4. Verifies SHA256 (mandatory) + minisign signature (mandatory unless
   `ADOS_LITE_ALLOW_UNSIGNED=1` — only set this for testing locally
   built binaries)
5. Extracts the binary to `/usr/local/bin/ados-agent-lite`
6. Generates a stable `device_id` from `/etc/machine-id`
7. Writes `/etc/ados/agent.yaml` with the canonical relay URL and an
   empty `api_key` (unpaired)
8. Detects busybox sysv-rc, drops `/etc/init.d/S99ados-agent-lite`,
   registers it via `update-rc.d`
9. Starts the service. First heartbeat fires within ~2 seconds.

Expected end-of-install output:

```
==================================================================
  ADOS Drone Agent (lite) installed (UNPAIRED)
==================================================================
  Service:    ados-agent-lite is running unpaired
  Webapp:     http://<board-ip>:8080/setup

  To pair the drone, choose one:
    1. Visit http://<board-ip>:8080/setup and complete the wizard
    2. Run on this board:    sudo ados-agent-lite pair PAIRCODE
    3. In Mission Control "Add drone", enter the beacon code printed
       to the agent log on first boot:
          sudo journalctl -u ados-agent-lite -n 50 | grep -i beacon
==================================================================
```

If you see this banner, the install path passed.

## 4. Smoke check the agent

From the board's shell:

```sh
ados-agent-lite version
# expected: ados-agent-lite 0.1.0

curl -s http://127.0.0.1:8080/api/v1/setup/status | head -20
# expected: JSON object with device_id, runtime_mode: "lite", paired: false
```

Pass criteria for step 4: `ados-agent-lite version` prints a version,
`/api/v1/setup/status` returns 200 with the canonical JSON shape, and
no `Service start failed` lines in the init log.

## 5. Walk the wizard from the host browser

On your Mac/Linux: open `http://<board-ip>:8080/setup` in any browser.

The wizard renders in the same single-page-app code the Python full
agent ships (web/setup/), now served by axum from the Rust binary via
include_dir! at compile time.

Step through each screen, watching for the validation behaviour:

| Wizard step | Expected behaviour | What to verify |
|---|---|---|
| Welcome | Cannot be skipped | Skip button hidden or disabled |
| Profile | Drone preselected | Pick "Drone — Lite". POST /profile responds 200 |
| Hardware check | All four rows render | board=ok, fc=missing (no FC) or ok (FC present), camera=missing, wifi=ok |
| Cloud choice | Three modes available | Pick "cloud" (Altnautica relay). POST /cloud-choice responds 200 |
| Pair | Optional at this step | Skip with "Skip for now" — wizard accepts |
| Remote access | Cloudflare option | Optional — skip is fine |
| Finish | Single button | POST /finish flips setup_finalized=true |

After /finish, the wizard redirects to a "Setup complete" view.

Pass criteria for step 5: every screen renders without 404s, every
mutation route returns 200 + ok=true, the final /status read shows
`setup_finalized: true` and `next_action: "ready"`.

## 6. Pair the drone

Two paths — pick whichever is convenient.

### Path A — Mission Control beacon

1. In Mission Control's fleet view, click "Add drone"
2. The dialog asks for a pair code; the drone is already heartbeating
   the cloud relay with an unpaired beacon
3. Find the beacon code on the board:
   ```sh
   grep -i beacon /var/log/messages  # busybox syslog
   # or
   ados-agent-lite status --json | jq .device_id
   ```
4. Enter the code in Mission Control. The cloud relay sends a pair
   binding back via `cmd_droneCommands`; the agent persists the API key
   and switches from beacon to heartbeat

### Path B — CLI

If you already have a pair code:

```sh
sudo ados-agent-lite pair AB23X4
# expected: paired and config updated at /etc/ados/agent.yaml
#           restarted service via /etc/init.d/S99ados-agent-lite
```

Confirm:

```sh
sudo grep api_key /etc/ados/agent.yaml
# expected: api_key: AB23X4
```

Pass criteria for step 6: drone appears in Mission Control's fleet
card with a "Lite" badge, telemetry shows `runtime_mode: lite`, and
the next-action banner clears.

## 7. Cloudflare Tunnel (optional)

Only needed if you want public access to the setup webapp without
opening a port on your home router.

1. Create a Cloudflare Tunnel via the dashboard
   (<https://one.dash.cloudflare.com/> → Tunnels → Create)
2. Copy the install command Cloudflare shows you
3. Paste the entire command into the wizard's "Remote access" step,
   or POST to `/api/v1/setup/remote-access/cloudflare` with the JSON
   body `{"token_or_script": "<paste>"}`
4. The agent extracts the JWT token, persists it root-owned 0600 to
   `/etc/ados/secrets/cloudflare-tunnel-token`, downloads `cloudflared`
   from CF's official releases, drops a busybox init unit at
   `/etc/init.d/cloudflared`, and starts the service.
5. Watch the WS log stream at `/api/v1/setup/cloudflare/logs` to confirm
   the tunnel comes up without errors. Token-shaped substrings are
   redacted in the stream so a future regression that logs a bearer
   doesn't leak it through the wizard.
6. After ~10 seconds, hit `/api/v1/setup/cloudflare/verify` to confirm
   the public URL routes back to the agent.

Pass criteria for step 7: cloudflared service is running, log stream
is live, verify returns `reachable: true` with `latency_ms` populated.

## 8. Soak test (optional but recommended)

Leave the board running for ~1 hour with telemetry flowing into Mission
Control. Watch for:

- `dmesg | grep -i killed` — should be empty (no OOM events)
- `ps aux --sort=-rss | head` — `ados-agent-lite` resident memory
  should hold steady at ~15–20 MB
- No agent restarts in the init log (`/var/log/messages` or
  `journalctl -u ados-agent-lite` if you patched in systemd)
- Mission Control fleet card stays green; heartbeat interval ≤ 5 s

Pass criteria for step 8: no OOM, no restart, RSS stable.

---

## Failure triage

| Symptom | Likely cause | Fix |
|---|---|---|
| install.sh prints `error: tarball not found for armv7-unknown-linux-musleabihf` | GitHub Releases not yet published, or network down | Check `https://github.com/altnautica/ADOSDroneAgent/releases` — the rolling `lite-agent-main` should always have the latest binary |
| install.sh prints `signature verification failed` | minisign public key mismatch | Stop and report — never bypass with `ADOS_LITE_ALLOW_UNSIGNED=1` on a real install |
| Wizard form submissions return 404 | Board failed to start the agent | `ps aux \| grep ados-agent-lite`; check init.d log; restart with `/etc/init.d/S99ados-agent-lite restart` |
| Wizard form submissions return 500 with "could not persist" | /var/lib/ados/setup or /etc/ados is not writable | `chown root:root /var/lib/ados/setup /etc/ados; chmod 0755 /var/lib/ados/setup; chmod 0750 /etc/ados` |
| `cloudflared` doesn't start | Token rejected, or systemd missing on busybox | Read /var/log/messages; verify the token by pasting it into Cloudflare dashboard's connector list |
| Drone never appears in Mission Control | Cloud relay unreachable, or beacon code not entered | `curl https://convex-site.altnautica.com/agent/status` from the board's shell — should respond. If yes, the issue is on the GCS side |

---

## Closeout checklist

When all eight steps above pass, B7.10 (and MSN-054 Phase 1) is
complete:

- [ ] Step 1 — image flashed
- [ ] Step 2 — board on the network
- [ ] Step 3 — install.sh succeeds end-to-end with no manual intervention
- [ ] Step 4 — `/api/v1/setup/status` returns 200 with canonical shape
- [ ] Step 5 — every wizard screen renders + persists state
- [ ] Step 6 — drone shows up in Mission Control with "Lite" badge
- [ ] Step 7 — Cloudflare Tunnel comes up + verify responds (optional)
- [ ] Step 8 — 1-hour soak with no OOM + no restart (optional)

After this, DEC-142 flips DRAFT → REVIEW. FINAL is gated on MSN-055
landing the video pipeline.
