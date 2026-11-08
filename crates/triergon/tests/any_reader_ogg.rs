//! Ogg opens from its first bytes and reads through the neutral model.
#![cfg(feature = "ogg")]
use std::sync::atomic::AtomicBool;
use triergon::{
    AnyPacket, AnyReader, Budget, Bytes, CodecId, ContainerReader, Event, InputRequest,
    InputWindow, MediaKind, PacketInfo, Probe, SourceCapabilities, SourceId, Step, WorkBudget,
    ogg::page::{BEGIN_OF_STREAM, END_OF_STREAM, write_page},
    probe,
};

/// An Opus stream: headers, then five 20 ms CELT packets with a 312-sample pre-skip and
/// a final granule that ends 100 samples before the last packet does.
fn opus_file() -> Vec<u8> {
    let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
    let tags = b"OpusTags\x04\x00\x00\x00test\x00\x00\x00\x00".to_vec();
    let mut file = Vec::new();
    write_page(BEGIN_OF_STREAM, 0, 9, 0, &[head.len() as u8], &head, &mut file);
    write_page(0, 0, 9, 1, &[tags.len() as u8], &tags, &mut file);
    let packet = [0xfc, 0xff, 0xfe];
    let body: Vec<u8> = packet.repeat(5);
    let granule = 312 + 5 * 960 - 312 - 100;
    write_page(END_OF_STREAM, granule, 9, 2, &[3; 5], &body, &mut file);
    file
}

fn drain(reader: &mut AnyReader, pool: &Budget, file: &[u8]) -> Vec<AnyPacket> {
    let cancel = AtomicBool::new(false);
    let mut packets = Vec::new();
    loop {
        match reader.step(&mut WorkBudget::new(4096), &cancel).unwrap() {
            Step::Ready(Event::Packet(packet)) => packets.push(packet),
            Step::Ready(_) | Step::Yield => {}
            Step::NeedInput(request) => answer(reader, pool, file, request),
            Step::Backpressure(pressure) => panic!("{pressure:?}"),
            Step::End(_) => return packets,
        }
    }
}

fn answer(reader: &mut AnyReader, pool: &Budget, file: &[u8], request: InputRequest) {
    let at = request.offset as usize;
    if at >= file.len() {
        reader.finish(request.source, request.token).unwrap();
    } else {
        let end = file.len().min(at + request.maximum);
        let bytes = Bytes::copy_from_slice(pool, &file[at..end]).unwrap();
        let window = InputWindow::new(request.source, request.token, request.offset, bytes);
        reader.provide(&window.unwrap()).unwrap();
    }
}

#[test]
fn any_reader_reads_ogg_opus_with_its_trims() {
    let file = opus_file();
    assert_eq!(probe(&file), Probe::Ogg);
    let pool = Budget::new(4 << 20).unwrap();
    let capabilities = SourceCapabilities {
        length: Some(file.len() as u64),
        random_access: true,
    };
    let mut reader = AnyReader::open(&file[..16], &pool, SourceId(1), capabilities).unwrap();
    let packets = drain(&mut reader, &pool, &file);
    assert_eq!(reader.track_count(), 1);
    let track = reader.track(0).unwrap();
    assert_eq!(track.kind(), MediaKind::Audio);
    assert_eq!(track.codec(), CodecId::Opus);
    assert!(track.configuration().starts_with(b"OpusHead"));
    assert_eq!(packets.len(), 5);
    assert!(packets.iter().all(|p| p.payload() == [0xfc, 0xff, 0xfe]));
    assert_eq!(packets[0].trim().start, Some(312));
    assert_eq!(packets[4].trim().end, Some(100));
    assert_eq!(packets[1].pts(), Some(960 - 312));
}
