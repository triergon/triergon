//! SOURCE-ONLY independent source controls; no native outcome predictions.
#[path = "../examples/support/dts_discovery.rs"]
mod discovery;
use discovery::{Discovered, PackingProbe, ProbeStep};
use std::sync::atomic::AtomicBool;
use triergon_core::{Error, WorkBudget};
use triergon_framing::DtsPacking as P;

fn source_prefix(packing: P, high: [u16; 3]) -> Vec<u8> {
    match packing {
        P::CanonicalBe => vec![0x7f, 0xfe, 0x80, 0x01],
        P::Words16Le => vec![0xfe, 0x7f, 0x01, 0x80],
        P::Words14Be | P::Words14Le => {
            // Primary SYNC32 + normal6 + actual CPF0 and three block-count bits.
            let bits = (0x7ffe8001_u64 << 10) | 0x3f0;
            let mut bytes = Vec::new();
            for (n, high_bits) in high.into_iter().enumerate() {
                assert!(high_bits < 4);
                let word = (((bits >> (28 - 14 * n)) & 0x3fff) as u16) | (high_bits << 14);
                bytes.extend_from_slice(&if packing == P::Words14Be {
                    word.to_be_bytes()
                } else {
                    word.to_le_bytes()
                });
            }
            bytes
        }
    }
}

fn run(source: &[u8], chunk: usize, quantum: u64, limit: u64) -> Result<Option<Discovered>, Error> {
    let mut probe = PackingProbe::new(limit)?;
    let mut at = 0;
    let mut end = source.len().min(chunk);
    for _ in 0..(source.len() * 16 + 128) {
        let (consumed, step) = probe.feed(
            &source[at..end],
            end == source.len(),
            &mut WorkBudget::new(quantum),
            &AtomicBool::new(false),
        )?;
        assert!(consumed <= end - at);
        at += consumed;
        assert_eq!(probe.inspected_bytes(), at as u64);
        match step {
            ProbeStep::Found(value) => {
                assert_eq!(value.inspected_end, at as u64);
                assert_eq!(
                    &source[value.source_offset as usize..at],
                    &value.prefix[..value.prefix_bytes]
                );
                assert!(matches!(
                    probe.feed(&[], true, &mut WorkBudget::new(1), &AtomicBool::new(false))?,
                    (0, ProbeStep::End)
                ));
                return Ok(Some(value));
            }
            ProbeStep::SourceEnd { inspected_bytes } => {
                assert_eq!(at, source.len());
                assert_eq!(inspected_bytes, source.len() as u64);
                return Ok(None);
            }
            ProbeStep::NeedInput => {
                assert_eq!(at, end);
                assert!(end < source.len());
                end = source.len().min(end + chunk);
            }
            ProbeStep::Yield => {}
            ProbeStep::End => panic!("terminal before discovery/source end"),
        }
    }
    panic!("discovery bounded progress failure");
}

#[test]
fn all_four_packings_unused_high_bits_offsets_chunks_and_work_are_discovered() {
    let mut controls = Vec::new();
    for packing in [P::CanonicalBe, P::Words16Le] {
        controls.push((packing, source_prefix(packing, [0; 3])));
    }
    for packing in [P::Words14Be, P::Words14Le] {
        for high in 0..64 {
            controls.push((
                packing,
                source_prefix(packing, [high & 3, (high >> 2) & 3, (high >> 4) & 3]),
            ));
        }
    }
    assert_eq!(controls.len(), 130);
    for (packing, prefix) in controls {
        for junk in 0..4 {
            let mut source = vec![0xaa; junk];
            source.extend_from_slice(&prefix);
            for chunk in [1, 2, 3, 7, 13, 64, 4096] {
                for quantum in [1, 31] {
                    let found = run(&source, chunk, quantum, 16384).unwrap().unwrap();
                    assert_eq!(found.packing, packing);
                    assert_eq!(found.source_offset, junk as u64);
                }
            }
        }
    }
}

#[test]
fn unknown_truncated_and_zero_padded_backing_never_create_a_complete_sync() {
    for packing in [P::CanonicalBe, P::Words16Le, P::Words14Be, P::Words14Le] {
        let actual = source_prefix(packing, [0, 3, 0]);
        for size in 0..actual.len() {
            let mut backing = actual.clone();
            backing.extend_from_slice(&[0; 64]);
            // Only the source slice is visible; the remainder and64 physical
            // zero bytes do not complete a32-bit marker or a third14-bit word.
            for chunk in [1, 3, 4096] {
                assert_eq!(run(&backing[..size], chunk, 1, 16384).unwrap(), None);
            }
        }
    }
    for source in [
        vec![],
        vec![0; 19],
        vec![0x7f, 0xfe, 0x80, 0],
        vec![0x64, 0x58, 0x20, 0x25],
    ] {
        assert_eq!(run(&source, 1, 1, 16384).unwrap(), None);
    }
}

#[test]
fn limit_zero_work_and_cancel_are_explicit_without_consuming_unavailable_bytes() {
    let source = source_prefix(P::CanonicalBe, [0; 3]);
    let mut probe = PackingProbe::new(4).unwrap();
    assert_eq!(
        probe
            .feed(
                &source,
                true,
                &mut WorkBudget::new(0),
                &AtomicBool::new(false)
            )
            .unwrap(),
        (0, ProbeStep::Yield)
    );
    assert_eq!(probe.inspected_bytes(), 0);
    assert_eq!(
        run(&source, 1, 1, 3),
        Err(Error::Limit {
            requested: 4,
            maximum: 3
        })
    );
    assert_eq!(
        run(&source, 1, 1, 4).unwrap().unwrap().packing,
        P::CanonicalBe
    );
    let mut probe = PackingProbe::new(4).unwrap();
    assert_eq!(
        probe.feed(
            &source,
            true,
            &mut WorkBudget::new(1),
            &AtomicBool::new(true)
        ),
        Err(Error::Cancelled)
    );
    assert_eq!(
        probe.feed(
            &source,
            true,
            &mut WorkBudget::new(100),
            &AtomicBool::new(false)
        ),
        Err(Error::Cancelled)
    );
    assert_eq!(probe.inspected_bytes(), 0);
    let mut partial = PackingProbe::new(4).unwrap();
    assert_eq!(
        partial
            .feed(
                &source,
                false,
                &mut WorkBudget::new(1),
                &AtomicBool::new(false)
            )
            .unwrap(),
        (1, ProbeStep::Yield)
    );
    assert_eq!(partial.inspected_bytes(), 1);
    assert_eq!(
        partial.feed(
            &source[1..],
            true,
            &mut WorkBudget::new(1),
            &AtomicBool::new(true)
        ),
        Err(Error::Cancelled)
    );
    assert_eq!(
        partial.feed(
            &source[1..],
            true,
            &mut WorkBudget::new(100),
            &AtomicBool::new(false)
        ),
        Err(Error::Cancelled)
    );
    assert_eq!(partial.inspected_bytes(), 1);
    assert!(matches!(PackingProbe::new(0), Err(Error::Invalid(_))));
}
