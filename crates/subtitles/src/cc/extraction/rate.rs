//! The frame duration of a stream whose packets and SPS give none, estimated
//! from the times of its first packets as the reference does (probes p58,
//! p71-p73, p77-p83, p96-p100).
//!
//! The estimate takes the times of the first packets that carry one: 21 of
//! them when the time base is 1/2000 s or finer, 41 when it is coarser. When
//! the steps from the fourth time on share a divisor longer than 1/500 s,
//! that divisor is the duration. Otherwise each candidate frame rate is scored
//! by how far the times from the second on lie from its frame grid, and the
//! duration is one frame at the best rate, rounded down to the time base; with
//! no candidate close enough, or fewer than three times, it is one tick of the
//! time base.
//!
//! The times come from the packets read while fewer than 5,000,000 bytes of
//! packets came before them (probes p93b, p100): the estimate settles once it
//! has its times, once that much has been read, or at the end of the stream.
//! Frames waiting for it hold only their caption bytes, not the packets the
//! reference keeps until then.
//!
//! Packets of a length-prefixed (Matroska) stream that carry only
//! presentation times are taken in [`Shown`] order instead.

/// The most times the estimate takes: those of a time base coarser than
/// 1/2000 s (probes p96, p98).
const TAKEN_MAX: usize = 41;

/// The times the estimate takes in a time base of 1/2000 s or finer (probes
/// p71, p77; p96, p98: TimestampScale 500000 ns takes 21, 500001 ns 41).
const TAKEN_FINE: usize = 21;

/// The estimate takes times from the packets read before this many bytes of
/// packets (probes p93b, p100: untimed access units of 1000, 2000 and 4000
/// bytes up to 4,980,000 bytes in all leave room for the times after them,
/// 5,000,000 do not; p100c, p100d: a packet that starts below 5,000,000 bytes
/// counts, also when it ends above, one that starts at it does not).
const LOOK_BYTES: u64 = 5_000_000;

/// The times the estimate takes in `time_base`.
fn taken(time_base: (u32, u32)) -> usize {
    if u64::from(time_base.0) * 2000 <= u64::from(time_base.1) {
        TAKEN_FINE
    } else {
        TAKEN_MAX
    }
}

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
fn candidates() -> impl Iterator<Item = (u32, u32)> {
    let twelfths = (12..=360).map(|k| (k, 12));
    let whole = (31..=60).chain([80, 120, 240]).map(|k| (k, 1));
    let ntsc = [12, 15, 24, 30, 48, 60].map(|k| (k * 1000, 1001));
    twelfths.chain(whole).chain(ntsc)
}

/// The times of the first packets.
#[derive(Clone, Copy, Debug)]
pub(super) struct RateEstimate {
    times: [i64; TAKEN_MAX],
    taken: usize,
    /// The times the estimate takes.
    wanted: usize,
    /// Bytes of the packets seen.
    bytes: u64,
    /// The stream has ended.
    ended: bool,
}

impl RateEstimate {
    /// An estimate of packet times in `time_base`.
    pub(super) fn new(time_base: (u32, u32)) -> Self {
        Self {
            times: [0; TAKEN_MAX],
            taken: 0,
            wanted: taken(time_base),
            bytes: 0,
            ended: false,
        }
    }

    /// Takes the next packet: its size in bytes and its time, `None` for a
    /// packet that gives the estimate none.
    pub(super) fn packet(&mut self, bytes: usize, time: Option<i64>) {
        if let Some(time) = time
            && self.bytes < LOOK_BYTES
            && self.taken < self.wanted
        {
            self.times[self.taken] = time;
            self.taken += 1;
        }
        self.bytes = self.bytes.saturating_add(bytes as u64);
    }

    /// The stream ends: the estimate takes the times it has.
    pub(super) fn end(&mut self) {
        self.ended = true;
    }

    /// Whether the estimate has every time it takes: all of them, those of
    /// a stream that ended, or those of the packets before [`LOOK_BYTES`].
    pub(super) fn settled(&self) -> bool {
        self.taken >= self.wanted || self.ended || self.bytes >= LOOK_BYTES
    }

    /// The estimated duration in ticks of `time_base`: one tick with fewer
    /// than three times (probe p42: one or two packets last 1, three 3003;
    /// probe p93: packets without any time last 1).
    pub(super) fn duration(&self, time_base: (u32, u32)) -> i64 {
        estimate(&self.times[..self.taken], time_base)
    }
}

/// The duration estimated from `times`.
fn estimate(times: &[i64], time_base: (u32, u32)) -> i64 {
    let [first, .., last] = *times else {
        return 1;
    };
    if times.len() < 3 {
        return 1;
    }
    let (num, den) = (u64::from(time_base.0.max(1)), u64::from(time_base.1.max(1)));
    let divisor = times
        .windows(2)
        .skip(DIVISOR_FROM)
        .filter_map(|w| u64::try_from(w[1].checked_sub(w[0])?).ok())
        .filter(|&step| step > 0)
        .fold(0, gcd);
    if u128::from(divisor) * u128::from(num) * DIVISOR_ABOVE.1 > u128::from(den) * DIVISOR_ABOVE.0 {
        return i64::try_from(divisor).unwrap_or(i64::MAX);
    }
    let tick = num as f64 / den as f64;
    // Times too far apart for a step leave no candidate out.
    let mean_rate = last
        .checked_sub(first)
        .map_or(0.0, |span| (times.len() - 1) as f64 / (span as f64 * tick));
    // The fitted times in seconds, as each candidate scales them.
    let mut seconds = [0.0; TAKEN_MAX];
    for (s, &t) in seconds.iter_mut().zip(&times[FIT_FROM..]) {
        *s = t as f64 * tick;
    }
    let seconds = &seconds[..times.len() - FIT_FROM];
    let mut best: Option<(f64, (u32, u32))> = None;
    for (p, q) in candidates() {
        let rate = f64::from(p) / f64::from(q);
        if rate < SLOWEST * mean_rate {
            continue;
        }
        let score = score(seconds, rate);
        if score < best.map_or(CLOSE, |(least, _)| least) {
            best = Some((score, (p, q)));
        }
    }
    best.map_or(1, |(_, (p, q))| {
        // One frame rounded down: q / (p * tick) = den * q / (num * p).
        let ticks = u128::from(den) * u128::from(q) / (u128::from(num) * u128::from(p));
        i64::try_from(ticks).unwrap_or(i64::MAX)
    })
}

/// The packets [`Shown`] keeps the presentation times of: those of a start
/// hold, its times and the packets a B picture adds, with room to spare.
const SHOWN_CAP: usize = 128;

/// The estimate of an H.264 stream in length-prefixed (Matroska) framing
/// whose packets carry presentation times only, without durations or VUI
/// timing (probes p86, p96-p98; `h264-nodur-mkv`).
///
/// The packets up to the first picture output give no time. From the next
/// packet on, the estimate takes the presentation times of 21 or 41 packets
/// ([`taken`]), in presentation order among the packets read by then. A first
/// B picture among those packets makes the estimate wait one packet more when
/// it is in order and two when it is displaced; a first B picture before them
/// (in the start hold) adds none, and later B pictures add none (probes p97,
/// p98: B pictures at index 8 to 47, one or two at once). Until the estimate
/// is settled each frame lasts 0 when a tick of the time base is 1/1000 s or
/// shorter and one tick otherwise (probe p96: TimestampScale 1000000 ns gives
/// 0, 1000001 ns 1), and at least one picture waits in the buffer (probes p96,
/// p97: a B picture one below the last at index 21 to 27, or 47 at the coarser
/// time base, is kept).
#[derive(Clone, Copy, Debug)]
pub(super) struct Shown {
    pts: [i64; SHOWN_CAP],
    len: usize,
    /// Pictures seen.
    seen: u32,
    /// The first packet whose time counts: the one after the first picture
    /// output.
    start: Option<u32>,
    /// The packets the first B picture adds, once it has come.
    lag: Option<u32>,
    wanted: u32,
    /// The duration of a frame before the estimate is settled.
    early: i64,
    /// The estimate, once settled.
    settled: Option<i64>,
}

impl Shown {
    pub(super) fn new(time_base: (u32, u32)) -> Self {
        let fine = u64::from(time_base.0) * 1000 <= u64::from(time_base.1);
        Self {
            pts: [0; SHOWN_CAP],
            len: 0,
            seen: 0,
            start: None,
            lag: None,
            wanted: taken(time_base) as u32,
            early: i64::from(!fine),
            settled: None,
        }
    }

    /// The duration of the next picture's frame while the estimate is not
    /// settled.
    pub(super) fn early(&self) -> Option<i64> {
        self.settled.is_none().then_some(self.early)
    }

    /// Takes the next picture: its presentation time, and for a B picture
    /// whether it is displaced (below a recent picture).
    pub(super) fn picture(&mut self, pts: Option<i64>, b_displaced: Option<bool>) {
        let index = self.seen;
        self.seen = self.seen.saturating_add(1);
        if self.settled.is_some() {
            return;
        }
        if let Some(pts) = pts
            && self.len < SHOWN_CAP
        {
            self.pts[self.len] = pts;
            self.len += 1;
        }
        if let Some(displaced) = b_displaced
            && self.lag.is_none()
        {
            self.lag = Some(match self.start {
                Some(start) if index >= start => 1 + u32::from(displaced),
                _ => 0,
            });
        }
    }

    /// A picture was output: the first starts the times.
    pub(super) fn output(&mut self) {
        if self.start.is_none() {
            self.start = Some(self.seen);
        }
    }

    /// Whether the estimate settles at the picture last taken.
    fn settles(&self) -> bool {
        let Some(start) = self.start else {
            return self.len == SHOWN_CAP;
        };
        let last = start
            .saturating_add(self.wanted - 1)
            .saturating_add(self.lag.unwrap_or(0));
        self.seen > last || self.len == SHOWN_CAP
    }

    /// Whether a picture still has to wait in the buffer: the estimate is
    /// not settled by the picture last taken.
    pub(super) fn waits(&self) -> bool {
        self.settled.is_none() && !self.settles()
    }

    /// Settles the estimate when the picture last taken completes it.
    pub(super) fn settle(&mut self, time_base: (u32, u32)) {
        if self.settled.is_some() || !self.settles() {
            return;
        }
        let times = &mut self.pts[..self.len];
        times.sort_unstable();
        let from = self
            .start
            .map_or(times.len(), |s| (s as usize).min(times.len()));
        let to = (from + self.wanted as usize).min(times.len());
        self.settled = Some(estimate(&times[from..to], time_base));
    }

    /// The estimate, once settled.
    pub(super) fn duration(&self) -> Option<i64> {
        self.settled
    }
}

/// How far the times `seconds` lie from the frame grid of `rate`: the
/// variance of each time's distance from its nearest frame boundary, in
/// frames, the lower of that and the same with the boundaries half a frame on
/// (probes p79, p80: a stream half a frame off the grid scores as one on it).
fn score(seconds: &[f64], rate: f64) -> f64 {
    let n = seconds.len() as f64;
    [0.0, 0.5]
        .into_iter()
        .map(|offset| {
            let (mut sum, mut squares) = (0.0, 0.0);
            for &s in seconds {
                let x = s * rate + offset;
                let e = x - floor(x + 0.5);
                sum += e;
                squares += e * e;
            }
            let mean = sum / n;
            squares / n - mean * mean
        })
        .fold(f64::INFINITY, f64::min)
}

/// `y.floor()`. 32-bit Arm has no instruction for it, and the library call
/// it becomes there would dominate the fit; within the range of `i32` the
/// conversion to it, which truncates, gives the same value.
#[cfg(target_arch = "arm")]
fn floor(y: f64) -> f64 {
    if y.abs() < 2_147_483_648.0 {
        let t = f64::from(y as i32);
        (if t > y { t - 1.0 } else { t }).copysign(y)
    } else {
        y.floor()
    }
}

#[cfg(not(target_arch = "arm"))]
fn floor(y: f64) -> f64 {
    y.floor()
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::{LOOK_BYTES, RateEstimate, Shown, TAKEN_FINE as TAKEN};

    const TB: (u32, u32) = (1, 90_000);

    fn estimate(times: impl IntoIterator<Item = i64>) -> i64 {
        let mut e = RateEstimate::new(TB);
        for t in times {
            e.packet(100, Some(t));
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
        assert_eq!(estimate(equal(181)), 181);
        assert_eq!(estimate(equal(180)), 1);
        assert_eq!(estimate(equal(3100)), 3100);
    }

    #[test]
    fn each_candidate_is_picked_for_its_own_rate() {
        // Probe p79.
        assert_eq!(estimate(at_rate(30_000, 1001)), 3003);
        assert_eq!(estimate(at_rate(359, 12)), 3008);
        assert_eq!(estimate(at_rate(80, 1)), 1125);
        // Not candidates: 11/12 gives 22/12, 70 the fallback.
        assert_eq!(estimate(at_rate(11, 12)), 49_090);
        assert_eq!(estimate(at_rate(70, 1)), 1);
    }

    #[test]
    fn fewer_than_three_times_give_one_tick() {
        // Probe p42: streams of one, two and three packets 3003 apart; probe
        // p93: none.
        assert_eq!(estimate([]), 1);
        assert_eq!(estimate([90_000]), 1);
        assert_eq!(estimate([90_000, 93_003]), 1);
        assert_eq!(estimate([90_000, 93_003, 96_006]), 3003);
    }

    #[test]
    fn a_score_below_one_hundredth_is_close_enough() {
        // Probe p83: every other packet A ticks late at 25 fps.
        let late = |a: i64| (0..TAKEN as i64).map(move |n| 90_000 + 3600 * n + a * (n % 2));
        assert_eq!(estimate(late(719)), 3600);
        assert_eq!(estimate(late(721)), 1);
    }

    #[test]
    fn floor_rounds_down_as_f64_floor_does() {
        let small = [0.0, -0.0, 0.25, -0.25, 1.0, -1.0, 2.5, -2.5];
        let large = [
            1e9 + 0.5,
            -1e9 - 0.5,
            2_147_483_647.5,
            -2_147_483_648.5,
            3e12,
        ];
        for y in small
            .into_iter()
            .chain(large)
            .chain([f64::MAX, f64::MIN_POSITIVE])
        {
            assert_eq!(super::floor(y).to_bits(), y.floor().to_bits(), "{y}");
        }
        assert!(super::floor(f64::NAN).is_nan());
    }

    #[test]
    fn times_too_far_apart_for_a_step_leave_no_candidate_out() {
        // The span from the first time to the last does not fit a time.
        let times = (0..TAKEN as i64).map(|n| if n == 0 { i64::MIN } else { i64::MAX - n });
        assert!(estimate(times) > 0);
    }

    #[test]
    fn the_end_of_the_stream_settles_the_estimate() {
        let mut e = RateEstimate::new(TB);
        for t in [90_000, 93_003] {
            e.packet(100, Some(t));
        }
        assert!(!e.settled());
        e.end();
        assert!(e.settled());
        assert_eq!(e.duration(TB), 1);
    }

    #[test]
    fn times_count_from_packets_starting_below_five_million_bytes() {
        // Probes p100c, p100d: after 4,996 untimed units of 1000 bytes and
        // one of 2999 (4,997,999 bytes), the third timed unit starts at
        // 4,999,999 and counts; after 4,998,000 bytes it starts at 5,000,000
        // and does not, which leaves two times: one tick.
        for (untimed, expected) in [(4_997_999u64, 3003), (4_998_000, 1)] {
            let mut e = RateEstimate::new(TB);
            e.packet(usize::try_from(untimed).unwrap(), None);
            for n in 0..60 {
                e.packet(1000, Some(90_000 + 3003 * n));
            }
            assert!(e.settled());
            assert_eq!(e.duration(TB), expected, "{untimed}");
        }
        let mut e = RateEstimate::new(TB);
        e.packet(usize::try_from(LOOK_BYTES - 1).unwrap(), None);
        assert!(!e.settled());
        e.packet(1, None);
        assert!(e.settled());
    }

    #[test]
    fn a_time_base_coarser_than_a_two_thousandth_takes_41_times() {
        // Probes p96, p98: TimestampScale 500000 ns takes 21 times, 500001 ns
        // 41; at 1 ms, 41 steps of 40 with one of 20 at the 30th.
        let times = |n: i64| 1000 + 40 * n - if n >= 30 { 20 } else { 0 };
        let mut e = RateEstimate::new((1, 1000));
        let mut fine = RateEstimate::new((1, 2000));
        for n in 0..60 {
            e.packet(100, Some(times(n)));
            fine.packet(100, Some(times(n)));
        }
        assert_eq!(e.duration((1, 1000)), 20);
        assert_eq!(fine.duration((1, 2000)), 40);
    }

    /// The durations the [`Shown`] estimate gives pictures with the given
    /// presentation times, the first output at `first_output`, and B
    /// pictures (index, displaced).
    fn shown(
        time_base: (u32, u32),
        pts: &[i64],
        first_output: u32,
        bs: &[(u32, bool)],
    ) -> Vec<i64> {
        let mut s = Shown::new(time_base);
        let mut out = Vec::new();
        for (n, &t) in (0u32..).zip(pts) {
            let early = s.early();
            let b = bs.iter().find(|&&(at, _)| at == n).map(|&(_, d)| d);
            s.picture(Some(t), b);
            if n >= first_output {
                s.output();
            }
            s.settle(time_base);
            out.push(early.or(s.duration()).unwrap());
        }
        out
    }

    #[test]
    fn presentation_times_count_from_the_first_output_on() {
        // Probes p96, p98 (25 fps, first output at index 6): 28 frames of 0
        // at 0.1 ms, 48 at 1 ms; one tick of 1.000001 ms or more lasts 1
        // until then.
        let pts = |tick_ns: i64| {
            (0..90)
                .map(move |n| (1_000_000_000 + n * 40_000_000) / tick_ns)
                .collect::<Vec<_>>()
        };
        let lead = |d: &[i64]| d.iter().take_while(|&&x| x == d[0]).count();
        let fine = shown((1, 10_000), &pts(100_000), 6, &[]);
        assert_eq!((lead(&fine), fine[0], fine[89]), (28, 0, 400));
        let coarse = shown((1, 1000), &pts(1_000_000), 6, &[]);
        assert_eq!((lead(&coarse), coarse[0], coarse[89]), (48, 0, 40));
        let one = shown((1, 500), &pts(2_000_000), 6, &[]);
        assert_eq!((lead(&one), one[0], one[89]), (48, 1, 20));
        // Probe p97: a first B picture at 10 adds one packet in order, two
        // displaced; probe p98: a second adds none.
        assert_eq!(
            lead(&shown((1, 10_000), &pts(100_000), 6, &[(10, false)])),
            29
        );
        assert_eq!(
            lead(&shown((1, 10_000), &pts(100_000), 6, &[(10, true)])),
            30
        );
        assert_eq!(
            lead(&shown(
                (1, 10_000),
                &pts(100_000),
                6,
                &[(10, false), (15, true)]
            )),
            29
        );
        // Probe p98: a B picture in the start hold adds none (first output at
        // 8 after a B picture at 2: 30 frames).
        assert_eq!(
            lead(&shown((1, 10_000), &pts(100_000), 8, &[(2, true)])),
            30
        );
    }

    #[test]
    fn presentation_times_are_taken_in_presentation_order() {
        // Probe p97 (half): a B picture in order at 10 half a frame off enters
        // the divisor (200); displaced one below at 10 it sorts before the
        // fourth time and does not (400).
        let mut pts: Vec<i64> = (0..90).map(|n| 10_000 + 400 * n).collect();
        pts[10] = pts[9] + 200;
        let lead = shown((1, 10_000), &pts, 6, &[(10, false)]);
        assert_eq!(lead[89], 200);
        let mut pts: Vec<i64> = (0..90).map(|n| 10_000 + 400 * n).collect();
        pts[10] = pts[9] - 200;
        assert_eq!(shown((1, 10_000), &pts, 6, &[(10, true)])[89], 400);
    }
}
