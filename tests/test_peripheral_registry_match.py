"""Connection matching for peripheral manifests.

A class-wide path pattern (any framebuffer, any V4L2 node) must not
report a device the manifest does not describe: a VM console
framebuffer is not an SPI LCD, a SoC codec node is not a UVC camera.
The shipped seeds narrow their path match with a sysfs identity, and
these tests run them against a fake ``/dev`` listing and sysfs tree.
"""

from __future__ import annotations

import os
import shutil
from pathlib import Path

import pytest

from ados.services.peripherals import registry as reg
from ados.services.peripherals.registry import PeripheralRegistry

_SEEDS = Path(__file__).resolve().parents[1] / "scripts" / "peripherals-seed"


@pytest.fixture
def sys_class(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    root = tmp_path / "sys" / "class"
    root.mkdir(parents=True)
    monkeypatch.setattr(reg, "_SYS_CLASS_ROOT", root)
    return root


def _dev_nodes(monkeypatch: pytest.MonkeyPatch, *paths: str) -> None:
    monkeypatch.setattr(reg, "_dev_nodes", lambda: iter(paths))


def _registry(tmp_path: Path, seed: str) -> PeripheralRegistry:
    manifests = tmp_path / "manifests"
    manifests.mkdir(exist_ok=True)
    shutil.copy(_SEEDS / seed, manifests / seed)
    return PeripheralRegistry(glob_path=str(manifests / "*.yaml"))


def _framebuffer(sys_class: Path, node: str, name: str) -> None:
    fb = sys_class / "graphics" / node
    fb.mkdir(parents=True)
    (fb / "name").write_text(f"{name}\n")


def _video_node(tmp_path: Path, sys_class: Path, node: str, driver: str) -> None:
    driver_dir = tmp_path / "sys" / "bus" / "drivers" / driver
    driver_dir.mkdir(parents=True, exist_ok=True)
    device = sys_class / "video4linux" / node / "device"
    device.mkdir(parents=True)
    os.symlink(driver_dir, device / "driver")


def _connected(registry: PeripheralRegistry, peripheral_id: str) -> bool:
    entry = registry.get(peripheral_id)
    assert entry is not None
    return entry["connected"]


def test_spi_lcd_seed_ignores_a_console_framebuffer(
    tmp_path: Path, sys_class: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _framebuffer(sys_class, "fb0", "hyperv_fb")
    _dev_nodes(monkeypatch, "/dev/fb0")
    registry = _registry(tmp_path, "spi-lcd-rpi-a.yaml")
    assert _connected(registry, "ados.spi-lcd-rpi-a") is False


def test_spi_lcd_seed_ignores_a_framebuffer_with_no_sysfs_entry(
    tmp_path: Path, sys_class: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _dev_nodes(monkeypatch, "/dev/fb0")
    registry = _registry(tmp_path, "spi-lcd-rpi-a.yaml")
    assert _connected(registry, "ados.spi-lcd-rpi-a") is False


def test_spi_lcd_seed_matches_the_ili9486_framebuffer(
    tmp_path: Path, sys_class: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _framebuffer(sys_class, "fb0", "vc4drmfb")
    _framebuffer(sys_class, "fb1", "fb_ili9486")
    _dev_nodes(monkeypatch, "/dev/fb0", "/dev/fb1")
    registry = _registry(tmp_path, "spi-lcd-rpi-a.yaml")
    assert _connected(registry, "ados.spi-lcd-rpi-a") is True


def test_uvc_seed_ignores_a_soc_codec_node(
    tmp_path: Path, sys_class: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _video_node(tmp_path, sys_class, "video10", "bcm2835-codec")
    _dev_nodes(monkeypatch, "/dev/video10")
    registry = _registry(tmp_path, "usb-camera-uvc.yaml")
    assert _connected(registry, "ados.usb-camera-uvc") is False


def test_uvc_seed_matches_a_uvcvideo_node(
    tmp_path: Path, sys_class: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _video_node(tmp_path, sys_class, "video10", "bcm2835-codec")
    _video_node(tmp_path, sys_class, "video0", "uvcvideo")
    _dev_nodes(monkeypatch, "/dev/video10", "/dev/video0")
    registry = _registry(tmp_path, "usb-camera-uvc.yaml")
    assert _connected(registry, "ados.usb-camera-uvc") is True


def test_path_only_manifest_still_matches_by_path(
    tmp_path: Path, sys_class: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    manifests = tmp_path / "manifests"
    manifests.mkdir()
    (manifests / "fc.yaml").write_text(
        "id: example.fc\n"
        "display_name: Example FC\n"
        "transport: serial\n"
        "match:\n"
        '  regex: "^/dev/ttyACM[0-9]+$"\n'
    )
    _dev_nodes(monkeypatch, "/dev/ttyACM0")
    registry = PeripheralRegistry(glob_path=str(manifests / "*.yaml"))
    assert _connected(registry, "example.fc") is True
