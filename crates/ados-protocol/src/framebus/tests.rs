use super::*;

#[test]
fn mono_ns_is_monotonic() {
    let a = mono_ns();
    let b = mono_ns();
    assert!(a > 0 && b >= a);
}

#[test]
fn frame_bytes_matches_format() {
    assert_eq!(FrameFormat::Rgb24.frame_bytes(640, 480), 640 * 480 * 3);
    assert_eq!(FrameFormat::Nv12.frame_bytes(640, 480), 640 * 480 * 3 / 2);
    assert_eq!(
        FrameFormat::Yuv420p.frame_bytes(1280, 720),
        1280 * 720 * 3 / 2
    );
    assert_eq!(FrameFormat::Gray8.frame_bytes(640, 480), 640 * 480);
    assert_eq!(FrameFormat::Gray16.frame_bytes(640, 480), 640 * 480 * 2);
    assert_eq!(FrameFormat::BayerRggb8.frame_bytes(641, 481), 641 * 481);
    assert_eq!(
        FrameFormat::BayerRggb16.frame_bytes(640, 480),
        640 * 480 * 2
    );
}

#[test]
fn descriptor_round_trips_through_msgpack() {
    let base = FrameDescriptor {
        v: FRAMEBUS_DESCRIPTOR_VERSION,
        camera_id: "uvc-0".into(),
        frame_id: 42,
        ts_ns: 1_700_000_000_000_000_000,
        ts_sensor_ns: None,
        exposure_ns: None,
        trigger_seq: None,
        timebase: TIMEBASE_HOST,
        width: 640,
        height: 480,
        format: FrameFormat::Rgb24,
        shm_name: "ados-vision-uvc-0".into(),
        slot: 3,
        seq: 1234,
        byte_len: (640 * 480 * 3) as u32,
    };
    let bytes = base.to_msgpack().unwrap();
    assert_eq!(FrameDescriptor::from_msgpack(&bytes).unwrap(), base);
    for format in [
        FrameFormat::Gray8,
        FrameFormat::Gray16,
        FrameFormat::BayerRggb8,
        FrameFormat::BayerRggb16,
    ] {
        let d = FrameDescriptor {
            ts_sensor_ns: Some(123_456_789),
            exposure_ns: Some(4_000_000),
            trigger_seq: Some(77),
            timebase: TIMEBASE_SENSOR,
            format,
            byte_len: format.frame_bytes(640, 480) as u32,
            ..base.clone()
        };
        let bytes = d.to_msgpack().unwrap();
        assert_eq!(FrameDescriptor::from_msgpack(&bytes).unwrap(), d);
    }
}

#[test]
fn ring_version_matches_registry() {
    assert_eq!(
        RingLayout::VERSION,
        crate::contracts::contract_version("framebus.ring").unwrap()
    );
}

#[test]
fn descriptor_rejects_a_future_version() {
    // A descriptor stamped with a version this build does not speak must
    // fail the decode loudly rather than silently mis-parse.
    let d = FrameDescriptor {
        v: FRAMEBUS_DESCRIPTOR_VERSION + 1,
        camera_id: "uvc-0".into(),
        frame_id: 1,
        ts_ns: 0,
        ts_sensor_ns: None,
        exposure_ns: None,
        trigger_seq: None,
        timebase: TIMEBASE_HOST,
        width: 8,
        height: 8,
        format: FrameFormat::Rgb24,
        shm_name: "ados-vision-uvc-0".into(),
        slot: 0,
        seq: 1,
        byte_len: 192,
    };
    let bytes = d.to_msgpack().unwrap();
    assert!(matches!(
        FrameDescriptor::from_msgpack(&bytes),
        Err(DescriptorError::Version { .. })
    ));
}

#[test]
fn descriptor_v1_with_ts_ms_is_rejected() {
    // A v1 descriptor (millisecond `ts_ms`, no `ts_ns`) must not decode.
    #[derive(Serialize)]
    struct V1Descriptor {
        v: u16,
        camera_id: String,
        frame_id: u64,
        ts_ms: i64,
        width: u32,
        height: u32,
        format: FrameFormat,
        shm_name: String,
        slot: u32,
        seq: u64,
        byte_len: u32,
    }
    let old = V1Descriptor {
        v: 1,
        camera_id: "uvc-0".into(),
        frame_id: 1,
        ts_ms: 0,
        width: 8,
        height: 8,
        format: FrameFormat::Rgb24,
        shm_name: "ados-vision-uvc-0".into(),
        slot: 0,
        seq: 1,
        byte_len: 192,
    };
    let bytes = rmp_serde::to_vec_named(&old).unwrap();
    assert!(matches!(
        FrameDescriptor::from_msgpack(&bytes),
        Err(DescriptorError::Decode(_) | DescriptorError::Version { .. })
    ));
}

#[test]
fn format_serializes_lowercase() {
    // The Python reader matches on the lowercase string.
    let bytes = rmp_serde::to_vec_named(&FrameFormat::Yuv420p).unwrap();
    let back: String = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back, "yuv420p");
    for (f, name) in [
        (FrameFormat::Gray8, "gray8"),
        (FrameFormat::Gray16, "gray16"),
        (FrameFormat::BayerRggb8, "bayerrggb8"),
        (FrameFormat::BayerRggb16, "bayerrggb16"),
    ] {
        let bytes = rmp_serde::to_vec_named(&f).unwrap();
        let back: String = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, name);
    }
}

#[test]
fn header_round_trips() {
    let layout = RingLayout::for_frame(4, 64, 48, FrameFormat::Rgb24);
    let mut region = vec![0u8; layout.total_len()];
    layout.write_header(&mut region).unwrap();
    let read = RingLayout::read_header(&region).unwrap();
    assert_eq!(read, layout);
}

#[test]
fn write_then_read_returns_the_frame() {
    let layout = RingLayout::for_frame(4, 8, 8, FrameFormat::Rgb24); // 192-byte slots
    let mut region = vec![0u8; layout.total_len()];
    layout.write_header(&mut region).unwrap();

    let frame: Vec<u8> = (0..layout.slot_bytes as u8).collect();
    let seq = 9u64;
    let slot = (seq % layout.slot_count as u64) as u32;
    write_slot(&mut region, &layout, slot, seq, &frame).unwrap();

    let mut out = Vec::new();
    let got = read_slot_into(&region, &layout, slot, seq, &mut out).unwrap();
    assert_eq!(got, Some(frame.len()));
    assert_eq!(out, frame);
}

#[test]
fn read_slot_into_reuses_capacity() {
    let layout = RingLayout::for_frame(2, 8, 8, FrameFormat::Rgb24);
    let mut region = vec![0u8; layout.total_len()];
    layout.write_header(&mut region).unwrap();
    let mut out = Vec::with_capacity(layout.slot_bytes as usize);
    out.extend_from_slice(&[0xAA; 5]);
    let (ptr, cap) = (out.as_ptr(), out.capacity());
    for seq in 1..=4u64 {
        let frame = vec![seq as u8; layout.slot_bytes as usize];
        let slot = (seq % 2) as u32;
        write_slot(&mut region, &layout, slot, seq, &frame).unwrap();
        let n = read_slot_into(&region, &layout, slot, seq, &mut out).unwrap();
        assert_eq!(n, Some(frame.len()));
        assert_eq!(out, frame);
        assert_eq!(out.as_ptr(), ptr);
        assert_eq!(out.capacity(), cap);
    }
}

#[test]
fn stale_seq_reads_none() {
    let layout = RingLayout::for_frame(2, 4, 4, FrameFormat::Rgb24);
    let mut region = vec![0u8; layout.total_len()];
    layout.write_header(&mut region).unwrap();
    write_slot(&mut region, &layout, 0, 10, &[1, 2, 3]).unwrap();
    // The consumer asks for a sequence the slot never held.
    let mut out = Vec::new();
    assert_eq!(
        read_slot_into(&region, &layout, 0, 11, &mut out).unwrap(),
        None
    );
}

#[test]
fn recycled_slot_is_detected_as_torn() {
    let layout = RingLayout::for_frame(1, 4, 4, FrameFormat::Rgb24);
    let mut region = vec![0u8; layout.total_len()];
    layout.write_header(&mut region).unwrap();
    write_slot(&mut region, &layout, 0, 100, &[7, 7, 7, 7]).unwrap();
    // Writer recycles the single slot for the next frame.
    write_slot(&mut region, &layout, 0, 101, &[8, 8, 8, 8]).unwrap();
    // A consumer still holding the old descriptor sees the new seq and drops.
    let mut out = Vec::new();
    assert_eq!(
        read_slot_into(&region, &layout, 0, 100, &mut out).unwrap(),
        None
    );
    assert_eq!(
        read_slot_into(&region, &layout, 0, 101, &mut out).unwrap(),
        Some(4)
    );
    assert_eq!(out, vec![8; 4]);
}

#[test]
fn oversized_payload_rejected() {
    let layout = RingLayout::for_frame(2, 2, 2, FrameFormat::Rgb24); // 12-byte slots
    let mut region = vec![0u8; layout.total_len()];
    layout.write_header(&mut region).unwrap();
    let too_big = vec![0u8; layout.slot_bytes as usize + 1];
    assert!(matches!(
        write_slot(&mut region, &layout, 0, 1, &too_big),
        Err(RingError::PayloadTooLarge { .. })
    ));
}

#[test]
fn slot_out_of_range_rejected() {
    let layout = RingLayout::for_frame(2, 2, 2, FrameFormat::Rgb24);
    let mut region = vec![0u8; layout.total_len()];
    layout.write_header(&mut region).unwrap();
    assert!(matches!(
        write_slot(&mut region, &layout, 2, 1, &[0]),
        Err(RingError::SlotOutOfRange { .. })
    ));
}

#[test]
fn small_region_rejected() {
    let layout = RingLayout::for_frame(4, 64, 64, FrameFormat::Rgb24);
    let mut tiny = vec![0u8; 8];
    assert!(matches!(
        layout.write_header(&mut tiny),
        Err(RingError::RegionTooSmall { .. })
    ));
}

#[test]
fn slot_count_at_the_header_maximum_round_trips() {
    // u16::MAX is the largest count the two-byte header field carries; it
    // must validate and round-trip exactly (no truncation).
    let layout = RingLayout {
        slot_count: MAX_SLOT_COUNT,
        slot_bytes: 4,
    };
    assert!(layout.validate().is_ok());
    // The header is only HEADER_LEN bytes; write it into a region sized to
    // just the header so the round-trip exercises the field, not the slots.
    let mut header_region = vec![0u8; layout.total_len()];
    layout.write_header(&mut header_region).unwrap();
    let read = RingLayout::read_header(&header_region).unwrap();
    assert_eq!(read.slot_count, MAX_SLOT_COUNT);
}

#[test]
fn slot_count_above_the_header_maximum_is_rejected() {
    // A slot_count that the u16 header field would truncate is refused at
    // header-write time rather than silently wrapping to a smaller ring.
    let layout = RingLayout {
        slot_count: MAX_SLOT_COUNT + 1,
        slot_bytes: 4,
    };
    assert!(matches!(
        layout.validate(),
        Err(RingError::SlotCountTooLarge { .. })
    ));
    let mut region = vec![0u8; 64];
    assert!(matches!(
        layout.write_header(&mut region),
        Err(RingError::SlotCountTooLarge { .. })
    ));
}

#[test]
fn concurrent_writer_and_reader_never_yield_a_torn_frame() {
    // A genuine cross-thread seqlock stress: one writer recycles a small
    // ring as fast as it can while a reader copies the slot the latest
    // descriptor named. Every successful read must be an internally
    // consistent frame (all bytes equal the frame's marker), never a torn
    // mix of two frames. The volatile guards + Acquire/Release fences are
    // what make this hold on a weakly-ordered CPU.
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as O};
    use std::sync::Arc;

    const SLOTS: u32 = 3;
    const FRAME: usize = 256;
    let layout = RingLayout {
        slot_count: SLOTS,
        slot_bytes: FRAME as u32,
    };
    let total = layout.total_len();

    // The shared region behind an UnsafeCell-equivalent: a raw buffer the
    // writer mutates and the reader reads, exactly the /dev/shm aliasing the
    // seqlock is designed for. A Mutex would serialize the two and defeat
    // the test, so a raw pointer wrapper carries the aliasing explicitly.
    struct Shared(*mut u8, usize);
    unsafe impl Send for Shared {}
    unsafe impl Sync for Shared {}
    let mut backing = vec![0u8; total];
    layout.write_header(&mut backing).unwrap();
    let ptr = backing.as_mut_ptr();
    let shared = Arc::new(Shared(ptr, total));

    // The latest committed seq the reader chases.
    let latest = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let w_shared = shared.clone();
    let w_latest = latest.clone();
    let w_stop = stop.clone();
    let writer = std::thread::spawn(move || {
        // SAFETY: the writer is the single mutator of the region for the
        // test's lifetime; the reader only reads. This mirrors the
        // single-writer / many-reader ring contract.
        let region = unsafe { std::slice::from_raw_parts_mut(w_shared.0, w_shared.1) };
        let mut seq = 1u64;
        while !w_stop.load(O::Relaxed) {
            let marker = (seq & 0xFF) as u8;
            let frame = vec![marker; FRAME];
            let slot = (seq % SLOTS as u64) as u32;
            write_slot(region, &layout, slot, seq, &frame).unwrap();
            w_latest.store(seq, O::Release);
            seq += 1;
        }
    });

    let r_shared = shared.clone();
    let r_latest = latest.clone();
    let mut reads = 0u64;
    // Read until a healthy number of committed frames have been observed,
    // bounded so a wedged writer cannot spin forever. A fixed iteration
    // budget is flaky under loaded CI scheduling: a fast reader can drain
    // its whole budget before a starved writer commits its first frame,
    // a test-timing artifact rather than a seqlock failure.
    let mut iters = 0u64;
    let mut data = Vec::new();
    while reads < 10_000 && iters < 50_000_000 {
        iters += 1;
        let seq = r_latest.load(O::Acquire);
        if seq == 0 {
            std::hint::spin_loop();
            continue;
        }
        let slot = (seq % SLOTS as u64) as u32;
        // SAFETY: read-only view of the same region the writer mutates; the
        // seqlock discards any read torn by a concurrent recycle.
        let region = unsafe { std::slice::from_raw_parts(r_shared.0, r_shared.1) };
        if read_slot_into(region, &layout, slot, seq, &mut data)
            .unwrap()
            .is_some()
        {
            reads += 1;
            let marker = (seq & 0xFF) as u8;
            assert!(
                data.iter().all(|&b| b == marker),
                "torn read at seq {seq}: expected all {marker:#x}"
            );
        }
    }
    stop.store(true, O::Relaxed);
    writer.join().unwrap();
    // The reader saw at least some committed frames (not a vacuous pass).
    assert!(reads > 0, "reader observed no committed frames");
}
