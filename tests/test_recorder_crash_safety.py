# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""The ground-station capture must survive an unclean shutdown.

A plain (or ``+faststart``) MP4 writes its ``moov`` atom only when the muxer
closes cleanly. A ground station that loses power, is SIGKILLed or fills its
disk mid-capture therefore leaves a file ``ffprobe`` reports as "moov atom not
found" — the whole flight recording, not its tail. This is the recording an
operator reaches for after an incident.

A fragmented MP4 carries an index per fragment, so the file is valid at every
fragment boundary. Two front ends can serve a recording start on a ground
station (the native ``ados-control`` front and the Python REST surface behind
its proxy), so the capture must not be crash-safe on only one of them: the
muxer flags are compared across both.
"""

from __future__ import annotations

import asyncio
import os
import re
import stat
from pathlib import Path

import pytest

from ados.services.ground_station.recorder import (
    RECORDER_MOVFLAGS,
    GroundStationRecorder,
)

REPO_ROOT = Path(__file__).resolve().parents[1]
NATIVE_RECORDER = REPO_ROOT / "crates" / "ados-video" / "src" / "recorder.rs"


async def _captured_argv(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> list[str]:
    """Drive a real start against an `ffmpeg` that records its own argv."""
    bindir = tmp_path / "bin"
    bindir.mkdir()
    fake = bindir / "ffmpeg"
    # `exec` so the idling process IS the shell: a forked child would inherit
    # the stderr pipe and outlive the kill below, and `proc.wait()` would then
    # block on the pipe rather than on the process.
    fake.write_text(
        "#!/bin/sh\n"
        "for out; do :; done\n"
        "printf '%s\\n' \"$@\" > \"$out.argv\"\n"
        "exec sleep 30\n",
        encoding="utf-8",
    )
    fake.chmod(fake.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}{os.environ['PATH']}")

    recorder = GroundStationRecorder(recording_dir=tmp_path / "recordings")
    started = await recorder.start("frag")
    argv_path = Path(f"{started['path']}.argv")
    try:
        for _ in range(100):
            if argv_path.is_file():
                break
            await asyncio.sleep(0.02)
        return argv_path.read_text(encoding="utf-8").splitlines()
    finally:
        proc = recorder._process
        if proc is not None and proc.returncode is None:
            proc.kill()
            await proc.wait()


async def test_the_capture_is_muxed_as_a_fragmented_mp4(tmp_path, monkeypatch):
    argv = await _captured_argv(tmp_path, monkeypatch)

    assert "-movflags" in argv, f"the recorder passes no -movflags: {argv}"
    flags = argv[argv.index("-movflags") + 1]
    for required in ("frag_keyframe", "empty_moov", "default_base_moof"):
        assert required in flags, (
            "a capture killed mid-write is only playable with all three fMP4 "
            f"flags; -movflags was {flags!r}"
        )
    assert not any("faststart" in arg for arg in argv), (
        f"+faststart defers the moov atom to a clean exit: {argv}"
    )

    # Without this, a fragment's tail sits in ffmpeg's AVIO buffer and the loss
    # window is the buffer rather than the current GOP.
    assert "-flush_packets" in argv, f"the recorder does not flush packets: {argv}"
    assert argv[argv.index("-flush_packets") + 1] == "1"


@pytest.mark.skipif(
    not NATIVE_RECORDER.exists(), reason="native crate not in this checkout"
)
def test_both_front_ends_mux_identically():
    """Either front end can serve a start, so both must be crash-safe.

    Parsed out of the Rust source rather than imported, because there is no
    build step in this suite that would expose it; that makes the parse itself
    a failure mode, so it is asserted before the comparison runs.
    """
    src = NATIVE_RECORDER.read_text(encoding="utf-8")
    match = re.search(r'const RECORDER_MOVFLAGS:\s*&str\s*=\s*"([^"]+)"\s*;', src)
    assert match is not None, (
        f"could not parse RECORDER_MOVFLAGS out of {NATIVE_RECORDER.name}; the "
        "parse is broken, so this comparison would prove nothing"
    )
    assert match.group(1) == RECORDER_MOVFLAGS
