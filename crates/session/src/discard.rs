//! The decoded samples a container marks for discarding.
use triergon_codec::AudioFrame;
use triergon_core::container::{PacketInfo, Trim};
use triergon_core::{Rounding, TimeBase};

/// What a container says about discarding one packet's decoded output: its
/// [`Trim`] and any samples before time zero (Matroska CodecDelay, Opus pre-skip in
/// Ogg). It is kept in the packet's own time base until the output rate is known.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Discard {
    pts: Option<i64>,
    trim: Trim,
    time_base: Option<TimeBase>,
}

impl Discard {
    /// What `packet` marks for discarding.
    pub fn of(packet: &impl PacketInfo) -> Discard {
        Discard {
            pts: packet.pts(),
            trim: packet.trim(),
            time_base: Some(packet.time_base()),
        }
    }

    /// Drop the marked samples from `frame`, and at least `delay` samples from its start.
    /// What does not fit in this frame stays in `delay` for the next one: a start delay
    /// or start trim may span packets (a LAME skip of 1105 samples over 576-sample
    /// frames, a long Opus pre-skip).
    pub fn apply(self, frame: &mut AudioFrame, delay: &mut usize) {
        let len = frame.samples_per_channel;
        if len == 0 {
            return;
        }
        let samples = |ticks: i64| {
            self.time_base
                .map_or(0, |base| to_samples(ticks, base, frame.sample_rate))
        };
        let before_zero = self
            .pts
            .filter(|&pts| pts < 0)
            .map_or(0, |pts| samples(-pts));
        let pending = self
            .trim
            .start
            .map_or(0, samples)
            .max(before_zero)
            .max(*delay);
        let start = pending.min(len);
        *delay = pending - start;
        let end = self.trim.end.map_or(0, samples).min(len - start);
        if start == 0 && end == 0 {
            return;
        }
        let kept = len - start - end;
        for plane in &mut frame.planes {
            plane.copy_within(start..start + kept, 0);
            plane.truncate(kept);
        }
        frame.samples_per_channel = kept;
    }
}

/// Samples at `rate` in `ticks` of `base`, rounded to the nearest; 0 when it does not fit.
fn to_samples(ticks: i64, base: TimeBase, rate: u32) -> usize {
    TimeBase::new(1, u64::from(rate.max(1)))
        .and_then(|samples| base.rescale(ticks.max(0), samples, Rounding::NearestAway))
        .ok()
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_codec::ChannelLayout;

    fn frame(len: usize) -> AudioFrame {
        AudioFrame {
            sample_rate: 48_000,
            layout: ChannelLayout::default_for(1).unwrap_or_default(),
            samples_per_channel: len,
            planes: vec![(0..len).map(|i| i as f32).collect()],
        }
    }

    fn discard(pts: i64, start: Option<i64>, end: Option<i64>, base: u64) -> Discard {
        Discard {
            pts: Some(pts),
            trim: Trim { start, end },
            time_base: TimeBase::new(1, base).ok(),
        }
    }

    #[test]
    fn trims_negative_time_and_start_delay_combine() {
        // An Ogg Opus first packet: pts -312, start trim 312, pre-skip 312.
        let mut out = frame(960);
        let mut delay = 312;
        discard(-312, Some(312), None, 48_000).apply(&mut out, &mut delay);
        assert_eq!(
            (out.samples_per_channel, out.planes[0][0], delay),
            (648, 312.0, 0)
        );
        // Matroska: CodecDelay as negative nanoseconds, no trim.
        let mut out = frame(960);
        let mut delay = 312;
        discard(-6_500_000, None, Some(2_000_000), 1_000_000_000).apply(&mut out, &mut delay);
        assert_eq!(
            (out.samples_per_channel, out.planes[0][0], delay),
            (552, 312.0, 0)
        );
        // A pre-skip longer than the first packet carries into the next.
        let mut out = frame(120);
        let mut delay = 200;
        discard(0, None, None, 48_000).apply(&mut out, &mut delay);
        assert_eq!((out.samples_per_channel, delay), (0, 80));
        // A start trim longer than its packet carries into the next.
        let mut out = frame(576);
        let mut delay = 0;
        discard(0, Some(1105), None, 48_000).apply(&mut out, &mut delay);
        assert_eq!((out.samples_per_channel, delay), (0, 529));
        let mut out = frame(576);
        discard(576, None, None, 48_000).apply(&mut out, &mut delay);
        assert_eq!(
            (out.samples_per_channel, out.planes[0][0], delay),
            (47, 529.0, 0)
        );
    }
}
