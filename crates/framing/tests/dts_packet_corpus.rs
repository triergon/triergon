//! Compare both header inspectors with the core prefix selected by the reference filter.
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, SourceId, WorkBudget};
use triergon_framing::{
    DtsCoreField, DtsHeaderInspector, DtsInspectionIdentity, DtsInspectionStep,
    inspect_dts_header_bits,
};

fn output(command: &mut Command) -> Vec<u8> {
    let done = command.output().unwrap();
    assert!(
        done.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&done.stderr)
    );
    done.stdout
}

fn compare_file(path: &Path, stream: &str, packets: usize) {
    let sizes = output(
        Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                stream,
                "-show_packets",
                "-show_entries",
                "packet=size",
                "-of",
                "default=nw=1:nk=1",
            ])
            .arg(path),
    );
    let audio = |filter: &[&str], muxer| {
        output(
            Command::new("ffmpeg")
                .args(["-nostdin", "-v", "error", "-i"])
                .arg(path)
                .args(["-map", &format!("0:{stream}"), "-c:a", "copy"])
                .args(filter)
                .args(["-f", muxer, "pipe:1"]),
        )
    };
    let source = audio(&[], "dts");
    let filter = ["-bsf:a", "dca_core"];
    let reference = audio(&filter, "dts");
    let hashes = audio(&filter, "framehash");
    // framehash keeps the filter's packet boundaries even when adjacent payloads match.
    let mut core_sizes = std::str::from_utf8(&hashes)
        .unwrap()
        .lines()
        .filter(|row| !row.is_empty() && !row.starts_with('#'))
        .map(|row| {
            row.split(',')
                .nth(4)
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap()
        });
    let mut raw = source.as_slice();
    let mut expected = reference.as_slice();
    let budget = Budget::new(128 * 1024).unwrap();
    let sizes = std::str::from_utf8(&sizes).unwrap();
    assert_eq!(sizes.lines().count(), packets);
    for size in sizes.lines() {
        let (packet, rest) = raw.split_at(size.parse().unwrap());
        let identity = DtsInspectionIdentity {
            source: SourceId(1),
            generation: 0,
            offset: u64::try_from(source.len() - raw.len()).unwrap(),
        };
        raw = rest;
        let bytes = Bytes::copy_from_slice(&budget, packet).unwrap();
        let mut job = DtsHeaderInspector::new(&budget, identity, bytes, 32 * 1024).unwrap();
        let owned = loop {
            match job
                .step(&mut WorkBudget::new(128), &AtomicBool::new(false))
                .unwrap()
            {
                DtsInspectionStep::Ready(value) => break value,
                DtsInspectionStep::Yield => {}
                step => panic!("complete corpus packet produced {step:?}"),
            }
        };
        let packed = inspect_dts_header_bits(packet, packet.len() * 8).unwrap();
        assert!(owned.syntax_complete() && packed.syntax_complete());
        assert_eq!(owned.issues().bits(), packed.issues());
        assert_eq!(owned.views(), packed.views());
        for field in DtsCoreField::ALL {
            assert_eq!(owned.field(field), packed.field(field));
        }
        let length = usize::try_from(owned.views().declared_frame_bytes.unwrap()).unwrap();
        let core_size = core_sizes.next().expect("missing filtered packet");
        assert_eq!(length, core_size, "core length at byte {}", identity.offset);
        let (core, remaining) = expected.split_at(core_size);
        assert_eq!(packet.get(..length), Some(core));
        expected = remaining;
    }
    assert!(raw.is_empty() && expected.is_empty() && core_sizes.next().is_none());
}

#[test]
#[ignore = "requires the DTS packet corpus and the pinned ffmpeg and ffprobe on PATH"]
fn packet_core_prefixes_match_the_reference_filter() {
    let root = std::env::var_os("TRIERGON_DTS_CORPUS")
        .expect("set TRIERGON_DTS_CORPUS to the corpus directory");
    let root = Path::new(&root);
    for kind in ["mp4", "mkv", "mov"] {
        compare_file(&root.join(format!("dts.{kind}")), "a:1", 12_375);
    }
    for file in [
        "master_audio_7.1_24bit.dts",
        "remux.mkv",
        "remux.mp4",
        "remux.mov",
    ] {
        compare_file(&root.join("fate-dts").join(file), "a:0", 1_172);
    }
}
