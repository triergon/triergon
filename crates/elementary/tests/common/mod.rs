//! Authored elementary streams and a driver that feeds any window size and work.
#![allow(dead_code)]
use std::sync::atomic::AtomicBool;
use triergon_bitstream::crc::{crc8, crc16_a52};
use triergon_core::container::{ContainerReader, Event, PacketInfo, SeekReceipt};
use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_elementary::{Format, Limits, Packet, Reader};

/// What a test compares about a packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub offset: u64,
    pub len: usize,
    pub pts: i64,
    pub duration: i64,
    pub start_trim: Option<i64>,
    pub end_trim: Option<i64>,
    pub complete: bool,
}

impl Seen {
    pub fn of(packet: &Packet) -> Seen {
        Seen {
            offset: packet.offset(),
            len: packet.payload().len(),
            pts: packet.pts().unwrap(),
            duration: packet.duration().unwrap(),
            start_trim: packet.trim().start,
            end_trim: packet.trim().end,
            complete: packet.is_complete(),
        }
    }
}

#[derive(Debug, Default)]
pub struct Reading {
    pub packets: Vec<Seen>,
    pub receipts: Vec<SeekReceipt>,
    pub opened: usize,
}

pub fn capabilities(file: &[u8], known: bool) -> SourceCapabilities {
    SourceCapabilities {
        length: known.then_some(file.len() as u64),
        random_access: true,
    }
}

pub fn open<F: Format>(pool: &Budget, file: &[u8], known: bool) -> Reader<F> {
    Reader::new(
        pool,
        SourceId(1),
        capabilities(file, known),
        Limits::default(),
    )
    .unwrap()
}

pub fn supply<F: Format>(
    reader: &mut Reader<F>,
    pool: &Budget,
    file: &[u8],
    request: &InputRequest,
    most: usize,
) {
    let at = request.offset as usize;
    if at >= file.len() {
        reader
            .finish(request.source, request.token.clone())
            .unwrap();
        return;
    }
    let end = file.len().min(at + request.maximum.min(most));
    let bytes = Bytes::copy_from_slice(pool, &file[at..end]).unwrap();
    let window =
        InputWindow::new(request.source, request.token.clone(), request.offset, bytes).unwrap();
    reader.provide(&window).unwrap();
}

/// Run to the end with `work` units per step and at most `most` bytes per window.
pub fn read<F: Format>(
    reader: &mut Reader<F>,
    pool: &Budget,
    file: &[u8],
    work: u64,
    most: usize,
) -> Result<Reading, Error> {
    let cancel = AtomicBool::new(false);
    let mut out = Reading::default();
    for _ in 0..50_000_000 {
        match reader.step(&mut WorkBudget::new(work), &cancel)? {
            Step::Ready(Event::Packet(packet)) => out.packets.push(Seen::of(&packet)),
            Step::Ready(Event::Seek(receipt)) => out.receipts.push(receipt),
            Step::Ready(Event::Other(_)) => out.opened += 1,
            Step::NeedInput(request) => supply(reader, pool, file, &request, most),
            Step::Yield => {}
            Step::Backpressure(pressure) => panic!("{pressure:?}"),
            Step::End(_) => return Ok(out),
        }
    }
    panic!("the reader did not finish");
}

pub fn read_file<F: Format>(
    file: &[u8],
    work: u64,
    most: usize,
    known: bool,
) -> Result<Reading, Error> {
    let pool = Budget::new(64 << 20).unwrap();
    let mut reader = open::<F>(&pool, file, known);
    read(&mut reader, &pool, file, work, most)
}

/// Deterministic filler without long zero runs.
pub fn noise(seed: u32, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 7) as u8
        })
        .collect()
}

/// An ID3v2.4 tag with `body` bytes.
pub fn id3v2(body: usize) -> Vec<u8> {
    let size = body as u32;
    let mut tag = b"ID3\x04\x00\x00".to_vec();
    tag.extend([
        (size >> 21) as u8 & 127,
        (size >> 14) as u8 & 127,
        (size >> 7) as u8 & 127,
        size as u8 & 127,
    ]);
    tag.extend(std::iter::repeat_n(0u8, body));
    tag
}

pub fn id3v1() -> Vec<u8> {
    let mut tag = b"TAG".to_vec();
    tag.resize(128, b' ');
    tag
}

/// MPEG-1 Layer III, 128 kbit/s, 44.1 kHz, joint stereo; `padded` adds one byte.
pub fn mp3_frame(seed: u32, padded: bool) -> Vec<u8> {
    let word: u32 = 0xfffb_9064 | (u32::from(padded) << 9);
    let mut frame = word.to_be_bytes().to_vec();
    // Payload without 0xff, so no candidate sync hides inside a frame.
    frame.extend(
        noise(seed, 413 + usize::from(padded))
            .into_iter()
            .map(|b| b & 0x7f),
    );
    frame
}

/// An Info frame with a LAME tag (delay, padding) describing `frames` audio frames of
/// `bytes` bytes in all, the Info frame included.
pub fn mp3_info(frames: u32, bytes: u32, delay: u16, padding: u16) -> Vec<u8> {
    let mut frame = vec![0u8; 417];
    frame[..4].copy_from_slice(&0xfffb_9064u32.to_be_bytes());
    let at = 36;
    frame[at..at + 4].copy_from_slice(b"Info");
    frame[at + 4..at + 8].copy_from_slice(&7u32.to_be_bytes());
    frame[at + 8..at + 12].copy_from_slice(&frames.to_be_bytes());
    frame[at + 12..at + 16].copy_from_slice(&bytes.to_be_bytes());
    for i in 0..100 {
        frame[at + 16 + i] = (i * 256 / 100) as u8;
    }
    let lame = at + 16 + 100;
    frame[lame..lame + 9].copy_from_slice(b"LAME3.100");
    frame[lame + 21] = (delay >> 4) as u8;
    frame[lame + 22] = ((delay & 15) << 4) as u8 | (padding >> 8) as u8;
    frame[lame + 23] = padding as u8;
    frame
}

/// A FLAC frame header for fixed blocking: 4096 samples, 44.1 kHz, mono, 16 bits.
fn flac_header(number: u32, variable: bool) -> Vec<u8> {
    let mut header = vec![0xff, 0xf8 | u8::from(variable), 0xc9, 0x08];
    // UTF-8-like coded number.
    match number {
        0..=0x7f => header.push(number as u8),
        0x80..=0x7ff => header.extend([0xc0 | (number >> 6) as u8, 0x80 | (number & 63) as u8]),
        _ => header.extend([
            0xe0 | (number >> 12) as u8,
            0x80 | ((number >> 6) & 63) as u8,
            0x80 | (number & 63) as u8,
        ]),
    }
    header.push(crc8(0, &header));
    header
}

/// A FLAC frame with a valid CRC-16 over `len` body bytes.
pub fn flac_frame(number: u32, len: usize) -> Vec<u8> {
    let mut frame = flac_header(number, false);
    frame.extend(noise(number + 11, len));
    let crc = crc16_a52(0, &frame);
    frame.extend(crc.to_be_bytes());
    frame
}

pub fn streaminfo(total: u64) -> [u8; 34] {
    let mut b = [0u8; 34];
    b[..4].copy_from_slice(&[0x10, 0, 0x10, 0]);
    let packed: u64 = (44100 << 44) | (15 << 36) | total;
    b[10..18].copy_from_slice(&packed.to_be_bytes());
    b
}

/// `fLaC`, STREAMINFO, a SEEKTABLE of (sample, offset) points and PADDING.
pub fn flac_head(total: u64, points: &[(u64, u64)]) -> Vec<u8> {
    let mut file = b"fLaC\x00\x00\x00\x22".to_vec();
    file.extend(streaminfo(total));
    let table = points.len() * 18;
    file.extend([3, 0, (table >> 8) as u8, table as u8]);
    for &(sample, offset) in points {
        file.extend(sample.to_be_bytes());
        file.extend(offset.to_be_bytes());
        file.extend(4096u16.to_be_bytes());
    }
    file.extend([0x81, 0, 0, 100]);
    file.extend([0; 100]);
    file
}

/// An ADTS frame: AAC-LC, 44.1 kHz, stereo, no CRC, `len` bytes in all.
pub fn adts_frame(seed: u32, len: usize) -> Vec<u8> {
    let l = len as u32;
    let mut frame = vec![
        0xff,
        0xf1,
        (1 << 6) | (4 << 2),
        (2 << 6) | ((l >> 11) & 3) as u8,
        (l >> 3) as u8,
        ((l & 7) << 5) as u8 | 0x1f,
        0xfc,
    ];
    frame.extend(noise(seed, len - 7).into_iter().map(|b| b & 0x7f));
    frame
}

/// An AC-3 frame: 48 kHz, 128 kbit/s (512 bytes), stereo.
pub fn ac3_frame(seed: u32) -> Vec<u8> {
    let mut frame = vec![0x0b, 0x77, 0x12, 0x34, 0x10, 0x40, 0x40, 0x00];
    frame.extend(noise(seed, 512 - 8).into_iter().map(|b| b & 0x07));
    frame
}

/// An E-AC-3 frame of 256 bytes: 48 kHz, six blocks, stereo; `dependent` makes it a
/// dependent substream.
pub fn eac3_frame(seed: u32, dependent: bool) -> Vec<u8> {
    let stream_type = u8::from(dependent);
    let mut frame = vec![0x0b, 0x77, stream_type << 6, 127, 0x34, 0x87, 0xc0, 0x00];
    frame.extend(noise(seed, 256 - 8).into_iter().map(|b| b & 0x07));
    frame
}
