use std::{io::Write, sync::atomic::AtomicBool};

use triergon::{
    Budget, PacketInfo, TrackInfo,
    codec::flac::{DecodeError, Decoder, IntegerFrame, compat::ffmpeg::CrcPolicy},
    elementary::Flac,
};
use triergon_harness_protocol::md5;

use super::metadata::opened;
use super::{Totals, record};

// Fixed-block frame, 16 mono samples at 32 kHz, constant signed value -32767.
const MONO_FRAME: &[u8] = b"\xff\xf8\x68\x08\x00\x0f\x26\x00\x80\x01\x6e\x8c";

#[derive(Clone, Copy)]
pub(super) enum Mode {
    Configured,
    Bare,
    Crc,
}

fn pack(frame: &IntegerFrame, pcm: &mut Vec<u8>) -> Result<(), String> {
    let block = frame.samples_per_channel;
    let width = usize::from(frame.bits_per_sample).div_ceil(8);
    let bytes = block
        .checked_mul(frame.layout.channels())
        .and_then(|count| count.checked_mul(width))
        .ok_or("FLAC integer output size overflow")?;
    if bytes > (32 << 20) - pcm.len() {
        return Err("FLAC integer comparison exceeds 32 MiB".into());
    }
    pcm.try_reserve(bytes).map_err(|e| e.to_string())?;
    let mut planes: Vec<_> = frame.planes().map(|plane| plane.iter()).collect();
    for _ in 0..block {
        for plane in &mut planes {
            let value = plane.next().ok_or("short FLAC integer plane")?;
            pcm.extend_from_slice(&value.to_le_bytes()[..width]);
        }
    }
    Ok(())
}

fn decode(decoder: &mut Decoder, packet: &[u8], frame: &mut IntegerFrame) -> Result<(), String> {
    let decoded = decoder
        .decode_integer(packet, frame)
        .map_err(|error| format!("{error:?}"))?;
    if decoded.sample_rate != frame.sample_rate
        || decoded.layout != frame.layout
        || decoded.bits_per_sample != frame.bits_per_sample
        || decoded.samples_per_channel != frame.samples_per_channel
        || !frame.plane(frame.layout.channels()).is_empty()
    {
        return Err("inconsistent FLAC integer output".into());
    }
    Ok(())
}

pub(super) fn run(
    input: &[u8],
    mode: Mode,
    work: &mut Totals,
    mut trace: Option<&mut dyn Write>,
) -> Result<(), String> {
    let error = |value: &dyn std::fmt::Debug| format!("{value:?}");
    let bare = matches!(mode, Mode::Bare);
    let budget = Budget::new(64 << 20).map_err(|e| error(&e))?;
    let (mut source, mut next) = opened::<Flac>(input).map_err(|e| error(&e))?;
    let track = source.reader().stream().ok_or("missing FLAC track")?;
    let configuration = if bare { &[] } else { track.configuration() };
    let mut decoder = Decoder::new_in(configuration, &budget).map_err(|e| error(&e))?;
    let mut integer = decoder.integer_frame();
    let mut pcm = Vec::new();
    let mut format = None;
    let mut samples = 0;
    let cancel = AtomicBool::new(false);
    while let Some(packet) = next {
        decode(&mut decoder, packet.payload(), &mut integer)?;
        let observed = (
            integer.sample_rate,
            integer.layout.channels(),
            integer.bits_per_sample,
        );
        if format
            .replace(observed)
            .is_some_and(|prior| prior != observed)
        {
            return Err("FLAC stream changed format within the checksum span".into());
        }
        pack(&integer, &mut pcm)?;
        samples += integer.samples_per_channel as u64;
        next = source.next_packet(&cancel).map_err(|e| error(&e))?;
    }
    let (rate, channels, bits) = format.ok_or("no decoded FLAC frames")?;
    record(
        work,
        trace.as_mut().map(|writer| &mut **writer as &mut dyn Write),
        3,
        &[u64::from(rate), channels as u64, u64::from(bits), samples],
        &md5::digest(&pcm),
    )?;
    if matches!(mode, Mode::Crc) {
        return check_crc(&budget, work, trace);
    }
    if bare {
        decode(&mut decoder, MONO_FRAME, &mut integer)?;
        pcm.clear();
        pack(&integer, &mut pcm)?;
        record(
            work,
            trace,
            3,
            &[
                u64::from(integer.sample_rate),
                integer.layout.channels() as u64,
                u64::from(integer.bits_per_sample),
                integer.samples_per_channel as u64,
            ],
            &md5::digest(&pcm),
        )?;
    }
    if decoder.decode_integer(&[], &mut integer).is_ok()
        || integer.samples_per_channel != 0
        || integer.planes().any(|plane| !plane.is_empty())
    {
        return Err("failed FLAC frame retained integer samples".into());
    }
    Ok(())
}

fn check_crc(
    budget: &Budget,
    work: &mut Totals,
    mut trace: Option<&mut dyn Write>,
) -> Result<(), String> {
    let mut bad_footer = MONO_FRAME.to_vec();
    let last = bad_footer.last_mut().ok_or("missing FLAC CRC footer")?;
    *last ^= 1;
    for (mode, policy) in [None, Some(CrcPolicy::Ignore), Some(CrcPolicy::Reject)]
        .into_iter()
        .enumerate()
    {
        let mut decoder = Decoder::new_in(&[], budget).map_err(|error| format!("{error:?}"))?;
        if let Some(policy) = policy {
            decoder = decoder.with_crc_policy(policy);
        }
        let mut integer = decoder.integer_frame();
        for (index, packet) in [MONO_FRAME, bad_footer.as_slice(), MONO_FRAME]
            .into_iter()
            .enumerate()
        {
            let result = decoder.decode_integer(packet, &mut integer);
            let rejected = policy == Some(CrcPolicy::Reject) && index == 1;
            let mut pcm = Vec::new();
            if rejected {
                if !matches!(result, Err(DecodeError::CrcMismatch))
                    || integer.samples_per_channel != 0
                    || integer.planes().any(|plane| !plane.is_empty())
                {
                    return Err("FLAC CRC rejection did not clear the decoded frame".into());
                }
            } else {
                result.map_err(|error| format!("{error:?}"))?;
                pack(&integer, &mut pcm)?;
            }
            record(
                work,
                trace.as_mut().map(|writer| &mut **writer as &mut dyn Write),
                4,
                &[
                    mode as u64,
                    index as u64,
                    u64::from(rejected),
                    integer.samples_per_channel as u64,
                ],
                &md5::digest(&pcm),
            )?;
        }
    }
    Ok(())
}
