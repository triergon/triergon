//! ADTS for an AudioSpecificConfig of channel configuration 0: its program config
//! element opens the raw data block of the first frame published (ISO/IEC 14496-3,
//! 1.A.4.3), written from the syntax of 4.4.1.1 independently of the bridges.
use crate::framing::ready;
use crate::support::{View, byte_input, collect, index_job, off, packets, run_index, run_open};
use triergon_bridge::{
    aac::AacBridge,
    access::{AccessUnitLimits, AccessUnits},
};
use triergon_core::container::{CodecId, MediaKind, PacketInfo, RandomAccess, TrackId, TrackInfo};
use triergon_core::{Budget, Error, SourceCapabilities, SourceId, TimeBase, WorkBudget};
use triergon_framing::{
    BuildStep,
    aac::{AacAscParser, AacConfigLimits, AacSource},
};
use triergon_isobmff::{
    Next,
    config::{AacConfigurationJob, ConfigurationLimits},
};
use triergon_playback::{
    ByteSource, Engine, Output, Player, PlayerPolicy, Selection, SourceOwner,
    mp4::{Mp4Open, Mp4Packet, OpenLimits, PlanLimits},
};

const ID: SourceId = SourceId(41);
const PROGRAM_MOV: &[u8] =
    include_bytes!("../../../playback/tests/fixtures/mp4/avc-aac-program.mov");

/// `fields` as `(width, value)` pairs, most significant bit first, then zero bits to
/// the next byte boundary.
fn pack(fields: &[(u32, u32)]) -> Vec<u8> {
    let mut bits = Vec::new();
    for &(width, value) in fields {
        bits.extend((0..width).rev().map(|bit| (value >> bit) & 1 == 1));
    }
    bits.chunks(8)
        .map(|byte| {
            byte.iter()
                .enumerate()
                .fold(0u8, |acc, (i, &bit)| acc | (u8::from(bit) << (7 - i)))
        })
        .collect()
}

/// The fields of `program_config_element()` from `element_instance_tag` through the
/// LFE list of the fixture's 6.1 programme: LC at 48 kHz; a front centre SCE 0 and a
/// front CPE 0, a back CPE 1 and a back centre SCE 1, LFE 0; no mixdown.
const FIXTURE_PCE: [(u32, u32); 21] = [
    (4, 0),
    (2, 1),
    (4, 3),
    (4, 2),
    (4, 0),
    (4, 2),
    (2, 1),
    (3, 0),
    (4, 0),
    (1, 0),
    (1, 0),
    (1, 0),
    (1, 0),
    (4, 0),
    (1, 1),
    (4, 0),
    (1, 1),
    (4, 1),
    (1, 0),
    (4, 1),
    (4, 0),
];

/// The fixture's element as a raw data block opens with it: `ID_PCE`, the fields,
/// alignment from the start of the block, and the four-byte comment.
fn fixture_element() -> Vec<u8> {
    let mut fields = vec![(3, 5)];
    fields.extend_from_slice(&FIXTURE_PCE);
    let mut element = pack(&fields);
    element.push(4);
    element.extend_from_slice(b"Lavc");
    element
}

fn frame_length(adts: &[u8]) -> usize {
    (usize::from(adts[3] & 3) << 11) | (usize::from(adts[4]) << 3) | usize::from(adts[5] >> 5)
}

fn channel_configuration(adts: &[u8]) -> u8 {
    ((adts[2] & 1) << 2) | (adts[3] >> 6)
}

fn fixture_engine(pool: &Budget, identify: bool) -> (Engine, SourceOwner<ByteSource>) {
    let capabilities = SourceCapabilities {
        length: Some(PROGRAM_MOV.len() as u64),
        random_access: true,
    };
    let mut open = Mp4Open::new(
        byte_input(pool, ID, PROGRAM_MOV, 4096, capabilities, 4096).unwrap(),
        OpenLimits::default(),
    )
    .unwrap();
    run_open(&mut open, 17).unwrap();
    let opened = open.finish().unwrap();
    let mut job = index_job(&opened.movie, View::Source, PlanLimits::default()).unwrap();
    run_index(&mut job, 17).unwrap();
    let mut plan = job.finish().unwrap();
    if identify {
        while !plan
            .identify_tracks(ConfigurationLimits::default(), &mut WorkBudget::new(1))
            .unwrap()
        {}
    }
    (
        Engine::mp4(opened.input, plan, 1 << 20).unwrap(),
        opened.source,
    )
}

#[test]
fn access_units_open_the_first_frame_of_a_program_configuration_with_its_element() {
    let pool = Budget::new(64 << 20).unwrap();
    let (engine, source) = fixture_engine(&pool, true);
    let track = (0..engine.track_count())
        .map(|index| engine.track(index).unwrap())
        .find(|track| track.codec() == CodecId::Aac)
        .unwrap();
    let id = track.id();
    let mut setup = AccessUnits::for_track(&pool, &track, AccessUnitLimits::default()).unwrap();
    let mut units = ready(|work| setup.step(work, &off()).unwrap());
    let mut player = Player::new(engine, source, Selection::All, PlayerPolicy::default()).unwrap();
    let out = collect(&mut player, 17).unwrap();
    let element = fixture_element();
    assert_eq!(element.len(), 13);
    let mut frames = 0;
    for packet in out
        .iter()
        .filter_map(Output::packet)
        .filter(|packet| packet.track() == id)
    {
        let mut job = units.packet(&packet).unwrap();
        let unit = ready(|work| job.step(work, &off()).unwrap());
        let adts = unit.payload();
        assert_eq!(channel_configuration(adts), 0);
        assert_eq!(frame_length(adts), adts.len());
        if frames == 0 {
            assert_eq!(&adts[7..20], element.as_slice());
            assert_eq!(&adts[20..], packet.payload());
        } else {
            assert_eq!(&adts[7..], packet.payload(), "frame {frames}");
        }
        frames += 1;
    }
    assert_eq!(frames, 114);
}

fn fixture_bridge(pool: &Budget, packet: &Mp4Packet) -> AacBridge {
    let entry = &packet.configuration;
    let mut find =
        AacConfigurationJob::new(&entry.description, ConfigurationLimits::default()).unwrap();
    let record = loop {
        match find.poll(&mut WorkBudget::new(1), &off()).unwrap() {
            Next::Item(value) => break value.bytes,
            Next::Yield => {}
            Next::End => panic!("sample entry has no AAC record"),
        }
    };
    let mut parser = AacAscParser::new(
        pool,
        AacSource {
            source: ID,
            generation: entry.revision,
            offset: 0,
        },
        record,
        AacConfigLimits {
            maximum_record_bytes: 4096,
            maximum_pce_elements: 64,
            maximum_comment_bytes: 255,
        },
    )
    .unwrap();
    let configuration = ready(|work| parser.step(work, &off()).unwrap());
    AacBridge::new(
        pool,
        ID,
        entry.track.track_id,
        entry.revision,
        configuration,
        8191,
    )
    .unwrap()
}

#[test]
fn aac_bridge_opens_its_first_published_frame_with_the_program_config_element() {
    let pool = Budget::new(64 << 20).unwrap();
    let (engine, source) = fixture_engine(&pool, false);
    let mut player = Player::new(engine, source, Selection::All, PlayerPolicy::default()).unwrap();
    let out = collect(&mut player, 17).unwrap();
    let audio: Vec<_> = packets(&out)
        .into_iter()
        .filter(|packet| packet.configuration.description.format == *b"mp4a")
        .cloned()
        .collect();
    assert_eq!(audio.len(), 114);
    let mut bridge = fixture_bridge(&pool, &audio[0]);
    let element = fixture_element();
    // A job dropped before it publishes, or still yielding, leaves the element pending.
    drop(bridge.packet(audio[0].clone()).unwrap());
    let mut job = bridge.packet(audio[0].clone()).unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(1), &off()).unwrap(),
        BuildStep::Yield
    ));
    let first = ready(|work| job.step(work, &off()).unwrap());
    let adts = first.adts.bytes();
    assert_eq!(channel_configuration(adts), 0);
    assert_eq!(frame_length(adts), adts.len());
    assert_eq!(&adts[7..20], element.as_slice());
    assert_eq!(&adts[20..], audio[0].source.payload.as_ref());
    assert_eq!(
        first.adts.payload_source().offset,
        audio[0].source.sample.range.start
    );
    for packet in &audio[1..] {
        let mut job = bridge.packet(packet.clone()).unwrap();
        let output = ready(|work| job.step(work, &off()).unwrap());
        assert_eq!(&output.adts.bytes()[7..], packet.source.payload.as_ref());
    }
}

/// A track whose AudioSpecificConfig declares channel configuration 0 and a stereo
/// program: one front CPE, with a one-byte comment.
struct ProgramTrack {
    configuration: Vec<u8>,
}

/// `element_instance_tag` 2, LC at 48 kHz, one front CPE 0, no mixdown.
const STEREO_PCE: [(u32, u32); 14] = [
    (4, 2),
    (2, 1),
    (4, 3),
    (4, 1),
    (4, 0),
    (4, 0),
    (2, 0),
    (3, 0),
    (4, 0),
    (1, 0),
    (1, 0),
    (1, 0),
    (1, 1),
    (4, 0),
];

impl ProgramTrack {
    fn new() -> Self {
        // AAC LC, 48 kHz, channel configuration 0, GASpecificConfig flags clear. The
        // element starts 16 bits in, so its comment needs one alignment bit here.
        let mut fields = vec![(5, 2), (4, 3), (4, 0), (1, 0), (1, 0), (1, 0)];
        fields.extend_from_slice(&STEREO_PCE);
        let mut configuration = pack(&fields);
        configuration.extend_from_slice(&[1, b'x']);
        Self { configuration }
    }

    /// The element as it opens a raw data block: six alignment bits there.
    fn element() -> Vec<u8> {
        let mut fields = vec![(3, 5)];
        fields.extend_from_slice(&STEREO_PCE);
        let mut element = pack(&fields);
        element.extend_from_slice(&[1, b'x']);
        element
    }
}

impl TrackInfo for ProgramTrack {
    fn id(&self) -> TrackId {
        TrackId(1)
    }
    fn kind(&self) -> MediaKind {
        MediaKind::Audio
    }
    fn codec_tag(&self) -> &[u8] {
        b"mp4a"
    }
    fn codec(&self) -> CodecId {
        CodecId::Aac
    }
    fn configuration(&self) -> &[u8] {
        &self.configuration
    }
}

struct Unit(Vec<u8>);

impl PacketInfo for Unit {
    fn track(&self) -> TrackId {
        TrackId(1)
    }
    fn payload(&self) -> &[u8] {
        &self.0
    }
    fn time_base(&self) -> TimeBase {
        TimeBase::new(1, 48_000).unwrap()
    }
    fn pts(&self) -> Option<i64> {
        None
    }
    fn dts(&self) -> Option<i64> {
        None
    }
    fn duration(&self) -> Option<i64> {
        None
    }
    fn random_access(&self) -> RandomAccess {
        RandomAccess::Sync
    }
}

fn program_units(pool: &Budget, unit_bytes: usize) -> AccessUnits {
    let limits = AccessUnitLimits {
        unit_bytes,
        ..AccessUnitLimits::default()
    };
    let mut setup = AccessUnits::for_track(pool, &ProgramTrack::new(), limits).unwrap();
    ready(|work| setup.step(work, &off()).unwrap())
}

fn frame(units: &mut AccessUnits, packet: &Unit) -> Vec<u8> {
    let mut job = units.packet(packet).unwrap();
    let unit = ready(|work| job.step(work, &off()).unwrap());
    unit.payload().to_vec()
}

#[test]
fn the_program_element_is_realigned_and_sent_once() {
    let pool = Budget::new(1 << 20).unwrap();
    let element = ProgramTrack::element();
    assert_eq!(element.len(), 8);
    let mut units = program_units(&pool, 1 << 20);
    let (first, second) = (
        Unit(vec![0x21, 0x10, 0x05, 0x00]),
        Unit(vec![0x21, 0x10, 0x07]),
    );
    // A dropped job leaves the element to the next.
    drop(units.packet(&first).unwrap());
    let adts = frame(&mut units, &first);
    assert_eq!(frame_length(&adts), 7 + element.len() + first.0.len());
    assert_eq!(&adts[7..15], element.as_slice());
    assert_eq!(&adts[15..], first.0.as_slice());
    assert_eq!(&frame(&mut units, &second)[7..], second.0.as_slice());
}

#[test]
fn a_frame_too_large_for_the_element_leaves_it_to_the_next() {
    let pool = Budget::new(1 << 20).unwrap();
    let element = ProgramTrack::element();
    let large = Unit(vec![0x21; 16]);
    let mut units = program_units(&pool, 7 + large.0.len() + element.len() - 1);
    assert!(matches!(
        units.packet(&large),
        Err(Error::Limit { requested, .. }) if requested == 7 + element.len() + large.0.len()
    ));
    let small = Unit(vec![0x21; 4]);
    assert_eq!(&frame(&mut units, &small)[7..15], element.as_slice());
    assert_eq!(&frame(&mut units, &large)[7..], large.0.as_slice());
}

#[test]
fn a_block_that_opens_with_a_program_element_is_framed_as_it_is() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut units = program_units(&pool, 1 << 20);
    let carried = Unit([ProgramTrack::element(), vec![0x21, 0x10]].concat());
    assert_eq!(&frame(&mut units, &carried)[7..], carried.0.as_slice());
    let plain = Unit(vec![0x21, 0x10, 0x05]);
    assert_eq!(&frame(&mut units, &plain)[7..], plain.0.as_slice());
}
