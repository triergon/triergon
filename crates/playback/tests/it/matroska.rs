use crate::support::players::*;
use crate::support::*;
use triergon_core::Budget;
use triergon_playback::*;

#[test]
fn real_encoded_matroska_uses_old183_reader_after_sequential_probe() {
    let raw = include_bytes!("../fixtures/mkv/observed-11.mkv");
    let (selected, b, _) = probe(raw, 7, true);
    assert_eq!(selected.format, Format::Matroska);
    let engine = Engine::matroska(
        triergon_matroska::Reader::new(
            &b,
            ID,
            selected.input.capabilities(),
            triergon_matroska::Limits::default(),
            4096,
        )
        .unwrap(),
    );
    let mut p = Player::new(
        engine,
        selected.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let out = collect(&mut p, 17).unwrap();
    let packets: Vec<_> = out
        .iter()
        .filter_map(|o| {
            if let Event::Matroska(triergon_matroska::Event::Packet(p)) = o.event() {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    assert!(!packets.is_empty());
    assert!(packets.iter().any(|p| {
        p.configuration
            .tracks()
            .iter()
            .any(|t| t.codec.as_ref() == "V_MPEG4/ISO/AVC")
    }));
    assert!(packets.iter().all(|p| !p.packet.data.is_empty()));
    drop(out);
    drop(p);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
