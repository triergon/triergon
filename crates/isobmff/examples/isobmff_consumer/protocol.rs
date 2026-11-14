//! Matched research consumer fields. Checksums check work, not correctness:
//! complete ordered trace equality and independent source audits are required.
use std::fmt::Write as _;
use triergon_core::{Budget, BudgetVec, Bytes, Error};
pub use triergon_harness_protocol::digest::{append_words, fold, fold_framed, signed};
use triergon_harness_protocol::{push_hex, push_quoted};
use triergon_isobmff::{
    TrackHeader,
    compat::mov::{
        encoded::{EncodedCreationSource, EncodedMetadataProfile, EncodedStream, LanguageCause},
        packets::{PacketView, StreamView},
    },
};

pub const MAX_PACKETS: u64 = 1_000_000;
pub const MAX_DELIVERED_BYTES: u64 = 256 * 1024 * 1024;
pub const EOF: i64 = -541478725;
pub const DOMAIN: u64 = 0x4d4f5633;
pub const CORRUPT_PACKET: u32 = 2;
const SKIP_SAMPLES: u32 = 11;

pub fn raw_box(out: &mut String, raw: &triergon_isobmff::BoxView) {
    write!(out, "{{\"offset\":{},\"box_hex\":\"", raw.header().start).expect("String write");
    push_hex(out, raw.bytes().as_ref());
    out.push_str("\"}");
}
fn time(out: &mut String, t: &triergon_core::Time) {
    write!(out, "[{},{}]", t.numerator(), t.denominator()).expect("String write");
}
fn edit(out: &mut String, e: &triergon_isobmff::EditEntry) {
    write!(
        out,
        "[{}, {}, {}, {}]",
        e.segment_duration, e.media_time, e.rate_integer, e.rate_fraction
    )
    .expect("String write");
}
pub fn source_sample(out: &mut String, s: &triergon_isobmff::Sample) {
    write!(
        out,
        "{{\"ordinal\":{},\"description\":{},\"range\":[{},{}],\"duration\":{},\"dts\":",
        s.ordinal, s.description_index, s.range.start, s.range.end, s.duration_ticks
    )
    .expect("String write");
    match s.dts_ticks {
        Some(v) => write!(out, "{v}").expect("String write"),
        None => out.push_str("null"),
    }
    out.push_str(",\"cts\":");
    match s.cts_ticks {
        Some(v) => write!(out, "{v}").expect("String write"),
        None => out.push_str("null"),
    }
    match s.composition_offset {
        Some(v) => write!(
            out,
            ",\"offset_version\":{},\"offset_bits\":{}",
            v.version(),
            v.raw_bits()
        )
        .expect("String write"),
        None => out.push_str(",\"offset_version\":null,\"offset_bits\":null"),
    }
    write!(
        out,
        ",\"access\":\"{:?}\",\"partial_sync\":{},\"dependency\":",
        s.access, s.partial_sync
    )
    .expect("String write");
    match s.dependency {
        Some(v) => write!(out, "{v}").expect("String write"),
        None => out.push_str("null"),
    }
    out.push_str(",\"fragment_flags\":");
    match s.fragment_flags {
        Some(v) => write!(out, "{v}").expect("String write"),
        None => out.push_str("null"),
    }
    out.push('}');
}
pub fn source_landings(out: &mut String, r: &triergon_isobmff::MovieSeekResult<'_>) {
    use triergon_isobmff::RecoveryDefinitionOrigin;
    out.push_str(",\"point_receipt\":");
    if let Some(p) = &r.point {
        out.push_str("{\"requested_movie\":");
        time(out, &p.requested_movie_time);
        out.push_str(",\"source\":");
        time(out, &p.source_time);
        out.push_str(",\"movie_start\":");
        time(out, &p.movie_start);
        out.push_str(",\"movie_end\":");
        if let Some(t) = &p.movie_end {
            time(out, t);
        } else {
            out.push_str("null");
        }
        out.push_str(",\"edit_ordinal\":");
        match p.edit_ordinal {
            Some(v) => write!(out, "{v}").expect("String write"),
            None => out.push_str("null"),
        }
        out.push_str(",\"raw_edit_fields\":");
        if let Some(e) = &p.edit_entry {
            edit(out, e);
        } else {
            out.push_str("null");
        }
        out.push('}');
    } else {
        out.push_str("null");
    }
    out.push_str(",\"gap_receipt\":");
    if let Some(g) = &r.gap {
        write!(
            out,
            "{{\"edit_ordinal\":{},\"requested_movie\":",
            g.edit_ordinal
        )
        .expect("String write");
        time(out, &g.requested_movie_time);
        out.push_str(",\"movie_start\":");
        time(out, &g.movie_range.start);
        out.push_str(",\"movie_end\":");
        time(out, &g.movie_range.end);
        out.push_str(",\"raw_edit_fields\":");
        edit(out, &g.entry);
        out.push('}');
    } else {
        out.push_str("null");
    }
    out.push_str(",\"recovery_receipt\":");
    if let Some(s) = &r.recovery {
        write!(
            out,
            "{{\"kind\":\"{:?}\",\"mapping_origin\":\"{:?}\",\"raw_mapping_index\":",
            s.kind, s.mapping_origin
        )
        .expect("String write");
        match s.raw_mapping_index {
            Some(v) => write!(out, "{v}").expect("String write"),
            None => out.push_str("null"),
        }
        out.push_str(",\"definition_origin\":");
        match s.definition_origin {
            Some(RecoveryDefinitionOrigin::Track) => out.push_str("{\"kind\":\"track\"}"),
            Some(RecoveryDefinitionOrigin::Fragment(v)) => {
                write!(out, "{{\"kind\":\"fragment\",\"ordinal\":{v}}}").expect("String write");
            }
            None => out.push_str("null"),
        }
        out.push_str(",\"description\":");
        if let Some(d) = &s.description {
            write!(
                out,
                "{{\"index\":{},\"distance\":{},\"raw_bits\":{},\"source_range\":[{},{}]}}",
                d.index, d.distance, d.raw_bits, d.source_range.start, d.source_range.end
            )
            .expect("String write");
        } else {
            out.push_str("null");
        }
        out.push('}');
    } else {
        out.push_str("null");
    }
    out.push_str(",\"plan_receipt\":");
    if let Some(plan) = &r.plan {
        let p = plan.landing();
        write!(out,"{{\"track_id\":{},\"requested_ordinal\":{},\"requested_preroll\":{},\"available_preroll\":{},\"missing_preroll\":{},\"validated_samples\":{},\"target\":",
            p.track_id,p.requested_ordinal,p.requested_preroll_samples,p.available_preroll_samples,p.missing_preroll_samples,p.validated_samples).expect("String write");
        source_sample(out, &p.target);
        out.push_str(",\"sync\":");
        source_sample(out, &p.sync_sample);
        out.push_str(",\"decode_start\":");
        source_sample(out, &p.decode_start);
        out.push('}');
    } else {
        out.push_str("null");
    }
}
pub fn nullable(out: &mut String, value: i64) {
    if value == i64::MIN {
        out.push_str("null");
    } else {
        write!(out, "{value}").expect("String write");
    }
}

#[derive(Clone, Debug)]
pub struct Packet {
    pub stream: usize,
    pub pid: i64,
    pub pts: i64,
    pub dts: i64,
    pub duration: i64,
    pub position: i64,
    pub flags: u32,
    pub payload: Bytes,
    pub skip_samples: Option<[u8; 10]>,
}
impl Packet {
    pub fn from_view(
        stream: usize,
        pid: u32,
        demux: &PacketView,
        payload: Bytes,
    ) -> Result<Self, Error> {
        let pid = i32::try_from(pid)
            .map_err(|_| Error::Unsupported("native signed stream ID projection"))?;
        Ok(Self {
            stream,
            pid: i64::from(pid),
            pts: demux.pts_field.signed_ticks(),
            dts: demux.dts_field.signed_ticks(),
            duration: demux.duration,
            position: i64::try_from(demux.source_range.start)
                .map_err(|_| Error::Unsupported("native signed position projection"))?,
            flags: demux.flags,
            payload,
            skip_samples: None,
        })
    }
    pub fn consume(&self, h: &mut u64) -> Result<(), Error> {
        fold(h, 0x50);
        fold(h, u64::try_from(self.stream).map_err(|_| Error::Overflow)?);
        fold(h, signed(self.pid));
        for clock in [self.pts, self.dts] {
            fold(h, u64::from(clock != i64::MIN));
            fold(h, signed(clock));
        }
        for value in [self.duration, self.position] {
            fold(h, signed(value));
        }
        fold(h, u64::from(self.flags));
        fold_framed(h, self.payload.as_ref());
        fold(h, u64::from(self.skip_samples.is_some()));
        if let Some(skip) = &self.skip_samples {
            fold(h, u64::from(SKIP_SAMPLES));
            fold_framed(h, skip);
        }
        Ok(())
    }
    pub fn trace(&self, out: &mut String) {
        write!(
            out,
            "{{\"stream\":{},\"pid\":{},\"pts_raw\":{},\"dts_raw\":{},\"pts\":",
            self.stream, self.pid, self.pts, self.dts
        )
        .expect("String write");
        nullable(out, self.pts);
        out.push_str(",\"dts\":");
        nullable(out, self.dts);
        write!(
            out,
            ",\"duration\":{},\"position\":{},\"flags\":{},\"payload\":\"",
            self.duration, self.position, self.flags
        )
        .expect("String write");
        push_hex(out, self.payload.as_ref());
        out.push_str("\",\"side_data\":[");
        if let Some(skip) = &self.skip_samples {
            write!(out, "{{\"type\":{SKIP_SAMPLES},\"data\":\"").expect("String write");
            push_hex(out, skip);
            out.push_str("\"}");
        }
        out.push_str("]}");
    }
}

pub struct CatalogueStream {
    view: StreamView,
    creation_time: Option<[u8; 27]>,
    language_present: bool,
    handler_present: bool,
    encoder: Option<Bytes>,
}

impl From<StreamView> for CatalogueStream {
    fn from(view: StreamView) -> Self {
        Self {
            view,
            creation_time: None,
            language_present: true,
            handler_present: true,
            encoder: None,
        }
    }
}

impl CatalogueStream {
    pub fn encoded(stream: &EncodedStream, header: &TrackHeader) -> Result<Self, Error> {
        let source = stream
            .receiver_source()
            .ok_or(Error::Invalid("missing encoded catalogue metadata"))?;
        let creation = EncodedCreationSource::from_source(header, EncodedMetadataProfile)?;
        Ok(Self {
            view: stream.demux().clone(),
            creation_time: creation.demux().copied(),
            language_present: source.language.cause != LanguageCause::QuickTimeUnspecified,
            handler_present: source.handler.demux.is_some(),
            encoder: stream.encoder_name().cloned(),
        })
    }

    fn tags<'a>(&'a self, vendor: &'a mut [u8; 20]) -> [Option<(&'static [u8], &'a [u8])>; 5] {
        [
            self.creation_time
                .as_ref()
                .map(|time| (b"creation_time".as_slice(), time.as_slice())),
            self.language_present
                .then_some((b"language", self.view.language.as_slice())),
            self.handler_present
                .then_some((b"handler_name", self.view.handler_name.as_ref())),
            self.view
                .vendor_id
                .map(|id| (b"vendor_id".as_slice(), vendor_text(id, vendor))),
            self.encoder
                .as_ref()
                .map(|name| (b"encoder".as_slice(), name.as_ref())),
        ]
    }
}

// The MOV metadata API prints letters, digits, spaces, dots, dashes and
// underscores literally. Other vendor bytes are represented in decimal brackets.
fn vendor_text(id: [u8; 4], out: &mut [u8; 20]) -> &[u8] {
    if id == [0; 4] {
        return b"[0][0][0][0]";
    }
    let mut at = 0;
    for byte in id {
        if byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'.' | b'-' | b'_') {
            out[at] = byte;
            at += 1;
        } else {
            out[at] = b'[';
            at += 1;
            if byte >= 100 {
                out[at] = b'0' + byte / 100;
                at += 1;
            }
            if byte >= 10 {
                out[at] = b'0' + byte / 10 % 10;
                at += 1;
            }
            out[at] = b'0' + byte % 10;
            out[at + 1] = b']';
            at += 2;
        }
    }
    &out[..at]
}

pub fn consume_stream(h: &mut u64, index: usize, catalogue: &CatalogueStream) -> Result<(), Error> {
    let stream = &catalogue.view;
    fold(h, 0x53);
    fold(h, u64::try_from(index).map_err(|_| Error::Overflow)?);
    fold(
        h,
        signed(i64::from(i32::try_from(stream.track_id).map_err(|_| {
            Error::Unsupported("native signed stream ID projection")
        })?)),
    );
    fold_framed(h, stream.codec.as_bytes());
    fold(h, u64::from(stream.media_type));
    fold(h, stream.time_base.numerator());
    fold(h, stream.time_base.denominator());
    fold_framed(h, stream.extradata.as_ref());
    let mut vendor = [0; 20];
    let tags = catalogue.tags(&mut vendor);
    fold(h, tags.iter().flatten().count() as u64);
    for (key, value) in tags.into_iter().flatten() {
        fold_framed(h, key);
        fold_framed(h, value);
    }
    Ok(())
}
pub fn trace_stream(out: &mut String, index: usize, catalogue: &CatalogueStream) {
    let stream = &catalogue.view;
    write!(
        out,
        "{{\"stream\":{index},\"pid\":{},\"codec\":",
        stream.track_id
    )
    .expect("String write");
    push_quoted(out, stream.codec);
    write!(
        out,
        ",\"type\":{},\"time_base\":[{},{}],\"extradata\":\"",
        stream.media_type,
        stream.time_base.numerator(),
        stream.time_base.denominator()
    )
    .expect("String write");
    push_hex(out, stream.extradata.as_ref());
    out.push_str("\",\"tags\":[");
    let mut vendor = [0; 20];
    for (i, (key, value)) in catalogue
        .tags(&mut vendor)
        .into_iter()
        .flatten()
        .enumerate()
    {
        if i != 0 {
            out.push(',');
        }
        out.push_str("{\"key_hex\":\"");
        push_hex(out, key);
        out.push_str("\",\"value_hex\":\"");
        push_hex(out, value);
        out.push_str("\"}");
    }
    out.push_str("]}");
}

#[derive(Default, Debug)]
pub struct Stats {
    pub packets: u64,
    pub bytes: u64,
    pub digest: u64,
}
impl Stats {
    /// Folds the operation's settings, as the reference does. Input is always read
    /// from memory, so its file-I/O flag is zero.
    pub fn start(&mut self, retain: usize, max_read: usize) -> Result<(), Error> {
        fold(&mut self.digest, DOMAIN);
        fold(
            &mut self.digest,
            u64::try_from(retain).map_err(|_| Error::Overflow)?,
        );
        fold(
            &mut self.digest,
            u64::try_from(max_read).map_err(|_| Error::Overflow)?,
        );
        fold(&mut self.digest, 0);
        Ok(())
    }
    pub fn packet(&mut self, p: &Packet) -> Result<(), Error> {
        let count = self.packets.checked_add(1).ok_or(Error::Overflow)?;
        let bytes = self
            .bytes
            .checked_add(u64::try_from(p.payload.len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        if count > MAX_PACKETS || bytes > MAX_DELIVERED_BYTES {
            return Err(Error::Limit {
                requested: usize::try_from(bytes).map_err(|_| Error::Overflow)?,
                maximum: MAX_DELIVERED_BYTES as usize,
            });
        }
        p.consume(&mut self.digest)?;
        self.packets = count;
        self.bytes = bytes;
        Ok(())
    }
    pub fn add(&mut self, other: Self) -> Result<(), Error> {
        self.packets = self
            .packets
            .checked_add(other.packets)
            .ok_or(Error::Overflow)?;
        self.bytes = self.bytes.checked_add(other.bytes).ok_or(Error::Overflow)?;
        fold(&mut self.digest, other.digest);
        Ok(())
    }
}
pub struct Retention {
    slots: BudgetVec<Option<Packet>>,
    next: usize,
}
impl Retention {
    pub fn new(pool: &Budget, count: usize) -> Result<Self, Error> {
        if count > 64 {
            return Err(Error::Limit {
                requested: count,
                maximum: 64,
            });
        }
        let mut slots = BudgetVec::new(pool);
        slots.try_reserve_exact(count)?;
        for _ in 0..count {
            slots.try_push(None).map_err(|e| e.error)?;
        }
        Ok(Self { slots, next: 0 })
    }
    pub fn retain(&mut self, packet: Packet) {
        if self.slots.is_empty() {
            return;
        }
        self.slots.as_mut_slice()[self.next] = Some(packet);
        self.next = (self.next + 1) % self.slots.len();
    }
    pub fn consume_after_close(&self, h: &mut u64) -> Result<(), Error> {
        fold(h, 0x52);
        fold(
            h,
            u64::try_from(self.slots.len()).map_err(|_| Error::Overflow)?,
        );
        for (index, packet) in self.slots.as_slice().iter().enumerate() {
            fold(h, u64::try_from(index).map_err(|_| Error::Overflow)?);
            fold(h, u64::from(packet.is_some()));
            if let Some(packet) = packet {
                packet.consume(h)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streamed_index_count_prefix_matches_complete_scalar_consumer() {
        for count in [0u64, 1, 6, 100] {
            let mut full = 53;
            fold(&mut full, count);
            let mut child = 0;
            for word in 0..count * 5 {
                let value = word.wrapping_mul(19);
                fold(&mut full, value);
                fold(&mut child, value);
            }
            let mut composed = 53;
            fold(&mut composed, count);
            append_words(&mut composed, child, u32::try_from(count * 5).unwrap());
            assert_eq!(composed, full);
        }
    }
    #[test]
    fn byte_fields_are_framed_and_packet_sequence_is_not_sorted() {
        let mut a = 0;
        fold_framed(&mut a, b"ab");
        fold_framed(&mut a, b"c");
        let mut b = 0;
        fold_framed(&mut b, b"a");
        fold_framed(&mut b, b"bc");
        assert_ne!(a, b);
        let mut a = 0;
        let mut b = 0;
        for value in [1, 1, 2, 2, 1, 2] {
            fold(&mut a, value);
        }
        for value in [1, 1, 1, 2, 2, 2] {
            fold(&mut b, value);
        }
        assert_ne!(a, b);
    }
    #[test]
    fn raw_unknown_clock_and_explicit_zero_are_distinct() {
        let mut a = 0;
        fold(&mut a, 0);
        fold(&mut a, signed(i64::MIN));
        let mut b = 0;
        fold(&mut b, 1);
        fold(&mut b, signed(0));
        assert_ne!(a, b);
    }
    #[test]
    fn retained_ring_survives_parser_scope_and_releases_all_payload_charges() {
        let pool = Budget::new(65536).unwrap();
        let baseline = pool.used();
        let mut ring = Retention::new(&pool, 2).unwrap();
        let packet = |value: u8| Packet {
            stream: 0,
            pid: 1,
            pts: 0,
            dts: 0,
            duration: 1,
            position: i64::from(value),
            flags: 1,
            payload: Bytes::copy_from_slice(&pool, &[value, value + 1, value + 2]).unwrap(),
        };
        let second = packet(20);
        let third = packet(30);
        let mut wanted = 0;
        fold(&mut wanted, 0x52);
        fold(&mut wanted, 2);
        fold(&mut wanted, 0);
        fold(&mut wanted, 1);
        third.consume(&mut wanted).unwrap();
        fold(&mut wanted, 1);
        fold(&mut wanted, 1);
        second.consume(&mut wanted).unwrap();
        {
            let parser_owned = packet(10);
            ring.retain(parser_owned);
            ring.retain(second);
            ring.retain(third);
        }
        let mut actual = 0;
        ring.consume_after_close(&mut actual).unwrap();
        assert_eq!(actual, wanted);
        assert!(pool.used() > baseline);
        drop(ring);
        assert_eq!(pool.used(), baseline);
    }
}
