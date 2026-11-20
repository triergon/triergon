//! Measurement worker for the AC-3 encoder. Its reference is
//! `harness/reference_ac3_encode.c`.

use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::time::Instant;

use triergon_codec_ac3::encoder::{ChannelMode, Encoder, MAX_FRAME_BYTES, SAMPLES_PER_FRAME};

const FRAMES: usize = 6;
// Six input frames repeated 49 times cover two complete 44.1 kHz size cycles,
// including at 256 and 640 kbit/s.
const REPEATS: u32 = 49;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Input {
    rate: u32,
    mode: ChannelMode,
    channels: usize,
    header: u64,
    samples: Vec<f32>,
}

impl Input {
    fn read(path: &str, rate: u32, channels: usize) -> Result<Self> {
        let fscod = match rate {
            48_000 => 0,
            44_100 => 1,
            32_000 => 2,
            _ => return Err("sample rate".into()),
        };
        let (mode, acmod, lfe, size_code) = match channels {
            1 => (ChannelMode::Mono, 1, 0, 10),
            2 => (ChannelMode::Stereo, 2, 0, 12),
            6 => (ChannelMode::Surround51, 7, 1, 18),
            _ => return Err("channel layout".into()),
        };
        let mut file = BufReader::new(File::open(path)?);
        let mut samples = vec![0.0; channels * FRAMES * SAMPLES_PER_FRAME];
        let mut row = [0; 6 * 4];
        for frame in samples.chunks_exact_mut(channels * SAMPLES_PER_FRAME) {
            for i in 0..SAMPLES_PER_FRAME {
                let row = &mut row[..4 * channels];
                file.read_exact(row)?;
                for (ch, word) in row.chunks_exact(4).enumerate() {
                    let value = f32::from_le_bytes(word.try_into()?);
                    if !(-1.0..=1.0).contains(&value) {
                        return Err("PCM outside the normalized range".into());
                    }
                    frame[ch * SAMPLES_PER_FRAME + i] = value;
                }
            }
        }
        if file.read(&mut row[..1])? != 0 {
            return Err("oversized PCM input".into());
        }
        Ok(Self {
            rate,
            mode,
            channels,
            header: (fscod << 16) | (8 << 12) | (acmod << 8) | (lfe << 6) | size_code,
            samples,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Work {
    packets: u64,
    bytes: u64,
    digest: u64,
}

impl Work {
    fn record(&mut self, packet: &[u8], expected: u64) -> Result<()> {
        let [0x0b, 0x77, _, _, size, version, layout, ..] = packet else {
            return Err("AC-3 syncframe".into());
        };
        let acmod = layout >> 5;
        let lfe = (layout
            >> match acmod {
                1 => 4,
                2 => 2,
                7 => 0,
                _ => return Err("encoded layout".into()),
            })
            & 1;
        // The low frame-size bit is the legal 44.1 kHz word adjustment.
        let header = (u64::from(size >> 6) << 16)
            | (u64::from(version >> 3) << 12)
            | (u64::from(acmod) << 8)
            | (u64::from(lfe) << 6)
            | u64::from((size & 63) >> 1);
        if header != expected {
            return Err("encoded rate, layout or bitrate".into());
        }
        self.packets += 1;
        self.bytes += packet.len() as u64;
        self.digest = (self.digest ^ header).wrapping_mul(0x100_0000_01b3);
        Ok(())
    }
}

fn encode(input: &Input, repeats: u32, sink: &mut impl Write) -> Result<Work> {
    let mut encoder = Encoder::new(input.rate, input.mode)?;
    let mut out = [0; MAX_FRAME_BYTES];
    let mut work = Work {
        packets: 0,
        bytes: 0,
        digest: 0xcbf2_9ce4_8422_2325,
    };
    for _ in 0..repeats {
        for frame in input
            .samples
            .chunks_exact(input.channels * SAMPLES_PER_FRAME)
        {
            let mut planes = [&[][..]; 6];
            for (dst, src) in planes.iter_mut().zip(frame.chunks_exact(SAMPLES_PER_FRAME)) {
                *dst = src;
            }
            let size = encoder.encode(&planes[..input.channels], &mut out)?;
            let packet = std::hint::black_box(&out[..size]);
            work.record(packet, input.header)?;
            sink.write_all(packet)?;
        }
    }
    Ok(work)
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [mode, path, rate, channels]
            if mode == "ac3-encode-fold" || mode == "ac3-encode-memory" =>
        {
            let memory = mode == "ac3-encode-memory";
            let input = Input::read(path, rate.parse()?, channels.parse()?)?;
            let mut stdout = io::stdout().lock();
            if memory {
                stdout.write_all(b"TRIERGON_BEGIN_V1\n")?;
                stdout.flush()?;
            }
            let work = encode(&input, REPEATS, &mut io::sink())?;
            if memory {
                stdout.write_all(b"TRIERGON_END_V1\n")?;
                stdout.flush()?;
            }
            writeln!(
                stdout,
                "{{\"eof\":true,\"packets\":{},\"bytes\":{},\"digest\":{}}}",
                work.packets, work.bytes, work.digest
            )?;
        }
        [mode, path, iterations, rate, channels] if mode == "ac3-encode-bench" => {
            let iterations: u32 = iterations.parse()?;
            if iterations == 0 {
                return Err("iterations must be positive".into());
            }
            let input = Input::read(path, rate.parse()?, channels.parse()?)?;
            let mut first = None;
            let start = Instant::now();
            for _ in 0..iterations {
                let work = encode(&input, REPEATS, &mut io::sink())?;
                if *first.get_or_insert(work) != work {
                    return Err("encoded work changed between iterations".into());
                }
            }
            let elapsed_ns = start.elapsed().as_nanos();
            let work = first.expect("positive iterations");
            println!(
                "{{\"iterations\":{iterations},\"elapsed_ns\":{elapsed_ns},\"packets\":{},\"bytes\":{},\"digest\":{}}}",
                work.packets * u64::from(iterations),
                work.bytes * u64::from(iterations),
                work.digest
            );
        }
        [mode, path, rate, channels, output] if mode == "ac3-encode-emit" => {
            let input = Input::read(path, rate.parse()?, channels.parse()?)?;
            let mut sink = io::BufWriter::new(File::create_new(output)?);
            encode(&input, REPEATS, &mut sink)?;
            sink.flush()?;
        }
        _ => return Err("expected ac3-encode-fold|memory|bench|emit and input arguments".into()),
    }
    Ok(())
}

fn main() -> Result<()> {
    run()
}
