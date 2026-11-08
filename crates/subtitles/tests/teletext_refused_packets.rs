//! A packet the teletext decoder refuses leaves it as it was: the subtitles
//! afterwards are those of a stream without that packet.

mod teletext_common;

use teletext_common::{NORMAL, SUBTITLE, header, payload, row};
use triergon_core::{Budget, WorkBudget};
use triergon_subtitles::compat::teletext::{Decoded, Decoder, Options, Refusal};

/// A decode call's result: `Ok(Some(text))` for a subtitle with text,
/// `Ok(None)` for an empty one or nothing, `Err` for a refusal.
type Call = (Option<i64>, Result<Option<String>, Refusal>);

/// The results of decoding `packets`, each followed by the refused packets
/// placed before it, over a pool of `pool` bytes.
fn decode(pool: usize, packets: &[Vec<u8>], refused: &[(usize, &[u8])]) -> Vec<Call> {
    let pool = Budget::new(pool).expect("pool");
    let mut decoder = Decoder::new(&pool, Options::default()).expect("decoder");
    let mut calls = Vec::new();
    for (n, packet) in packets.iter().enumerate() {
        for &(_, bad) in refused.iter().filter(|(before, _)| *before == n) {
            let mut work = WorkBudget::new(Decoder::MAX_PACKET_WORK);
            match decoder.decode(bad, Some(1), &mut work) {
                Err(_) | Ok(Decoded::Refused(Refusal::InvalidData)) => {}
                other => panic!("not refused: {other:?}"),
            }
        }
        let pts = Some(1_000_000 * (n as i64 + 1));
        let mut work = WorkBudget::new(Decoder::MAX_PACKET_WORK);
        calls.push(
            match decoder.decode(packet, pts, &mut work).expect("decode") {
                Decoded::Subtitle(s) => (s.pts, Ok(s.ass.map(str::to_owned))),
                Decoded::Refused(refusal) => (None, Err(refusal)),
                Decoded::Nothing => (None, Ok(None)),
            },
        );
    }
    calls
}

#[test]
fn a_refused_teletext_packet_leaves_the_decoder_as_it_was() {
    // Subtitle page 888, its rows and a time filler header ending it sent in
    // the packet after its header.
    let packets: Vec<_> = (0..8u8)
        .flat_map(|n| {
            let text = format!("Line number {n}");
            [
                payload(&[header(8, 0x88, SUBTITLE)]),
                payload(&[
                    row(8, 20, text.as_bytes()),
                    row(8, 22, b"below"),
                    header(8, 0xFF, NORMAL),
                ]),
            ]
        })
        .collect();
    // It ends page 888 while its rows are awaited, then completes more
    // non-subtitle pages than the pool can remember.
    let mut units = vec![header(8, 0x89, NORMAL)];
    units.extend((0xA0..0xDF).map(|page| header(1, page, NORMAL)));
    let bad = payload(&units);
    // Not a whole data field.
    let short = &packets[7][..packets[7].len() - 1];
    let pool = 112 << 10;
    let expected = decode(pool, &packets, &[]);
    let texts = expected.iter().filter(|call| matches!(call.1, Ok(Some(_))));
    assert_eq!(texts.count(), 8);
    let refused = [(5, &bad[..]), (7, short), (11, &bad[..])];
    assert_eq!(decode(pool, &packets, &refused), expected);
}
