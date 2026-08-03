"""Tests for `ados diag storage` rendering.

The point of this surface is that an operator can tell "healthy" from "nobody
measured" at a glance. Four SD cards were reflashed while every reading was
either absent or unread, so the cases that matter most here are the ones where a
number does NOT exist: they must render as a stated absence, never as a zero that
reads like a clean bill of health.
"""

from __future__ import annotations

from unittest.mock import patch

from click.testing import CliRunner

from ados.cli.diag import diag_group


def _invoke(payload: dict) -> str:
    runner = CliRunner()
    with patch("ados.cli.diag._request", return_value=payload):
        result = runner.invoke(diag_group, ["storage"])
    assert result.exit_code == 0, result.output
    return result.output


def _payload(**overrides) -> dict:
    base = {
        "verdict": "ok",
        "reason": "writing 1 KB/s sustained; no throttle events recorded",
        "write": {
            "kb_per_s": 1.0,
            "gb_per_day": 0.1,
            "window_s": 120.0,
            "device": "mmcblk0",
            "reason": None,
        },
        "throttle": {"supported": True, "clean": True},
        "store": {"live_bytes": 1048576, "wal_bytes": 4096, "quarantined": 0},
        "filesystem": {"total_bytes": 32212254720, "used_bytes": 6442450944,
                       "used_pct": 20.0, "reason": None},
    }
    base.update(overrides)
    return base


# --- the footprint budget --------------------------------------------------
#
# Space is what actually breaks these nodes: the card fills, a rewrite cannot get
# its scratch, a write tears, the filesystem corrupts and the box will not boot.
# So footprint leads the report and write rate follows it.


def _janitor_with_footprint(**overrides) -> dict:
    base = {
        "ran": True,
        "rung": "routine",
        "age_s": 60,
        "reclaimed_bytes": 0,
        "reclaimed": {},
        "reclaimable_bytes": 0,
        "reclaimable": {},
        "footprint_bytes": 1694498816,
        "footprint": {
            "quarantined_stores": 1073741824,
            "journal": 322961408,
            "apt": 195035136,
        },
        "budget_bytes": 5368709120,
        "caps": {"quarantined_stores": 419430400, "journal": 419430400},
        "over_cap": {"quarantined_stores": 654311424},
        "installed_bytes": 634388480,
        "reason": None,
    }
    base.update(overrides)
    return base


def test_footprint_leads_the_report_and_write_rate_follows_it():
    out = _invoke(_payload(janitor=_janitor_with_footprint()))
    assert "FOOTPRINT" in out
    assert "STORAGE WEAR" in out
    assert out.index("FOOTPRINT") < out.index("STORAGE WEAR"), (
        "space is what fills a card and corrupts it; wear is the slower signal"
    )


def test_the_total_is_shown_against_its_budget_with_a_percentage():
    out = _invoke(_payload(janitor=_janitor_with_footprint()))
    assert "1.6 GB of 5.0 GB" in out
    assert "(32%)" in out


def test_a_category_over_its_cap_is_named_with_the_excess():
    # A single quarantined store larger than the whole quarantine share is the
    # case that actually happened. The janitor's floor holds it, so the operator
    # has to know it is there rather than find it on the next reflash.
    out = _invoke(_payload(janitor=_janitor_with_footprint()))
    assert "quarantined stores" in out
    assert "over by" in out
    assert "624.0 MB" in out


def test_each_category_is_shown_against_its_own_cap():
    out = _invoke(_payload(janitor=_janitor_with_footprint()))
    # journal: 308 MB used of a 400 MB share.
    assert "308.0 MB / 400.0 MB" in out


def test_the_installed_agent_is_reported_and_marked_uncounted():
    # Reported so a card can be sized honestly; excluded so a release shipping a
    # bigger model does not silently eat the allowance for recordings.
    out = _invoke(_payload(janitor=_janitor_with_footprint()))
    assert "installed agent" in out
    assert "605.0 MB" in out
    assert "not counted" in out


def test_an_unmeasured_footprint_says_so_rather_than_claiming_zero():
    # A total of zero would say the agent occupies nothing at all, on a box
    # where nobody has looked.
    out = _invoke(
        _payload(
            janitor={
                "ran": False,
                "reason": "the janitor has not completed a pass since this box booted",
            }
        )
    )
    assert "FOOTPRINT" in out
    assert "has not completed a pass" in out
    assert "0 B of" not in out


def test_a_footprint_over_budget_reads_as_a_failure_not_a_warning():
    out = _invoke(
        _payload(
            janitor=_janitor_with_footprint(
                footprint_bytes=5500000000,
                budget_bytes=5368709120,
            )
        )
    )
    assert "(102%)" in out


def test_a_disabled_store_reads_as_off_not_as_an_empty_one():
    # Off is the default and is not a fault. "0 B live" would describe a store
    # that exists and happens to be empty, which is a different and much more
    # alarming claim than one that was never asked to run.
    out = _invoke(
        _payload(
            store={
                "enabled": False,
                "live_bytes": 0,
                "wal_bytes": 0,
                "quarantined": 0,
            }
        )
    )
    assert "disabled" in out
    assert "journal is the record" in out
    assert "0 B live" not in out


def test_an_enabled_store_still_reports_its_footprint():
    out = _invoke(
        _payload(
            store={
                "enabled": True,
                "live_bytes": 1048576,
                "wal_bytes": 4096,
                "quarantined": 0,
            }
        )
    )
    assert "1.0 MB live" in out
    assert "disabled" not in out


def test_the_write_rates_provenance_is_stated_not_left_implicit():
    # Five seconds of kernel counter and hours of stored history answer
    # different questions. Reading one as the other is how a change that did
    # nothing looks like a change that worked.
    direct = _invoke(
        _payload(
            write={
                "kb_per_s": 48.0,
                "gb_per_day": 4.0,
                "window_s": 5.0,
                "device": "mmcblk0",
                "source": "direct",
                "reason": None,
            }
        )
    )
    assert "kernel" in direct
    assert "retained history" not in direct

    stored = _invoke(
        _payload(
            write={
                "kb_per_s": 48.0,
                "gb_per_day": 4.0,
                "window_s": 3600.0,
                "device": "mmcblk0",
                "source": "store",
                "reason": None,
            }
        )
    )
    assert "retained history" in stored


def test_healthy_box_reports_the_rate_and_the_device():
    out = _invoke(_payload())
    assert "OK" in out
    assert "1.0 KB/s" in out
    assert "mmcblk0" in out
    assert "120s window" in out


def test_an_unmeasured_rate_states_why_and_never_prints_a_zero():
    out = _invoke(
        _payload(
            verdict="unknown",
            reason="the logging store did not answer",
            write={"kb_per_s": None, "gb_per_day": None, "window_s": None,
                   "device": None, "reason": "the logging store did not answer"},
        )
    )
    assert "UNKNOWN" in out
    assert "the logging store did not answer" in out
    assert "0.0 KB/s" not in out


def test_a_recorded_undervoltage_is_named_not_summarised_as_a_flag():
    out = _invoke(
        _payload(
            verdict="wearing",
            throttle={
                "supported": True,
                "clean": False,
                "undervoltage_occurred": True,
                "arm_frequency_capped_occurred": False,
                "throttling_occurred": False,
                "soft_temperature_limit_occurred": False,
            },
        )
    )
    assert "undervoltage" in out
    assert "retained window" in out


def test_a_board_with_no_throttle_bitfield_does_not_read_as_clean():
    out = _invoke(
        _payload(
            throttle={
                "supported": False,
                "reason": "this board does not report a throttle bitfield",
            }
        )
    )
    assert "does not report a throttle bitfield" in out
    assert "clean, no events recorded" not in out


def test_quarantined_corpses_are_surfaced_with_their_size():
    out = _invoke(
        _payload(
            store={
                "live_bytes": 935616512,
                "wal_bytes": 4387832,
                "quarantined": 2,
                "quarantined_bytes": 1620725760,
            }
        )
    )
    assert "2 torn store(s)" in out
    assert "not reclaimed" in out


def test_a_critical_verdict_carries_its_reason():
    out = _invoke(
        _payload(
            verdict="critical",
            reason="writing 1714 KB/s sustained (141 GB/day); this is the rate "
            "that wore a card out in under two days",
            write={"kb_per_s": 1714.0, "gb_per_day": 141.0, "window_s": 120.0,
                   "device": "mmcblk0", "reason": None},
        )
    )
    assert "CRITICAL" in out
    assert "wore a card out" in out
    assert "1714.0 KB/s" in out


# --- the disk janitor's record --------------------------------------------
#
# The card that filled was not writing quickly. It was holding 349 MB of
# downloaded packages that nothing ever removed, so the write rate above is only
# half the picture and these rows are the other half.


def test_a_box_whose_janitor_never_ran_says_so_and_prints_no_figures():
    out = _invoke(
        _payload(
            janitor={
                "ran": False,
                "rung": None,
                "reclaimed_bytes": None,
                "reclaimable_bytes": None,
                "reason": "the janitor has not completed a pass since this box booted",
            }
        )
    )
    assert "RECLAIM" in out
    assert "has not completed a pass" in out
    # The distinction that matters: nobody has looked is not the same answer as
    # there being nothing left to reclaim, and a zero here would say the latter.
    assert "0 B" not in out


def test_the_last_pass_names_its_rung_and_breaks_the_bytes_down_by_category():
    out = _invoke(
        _payload(
            janitor={
                "ran": True,
                "rung": "pressure",
                "ran_at_unix": 1700000000,
                "age_s": 2400,
                "reclaimed_bytes": 366002176,
                "reclaimed": {
                    "apt_archives": 195035136,
                    "apt_lists": 170967040,
                    "plugin_logs": 0,
                },
                "reclaimable_bytes": 1073741824,
                "reclaimable": {"quarantined_stores": 1073741824},
                "reason": None,
            }
        )
    )
    assert "pressure" in out
    assert "40 min ago" in out
    assert "349.0 MB" in out
    assert "downloaded packages" in out
    assert "package index" in out
    assert "quarantined stores" in out
    assert "1.0 GB" in out
    # A category that reclaimed nothing is not listed; a row of zeroes tells an
    # operator nothing and buries the two categories that matter.
    assert "plugin logs" not in out


def test_an_unknown_pass_age_is_not_rendered_as_just_now():
    out = _invoke(
        _payload(
            janitor={
                "ran": True,
                "rung": "routine",
                "age_s": None,
                "reclaimed_bytes": 0,
                "reclaimed": {},
                "reclaimable_bytes": None,
                "reclaimable": None,
                "reason": None,
            }
        )
    )
    assert "at an unknown time" in out
    assert "0 min ago" not in out
    # The reclaimable figure genuinely was not reported; say so rather than
    # implying the box has nothing left to give.
    assert "not reported" in out


def test_an_agent_that_reports_an_unknown_category_still_shows_it():
    # A category this renderer has no label for is still something the janitor
    # deleted. Dropping the row would leave bytes disappearing with nothing on
    # screen to account for them.
    out = _invoke(
        _payload(
            janitor={
                "ran": True,
                "rung": "routine",
                "age_s": 60,
                "reclaimed_bytes": 4096,
                "reclaimed": {"some_future_category": 4096},
                "reclaimable_bytes": 0,
                "reclaimable": {},
                "reason": None,
            }
        )
    )
    assert "some_future_category" in out
