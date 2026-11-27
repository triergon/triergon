use std::ffi::OsString;

use triergon_core::signature::{
    Container, Policy, Selection, TransportSync, TransportTie, ebml, ogg, opening_box, select,
};

use super::packets::record;
use triergon_harness_runner::Operation;

const BOX_SETS: [&[[u8; 4]]; 3] = [&[], &[*b"ftyp", *b"moof"], &[*b"wide", *b"\0foo", *b"uuid"]];

pub(super) fn operation<'a>(
    _prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    if !arguments.is_empty() || input.len() > 390 {
        return Err("source signatures take a prefix of at most 390 bytes and no arguments".into());
    }
    Ok(Operation {
        name: "source-signatures",
        run: Box::new(move |work, mut trace| {
            let ts = TransportSync::Ts188;
            let m2ts = TransportSync::M2ts192;
            let observed = |value: Option<bool>| value.map_or(0, |seen| 1 + u64::from(seen));
            for size in 0..=input.len() {
                let prefix = &input[..size];
                record(
                    work,
                    &mut trace,
                    3,
                    &[
                        size as u64,
                        u64::from(ebml(prefix)),
                        u64::from(ogg(prefix)),
                        ts.prefix_len() as u64,
                        observed(ts.observed(prefix)),
                        m2ts.prefix_len() as u64,
                        observed(m2ts.observed(prefix)),
                    ],
                    opening_box(prefix).map_or(&[][..], |kind| kind.as_slice()),
                )?;
                for (boxes, opening_boxes) in BOX_SETS.into_iter().enumerate() {
                    for allow_ogg in [false, true] {
                        for (tie, transport_tie) in [TransportTie::Ts188, TransportTie::Refuse]
                            .into_iter()
                            .enumerate()
                        {
                            let policy = Policy {
                                opening_boxes,
                                ogg: allow_ogg,
                                short_audio_prefix: false,
                                transport_tie,
                            };
                            let (kind, need) = match select(prefix, &policy) {
                                Selection::Need(length) => (0, length as u64),
                                Selection::Unrecognised => (1, 0),
                                Selection::Ambiguous => (2, 0),
                                Selection::Container(Container::Matroska) => (3, 0),
                                Selection::Container(Container::Isobmff) => (4, 0),
                                Selection::Container(Container::Ogg) => (5, 0),
                                Selection::Container(Container::Transport(
                                    TransportSync::Ts188,
                                )) => (6, 0),
                                Selection::Container(Container::Transport(
                                    TransportSync::M2ts192,
                                )) => (7, 0),
                            };
                            record(
                                work,
                                &mut trace,
                                4,
                                &[boxes as u64, u64::from(allow_ogg), tie as u64, kind, need],
                                &[],
                            )?;
                        }
                    }
                }
            }
            Ok(())
        }),
    })
}
