//! Public blocking results, exact provider calls, pressure and retained owners.
use super::*;
use std::io::{Cursor, SeekFrom};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

fn element(id: u64, body: &[u8]) -> Vec<u8> {
    let mut bytes: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    let width = (1..=8)
        .find(|n| (body.len() as u64) < (1_u64 << (7 * n)) - 1)
        .unwrap();
    bytes.extend_from_slice(
        &((1_u64 << (7 * width)) | body.len() as u64).to_be_bytes()[8 - width..],
    );
    bytes.extend_from_slice(body);
    bytes
}
fn uint(id: u64, value: u64) -> Vec<u8> {
    element(id, &value.to_be_bytes())
}
fn fixture() -> Vec<u8> {
    let track = element(
        0xae,
        &[
            uint(0xd7, 1),
            uint(0x73c5, 1),
            uint(0x83, 1),
            element(0x86, b"V_AUTHORED/OPAQUE"),
            element(0x63a2, b"exact-private"),
            uint(0x23e383, 40_000_000),
        ]
        .concat(),
    );
    let mut body = element(TRACKS, &track);
    for (at, byte) in [(0, b'a'), (40, b'b'), (80, b'c')] {
        let mut block = vec![0x81, 0, 0, 0x80];
        block.extend_from_slice(&[byte; 32]);
        body.extend_from_slice(&element(
            CLUSTER,
            &[uint(0xe7, at), element(0xa3, &block)].concat(),
        ));
    }
    [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(SEGMENT, &body),
    ]
    .concat()
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Read {
        before: u64,
        requested: usize,
        returned: usize,
        used: usize,
    },
    Fault {
        before: u64,
        requested: usize,
        used: usize,
    },
    Seek {
        request: String,
        after: u64,
        used: usize,
    },
}
#[derive(Debug)]
struct Source {
    bytes: Cursor<Vec<u8>>,
    chunk: usize,
    pool: Budget,
    log: Arc<Mutex<Vec<Call>>>,
    mode: Arc<AtomicU8>,
    cancel: Arc<AtomicBool>,
}
impl Read for Source {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let before = self.bytes.position();
        let requested = out.len();
        let mode = self.mode.swap(0, Ordering::Relaxed);
        if mode == 1 || mode == 3 {
            self.cancel.store(true, Ordering::Relaxed);
        }
        if mode == 2 || mode == 3 {
            self.log.lock().unwrap().push(Call::Fault {
                before,
                requested,
                used: self.pool.used(),
            });
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "authored transport",
            ));
        }
        let count = out.len().min(self.chunk);
        let returned = self.bytes.read(&mut out[..count])?;
        self.log.lock().unwrap().push(Call::Read {
            before,
            requested,
            returned,
            used: self.pool.used(),
        });
        Ok(returned)
    }
}
impl Seek for Source {
    fn seek(&mut self, at: SeekFrom) -> std::io::Result<u64> {
        let result = self.bytes.seek(at)?;
        self.log.lock().unwrap().push(Call::Seek {
            request: format!("{at:?}"),
            after: result,
            used: self.pool.used(),
        });
        Ok(result)
    }
}
fn source(
    raw: Vec<u8>,
    chunk: usize,
    pool: &Budget,
    cancel: &Arc<AtomicBool>,
) -> (Source, Arc<Mutex<Vec<Call>>>, Arc<AtomicU8>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mode = Arc::new(AtomicU8::new(0));
    (
        Source {
            bytes: Cursor::new(raw),
            chunk,
            pool: pool.clone(),
            log: log.clone(),
            mode: mode.clone(),
            cancel: cancel.clone(),
        },
        log,
        mode,
    )
}
fn facts(packet: &crate::typed::Packet) -> (Option<i64>, Vec<u8>, Vec<u8>) {
    (
        packet.packet.pts_ns,
        packet.packet.data.as_ref().to_vec(),
        packet.configuration.tracks()[0]
            .codec_private
            .as_ref()
            .to_vec(),
    )
}
fn policy_cases() -> [crate::typed::PayloadOwnership; 2] {
    [
        crate::typed::PayloadOwnership::OwnedExact,
        crate::typed::PayloadOwnership::SharedBounded,
    ]
}
#[test]
fn seek_window_control_fragmented_packets_and_retained_parent_lifetimes() {
    for policy in policy_cases() {
        for chunk in [1, 7, 8192] {
            let pool = Budget::new(512 * 1024).unwrap();
            let base = pool.used();
            let cancel = Arc::new(AtomicBool::new(false));
            let (source, log, _) = source(fixture(), chunk, &pool, &cancel);
            let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
                source,
                Limits::default(),
                &pool,
                policy,
            )
            .unwrap();
            let mut held = Vec::new();
            while let Some(packet) = reader.next_typed_packet(&cancel).unwrap() {
                held.push(packet);
            }
            let actual: Vec<_> = held.iter().map(facts).collect();
            assert_eq!(
                actual,
                [(0, b'a'), (40_000_000, b'b'), (80_000_000, b'c')].map(|(at, byte)| (
                    Some(at),
                    vec![byte; 32],
                    b"exact-private".to_vec()
                ))
            );
            let calls = log.lock().unwrap().clone();
            drop(reader);
            assert!(pool.used() > base);
            assert_eq!(held.iter().map(facts).collect::<Vec<_>>(), actual);
            drop(held);
            assert_eq!(pool.used(), base);
            println!("SEEK_WINDOW_CONTROL plain-{policy:?}-{chunk} {calls:?} {actual:?}");
        }
    }
}
#[test]
fn seek_window_control_reentrant_seeks_keep_complete_receipts_and_provider_order() {
    for policy in policy_cases() {
        for chunk in [1, 7, 8192] {
            let pool = Budget::new(512 * 1024).unwrap();
            let base = pool.used();
            let cancel = Arc::new(AtomicBool::new(false));
            let (source, log, _) = source(fixture(), chunk, &pool, &cancel);
            let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
                source,
                Limits::default(),
                &pool,
                policy,
            )
            .unwrap();
            let held = reader.next_typed_packet(&cancel).unwrap().unwrap();
            let mut observations = Vec::new();
            for target in [40_000_000, 0, 80_000_000] {
                let receipt = reader.seek_for_playback(0, target, &cancel).unwrap();
                assert_eq!(receipt.actual_ns, target);
                let mut packets = Vec::new();
                while let Some(packet) = reader.next_typed_packet(&cancel).unwrap() {
                    packets.push(facts(&packet));
                }
                assert_eq!(packets.first().unwrap().0, Some(target));
                assert_eq!(packets.last().unwrap().0, Some(80_000_000));
                observations.push(format!("{receipt:?}:{packets:?}:used={}", pool.used()));
            }
            let calls = log.lock().unwrap().clone();
            drop(reader);
            assert_eq!(held.packet.data.as_ref(), &[b'a'; 32]);
            assert!(pool.used() > base);
            drop(held);
            assert_eq!(pool.used(), base);
            println!("SEEK_WINDOW_CONTROL seek-{policy:?}-{chunk} {calls:?} {observations:?}");
        }
    }
}
#[test]
fn seek_window_control_late_transport_and_cancellation_priorities() {
    for policy in policy_cases() {
        for kind in [1, 2, 3] {
            let pool = Budget::new(512 * 1024).unwrap();
            let base = pool.used();
            let cancel = Arc::new(AtomicBool::new(false));
            let (source, log, mode) = source(fixture(), 7, &pool, &cancel);
            let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
                source,
                Limits::default(),
                &pool,
                policy,
            )
            .unwrap();
            mode.store(kind, Ordering::Relaxed);
            let error = reader
                .seek_for_playback(0, 40_000_000, &cancel)
                .unwrap_err();
            if kind == 1 {
                assert!(matches!(error, Error::Cancelled));
            } else {
                assert!(matches!(error, Error::Io(_)));
            }
            let observed = format!("{error:?}");
            let calls = log.lock().unwrap().clone();
            drop(reader);
            assert_eq!(pool.used(), base);
            println!("SEEK_WINDOW_CONTROL failure-{policy:?}-{kind} {calls:?} {observed}");
        }
    }
}
#[test]
fn seek_window_control_pressure_retry_and_truncation_keep_source_and_owner_facts() {
    for policy in policy_cases() {
        let pool = Budget::new(512 * 1024).unwrap();
        let base = pool.used();
        let cancel = Arc::new(AtomicBool::new(false));
        let (input, log, _) = source(fixture(), 7, &pool, &cancel);
        let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
            input,
            Limits::default(),
            &pool,
            policy,
        )
        .unwrap();
        let held = reader.next_typed_packet(&cancel).unwrap().unwrap();
        let pressure = pool.reserve(pool.available()).unwrap();
        let attempt = reader.seek_for_playback(0, 40_000_000, &cancel);
        assert!(attempt.is_ok() || matches!(&attempt, Err(Error::Backpressure { .. })));
        assert!(!reader.reader.failed);
        let observed = format!("{attempt:?}:used={}", pool.used());
        drop(pressure);
        let receipt = reader.seek_for_playback(0, 40_000_000, &cancel).unwrap();
        assert_eq!(receipt.actual_ns, 40_000_000);
        let mut packets = Vec::new();
        while let Some(packet) = reader.next_typed_packet(&cancel).unwrap() {
            packets.push(facts(&packet));
        }
        assert_eq!(packets.len(), 2);
        let calls = log.lock().unwrap().clone();
        drop(reader);
        assert_eq!(held.packet.data.as_ref(), &[b'a'; 32]);
        drop(held);
        assert_eq!(pool.used(), base);
        println!("SEEK_WINDOW_CONTROL pressure-{policy:?} {calls:?} {observed} {packets:?}");
        let pool = Budget::new(512 * 1024).unwrap();
        let base = pool.used();
        let mut raw = fixture();
        raw.pop();
        let cancel = Arc::new(AtomicBool::new(false));
        let (input, log, _) = source(raw, 1, &pool, &cancel);
        let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
            input,
            Limits::default(),
            &pool,
            policy,
        )
        .unwrap();
        let mut packets = Vec::new();
        let error = loop {
            match reader.next_typed_packet(&cancel) {
                Ok(Some(packet)) => packets.push(facts(&packet)),
                Ok(None) => panic!("truncated source became clean EOF"),
                Err(error) => break error,
            }
        };
        assert!(matches!(error, Error::Truncated));
        let observed = format!("{error:?}");
        let calls = log.lock().unwrap().clone();
        drop(reader);
        assert_eq!(pool.used(), base);
        println!("SEEK_WINDOW_CONTROL truncated-{policy:?} {calls:?} {observed} {packets:?}");
    }
}

#[test]
fn seek_window_whole_core_response_reduces_physical_callback_bounces() {
    for policy in policy_cases() {
        let pool = Budget::new(512 * 1024).unwrap();
        let base = pool.used();
        let cancel = Arc::new(AtomicBool::new(false));
        let (input, log, _) = source(fixture(), 8192, &pool, &cancel);
        let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
            input,
            Limits::default(),
            &pool,
            policy,
        )
        .unwrap();
        let initial = log.lock().unwrap().len();
        let receipt = reader.seek_for_playback(0, 40_000_000, &cancel).unwrap();
        assert_eq!(receipt.actual_ns, 40_000_000);
        let after_seek = log.lock().unwrap().len();
        let calls = log.lock().unwrap()[initial..after_seek].to_vec();
        let reads = calls
            .iter()
            .filter(|call| matches!(call, Call::Read { .. }))
            .count();
        let mut packets = Vec::new();
        while let Some(packet) = reader.next_typed_packet(&cancel).unwrap() {
            packets.push(facts(&packet));
        }
        assert_eq!(
            packets,
            [(40_000_000, b'b'), (80_000_000, b'c')].map(|(at, byte)| (
                Some(at),
                vec![byte; 32],
                b"exact-private".to_vec()
            ))
        );
        drop(reader);
        assert_eq!(pool.used(), base);
        println!(
            "SEEK_WINDOW_COUNTER {policy:?} reads={reads} calls={calls:?} {receipt:?} {packets:?}"
        );
        assert!(
            reads <= 12,
            "checked whole input window must remove callback bounces; observed {reads}"
        );
    }
}

#[test]
fn seek_window_epoch_certificate_keeps_public_hint_and_stops_before_replay() {
    let raw = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.research/seek-fixtures/cues-end.mkv"
    ));
    let pool = Budget::new(512 * 1024).unwrap();
    let base = pool.used();
    let bytes = triergon_core::Bytes::copy_from_slice(&pool, raw).unwrap();
    let mut reader = Reader::new(
        &pool,
        SourceId(2497),
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        Limits::default(),
        8192,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut sought = false;
    let mut checked = false;
    for _ in 0..20_000 {
        if !sought && reader.is_ready() {
            reader.seek_for_playback(0, 7_500_000_000).unwrap();
            sought = true;
        }
        match reader.step(&mut WorkBudget::new(8192), &cancel).unwrap() {
            ParseStep::NeedInput(request) => {
                if sought
                    && matches!(reader.seek_mode, SeekMode::Epoch)
                    && request.offset == reader.first_cluster.as_ref().unwrap().offset()
                    && request.maximum == 1
                {
                    let plan = reader
                        .blocking_seek_header_refill(&request)
                        .unwrap()
                        .expect("new seek engine must offer a real bounded input response");
                    let stop = reader.last_location.as_ref().unwrap().cluster_offset();
                    assert!(stop > request.offset);
                    assert!(plan.expanded().maximum > request.maximum);
                    assert!(plan.expanded().maximum <= 4096);
                    assert!(plan.expanded().offset + plan.expanded().maximum as u64 <= stop);
                    let ParseStep::NeedInput(public) =
                        reader.canonical_input_hint(request.clone()).unwrap()
                    else {
                        panic!("canonical request disappeared")
                    };
                    assert_eq!(public.source, request.source);
                    assert_eq!(public.token, request.token);
                    assert_eq!(public.offset, request.offset);
                    assert_eq!(public.maximum, 1);
                    assert!(reader.ordinary_header_refill().unwrap().is_none());
                    println!(
                        "SEEK_WINDOW_BOUND exact={request:?} expanded={:?} stop={stop}",
                        plan.expanded()
                    );
                    checked = true;
                    break;
                }
                let at = request.offset as usize;
                if at == raw.len() {
                    reader.finish(request.source, request.token).unwrap();
                } else {
                    let end = at + request.maximum.min(raw.len() - at);
                    reader
                        .provide(
                            &InputWindow::new(
                                request.source,
                                request.token,
                                request.offset,
                                bytes.slice(at..end).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
            }
            ParseStep::Finished => break,
            _ => (),
        }
    }
    assert!(
        checked,
        "authored operation must reach real Epoch range header"
    );
    drop((reader, bytes));
    assert_eq!(pool.used(), base);
}
