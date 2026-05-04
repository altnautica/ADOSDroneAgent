# Relay Mode Setup

A relay listens to the drone with its own WFB-ng radio and forwards fragments to
a receiver over the local mesh. Use the setup webapp, OLED, Mission Control
Hardware tab, or REST API for relay setup.

## Flow

1. Install or upgrade the Ground Agent.
2. Confirm a second mesh-capable WiFi adapter is present.
3. On the receiver, open the Accept relay window from OLED or setup webapp.
4. On the relay, use OLED or setup webapp to join the mesh.
5. Approve the pending relay on the receiver.
6. Switch the relay role from direct to relay.
7. Confirm receiver reachability and fragment counters from setup webapp,
   Hardware tab, or `/api/v1/ground-station/wfb/relay/status`.

## Recovery

| Symptom | Recovery |
|---|---|
| Join flow finds no receiver | Confirm mesh carrier is up and the receiver is advertising on the same mesh. |
| Relay role fails before pairing | Complete receiver pairing first. Relay role requires mesh identity files. |
| Receiver is unreachable | Check receiver power, mesh neighbors, mDNS, and `ados-batman.service`. |
| Need direct mode again | Use setup webapp or OLED role controls to switch back to direct. |

## References

- [Field pairing protocol](pairing-protocol.md).
- [Ground station role and mesh actions](cli-reference-mesh.md).
- [Mesh networking](mesh-networking.md).

