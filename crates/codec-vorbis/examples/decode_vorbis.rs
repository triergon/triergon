#![forbid(unsafe_code)]
//! Decode a Vorbis file to interleaved f32le, planes in FFmpeg's channel order. The
//! container (Ogg, Matroska) is read through the `triergon` facade; the decoder's whole
//! output is written, without container trims.
//!
//! usage: decode_vorbis INPUT OUTPUT.f32
//!
//! Decodes the first Vorbis track and, in a chained Ogg file, the first Vorbis track of
//! each link after it the way FFmpeg does: a link whose headers repeat the current ones
//! continues the same decoder, so its first packet laps the previous link's last block;
//! a link with new headers starts a new decoder; a link that changes the channel count
//! or sample rate ends decoding, as FFmpeg's Ogg demuxer refuses it. Prints one JSON
//! line to stderr: the first link's format, packet and sample counts, errors, the last
//! error and the decoder's peak storage.

use std::io::Write;
use std::process::ExitCode;

use triergon::ogg::ReaderEvent;
use triergon::{AnyOther, Budget, CodecId, ContainerReader, Event, MediaKind, PacketInfo, TrackId};
use triergon_codec::{AudioDecoder, AudioFrame, ChannelLayout};
use triergon_codec_vorbis::{Decoder, Limits};
use triergon_framing::VorbisHeaders;

/// The decoder of the current link and the configuration it was built from.
struct Current {
    decoder: Decoder,
    configuration: Vec<u8>,
    budget: Budget,
}

#[derive(Default)]
struct Summary {
    format: Option<(u32, ChannelLayout)>,
    decoded: usize,
    samples: usize,
    errors: usize,
    last_error: String,
    peak: usize,
}

impl Summary {
    fn fail(&mut self, error: impl ToString) {
        self.errors += 1;
        self.last_error = error.to_string();
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, output] = args.as_slice() else {
        eprintln!("usage: decode_vorbis INPUT OUTPUT.f32");
        return ExitCode::from(2);
    };
    let opened = std::fs::File::open(input)
        .map_err(|e| e.to_string())
        .and_then(|file| {
            let pool = Budget::new(256 << 20).map_err(|e| e.to_string())?;
            triergon::open_blocking(std::io::BufReader::new(file), &pool).map_err(|e| e.to_string())
        });
    let mut source = match opened {
        Ok(source) => source,
        Err(error) => {
            eprintln!("{input}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut out = match std::fs::File::create(output) {
        Ok(file) => std::io::BufWriter::new(file),
        Err(error) => {
            eprintln!("{output}: {error}");
            return ExitCode::FAILURE;
        }
    };

    let mut summary = Summary::default();
    let mut frame = AudioFrame::default();
    let mut interleaved = Vec::new();
    let mut current: Option<Current> = None;
    // The track decoded in the current link; `None` until its first packet.
    let mut track: Option<TrackId> = None;
    loop {
        let packet = match source.next_event() {
            Ok(Some(Event::Packet(packet))) => packet,
            Ok(Some(Event::Other(AnyOther::Ogg(ReaderEvent::Link { .. })))) => {
                track = None;
                continue;
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(error) => {
                summary.fail(format_args!("read: {error}"));
                break;
            }
        };
        if track.is_none() {
            let reader = source.reader();
            let Some(new) = (0..reader.track_count())
                .filter_map(|i| reader.track(i))
                .find(|t| t.id() == packet.track())
                .filter(|t| t.kind() == MediaKind::Audio && t.codec() == CodecId::Vorbis)
            else {
                continue;
            };
            track = Some(new.id());
            if !start_link(new.configuration(), &mut current, &mut summary) {
                break;
            }
        }
        if track != Some(packet.track()) {
            continue;
        }
        let Some(current) = current.as_mut() else {
            continue;
        };
        if let Err(error) = current.decoder.decode(packet.payload(), &mut frame) {
            summary.fail(error.0);
            continue;
        }
        summary.decoded += 1;
        summary.samples += frame.samples_per_channel;
        interleaved.clear();
        for i in 0..frame.samples_per_channel {
            for plane in &frame.planes {
                interleaved.extend_from_slice(&plane[i].to_le_bytes());
            }
        }
        if let Err(error) = out.write_all(&interleaved) {
            eprintln!("{output}: {error}");
            return ExitCode::FAILURE;
        }
    }
    if let Some(current) = &current {
        summary.peak = summary.peak.max(current.budget.peak());
    }
    if let Err(error) = out.flush() {
        eprintln!("{output}: {error}");
        return ExitCode::FAILURE;
    }
    let (rate, layout) = summary.format.unwrap_or((0, ChannelLayout::EMPTY));
    eprintln!(
        "{{\"sample_rate\":{rate},\"channels\":{},\"layout\":\"{}\",\"frames\":{},\"samples_per_channel\":{},\"errors\":{},\"last_error\":\"{}\",\"storage_peak\":{}}}",
        layout.channels(),
        // ffprobe's name for a layout of unspecified order (beyond eight channels).
        if layout.mask() == 0 {
            "unknown".into()
        } else {
            layout.name()
        },
        summary.decoded,
        summary.samples,
        summary.errors,
        summary.last_error.replace('"', "'"),
        summary.peak,
    );
    ExitCode::SUCCESS
}

/// Set `current` up for a link whose track carries `configuration` (the three headers,
/// Xiph-laced): keep the decoder when the identification and setup headers repeat, else
/// build a new one. `false` when the link changes the format, which ends decoding.
fn start_link(configuration: &[u8], current: &mut Option<Current>, summary: &mut Summary) -> bool {
    let headers = match VorbisHeaders::from_laced(configuration) {
        Ok(headers) => headers,
        Err(error) => {
            summary.fail(error);
            *current = None;
            return true;
        }
    };
    let continues = current.as_ref().is_some_and(|current| {
        VorbisHeaders::from_laced(&current.configuration).is_ok_and(|previous| {
            previous.identification == headers.identification && previous.setup == headers.setup
        })
    });
    if continues {
        return true;
    }
    if let Some(previous) = current.take() {
        summary.peak = summary.peak.max(previous.budget.peak());
    }
    let budget = match Budget::new(Limits::default().live_storage_bytes) {
        Ok(budget) => budget,
        Err(error) => {
            summary.fail(error);
            return false;
        }
    };
    let decoder = match Decoder::new_in(configuration, &budget) {
        Ok(decoder) => decoder,
        Err(error) => {
            summary.fail(error);
            return true;
        }
    };
    let format = (decoder.sample_rate(), decoder.layout());
    if summary.format.is_some_and(|first| first != format) {
        summary.fail("chained stream changes the sample rate or channels");
        return false;
    }
    summary.format = Some(format);
    *current = Some(Current {
        decoder,
        configuration: configuration.to_vec(),
        budget,
    });
    true
}
