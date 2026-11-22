use super::{Output, Source, extent};
use std::sync::atomic::AtomicBool;
use triergon_core::{Error, InputCursor, Result, WorkBudget};
use triergon_isobmff::{
    CursorStatus,
    compat::mov::{
        edits::{EncodedEditContext, EncodedEditJob, EncodedEditLimits, EncodedEditProfile},
        encoded::{
            EncodedCreationSource, EncodedMetadataProfile, EncodedRecoveryProfile,
            EncodedSelection, EncodedStreamJob,
        },
        packets::{DemuxTrack, PacketCursor, PacketLimits},
    },
    config::ConfigurationLimits,
    open::Movie,
};

#[derive(Clone, Copy)]
pub(super) enum Profile {
    Metadata,
    Recovery,
    Dependency,
}

impl Profile {
    pub(super) fn parse(name: &str) -> std::result::Result<Self, String> {
        match name {
            "metadata" => Ok(Self::Metadata),
            "recovery" => Ok(Self::Recovery),
            "dependency" => Ok(Self::Dependency),
            _ => Err("invalid encoded profile".into()),
        }
    }
}

pub(super) fn run(
    source: Source<'_>,
    input: &mut InputCursor,
    cancel: &AtomicBool,
    movie: &Movie,
    profile: Profile,
    out: &mut Output<'_, '_>,
) -> Result<()> {
    let q = source.quantum;
    let movie_base = movie.header().time_base()?;
    let mut tracks = Vec::new();
    let mut configs = Vec::new();
    let mut plans = Vec::new();
    for track in movie.tracks() {
        let selection = EncodedSelection {
            major_brand: movie.major_brand(),
            description: 1,
            limits: ConfigurationLimits::default(),
        };
        let mut job = EncodedStreamJob::new_description_with_metadata(
            track.header(),
            track.table(),
            input,
            selection,
            EncodedMetadataProfile,
        )?;
        while job.poll(input, &mut WorkBudget::new(q), cancel)? == CursorStatus::Yield {}
        let config = job.take(input, cancel)?;
        let base = track.header().time_base()?;
        let plan = if let Some(edits) = track.edits() {
            let context = EncodedEditContext {
                table: track.table(),
                edits,
                stream: &config,
                media_base: base,
                movie_base,
            };
            let configs = std::slice::from_ref(&config);
            let limits = EncodedEditLimits::default();
            let mut job = match profile {
                Profile::Metadata => {
                    EncodedEditJob::new(context, input, limits, EncodedEditProfile)?
                }
                Profile::Recovery => EncodedEditJob::new_with_recovery_groups(
                    context,
                    configs,
                    input,
                    limits,
                    EncodedRecoveryProfile,
                )?,
                Profile::Dependency => EncodedEditJob::new_with_dependency_table(
                    context,
                    configs,
                    input,
                    limits,
                    EncodedMetadataProfile,
                )?,
            };
            while job.poll(input, &mut WorkBudget::new(q), cancel)? == CursorStatus::Yield {}
            Some(job.take(input, cancel)?)
        } else {
            None
        };
        configs.push(config);
        plans.push(plan);
        tracks.push(DemuxTrack {
            track_id: track.header().track_id,
            table: track.table(),
            fragments: track.fragments(),
            media_base: base,
            edits: track.edits().map(|edits| (edits, movie_base)),
        });
    }
    let mut cursor = PacketCursor::new_encoded_regular(
        movie.pool(),
        &tracks,
        movie.media(),
        PacketLimits::default(),
        &plans,
        EncodedEditProfile,
    )?;
    out.row(2, &[tracks.len() as u64], &[])?;
    for (index, (track, config)) in movie.tracks().iter().zip(&configs).enumerate() {
        let stream = config.demux();
        out.row(
            2,
            &[
                index as u64,
                u64::from(stream.track_id),
                u64::from(stream.media_type),
                stream.time_base.numerator(),
                stream.time_base.denominator(),
                0,
            ],
            &[],
        )?;
        out.string(1, stream.codec.as_bytes())?;
        out.string(2, stream.extradata.as_ref())?;
        out.string(
            3,
            if stream.language == [0; 3] {
                &[]
            } else {
                &stream.language
            },
        )?;
        out.string(4, stream.handler_name.as_ref())?;
        let vendor = stream.vendor_id.map(vendor_text);
        out.string(5, vendor.as_ref().map_or(&[], String::as_bytes))?;
        let creation = EncodedCreationSource::from_source(track.header(), EncodedMetadataProfile)?;
        out.string(6, creation.demux().map_or(&[], |bytes| &bytes[..]))?;
        // This profile publishes no stream-level skip-samples data.
        out.string(7, &[])?;
    }
    loop {
        match cursor.poll(&mut WorkBudget::new(q), cancel)? {
            CursorStatus::Yield => (),
            CursorStatus::End => return out.terminal(None),
            CursorStatus::Ready => {
                let skip = cursor.ready_encoded_skip()?;
                let packet = cursor.take_occurrence()?;
                let index = tracks
                    .iter()
                    .position(|track| track.track_id == packet.track_id)
                    .ok_or(Error::Invalid("missing encoded track"))?;
                let range = packet.demux.source_range.clone();
                let media = extent(movie.media(), &range)?;
                let payload = source.sample(
                    input,
                    triergon_isobmff::Sample {
                        range,
                        ..packet.source
                    },
                    media,
                    cancel,
                )?;
                out.view_packet(index, &packet.demux, payload.as_ref())?;
                out.string(8, skip.as_ref().map_or(&[], |skip| &skip[..]))?;
            }
        }
    }
}

fn vendor_text(vendor: [u8; 4]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for byte in vendor {
        if (b' '..=b'~').contains(&byte) {
            out.push(char::from(byte));
        } else {
            write!(out, "[{byte}]").expect("String write");
        }
    }
    out
}
