//! Test harness: stream splitting, whole-stream decoding and fingerprint checks.
#![allow(dead_code)]

use triergon_codec_dca::{AudioFrame, DecodeError, Decoder, Packing};

/// Byte length of the core frame at the start of `data` in its packing.
pub fn frame_len(data: &[u8], packing: Packing) -> Option<usize> {
    let mut words = [0u16; 5];
    for (i, w) in words.iter_mut().enumerate() {
        let (a, b) = (*data.get(2 * i)?, *data.get(2 * i + 1)?);
        *w = match packing {
            Packing::Be16 | Packing::Be14 => u16::from_be_bytes([a, b]),
            Packing::Le16 | Packing::Le14 => u16::from_le_bytes([a, b]),
        };
    }
    let bits: u128 = if packing.is_14bit() {
        words
            .iter()
            .fold(0u128, |acc, &w| (acc << 14) | u128::from(w & 0x3FFF))
            << (128 - 70)
    } else {
        words
            .iter()
            .fold(0u128, |acc, &w| (acc << 16) | u128::from(w))
            << (128 - 80)
    };
    let fsize = ((bits >> (128 - 60)) & 0x3FFF) as usize + 1;
    Some(match packing {
        Packing::Be14 | Packing::Le14 => (fsize * 8).div_ceil(14) * 2,
        Packing::Le16 => fsize.div_ceil(2) * 2,
        Packing::Be16 => fsize,
    })
}

/// Split a raw stream into core frames, skipping DTS-HD extension substreams and garbage.
pub fn split(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 16 <= data.len() {
        let rest = &data[pos..];
        if rest[..4] == [0x64, 0x58, 0x20, 0x25] {
            let v = u128::from_be_bytes(rest[..16].try_into().unwrap());
            let header_type = (v >> (128 - 43)) & 1;
            let (hbits, fbits) = if header_type == 1 { (12, 20) } else { (8, 16) };
            let fsize = ((v >> (128 - 43 - hbits - fbits)) & ((1 << fbits) - 1)) as usize + 1;
            pos += fsize.max(4);
            continue;
        }
        match Packing::detect(rest).and_then(|p| frame_len(rest, p)) {
            Some(len) => {
                let len = len.min(rest.len());
                out.push(&rest[..len]);
                pos += len;
            }
            None => pos += 1,
        }
    }
    out
}

pub struct Decoded {
    pub sample_rate: u32,
    pub speakers: Vec<&'static str>,
    pub planes: Vec<Vec<f32>>,
    pub frames: usize,
    pub errors: Vec<DecodeError>,
}

/// Decode every frame; planes are concatenated (layout taken from the first good frame).
pub fn decode_all(frames: &[&[u8]]) -> Decoded {
    let mut dec = Decoder::new();
    let mut f = AudioFrame::default();
    let mut d = Decoded {
        sample_rate: 0,
        speakers: Vec::new(),
        planes: Vec::new(),
        frames: 0,
        errors: Vec::new(),
    };
    for au in frames {
        match dec.decode(au, &mut f) {
            Ok(()) => {
                if d.frames == 0 {
                    d.sample_rate = f.sample_rate;
                    d.speakers = f.layout.speakers().iter().map(|s| s.name()).collect();
                    d.planes = vec![Vec::new(); f.planes.len()];
                }
                assert_eq!(f.planes.len(), d.planes.len(), "layout change mid-stream");
                for (dst, src) in d.planes.iter_mut().zip(&f.planes) {
                    assert_eq!(src.len(), f.samples_per_channel);
                    dst.extend_from_slice(src);
                }
                d.frames += 1;
            }
            Err(e) => d.errors.push(e),
        }
    }
    d
}

/// One reference fingerprint (see harness/codecs/dca/fingerprint.py).
pub struct Fingerprint {
    pub name: String,
    pub channels: usize,
    pub samples: usize,
    pub rate: u32,
    /// Per channel: block size and block sums.
    pub blocks: Vec<(usize, Vec<f64>)>,
    /// Per channel: (sample index, value).
    pub spots: Vec<Vec<(usize, f64)>>,
}

pub fn parse_fingerprints(text: &str) -> Vec<Fingerprint> {
    let mut out: Vec<Fingerprint> = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        match it.next() {
            Some("stream") => {
                let name = it.next().unwrap().to_string();
                let channels = it.next().unwrap().parse().unwrap();
                let samples = it.next().unwrap().parse().unwrap();
                let rate = it.next().unwrap().parse().unwrap();
                out.push(Fingerprint {
                    name,
                    channels,
                    samples,
                    rate,
                    blocks: Vec::new(),
                    spots: Vec::new(),
                });
            }
            Some("blocks") => {
                let fp = out.last_mut().unwrap();
                let _ch: usize = it.next().unwrap().parse().unwrap();
                let b: usize = it.next().unwrap().parse().unwrap();
                fp.blocks
                    .push((b, it.map(|v| v.parse().unwrap()).collect()));
            }
            Some("spots") => {
                let fp = out.last_mut().unwrap();
                let _ch: usize = it.next().unwrap().parse().unwrap();
                fp.spots.push(
                    it.map(|v| {
                        let (i, x) = v.split_once(':').unwrap();
                        (i.parse().unwrap(), x.parse().unwrap())
                    })
                    .collect(),
                );
            }
            _ => {}
        }
    }
    out
}

/// D4 numeric rules: per-sample |diff| <= 2^-14; per-channel RMS <= 8.8e-6, applied to
/// block sums through |sum d| <= B * rms (Cauchy-Schwarz).
pub fn check(fp: &Fingerprint, d: &Decoded) -> Result<(), String> {
    if d.sample_rate != fp.rate {
        return Err(format!(
            "{}: rate {} != {}",
            fp.name, d.sample_rate, fp.rate
        ));
    }
    if d.planes.len() != fp.channels {
        return Err(format!(
            "{}: channels {} != {}",
            fp.name,
            d.planes.len(),
            fp.channels
        ));
    }
    for (c, plane) in d.planes.iter().enumerate() {
        if plane.len() != fp.samples {
            return Err(format!(
                "{}: ch{c} samples {} != {}",
                fp.name,
                plane.len(),
                fp.samples
            ));
        }
        let (b, sums) = &fp.blocks[c];
        for (k, &want) in sums.iter().enumerate() {
            let s: f64 = plane[k * b..((k + 1) * b).min(plane.len())]
                .iter()
                .map(|&v| f64::from(v))
                .sum();
            if (s - want).abs() > *b as f64 * 8.8e-6 {
                return Err(format!("{}: ch{c} block {k}: sum {s} vs {want}", fp.name));
            }
        }
        for &(i, want) in &fp.spots[c] {
            let got = f64::from(plane[i]);
            if (got - want).abs() > 6.1035e-5 {
                return Err(format!("{}: ch{c} sample {i}: {got} vs {want}", fp.name));
            }
        }
    }
    Ok(())
}
