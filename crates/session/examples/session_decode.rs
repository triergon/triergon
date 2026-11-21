//! Drive bounded audio and video sessions on a stream and write their device output.
//! The packet list supplies ranges for elementary formats without a file reader.
use std::io::{BufWriter, Write};
use std::path::Path;

use triergon::{Budget, ContainerReader, MediaKind, PacketInfo, open_blocking};
use triergon_core::{BudgetVec, Rounding};
use triergon_session::mix::Downmix;
use triergon_session::{
    AudioCodec, AudioSession, Canvas, Discard, FeedError, OnRefusal, PcmBlock, PcmForm, Planar,
    Planes, QueueLimits, SessionState, StreamSelection, VideoCodec, VideoSession, audio_codec,
    audio_configuration, audio_decoder,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Options<'a> {
    lane: &'a str,
    form: &'a str,
    indexes: Vec<usize>,
    packets: Option<&'a str>,
    refusal: OnRefusal,
}

impl<'a> Options<'a> {
    fn parse(args: &'a [String]) -> Result<Self> {
        let mut options = Self {
            lane: "",
            form: "planar",
            indexes: Vec::new(),
            packets: None,
            refusal: OnRefusal::Fail,
        };
        let mut pairs = args.chunks_exact(2);
        for pair in &mut pairs {
            match (pair[0].as_str(), pair[1].as_str()) {
                ("--lane", lane) => options.lane = lane,
                ("--form", form @ ("planar" | "device")) => options.form = form,
                ("--packets", path) => options.packets = Some(path),
                ("--on-refusal", "skip") => options.refusal = OnRefusal::Skip,
                ("--reset-at", indexes) => {
                    options.indexes = indexes
                        .split(',')
                        .map(str::parse)
                        .collect::<std::result::Result<_, _>>()?;
                }
                _ => return Err(format!("unknown session option {}", pair[0]).into()),
            }
        }
        if !pairs.remainder().is_empty() || options.lane.is_empty() {
            return Err("session options require a value and --lane".into());
        }
        Ok(options)
    }
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let [_, input, output, args @ ..] = args.as_slice() else {
        return Err("usage: session_decode INPUT OUTPUT --lane CODEC [--form planar|device] [--packets FILE] [--on-refusal skip] [--reset-at N,...]".into());
    };
    let options = Options::parse(args)?;
    let mut output: Box<dyn Write> = if output == "-" {
        Box::new(BufWriter::new(std::io::stdout().lock()))
    } else {
        Box::new(BufWriter::new(std::fs::File::create(output)?))
    };
    match (options.lane, options.form) {
        ("h264", _) => video(input, VideoCodec::H264, &options, &mut output)?,
        ("hevc", _) => video(input, VideoCodec::Hevc, &options, &mut output)?,
        (_, "device") => audio(input, Downmix::default(), &options, &mut output)?,
        _ => audio(input, Planar, &options, &mut output)?,
    }
    output.flush()?;
    Ok(())
}

fn limits(base: QueueLimits) -> QueueLimits {
    // One unit makes both the retained-unit retry and each block's start visible.
    QueueLimits { units: 1, ..base }
}

fn report(pts: Option<i64>, rate: u32, count: usize) {
    let pts = pts.map_or_else(|| "null".into(), |v| v.to_string());
    eprintln!("{{\"pts_ns\":{pts},\"sample_rate\":{rate},\"nb_samples\":{count}}}");
}

trait OutputForm: PcmForm {
    fn emit(samples: &Self::Samples, sink: &mut dyn Write) -> std::io::Result<usize>;
    fn layout(samples: &Self::Samples) -> (usize, String);
    fn load(
        session: &mut AudioSession<Self>,
        codec: AudioCodec,
        configuration: &[u8],
    ) -> Result<()>
    where
        Self: Sized;
}

impl OutputForm for Planar {
    fn emit(samples: &Planes, sink: &mut dyn Write) -> std::io::Result<usize> {
        let count = samples.samples_per_channel();
        let mut bytes = Vec::with_capacity(count * samples.planes.len() * size_of::<f32>());
        for i in 0..count {
            for plane in &samples.planes {
                bytes.extend_from_slice(&plane[i].to_le_bytes());
            }
        }
        sink.write_all(&bytes)?;
        Ok(count)
    }

    fn layout(samples: &Planes) -> (usize, String) {
        (samples.layout.channels(), samples.layout.name())
    }

    fn load(
        session: &mut AudioSession<Self>,
        codec: AudioCodec,
        configuration: &[u8],
    ) -> Result<()> {
        session.load(Some(audio_decoder(codec, configuration)?));
        Ok(())
    }
}

impl OutputForm for Downmix {
    fn emit(samples: &Vec<i16>, sink: &mut dyn Write) -> std::io::Result<usize> {
        let bytes: Vec<_> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        sink.write_all(&bytes)?;
        Ok(samples.len() / 2)
    }

    fn layout(_: &Vec<i16>) -> (usize, String) {
        (2, "stereo".into())
    }

    fn load(
        session: &mut AudioSession<Self>,
        codec: AudioCodec,
        configuration: &[u8],
    ) -> Result<()> {
        StreamSelection {
            video: VideoCodec::H264,
            audio: Some(codec),
        }
        .load(
            &mut VideoSession::new(QueueLimits::VIDEO),
            session,
            configuration,
        )?;
        Ok(())
    }
}

#[derive(Default)]
struct AudioOutput {
    format: Option<(u32, usize, String)>,
    blocks: usize,
    samples: usize,
}

impl AudioOutput {
    fn pull<F: OutputForm>(
        &mut self,
        session: &mut AudioSession<F>,
        sink: &mut dyn Write,
    ) -> Result<bool> {
        let mut block = PcmBlock::default();
        let consumed = session.pull(None, 0, &mut block);
        let count = F::emit(&block.samples, sink)?;
        if count != 0 {
            let (channels, layout) = F::layout(&block.samples);
            self.format
                .get_or_insert((block.sample_rate, channels, layout));
            self.blocks += 1;
            self.samples += count;
            report(block.pts, block.sample_rate, count);
        }
        if let SessionState::Failed(error) = session.state() {
            return Err(error.into());
        }
        Ok(consumed != 0 || count != 0)
    }
}

struct AudioRun<'a, F: OutputForm> {
    session: AudioSession<F>,
    codec: AudioCodec,
    configuration: BudgetVec<u8>,
    output: AudioOutput,
    options: &'a Options<'a>,
    sink: &'a mut dyn Write,
    index: usize,
    refusals: u64,
}

impl<F: OutputForm> AudioRun<'_, F> {
    fn feed(&mut self, bytes: &[u8], pts: Option<i64>, discard: Discard) -> Result<()> {
        if self.options.indexes.contains(&self.index) {
            while self.output.pull(&mut self.session, self.sink)? {}
            self.refusals += self.session.refusals();
            self.session.stop();
            F::load(&mut self.session, self.codec, &self.configuration)?;
        }
        loop {
            match self.session.feed_discarding(bytes, pts, discard) {
                Ok(()) => break,
                Err(FeedError::Backpressure)
                    if self.output.pull(&mut self.session, self.sink)? => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.index += 1;
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        // Publish the final timed packet before asking for any decoder drain.
        while self.output.pull(&mut self.session, self.sink)? {}
        self.session.end_of_stream();
        while self.output.pull(&mut self.session, self.sink)? {}
        if self.session.state() != SessionState::Ended {
            return Err("audio session did not end".into());
        }
        let (rate, channels, layout) = self.output.format.unwrap_or_default();
        let frames = self.output.blocks;
        let count = self.output.samples;
        let refusals = self.refusals + self.session.refusals();
        eprintln!(
            "{{\"sample_rate\":{rate},\"channels\":{channels},\"layout\":{layout:?},\"frames\":{frames},\"samples_per_channel\":{count},\"errors\":{refusals}}}"
        );
        Ok(())
    }
}

fn audio<F: OutputForm>(
    input: &str,
    form: F,
    options: &Options<'_>,
    sink: &mut dyn Write,
) -> Result<()> {
    let pool = Budget::new(256 << 20)?;
    let mut session =
        AudioSession::with_form(limits(QueueLimits::AUDIO), form).on_refusal(options.refusal);
    let mut configuration = BudgetVec::new(&pool);
    if let Some(packets) = options.packets {
        let codec = AudioCodec::Dts;
        F::load(&mut session, codec, &configuration)?;
        let mut run = AudioRun {
            session,
            codec,
            configuration,
            output: AudioOutput::default(),
            options,
            sink,
            index: 0,
            refusals: 0,
        };
        elementary(input, packets, |bytes, pts| {
            run.feed(bytes, pts, Discard::default())
        })?;
        return run.finish();
    }
    let file = std::fs::File::open(input)?;
    let mut source = open_blocking(std::io::BufReader::new(file), &pool)?;
    let first = source.next_packet()?.ok_or("audio source has no packets")?;
    let reader = source.reader();
    let track = (0..reader.track_count())
        .filter_map(|index| reader.track(index))
        .find(|track| track.kind() == MediaKind::Audio)
        .ok_or("source has no audio track")?;
    let id = track.id();
    let codec = audio_codec(track.codec()).ok_or("unsupported audio codec")?;
    audio_configuration(codec, track.configuration(), &mut configuration)?;
    F::load(&mut session, codec, &configuration)?;
    let mut run = AudioRun {
        session,
        codec,
        configuration,
        output: AudioOutput::default(),
        options,
        sink,
        index: 0,
        refusals: 0,
    };
    let mut next = Some(first);
    while let Some(packet) = next {
        if packet.track() == id {
            let pts = packet
                .pts_time()?
                .map(|time| time.to_nanoseconds(Rounding::NearestAway))
                .transpose()?;
            run.feed(packet.payload(), pts, Discard::of(&packet))?;
        }
        next = source.next_packet()?;
    }
    run.finish()
}

fn elementary(
    input: &str,
    packets: &str,
    mut feed: impl FnMut(&[u8], Option<i64>) -> Result<()>,
) -> Result<()> {
    let bytes = std::fs::read(input)?;
    let list = std::fs::read_to_string(Path::new(packets))?;
    for line in list.lines() {
        let mut fields = line.split_ascii_whitespace();
        let start: usize = fields.next().ok_or("packet start")?.parse()?;
        let size: usize = fields.next().ok_or("packet size")?.parse()?;
        let pts = fields.next().ok_or("packet pts")?;
        let pts = (pts != "-").then(|| pts.parse()).transpose()?;
        let end = start.checked_add(size).ok_or("packet range overflow")?;
        feed(bytes.get(start..end).ok_or("packet exceeds input")?, pts)?;
    }
    Ok(())
}

fn video(
    input: &str,
    codec: VideoCodec,
    options: &Options<'_>,
    sink: &mut dyn Write,
) -> Result<()> {
    let mut session = VideoSession::with_canvas(limits(QueueLimits::VIDEO), Canvas::default())
        .on_refusal(options.refusal);
    StreamSelection {
        video: codec,
        audio: None,
    }
    .load(
        &mut session,
        &mut AudioSession::new(QueueLimits::AUDIO),
        &[],
    )?;
    let mut publish = |session: &mut VideoSession| -> Result<bool> {
        if session.step(None) {
            let picture = session.latest().ok_or("video session did not publish")?;
            if picture.rgba.len() != session.canvas().bytes() {
                return Err("published picture has the wrong canvas".into());
            }
            sink.write_all(&picture.rgba)?;
            report(picture.pts, 0, 0);
            return Ok(true);
        }
        if let SessionState::Failed(error) = session.state() {
            return Err(error.into());
        }
        Ok(false)
    };
    let mut index = 0;
    elementary(
        input,
        options.packets.ok_or("video requires packet ranges")?,
        |bytes, pts| {
            if options.indexes.contains(&index) {
                while publish(&mut session)? {}
                session.flush();
            }
            loop {
                match session.feed(bytes, pts) {
                    Ok(()) => break,
                    Err(FeedError::Backpressure) => {
                        // A compressed unit can be consumed without a picture being ready yet.
                        publish(&mut session)?;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            index += 1;
            Ok(())
        },
    )?;
    session.end_of_stream();
    while publish(&mut session)? {}
    if session.state() != SessionState::Ended {
        return Err("video session did not end".into());
    }
    Ok(())
}
