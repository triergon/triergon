use std::io::Write;
use std::sync::atomic::AtomicBool;

use triergon::elementary::{Ac3, Adts, Flac, FlacBlockKind, Format, MpegAudio, TagKind};
use triergon::framing::mpeg_audio::{MpegInfoKind, MpegVersion};

use super::metadata::opened;
use super::{Work, record};

type Emit<'a> = dyn FnMut(u64, &[u64], &[u8]) -> Result<(), String> + 'a;

fn inspect<F: Format>(
    input: &[u8],
    emit: &mut Emit<'_>,
    facts: impl FnOnce(&F, &mut Emit<'_>) -> Result<(), String>,
) -> Result<(), String> {
    let Ok((mut reader, _)) = opened::<F>(input) else {
        return emit(8, &[], &[]);
    };
    // The trailing tag is discovered during the last read.
    let cancel = AtomicBool::new(false);
    let mut refused = false;
    loop {
        match reader.next_packet(&cancel) {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {
                refused = true;
                break;
            }
        }
    }
    facts(reader.reader().format(), emit)?;
    for tag in reader.reader().tags() {
        let kind = match tag.kind {
            TagKind::Id3v2 => 2,
            TagKind::Id3v1 => 1,
            TagKind::Ape => 3,
        };
        let bytes = tag.bytes.as_deref().unwrap_or_default();
        emit(5, &[kind, tag.offset, tag.len, bytes.len() as u64], bytes)?;
    }
    if let Some(track) = reader.reader().stream() {
        emit(
            2,
            &[
                u64::from(track.sample_rate()),
                u64::from(track.channels().unwrap_or(0)),
                track.duration().unwrap_or(u64::MAX),
            ],
            &[],
        )?;
    }
    emit(if refused { 8 } else { 7 }, &[], &[])
}

fn flac(format: &Flac, emit: &mut Emit<'_>) -> Result<(), String> {
    if let Some(info) = format.stream_info() {
        emit(
            3,
            &[
                u64::from(info.min_block_size),
                u64::from(info.max_block_size),
                u64::from(info.min_frame_size),
                u64::from(info.max_frame_size),
                u64::from(info.sample_rate),
                u64::from(info.channels),
                u64::from(info.bits_per_sample),
                info.total_samples,
            ],
            &info.md5,
        )?;
    }
    for block in format.metadata() {
        let kind = match block.kind {
            FlacBlockKind::StreamInfo => 0,
            FlacBlockKind::Padding => 1,
            FlacBlockKind::Application => 2,
            FlacBlockKind::SeekTable => 3,
            FlacBlockKind::VorbisComment => 4,
            FlacBlockKind::CueSheet => 5,
            FlacBlockKind::Picture => 6,
            FlacBlockKind::Reserved(kind) => kind,
        };
        let body = block.body.as_deref().unwrap_or_default();
        emit(
            6,
            &[u64::from(kind), u64::from(block.len), body.len() as u64],
            body,
        )?;
    }
    for point in format.seek_points() {
        emit(
            4,
            &[point.sample, point.offset, u64::from(point.samples)],
            &[],
        )?;
    }
    Ok(())
}

fn mpeg(format: &MpegAudio, emit: &mut Emit<'_>) -> Result<(), String> {
    if let Some(header) = format.header() {
        let version = match header.version {
            MpegVersion::Mpeg1 => 3,
            MpegVersion::Mpeg2 => 2,
            MpegVersion::Mpeg25 => 0,
        };
        emit(
            3,
            &[
                version,
                header.layer as u64 + 1,
                u64::from(header.crc),
                u64::from(header.bitrate_kbps),
                u64::from(header.sample_rate),
                u64::from(header.padding),
                header.channel_mode as u64,
                u64::from(header.mode_extension),
                u64::from(header.copyright),
                u64::from(header.original),
                header.emphasis as u64,
            ],
            &[],
        )?;
    }
    if let Some(info) = format.info_frame() {
        let kind = match info.frame.kind {
            MpegInfoKind::Xing => 1,
            MpegInfoKind::Info => 2,
            MpegInfoKind::Vbri => 3,
        };
        let toc: &[u8] = info.frame.toc.as_ref().map_or(&[], |value| value);
        emit(
            6,
            &[
                kind,
                info.offset,
                info.len,
                u64::from(info.consistent),
                info.frame.frames.map_or(u64::MAX, u64::from),
                info.frame.bytes.map_or(u64::MAX, u64::from),
                toc.len() as u64,
            ],
            toc,
        )?;
    }
    if let Some(lame) = format.lame() {
        emit(
            4,
            &[
                u64::from(lame.encoder_delay),
                u64::from(lame.encoder_padding),
            ],
            &lame.encoder,
        )?;
    }
    Ok(())
}

fn adts(format: &Adts, emit: &mut Emit<'_>) -> Result<(), String> {
    if let Some(h) = format.header() {
        emit(
            3,
            &[
                u64::from(h.mpeg2_id()),
                u64::from(h.layer()),
                u64::from(h.protection_absent()),
                u64::from(h.profile_code()),
                u64::from(h.frequency_index()),
                u64::from(h.private_bit()),
                u64::from(h.channel_configuration()),
                u64::from(h.original_copy()),
                u64::from(h.home()),
                u64::from(h.copyright_id_bit()),
                u64::from(h.copyright_id_start()),
                u64::from(h.frame_length()),
                u64::from(h.buffer_fullness()),
                u64::from(h.raw_blocks_minus_one()),
            ],
            &h.raw_prefix(),
        )?;
    }
    Ok(())
}

fn ac3(format: &Ac3, emit: &mut Emit<'_>) -> Result<(), String> {
    if let Some(h) = format.header() {
        emit(
            3,
            &[
                h.frame_bytes as u64,
                u64::from(h.sample_rate_hz),
                u64::from(h.blocks),
                u64::from(h.bsid),
                u64::from(h.acmod),
                u64::from(h.lfe_on),
            ],
            &[],
        )?;
    }
    Ok(())
}

pub(super) fn run(
    input: &[u8],
    format: &str,
    work: &mut Work,
    mut trace: Option<&mut dyn Write>,
) -> Result<(), String> {
    let mut emit = |kind, words: &[u64], bytes: &[u8]| {
        let dest = trace.as_mut().map(|writer| &mut **writer as &mut dyn Write);
        record(work, dest, kind, words, bytes)
    };
    match format {
        "flac" => inspect::<Flac>(input, &mut emit, flac),
        "mp3" => inspect::<MpegAudio>(input, &mut emit, mpeg),
        "aac" => inspect::<Adts>(input, &mut emit, adts),
        "ac3" | "eac3" => inspect::<Ac3>(input, &mut emit, ac3),
        _ => Err("unknown elementary format".into()),
    }
}
