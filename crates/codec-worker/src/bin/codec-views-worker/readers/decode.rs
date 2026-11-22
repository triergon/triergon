//! Read ADTS and AC-3-family access units through the elementary reader before decoding.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufWriter, Cursor, Write};
use std::sync::atomic::AtomicBool;

use triergon::PacketInfo;
use triergon::elementary::{Ac3, Adts, BlockingReader, Format, Limits};
use triergon_codec::{AudioDecoder, AudioFrame};
use triergon_harness_protocol::interleaved::InterleavedDigest;
use triergon_harness_runner::{BenchLine, Boundary, Timer};
use triergon_session::mix::Downmix;

#[derive(Default)]
struct Output {
    frames: u64,
    bytes: u64,
    digest: InterleavedDigest,
    raw: Option<BufWriter<File>>,
}

fn decode<F: Format>(
    input: &[u8],
    mut decoder: impl AudioDecoder<Error: std::fmt::Debug>,
    out: &mut Output,
) -> Result<(), String> {
    let mut reader = BlockingReader::<F, _>::open(Cursor::new(input), Limits::default())
        .map_err(|error| error.to_string())?;
    let cancel = AtomicBool::new(false);
    let mut downmix = Downmix::default();
    let mut frame = AudioFrame::default();
    let mut pcm = Vec::new();
    let mut bytes = Vec::new();
    while let Some(packet) = reader
        .next_packet(&cancel)
        .map_err(|error| error.to_string())?
    {
        let levels = decoder
            .decode(packet.payload(), &mut frame)
            .map_err(|error| format!("{error:?}"))?;
        if frame.samples_per_channel == 0 {
            continue;
        }
        pcm.clear();
        downmix.mix(&frame, levels.unwrap_or_default(), &mut pcm);
        bytes.clear();
        bytes.extend(pcm.iter().flat_map(|sample| sample.to_le_bytes()));
        out.digest.update(&bytes);
        out.frames += 1;
        out.bytes += bytes.len() as u64;
        if let Some(raw) = out.raw.as_mut() {
            raw.write_all(&bytes).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

pub(super) fn run(command: &str, args: &[OsString]) -> Result<(), String> {
    let (operation, mode) = command
        .rsplit_once('-')
        .ok_or("missing decoder worker mode")?;
    let (input, extra) = args.split_first().ok_or("missing elementary input")?;
    let input = std::fs::read(input).map_err(|error| error.to_string())?;
    let mut out = Output::default();
    let run = match operation {
        "audio-aac" => |input: &[u8], out: &mut Output| {
            decode::<Adts>(input, triergon_codec_aac::Decoder::new(), out)
        },
        "audio-ac3" | "audio-eac3" => |input: &[u8], out: &mut Output| {
            decode::<Ac3>(input, triergon_codec_ac3::Decoder::new(), out)
        },
        _ => return Err("unknown elementary decode operation".into()),
    };
    let marker = |boundary: Boundary| {
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(boundary.line())
            .and_then(|()| stdout.flush())
            .map_err(|error| error.to_string())
    };
    let iterations = match (mode, extra) {
        ("bench", [iterations]) => iterations
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|&n| n != 0)
            .ok_or("invalid iteration count")?,
        ("raw", [path]) => {
            out.raw = Some(BufWriter::new(
                File::create(path).map_err(|error| error.to_string())?,
            ));
            1
        }
        ("memory" | "fold", []) => 1,
        _ => return Err("invalid elementary decode arguments".into()),
    };
    if mode == "memory" {
        marker(Boundary::Begin)?;
    }
    let start = Timer::start(mode == "bench");
    for _ in 0..iterations {
        run(&input, &mut out)?;
    }
    match mode {
        "bench" => {
            let elapsed = start.elapsed().as_nanos();
            println!(
                "{}",
                BenchLine::with_schema(
                    u64::from(iterations),
                    elapsed,
                    (out.frames, out.bytes, out.digest.finish())
                )
            );
        }
        "memory" => {
            marker(Boundary::End)?;
            println!("{{\"packets\":{},\"bytes\":{}}}", out.frames, out.bytes);
            eprintln!("{{\"digest\":{}}}", out.digest.finish());
        }
        _ => {
            if let Some(raw) = out.raw.as_mut() {
                raw.flush().map_err(|error| error.to_string())?;
            }
            println!(
                "{{\"packets\":{},\"bytes\":{},\"digest\":{}}}",
                out.frames,
                out.bytes,
                out.digest.finish()
            );
        }
    }
    Ok(())
}
