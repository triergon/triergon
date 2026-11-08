//! Reading a whole in-memory file through `AnyReader`.
use std::sync::atomic::AtomicBool;
use triergon::{
    AnyPacket, AnyReader, Budget, Bytes, ContainerReader, Event, InputRequest, InputWindow,
    SourceCapabilities, SourceId, Step, WorkBudget,
};

/// Open `file` from its first bytes and read every packet.
pub fn read(file: &[u8]) -> (AnyReader, Vec<AnyPacket>) {
    let pool = Budget::new(16 << 20).unwrap();
    let capabilities = SourceCapabilities {
        length: Some(file.len() as u64),
        random_access: true,
    };
    let prefix = &file[..file.len().min(4096)];
    let mut reader = AnyReader::open(prefix, &pool, SourceId(1), capabilities).unwrap();
    let cancel = AtomicBool::new(false);
    let mut packets = Vec::new();
    loop {
        match reader.step(&mut WorkBudget::new(4096), &cancel).unwrap() {
            Step::Ready(Event::Packet(packet)) => packets.push(packet),
            Step::Ready(_) | Step::Yield => {}
            Step::NeedInput(request) => answer(&mut reader, &pool, file, request),
            Step::Backpressure(pressure) => panic!("{pressure:?}"),
            Step::End(_) => return (reader, packets),
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
