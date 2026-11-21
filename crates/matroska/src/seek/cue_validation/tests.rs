use super::*;
use crate::ebml_kit::{abc_laces, element, mkv, track, uint, zlib_encoding};
use crate::seek::plan::{LandingPlan, RecoveryAudio, SeekPlanJob};
use crate::test_kit::{seed_configuration, seed_configuration_tracks};
use triergon_core::{Bytes, InputWindow, SourceCapabilities, SourceId};

fn cluster_anchor(pool: &Budget, tail: &[u8]) -> (ElementReader, ClusterCheckpoint, Vec<u8>) {
    use crate::element::{ElementEvent, ElementReader};
    let raw = mkv(tail);
    let mut reader = ElementReader::new(
        pool,
        SourceId(11),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        Limits::default(),
        1,
    )
    .unwrap();
    let mut anchor = None;
    for _ in 0..100_000 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::NeedInput(request) => {
                let at = request.offset as usize;
                if at == raw.len() {
                    reader.finish(request.source, request.token).unwrap();
                } else {
                    let bytes = Bytes::copy_from_slice(pool, &raw[at..at + 1]).unwrap();
                    reader
                        .provide(
                            &InputWindow::new(request.source, request.token, request.offset, bytes)
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
            ParseStep::Yield => (),
            ParseStep::Event(ElementEvent::Start(header)) if header.id == 0x1f43b675 => {
                anchor = reader.cluster_checkpoint();
                break;
            }
            ParseStep::Event(_) => (),
            other => panic!("anchor source: {other:?}"),
        }
    }
    (reader, anchor.unwrap(), raw)
}

#[test]
fn async_cue_validation_requests_ranges_checks_all_laces_and_repositions_before_receipt() {
    use crate::{element::ElementEvent, seek::Cue};
    let pool = Budget::new(128 * 1024).unwrap();
    let configuration = seed_configuration(&pool, &zlib_encoding(1));
    let block = element(0xa3, &abc_laces(false));
    let timestamp = uint(0xe7, 2);
    let (mut reader, anchor, raw) = cluster_anchor(
        &pool,
        &element(0x1f43b675, &[timestamp.clone(), block].concat()),
    );
    let generation = reader.generation();
    let cue = Cue {
        ticks: 2,
        track: 1,
        cluster: anchor.offset(),
        relative: Some(timestamp.len() as u64),
        block: Some(1),
        codec_state: 0,
    };
    let configuration = configuration.rebind(generation, 3);
    let mut job = CueValidationJob::new(
        CueTarget {
            cue,
            track: 0,
            scale: 1_000_000,
            anchor,
            whole_cluster: false,
        },
        configuration,
        Limits::default(),
        &pool,
    )
    .unwrap();
    let mut result = None;
    let mut request_tokens = Vec::new();
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(1);
        match job
            .step(&mut reader, &mut work, &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::NeedInput(request) => {
                request_tokens.push(request.token.clone());
                let at = request.offset as usize;
                assert!(at < raw.len());
                let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                reader
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            ParseStep::Event(value) => {
                result = Some(value);
                break;
            }
            other => panic!("cue validator: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    let result = result.unwrap();
    assert_eq!(result.actual_ns, 2_000_000);
    assert!(result.scanned_bytes > 0);
    assert_eq!(reader.generation(), generation);
    assert!(
        request_tokens
            .iter()
            .all(|token| token.generation() == generation)
    );
    assert_eq!(reader.position(), result.location.block_offset());
    // A replay range has a fresh core request token before a receipt can escape.
    assert_ne!(reader.request().token, *request_tokens.last().unwrap());
    let mut replay = None;
    for _ in 0..100_000 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::NeedInput(request) => {
                let at = request.offset as usize;
                let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                reader
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            ParseStep::Yield => (),
            ParseStep::Event(ElementEvent::Leaf { header, data }) => {
                replay = Some((header, data));
                break;
            }
            other => panic!("replayed block: {other:?}"),
        }
    }
    let (header, data) = replay.unwrap();
    assert_eq!(header.id, 0xa3);
    assert_eq!(data.as_ref(), abc_laces(false).as_slice());
}

#[test]
fn async_cue_validation_cannot_turn_bad_last_crc_or_wrong_source_position_into_landing() {
    use crate::seek::Cue;
    for bad_crc in [false, true] {
        let pool = Budget::new(128 * 1024).unwrap();
        let configuration = seed_configuration(&pool, &zlib_encoding(1));
        let timestamp = uint(0xe7, 2);
        let (mut reader, anchor, raw) = cluster_anchor(
            &pool,
            &element(
                0x1f43b675,
                &[timestamp.clone(), element(0xa3, &abc_laces(bad_crc))].concat(),
            ),
        );
        let cue = Cue {
            ticks: 2,
            track: 1,
            cluster: anchor.offset(),
            relative: Some(timestamp.len() as u64 + 1),
            block: Some(1),
            codec_state: 0,
        };
        let mut job = CueValidationJob::new(
            CueTarget {
                cue,
                track: 0,
                scale: 1_000_000,
                anchor,
                whole_cluster: true,
            },
            configuration.rebind(reader.generation(), 3),
            Limits::default(),
            &pool,
        )
        .unwrap();
        let mut failed = false;
        for _ in 0..100_000 {
            match job.step(
                &mut reader,
                &mut WorkBudget::new(1),
                &AtomicBool::new(false),
            ) {
                Ok(ParseStep::Yield) => (),
                Ok(ParseStep::NeedInput(request)) => {
                    let at = request.offset as usize;
                    if at == raw.len() {
                        reader.finish(request.source, request.token).unwrap();
                    } else {
                        let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                        reader
                            .provide(
                                &InputWindow::new(
                                    request.source,
                                    request.token,
                                    request.offset,
                                    bytes,
                                )
                                .unwrap(),
                            )
                            .unwrap();
                    }
                }
                Err(Error::Invalid(message)) => {
                    assert_eq!(
                        message,
                        if bad_crc {
                            "Adler-32 mismatch"
                        } else {
                            "cue position disagrees with referenced block"
                        }
                    );
                    failed = true;
                    break;
                }
                other => panic!("unverified cue must not produce a landing: {other:?}"),
            }
        }
        assert!(failed);
    }
}

#[test]
fn playback_preroll_plan_validates_video_but_replays_the_earlier_interleaved_audio() {
    use crate::{element::ElementEvent, seek::Cue};
    let pool = Budget::new(128 * 1024).unwrap();
    let entries = [track(1, 1, &[]), track(2, 2, &uint(0x56bb, 2_000_000))].concat();
    let configuration = seed_configuration_tracks(&pool, &entries);
    let timestamp = uint(0xe7, 2);
    let audio = element(0xa3, &[0x82, 0xff, 0xff, 0x80, b'A']);
    let video = element(0xa3, &[0x81, 0, 0, 0x80, b'V']);
    let (mut reader, anchor, raw) = cluster_anchor(
        &pool,
        &element(
            0x1f43b675,
            &[timestamp.clone(), audio.clone(), video].concat(),
        ),
    );
    let configuration = configuration.rebind(reader.generation(), 3);
    let cues = [Cue {
        ticks: 2,
        track: 1,
        cluster: anchor.offset(),
        relative: Some((timestamp.len() + audio.len()) as u64),
        block: Some(2),
        codec_state: 0,
    }];
    let mut plan = SeekPlanJob::new(
        configuration.clone(),
        0,
        2_000_000,
        true,
        1_000_000,
        anchor.offset(),
        RecoveryAudio::Overlap,
    )
    .unwrap();
    let mut result = None;
    for _ in 0..1000 {
        match plan
            .step(&cues, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                result = Some(value);
                break;
            }
            other => panic!("preroll plan: {other:?}"),
        }
    }
    let result = result.unwrap();
    assert_eq!(result.preroll_ns, 2_000_000);
    assert_eq!(result.discard_until_ns, 2_000_000);
    let LandingPlan::Cue {
        cue,
        whole_cluster,
        scan_after_validation: None,
    } = result.landing
    else {
        panic!("single-cue preroll")
    };
    assert!(whole_cluster);
    let mut job = CueValidationJob::new(
        CueTarget {
            cue,
            track: 0,
            scale: 1_000_000,
            anchor,
            whole_cluster,
        },
        configuration,
        Limits::default(),
        &pool,
    )
    .unwrap();
    let mut landing = None;
    for _ in 0..100_000 {
        match job
            .step(
                &mut reader,
                &mut WorkBudget::new(1),
                &AtomicBool::new(false),
            )
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::NeedInput(request) => {
                let at = request.offset as usize;
                assert!(at < raw.len());
                let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                reader
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            ParseStep::Event(value) => {
                landing = Some(value);
                break;
            }
            other => panic!("preroll validation: {other:?}"),
        }
    }
    let landing = landing.unwrap();
    assert_eq!(landing.actual_ns, 2_000_000);
    assert_eq!(landing.replay_cluster_time, None);
    assert_eq!(reader.position(), cue.cluster);
    let mut first_block = None;
    for _ in 0..100_000 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::NeedInput(request) => {
                let at = request.offset as usize;
                let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                reader
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            ParseStep::Event(ElementEvent::Leaf { header, data }) if header.id == 0xa3 => {
                first_block = Some(data);
                break;
            }
            ParseStep::Event(_) => (),
            other => panic!("whole-Cluster replay: {other:?}"),
        }
    }
    assert_eq!(
        first_block.unwrap().as_ref(),
        &[0x82, 0xff, 0xff, 0x80, b'A']
    );
}

fn landing(
    group: bool,
    relative: bool,
    number: bool,
    laced: bool,
    conflict: bool,
    quantum: u64,
) -> Result<(u64, u64)> {
    let pool = Budget::new(128 * 1024).unwrap();
    let config = seed_configuration(&pool, &[]);
    let stamp = uint(0xe7, 2);
    let mut content = stamp;
    let mut target = 0;
    for index in 1..=3 {
        if index == 3 {
            target = content.len() as u64;
        }
        let mut body = vec![0x81, 0, 0, if group { 0 } else { 0x80 }];
        if laced && index == 3 {
            body[3] |= 4;
            body.extend([1, 7, 8]);
        } else {
            body.push(index);
        }
        let block = if group {
            element(0xa0, &element(0xa1, &body))
        } else {
            element(0xa3, &body)
        };
        content.extend(block);
        content.extend(element(0xec, &[0; 3]));
    }
    let cluster = element(0x1f43b675, &content);
    let (mut reader, anchor, raw) = cluster_anchor(&pool, &cluster);
    let expected = anchor.offset() + (cluster.len() - content.len()) as u64 + target;
    let cue = Cue {
        ticks: 2,
        track: 1,
        cluster: anchor.offset(),
        relative: relative.then_some(target),
        block: number.then_some(if conflict { 2 } else { 3 }),
        codec_state: 0,
    };
    let mut job = CueValidationJob::new(
        CueTarget {
            cue,
            track: 0,
            scale: 1_000_000,
            anchor,
            whole_cluster: false,
        },
        config.rebind(reader.generation(), 3),
        Limits::default(),
        &pool,
    )?;
    let cancel = AtomicBool::new(false);
    let mut zero = WorkBudget::new(0);
    let before = reader.position();
    assert!(matches!(
        job.step(&mut reader, &mut zero, &cancel)?,
        ParseStep::Yield
    ));
    assert_eq!(reader.position(), before);
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(quantum);
        let step = job.step(&mut reader, &mut work, &cancel)?;
        assert!(work.spent() <= quantum);
        match step {
            ParseStep::Yield => (),
            ParseStep::NeedInput(request) => {
                let at = request.offset as usize;
                if at == raw.len() {
                    reader.finish(request.source, request.token)?;
                } else {
                    let b = Bytes::copy_from_slice(&pool, &raw[at..at + 1])?;
                    reader.provide(&InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        b,
                    )?)?;
                }
            }
            ParseStep::Event(value) => {
                assert_eq!(value.location.block_offset(), expected);
                assert_eq!(reader.position(), expected);
                return Ok((value.location.block_offset(), expected));
            }
            other => panic!("bounded reference validator {other:?}"),
        }
    }
    panic!("validator stalled")
}

#[test]
fn direct_relative_selector_reaches_later_equal_time_simple_rap() {
    landing(false, true, false, false, false, 1).unwrap();
}

#[test]
fn direct_number_selector_reaches_later_equal_time_simple_rap() {
    landing(false, false, true, false, false, 1).unwrap();
}

#[test]
fn direct_both_selectors_reach_later_equal_time_group() {
    landing(true, true, true, false, false, 7).unwrap();
}

#[test]
fn direct_referenced_lace_validates_and_positions_the_whole_batch() {
    landing(false, true, true, true, false, 1).unwrap();
}

#[test]
fn direct_conflicting_selectors_remain_invalid() {
    assert!(matches!(
        landing(false, true, true, false, true, 1),
        Err(Error::Invalid(_))
    ));
}
