//! Resource/work and cancellation regressions for retained typed operations.
use std::sync::atomic::AtomicBool;
use triergon_core::Step;
use triergon_core::{Budget, Bytes, InputWindow, SourceCapabilities, SourceId, WorkBudget};
use triergon_matroska::{
    Error, Limits,
    incremental::ParseStep,
    storage::SharedBytes,
    typed::{Event, Reader},
    zlib::{InflateLimits, Inflater},
};
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    used: usize,
}
impl Bits {
    fn put(&mut self, value: u32, width: usize) {
        for bit in 0..width {
            if self.used.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let index = self.used / 8;
            self.bytes[index] |= (((value >> bit) & 1) as u8) << (self.used % 8);
            self.used += 1;
        }
    }
    fn fixed(&mut self, symbol: u32) {
        let (code, width) = match symbol {
            0..=143 => (symbol + 0x30, 8),
            144..=255 => (symbol - 144 + 0x190, 9),
            256..=279 => (symbol - 256, 7),
            280..=287 => (symbol - 280 + 0xc0, 8),
            _ => panic!("authored fixed symbol"),
        };
        self.put(code.reverse_bits() >> (32 - width), width);
    }
}
fn repeated(repeats: usize) -> (Vec<u8>, Vec<u8>) {
    let mut bits = Bits::default();
    bits.put(1, 1);
    bits.put(1, 2);
    bits.fixed(u32::from(b'a'));
    for _ in 0..repeats {
        bits.fixed(285);
        bits.put(0, 5);
    }
    bits.fixed(256);
    let expected = vec![b'a'; 1 + 258 * repeats];
    let (mut a, mut b) = (1_u32, 0_u32);
    for byte in &expected {
        a = (a + u32::from(*byte)) % 65521;
        b = (b + a) % 65521;
    }
    let mut source = vec![120, 1];
    source.extend(bits.bytes);
    source.extend(((b << 16) | a).to_be_bytes());
    (source, expected)
}
fn e(id: u64, data: &[u8]) -> Vec<u8> {
    let mut out: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    let width = (1..=8)
        .find(|w| (data.len() as u64) < (1_u64 << (7 * w)) - 1)
        .unwrap();
    out.extend(((1_u64 << (7 * width)) | data.len() as u64).to_be_bytes()[8 - width..].iter());
    out.extend(data);
    out
}

#[test]
fn inflater_growth_retains_both_allocations_and_yields_before_copying_the_live_prefix() {
    let (raw, expected) = repeated(1);
    let pool = Budget::new(64 * 1024).unwrap();
    let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
    let mut decoder = Inflater::new(
        source,
        InflateLimits {
            output_bytes: 4096,
            expansion_ratio: 1024,
            work: 1_000_000,
        },
        &pool,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    for _ in 0..20_000 {
        if decoder.output_len() == 256 {
            break;
        }
        assert!(matches!(
            decoder.step(&mut WorkBudget::new(1), &cancel).unwrap(),
            ParseStep::Yield
        ));
    }
    assert_eq!(decoder.output_len(), 256);
    let before = pool.used();
    let mut work = WorkBudget::new(1);
    assert!(matches!(
        decoder.step(&mut work, &cancel).unwrap(),
        ParseStep::Yield
    ));
    assert!(work.spent() <= 1);
    assert_eq!(
        decoder.output_len(),
        256,
        "one allocation step must not synchronously copy the old output and publish another byte"
    );
    assert!(
        pool.used() >= before + 512,
        "old and replacement allocations must stay charged while the growth copy is pending"
    );
    let mut result = None;
    for _ in 0..20_000 {
        match decoder.step(&mut WorkBudget::new(1), &cancel).unwrap() {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                result = Some(value);
                break;
            }
            other => panic!("bounded growth must finish: {other:?}"),
        }
    }
    assert_eq!(result.unwrap().as_ref(), expected.as_slice());
}

#[test]
fn an_atomic_frame_larger_than_its_own_pool_minimum_is_a_permanent_limit_not_an_endless_wait() {
    let (compressed, _) = repeated(128);
    let encoding = e(
        0x6d80,
        &e(
            0x6240,
            &[e(0x5032, &[1]), e(0x5034, &e(0x4254, &[0]))].concat(),
        ),
    );
    let track = e(
        0xae,
        &[
            e(0xd7, &[1]),
            e(0x73c5, &[1]),
            e(0x83, &[1]),
            e(0x86, b"V_AUTHORED/OPAQUE"),
            encoding,
        ]
        .concat(),
    );
    let block = [&[0x81, 0, 0, 0x80][..], compressed.as_slice()].concat();
    let raw = [
        e(0x1a45dfa3, &e(0x4282, b"matroska")),
        e(
            0x18538067,
            &[
                e(0x1654ae6b, &track),
                e(0x1f43b675, &[e(0xe7, &[0]), e(0xa3, &block)].concat()),
            ]
            .concat(),
        ),
    ]
    .concat();
    let pool = Budget::new(4096).unwrap();
    let mut reader = Reader::new(
        &pool,
        SourceId(202),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        Limits::default(),
        1,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut limit = None;
    let mut pressure = 0;
    for _ in 0..20_000 {
        let mut work = WorkBudget::new(64);
        match reader.step(&mut work, &cancel) {
            Ok(Step::Yield) => (),
            Ok(Step::Ready(Event::Packet(_))) => {
                panic!("the complete decoded frame cannot fit this pool")
            }
            Ok(Step::Ready(_)) => (),
            Ok(Step::NeedInput(request)) => {
                let at = request.offset as usize;
                if at == raw.len() {
                    reader.finish(request.source, request.token).unwrap();
                } else {
                    let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                    reader
                        .provide(
                            &InputWindow::new(request.source, request.token, request.offset, bytes)
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
            Ok(Step::Backpressure(_)) => pressure += 1,
            Err(Error::Policy(message)) => {
                limit = Some(message);
                break;
            }
            other => panic!("valid compressed source must retain failure distinctions: {other:?}"),
        }
        assert!(work.spent() <= 64);
    }
    assert_eq!(
        limit,
        Some("operation retained storage"),
        "no caller retains a packet/configuration or reservation; repeated own-storage pressure ({pressure} polls) must be permanent"
    );
}

#[test]
fn cancelling_an_incomplete_growth_releases_both_capacities_without_a_partial_event() {
    let (raw, _) = repeated(128);
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
    let mut decoder = Inflater::new(
        source,
        InflateLimits {
            output_bytes: 64 * 1024,
            expansion_ratio: 1024,
            work: 1_000_000,
        },
        &pool,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut pending = false;
    for _ in 0..100_000 {
        let len = decoder.output_len();
        let before = pool.used();
        let mut work = WorkBudget::new(1);
        let result = decoder.step(&mut work, &cancel).unwrap();
        assert!(work.spent() <= 1);
        assert!(matches!(result, ParseStep::Yield));
        if len > 2048 && decoder.output_len() == len && pool.used() > before {
            pending = true;
            break;
        }
    }
    assert!(
        pending,
        "the authored stream must pause with old and replacement buffers live"
    );
    let mut work = WorkBudget::new(1);
    assert!(matches!(
        decoder.step(&mut work, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(work.spent(), 0);
    assert!(matches!(
        decoder.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Invalid(_))
    ));
    drop(decoder);
    assert_eq!(pool.used(), baseline);
}
