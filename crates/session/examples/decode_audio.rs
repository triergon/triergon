//! Decode the first audio track of a file to interleaved f32le, as a player would: the
//! container is recognised from its first bytes, the decoder chosen from the track, and
//! the container's trims applied.
//!
//! usage: decode_audio INPUT OUTPUT.f32 [--native-frames] [--crc reject] [--reset-at N,...]
//!
//! Prints one JSON line to stderr: sample rate, channels, layout, frames, samples.
use std::io::Write;
use triergon::{
    AnyPacket, AnyReader, Budget, CodecId, ContainerReader, MediaKind, PacketInfo, open_blocking,
};
use triergon_codec::{AudioFrame, ChannelLayout};
use triergon_session::TrackDecoder;

#[derive(Default)]
struct Summary {
    format: Option<(u32, ChannelLayout)>,
    frames: u64,
    samples: u64,
    errors: u64,
    last_error: String,
    frame_samples: Vec<usize>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, output, options @ ..] = args.as_slice() else {
        eprintln!(
            "usage: decode_audio INPUT OUTPUT.f32 [--native-frames] [--crc reject] [--reset-at N,...]"
        );
        std::process::exit(2);
    };
    let options = Options::parse(options).expect("decode options");
    let file = std::fs::File::open(input).expect("input");
    let mut out = std::io::BufWriter::new(std::fs::File::create(output).expect("output"));
    let mut summary = Summary::default();
    if let Err(error) = decode(file, &mut out, &mut summary, &options) {
        summary.errors += 1;
        summary.last_error = error;
    }
    out.flush().expect("flush");
    if options.reject_crc {
        eprintln!("{{\"frame_samples\":{:?}}}", summary.frame_samples);
    }
    let (rate, layout) = summary.format.unwrap_or_default();
    eprintln!(
        "{{\"sample_rate\":{rate},\"channels\":{},\"layout\":\"{}\",\"frames\":{},\"samples_per_channel\":{},\"errors\":{},\"last_error\":{:?}}}",
        layout.channels(),
        if summary.format.is_some() {
            layout.name()
        } else {
            String::new()
        },
        summary.frames,
        summary.samples,
        summary.errors,
        summary.last_error,
    );
}

#[derive(Default)]
struct Options {
    reject_crc: bool,
    reset_at: Vec<usize>,
    native_frames: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self::default();
        let args = if args.first().is_some_and(|arg| arg == "--native-frames") {
            options.native_frames = true;
            &args[1..]
        } else {
            args
        };
        let mut pairs = args.chunks_exact(2);
        for pair in &mut pairs {
            match (pair[0].as_str(), pair[1].as_str()) {
                ("--crc", "reject") => options.reject_crc = true,
                ("--reset-at", indices) => {
                    options.reset_at = indices
                        .split(',')
                        .map(str::parse)
                        .collect::<Result<_, _>>()
                        .map_err(|_| "invalid reset packet index")?;
                }
                _ => return Err(format!("unknown decode option {}", pair[0])),
            }
        }
        if !pairs.remainder().is_empty() {
            return Err("decode option requires a value".into());
        }
        Ok(options)
    }
}

enum Decoder {
    Track(TrackDecoder),
    Flac(Box<triergon_codec_flac::Decoder>),
}

impl Decoder {
    fn decode(&mut self, packet: &AnyPacket, frame: &mut AudioFrame) -> Result<(), String> {
        match self {
            Self::Track(decoder) => decoder
                .decode(packet, frame)
                .map(|_| ())
                .map_err(|e| format!("{e:?}")),
            Self::Flac(decoder) => decoder
                .decode(packet.payload(), frame)
                .map(|_| ())
                .map_err(|e| format!("{e:?}")),
        }
    }
}

fn decode(
    file: std::fs::File,
    out: &mut impl Write,
    summary: &mut Summary,
    options: &Options,
) -> Result<(), String> {
    let pool = Budget::new(256 << 20).map_err(|e| e.to_string())?;
    let mut source =
        open_blocking(std::io::BufReader::new(file), &pool).map_err(|e| e.to_string())?;
    let mut decoder: Option<(triergon::TrackId, Decoder, bool)> = None;
    let mut frame = AudioFrame::default();
    let mut bytes = Vec::new();
    let mut packet_index = 0;
    while let Some(packet) = source.next_packet().map_err(|e| format!("read: {e}"))? {
        if decoder.is_none() {
            let reader = source.reader();
            let track = (0..reader.track_count())
                .filter_map(|index| reader.track(index))
                .find(|track| track.kind() == MediaKind::Audio)
                .ok_or("no audio track")?;
            let raw_aac = matches!(reader, AnyReader::Isobmff(_)) && track.codec() == CodecId::Aac;
            let selected = if options.reject_crc {
                if track.codec() != CodecId::Flac {
                    return Err("CRC rejection requires a FLAC track".into());
                }
                let mut configuration = triergon_core::BudgetVec::new(&pool);
                triergon_session::audio_configuration(
                    triergon_session::AudioCodec::Flac,
                    track.configuration(),
                    &mut configuration,
                )
                .map_err(|error| format!("{error:?}"))?;
                Decoder::Flac(Box::new(
                    triergon_codec_flac::Decoder::new_in(&configuration, &pool)
                        .map_err(|error| format!("{error:?}"))?
                        .with_crc_policy(triergon_codec_flac::CrcPolicy::Reject),
                ))
            } else if raw_aac {
                TrackDecoder::aac_access_units_in(track.configuration(), &pool)
                    .map(Decoder::Track)
                    .map_err(|error| format!("{error:?}"))?
            } else if triergon_session::audio_codec(track.codec()).is_none() {
                return Err(match track.codec() {
                    CodecId::Other => format!("unsupported codec Other({:?})", track.codec_tag()),
                    codec => format!("unsupported codec {codec:?}"),
                });
            } else {
                TrackDecoder::new(track)
                    .map(Decoder::Track)
                    .map_err(|error| format!("{error:?}"))?
            };
            decoder = Some((track.id(), selected, raw_aac));
        }
        let Some((track, decoder, raw_aac)) = decoder.as_mut() else {
            continue;
        };
        if packet.track() != *track {
            continue;
        }
        // The neutral track publishes the first description. The example
        // cannot decode another description with those configuration bytes.
        if *raw_aac
            && !matches!(&packet, AnyPacket::Isobmff(p) if p.sample().description_index == 1)
        {
            return Err("AAC sample description changed".into());
        }
        if options.reset_at.contains(&packet_index)
            && let Decoder::Track(decoder) = decoder
        {
            decoder.reset();
        }
        packet_index += 1;
        let decoded = decoder.decode(&packet, &mut frame);
        summary.write(decoded, &frame, &mut bytes, out, options.native_frames)?;
    }
    if let Some((_, Decoder::Track(decoder), _)) = decoder.as_mut() {
        let flushed = decoder.flush(&mut frame).map_err(|e| format!("{e:?}"));
        summary.write(flushed, &frame, &mut bytes, out, options.native_frames)?;
    }
    Ok(())
}

impl Summary {
    /// Account for one decode result and write its samples.
    fn write<T>(
        &mut self,
        decoded: Result<T, String>,
        frame: &AudioFrame,
        bytes: &mut Vec<u8>,
        out: &mut impl Write,
        native_frames: bool,
    ) -> Result<(), String> {
        match decoded {
            Ok(_) if frame.samples_per_channel == 0 => {}
            Ok(_) => {
                self.frames += 1;
                self.frame_samples.push(frame.samples_per_channel);
                self.samples += frame.samples_per_channel as u64;
                self.format.get_or_insert((frame.sample_rate, frame.layout));
                if native_frames {
                    eprintln!(
                        "{{\"rate\":{},\"channels\":{},\"layout\":\"{}\",\"samples\":{}}}",
                        frame.sample_rate,
                        frame.layout.channels(),
                        frame.layout.name(),
                        frame.samples_per_channel
                    );
                }
                bytes.clear();
                for i in 0..frame.samples_per_channel {
                    for plane in &frame.planes {
                        bytes.extend_from_slice(&plane[i].to_le_bytes());
                    }
                }
                out.write_all(bytes).map_err(|e| e.to_string())?;
            }
            Err(error) => {
                self.errors += 1;
                self.last_error = error;
            }
        }
        Ok(())
    }
}
