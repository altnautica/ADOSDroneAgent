# H.264 test fixtures

Synthetic H.264 byte streams used by the `ados-video` integration test
suite. Each file is valid Annex-B framing — start codes (`00 00 00 01`)
delimiting NAL units whose header byte carries the right NAL type — but
the SPS / PPS bodies are minimal and the slice payloads are filler bytes
(`0xAA` / `0xBB`). No external H.264 decoder will produce a picture
from these fixtures; they exist only to exercise the Annex-B scanner
and the RTP packetizer.

## Files

| File | Purpose | Size |
|---|---|---|
| `nal_small.h264` | SPS + PPS + one IDR slice, all under MTU. Single-NAL RTP packets, no FU-A. | ~1.5 KiB |
| `nal_large.h264` | SPS + PPS + one IDR slice ≥ 4× MTU. Forces FU-A fragmentation. | ~6 KiB |
| `nal_split.h264` | 60-frame stream at 30 fps with two GOPs. Alternates SPS/PPS/IDR (frame 0, 30) and non-IDR (everything else). All small NALs. | ~9 KiB |

## Regenerating

The bytes are produced deterministically by the `fixtures_gen` test
target. From the workspace root:

```sh
cargo test -p ados-video --release --test fixtures_gen \
    -- --ignored --nocapture
```

The `#[ignore]` attribute keeps the generator out of the default test
run, so the fixtures stay reproducible from version-controlled source
without writing to the filesystem on every `cargo test` invocation.

## NAL unit reference

- Type 1: non-IDR slice
- Type 5: IDR slice
- Type 7: SPS (sequence parameter set)
- Type 8: PPS (picture parameter set)
- Type 28: FU-A (fragmentation unit, see RFC 3984 §5.8)

The NAL unit type lives in the low 5 bits of the byte immediately after
the start code; the high 3 bits are `forbidden_zero_bit` (1 bit) and
`nal_ref_idc` (2 bits). See ITU-T Rec. H.264 §7.3.1 for the full spec.

## Format references

- [ITU-T Rec. H.264 / ISO/IEC 14496-10](https://www.itu.int/rec/T-REC-H.264) — Annex B byte-stream format, NAL unit header layout (§7.3.1, §7.4.1).
- [RFC 3984](https://www.rfc-editor.org/rfc/rfc3984) — RTP payload format for H.264, including the FU-A fragmentation rules used by the round-trip test.
