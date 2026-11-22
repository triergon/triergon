use std::{ffi::OsString, io::Write, ops::Range, sync::atomic::AtomicBool};

use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities, SourceId,
    Step, WorkBudget,
};
use triergon_harness_protocol::digest::{fold_bytes, signed};
use triergon_isobmff::{
    CursorStatus, Next, Sample,
    compat::mov::{
        clock::ClockField,
        packets::PacketView,
        pcm::{PcmCursor, PcmLimits, PcmPacket, PcmProfile, PcmReader, PcmStream},
    },
    config::{ConfigurationLimits, check_dts_core_description},
    open::{Movie, MovieOpen, OpenLimits, OpenStep},
    payload::SampleReader,
    quicktime::{PcmEncoding, PcmSampleReader, SoundDescriptionV0},
};

use super::protocol::{Operation, Work};

#[path = "packets/encoded.rs"]
mod encoded;
#[path = "packets/recovery.rs"]
mod recovery;

const MAX_PACKET: usize = 16 << 20;
const EOF: i64 = -541478725;

#[derive(Clone, Copy)]
struct Source<'a> {
    bytes: &'a [u8],
    window: usize,
    quantum: u64,
}

impl Source<'_> {
    fn input(self) -> triergon_core::Result<InputCursor> {
        InputCursor::new(
            &Budget::new(64 << 20)?,
            SourceId(1),
            SourceCapabilities {
                length: Some(self.bytes.len() as u64),
                random_access: true,
            },
            self.window,
        )
    }

    fn supply(self, input: &mut InputCursor, request: InputRequest) -> triergon_core::Result<()> {
        let offset = usize::try_from(request.offset).map_err(|_| Error::Overflow)?;
        let Some(rest) = self.bytes.get(offset..).filter(|rest| !rest.is_empty()) else {
            return input.finish(request.source, request.token);
        };
        let bytes = &rest[..rest.len().min(request.maximum).min(self.window)];
        if bytes.len() <= InputCursor::INLINE_CAPACITY {
            input.provide_inline(request.source, request.token, request.offset, bytes)
        } else {
            let bytes = Bytes::copy_from_slice(input.budget(), bytes)?;
            input.provide_owned(InputWindow::new(
                request.source,
                request.token,
                request.offset,
                bytes,
            )?)
        }
    }

    fn read<T>(
        self,
        input: &mut InputCursor,
        mut poll: impl FnMut(&mut InputCursor, &mut WorkBudget) -> triergon_core::Result<Step<T>>,
    ) -> triergon_core::Result<T> {
        loop {
            match poll(input, &mut WorkBudget::new(self.quantum))? {
                Step::Ready(value) => return Ok(value),
                Step::NeedInput(request) => self.supply(input, request)?,
                Step::Yield => (),
                Step::End(_) => return Err(Error::Truncated),
            }
        }
    }

    fn open(self, input: &mut InputCursor, cancel: &AtomicBool) -> triergon_core::Result<Movie> {
        let mut open = MovieOpen::new_recovery(input, OpenLimits::default())?;
        loop {
            match open.advance(input, &mut WorkBudget::new(self.quantum), cancel)? {
                OpenStep::Ready => return open.finish(input),
                OpenStep::NeedInput(request) => self.supply(input, request)?,
                OpenStep::Root(_) | OpenStep::Yield => (),
            }
        }
    }

    fn sample(
        self,
        input: &mut InputCursor,
        sample: Sample,
        extent: Range<u64>,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<Bytes> {
        let mut reader = SampleReader::new(input, sample, extent, MAX_PACKET)?;
        Ok(self
            .read(input, |input, work| reader.poll(input, work, cancel))?
            .payload)
    }
}

struct Output<'a, 'b> {
    work: &'a mut Work,
    trace: Option<&'b mut dyn Write>,
}

impl Output<'_, '_> {
    fn row(&mut self, kind: u64, words: &[u64], payload: &[u8]) -> triergon_core::Result<()> {
        self.work.record(kind, words, payload);
        if let Some(out) = self.trace.as_deref_mut() {
            let mut render = || -> std::io::Result<()> {
                write!(out, "{kind}")?;
                for word in words {
                    write!(out, " {word}")?;
                }
                if !payload.is_empty() {
                    let mut digest = 0;
                    fold_bytes(&mut digest, payload);
                    write!(out, " bytes={} digest={digest}", payload.len())?;
                }
                writeln!(out)
            };
            render().map_err(|_| Error::Transport)?;
        }
        Ok(())
    }

    fn terminal(&mut self, code: Option<i64>) -> triergon_core::Result<()> {
        let kind = if code.is_none_or(|code| code == EOF) {
            7
        } else {
            8
        };
        self.row(kind, code.map(signed).as_slice(), &[])
    }

    fn packet(
        &mut self,
        index: usize,
        clocks: [i64; 3],
        position: u64,
        flags: u32,
        payload: &[u8],
    ) -> triergon_core::Result<()> {
        let [pts, dts, duration] = clocks;
        self.row(
            1,
            &[
                index as u64,
                u64::from(pts != i64::MIN),
                signed(pts),
                u64::from(dts != i64::MIN),
                signed(dts),
                signed(duration),
                position,
                u64::from(flags),
                payload.len() as u64,
            ],
            payload,
        )
    }

    fn view_packet(
        &mut self,
        index: usize,
        packet: &PacketView,
        payload: &[u8],
    ) -> triergon_core::Result<()> {
        self.packet(
            index,
            [
                packet.pts_field.signed_ticks(),
                packet.dts_field.signed_ticks(),
                packet.duration,
            ],
            packet.source_range.start,
            packet.flags,
            payload,
        )
    }

    fn string(&mut self, field: u64, bytes: &[u8]) -> triergon_core::Result<()> {
        self.row(3, &[field, bytes.len() as u64], bytes)
    }
}

fn extent(media: &[Range<u64>], range: &Range<u64>) -> triergon_core::Result<Range<u64>> {
    media
        .iter()
        .find(|r| r.start <= range.start && range.end <= r.end)
        .cloned()
        .ok_or(Error::Invalid("sample range outside media"))
}

#[derive(Clone, Copy)]
enum Mode {
    PcmSamples,
    SoundDescription,
    PcmPackets,
    Recover(recovery::Profile),
    Dts { oracle: bool, mismatched: bool },
    Encoded(encoded::Profile),
}

pub(super) fn operation<'a>(
    prefix: &str,
    bytes: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let [window, quantum, extra @ ..] = arguments else {
        return Err("expected window, work quantum and operation profile".into());
    };
    let positive = |value: &OsString| {
        value
            .to_str()
            .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| (1..=i32::MAX as u32).contains(value))
            .ok_or("invalid window or work quantum")
    };
    let source = Source {
        bytes,
        window: positive(window)? as usize,
        quantum: u64::from(positive(quantum)?),
    };
    let profile = match extra {
        [] => "",
        [argument] => argument.to_str().ok_or("non-UTF-8 profile")?,
        _ => return Err("too many operation arguments".into()),
    };
    let (name, mode) = match (prefix, profile) {
        ("qt-pcm-", "") => ("qt-pcm", Mode::PcmSamples),
        ("qt-sound-", "") => ("qt-sound", Mode::SoundDescription),
        ("mov-pcm-", "") => ("mov-pcm", Mode::PcmPackets),
        ("mov-recover-", "run") => ("mov-recover", Mode::Recover(recovery::Profile::Run)),
        ("mov-recover-", "direct") => ("mov-recover", Mode::Recover(recovery::Profile::Direct)),
        ("mov-recover-", "physical") => ("mov-recover", Mode::Recover(recovery::Profile::Physical)),
        ("dts-entry-" | "ddts-", "" | "mismatched-format") => {
            let oracle = prefix == "ddts-";
            (
                if oracle { "ddts" } else { "dts-entry" },
                Mode::Dts {
                    oracle,
                    mismatched: profile == "mismatched-format",
                },
            )
        }
        ("mov-encoded-", profile) => (
            "mov-encoded",
            Mode::Encoded(encoded::Profile::parse(profile)?),
        ),
        _ => return Err("unknown MOV operation profile".into()),
    };
    Ok(Operation {
        name,
        run: Box::new(move |work, trace| {
            let mut out = Output { work, trace };
            match run(source, mode, &mut out) {
                Ok(()) => Ok(()),
                Err(error @ (Error::Invalid(_) | Error::Unsupported(_) | Error::Truncated)) => {
                    if out.trace.is_some() {
                        eprintln!("{error:?}");
                    }
                    out.row(8, &[], &[]).map_err(|e| format!("{e:?}"))
                }
                Err(error) => Err(format!("{error:?}")),
            }
        }),
    })
}

fn run(source: Source<'_>, mode: Mode, out: &mut Output<'_, '_>) -> triergon_core::Result<()> {
    let mut input = source.input()?;
    let cancel = AtomicBool::new(false);
    if let Mode::Recover(direct) = mode {
        return recovery::run(source, &mut input, &cancel, direct, out);
    }
    let movie = source.open(&mut input, &cancel)?;
    match mode {
        Mode::PcmSamples => pcm_samples(source, &mut input, &cancel, &movie, out),
        Mode::SoundDescription => sound_descriptions(&movie, out),
        Mode::PcmPackets => pcm_packets(source, &mut input, &cancel, &movie, out),
        Mode::Dts { oracle, mismatched } => dts_entries(&movie, oracle, mismatched, out),
        Mode::Encoded(profile) => encoded::run(source, &mut input, &cancel, &movie, profile, out),
        Mode::Recover(_) => unreachable!("recovery uses its own table builder"),
    }
}

fn sound_descriptions(movie: &Movie, out: &mut Output<'_, '_>) -> triergon_core::Result<()> {
    for (index, track) in movie.tracks().iter().enumerate() {
        if track.header().handler != *b"soun" {
            continue;
        }
        for entry in track.table().descriptions() {
            let key = [index as u64, u64::from(entry.index)];
            let Ok(sound) = SoundDescriptionV0::parse(track.header(), entry) else {
                out.row(8, &key, &[])?;
                continue;
            };
            let (encoding, frame_bytes) = sound.pcm_layout().map_or((0, 0), |layout| {
                let (id, width) = match layout.encoding() {
                    PcmEncoding::Unsigned8 => (1, 1),
                    PcmEncoding::Signed16BigEndian => (2, 2),
                };
                (id, width * u64::from(layout.channels()))
            });
            out.row(
                3,
                &[
                    key[0],
                    key[1],
                    u64::from(u32::from_be_bytes(entry.format)),
                    u64::from(sound.revision()),
                    u64::from(u32::from_be_bytes(sound.vendor())),
                    u64::from(sound.channels()),
                    u64::from(sound.sample_bits()),
                    signed(i64::from(sound.compression_id())),
                    u64::from(sound.packet_size()),
                    u64::from(sound.sample_rate_16_16()),
                    encoding,
                    frame_bytes,
                ],
                &[],
            )?;
        }
    }
    Ok(())
}

fn dts_entries(
    movie: &Movie,
    oracle: bool,
    mismatched: bool,
    out: &mut Output<'_, '_>,
) -> triergon_core::Result<()> {
    out.row(2, &[movie.tracks().len() as u64], &[])?;
    for (index, track) in movie.tracks().iter().enumerate() {
        if track.header().handler != *b"soun" {
            continue;
        }
        for entry in track.table().descriptions() {
            let altered = mismatched.then(|| {
                let mut altered = entry.clone();
                altered.format = *b"dtsh";
                altered
            });
            let result = check_dts_core_description(
                altered.as_ref().unwrap_or(entry),
                ConfigurationLimits::default(),
            );
            let (verdict, rate, frequency, samples) = match result {
                Ok(Some(dts)) => {
                    let (frequency, samples) = dts.access_unit_clock().unwrap_or_default();
                    (1, dts.sample_entry_rate_hz(), frequency, samples)
                }
                Ok(None) => (0, 0, 0, 0),
                Err(_) => (2, 0, 0, 0),
            };
            if oracle {
                out.row(
                    2,
                    &[
                        index as u64,
                        u64::from(entry.index),
                        verdict,
                        rate.into(),
                        frequency.into(),
                        samples.into(),
                    ],
                    &[],
                )?;
            } else {
                out.row(
                    2,
                    &[index as u64, verdict, rate.into(), samples.into()],
                    &[],
                )?;
            }
        }
    }
    Ok(())
}

fn pcm_samples(
    source: Source<'_>,
    input: &mut InputCursor,
    cancel: &AtomicBool,
    movie: &Movie,
    out: &mut Output<'_, '_>,
) -> triergon_core::Result<()> {
    for (index, track) in movie.tracks().iter().enumerate() {
        if track.header().handler != *b"soun" {
            continue;
        }
        let descriptions = track.table().descriptions();
        let first = descriptions
            .first()
            .ok_or(Error::Invalid("no sound description"))?;
        let sound = SoundDescriptionV0::parse(track.header(), first)?;
        let layout = sound.pcm_layout()?;
        let mut digest = 0;
        let (mut count, mut frames) = (0, 0);
        let mut cursor = track.table().cursor();
        loop {
            let sample = match cursor.poll(&mut WorkBudget::new(source.quantum))? {
                Next::Item(sample) => sample,
                Next::Yield => continue,
                Next::End => break,
            };
            let entry = descriptions
                .get(sample.description_index as usize - 1)
                .ok_or(Error::Invalid("no referenced sound description"))?;
            let description = SoundDescriptionV0::parse(track.header(), entry)?;
            let range = extent(movie.media(), &sample.range)?;
            let mut reader = PcmSampleReader::new(input, description, sample, range, MAX_PACKET)?;
            let packet = source.read(input, |input, work| reader.poll(input, work, cancel))?;
            let bytes = packet.packet().payload.as_ref();
            fold_bytes(&mut digest, bytes);
            count += bytes.len() as u64;
            frames += packet.complete_frames() as u64;
        }
        let encoding = match layout.encoding() {
            PcmEncoding::Unsigned8 => 1,
            PcmEncoding::Signed16BigEndian => 2,
        };
        let bits = u64::from(sound.sample_bits());
        let channels = u64::from(layout.channels());
        out.row(
            2,
            &[
                index as u64,
                encoding,
                channels,
                u64::from(layout.sample_rate_16_16() >> 16),
                bits,
                bits * channels / 8,
            ],
            &[],
        )?;
        out.row(7, &[frames, count, digest], &[])?;
        out.work.bytes += count;
    }
    Ok(())
}

fn pcm_head(
    source: Source<'_>,
    cursor: &mut PcmCursor<'_>,
    input: &mut InputCursor,
    cancel: &AtomicBool,
) -> triergon_core::Result<Option<PcmPacket>> {
    loop {
        match cursor.poll(input, &mut WorkBudget::new(source.quantum), cancel)? {
            CursorStatus::Yield => (),
            CursorStatus::Ready => return cursor.take_packet(input, cancel).map(Some),
            CursorStatus::End => return Ok(None),
        }
    }
}

fn pcm_packets(
    source: Source<'_>,
    input: &mut InputCursor,
    cancel: &AtomicBool,
    movie: &Movie,
    out: &mut Output<'_, '_>,
) -> triergon_core::Result<()> {
    let media = movie
        .media()
        .first()
        .ok_or(Error::Invalid("no PCM media"))?;
    let mut tracks = Vec::new();
    for track in movie.tracks() {
        PcmStream::new(
            track.header(),
            track.table(),
            movie.major_brand(),
            PcmProfile,
        )?;
        let mut cursor = PcmCursor::new(
            track.header(),
            track.table(),
            input,
            media.clone(),
            PcmLimits::default(),
            PcmProfile,
        )?;
        let head = pcm_head(source, &mut cursor, input, cancel)?;
        tracks.push((cursor, head, track.header().time_base()?));
    }
    loop {
        let mut first = None;
        for (index, (_, head, base)) in tracks.iter().enumerate() {
            if let Some(head) = head {
                let clock = ClockField::from_exact_ticks(i128::from(head.dts()))?;
                let clock =
                    triergon_isobmff::compat::mov::clock::SchedulingClock::from_demux_clock(
                        clock, *base,
                    )?;
                let position = head.demux_range().start;
                if first.is_none_or(|(_, previous, previous_position)| {
                    pcm_precedes(clock, position, previous, previous_position)
                }) {
                    first = Some((index, clock, position));
                }
            }
        }
        let Some((index, _, _)) = first else {
            return out.terminal(None);
        };
        let (cursor, head, _) = &mut tracks[index];
        let plan = head
            .take()
            .ok_or(Error::Invalid("missing PCM packet head"))?;
        let mut reader = PcmReader::new(input, plan, MAX_PACKET)?;
        let packet = source.read(input, |input, work| reader.poll(input, work, cancel))?;
        let plan = packet.plan();
        out.packet(
            index,
            [plan.pts(), plan.dts(), plan.duration().into()],
            plan.demux_range().start,
            plan.flags(),
            packet.payload().as_ref(),
        )?;
        *head = pcm_head(source, cursor, input, cancel)?;
    }
}

fn pcm_precedes(
    clock: triergon_isobmff::compat::mov::clock::SchedulingClock,
    offset: u64,
    previous: triergon_isobmff::compat::mov::clock::SchedulingClock,
    previous_offset: u64,
) -> bool {
    use triergon_isobmff::compat::mov::clock::SchedulingClock::ExactMicroseconds;
    match (clock, previous) {
        (ExactMicroseconds(clock), ExactMicroseconds(previous)) => {
            if clock.abs_diff(previous) > 1_000_000 {
                clock < previous
            } else {
                offset < previous_offset
            }
        }
        _ => false,
    }
}
