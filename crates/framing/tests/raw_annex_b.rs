use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputWindow, SourceCapabilities, SourceFailure, SourceId, Step,
    WorkBudget,
};
use triergon_framing::{Codec, RawAccessUnit, RawAnnexBLimits, RawAnnexBReader};

const ID: SourceId = SourceId(712);
static OFF: AtomicBool = AtomicBool::new(false);

struct Drive<'a> {
    reader: RawAnnexBReader,
    source: &'a Bytes,
    quantum: usize,
}

impl<'a> Drive<'a> {
    fn new(
        pool: &Budget,
        codec: Codec,
        source: &'a Bytes,
        quantum: usize,
        known_length: bool,
        limits: RawAnnexBLimits,
    ) -> Self {
        Self {
            reader: RawAnnexBReader::new(
                pool,
                codec,
                ID,
                SourceCapabilities {
                    length: known_length.then_some(source.len() as u64),
                    random_access: false,
                },
                limits,
            )
            .unwrap(),
            source,
            quantum,
        }
    }

    fn next(&mut self, work: u64) -> Result<Option<RawAccessUnit>, Error> {
        for _ in 0..500_000 {
            let mut budget = WorkBudget::new(work);
            let step = self.reader.step(&mut budget, &OFF);
            assert!(budget.spent() <= work);
            match step? {
                Step::NeedInput(request) => {
                    let start = usize::try_from(request.offset).unwrap();
                    if start == self.source.len() {
                        self.reader.finish(request.source, request.token)?;
                    } else {
                        let end =
                            (start + self.quantum.min(request.maximum)).min(self.source.len());
                        let reply = InputWindow::new(
                            request.source,
                            request.token,
                            request.offset,
                            self.source.slice(start..end)?,
                        )?;
                        self.reader.provide(&reply)?;
                    }
                }
                Step::Yield => {}
                Step::Ready(unit) => return Ok(Some(unit)),
                Step::End(at) => {
                    assert_eq!(at.source, ID);
                    assert_eq!(at.offset, self.source.len() as u64);
                    return Ok(None);
                }
            }
        }
        panic!("raw reader did not advance");
    }
}

fn compare(codec: Codec, input: &[u8], expected: &str) {
    for (quantum, work, known_length) in [(1, 1, false), (7, 31, true), (4096, 4096, true)] {
        let pool = Budget::new(1 << 20).unwrap();
        let source = Bytes::copy_from_slice(&pool, input).unwrap();
        let mut drive = Drive::new(
            &pool,
            codec,
            &source,
            quantum,
            known_length,
            RawAnnexBLimits::default(),
        );
        let mut count = 0;
        for line in expected.lines() {
            let mut fields = line.split_ascii_whitespace();
            let offset: u64 = fields.next().unwrap().parse().unwrap();
            let bytes: u64 = fields.next().unwrap().parse().unwrap();
            let key = fields.next().unwrap() == "1";
            let packet = drive
                .next(work)
                .unwrap()
                .expect("short access-unit sequence");
            assert_eq!(packet.source, ID);
            assert_eq!(packet.codec, codec);
            assert_eq!(
                packet.range,
                offset..offset + bytes,
                "{codec:?} packet {count}"
            );
            assert!(packet.has_vcl);
            assert_eq!(packet.random_access, key);
            assert!(
                packet
                    .segments()
                    .flatten()
                    .copied()
                    .eq(input[offset as usize..(offset + bytes) as usize]
                        .iter()
                        .copied(),),
                "{codec:?} packet {count} wire bytes"
            );
            count += 1;
        }
        assert_eq!(count, 12);
        assert!(drive.next(work).unwrap().is_none());
        drop(drive);
        drop(source);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn hevc_access_unit_wire_matches_the_raw_demuxer_with_and_without_aud() {
    compare(
        Codec::Hevc,
        include_bytes!("../fixtures/raw-annex-b/hevc-aud0.hevc"),
        include_str!("../fixtures/raw-annex-b/hevc-aud0.packets"),
    );
    compare(
        Codec::Hevc,
        include_bytes!("../fixtures/raw-annex-b/hevc-aud1.hevc"),
        include_str!("../fixtures/raw-annex-b/hevc-aud1.packets"),
    );
    // An end of sequence starts the CRA access unit; the end of bitstream
    // ending the stream stays in the last one.
    compare(
        Codec::Hevc,
        include_bytes!("../fixtures/raw-annex-b/hevc-eos.hevc"),
        include_str!("../fixtures/raw-annex-b/hevc-eos.packets"),
    );
}

fn hevc_nal(out: &mut Vec<u8>, zeros: usize, kind: u8, layer: u8, body: u8) {
    out.resize(out.len() + zeros, 0);
    out.extend([1, (kind << 1) | (layer >> 5), (layer << 3) | 1, body]);
}

fn authored_hevc() -> (Vec<u8>, usize) {
    let mut bytes = Vec::new();
    hevc_nal(&mut bytes, 5, 32, 0, 0x55); // VPS and leading zero bytes
    hevc_nal(&mut bytes, 2, 19, 0, 0x80); // first base-layer IRAP slice
    hevc_nal(&mut bytes, 2, 19, 0, 0x40); // another slice in the same picture
    hevc_nal(&mut bytes, 3, 40, 0, 0x33); // suffix SEI
    hevc_nal(&mut bytes, 2, 1, 1, 0x80); // a non-base NAL cannot split the AU
    let second = bytes.len();
    hevc_nal(&mut bytes, 3, 36, 0, 0x80); // EOS begins the next AU, as in FFmpeg's parser
    hevc_nal(&mut bytes, 2, 37, 0, 0x80); // EOB
    hevc_nal(&mut bytes, 3, 39, 0, 0x55); // prefix SEI
    hevc_nal(&mut bytes, 2, 1, 0, 0x80);
    bytes.extend([0; 5]);
    (bytes, second)
}

#[test]
fn hevc_retains_prefix_suffix_other_layer_and_padding_at_literal_boundaries() {
    let (input, second) = authored_hevc();
    let pool = Budget::new(1 << 20).unwrap();
    let source = Bytes::copy_from_slice(&pool, &input).unwrap();
    let mut drive = Drive::new(
        &pool,
        Codec::Hevc,
        &source,
        1,
        false,
        RawAnnexBLimits::default(),
    );
    for (range, nals, key) in [(0..second, 5, true), (second..input.len(), 4, false)] {
        let unit = drive.next(1).unwrap().unwrap();
        assert_eq!(unit.range, range.start as u64..range.end as u64);
        assert_eq!(unit.nals().len(), nals);
        assert_eq!(unit.random_access, key);
        assert!(
            unit.segments()
                .flatten()
                .copied()
                .eq(input[range].iter().copied())
        );
    }
    assert!(drive.next(1).unwrap().is_none());
}

#[test]
fn starvation_transport_and_limits_never_publish_a_partial_access_unit() {
    let (input, _) = authored_hevc();
    let pool = Budget::new(1 << 20).unwrap();
    let source = Bytes::copy_from_slice(&pool, &input).unwrap();
    let mut drive = Drive::new(
        &pool,
        Codec::Hevc,
        &source,
        7,
        true,
        RawAnnexBLimits {
            maximum_access_unit_nals: 4,
            ..RawAnnexBLimits::default()
        },
    );
    let limit = Error::Limit {
        requested: 5,
        maximum: 4,
    };
    assert_eq!(drive.next(1).unwrap_err(), limit);
    assert_eq!(drive.next(1).unwrap_err(), limit);
    drop(drive);

    let mut reader = Drive::new(
        &pool,
        Codec::Hevc,
        &source,
        7,
        false,
        RawAnnexBLimits::default(),
    )
    .reader;
    let request = reader.request();
    let empty = InputWindow::new(
        ID,
        request.token,
        0,
        Bytes::copy_from_slice(&pool, &[]).unwrap(),
    )
    .unwrap();
    reader.provide(&empty).unwrap();
    let Step::NeedInput(request) = reader.step(&mut WorkBudget::new(9), &OFF).unwrap() else {
        panic!("empty transport window did not preserve starvation");
    };
    reader
        .fail(ID, request.token, SourceFailure::Transport)
        .unwrap();
    assert_eq!(
        reader.step(&mut WorkBudget::new(9), &OFF).unwrap_err(),
        Error::Transport
    );

    let mut reader = Drive::new(
        &pool,
        Codec::Hevc,
        &source,
        7,
        true,
        RawAnnexBLimits::default(),
    )
    .reader;
    assert_eq!(
        reader.finish(ID, reader.request().token).unwrap_err(),
        Error::Truncated
    );
    assert_eq!(
        reader.step(&mut WorkBudget::new(9), &OFF).unwrap_err(),
        Error::Truncated
    );
}

#[test]
fn two_slice_avc_pictures_match_the_raw_demuxer_with_and_without_aud() {
    compare(
        Codec::Avc,
        include_bytes!("../fixtures/raw-annex-b/avc-aud0.h264"),
        include_str!("../fixtures/raw-annex-b/avc-aud0.packets"),
    );
    compare(
        Codec::Avc,
        include_bytes!("../fixtures/raw-annex-b/avc-aud1.h264"),
        include_str!("../fixtures/raw-annex-b/avc-aud1.packets"),
    );
}

#[test]
fn raw_nal_backpressure_retries_the_exact_source_bytes() {
    let (input, second) = authored_hevc();
    let pool = Budget::new(8192).unwrap();
    let source = Bytes::copy_from_slice(&pool, &input).unwrap();
    let mut drive = Drive::new(
        &pool,
        Codec::Hevc,
        &source,
        7,
        false,
        RawAnnexBLimits::default(),
    );
    let remaining = pool.limit() - pool.used();
    let held = pool.reserve(remaining).unwrap();
    assert!(matches!(drive.next(5), Err(Error::Backpressure { .. })));
    drop(held);

    for range in [0..second, second..input.len()] {
        let packet = drive.next(1).unwrap().unwrap();
        assert_eq!(packet.range, range.start as u64..range.end as u64);
        assert!(
            packet
                .segments()
                .flatten()
                .copied()
                .eq(input[range].iter().copied())
        );
    }
    assert!(drive.next(1).unwrap().is_none());
}
