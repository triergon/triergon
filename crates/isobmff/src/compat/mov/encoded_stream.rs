//! Admission of one encoded sample entry as FFmpeg's MOV demuxer reads it.
//! [`EncodedStreamJob`] checks an AVC, HEVC, AV1, VP9, AAC, AC-3, E-AC-3, DTS or Opus
//! entry and its configuration box and returns an [`EncodedStream`]: the stream the
//! demuxer reports, beside the source entry, its configuration and the track metadata
//! it was derived from.
use crate::{
    CursorStatus, Limits, Next,
    boxes::{BoxChildren, BoxView},
    compat::mov::{clocks::MediaClock, view::StreamView},
    config::{
        ConfigurationLimits, DtsCoreDescription, EsdsProfile, EsdsWalk, check_configuration_size,
        check_dts_sample_entry, check_mp4a_dts, dts_entry_children, dts_object_type,
        quicktime_aac_extensions,
    },
    fields,
    movie::{EntryLayout, SampleDescription, TrackHeader},
    table::SampleTable,
};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, InputCursor, InputToken, Result, SourceId, WorkBudget};
use triergon_framing::opus::{self, OpusHead};

/// Opt-in token for [`EncodedStreamJob::new`]: admit the first description of a track
/// in an ISO-branded file.
#[derive(Clone, Copy, Debug)]
pub struct EncodedStreamProfile;

/// The codec of an admitted sample entry, by entry format and configuration box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodedEntryKind {
    /// H.264: an `avc1` or `avc3` entry with `avcC`.
    Avc,
    /// H.265: an `hvc1` or `hev1` entry with `hvcC`; range playback also admits the
    /// Dolby Vision `dvh1` and `dvhe` entries.
    Hevc,
    /// AV1: an `av01` or `dav1` entry with `av1C`; range playback only.
    Av1,
    /// VP9: a `vp09` entry with `vpcC`; range playback only.
    Vp9,
    /// AAC: an `mp4a` entry whose `esds` holds an AAC LC, SBR or PS configuration.
    Aac,
    /// AC-3: an `ac-3` entry with `dac3`.
    Ac3,
    /// E-AC-3: an `ec-3` entry with `dec3`.
    Eac3,
    /// DTS core: a `dtsc`, `dtsh` or `dtsl` entry, or an `mp4a` entry whose `esds` names
    /// a DTS object type; range playback only.
    Dts,
    /// Opus: an `Opus` entry with `dOps`; range playback only.
    Opus,
}

impl EncodedEntryKind {
    pub(crate) const fn is_video(self) -> bool {
        matches!(self, Self::Avc | Self::Hevc | Self::Av1 | Self::Vp9)
    }

    pub(crate) fn require_regular_codec(self) -> Result<()> {
        if matches!(self, Self::Av1 | Self::Vp9 | Self::Dts | Self::Opus) {
            return Err(Error::Unsupported("encoded codec requires range playback"));
        }
        Ok(())
    }
}
/// An admitted encoded sample entry: the stream FFmpeg's MOV demuxer reports for it,
/// with the source entry, its configuration box and the track headers it was read from.
#[derive(Clone, Debug)]
pub struct EncodedStream {
    demux: StreamView,
    entry: SampleDescription,
    configuration: BoxView,
    pub(crate) dts: Option<DtsCoreDescription>,
    encoder: Option<Bytes>,
    kind: EncodedEntryKind,
    raw_headers: [BoxView; 3],
    receiver_source: Option<crate::compat::mov::encoded_metadata::EncodedSource>,
    wave_source: Option<crate::compat::mov::encoded_contexts::WaveObservation>,
}
impl EncodedStream {
    /// The `wave` box as [`EncodedStreamJob::new_description_with_contexts`] read it, with
    /// the faults it read past; `None` for other views and for entries without `wave`.
    pub fn wave_source(&self) -> Option<&crate::compat::mov::encoded_contexts::WaveObservation> {
        self.wave_source.as_ref()
    }

    /// The stream as FFmpeg's MOV demuxer reports it: codec, time base, extradata,
    /// language and handler name.
    pub fn demux(&self) -> &StreamView {
        &self.demux
    }
    /// The admitted `stsd` entry, unchanged.
    pub fn source_description(&self) -> &SampleDescription {
        &self.entry
    }
    /// The configuration box as stored, or the sample entry itself for a QuickTime
    /// `dtsc` entry, which carries its configuration in band.
    pub fn configuration_box(&self) -> &BoxView {
        &self.configuration
    }
    /// The `compressorname` of a visual entry, at most 31 ASCII bytes without its length
    /// byte; `None` for audio entries and for an empty name.
    pub fn encoder_name(&self) -> Option<&Bytes> {
        self.encoder.as_ref()
    }
    /// The codec; [`EncodedEntryKind::Dts`] also for an `mp4a` entry carrying DTS.
    pub fn kind(&self) -> EncodedEntryKind {
        self.kind
    }
    /// The `mdhd` language, `hdlr` name and QuickTime sound fields the stream was derived
    /// from; `None` for streams from [`EncodedStreamJob::new`] and
    /// [`EncodedStreamJob::new_description`].
    pub fn receiver_source(&self) -> Option<&crate::compat::mov::encoded_metadata::EncodedSource> {
        self.receiver_source.as_ref()
    }
    /// The track's `tkhd`, `mdhd` and `hdlr` boxes, in that order.
    pub fn raw_headers(&self) -> &[BoxView; 3] {
        &self.raw_headers
    }
}

// One nested, charged-storage-relative wave context; no recursive parser or queue.
#[derive(Debug)]
struct Wave {
    raw: BoxView,
    children: Option<BoxChildren>,
    format: Option<BoxView>,
    marker: Option<BoxView>,
    terminator: Option<BoxView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DescriptionView {
    Regular,
    Metadata,
    Contexts,
    RangePlayback,
}
impl DescriptionView {
    fn observes_contexts(self) -> bool {
        self == Self::Contexts
    }
}

/// Reads one sample entry of a track and its configuration as FFmpeg's MOV demuxer
/// admits them, and returns an [`EncodedStream`]. The constructor chooses which entries
/// and metadata forms are admitted. The job reads only metadata already held; poll it
/// with the input cursor the sample table was read from.
#[derive(Debug)]
pub struct EncodedStreamJob<'a> {
    header: &'a TrackHeader,
    entry: SampleDescription,
    children: Option<BoxChildren>,
    configuration: Option<BoxView>,
    esds: Option<EsdsWalk>,
    kind: EncodedEntryKind,
    config_kind: [u8; 4],
    limits: ConfigurationLimits,
    pool: Budget,
    source: SourceId,
    token: InputToken,
    result: Option<EncodedStream>,
    failed: Option<Error>,
    finished: bool,
    view: DescriptionView,
    major: Option<[u8; 4]>,
    sound_source: Option<crate::compat::mov::encoded_metadata::SoundV1Source>,
    requires_wave: bool,
    wave_fields: Option<Wave>,
    inspected_boxes: u32,
    wave_source: Option<crate::compat::mov::encoded_contexts::WaveObservation>,
    pending_esds: Option<(BoxView, EsdsWalk)>,
    descriptors_left: u32,
}
impl<'a> EncodedStreamJob<'a> {
    /// Admits the first description of a track whose file has an `isom`, `iso2`, `mp41`
    /// or `mp42` major brand: AVC, HEVC, AAC, AC-3 or E-AC-3 with ISO language and
    /// handler fields. `Error::Unsupported` for another brand, codec, handler or data
    /// reference, or an empty table; `Error::ForeignPool` when the table or the headers
    /// are not in `input`'s pool.
    pub fn new(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        major: Option<[u8; 4]>,
        limits: ConfigurationLimits,
        _profile: EncodedStreamProfile,
    ) -> Result<Self> {
        Self::new_description_inner(
            header,
            table,
            input,
            crate::compat::mov::encoded_metadata::EncodedSelection {
                major_brand: major,
                limits,
                description: 1,
            },
            DescriptionView::Regular,
        )
    }
    /// Like [`new`](Self::new), for the 1-based `description` of a track with several.
    /// `Error::Invalid` when the table has no such description.
    pub fn new_description(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        major: Option<[u8; 4]>,
        limits: ConfigurationLimits,
        description: u32,
        _profile: crate::compat::mov::encoded_configs::EncodedConfigProfile,
    ) -> Result<Self> {
        Self::new_description_inner(
            header,
            table,
            input,
            crate::compat::mov::encoded_metadata::EncodedSelection {
                major_brand: major,
                limits,
                description,
            },
            DescriptionView::Regular,
        )
    }
    /// Like [`new_description`](Self::new_description), and also admits a `qt  ` major
    /// brand with an AAC entry in a QuickTime sound description version 1 whose `esds`
    /// is in a `wave` box, a zero `mdhd` language (read as `eng`) and the Pascal name of
    /// a QuickTime `mhlr` handler. The stream carries the
    /// [`EncodedSource`](crate::compat::mov::encoded::EncodedSource) it was read from.
    pub fn new_description_with_metadata(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        selection: crate::compat::mov::encoded_metadata::EncodedSelection,
        _profile: crate::compat::mov::encoded_metadata::EncodedMetadataProfile,
    ) -> Result<Self> {
        Self::new_description_inner(header, table, input, selection, DescriptionView::Metadata)
    }
    /// Like [`new_description_with_metadata`](Self::new_description_with_metadata), but
    /// reads a `wave` box as FFmpeg's MOV demuxer does: a duplicate or conflicting `frma`,
    /// a duplicate `esds`, terminator data, a child after the terminator, an incomplete
    /// context or a compression ID of 0 is counted in the stream's
    /// [`wave_source`](EncodedStream::wave_source) instead of failing.
    pub fn new_description_with_contexts(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        selection: crate::compat::mov::encoded_metadata::EncodedSelection,
        _profile: crate::compat::mov::encoded_contexts::EncodedContextProfile,
    ) -> Result<Self> {
        Self::new_description_inner(header, table, input, selection, DescriptionView::Contexts)
    }
    /// Admits the entries range playback reads: those of
    /// [`new_description_with_metadata`](Self::new_description_with_metadata), Dolby
    /// Vision HEVC, AV1, VP9, Opus and DTS core entries, video entries of `qt  ` files,
    /// AAC in a QuickTime sound description version 2 and the unspecified QuickTime
    /// language (`und`). An `mp4a` entry whose `esds` names a DTS object type is read as
    /// DTS; an AAC `wave` box outside a QuickTime sound description is unsupported.
    pub fn new_range_playback(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        selection: crate::compat::mov::encoded_metadata::EncodedSelection,
    ) -> Result<Self> {
        Self::new_description_inner(
            header,
            table,
            input,
            selection,
            DescriptionView::RangePlayback,
        )
    }
    fn new_description_inner(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        selection: crate::compat::mov::encoded_metadata::EncodedSelection,
        view: DescriptionView,
    ) -> Result<Self> {
        let crate::compat::mov::encoded_metadata::EncodedSelection {
            major_brand: major,
            limits,
            description,
        } = selection;
        let metadata_receiver = view != DescriptionView::Regular;
        if !table.raw().bytes().belongs_to(input.budget())
            || [&header.raw_tkhd, &header.raw_mdhd, &header.raw_hdlr]
                .iter()
                .any(|x| !x.bytes().belongs_to(input.budget()))
        {
            return Err(Error::ForeignPool);
        }
        if !matches!(major,Some(x) if x==*b"isom"||x==*b"iso2"||x==*b"mp41"||x==*b"mp42"||(metadata_receiver&&x==*b"qt  "))
        {
            return Err(Error::Unsupported(
                "encoded track major brand other than isom, iso2, mp41 or mp42",
            ));
        }
        let at = usize::try_from(
            description
                .checked_sub(1)
                .ok_or(Error::Invalid("encoded sample description zero"))?,
        )
        .map_err(|_| Error::Overflow)?;
        let entry = table
            .descriptions()
            .get(at)
            .ok_or(Error::Invalid("encoded initial sample description absent"))?
            .clone();
        if table.sample_count() == 0 {
            return Err(Error::Unsupported(
                "encoded nonempty regular source required",
            ));
        }
        if entry.data_reference_index != 1 {
            return Err(Error::Unsupported("encoded external data reference"));
        }
        let (kind, config_kind, layout) = match (&header.handler, &entry.format) {
            (b"vide", b"avc1" | b"avc3") => (EncodedEntryKind::Avc, *b"avcC", EntryLayout::Visual),
            (b"vide", b"hvc1" | b"hev1") => (EncodedEntryKind::Hevc, *b"hvcC", EntryLayout::Visual),
            (b"vide", b"dvh1" | b"dvhe") if view == DescriptionView::RangePlayback => {
                (EncodedEntryKind::Hevc, *b"hvcC", EntryLayout::Visual)
            }
            (b"vide", b"av01" | b"dav1") if view == DescriptionView::RangePlayback => {
                (EncodedEntryKind::Av1, *b"av1C", EntryLayout::Visual)
            }
            (b"vide", b"vp09") if view == DescriptionView::RangePlayback => {
                (EncodedEntryKind::Vp9, *b"vpcC", EntryLayout::Visual)
            }
            (b"soun", b"mp4a") => (EncodedEntryKind::Aac, *b"esds", EntryLayout::AudioV0),
            (b"soun", b"ac-3") => (EncodedEntryKind::Ac3, *b"dac3", EntryLayout::AudioV0),
            (b"soun", b"ec-3") => (EncodedEntryKind::Eac3, *b"dec3", EntryLayout::AudioV0),
            (b"soun", b"Opus") if view == DescriptionView::RangePlayback => {
                (EncodedEntryKind::Opus, *b"dOps", EntryLayout::AudioV0)
            }
            (b"soun", b"dtsc" | b"dtsh" | b"dtsl") if view == DescriptionView::RangePlayback => {
                (EncodedEntryKind::Dts, *b"ddts", EntryLayout::AudioV0)
            }
            _ => return Err(Error::Unsupported("encoded catalogue namespace/handler")),
        };
        let child_limits = Limits {
            max_boxes: limits.max_child_boxes,
            ..Limits::default()
        };
        let quicktime = metadata_receiver && major == Some(*b"qt  ");
        let sound_version = if quicktime && kind == EncodedEntryKind::Aac {
            fields::u16_at(entry.raw().payload(), 8)?
        } else {
            0
        };
        let sound_source = if sound_version == 1 {
            Some(match view {
                DescriptionView::Contexts | DescriptionView::RangePlayback => {
                    crate::compat::mov::encoded_metadata::SoundV1Source::parse_contexts(&entry)?
                }
                _ => crate::compat::mov::encoded_metadata::SoundV1Source::parse(&entry)?,
            })
        } else {
            None
        };
        let wave_offset = match sound_version {
            1 => Some(44),
            2 if view == DescriptionView::RangePlayback => Some(quicktime_aac_extensions(&entry)?),
            _ => None,
        };
        let quicktime_visual =
            view == DescriptionView::RangePlayback && matches!(layout, EntryLayout::Visual);
        if quicktime && wave_offset.is_none() && !quicktime_visual && kind != EncodedEntryKind::Dts
        {
            return Err(Error::Unsupported(
                if view == DescriptionView::RangePlayback {
                    "QuickTime range playback sample entry"
                } else {
                    "encoded QuickTime entry other than sound-v1 AAC"
                },
            ));
        }
        let children = if kind == EncodedEntryKind::Dts {
            if major != Some(*b"qt  ") && fields::u16_at(entry.raw().payload(), 8)? != 0 {
                return Err(Error::Unsupported("DTS QuickTime sample entry brand"));
            }
            dts_entry_children(&entry, child_limits)?
        } else if let Some(offset) = wave_offset {
            entry.raw().children(offset, child_limits)?
        } else {
            entry.extensions(layout, child_limits)?
        };
        Ok(Self {
            header,
            entry,
            children: Some(children),
            configuration: None,
            esds: None,
            kind,
            config_kind,
            limits,
            pool: input.budget().clone(),
            source: input.source(),
            token: input.token(),
            result: None,
            failed: None,
            finished: false,
            view,
            major,
            sound_source,
            requires_wave: wave_offset.is_some(),
            wave_fields: None,
            inspected_boxes: 0,
            wave_source: None,
            pending_esds: None,
            descriptors_left: limits.max_descriptors,
        })
    }
    fn inspect_box(view: DescriptionView, inspected: &mut u32, maximum: u32) -> Result<()> {
        if view != DescriptionView::Regular {
            *inspected = inspected.checked_add(1).ok_or(Error::Overflow)?;
            if *inspected > maximum {
                return Err(Error::Limit {
                    requested: *inspected as usize,
                    maximum: maximum as usize,
                });
            }
        }
        Ok(())
    }
    fn accept_configuration(&mut self, raw: BoxView) -> Result<()> {
        check_configuration_size(&raw, self.limits)?;
        // A second `esds` in `wave` is one of the faults the context view reads past, the
        // later box becoming the configuration; every other view refuses it.
        if self.view.observes_contexts()
            && self.wave_fields.is_some()
            && self.kind == EncodedEntryKind::Aac
        {
            if let Some(report) = &mut self.wave_source {
                report.configuration_count = report
                    .configuration_count
                    .checked_add(1)
                    .ok_or(Error::Overflow)?;
                if self.configuration.is_some() {
                    report.fault(
                        Error::Unsupported("encoded duplicate configuration box"),
                        raw.clone(),
                        report.child_count,
                    )?;
                }
            }
            let at = usize::from(raw.header().header_size);
            let bytes = raw.bytes().slice(at..raw.bytes().len())?;
            self.pending_esds = Some((raw, EsdsWalk::new(bytes, self.descriptors_left)?));
        } else {
            if self.configuration.is_some() {
                return Err(Error::Unsupported("encoded duplicate configuration box"));
            }
            self.configuration = Some(raw);
        }
        Ok(())
    }
    fn check(
        &self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if !input.budget().same_pool(&self.pool) {
            return Err(Error::ForeignPool);
        }
        if input.source() != self.source {
            return Err(Error::SourceMismatch);
        }
        if input.token().generation() != self.token.generation() {
            return Err(Error::StaleGeneration {
                expected: self.token.generation(),
                received: input.token().generation(),
            });
        }
        input.read_into(&mut [], work, cancel)?;
        Ok(())
    }
    /// Reads the entry's child boxes and configuration within `work`; `Ready` when the
    /// stream can be taken. `Error::Unsupported` or `Error::Invalid` for an entry the
    /// view does not admit. Every later call returns the same error.
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<CursorStatus> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let answer = self.inner(input, work, cancel);
        if let Err(e) = answer {
            self.failed = Some(e);
            self.result = None;
        }
        answer
    }
    fn inner(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<CursorStatus> {
        self.check(input, work, cancel)?;
        if self.result.is_some() {
            return Ok(CursorStatus::Ready);
        }
        if self.finished {
            return Ok(CursorStatus::End);
        }
        loop {
            self.check(input, work, cancel)?;
            if let Some((_, walker)) = &mut self.pending_esds {
                let outcome = walker.poll(work);
                match outcome {
                    Err(Error::Limit { requested, .. }) => {
                        return Err(Error::Limit {
                            requested: (self.limits.max_descriptors - self.descriptors_left)
                                as usize
                                + requested,
                            maximum: self.limits.max_descriptors as usize,
                        });
                    }
                    Err(error) => return Err(error),
                    Ok(false) => return Ok(CursorStatus::Yield),
                    Ok(true) => {}
                }
                let (raw, walker) = self.pending_esds.take().ok_or(Error::Invalid(
                    "encoded context view configuration pending invariant",
                ))?;
                if walker.dsi.is_none() {
                    return Err(Error::Unsupported(
                        "encoded AAC decoder-specific config absent",
                    ));
                }
                self.descriptors_left = self
                    .descriptors_left
                    .checked_sub(walker.rows)
                    .ok_or(Error::Overflow)?;
                self.configuration = Some(raw);
                self.esds = Some(walker);
                continue;
            }
            if work.remaining() == 0 {
                return Ok(CursorStatus::Yield);
            }
            if let Some(w) = &mut self.wave_fields
                && let Some(children) = &mut w.children
            {
                match children.poll(work)? {
                    Next::Yield => return Ok(CursorStatus::Yield),
                    Next::Item(raw) => {
                        Self::inspect_box(
                            self.view,
                            &mut self.inspected_boxes,
                            self.limits.max_child_boxes,
                        )?;
                        let ordinal = if let Some(report) = &mut self.wave_source {
                            report.child_count =
                                report.child_count.checked_add(1).ok_or(Error::Overflow)?;
                            report.child_count
                        } else {
                            0
                        };
                        if w.terminator.is_some() {
                            let error = Error::Invalid("encoded wave child after terminator");
                            if let Some(report) = &mut self.wave_source {
                                report.fault(error, raw.clone(), ordinal)?;
                            } else {
                                return Err(error);
                            }
                        }
                        match raw.header().kind {
                            x if x == *b"frma" => {
                                if raw.payload().len() != 4 {
                                    return Err(Error::Truncated);
                                }
                                if w.format.is_some() {
                                    let e = Error::Invalid("encoded duplicate wave format");
                                    if let Some(r) = &mut self.wave_source {
                                        r.fault(e, raw.clone(), ordinal)?;
                                    } else {
                                        return Err(e);
                                    }
                                }
                                if raw.payload() != b"mp4a" {
                                    let e = Error::Unsupported(
                                        "encoded wave format conflicts with entry",
                                    );
                                    if let Some(r) = &mut self.wave_source {
                                        r.fault(e, raw.clone(), ordinal)?;
                                    } else {
                                        return Err(e);
                                    }
                                }
                                if let Some(r) = &mut self.wave_source {
                                    r.format_count =
                                        r.format_count.checked_add(1).ok_or(Error::Overflow)?;
                                    if r.format.is_none() {
                                        r.format = Some(raw.clone());
                                    }
                                }
                                if w.format.is_none() {
                                    w.format = Some(raw);
                                }
                            }
                            x if x == *b"mp4a" => {
                                if w.marker.is_some() {
                                    return Err(Error::Invalid(
                                        "encoded duplicate wave mp4a marker",
                                    ));
                                }
                                if raw.payload() != [0; 4] {
                                    return Err(Error::Unsupported(
                                        "encoded wave mp4a marker profile",
                                    ));
                                }
                                if let Some(r) = &mut self.wave_source {
                                    r.marker = Some(raw.clone());
                                }
                                w.marker = Some(raw);
                            }
                            [0, 0, 0, 0] => {
                                if !raw.payload().is_empty() {
                                    let e = Error::Invalid("encoded wave terminator data");
                                    if let Some(r) = &mut self.wave_source {
                                        r.fault(e, raw.clone(), ordinal)?;
                                    } else {
                                        return Err(e);
                                    }
                                }
                                if let Some(r) = &mut self.wave_source
                                    && r.terminator.is_none()
                                {
                                    r.terminator = Some(raw.clone());
                                }
                                w.terminator = Some(raw);
                            }
                            x if x == self.config_kind => self.accept_configuration(raw)?,
                            x if x == *b"wave" => {
                                return Err(Error::Unsupported("encoded nested wave box"));
                            }
                            _ => {}
                        }
                    }
                    Next::End => {
                        if w.terminator.is_none()
                            || (self.sound_source.is_some()
                                && (w.format.is_none() || w.marker.is_none()))
                        {
                            let error = Error::Unsupported("encoded incomplete mp4a wave context");
                            if let Some(report) = &mut self.wave_source {
                                report.fault(error, w.raw.clone(), report.child_count)?;
                            } else {
                                return Err(error);
                            }
                        }
                        w.children = None;
                    }
                }
                continue;
            }
            if let Some(children) = &mut self.children {
                match children.poll(work)? {
                    Next::Yield => return Ok(CursorStatus::Yield),
                    Next::Item(raw) => {
                        Self::inspect_box(
                            self.view,
                            &mut self.inspected_boxes,
                            self.limits.max_child_boxes,
                        )?;
                        if raw.header().kind == *b"wave" && self.requires_wave {
                            if self.wave_fields.is_some() {
                                return Err(Error::Invalid("encoded duplicate wave context"));
                            }
                            let nested = raw.children(
                                0,
                                Limits {
                                    max_boxes: self.limits.max_child_boxes,
                                    ..Limits::default()
                                },
                            )?;
                            if self.view.observes_contexts() {
                                let mut report =
                                    crate::compat::mov::encoded_contexts::WaveObservation::new(
                                        raw.clone(),
                                    );
                                if self
                                    .sound_source
                                    .as_ref()
                                    .is_some_and(|s| s.compression_id() != -2)
                                {
                                    report.fault(
                                        Error::Unsupported(
                                            "encoded sound-v1 frame-table compression ID",
                                        ),
                                        self.entry.raw().clone(),
                                        0,
                                    )?;
                                }
                                self.wave_source = Some(report);
                            }
                            self.wave_fields = Some(Wave {
                                raw,
                                children: Some(nested),
                                format: None,
                                marker: None,
                                terminator: None,
                            });
                        } else if raw.header().kind == *b"wave"
                            && self.view == DescriptionView::RangePlayback
                            && self.kind == EncodedEntryKind::Aac
                        {
                            return Err(Error::Unsupported(
                                "encoded AAC wave requires a QuickTime sound description",
                            ));
                        } else if raw.header().kind == self.config_kind {
                            if self.requires_wave {
                                return Err(Error::Unsupported(if self.sound_source.is_some() {
                                    "encoded sound-v1 config outside wave"
                                } else {
                                    "encoded QuickTime AAC configuration outside wave"
                                }));
                            }
                            self.accept_configuration(raw)?;
                        }
                    }
                    Next::End => {
                        self.children = None;
                        if self.kind == EncodedEntryKind::Dts {
                            continue;
                        }
                        let cfg = self.configuration.as_ref().ok_or(Error::Unsupported(
                            "encoded configuration path or absence not established",
                        ))?;
                        if self.kind == EncodedEntryKind::Aac && self.esds.is_none() {
                            let start = usize::from(cfg.header().header_size);
                            let bytes = cfg.bytes().slice(start..cfg.bytes().len())?;
                            let profile = if self.view == DescriptionView::RangePlayback
                                && !self.requires_wave
                            {
                                EsdsProfile::RangeAudio
                            } else {
                                EsdsProfile::Aac
                            };
                            self.esds = Some(EsdsWalk::with_profile(
                                bytes,
                                self.limits.max_descriptors,
                                profile,
                            )?);
                        }
                    }
                }
                continue;
            }
            if let Some(esds) = &mut self.esds
                && !esds.poll(work)?
            {
                return Ok(CursorStatus::Yield);
            }
            if !work.try_charge(1) {
                return Ok(CursorStatus::Yield);
            }
            self.result = Some(self.build_stream()?);
            return Ok(CursorStatus::Ready);
        }
    }
    /// The stream of a completed walk.
    fn build_stream(&mut self) -> Result<EncodedStream> {
        let cfg = match self.configuration.as_ref() {
            Some(cfg) => cfg,
            None if self.kind == EncodedEntryKind::Dts => self.entry.raw(),
            None => return Err(Error::Invalid("encoded config missing at publication")),
        };
        let dts = match self.kind {
            EncodedEntryKind::Dts => Some(check_dts_sample_entry(
                &self.entry,
                self.configuration.as_ref(),
            )?),
            EncodedEntryKind::Aac if self.view == DescriptionView::RangePlayback => {
                match self.esds.as_ref().and_then(|esds| esds.object_type) {
                    Some(oti) if dts_object_type(oti)? => {
                        self.kind = EncodedEntryKind::Dts;
                        Some(check_mp4a_dts(&self.entry)?)
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(dts) = dts {
            dts.media_clock(self.header.time_base()?)?;
        }
        let extradata = match self.kind {
            EncodedEntryKind::Aac => self
                .esds
                .as_ref()
                .and_then(|x| x.dsi.as_ref().map(|(_, bytes)| bytes.clone()))
                .ok_or(Error::Unsupported(
                    "encoded AAC decoder-specific config absent",
                ))?,
            EncodedEntryKind::Avc | EncodedEntryKind::Hevc | EncodedEntryKind::Av1 => {
                cfg.bytes()
                    .slice(usize::from(cfg.header().header_size)..cfg.bytes().len())?
            }
            EncodedEntryKind::Opus => opus_configuration(&self.pool, &self.entry, cfg)?,
            EncodedEntryKind::Ac3
            | EncodedEntryKind::Eac3
            | EncodedEntryKind::Vp9
            | EncodedEntryKind::Dts => cfg.bytes().slice(cfg.bytes().len()..cfg.bytes().len())?,
        };
        if self.kind == EncodedEntryKind::Aac {
            let first = *extradata
                .as_ref()
                .first()
                .ok_or(Error::Unsupported("encoded empty AAC configuration"))?;
            if !matches!(first >> 3, 2 | 5 | 29) {
                return Err(Error::Unsupported(
                    "encoded unselected MPEG-4 Audio object type",
                ));
            }
        }
        let (mut demux, encoder, receiver_source) = metadata(
            self.header,
            &self.entry,
            self.kind,
            extradata,
            self.view,
            self.major,
        )?;
        let receiver_source = receiver_source.map(|mut source| {
            source.sound.clone_from(&self.sound_source);
            source.wave = self.wave_fields.as_ref().map(|w| w.raw.clone());
            source
        });
        if self.view != DescriptionView::Regular && self.major == Some(*b"qt  ") {
            let vendor = fields::fourcc(self.entry.raw().payload(), 12)?;
            if self.requires_wave && vendor != [0; 4] {
                return Err(Error::Unsupported(
                    "encoded QuickTime nonzero vendor profile",
                ));
            }
            demux.vendor_id = Some(vendor);
        }
        Ok(EncodedStream {
            demux,
            entry: self.entry.clone(),
            configuration: cfg.clone(),
            dts,
            encoder,
            kind: self.kind,
            receiver_source,
            wave_source: self.wave_source.take(),
            raw_headers: [
                self.header.raw_tkhd.clone(),
                self.header.raw_mdhd.clone(),
                self.header.raw_hdlr.clone(),
            ],
        })
    }
    /// Takes the stream once [`poll`](Self::poll) returned `Ready`; `Error::Invalid`
    /// before that. Later polls return `End`.
    pub fn take(&mut self, input: &mut InputCursor, cancel: &AtomicBool) -> Result<EncodedStream> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let answer = (|| {
            self.check(input, &mut WorkBudget::new(0), cancel)?;
            self.result
                .take()
                .ok_or(Error::Invalid("encoded catalogue not ready"))
        })();
        match answer {
            Ok(value) => {
                self.finished = true;
                Ok(value)
            }
            Err(error) => {
                self.failed = Some(error);
                self.result = None;
                Err(error)
            }
        }
    }
}

type MetadataParts = (
    StreamView,
    Option<Bytes>,
    Option<crate::compat::mov::encoded_metadata::EncodedSource>,
);
fn metadata(
    header: &TrackHeader,
    entry: &SampleDescription,
    kind: EncodedEntryKind,
    extradata: Bytes,
    view: DescriptionView,
    major: Option<[u8; 4]>,
) -> Result<MetadataParts> {
    let parsed = TrackHeader::parse(&header.raw_tkhd, &header.raw_mdhd, &header.raw_hdlr)?;
    if (
        parsed.track_id,
        parsed.timescale,
        parsed.duration,
        parsed.handler,
        parsed.enabled,
    ) != (
        header.track_id,
        header.timescale,
        header.duration,
        header.handler,
        header.enabled,
    ) {
        return Err(Error::Invalid(
            "encoded public header differs from raw source",
        ));
    }
    let clock = MediaClock::from_source(header.time_base()?)?;
    let metadata_source = (view != DescriptionView::Regular)
        .then(|| -> Result<_> {
            Ok(crate::compat::mov::encoded_metadata::EncodedSource {
                language: crate::compat::mov::encoded_metadata::language(
                    header,
                    view == DescriptionView::RangePlayback && major == Some(*b"qt  "),
                )?,
                handler: crate::compat::mov::encoded_metadata::handler(header, major)?,
                sound: None,
                wave: None,
            })
        })
        .transpose()?;
    let b = header.raw_mdhd.payload();
    let at = match fields::full(b)?.0 {
        0 => 20,
        1 => 32,
        _ => return Err(Error::Unsupported("encoded media header version")),
    };
    let packed = fields::u16_at(b, at)?;
    let mut language = [0; 3];
    for (i, v) in language.iter_mut().enumerate() {
        let c = ((packed >> (10 - 5 * i)) & 31) as u8;
        if metadata_source.is_none() && (!(1..=26).contains(&c) || packed & 0x8000 != 0) {
            return Err(Error::Unsupported(
                "encoded track language other than a packed ISO code",
            ));
        }
        *v = c + 0x60;
    }
    if let Some(source) = &metadata_source {
        language = source.language.demux;
    }
    let h = header.raw_hdlr.payload();
    if metadata_source.is_none()
        && (fields::u32_at(h, 4)? != 0 || fields::slice(h, 12, 12)? != [0; 12])
    {
        return Err(Error::Unsupported(
            "encoded handler with QuickTime component fields",
        ));
    }
    let tail = h.get(24..).ok_or(Error::Truncated)?;
    if tail.len() > 256 {
        return Err(Error::Limit {
            requested: tail.len(),
            maximum: 256,
        });
    }
    let handler_name = if let Some(source) = &metadata_source {
        if let Some(name) = &source.handler.demux {
            name.clone()
        } else {
            header
                .raw_hdlr
                .bytes()
                .slice(header.raw_hdlr.bytes().len()..header.raw_hdlr.bytes().len())?
        }
    } else {
        let n = tail
            .iter()
            .position(|b| *b == 0)
            .ok_or(Error::Unsupported("encoded unterminated handler"))?;
        if !tail[..n].iter().all(u8::is_ascii) {
            return Err(Error::Unsupported("encoded handler name not ASCII"));
        }
        let start = usize::from(header.raw_hdlr.header().header_size) + 24;
        header.raw_hdlr.bytes().slice(start..start + n)?
    };
    let video = kind.is_video();
    let encoder = if video {
        let body = entry.raw().payload();
        let n = usize::from(*body.get(42).ok_or(Error::Truncated)?);
        if n > 31 {
            return Err(Error::Unsupported("encoded compressor-name width"));
        }
        if n == 0 {
            None
        } else {
            let at = usize::from(entry.raw().header().header_size) + 43;
            let name = entry.raw().bytes().slice(at..at + n)?;
            if !name.as_ref().iter().all(|b| b.is_ascii() && *b != 0) {
                return Err(Error::Unsupported("encoded compressor-name encoding"));
            }
            Some(name)
        }
    } else {
        None
    };
    let codec = match kind {
        EncodedEntryKind::Avc => "h264",
        EncodedEntryKind::Hevc => "hevc",
        EncodedEntryKind::Av1 => "av1",
        EncodedEntryKind::Vp9 => "vp9",
        EncodedEntryKind::Aac => "aac",
        EncodedEntryKind::Ac3 => "ac3",
        EncodedEntryKind::Eac3 => "eac3",
        EncodedEntryKind::Dts => "dts",
        EncodedEntryKind::Opus => "opus",
    };
    Ok((
        StreamView {
            track_id: header.track_id,
            codec,
            media_type: u8::from(!video),
            time_base: clock.demux_base,
            clock,
            extradata,
            language,
            handler_name,
            vendor_id: None,
        },
        encoder,
        metadata_source,
    ))
}

fn opus_configuration(pool: &Budget, entry: &SampleDescription, config: &BoxView) -> Result<Bytes> {
    // Opus in ISO BMFF, sections 4.3.1 and 4.3.2.
    let head = OpusHead::from_dops(config.payload())?;
    let sample_entry = entry.raw().payload();
    let coded_channels = u16::from(head.streams) + u16::from(head.coupled_streams);
    if fields::u16_at(sample_entry, 16)? != coded_channels
        || fields::u16_at(sample_entry, 18)? != 16
        || fields::u32_at(sample_entry, 24)? != opus::SAMPLE_RATE << 16
    {
        return Err(Error::Invalid("Opus sample entry audio fields"));
    }
    let mut packet = [0; OpusHead::MAX_BYTES];
    let len = head.write(&mut packet);
    if config.payload().len() != len - b"OpusHead".len() {
        return Err(Error::Invalid("Opus specific box trailing bytes"));
    }
    Bytes::copy_from_slice(pool, &packet[..len])
}
