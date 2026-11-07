//! Decode an AC-3 / E-AC-3 elementary stream to interleaved f32le (plane order).
//!
//! usage: decode <input.ac3|eac3> <output.f32|-> [--reject-crc] [--no-noise] [--quiet]
//!
//! Prints one JSON line to stderr: sample rate, channels, layout, frames, samples, ...

use std::io::Write;

use triergon_codec_ac3::{
    AudioFrame, CrcPolicy, DecodeError, Decoder, DecoderOptions, FrameFeatures, NoiseMode,
    access_unit_len,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: decode <input> <output.f32|-> [--reject-crc] [--no-noise]");
        std::process::exit(2);
    }
    let data = std::fs::read(&args[1]).expect("read input");
    let mut opts = DecoderOptions::default();
    for a in &args[3..] {
        match a.as_str() {
            "--reject-crc" => opts.crc = CrcPolicy::Reject,
            "--no-noise" => opts.noise = NoiseMode::Off,
            _ => {}
        }
    }
    let mut dec = Decoder::new().with_options(opts);
    let mut frame = AudioFrame::default();
    let mut out: Box<dyn Write> = if args[2] == "-" {
        Box::new(std::io::BufWriter::new(std::io::stdout().lock()))
    } else {
        Box::new(std::io::BufWriter::new(
            std::fs::File::create(&args[2]).expect("create output"),
        ))
    };
    let (mut frames, mut samples, mut concealed, mut crc_fail, mut errors, mut skipped) =
        (0, 0, 0, 0, 0, 0usize);
    let mut feats = FrameFeatures::default();
    let mut first: Option<(u32, String, Vec<&'static str>, usize)> = None;
    let mut interleaved: Vec<u8> = Vec::new();
    let mut pos = 0usize;
    let mut last_err: Option<DecodeError> = None;
    let mut third_downmix = None;
    while pos + 8 <= data.len() {
        let Ok(len) = access_unit_len(&data[pos..]) else {
            // Resynchronise on the next syncword.
            pos += 1;
            skipped += 1;
            continue;
        };
        let end = (pos + len).min(data.len());
        frame.clear();
        match dec.decode(&data[pos..end], &mut frame) {
            Ok(info) => {
                frames += 1;
                samples += info.samples;
                if info.concealed.is_some() {
                    concealed += 1;
                }
                if !info.crc_ok {
                    crc_fail += 1;
                }
                merge(&mut feats, &info.features);
                if frames == 3 && std::env::var_os("DMXCODES").is_some() {
                    eprintln!("{:?}", info.downmix_codes);
                }
                if frames == 3 {
                    third_downmix = info.downmix;
                }
                if std::env::var_os("DMXALL").is_some() {
                    eprintln!("DMX {}", fmt_downmix(info.downmix));
                }
                if first.is_none() {
                    first = Some((
                        info.sample_rate,
                        triergon_codec_ac3::layout_name(&info.layout),
                        info.layout.speakers().iter().map(|s| s.name()).collect(),
                        info.layout.channels(),
                    ));
                }
                let n = frame.samples_per_channel;
                let ch = frame.planes.len();
                interleaved.clear();
                interleaved.reserve(n * ch * 4);
                for i in 0..n {
                    for p in &frame.planes {
                        interleaved.extend_from_slice(&p[i].to_le_bytes());
                    }
                }
                out.write_all(&interleaved).expect("write");
            }
            Err(e) => {
                errors += 1;
                last_err = Some(e);
            }
        }
        pos = end;
    }
    out.flush().expect("flush");
    let (rate, layout, speakers, channels) = first.unwrap_or((0, String::new(), Vec::new(), 0));
    let sp: Vec<String> = speakers.iter().map(|s| format!("\"{s}\"")).collect();
    eprintln!(
        "{{\"sample_rate\":{rate},\"channels\":{channels},\"layout\":\"{layout}\",\"speakers\":[{}],\"frames\":{frames},\"samples_per_channel\":{samples},\"concealed_frames\":{concealed},\"crc_failures\":{crc_fail},\"errors\":{errors},\"skipped_bytes\":{skipped},\"last_error\":\"{}\",\"features\":\"{:?}\",\"downmix_frame3\":\"{}\"}}",
        sp.join(","),
        last_err.map(|e| e.to_string()).unwrap_or_default(),
        feats,
        fmt_downmix(third_downmix)
    );
}

fn fmt_downmix(d: Option<triergon_codec_ac3::DownmixInfo>) -> String {
    let Some(d) = d else { return "none".into() };
    let f = |v: Option<f64>| v.map_or_else(|| "-".into(), |x| format!("{x:.6}"));
    format!(
        "pref={:?} center={} center_ltrt={} surround={} surround_ltrt={} lfe={}",
        d.preferred_downmix_type
            .unwrap_or(triergon_codec_ac3::DownmixType::Unknown),
        f(d.center_mix_level),
        f(d.center_mix_level_ltrt),
        f(d.surround_mix_level),
        f(d.surround_mix_level_ltrt),
        f(d.lfe_mix_level)
    )
}

fn merge(a: &mut FrameFeatures, b: &FrameFeatures) {
    a.coupling |= b.coupling;
    a.phase_flags |= b.phase_flags;
    a.rematrix |= b.rematrix;
    a.block_switch |= b.block_switch;
    a.dither |= b.dither;
    a.delta_ba |= b.delta_ba;
    a.dynrng |= b.dynrng;
    a.spx |= b.spx;
    a.spx_atten |= b.spx_atten;
    a.aht |= b.aht;
    a.gaq |= b.gaq;
    a.tpnp |= b.tpnp;
    a.frame_expstr |= b.frame_expstr;
    a.skip_fields |= b.skip_fields;
    a.ecpl |= b.ecpl;
    a.overrun |= b.overrun;
}
