"""Unit checks on volatile masking, JSON canonicalization, and the header comparator.

Every check here is pure logic over synthetic byte strings and dicts: no live
service, no network, deterministic on any host.
"""

from api_conformance.normalize import (
    VOLATILE_SENTINEL,
    canonical_json,
    compare_headers,
    normalize_body,
    normalize_sse_frames,
)


def test_canonical_json_sorts_keys_and_is_compact():
    a = canonical_json({"b": 1, "a": 2})
    b = canonical_json({"a": 2, "b": 1})
    assert a == b
    assert a == b'{"a":2,"b":1}'


def test_canonical_json_sorts_nested_keys():
    value = {"outer": {"z": 1, "a": 2}, "list": [{"y": 1, "x": 2}]}
    out = canonical_json(value)
    assert out == b'{"list":[{"x":2,"y":1}],"outer":{"a":2,"z":1}}'


def test_normalize_body_masks_default_volatile_keys():
    body = b'{"name":"radio","uptime":1234,"ts":99,"pid":42}'
    out = normalize_body(body)
    # name survives; uptime/ts/pid collapse to the sentinel.
    assert out == canonical_json(
        {
            "name": "radio",
            "uptime": VOLATILE_SENTINEL,
            "ts": VOLATILE_SENTINEL,
            "pid": VOLATILE_SENTINEL,
        }
    )


def test_normalize_body_masks_nested_and_listed_volatile_keys():
    body = b'{"items":[{"id":1,"started_at":7},{"id":2,"started_at":8}],"v":5}'
    out = normalize_body(body, extra_volatile=("id",))
    assert out == canonical_json(
        {
            "items": [
                {"id": VOLATILE_SENTINEL, "started_at": VOLATILE_SENTINEL},
                {"id": VOLATILE_SENTINEL, "started_at": VOLATILE_SENTINEL},
            ],
            "v": 5,
        }
    )


def test_normalize_body_masks_by_key_not_by_value():
    # A non-volatile field holding a timestamp-looking number is left untouched:
    # the key is the contract, not the value shape.
    body = b'{"deadline_ms":1700000000000,"ts":1}'
    out = normalize_body(body)
    assert out == canonical_json(
        {"deadline_ms": 1700000000000, "ts": VOLATILE_SENTINEL}
    )


def test_normalize_body_is_case_insensitive_on_keys():
    body = b'{"Timestamp":1,"UpTime":2,"keep":3}'
    out = normalize_body(body)
    assert out == canonical_json(
        {"Timestamp": VOLATILE_SENTINEL, "UpTime": VOLATILE_SENTINEL, "keep": 3}
    )


def test_normalize_body_passes_non_json_through_unchanged():
    raw = b"plain text error, not json"
    assert normalize_body(raw) == raw


def test_normalize_body_passes_json_scalar_through_unchanged():
    # A bare scalar has no keys to mask; the raw bytes are already canonical.
    assert normalize_body(b"42") == b"42"
    assert normalize_body(b'"ok"') == b'"ok"'


def test_normalize_body_canonicalizes_equal_objects_with_different_key_order():
    a = normalize_body(b'{"b":1,"a":2}')
    b = normalize_body(b'{"a":2,"b":1}')
    assert a == b


def test_compare_headers_equal_on_allowlisted_match():
    native = {"content-type": "application/json", "date": "Mon"}
    python = {"content-type": "application/json", "date": "Tue", "server": "x"}
    # date/server are dropped; content-type matches → no diffs.
    assert compare_headers(native, python) == []


def test_compare_headers_ignores_content_type_charset_parameter():
    native = {"content-type": "application/json; charset=utf-8"}
    python = {"content-type": "application/json"}
    assert compare_headers(native, python) == []


def test_compare_headers_reports_allowlisted_value_difference():
    native = {"cache-control": "no-store"}
    python = {"cache-control": "no-cache"}
    diffs = compare_headers(native, python)
    assert len(diffs) == 1
    assert "cache-control" in diffs[0]


def test_compare_headers_reports_missing_side():
    native = {"etag": "abc"}
    python = {}
    diffs = compare_headers(native, python)
    assert len(diffs) == 1
    assert "missing on python" in diffs[0]


def test_compare_headers_never_compares_blocked_headers():
    # content-length differs purely from masking; it must never be compared.
    native = {"content-length": "10", "content-type": "application/json"}
    python = {"content-length": "20", "content-type": "application/json"}
    assert compare_headers(native, python) == []


def test_normalize_sse_frames_drops_comment_and_id_lines():
    frames = [
        ":keep-alive\n",  # comment-only frame → dropped entirely
        "id: 7\ndata: hello",  # id stripped, data kept
        "id: 8\ndata: hello",  # same data, different id → identical payload
    ]
    out = normalize_sse_frames(frames)
    assert out == ["data:hello", "data:hello"]


def test_normalize_sse_frames_masks_volatile_in_json_data():
    frames = [
        'data: {"v":1,"ts":100}',
        'data: {"v":1,"ts":200}',
    ]
    out = normalize_sse_frames(frames)
    # Both frames collapse to the same masked payload despite the moving ts.
    assert out[0] == out[1]


def test_normalize_sse_frames_keeps_non_data_lines():
    out = normalize_sse_frames(["event: status\ndata: ok"])
    assert out == ["event: status\ndata:ok"]
