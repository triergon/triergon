//! Segment-local AAC presentation clocks, separate from the original PES facts.

use crate::hls_aac::HlsAacFrame;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Error, Result, Rounding, Time, TimeBase};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HlsAacPtsOrigin {
    Pes,
    BeforeFirstPes,
    AfterPes,
    SegmentLocal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HlsAacClock {
    pub pts: Time,
    pub dts: Time,
    pub origin: HlsAacPtsOrigin,
    pub ffmpeg90: HlsAacFfmpegClock90,
}

/// The AAC demux clock in 90 kHz ticks. The reference reports the same decode
/// and presentation tick for AAC. Source DTS stays available on the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HlsAacFfmpegClock90 {
    pub pts_and_dts: i64,
    pub duration: i64,
}

/// Presentation and frame duration in integer nanoseconds before playlist mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HlsParserOnlyAudioClock {
    pub pts_ns: i64,
    pub duration_ns: i64,
}

/// Resolve a complete segment's audio lane using exact frame durations. A
/// missing leading timestamp is computed backwards from the first PES anchor.
/// With no PES anchor, the first frame is segment-local zero. Each later PES
/// timestamp keeps its own value. An absent DTS takes the selected AAC
/// receiver's presentation time. The input frames retain their original facts.
pub fn resolve_hls_aac_clocks(
    pool: &Budget,
    frames: &[HlsAacFrame],
    maximum_frames: usize,
    cancel: &AtomicBool,
) -> Result<BudgetVec<HlsAacClock>> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    if frames.len() > maximum_frames {
        return Err(Error::Limit {
            requested: frames.len(),
            maximum: maximum_frames,
        });
    }
    let mut clocks = BudgetVec::new(pool);
    let tick = TimeBase::new(1, 90_000)?;
    let mut prefix_duration = Time::new(0, 1)?;
    let mut first_anchor = None;
    for (index, frame) in frames.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if let Some(pts) = frame.pts {
            first_anchor = Some((index, pts));
            break;
        }
        prefix_duration = prefix_duration.checked_add(frame.duration)?;
    }
    let (mut next, mut next90) = match first_anchor {
        Some((index, pts)) => {
            let mut projected = pts.to_i64_ticks(tick, Rounding::NearestAway)?;
            for frame in frames[..index].iter().rev() {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                projected = step_on_frame_grid(tick, projected, frame.duration, -1)?;
            }
            (pts.checked_sub(prefix_duration)?, projected)
        }
        None => (Time::new(0, 1)?, 0),
    };
    let mut missing_origin = if first_anchor.is_some() {
        HlsAacPtsOrigin::BeforeFirstPes
    } else {
        HlsAacPtsOrigin::SegmentLocal
    };
    clocks.try_reserve_exact(frames.len())?;
    let mut previous = None;
    for frame in frames {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if frame.duration.numerator() <= 0 {
            return Err(Error::Invalid("AAC segment frame duration"));
        }
        let (pts, pts90, origin) = match frame.pts {
            Some(pts) => {
                missing_origin = HlsAacPtsOrigin::AfterPes;
                (
                    pts,
                    pts.to_i64_ticks(tick, Rounding::NearestAway)?,
                    HlsAacPtsOrigin::Pes,
                )
            }
            None => (next, next90, missing_origin),
        };
        if previous.is_some_and(|previous| pts < previous) {
            return Err(Error::Invalid("AAC segment timestamps move backwards"));
        }
        let dts = frame.dts.unwrap_or(pts);
        let ffmpeg90 = HlsAacFfmpegClock90 {
            pts_and_dts: pts90,
            duration: frame.duration.to_i64_ticks(tick, Rounding::Floor)?,
        };
        clocks
            .try_push(HlsAacClock {
                pts,
                dts,
                origin,
                ffmpeg90,
            })
            .map_err(|error| error.error)?;
        previous = Some(pts);
        next = pts.checked_add(frame.duration)?;
        next90 = step_on_frame_grid(tick, pts90, frame.duration, 1)?;
    }
    Ok(clocks)
}

/// Presentation nanoseconds for the AVC/AAC caller using a parser-only AAC
/// reference. Its missing frame durations use the ADTS-derived integer
/// nanoseconds. This view is separate from exact sample time and the full
/// FFmpeg demux clock.
pub(crate) fn parser_only_aac_pts_ns(
    frames: &BudgetVec<HlsAacFrame>,
    cancel: &AtomicBool,
) -> Result<BudgetVec<i64>> {
    parser_only_aac_clocks(frames, cancel, |clock| clock.pts_ns)
}

pub(crate) fn parser_only_aac_clocks<T>(
    frames: &BudgetVec<HlsAacFrame>,
    cancel: &AtomicBool,
    make: impl Fn(HlsParserOnlyAudioClock) -> T,
) -> Result<BudgetVec<T>> {
    let mut output = BudgetVec::new(frames.budget());
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    let anchor = |frame: &HlsAacFrame| frame.pts.or(frame.dts);
    let mut leading = 0_i64;
    let mut next = 0;
    for frame in frames.as_slice() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if let Some(time) = anchor(frame) {
            next = time
                .to_nanoseconds(Rounding::NearestAway)?
                .checked_sub(leading)
                .ok_or(Error::Overflow)?;
            break;
        }
        leading = leading
            .checked_add(frame.duration.to_nanoseconds(Rounding::Floor)?)
            .ok_or(Error::Overflow)?;
    }
    output.try_reserve_exact(frames.len())?;
    let mut previous = None;
    for frame in frames.as_slice() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let pts = match anchor(frame) {
            Some(time) => time.to_nanoseconds(Rounding::NearestAway)?,
            None => next,
        };
        if previous.is_some_and(|previous| pts < previous) {
            return Err(Error::Invalid("AAC segment timestamps move backwards"));
        }
        let duration_ns = frame.duration.to_nanoseconds(Rounding::Floor)?;
        output
            .try_push(make(HlsParserOnlyAudioClock {
                pts_ns: pts,
                duration_ns,
            }))
            .map_err(|error| error.error)?;
        previous = Some(pts);
        next = pts.checked_add(duration_ns).ok_or(Error::Overflow)?;
    }
    Ok(output)
}

// Keep the tick displacement from the nearest rounded frame-grid point.
// Observed libavformat packet timestamps use that phase across AAC frames.
fn step_on_frame_grid(tick: TimeBase, pts: i64, duration: Time, step: i128) -> Result<i64> {
    let numerator = u64::try_from(duration.numerator())
        .map_err(|_| Error::Invalid("AAC segment frame duration"))?;
    let denominator = u64::try_from(duration.denominator()).map_err(|_| Error::Overflow)?;
    let frame = TimeBase::new(numerator, denominator)?;
    let current =
        Time::from_ticks(i128::from(pts), tick)?.to_ticks(frame, Rounding::NearestAway)?;
    let adjacent = current.checked_add(step).ok_or(Error::Overflow)?;
    let project = |index| Time::from_ticks(index, frame)?.to_ticks(tick, Rounding::NearestAway);
    let delta = project(adjacent)?
        .checked_sub(project(current)?)
        .ok_or(Error::Overflow)?;
    i64::try_from(i128::from(pts).checked_add(delta).ok_or(Error::Overflow)?)
        .map_err(|_| Error::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_core::Bytes;
    use triergon_framing::inspect_aac_adts_header;

    fn frame(pool: &Budget, ticks: Option<i128>) -> HlsAacFrame {
        let bytes = [0xff, 0xf1, 0x50, 0x80, 0x01, 0x1f, 0xfc, 0];
        HlsAacFrame {
            bytes: Bytes::copy_from_slice(pool, &bytes).unwrap(),
            header: inspect_aac_adts_header(&bytes).unwrap(),
            pts: ticks.map(|ticks| Time::new(ticks * 1024, 44100).unwrap()),
            dts: None,
            duration: Time::new(1024, 44100).unwrap(),
        }
    }

    #[test]
    fn exact_44100_clock_marks_leading_holes_explicit_anchors_and_discontinuities() {
        let pool = Budget::new(8192).unwrap();
        let frames = [None, Some(3), None, Some(8), None].map(|pts| frame(&pool, pts));
        let result = resolve_hls_aac_clocks(&pool, &frames, 5, &AtomicBool::new(false)).unwrap();
        let expected = [
            (2, HlsAacPtsOrigin::BeforeFirstPes),
            (3, HlsAacPtsOrigin::Pes),
            (4, HlsAacPtsOrigin::AfterPes),
            (8, HlsAacPtsOrigin::Pes),
            (9, HlsAacPtsOrigin::AfterPes),
        ];
        for (stamp, (index, origin)) in result.as_slice().iter().zip(expected) {
            assert_eq!(stamp.pts, Time::new(index * 1024, 44100).unwrap());
            assert_eq!(stamp.dts, stamp.pts);
            assert_eq!(stamp.origin, origin);
        }
        let no_anchors = [frame(&pool, None), frame(&pool, None)];
        let result =
            resolve_hls_aac_clocks(&pool, &no_anchors, 2, &AtomicBool::new(false)).unwrap();
        assert_eq!(result.as_slice()[0].pts, Time::new(0, 1).unwrap());
        assert_eq!(result.as_slice()[1].pts, no_anchors[0].duration);
        assert!(
            result
                .as_slice()
                .iter()
                .all(|stamp| stamp.origin == HlsAacPtsOrigin::SegmentLocal)
        );
        let backwards = [frame(&pool, Some(2)), frame(&pool, Some(1))];
        assert!(matches!(
            resolve_hls_aac_clocks(&pool, &backwards, 2, &AtomicBool::new(false)),
            Err(Error::Invalid("AAC segment timestamps move backwards"))
        ));
    }

    #[test]
    fn parser_only_clock_uses_rounded_anchors_and_integer_adts_duration() {
        let pool = Budget::new(8192).unwrap();
        let mut frames = BudgetVec::new(&pool);
        for pts in [None, Some(3), None, Some(8), None] {
            frames.try_push(frame(&pool, pts)).unwrap();
        }
        let stamps = parser_only_aac_pts_ns(&frames, &AtomicBool::new(false)).unwrap();
        let step = 1_024_000_000_000 / 44_100;
        let first = frames.as_slice()[1]
            .pts
            .unwrap()
            .to_nanoseconds(Rounding::NearestAway)
            .unwrap();
        let next = frames.as_slice()[3]
            .pts
            .unwrap()
            .to_nanoseconds(Rounding::NearestAway)
            .unwrap();
        assert_eq!(
            stamps.as_slice(),
            &[first - step, first, first + step, next, next + step]
        );
        assert!(frames.as_slice()[0].pts.is_none());
    }
}
