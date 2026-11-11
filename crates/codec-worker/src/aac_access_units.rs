//! Decode preloaded AAC access units and fold every native PCM plane.

use std::fs::File;
use std::io::{Read, Write};
use triergon::Budget;
use triergon_codec::{AudioFrame, SampleFormat};
use triergon_session::TrackDecoder;
use triergon_session::compat::aac::Policy;

use crate::Output;

const PADDING: usize = 64;
const SAMPLES: usize = 1024;
const MAX_PACKET: usize = 64 << 10;
const MAX_INPUT: u64 = 64 << 20;
pub const OPERATION: &str = "audio-aac-access-units";
pub const COMPAT_OPERATION: &str = "audio-aac-access-units-compat";

pub fn load(path: &str) -> Result<Vec<u8>, String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || !(16..=MAX_INPUT).contains(&metadata.len()) {
        return Err("AAC input file bounds".into());
    }
    let len = metadata.len() as usize;
    let mut data = Vec::new();
    data.try_reserve_exact(len)
        .map_err(|error| error.to_string())?;
    data.resize(len, 0);
    file.read_exact(&mut data)
        .map_err(|error| error.to_string())?;
    if file.read(&mut [0]).map_err(|error| error.to_string())? != 0 {
        return Err("AAC input changed while reading".into());
    }
    Ok(data)
}

fn read_u32(data: &mut &[u8]) -> Result<u32, String> {
    let (word, rest) = data.split_first_chunk::<4>().ok_or("truncated AAC input")?;
    *data = rest;
    Ok(u32::from_le_bytes(*word))
}

pub fn decode(mut data: &[u8], layout: Policy, out: &mut Output) -> Result<(), String> {
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
    let mut decoder = TrackDecoder::aac_access_units_with_layout_in(config, layout, &pool)
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
        if out.raw.is_some() {
            let layout = frame.layout.speakers();
            if layout.len() != channels {
                return Err("AAC output speaker count".into());
            }
            let mut speakers = [0; 8];
            for (id, speaker) in speakers.iter_mut().zip(layout) {
                *id = *speaker as u8;
            }
            match out.aac_speakers {
                None => out.aac_speakers = Some((speakers, channels)),
                Some(previous) if previous == (speakers, channels) => {}
                Some(_) => return Err("AAC speaker order changed".into()),
            }
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
