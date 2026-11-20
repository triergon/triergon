#![cfg(feature = "encode")]

use triergon_codec_ac3::encoder::{
    ChannelMode, DELAY_SAMPLES, EncodeError, Encoder, MAX_FRAME_BYTES, SAMPLES_PER_FRAME,
};
use triergon_codec_ac3::{
    AudioFrame, CrcPolicy, Decoder, DecoderOptions, Speaker, Syntax, parse_header,
};

const MODES: [(ChannelMode, u32, u8, &[Speaker]); 3] = [
    (ChannelMode::Mono, 192_000, 1, &[Speaker::FrontCenter]),
    (
        ChannelMode::Stereo,
        256_000,
        2,
        &[Speaker::FrontLeft, Speaker::FrontRight],
    ),
    (
        ChannelMode::Surround51,
        640_000,
        7,
        &[
            Speaker::FrontLeft,
            Speaker::FrontRight,
            Speaker::FrontCenter,
            Speaker::LowFrequency,
            Speaker::SideLeft,
            Speaker::SideRight,
        ],
    ),
];
const SAMPLE_RATES: [u32; 3] = [32_000, 44_100, 48_000];
const FRAMES: usize = 5;

fn tones(speakers: &[Speaker], rate: u32) -> Vec<Vec<f32>> {
    speakers
        .iter()
        .enumerate()
        .map(|(channel, speaker)| {
            let frequency = if *speaker == Speaker::LowFrequency {
                73.0
            } else {
                431.0 + 617.0 * channel as f64
            };
            (0..SAMPLES_PER_FRAME * FRAMES)
                .map(|sample| {
                    let phase =
                        core::f64::consts::TAU * frequency * sample as f64 / f64::from(rate);
                    (phase.sin() * 0.31) as f32
                })
                .collect()
        })
        .collect()
}

fn snr(source: impl IntoIterator<Item = f32>, decoded: impl IntoIterator<Item = f32>) -> f64 {
    let (signal, error) =
        source
            .into_iter()
            .zip(decoded)
            .fold((0.0, 0.0), |(signal, error), (a, b)| {
                let a = f64::from(a);
                (signal + a * a, error + (a - f64::from(b)).powi(2))
            });
    10.0 * (signal / error).log10()
}

/// Ideal long-block LFE reconstruction with the seven transmitted bins and no quantizer.
/// The endpoints can differ from the input where a discontinuity has energy above those bins.
fn lfe_projection(source: &[f32]) -> Vec<f32> {
    let mut window = [0.0; DELAY_SAMPLES];
    triergon_codec::kbd_window(5.0, &mut window);
    let matrix: [[f64; DELAY_SAMPLES * 2]; 7] = core::array::from_fn(|k| {
        core::array::from_fn(|n| {
            let angle = core::f64::consts::PI / 512.0 * (n as f64 + 128.5) * (2 * k + 1) as f64;
            f64::from(window[n.min(511 - n)]) * angle.cos()
        })
    });
    let mut output = vec![0.0; source.len() + DELAY_SAMPLES * 2];
    for start in (0..=source.len()).step_by(DELAY_SAMPLES) {
        for row in &matrix {
            let coefficient = row
                .iter()
                .enumerate()
                .map(|(n, scale)| {
                    let sample = (start + n)
                        .checked_sub(DELAY_SAMPLES)
                        .and_then(|index| source.get(index))
                        .copied()
                        .unwrap_or(0.0);
                    scale * f64::from(sample)
                })
                .sum::<f64>()
                / 128.0;
            for (sample, scale) in output[start..].iter_mut().zip(row) {
                *sample += coefficient * scale;
            }
        }
    }
    output[DELAY_SAMPLES..DELAY_SAMPLES + source.len()]
        .iter()
        .map(|&sample| sample as f32)
        .collect()
}

#[test]
fn syncframes_round_trip_every_rate_and_program() {
    let mut below = Vec::new();
    for rate in SAMPLE_RATES {
        for (mode, bitrate, acmod, speakers) in MODES {
            let source = tones(speakers, rate);
            let mut encoder = Encoder::new(rate, mode).unwrap();
            let mut decoder = Decoder::new().with_options(DecoderOptions {
                crc: CrcPolicy::Reject,
                ..DecoderOptions::default()
            });
            let mut output = [0; MAX_FRAME_BYTES];
            let mut frame = AudioFrame::default();
            let mut written = 0usize;
            let mut result = vec![Vec::new(); speakers.len()];
            let silence = [0.0; SAMPLES_PER_FRAME];
            for block in 0..=FRAMES {
                let at = block * SAMPLES_PER_FRAME;
                let planes: Vec<&[f32]> = if block == FRAMES {
                    vec![&silence; speakers.len()]
                } else {
                    source
                        .iter()
                        .map(|p| &p[at..at + SAMPLES_PER_FRAME])
                        .collect()
                };
                let span = encoder.encode(&planes, &mut output).unwrap();
                written += span;
                let cumulative_words =
                    (block + 1) as u64 * u64::from(bitrate) * (SAMPLES_PER_FRAME as u64 / 16)
                        / u64::from(rate);
                assert_eq!(written as u64, 2 * cumulative_words);
                let header = parse_header(&output[..span]).unwrap();
                assert_eq!(header.syntax, Syntax::Ac3);
                assert_eq!(header.frame_bytes, span);
                assert_eq!(header.acmod, acmod);
                assert_eq!(header.lfeon, mode == ChannelMode::Surround51);
                assert_eq!(header.sample_rate, rate);
                frame.clear();
                let info = decoder.decode(&output[..span], &mut frame).unwrap();
                assert_eq!(info.concealed, None, "{mode:?} at {rate} Hz block {block}");
                assert_eq!(info.layout.speakers(), speakers);
                assert_eq!(frame.samples_per_channel, SAMPLES_PER_FRAME);
                for (dst, src) in result.iter_mut().zip(&frame.planes) {
                    dst.extend_from_slice(src);
                }
            }
            for (ch, (input, decoded)) in source.iter().zip(&result).enumerate() {
                let alignment = &decoded[DELAY_SAMPLES..DELAY_SAMPLES + input.len()];
                let score = if speakers[ch] == Speaker::LowFrequency {
                    snr(lfe_projection(input), alignment.iter().copied())
                } else {
                    snr(input.iter().copied(), alignment.iter().copied())
                };
                if score <= 35.0 {
                    below.push(format!("{mode:?} at {rate} Hz channel {ch}: {score:.2} dB"));
                }
            }
        }
    }
    assert!(below.is_empty(), "{below:#?}");
}

#[test]
fn invalid_input_and_short_output_leave_the_stream_unchanged() {
    let mut encoder = Encoder::new(44_100, ChannelMode::Mono).unwrap();
    let mut reference = Encoder::new(44_100, ChannelMode::Mono).unwrap();
    let pcm = tones(MODES[0].3, 44_100);
    let planes = [&pcm[0][..SAMPLES_PER_FRAME]];
    let mut damaged = planes[0].to_vec();
    damaged[SAMPLES_PER_FRAME - 1] = f32::NAN;
    let mut out = [0xa5; MAX_FRAME_BYTES];
    let mut expected = [0xa5; MAX_FRAME_BYTES];
    assert_eq!(
        encoder.encode(&planes, &mut out[..2]),
        Err(EncodeError::OutputLength { needed: 834 })
    );
    assert_eq!(
        encoder.encode(&[&damaged], &mut out),
        Err(EncodeError::InvalidSample)
    );
    assert_eq!(
        encoder.encode(&[&[]], &mut out),
        Err(EncodeError::InputShape)
    );
    assert_eq!(out, expected);
    let len = encoder.encode(&planes, &mut out).unwrap();
    assert_eq!(reference.encode(&planes, &mut expected).unwrap(), len);
    assert_eq!(out, expected);
    encoder.reset();
    assert_eq!(encoder.encode(&planes, &mut out).unwrap(), len);
    assert_eq!(out, expected);
}

#[test]
#[ignore = "requires TRIERGON_FFMPEG and an empty TRIERGON_AC3_OUTPUT directory"]
fn ffmpeg_interoperability_and_signal_quality() {
    use std::fs;
    use std::io::{BufWriter, Write};
    use std::path::PathBuf;
    use std::process::Command;

    let ffmpeg = std::env::var_os("TRIERGON_FFMPEG").expect("FFmpeg binary");
    let dir = PathBuf::from(std::env::var_os("TRIERGON_AC3_OUTPUT").expect("comparison output"));
    fs::create_dir(&dir).expect("create fresh comparison output");

    let run = |command: &mut Command| {
        let output = command.output().unwrap();
        assert!(
            output.status.success() && output.stderr.is_empty(),
            "{command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    for rate in SAMPLE_RATES {
        for (mode, bitrate, _, speakers) in MODES {
            for broadband in [false, true] {
                let name = format!(
                    "{rate}-{}-{}",
                    speakers.len(),
                    if broadband { "broadband" } else { "tones" }
                );
                let mut source = tones(speakers, rate);
                if broadband {
                    let mut random = 0x62a1_3bb1u32;
                    for (plane, speaker) in source.iter_mut().zip(speakers) {
                        if *speaker == Speaker::LowFrequency {
                            continue;
                        }
                        let mut low = 0.0;
                        for (at, sample) in plane.iter_mut().enumerate() {
                            random = random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                            let noise = f64::from(random) / f64::from(u32::MAX) - 0.5;
                            low += 0.5 * (noise - low);
                            let gain = if at % 997 < 128 { 1.1 } else { 0.24 };
                            *sample = 0.4 * *sample + (low * gain) as f32;
                        }
                    }
                }

                let pcm = dir.join(format!("{name}.pcm"));
                let candidate = dir.join(format!("{name}-candidate.ac3"));
                let reference = dir.join(format!("{name}-reference.ac3"));
                let sample_count = source[0].len();
                {
                    let mut file = BufWriter::new(fs::File::create(&pcm).unwrap());
                    for at in 0..sample_count {
                        for channel in &source {
                            file.write_all(&channel[at].to_le_bytes()).unwrap();
                        }
                    }
                    file.flush().unwrap();
                }
                {
                    let mut encoder = Encoder::new(rate, mode).unwrap();
                    let mut target = fs::File::create(&candidate).unwrap();
                    let mut frame = [0; MAX_FRAME_BYTES];
                    let silence = [0.0; SAMPLES_PER_FRAME];
                    for at in (0..=sample_count).step_by(SAMPLES_PER_FRAME) {
                        let planes = if at == sample_count {
                            vec![silence.as_slice(); speakers.len()]
                        } else {
                            source
                                .iter()
                                .map(|p| &p[at..at + SAMPLES_PER_FRAME])
                                .collect()
                        };
                        let bytes = encoder.encode(&planes, &mut frame).unwrap();
                        target.write_all(&frame[..bytes]).unwrap();
                    }
                }
                // Add one zero block for both encoders so all source samples can be scored.
                {
                    let mut file = fs::OpenOptions::new().append(true).open(&pcm).unwrap();
                    let extra = vec![0; SAMPLES_PER_FRAME * speakers.len() * 4];
                    file.write_all(&extra).unwrap();
                }
                let layout = match mode {
                    ChannelMode::Mono => "mono",
                    ChannelMode::Stereo => "stereo",
                    ChannelMode::Surround51 => "5.1(side)",
                };
                run(Command::new(&ffmpeg)
                    .args(["-v", "error", "-nostdin", "-f", "f32le", "-ar"])
                    .arg(rate.to_string())
                    .args(["-ch_layout", layout, "-i"])
                    .arg(&pcm)
                    .args(["-c:a", "ac3", "-b:a"])
                    .arg(bitrate.to_string())
                    .arg(&reference));

                let quality = |input: &std::path::Path, label: &str| {
                    let decoded = dir.join(format!("{name}-{label}.pcm"));
                    run(Command::new(&ffmpeg)
                        .args([
                            "-v",
                            "error",
                            "-nostdin",
                            "-err_detect",
                            "crccheck+explode",
                            "-f",
                            "ac3",
                            "-i",
                        ])
                        .arg(input)
                        .args(["-c:a", "pcm_f32le", "-f", "f32le"])
                        .arg(&decoded));
                    let output = fs::read(decoded).unwrap();
                    assert_eq!(
                        output.len(),
                        (sample_count + SAMPLES_PER_FRAME) * speakers.len() * 4
                    );
                    source
                        .iter()
                        .enumerate()
                        .map(|(ch, plane)| {
                            let samples = output
                                .chunks_exact(4 * speakers.len())
                                .skip(DELAY_SAMPLES)
                                .map(|frame| {
                                    f32::from_le_bytes(
                                        frame[ch * 4..ch * 4 + 4].try_into().unwrap(),
                                    )
                                });
                            snr(plane.iter().copied(), samples)
                        })
                        .collect::<Vec<_>>()
                };
                assert!(
                    fs::metadata(&candidate)
                        .unwrap()
                        .len()
                        .abs_diff(fs::metadata(&reference).unwrap().len())
                        <= 2,
                    "{name} should use the same bit rate"
                );
                let a = quality(&candidate, "candidate");
                let b = quality(&reference, "reference");
                println!("{name}\t{a:.2?}\t{b:.2?}");
                assert!(
                    a.iter().all(|value| *value > 0.0),
                    "{name} lost signal or channel alignment"
                );
            }
        }
    }
}
