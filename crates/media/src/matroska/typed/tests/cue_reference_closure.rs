//! Direct address assertions are separate from public plain-seek scan policy.
use super::*;

fn landing(
    group: bool,
    relative: bool,
    number: bool,
    laced: bool,
    conflict: bool,
    quantum: u64,
) -> crate::Result<(u64, u64)> {
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
    let cue = crate::cues::Cue {
        ticks: 2,
        track: 1,
        cluster: anchor.offset(),
        relative: relative.then_some(target),
        block: number.then_some(if conflict { 2 } else { 3 }),
        codec_state: 0,
    };
    let mut job = seek_validate::CueValidationJob::new(
        seek_validate::CueTarget {
            cue,
            track: 0,
            scale: 1_000_000,
            anchor,
            whole_cluster: false,
        },
        config.rebind(reader.generation(), 3),
        crate::Limits::default(),
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
        Err(crate::Error::Invalid(_))
    ));
}
