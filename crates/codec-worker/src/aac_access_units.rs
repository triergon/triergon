//! Decode preloaded AAC access units and fold every native PCM plane.

use std::io::Write;
use triergon::Budget;
use triergon_codec::{AudioFrame, SampleFormat};
use triergon_session::TrackDecoder;

use crate::Output;

const PADDING: usize = 64;
const SAMPLES: usize = 1024;
const MAX_PACKET: usize = 64 << 10;

fn read_u32(data: &mut &[u8]) -> Result<u32, String> {
    let (word, rest) = data.split_first_chunk::<4>().ok_or("truncated AAC input")?;
    *data = rest;
    Ok(u32::from_le_bytes(*word))
}

pub fn decode(mut data: &[u8], out: &mut Output) -> Result<(), String> {
    let rate = read_u32(&mut data)?;
    let channels = read_u32(&mut data)? as usize;
    let packets = read_u32(&mut data)?;
    let config_len = read_u32(&mut data)? as usize;
    if rate == 0
        || !(1..=8).contains(&channels)
        || packets == 0
        || !(1..=MAX_PACKET).contains(&config_len)
    {
        return Err("AAC configuration bounds".into());
    }
    let (config, rest) = data
        .split_at_checked(config_len)
        .ok_or("truncated AAC configuration")?;
    data = rest;
    let pool =
        Budget::new(TrackDecoder::DEFAULT_STORAGE_BYTES).map_err(|error| format!("{error:?}"))?;
    let mut decoder = TrackDecoder::aac_access_units_in(config, &pool)
        .map_err(|error| error.0)?
        .into_decoder();
    let mut frame = AudioFrame::default();
    for _ in 0..packets {
        let size = read_u32(&mut data)? as usize;
        if !(1..=MAX_PACKET).contains(&size) {
            return Err("AAC packet bounds".into());
        }
        let (packet, rest) = data.split_at_checked(size).ok_or("truncated AAC packet")?;
        let (padding, rest) = rest
            .split_first_chunk::<PADDING>()
            .ok_or("truncated AAC padding")?;
        if *padding != [0; PADDING] {
            return Err("nonzero AAC padding".into());
        }
        data = rest;
        decoder
            .decode(packet, &mut frame)
            .map_err(|error| error.0)?;
        if frame.sample_rate != rate
            || frame.planes.len() != channels
            || frame.samples_per_channel != SAMPLES
            || frame.sample_format != SampleFormat::Float
        {
            return Err("AAC output shape".into());
        }
        for plane in &frame.planes {
            if plane.len() != SAMPLES {
                return Err("AAC output plane size".into());
            }
            out.digest.update_f32(plane);
            if let Some(raw) = out.raw.as_mut() {
                for sample in plane {
                    raw.write_all(&sample.to_le_bytes())
                        .map_err(|error| error.to_string())?;
                }
            }
        }
        out.bytes += (SAMPLES * channels * size_of::<f32>()) as u64;
        out.frames += 1;
    }
    if !data.is_empty() {
        return Err("AAC trailing input".into());
    }
    decoder.flush(&mut frame).map_err(|error| error.0)?;
    if frame.samples_per_channel != 0 {
        return Err("AAC unexpected delayed output".into());
    }
    Ok(())
}
