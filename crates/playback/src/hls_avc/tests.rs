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
fn split_delimiter_keeps_first_byte_clock_and_cached_parameter_sets() {
    let (pool, mut reader) = fixture();
    let first = [DELIMITER, PREFIX, SPS, PREFIX, PPS, IDR].concat();
    let next = [DELIMITER, IDR].concat();
    let raw = [first.as_slice(), &next, &next].concat();
    let mut units = Vec::new();
    for (offset, byte) in raw.iter().enumerate() {
        if offset == 0 {
            reader.remember_clock(clock(9000)).unwrap();
        } else if offset == first.len() + 2 {
            reader.remember_clock(clock(12000)).unwrap();
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
fn prefix_anchor_ring_preserves_clocks_for_short_and_long_zero_runs() {
    for zeroes in 2_usize..=9 {
        for switch in 0..=zeroes {
            let (_pool, mut reader) = fixture();
            let first = [DELIMITER, PREFIX, SPS, PREFIX, PPS, IDR].concat();
            let delimiter = [vec![0; zeroes], vec![1, 9, 0xf0]].concat();
            let next = [delimiter.as_slice(), IDR].concat();
            let last = [DELIMITER, IDR].concat();
            let raw = [first.as_slice(), &next, &last].concat();
            let mut units = Vec::new();
            reader.remember_clock(clock(9000)).unwrap();
            for (offset, byte) in raw.iter().enumerate() {
                if offset == first.len() + switch {
                    reader.remember_clock(clock(12000)).unwrap();
                }
                push_span(&mut reader, std::slice::from_ref(byte));
                drain(&mut reader, &mut units);
            }
            reader.finish();
            drain(&mut reader, &mut units);
            let second_has_new_anchor = switch <= zeroes.saturating_sub(3);
            assert_eq!(units.len(), 3);
            assert_eq!(units[0].clock, Some(clock(9000)));
            assert_eq!(units[1].clock, second_has_new_anchor.then(|| clock(12000)));
            assert_eq!(
                units[2].clock,
                (!second_has_new_anchor).then(|| clock(12000))
            );
        }
    }
}

#[test]
fn prefix_clock_history_survives_a_new_pes_on_every_byte() {
    for zeroes in 2_usize..=9 {
        let (_pool, mut reader) = fixture();
        let first = [DELIMITER, PREFIX, SPS, PREFIX, PPS, IDR].concat();
        let delimiter = [vec![0; zeroes], vec![1, 9, 0xf0]].concat();
        let next = [delimiter.as_slice(), IDR].concat();
        let last = [DELIMITER, IDR].concat();
        let raw = [first.as_slice(), &next, &last].concat();
        let mut units = Vec::new();
        reader.remember_clock(clock(9000)).unwrap();
        for (offset, byte) in raw.iter().enumerate() {
            if offset >= first.len() {
                let pts = 12_000 + (offset - first.len()) as u64 * 3000;
                reader.remember_clock(clock(pts)).unwrap();
            }
            push_span(&mut reader, std::slice::from_ref(byte));
            drain(&mut reader, &mut units);
        }
        reader.finish();
        drain(&mut reader, &mut units);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].clock, Some(clock(9000)));
        assert_eq!(
            units[1].clock,
            Some(clock(12_000 + zeroes.saturating_sub(3) as u64 * 3000))
        );
        assert_eq!(
            units[2].clock,
            Some(clock(12_000 + next.len() as u64 * 3000))
        );
    }
}
