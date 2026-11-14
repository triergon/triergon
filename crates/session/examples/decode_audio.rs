//! Decode the first audio track of a file to interleaved f32le, as a player would: the
//! container is recognised from its first bytes, the decoder chosen from the track, and
//! the container's trims applied.
//!
//! usage: decode_audio INPUT OUTPUT.f32
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
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, output] = args.as_slice() else {
        eprintln!("usage: decode_audio INPUT OUTPUT.f32");
        std::process::exit(2);
    };
    let file = std::fs::File::open(input).expect("input");
    let mut out = std::io::BufWriter::new(std::fs::File::create(output).expect("output"));
    let mut summary = Summary::default();
    if let Err(error) = decode(file, &mut out, &mut summary) {
        summary.errors += 1;
        summary.last_error = error;
    }
    out.flush().expect("flush");
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

fn decode(file: std::fs::File, out: &mut impl Write, summary: &mut Summary) -> Result<(), String> {
    let pool = Budget::new(256 << 20).map_err(|e| e.to_string())?;
    let mut source =
        open_blocking(std::io::BufReader::new(file), &pool).map_err(|e| e.to_string())?;
    let mut decoder: Option<(triergon::TrackId, TrackDecoder, bool)> = None;
    let mut frame = AudioFrame::default();
    let mut bytes = Vec::new();
    while let Some(packet) = source.next_packet().map_err(|e| format!("read: {e}"))? {
        if decoder.is_none() {
            let reader = source.reader();
            let track = (0..reader.track_count())
                .filter_map(|index| reader.track(index))
                .find(|track| track.kind() == MediaKind::Audio)
                .ok_or("no audio track")?;
            let raw_aac = matches!(reader, AnyReader::Isobmff(_)) && track.codec() == CodecId::Aac;
            let selected = if raw_aac {
                TrackDecoder::aac_access_units_in(track.configuration(), &pool)
            } else {
                TrackDecoder::new(track)
            };
            decoder = Some((track.id(), selected.map_err(|e| e.0)?, raw_aac));
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
        let decoded = decoder.decode(&packet, &mut frame);
        summary.write(decoded, &frame, &mut bytes, out)?;
    }
    if let Some((_, decoder, _)) = decoder.as_mut() {
        let flushed = decoder.flush(&mut frame);
        summary.write(flushed, &frame, &mut bytes, out)?;
    }
    Ok(())
}

impl Summary {
    /// Account for one decode result and write its samples.
    fn write<T>(
        &mut self,
        decoded: Result<T, triergon_codec::DecodeFailure>,
        frame: &AudioFrame,
        bytes: &mut Vec<u8>,
        out: &mut impl Write,
    ) -> Result<(), String> {
        match decoded {
            Ok(_) if frame.samples_per_channel == 0 => {}
            Ok(_) => {
                self.frames += 1;
                self.samples += frame.samples_per_channel as u64;
                self.format.get_or_insert((frame.sample_rate, frame.layout));
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
                self.last_error = error.0;
            }
        }
        Ok(())
    }
}
