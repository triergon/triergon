//! An MP4 DVD subtitle track (`subp` handler, `mp4s` entry with object type
//! 0xE0) is named [`CodecId::VobSub`] with its colour table as configuration.
use std::sync::atomic::AtomicBool;
use triergon_core::container::{CodecId, ContainerReader, Event, MediaKind, PacketInfo, TrackInfo};
use triergon_core::{Budget, Bytes, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget};
use triergon_isobmff::{Reader, ReaderLimits};

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend(kind);
    out.extend(body);
    out
}

fn full(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    atom(kind, &[&[0; 4][..], body].concat())
}

/// An `esds` with one ES descriptor, a decoder configuration of `object_type`
/// and stream type 0x38, and `specific` decoder-specific information.
fn esds(object_type: u8, specific: &[u8]) -> Vec<u8> {
    let dsi = [&[5, specific.len() as u8][..], specific].concat();
    let config = [
        &[4, 13 + dsi.len() as u8, object_type, 0xe1, 0, 0, 0][..],
        &[0; 8],
        &dsi,
    ]
    .concat();
    let es = [
        &[3, 3 + config.len() as u8 + 3, 0, 1, 0][..],
        &config,
        &[6, 1, 2],
    ]
    .concat();
    full(b"esds", &es)
}

/// A movie with one subpicture track whose samples are `samples`.
fn movie(object_type: u8, specific: &[u8], samples: &[&[u8]]) -> Vec<u8> {
    let ftyp = atom(b"ftyp", b"isom\0\0\x02\0isomiso2mp41");
    let mut mdat_body = Vec::new();
    for s in samples {
        mdat_body.extend(*s);
    }
    let mdat = atom(b"mdat", &mdat_body);
    let data_start = (ftyp.len() + 8) as u32;
    let mut mvhd = vec![0; 96];
    mvhd[8..12].copy_from_slice(&1000_u32.to_be_bytes());
    mvhd[16..20].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
    mvhd[20..22].copy_from_slice(&0x0100_u16.to_be_bytes());
    mvhd[92..96].copy_from_slice(&2_u32.to_be_bytes());
    let mut tkhd = vec![0; 80];
    tkhd[8..12].copy_from_slice(&1_u32.to_be_bytes());
    let mut mdhd = vec![0; 20];
    mdhd[8..12].copy_from_slice(&1000_u32.to_be_bytes());
    mdhd[16..18].copy_from_slice(&0x55c4_u16.to_be_bytes());
    let hdlr = full(
        b"hdlr",
        &[&[0; 4][..], b"subp", &[0; 12], b"SubtitleHandler\0"].concat(),
    );
    let entry = atom(
        b"mp4s",
        &[&[0, 0, 0, 0, 0, 0, 0, 1][..], &esds(object_type, specific)].concat(),
    );
    let stsd = full(b"stsd", &[&1_u32.to_be_bytes()[..], &entry].concat());
    let count = samples.len() as u32;
    let stts = full(b"stts", &[1_u32, count, 500].map(u32::to_be_bytes).concat());
    let stsc = full(
        b"stsc",
        &[1_u32, 1, count, 1].map(u32::to_be_bytes).concat(),
    );
    let mut stsz = [0_u32, count].map(u32::to_be_bytes).concat();
    for s in samples {
        stsz.extend((s.len() as u32).to_be_bytes());
    }
    let stco = full(b"stco", &[1_u32, data_start].map(u32::to_be_bytes).concat());
    let stbl = atom(
        b"stbl",
        &[stsd, stts, stsc, full(b"stsz", &stsz), stco].concat(),
    );
    // A self-contained `url ` data reference (flags 1).
    let url = atom(b"url ", &[0, 0, 0, 1]);
    let dinf = atom(
        b"dinf",
        &full(b"dref", &[&1_u32.to_be_bytes()[..], &url].concat()),
    );
    let minf = atom(b"minf", &[full(b"nmhd", &[]), dinf, stbl].concat());
    let mdia = atom(b"mdia", &[full(b"mdhd", &mdhd), hdlr, minf].concat());
    let trak = atom(b"trak", &[full(b"tkhd", &tkhd), mdia].concat());
    let moov = atom(b"moov", &[full(b"mvhd", &mvhd), trak].concat());
    [ftyp, mdat, moov].concat()
}

/// The track's codec, kind and configuration, and the payload of each packet.
fn read(file: &[u8]) -> (CodecId<'static>, MediaKind, Vec<u8>, Vec<Vec<u8>>) {
    let pool = Budget::new(1 << 24).unwrap();
    let capabilities = SourceCapabilities {
        length: Some(file.len() as u64),
        random_access: true,
    };
    let mut reader = Reader::new(
        &pool,
        SourceId(1),
        capabilities,
        ReaderLimits::default(),
        4096,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut packets = Vec::new();
    loop {
        match reader.step(&mut WorkBudget::new(1 << 16), &cancel).unwrap() {
            Step::Ready(Event::Packet(p)) => packets.push(p.payload().to_vec()),
            Step::NeedInput(request) => {
                let at = request.offset as usize;
                if at >= file.len() {
                    reader.finish(request.source, request.token).unwrap();
                } else {
                    let end = file.len().min(at + request.maximum);
                    let bytes = Bytes::copy_from_slice(&pool, &file[at..end]).unwrap();
                    let window =
                        InputWindow::new(request.source, request.token, request.offset, bytes);
                    reader.provide(&window.unwrap()).unwrap();
                }
            }
            Step::End(_) => break,
            _ => {}
        }
    }
    let track = reader.track(0).unwrap();
    let codec = match track.codec() {
        CodecId::VobSub => CodecId::VobSub,
        CodecId::Other(tag) => CodecId::Other(tag.to_vec().leak()),
        other => panic!("unexpected codec {other:?}"),
    };
    (codec, track.kind(), track.configuration().to_vec(), packets)
}

#[test]
fn an_mp4s_track_of_object_type_0xe0_is_a_vobsub_track() {
    let specific: Vec<u8> = (0..16).flat_map(|i| [0, 16 + i, 128, 128]).collect();
    let samples: [&[u8]; 2] = [&[0, 10, 0, 6, 0, 0, 0, 6, 1, 0xff], &[0, 4, 0, 2]];
    let (codec, kind, configuration, packets) = read(&movie(0xe0, &specific, &samples));
    assert_eq!(codec, CodecId::VobSub);
    assert_eq!(kind, MediaKind::Subtitle);
    assert_eq!(configuration, specific);
    assert_eq!(packets, samples.map(<[u8]>::to_vec));
}

#[test]
fn another_object_type_stays_unnamed() {
    let (codec, kind, _, _) = read(&movie(0x40, &[0; 64], &[&[1, 2, 3]]));
    assert_eq!(codec, CodecId::Other(b"mp4s"));
    assert_eq!(kind, MediaKind::Subtitle);
}
