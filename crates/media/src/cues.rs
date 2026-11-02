//! Bounded Matroska Cues (RFC 9559 §5.1.5 and §22).
use crate::{Error, Result, ebml, storage::StorageVec};
use triergon_core::Budget;
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cue {
    pub ticks: u64,
    pub track: u64,
    pub cluster: u64,
    pub relative: Option<u64>,
    pub block: Option<u64>,
    pub codec_state: u64,
}

pub(crate) fn parse(
    bytes: &[u8],
    segment: u64,
    segment_end: Option<u64>,
    limit: usize,
    pool: &Budget,
) -> Result<StorageVec<Cue>> {
    let mut cues = StorageVec::<Cue>::new(pool);
    ebml::children(bytes, |id, data| {
        if id != 0xbb {
            return Ok(());
        }
        let mut ticks = None;
        let mut positions = StorageVec::new(pool);
        ebml::children(data, |id, data| {
            if id == 0xb3 {
                if ticks.replace(ebml::uint(data)?).is_some() {
                    return Err(Error::Invalid("duplicate CueTime"));
                }
            } else if id == 0xb7 {
                if cues.len() + positions.len() >= limit {
                    return Err(Error::Limit("cue entries"));
                }
                let (mut track, mut cluster, mut relative, mut block, mut codec_state) =
                    (None, None, None, None, None);
                ebml::children(data, |id, data| {
                    let field = match id {
                        0xf7 => &mut track,
                        0xf1 => &mut cluster,
                        0xf0 => &mut relative,
                        0x5378 => &mut block,
                        0xea => &mut codec_state,
                        _ => return Ok(()),
                    };
                    if field.replace(ebml::uint(data)?).is_some() {
                        return Err(Error::Invalid("duplicate cue position field"));
                    }
                    Ok(())
                })?;
                let track = track
                    .filter(|n| *n != 0)
                    .ok_or(Error::Invalid("missing CueTrack"))?;
                let cluster = segment
                    .checked_add(cluster.ok_or(Error::Invalid("missing CueClusterPosition"))?)
                    .ok_or(Error::Overflow)?;
                if segment_end.is_some_and(|end| cluster >= end) {
                    return Err(Error::Invalid("cue outside Segment"));
                }
                if block == Some(0) {
                    return Err(Error::Invalid("zero CueBlockNumber"));
                }
                positions.push((track, cluster, relative, block, codec_state.unwrap_or(0)))?;
            }
            Ok(())
        })?;
        let ticks = ticks.ok_or(Error::Invalid("missing CueTime"))?;
        if positions.is_empty() {
            return Err(Error::Invalid("missing CueTrackPositions"));
        }
        cues.try_reserve(positions.len())?;
        for &(track, cluster, relative, block, codec_state) in positions.iter() {
            cues.push(Cue {
                ticks,
                track,
                cluster,
                relative,
                block,
                codec_state,
            })?;
        }
        Ok(())
    })?;
    cues.sort_unstable_by_key(|cue| (cue.track, cue.ticks));
    Ok(cues)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn element(id: u64, bytes: &[u8]) -> Vec<u8> {
        let mut out: Vec<_> = id
            .to_be_bytes()
            .into_iter()
            .skip_while(|byte| *byte == 0)
            .collect();
        let width = (1..=8)
            .find(|width| (bytes.len() as u64) < (1_u64 << (7 * width)) - 1)
            .unwrap();
        out.extend(((1_u64 << (7 * width)) | bytes.len() as u64).to_be_bytes()[8 - width..].iter());
        out.extend(bytes);
        out
    }
    fn uint(id: u64, value: u64) -> Vec<u8> {
        element(id, &value.to_be_bytes())
    }
    #[test]
    fn many_out_of_order_cues_keep_all_positions_and_enforce_the_entry_limit() {
        let mut source = Vec::new();
        for ticks in (0..128).rev() {
            let position = element(
                0xb7,
                &[
                    uint(0xf7, 1),
                    uint(0xf1, ticks + 10),
                    uint(0xf0, 0),
                    uint(0x5378, 1),
                ]
                .concat(),
            );
            source.extend(element(0xbb, &[uint(0xb3, ticks), position].concat()));
        }
        let pool = Budget::new(64 * 1024).unwrap();
        let baseline = pool.used();
        let cues = parse(&source, 1000, Some(10_000), 128, &pool).unwrap();
        assert_eq!(cues.len(), 128);
        for (index, cue) in cues.iter().enumerate() {
            assert_eq!(cue.ticks, index as u64);
            assert_eq!(cue.cluster, 1010 + index as u64);
            assert_eq!(cue.relative, Some(0));
            assert_eq!(cue.block, Some(1));
            assert_eq!(cue.codec_state, 0);
        }
        assert!(pool.used() > baseline);
        drop(cues);
        assert_eq!(pool.used(), baseline);
        assert!(matches!(
            parse(&source, 1000, Some(10_000), 127, &pool),
            Err(Error::Limit("cue entries"))
        ));
        assert_eq!(pool.used(), baseline);
    }
}
