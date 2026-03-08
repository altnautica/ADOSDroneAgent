"""Tests for ParamCache — in-memory + persistent JSON parameter cache."""

from __future__ import annotations

import json
import tempfile
from pathlib import Path

from ados.services.mavlink.param_cache import ParamCache


def test_get_set_basic():
    """Set and retrieve a parameter."""
    cache = ParamCache(path="/tmp/ados_test_params_unused.json")
    cache.set("ARMING_CHECK", 1.0)
    assert cache.get("ARMING_CHECK") == 1.0


def test_get_missing():
    """Getting a non-existent param returns None."""
    cache = ParamCache(path="/tmp/ados_test_params_unused.json")
    assert cache.get("NONEXISTENT") is None


def test_get_all():
    """get_all returns all cached values."""
    cache = ParamCache(path="/tmp/ados_test_params_unused.json")
    cache.set("PARAM_A", 1.5)
    cache.set("PARAM_B", 2.5)

    all_params = cache.get_all()
    assert all_params == {"PARAM_A": 1.5, "PARAM_B": 2.5}


def test_get_all_detailed():
    """get_all_detailed includes metadata."""
    cache = ParamCache(path="/tmp/ados_test_params_unused.json")
    cache.set("PARAM_X", 42.0, param_type=9)

    detailed = cache.get_all_detailed()
    assert "PARAM_X" in detailed
    assert detailed["PARAM_X"]["value"] == 42.0
    assert detailed["PARAM_X"]["param_type"] == 9
    assert detailed["PARAM_X"]["last_updated"] > 0


def test_overwrite_param():
    """Setting a param twice should update the value."""
    cache = ParamCache(path="/tmp/ados_test_params_unused.json")
    cache.set("THROTTLE_MAX", 80.0)
    cache.set("THROTTLE_MAX", 90.0)
    assert cache.get("THROTTLE_MAX") == 90.0


def test_clear():
    """clear() should remove all params."""
    cache = ParamCache(path="/tmp/ados_test_params_unused.json")
    cache.set("A", 1.0)
    cache.set("B", 2.0)
    assert cache.count == 2

    cache.clear()
    assert cache.count == 0
    assert cache.get("A") is None


def test_count():
    """count property should reflect number of cached params."""
    cache = ParamCache(path="/tmp/ados_test_params_unused.json")
    assert cache.count == 0
    cache.set("X", 1.0)
    assert cache.count == 1
    cache.set("Y", 2.0)
    assert cache.count == 2


def test_save_and_load():
    """Save to disk, create new cache, load from disk."""
    with tempfile.TemporaryDirectory() as tmpdir:
        path = Path(tmpdir) / "params.json"
        cache = ParamCache(path=path)
        cache.set("BATT_CAPACITY", 5200.0, param_type=6)
        cache.set("FS_THR_VALUE", 975.0, param_type=9)
        cache.save()

        # Verify file exists and is valid JSON
        assert path.is_file()
        raw = json.loads(path.read_text())
        assert "BATT_CAPACITY" in raw
        assert raw["BATT_CAPACITY"]["value"] == 5200.0

        # Load into a fresh cache
        cache2 = ParamCache(path=path)
        cache2.load()
        assert cache2.get("BATT_CAPACITY") == 5200.0
        assert cache2.get("FS_THR_VALUE") == 975.0
        assert cache2.count == 2


def test_load_nonexistent_file():
    """Loading from a missing file should be a silent no-op."""
    cache = ParamCache(path="/tmp/ados_nonexistent_cache_file_xyz.json")
    cache.load()
    assert cache.count == 0


def test_load_corrupted_json():
    """Loading corrupted JSON should not crash."""
    with tempfile.TemporaryDirectory() as tmpdir:
        path = Path(tmpdir) / "bad.json"
        path.write_text("{not valid json!!!")

        cache = ParamCache(path=path)
        cache.load()
        assert cache.count == 0


def test_save_creates_parent_dirs():
    """save() should create parent directories if needed."""
    with tempfile.TemporaryDirectory() as tmpdir:
        path = Path(tmpdir) / "sub" / "dir" / "params.json"
        cache = ParamCache(path=path)
        cache.set("TEST", 1.0)
        cache.save()
        assert path.is_file()


def test_save_atomic_write():
    """save() should use atomic write (write to .tmp then rename)."""
    with tempfile.TemporaryDirectory() as tmpdir:
        path = Path(tmpdir) / "params.json"
        cache = ParamCache(path=path)
        cache.set("A", 1.0)
        cache.save()

        # After save, .tmp should NOT exist (was renamed)
        tmp_path = path.with_suffix(".tmp")
        assert not tmp_path.exists()
        assert path.is_file()
