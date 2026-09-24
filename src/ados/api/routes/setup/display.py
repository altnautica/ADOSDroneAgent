"""Display install + options + calibration routes."""

from __future__ import annotations

from fastapi import APIRouter, HTTPException

from ados.core.paths import DISPLAY_CONF_PATH
from ados.hal.detect import detect_board, detect_board_profile
from ados.setup import display_install
from ados.setup.models import (
    DisplayInstallRequest,
    DisplayJob,
    DisplayOption,
    DisplayOptionsResponse,
    SetupActionResult,
)

from ._common import now_iso

router = APIRouter()


def _label_for_display(display_id: str, controller: str) -> str:
    """Operator-facing label for a supported display id."""
    known = {
        "waveshare35a": 'Waveshare 3.5" SPI LCD',
        "waveshare35b": 'Waveshare 3.5" SPI LCD (B)',
        "waveshare35c": 'Waveshare 3.5" SPI LCD (C)',
    }
    return known.get(display_id, f"{display_id} ({controller})")


def _read_display_conf() -> dict[str, str] | None:
    """Parse /etc/ados/display.conf into a plain dict, or None when absent."""
    if not DISPLAY_CONF_PATH.exists():
        return None
    out: dict[str, str] = {}
    try:
        for raw in DISPLAY_CONF_PATH.read_text().splitlines():
            line = raw.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            k, _, v = line.partition("=")
            out[k.strip()] = v.strip()
    except OSError:
        return None
    return out or None


@router.get("/display/options", response_model=DisplayOptionsResponse)
async def get_display_options() -> DisplayOptionsResponse:
    """Return the supported displays for the active board plus the current state.

    Reads ``displays.supported`` from the matched board profile (variant
    applied, matched by device tree/stem like every other HAL consumer). A
    variant renames the board (an 8-core ROCK 5C is served by the Lite YAML), so
    matching loaded profiles by display name would miss it. Always includes a
    synthetic ``{ id: "none" }`` option so the wizard can offer an explicit skip.
    """
    board = detect_board()
    board_id = board.name or ""
    options: list[DisplayOption] = []

    profile = detect_board_profile()
    if profile is not None:
        for binding in profile.displays.supported:
            options.append(
                DisplayOption(
                    id=binding.id,
                    label=_label_for_display(binding.id, binding.controller),
                    controller=binding.controller,
                    touch_chip=binding.touch_chip,
                    resolution=binding.resolution,
                )
            )

    options.append(
        DisplayOption(id="none", label="Skip / no display attached")
    )

    current = _read_display_conf()
    return DisplayOptionsResponse(
        board_id=board_id,
        current=current,
        supported=options,
    )


@router.post("/display/install", response_model=DisplayJob)
async def trigger_display_install(request: DisplayInstallRequest) -> DisplayJob:
    """Spawn the LCD-overlay installer (or write the skip marker).

    ``display_id="none"`` is the skip path: the route writes
    ``/etc/ados/display.conf`` with ``display_id=none`` and returns a
    synthetic ``done`` job so the wizard can flip the step to
    ``optional`` without polling. Any other id spawns the shell driver
    via the in-process job tracker. Concurrent install requests get a
    409 ``Conflict``.
    """
    if not request.display_id:
        raise HTTPException(status_code=400, detail="display_id is required")

    if request.display_id == "none":
        try:
            display_install.write_skip_marker()
        except PermissionError as exc:
            raise HTTPException(
                status_code=500,
                detail=(
                    "Cannot write /etc/ados/display.conf — agent must run "
                    f"with permission to write the config dir ({exc})."
                ),
            ) from exc
        return DisplayJob(
            job_id="skip",
            display_id="none",
            status="done",
            started_at=now_iso(),
            finished_at=now_iso(),
            exit_code=0,
            log_tail=["operator skipped the display step"],
        )

    try:
        handle = await display_install.start_install(request.display_id)
    except RuntimeError as exc:
        raise HTTPException(status_code=409, detail=str(exc)) from exc
    except FileNotFoundError as exc:
        raise HTTPException(status_code=500, detail=str(exc)) from exc

    return DisplayJob(**handle.to_dict())


@router.get("/display/job/{job_id}", response_model=DisplayJob)
async def get_display_install_job(job_id: str) -> DisplayJob:
    """Poll the status of an in-flight or completed install job.

    The wizard hits this at 1-2 Hz while the job is queued or running
    and renders the ``log_tail`` so the operator can watch progress.
    Synthetic ``skip`` jobs (from the ``display_id=none`` path) return
    a static ``done`` snapshot.
    """
    if job_id == "skip":
        return DisplayJob(
            job_id="skip",
            display_id="none",
            status="done",
            started_at=now_iso(),
            finished_at=now_iso(),
            exit_code=0,
            log_tail=["operator skipped the display step"],
        )
    handle = display_install.get_job(job_id)
    if handle is None:
        raise HTTPException(status_code=404, detail=f"Unknown job id: {job_id}")
    return DisplayJob(**handle.to_dict())


@router.post("/display/calibrate/start", response_model=SetupActionResult)
async def start_display_calibration() -> SetupActionResult:
    """Launch the touch-calibration wizard on the panel.

    Drops the one-shot recalibrate flag the native display service consumes
    on its ~1 Hz loop; the wizard then runs on the panel with no restart.
    """
    from ados.api.routes.display import RECALIBRATE_FLAG_PATH, arm_touch_recalibration

    error = arm_touch_recalibration()
    if error is not None:
        return SetupActionResult(
            ok=False,
            message=f"Could not request touch calibration: {error}",
        )
    return SetupActionResult(
        ok=True,
        message="Touch calibration requested. The wizard appears on the panel within a few seconds.",
        data={"flag_path": str(RECALIBRATE_FLAG_PATH)},
    )
