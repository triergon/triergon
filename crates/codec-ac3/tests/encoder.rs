#![cfg(feature = "encode")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

fn burst_start(frame: usize, channel: usize) -> usize {
    frame * SAMPLES_PER_FRAME + (1 + frame % 3) * DELAY_SAMPLES + 19 + 7 * channel
}

fn attacks(speakers: &[Speaker], rate: u32) -> Vec<Vec<f32>> {
    let mut source = tones(speakers, rate);
    for (channel, (plane, speaker)) in source.iter_mut().zip(speakers).enumerate() {
        if *speaker == Speaker::LowFrequency {
            continue;
        }
        plane.fill(0.0);
        for frame in 0..FRAMES {
            let start = burst_start(frame, channel);
            for (at, sample) in plane[start..start + 96].iter_mut().enumerate() {
                let pitch = 0.16 + 0.023 * channel as f64;
                let phase = core::f64::consts::TAU * pitch * at as f64;
                *sample = (phase.sin() * (96 - at) as f64 / 192.0) as f32;
            }
        }
    }
    source
}

fn peak_before_attack(decoded: &[u8], speakers: &[Speaker]) -> f32 {
    let mut peak: f32 = 0.0;
    let stride = 4 * speakers.len();
    for (ch, &speaker) in speakers.iter().enumerate() {
        if speaker == Speaker::LowFrequency {
            continue;
        }
        for frame in 0..FRAMES {
            let block_start = burst_start(frame, ch) / DELAY_SAMPLES * DELAY_SAMPLES;
            let end = block_start + DELAY_SAMPLES;
            for at in end - 128..end {
                let start = at * stride + 4 * ch;
                let value = f32::from_le_bytes(decoded[start..start + 4].try_into().unwrap());
                peak = peak.max(value.abs());
            }
        }
    }
    peak
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

fn checked_run(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success() && output.stderr.is_empty(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn strict_decode(ffmpeg: &std::ffi::OsStr, input: &Path, decoded: &Path) -> Vec<u8> {
    checked_run(
        Command::new(ffmpeg)
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
            .arg(decoded),
    );
    fs::read(decoded).unwrap()
}

fn plane_sample(frame: &[u8], channel: usize) -> f32 {
    f32::from_le_bytes(frame[4 * channel..4 * channel + 4].try_into().unwrap())
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
fn a_single_attack_switches_the_program_and_keeps_the_previous_block_quiet() {
    for rate in SAMPLE_RATES {
        let (mode, _, _, speakers) = MODES[2];
        let mut source = tones(speakers, rate);
        source[0] = attacks(&speakers[..1], rate).remove(0);
        let mut encoder = Encoder::new(rate, mode).unwrap();
        let mut decoder = Decoder::new().with_options(DecoderOptions {
            crc: CrcPolicy::Reject,
            ..DecoderOptions::default()
        });
        let mut output = [0; MAX_FRAME_BYTES];
        let mut frame = AudioFrame::default();
        let mut decoded = vec![Vec::new(); speakers.len()];
        let mut switched = false;
        let silence = [0.0; SAMPLES_PER_FRAME];
        for number in 0..=FRAMES {
            let at = number * SAMPLES_PER_FRAME;
            let planes = if number == FRAMES {
                vec![silence.as_slice(); speakers.len()]
            } else {
                source
                    .iter()
                    .map(|p| &p[at..at + SAMPLES_PER_FRAME])
                    .collect()
            };
            let bytes = encoder.encode(&planes, &mut output).unwrap();
            frame.clear();
            let info = decoder.decode(&output[..bytes], &mut frame).unwrap();
            assert_eq!(info.concealed, None);
            switched |= info.features.block_switch;
            for (out, samples) in decoded.iter_mut().zip(&frame.planes) {
                out.extend_from_slice(samples);
            }
        }
        assert!(switched, "attack at {rate} Hz");
        assert!(
            decoded[0][DELAY_SAMPLES..2 * DELAY_SAMPLES]
                .iter()
                .all(|&sample| sample == 0.0)
        );
        for (i, (input, output)) in source.iter().zip(&decoded).enumerate() {
            if speakers[i] == Speaker::LowFrequency {
                continue;
            }
            let aligned = &output[DELAY_SAMPLES..][..input.len()];
            assert!(
                snr(input.iter().copied(), aligned.iter().copied()) > 30.0,
                "misaligned {rate} Hz channel {i}"
            );
        }
    }
}

#[test]
#[ignore = "requires TRIERGON_FFMPEG and an empty TRIERGON_AC3_OUTPUT directory"]
fn ffmpeg_interoperability_and_signal_quality() {
    use std::io::{BufWriter, Write};

    let ffmpeg = std::env::var_os("TRIERGON_FFMPEG").expect("FFmpeg binary");
    let dir = PathBuf::from(std::env::var_os("TRIERGON_AC3_OUTPUT").expect("comparison output"));
    fs::create_dir(&dir).expect("create fresh comparison output");

    for rate in SAMPLE_RATES {
        for (mode, bitrate, _, speakers) in MODES {
            for signal in ["tones", "broadband", "attacks"] {
                let name = format!("{rate}-{}-{signal}", speakers.len());
                let mut source = if signal == "attacks" {
                    attacks(speakers, rate)
                } else {
                    tones(speakers, rate)
                };
                if signal == "broadband" {
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
                checked_run(
                    Command::new(&ffmpeg)
                        .args(["-v", "error", "-nostdin", "-f", "f32le", "-ar"])
                        .arg(rate.to_string())
                        .args(["-ch_layout", layout, "-i"])
                        .arg(&pcm)
                        .args(["-c:a", "ac3", "-b:a"])
                        .arg(bitrate.to_string())
                        .arg(&reference),
                );

                let quality = |input: &Path, label: &str| {
                    let decoded = dir.join(format!("{name}-{label}.pcm"));
                    let output = strict_decode(&ffmpeg, input, &decoded);
                    assert_eq!(
                        output.len(),
                        (sample_count + SAMPLES_PER_FRAME) * speakers.len() * 4
                    );
                    let scores = source
                        .iter()
                        .enumerate()
                        .map(|(ch, plane)| {
                            let samples = output
                                .chunks_exact(4 * speakers.len())
                                .skip(DELAY_SAMPLES)
                                .map(|frame| plane_sample(frame, ch));
                            snr(plane.iter().copied(), samples)
                        })
                        .collect::<Vec<_>>();
                    let peak = (signal == "attacks").then(|| peak_before_attack(&output, speakers));
                    (scores, peak)
                };
                assert!(
                    fs::metadata(&candidate)
                        .unwrap()
                        .len()
                        .abs_diff(fs::metadata(&reference).unwrap().len())
                        <= 2,
                    "{name} should use the same bit rate"
                );
                let (a, peak_a) = quality(&candidate, "candidate");
                let (b, peak_b) = quality(&reference, "reference");
                println!("{name}\t{a:.2?}\t{b:.2?}");
                if let (Some(a), Some(b)) = (peak_a, peak_b) {
                    println!("pre-attack {name} {a:.7e} {b:.7e}");
                }
                assert!(
                    a.iter().all(|value| *value > 0.0),
                    "{name} lost signal or channel alignment"
                );
            }
        }
    }
}

#[test]
#[ignore = "requires the two encoder workers, pinned PCM and FFmpeg"]
fn encoder_workers_decode_full_sessions() {
    let ffmpeg = std::env::var_os("TRIERGON_FFMPEG").expect("FFmpeg binary");
    let dir = PathBuf::from(std::env::var_os("TRIERGON_AC3_OUTPUT").expect("comparison output"));
    let corpus = PathBuf::from(std::env::var_os("TRIERGON_AC3_INPUTS").expect("pinned PCM"));
    let candidate = std::env::var_os("TRIERGON_AC3_WORKER").expect("candidate worker");
    let reference = std::env::var_os("TRIERGON_AC3_REFERENCE").expect("reference worker");
    fs::create_dir(&dir).expect("create fresh comparison output");
    for rate in SAMPLE_RATES {
        for (_, _, _, speakers) in MODES {
            let channels = speakers.len();
            let stride = channels * 4;
            let cycle_bytes = (FRAMES + 1) * SAMPLES_PER_FRAME * stride;
            for signal in ["tones", "broadband", "attacks"] {
                let name = format!("{rate}-{channels}-{signal}");
                let input = corpus.join(format!("{name}.pcm"));
                let source = fs::read(&input).unwrap();
                assert_eq!(source.len(), cycle_bytes);
                let measure = |worker: &std::ffi::OsStr, label: &str| {
                    let stream = dir.join(format!("{name}-{label}.ac3"));
                    checked_run(
                        Command::new(worker)
                            .arg("ac3-encode-emit")
                            .arg(&input)
                            .arg(rate.to_string())
                            .arg(channels.to_string())
                            .arg(&stream),
                    );
                    let pcm = dir.join(format!("{name}-{label}.pcm"));
                    let decoded = strict_decode(&ffmpeg, &stream, &pcm);
                    fs::remove_file(pcm).unwrap();
                    assert_eq!(decoded.len(), 49 * cycle_bytes);
                    let scores = (0..channels)
                        .map(|ch| {
                            snr(
                                source
                                    .chunks_exact(stride)
                                    .cycle()
                                    .map(|f| plane_sample(f, ch)),
                                decoded
                                    .chunks_exact(stride)
                                    .skip(DELAY_SAMPLES)
                                    .map(|f| plane_sample(f, ch)),
                            )
                        })
                        .collect::<Vec<_>>();
                    assert!(
                        scores.iter().all(|value| *value > 0.0),
                        "{name} {label}: {scores:?}"
                    );
                    let peak = (signal == "attacks").then(|| {
                        decoded
                            .chunks_exact(cycle_bytes)
                            .map(|cycle| peak_before_attack(cycle, speakers))
                            .fold(0.0, f32::max)
                    });
                    (fs::metadata(stream).unwrap().len(), scores, peak)
                };
                let (candidate_bytes, a, peak_a) = measure(&candidate, "candidate");
                let (reference_bytes, b, peak_b) = measure(&reference, "reference");
                assert_eq!(candidate_bytes, reference_bytes, "{name} encoded bytes");
                println!("{name}\t{a:.2?}\t{b:.2?}");
                if let (Some(a), Some(b)) = (peak_a, peak_b) {
                    println!("pre-attack {name} {a:.7e} {b:.7e}");
                }
            }
        }
    }
}
