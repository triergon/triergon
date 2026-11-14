//! The frame duration of a stream whose packets and SPS give none, estimated
//! from the times of its first packets as the reference does (probes p58,
//! p71-p73, p77-p83).
//!
//! The estimate takes the times of the first [`TAKEN`] packets that carry
//! one. When the steps from the fourth to the twenty-first of them share a
//! divisor longer than 1/500 s, that divisor is the duration. Otherwise each
//! candidate frame rate is scored by how far the times of the second to the
//! twenty-first packets lie from its frame grid, and the duration is one frame
//! at the best rate, rounded down to the time base; with no candidate close
//! enough it is one tick of the time base.

/// The packets whose times the estimate takes (probes p71, p77).
pub(super) const TAKEN: usize = 21;

/// The divisor takes the steps from the fourth packet taken (probe p77:
/// moving the second or third packet leaves the divisor).
const DIVISOR_FROM: usize = 3;

/// The fit takes the times from the second packet taken (probes p73, p78:
/// the second and third packets count in the fit, not in the divisor).
const FIT_FROM: usize = 1;

/// A divisor of the steps is the duration when longer than this fraction of
/// a second (probes p73, p79: 180 ticks of 1/90000 s give the fit, 181 the
/// divisor).
const DIVISOR_ABOVE: (u128, u128) = (1, 500);

/// A rate below this fraction of the rate of the mean step over every step
/// taken is left out (probes p80, p83: 0.799986 left out, 0.8 kept).
const SLOWEST: f64 = 0.8;

/// A rate's score must be below this (probe p83: 0.009972 kept, 0.010028
/// not).
const CLOSE: f64 = 0.01;

/// The candidate rates in frames per second, as (numerator, denominator), in
/// the order they are tried: twelfths of a frame per second from 1 to 30,
/// whole rates from 31 to 60, 80, 120 and 240, and the NTSC rates of 12, 15,
/// 24, 30, 48 and 60 (probes p79, p80: each is picked for packets at its own
/// rate; rates below 1, between 30 and 31 off the twelfths, from 61 to 79, 81
/// to 119, 121 to 239 and above 240, and the NTSC rates of 25, 50, 72, 90,
/// 96, 100, 120, 144 and 240 are not).
fn candidates() -> impl Iterator<Item = (u64, u64)> {
    let twelfths = (12..=360).map(|k| (k, 12));
    let whole = (31..=60).chain([80, 120, 240]).map(|k| (k, 1));
    let ntsc = [12, 15, 24, 30, 48, 60].map(|k| (k * 1000, 1001));
    twelfths.chain(whole).chain(ntsc)
}

/// The times of the first packets, and the packets going without one.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RateEstimate {
    times: [i64; TAKEN],
    taken: usize,
    /// Packets seen.
    seen: u32,
    /// Packets up to this index go without a time unless their DTS differs
    /// from their PTS.
    untimed_through: Option<u32>,
    /// The stream has ended.
    ended: bool,
}

impl RateEstimate {
    /// Takes the next packet's time. `distinct_dts` says whether the packet
    /// has a DTS other than its PTS (probe p84: a DTS equal to the PTS, or
    /// none, alike), `b_picture` whether it holds a B picture, which ends a
    /// run of packets going without a time.
    pub(super) fn packet(&mut self, time: Option<i64>, distinct_dts: bool, b_picture: bool) {
        let index = self.seen;
        self.seen = self.seen.saturating_add(1);
        if b_picture {
            self.untimed_through = None;
        }
        if !distinct_dts && self.untimed_through.is_some_and(|last| index <= last) {
            return;
        }
        if let Some(time) = time
            && self.taken < TAKEN
        {
            self.times[self.taken] = time;
            self.taken += 1;
        }
    }

    /// The packets after those seen, up to index `last`, go without a time
    /// unless their DTS differs from their PTS.
    pub(super) fn untime_through(&mut self, last: u32) {
        if self.seen <= last {
            self.untimed_through = Some(last);
        }
    }

    /// The stream ends: the estimate takes the times it has.
    pub(super) fn end(&mut self) {
        self.ended = true;
    }

    /// Whether the estimate has every time it takes: all of them, or those
    /// of a stream that ended.
    pub(super) fn settled(&self) -> bool {
        self.taken >= TAKEN || self.ended
    }

    /// The estimated duration in ticks of `time_base`, or `None` without a
    /// packet time; one tick with fewer than three (probe p42: one or two
    /// packets last 1, three 3003).
    pub(super) fn duration(&self, time_base: (u32, u32)) -> Option<i64> {
        let times = &self.times[..self.taken];
        let (&first, &last) = (times.first()?, times.last()?);
        if times.len() < 3 {
            return Some(1);
        }
        let (num, den) = (u64::from(time_base.0.max(1)), u64::from(time_base.1.max(1)));
        let divisor = times
            .windows(2)
            .skip(DIVISOR_FROM)
            .filter_map(|w| u64::try_from(w[1].checked_sub(w[0])?).ok())
            .filter(|&step| step > 0)
            .fold(0, gcd);
        if u128::from(divisor) * u128::from(num) * DIVISOR_ABOVE.1
            > u128::from(den) * DIVISOR_ABOVE.0
        {
            return i64::try_from(divisor).ok();
        }
        let tick = num as f64 / den as f64;
        let mean_rate = (times.len() - 1) as f64 / ((last - first) as f64 * tick);
        let fitted = &times[FIT_FROM..];
        let mut best: Option<(f64, (u64, u64))> = None;
        for (p, q) in candidates() {
            let rate = p as f64 / q as f64;
            if rate < SLOWEST * mean_rate {
                continue;
            }
            let score = score(fitted, tick, rate);
            if score < best.map_or(CLOSE, |(least, _)| least) {
                best = Some((score, (p, q)));
            }
        }
        Some(best.map_or(1, |(_, (p, q))| {
            // One frame rounded down: q / (p * tick) = den * q / (num * p).
            let ticks = u128::from(den) * u128::from(q) / (u128::from(num) * u128::from(p));
            i64::try_from(ticks).unwrap_or(i64::MAX)
        }))
    }
}

/// How far `times` lie from the frame grid of `rate`: the variance of each
/// time's distance from its nearest frame boundary, in frames, the lower of
/// that and the same with the boundaries half a frame on (probes p79, p80: a
/// stream half a frame off the grid scores as one on it).
fn score(times: &[i64], tick: f64, rate: f64) -> f64 {
    let n = times.len() as f64;
    [0.0, 0.5]
        .into_iter()
        .map(|offset| {
            let (mut sum, mut squares) = (0.0, 0.0);
            for &t in times {
                let x = t as f64 * tick * rate + offset;
                let e = x - (x + 0.5).floor();
                sum += e;
                squares += e * e;
            }
            let mean = sum / n;
            squares / n - mean * mean
        })
        .fold(f64::INFINITY, f64::min)
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::{RateEstimate, TAKEN};

    const TB: (u32, u32) = (1, 90_000);

    fn estimate(times: impl IntoIterator<Item = i64>) -> Option<i64> {
        let mut e = RateEstimate::default();
        for t in times {
            e.packet(Some(t), true, false);
        }
        e.duration(TB)
    }

    /// Packets at 90000 + round(n * 90000 / rate), a tick added on odd
    /// packets so that the steps share no divisor.
    fn at_rate(p: i64, q: i64) -> Vec<i64> {
        (0..TAKEN as i64)
            .map(|n| 90_000 + (2 * n * 90_000 * q + p) / (2 * p) + n % 2)
            .collect()
    }

    #[test]
    fn a_divisor_longer_than_a_five_hundredth_is_the_duration() {
        // Probes p73 eq-180, eq-181 and p79 gcd-3100 (29 fps would fit
        // 3103).
        let equal = |s: i64| (0..TAKEN as i64).map(move |n| 90_000 + n * s);
        assert_eq!(estimate(equal(181)), Some(181));
        assert_eq!(estimate(equal(180)), Some(1));
        assert_eq!(estimate(equal(3100)), Some(3100));
    }

    #[test]
    fn each_candidate_is_picked_for_its_own_rate() {
        // Probe p79.
        assert_eq!(estimate(at_rate(30_000, 1001)), Some(3003));
        assert_eq!(estimate(at_rate(359, 12)), Some(3008));
        assert_eq!(estimate(at_rate(80, 1)), Some(1125));
        // Not candidates: 11/12 gives 22/12, 70 the fallback.
        assert_eq!(estimate(at_rate(11, 12)), Some(49_090));
        assert_eq!(estimate(at_rate(70, 1)), Some(1));
    }

    #[test]
    fn fewer_than_three_times_give_one_tick() {
        // Probe p42: streams of one, two and three packets 3003 apart.
        assert_eq!(estimate([]), None);
        assert_eq!(estimate([90_000]), Some(1));
        assert_eq!(estimate([90_000, 93_003]), Some(1));
        assert_eq!(estimate([90_000, 93_003, 96_006]), Some(3003));
    }

    #[test]
    fn a_score_below_one_hundredth_is_close_enough() {
        // Probe p83: every other packet A ticks late at 25 fps.
        let late = |a: i64| (0..TAKEN as i64).map(move |n| 90_000 + 3600 * n + a * (n % 2));
        assert_eq!(estimate(late(719)), Some(3600));
        assert_eq!(estimate(late(721)), Some(1));
    }

    #[test]
    fn the_end_of_the_stream_settles_the_estimate() {
        let mut e = RateEstimate::default();
        for t in [90_000, 93_003] {
            e.packet(Some(t), true, false);
        }
        assert!(!e.settled());
        e.end();
        assert!(e.settled());
        assert_eq!(e.duration(TB), Some(1));
    }

    #[test]
    fn packets_without_a_time_are_passed_over() {
        // Probes p81, p84: packets 2 to 6 go without a time after an order
        // count jump at packet 1; a DTS other than the PTS keeps them.
        let mut e = RateEstimate::default();
        for n in 0..30 {
            e.packet(Some(n), false, false);
            if n == 1 {
                e.untime_through(6);
            }
        }
        assert!(e.settled());
        assert_eq!(&e.times[..4], &[0, 1, 7, 8]);
        let mut e = RateEstimate::default();
        for n in 0..30 {
            e.packet(Some(n), true, false);
            if n == 1 {
                e.untime_through(6);
            }
        }
        assert_eq!(&e.times[..4], &[0, 1, 2, 3]);
    }
}
