"""Perception tier decision (a mirror of ``ados-offload::pick_tier``).

The canonical definition lives in the Rust crate ``ados-offload`` (``tier.rs``);
this small Python mirror lets the heartbeat report the tier a node would pick,
keyed on the board's accelerator. It is branch-for-branch identical.

Where perception runs:

- **local**   the drone's own NPU runs the model (lowest latency);
- **offload** an NPU-less board streams frames to a paired compute node;
- **hybrid**  light work local, heavy work on the node;
- **none**    no local accelerator and no usable compute node (bare odometry,
              no detection / tracking / map-based autonomy).

The offload inputs (a paired workstation + an acceptable link) are not wired on
the drone yet, so they are conservatively ``False`` here: a board with a local
compute path — an NPU, or the profile-declared CPU-ONNX local inference — reads
``local`` and a board without any path reads ``none`` (never a fabricated
``offload`` when no node is known). When the drone-side offload orchestration
lands, those signals feed in; a future Rust perception service may supersede this
by writing the tier to a sidecar the heartbeat prefers.
"""

from __future__ import annotations


def perception_tier(
    *,
    has_accelerator: bool,
    local_inference_capable: bool = False,
    models_fit_locally: bool = True,
    compute_node_paired: bool = False,
    bearer_acceptable: bool = False,
    can_run_light_local: bool = False,
) -> str:
    """Return the perception tier: ``local`` | ``offload`` | ``hybrid`` | ``none``.

    Mirrors ``ados-offload::pick_tier`` branch-for-branch. ``models_fit_locally``
    defaults ``True`` (an NPU board is assumed to fit its recommended detector);
    ``local_inference_capable`` is the board's CPU-ONNX declaration (an NPU-less
    but CPU-strong board runs the detector on-board, a full local path); the
    offload signals default ``False`` until the orchestration wires them.
    """
    if (has_accelerator and models_fit_locally) or local_inference_capable:
        return "local"
    if compute_node_paired and bearer_acceptable:
        return "hybrid" if can_run_light_local else "offload"
    if can_run_light_local:
        return "local"
    return "none"
