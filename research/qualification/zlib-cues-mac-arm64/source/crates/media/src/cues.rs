//! Bounded Matroska Cues (RFC 9559 §5.1.5 and §22).
use crate::{Error, Result, ebml};
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
) -> Result<Vec<Cue>> {
    let mut cues = Vec::new();
    ebml::children(bytes, |id, data| {
        if id != 0xbb {
            return Ok(());
        }
        let mut ticks = None;
        let mut positions = Vec::new();
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
                positions
                    .try_reserve(1)
                    .map_err(|_| Error::Limit("cue allocation"))?;
                positions.push((track, cluster, relative, block, codec_state.unwrap_or(0)));
            }
            Ok(())
        })?;
        let ticks = ticks.ok_or(Error::Invalid("missing CueTime"))?;
        if positions.is_empty() {
            return Err(Error::Invalid("missing CueTrackPositions"));
        }
        cues.try_reserve(positions.len())
            .map_err(|_| Error::Limit("cue allocation"))?;
        cues.extend(
            positions
                .into_iter()
                .map(|(track, cluster, relative, block, codec_state)| Cue {
                    ticks,
                    track,
                    cluster,
                    relative,
                    block,
                    codec_state,
                }),
        );
        Ok(())
    })?;
    cues.sort_unstable_by_key(|cue| (cue.track, cue.ticks));
    Ok(cues)
}
