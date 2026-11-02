#![forbid(unsafe_code)]
//! Evaluation adapter only. It never decodes movie frames or feeds a hardware player.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};
use symphonia::core::{
    formats::{FormatOptions, FormatReader, SeekMode, SeekTo, probe::Hint},
    io::{MediaSource, MediaSourceStream},
    meta::MetadataOptions,
    units::Time,
};

struct Source {
    file: File,
    seekable: bool,
}
impl Read for Source {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.file.read(b)
    }
}
impl Seek for Source {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        if self.seekable {
            self.file.seek(p)
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "sequential source",
            ))
        }
    }
}
impl MediaSource for Source {
    fn is_seekable(&self) -> bool {
        self.seekable
    }
    fn byte_len(&self) -> Option<u64> {
        if self.seekable {
            self.file.metadata().ok().map(|m| m.len())
        } else {
            None
        }
    }
}
fn sha(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}
fn open(path: &str, seekable: bool) -> Result<Box<dyn FormatReader>, Box<dyn std::error::Error>> {
    if std::fs::metadata(path)?.len() > 8 * 1024 * 1024 {
        return Err("fixture too large".into());
    }
    let mss = MediaSourceStream::new(
        Box::new(Source {
            file: File::open(path)?,
            seekable,
        }),
        Default::default(),
    );
    let mut hint = Hint::new();
    if let Some(ext) = Path::new(path).extension().and_then(|x| x.to_str()) {
        hint.with_extension(ext);
    }
    Ok(symphonia::default::get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    )?)
}
fn symphonia(path: &str, seekable: bool) -> Result<Value, Box<dyn std::error::Error>> {
    let mut f = open(path, seekable)?;
    let tracks:Vec<Value>=f.tracks().iter().map(|t|json!({"id":t.id,"codec":format!("{:?}",t.codec_params),"time_base":format!("{:?}",t.time_base)})).collect();
    let bases: std::collections::HashMap<_, _> = f
        .tracks()
        .iter()
        .filter_map(|t| t.time_base.map(|b| (t.id, b)))
        .collect();
    let mut packets = Vec::new();
    let termination = loop {
        let p = match f.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break json!({"clean_eof":true}),
            Err(e) => break json!({"error":e.to_string()}),
        };
        if packets.len() >= 10_000 {
            return Err("packet budget exceeded".into());
        }
        let ns = bases
            .get(&p.track_id)
            .and_then(|b| b.calc_time(p.pts))
            .map(|t| t.as_nanos().to_string());
        packets.push(json!({"track":p.track_id,"pts_ns":ns,"pts":p.pts.get(),"dts":p.dts.get(),"size":p.data.len(),"sha256":sha(&p.data)}));
    };
    let mut seeks = Vec::new();
    for ns in [1_500_000_000, 250_000_000] {
        let outcome = f.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: Time::from_nanos(ns),
                track_id: Some(f.tracks()[0].id),
            },
        );
        let r = match outcome {
            Ok(x) => {
                json!({"requested_ns":ns,"landed":format!("{x:?}"),"next":match f.next_packet(){Ok(Some(p))=>json!({"track":p.track_id,"pts":p.pts.get(),"sha256":sha(&p.data)}),Ok(None)=>Value::Null,Err(e)=>json!({"error":e.to_string()})}})
            }
            Err(e) => json!({"requested_ns":ns,"error":e.to_string()}),
        };
        seeks.push(r);
    }
    let mut fresh_seeks = Vec::new();
    for ns in [1_500_000_000, 250_000_000] {
        let mut fresh = open(path, seekable)?;
        let first = fresh.tracks()[0].id;
        let outcome = fresh.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: Time::from_nanos(ns),
                track_id: Some(first),
            },
        );
        fresh_seeks.push(match outcome {
            Ok(x)=>json!({"requested_ns":ns,"landed":format!("{x:?}"),"next":match fresh.next_packet(){Ok(Some(p))=>json!({"track":p.track_id,"pts":p.pts.get()}),Ok(None)=>Value::Null,Err(e)=>json!({"error":e.to_string()})}}),
            Err(e)=>json!({"requested_ns":ns,"error":e.to_string()})
        });
    }
    Ok(
        json!({"tracks":tracks,"packets":packets,"seeks_after_eof":seeks,"fresh_seeks":fresh_seeks,"termination":termination}),
    )
}
fn standalone(path: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let mut f = matroska_demuxer::MatroskaFile::open(File::open(path)?)?;
    let scale = f.info().timestamp_scale().get();
    let tracks: Vec<_> = f
        .tracks()
        .iter()
        .map(|t| json!({"id":t.track_number().get(),"codec":t.codec_id()}))
        .collect();
    let mut frame = matroska_demuxer::Frame::default();
    let mut packets = Vec::new();
    while f.next_frame(&mut frame)? {
        if packets.len() >= 10_000 {
            return Err("packet budget exceeded".into());
        }
        packets.push(json!({"track":frame.track,"pts_ns":(u128::from(frame.timestamp)*u128::from(scale)).to_string(),"size":frame.data.len(),"key":frame.is_keyframe,"sha256":sha(&frame.data)}));
    }
    let mut seeks = Vec::new();
    for ns in [1_500_000_000u64, 250_000_000] {
        let r = match f.seek(ns / scale) {
            Ok(()) => {
                let got = f.next_frame(&mut frame)?;
                json!({"requested_ns":ns,"next":if got{json!({"track":frame.track,"pts_ns":(u128::from(frame.timestamp)*u128::from(scale)).to_string(),"key":frame.is_keyframe,"sha256":sha(&frame.data)})}else{Value::Null}})
            }
            Err(e) => json!({"requested_ns":ns,"error":e.to_string()}),
        };
        seeks.push(r);
    }
    Ok(json!({"tracks":tracks,"packets":packets,"seeks":seeks}))
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: safe-media-compat-spike <symphonia|sequential|matroska> <fixture>");
        std::process::exit(2);
    }
    let out = match args[1].as_str() {
        "symphonia" => symphonia(&args[2], true),
        "sequential" => symphonia(&args[2], false),
        "matroska" => standalone(&args[2]),
        _ => Err("unknown candidate".into()),
    };
    match out {
        Ok(v) => println!("{v}"),
        Err(e) => {
            println!("{}", json!({"error":e.to_string()}));
            std::process::exit(1);
        }
    }
}
