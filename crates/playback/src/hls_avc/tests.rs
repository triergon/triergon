use super::*;

const DELIMITER: &[u8] = &[0, 0, 0, 1, 9, 0xf0];
// RBSP prefixes from the authored bframes-multislice fixture.
const SPS: &[u8] = &[
    0x67, 0x64, 0, 0x0a, 0xac, 0xd9, 0x46, 0x27, 0xe5, 0xc0, 0x44, 0, 0, 3, 0, 4, 0, 0, 3, 0, 0xc8,
    0x3c, 0x48, 0x96, 0x58,
];
const PPS: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0];
const IDR: &[u8] = &[0, 0, 1, 0x65, 0x88, 0x84];

fn fixture() -> (Budget, AvcTsReader) {
    let pool = Budget::new(64 * 1024).unwrap();
    let config = TsNativeConfiguration {
        track: TrackKey::Ts {
            program: 31,
            pid: 256,
        },
        configuration_epoch: 1,
        carriage: TsNativeCarriage::Registered(RegisteredCarriage::Avc),
    };
    let reader = AvcTsReader::new(&pool, config, AvcTsLimits::default()).unwrap();
    (pool, reader)
}

fn clock(pts: u64) -> AvcTsClock {
    AvcTsClock {
        source_pts_raw33: Some(pts),
        source_dts_raw33: None,
        pts90: Some(pts as i64),
        dts90: Some(pts as i64),
    }
}

fn push_span(reader: &mut AvcTsReader, bytes: &[u8]) {
    assert!(reader.input.is_none());
    reader.input = Some(Chunk {
        bytes: Bytes::copy_from_slice(&reader.pool, bytes).unwrap(),
        offset: 0,
    });
}

fn push_pes_span(reader: &mut AvcTsReader, bytes: &[u8]) {
    assert!(reader.pending_pes.is_none());
    push_span(reader, bytes);
    reader.pending_pes = Some(PendingPes {
        spans: BudgetVec::new(&reader.pool).into_iter(),
        remaining: 0,
        may_reserve: true,
    });
}

fn drain(reader: &mut AvcTsReader, out: &mut Vec<AvcTsAccessUnit>) {
    loop {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            AvcTsStep::Yield => {}
            AvcTsStep::Ready(unit) => out.push(unit),
            AvcTsStep::End | AvcTsStep::NeedInput => return,
        }
    }
}

#[test]
fn progressive_keeps_startup_sei_and_requires_the_delimiter_in_that_pes() {
    let (pool, mut strict) = fixture();
    let leading_sei = [
        0, 0, 1, SEI, 5, 16, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 0x80,
    ];
    let delimited_idr = [DELIMITER, PREFIX, SPS, PREFIX, PPS, IDR].concat();
    let first = [leading_sei.as_slice(), &delimited_idr].concat();
    let anchor = Anchor {
        serial: 1,
        clock: clock(9000),
    };
    let mut progressive =
        AvcTsReader::new_progressive(&pool, strict.config, strict.limits).unwrap();
    strict.pes = Some(anchor);
    push_span(&mut strict, &first);
    assert!(matches!(
        strict.step(&mut WorkBudget::new(1024), &AtomicBool::new(false)),
        Err(Error::Unsupported(
            "AVC TS access unit has no initial delimiter"
        ))
    ));

    progressive.pes = Some(anchor);
    let mut units = Vec::new();
    for bytes in first.chunks(3) {
        push_span(&mut progressive, bytes);
        drain(&mut progressive, &mut units);
    }
    assert!(units.is_empty());
    progressive.finish();
    drain(&mut progressive, &mut units);
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].clock, Some(anchor.clock));
    assert_eq!(units[0].source_bytes().as_ref(), first);

    let mut split = AvcTsReader::new_progressive(&pool, strict.config, strict.limits).unwrap();
    split.pes = Some(anchor);
    push_span(&mut split, &leading_sei);
    drain(&mut split, &mut Vec::new());
    split.pes = Some(Anchor {
        serial: 2,
        clock: clock(12000),
    });
    push_span(&mut split, &delimited_idr);
    assert!(matches!(
        split.step(&mut WorkBudget::new(1024), &AtomicBool::new(false)),
        Err(Error::Invalid(
            "AVC TS startup SEI without a delimiter in its PES"
        ))
    ));
}

#[test]
fn split_delimiter_keeps_first_byte_clock_and_cached_parameter_sets() {
    let (pool, mut reader) = fixture();
    let first = [DELIMITER, PREFIX, SPS, PREFIX, PPS, IDR].concat();
    let next = [DELIMITER, IDR].concat();
    let raw = [first.as_slice(), &next, &next].concat();
    let mut units = Vec::new();
    for (offset, byte) in raw.iter().enumerate() {
        if offset == 0 {
            reader.pes = Some(Anchor {
                serial: 1,
                clock: clock(9000),
            });
        } else if offset == first.len() + 2 {
            reader.pes = Some(Anchor {
                serial: 2,
                clock: clock(12000),
            });
        }
        push_span(&mut reader, std::slice::from_ref(byte));
        drain(&mut reader, &mut units);
    }
    reader.finish();
    drain(&mut reader, &mut units);
    assert_eq!(units.len(), 3);
    assert_eq!(units[0].clock, Some(clock(9000)));
    assert_eq!(units[1].clock, None);
    assert_eq!(units[2].clock, Some(clock(12000)));
    assert_eq!(units[0].source_bytes().as_ref(), first);
    assert_eq!(units[1].source_bytes().as_ref(), next);
    assert_eq!(units[2].source_bytes().as_ref(), next);
    let retained = pool.used();
    let contiguous = units[0].contiguous_feed(&pool).unwrap();
    assert!(contiguous.shares_storage_with(units[0].source_bytes()));
    assert_eq!(pool.used(), retained);
    for unit in &units[1..] {
        assert_eq!(
            unit.feed_spans().collect::<Vec<_>>(),
            [PREFIX, SPS, PREFIX, PPS, next.as_slice()]
        );
        let contiguous = unit.contiguous_feed(&pool).unwrap();
        assert_eq!(
            contiguous.as_ref(),
            [PREFIX, SPS, PREFIX, PPS, next.as_slice()].concat()
        );
    }
}

#[test]
fn eof_publication_retries_after_pressure_and_a_partial_next_nal_fails() {
    let (pool, mut reader) = fixture();
    let data = [DELIMITER, PREFIX, SPS, PREFIX, PPS, IDR].concat();
    push_span(&mut reader, &data);
    let mut units = Vec::new();
    drain(&mut reader, &mut units);
    assert!(units.is_empty());
    reader.finish();
    let pressure = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        reader.step(&mut WorkBudget::new(64), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    drop(pressure);
    drain(&mut reader, &mut units);
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].source_bytes().as_ref(), data);

    let (_pool, mut reader) = fixture();
    push_span(&mut reader, &[data.as_slice(), PREFIX].concat());
    drain(&mut reader, &mut Vec::new());
    reader.finish();
    assert!(matches!(
        reader.step(&mut WorkBudget::new(64), &AtomicBool::new(false)),
        Err(Error::Truncated)
    ));
}

#[test]
fn pes_extent_hint_preserves_packed_and_split_units_under_pressure() {
    let headers = [DELIMITER, PREFIX, SPS, PREFIX, PPS, IDR].concat();
    let first = [headers.as_slice(), &[0x55; 40]].concat();
    let second = [DELIMITER, IDR, &[0x66; 70]].concat();
    let (start, tail) = second.split_at(second.len() / 2);
    let packed = [first.as_slice(), start].concat();
    let cancel = AtomicBool::new(false);
    let ordinary_growth = 128;
    let publication = LOOKAHEAD + Bytes::owner_overhead_bytes();

    for headroom in [None, Some(ordinary_growth + publication), Some(0)] {
        let (pool, mut reader) = fixture();
        reader.pes = Some(Anchor {
            serial: 1,
            clock: clock(9000),
        });
        push_pes_span(&mut reader, &packed);
        // Stop after the first slice header. The IDR fixture ends with two
        // payload bytes and the four NAL transitions cost one unit each.
        let mut prefix_work = WorkBudget::new((headers.len() - 2 + 4) as u64);
        assert!(matches!(
            reader.step(&mut prefix_work, &cancel).unwrap(),
            AvcTsStep::Yield
        ));
        let mut pressure = headroom.map(|free| pool.reserve(pool.available() - free).unwrap());
        if headroom == Some(0) {
            let mut retry_work = WorkBudget::new(4096);
            assert!(matches!(
                reader.step(&mut retry_work, &cancel),
                Err(Error::Backpressure { .. })
            ));
            assert_eq!(retry_work.spent(), 0);
            drop(pressure.take());
        }
        let AvcTsStep::Ready(unit) = reader.step(&mut WorkBudget::new(4096), &cancel).unwrap()
        else {
            panic!("the first picture should be ready");
        };
        assert_eq!(unit.source_bytes().as_ref(), first);
        assert_eq!(unit.clock, Some(clock(9000)));
        let capacity = headroom.map_or(packed.len() + LOOKAHEAD, |_| ordinary_growth);
        assert_eq!(unit.source_bytes().retained_capacity(), capacity);
        drop(pressure);

        let mut units = vec![unit];
        drain(&mut reader, &mut units);
        push_pes_span(&mut reader, tail);
        reader.finish();
        drain(&mut reader, &mut units);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].source_bytes().as_ref(), first);
        assert_eq!(units[1].source_bytes().as_ref(), second);
        assert_eq!(units[1].clock, None);
    }
}

#[test]
fn nal_body_scan_keeps_possible_prefixes_at_every_input_boundary() {
    for len in 0..=34 {
        for first in 0..=len {
            for second in first..=len {
                let mut bytes = [1; 35];
                bytes[first] = 0;
                bytes[second] = 0;
                let input = &bytes[..len];
                let expected = input
                    .windows(2)
                    .position(|pair| pair == [0, 0])
                    .unwrap_or_else(|| len - usize::from(input.last() == Some(&0)));
                assert_eq!(
                    nal_body_prefix_len(input),
                    expected,
                    "{len}, {first}, {second}"
                );
            }
        }
    }
}
