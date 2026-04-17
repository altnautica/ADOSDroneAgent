"""Mesh-mode screens for the ground-station OLED.

Each module in this package exports:

- `render(draw, width, height, state)` — pure draw function, never
  mutates `state`. Reads `state['role']`, `state['mesh']`, and
  `state['pairing']` for the distributed-receive flow plus an optional
  `state['_overlay_hints']` dict the OLED service injects while the
  screen is active as an overlay.

Overlay screens additionally expose:

- `BUTTON_ACTIONS: dict[int, Callable[[service], Awaitable[None]]]`
  mapping the BCM button GPIO (B1/B2/B3/B4) to an async handler. The
  handler receives the live `OledService` instance so it can POST to
  the agent REST API, mutate `service._overlay_state`, or pop back out
  of the overlay by calling `service._exit_overlay()`.

Screens in this package:

- `unset_boot.py`     first-boot role-unset banner
- `role_picker.py`    cycle direct/relay/receiver, confirm to PUT /role
- `accept_window.py`  receiver 60 s accept window + pending relay list
- `join_scan.py`      relay mDNS scan for a receiver on bat0
- `join_request_inflight.py`  relay waiting for the invite bundle
- `joined_status.py`  relay happy-path summary
- `hub_unreachable.py`  relay receiver-lost grace period
- `neighbors.py`      peer list with TQ + last-seen
- `leave_confirm.py`  destructive confirm to exit the mesh
- `error_states.py`   switch-on-code error presentation
"""
