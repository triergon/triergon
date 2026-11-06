//! Original exact-bit header facts; output alignment zeros are never source bits.
use crate::{
    DtsCoreField as F, DtsInspectionIssues as I, DtsInspectionSourceStatus, DtsInspectionViews,
    FieldObservation,
};
use triergon_core::{Error, Result, Time};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsPackedHeader {
    prefix: [u8; 15],
    observed_bits: u8,
    issues: u32,
    views: DtsInspectionViews,
    required: Option<u16>,
}
impl DtsPackedHeader {
    /// Allocation-free descriptor for actual header bits. Cached views retain
    /// their existing const getter; the27 large descriptors are not retained.
    pub fn field(&self, field: F) -> FieldObservation {
        // Table5-1 bit positions with HCRC omitted. Its presence moves only
        // Filter..DialogWord by16; the source CPF itself is always at bit38.
        const OFFSETS: [u16; 27] = [
            0, 32, 33, 38, 39, 46, 60, 66, 70, 75, 76, 77, 78, 79, 80, 83, 84, 85, 87, 88, 88, 89,
            93, 95, 98, 99, 100,
        ];
        let cpf = if self.observed_bits >= 39 {
            Some((self.prefix[4] >> 1) & 1)
        } else {
            None
        };
        if field == F::Hcrc && cpf == Some(0) {
            return FieldObservation::NotInSyntax;
        }
        let mut offset = OFFSETS[field as usize];
        if (field as usize) >= F::Filter as usize && cpf == Some(1) {
            offset += 16;
        }
        if offset >= u16::from(self.observed_bits) {
            return FieldObservation::NotReached;
        }
        let count = u16::from(field.width()).min(u16::from(self.observed_bits) - offset);
        let mut prefix = 0_u32;
        for bit in offset..offset + count {
            prefix =
                (prefix << 1) | u32::from((self.prefix[usize::from(bit / 8)] >> (7 - bit % 8)) & 1);
        }
        FieldObservation::Bits {
            offset,
            width: field.width(),
            observed_bits: count as u8,
            prefix,
        }
    }
    pub fn value(&self, field: F) -> Option<u32> {
        self.field(field).value()
    }
    pub const fn issues(&self) -> u32 {
        self.issues
    }
    pub const fn views(&self) -> DtsInspectionViews {
        self.views
    }
    pub const fn required_header_bits(&self) -> Option<u16> {
        self.required
    }
    pub fn syntax_complete(&self) -> bool {
        self.value(F::Sync) == Some(0x7ffe8001) && self.value(F::DialogWord).is_some()
    }
    pub fn source_status(&self) -> DtsInspectionSourceStatus {
        let bits = self.issues;
        if bits & I::EXTENSION_SUBSTREAM != 0 {
            DtsInspectionSourceStatus::Unsupported
        } else if bits & I::NONCANONICAL_SYNC != 0 {
            DtsInspectionSourceStatus::Invalid
        } else if bits & I::TRUNCATED != 0 {
            DtsInspectionSourceStatus::Truncated
        } else if bits
            & (I::FIXED_BIT
                | I::FRAME_SIZE
                | I::BLOCK_COUNT
                | I::SAMPLE_RATE_CODE
                | I::BITRATE_CODE
                | I::LFE_CODE
                | I::PCM_RESOLUTION
                | I::NORMAL_SHORT)
            != 0
        {
            DtsInspectionSourceStatus::Invalid
        } else if bits & (I::TERMINATION | I::FUTURE_VERSION | I::UNSUPPORTED_EXTENSION) != 0 {
            DtsInspectionSourceStatus::Unsupported
        } else {
            DtsInspectionSourceStatus::NormalHeader
        }
    }
}

/// Fixed<=120 source-bit visits,27 descriptors; no allocation. A caller's
/// charged fixed prefix probe can invoke this once; the whole frame is not read.
pub fn inspect_dts_header_bits(storage: &[u8], valid_bits: usize) -> Result<DtsPackedHeader> {
    if valid_bits > storage.len().checked_mul(8).ok_or(Error::Overflow)? {
        return Err(Error::Invalid("DTS valid bit extent"));
    }
    let available = valid_bits.min(120);
    let mut position = 0_usize;
    let mut fields = [FieldObservation::NotReached; 27];
    for field in F::ALL {
        if field == F::Hcrc && fields[F::CrcPresent as usize].value() == Some(0) {
            fields[field as usize] = FieldObservation::NotInSyntax;
            continue;
        }
        if position == available {
            continue;
        }
        let width = usize::from(field.width());
        let count = width.min(available - position);
        let mut prefix = 0_u32;
        for bit in position..position + count {
            prefix = (prefix << 1) | u32::from((storage[bit / 8] >> (7 - bit % 8)) & 1);
        }
        fields[field as usize] = FieldObservation::Bits {
            offset: u16::try_from(position).map_err(|_| Error::Overflow)?,
            width: field.width(),
            observed_bits: u8::try_from(count).map_err(|_| Error::Overflow)?,
            prefix,
        };
        position += count;
    }
    let value = |field: F| fields[field as usize].value();
    let mut issues = 0;
    if value(F::Sync).is_some_and(|v| v != 0x7ffe8001) {
        issues |= I::NONCANONICAL_SYNC;
    } else if value(F::DialogWord).is_none() {
        issues |= I::TRUNCATED;
    }
    if value(F::Sync) == Some(0x64582025) {
        issues |= I::EXTENSION_SUBSTREAM;
    }
    if value(F::Fixed).is_some_and(|v| v != 0) {
        issues |= I::FIXED_BIT;
    }
    if value(F::FrameSize).is_some_and(|v| v < 95) {
        issues |= I::FRAME_SIZE;
    }
    let normal = value(F::Normal);
    let blocks = value(F::BlockCount);
    let short = value(F::Short);
    if blocks.is_some_and(|v| v < 5 || (normal == Some(1) && !matches!(v, 7 | 15 | 31 | 63 | 127)))
    {
        issues |= I::BLOCK_COUNT;
    }
    if normal == Some(1) && short.is_some_and(|v| v != 31) {
        issues |= I::NORMAL_SHORT;
    }
    if normal == Some(0) {
        issues |= I::TERMINATION;
    }
    let rate = value(F::SampleRate).and_then(core_rate);
    if value(F::SampleRate).is_some() && rate.is_none() {
        issues |= I::SAMPLE_RATE_CODE;
    }
    let bitrate = value(F::Rate).and_then(target_bitrate);
    if value(F::Rate).is_some_and(|v| v != 29) && bitrate.is_none() {
        issues |= I::BITRATE_CODE;
    }
    if value(F::Lfe) == Some(3) {
        issues |= I::LFE_CODE;
    }
    if value(F::PcmResolution).is_some_and(|v| !matches!(v, 0 | 1 | 2 | 3 | 5 | 6)) {
        issues |= I::PCM_RESOLUTION;
    }
    let version = value(F::Version);
    if version.is_some_and(|v| v > 7) {
        issues |= I::FUTURE_VERSION;
    }
    if value(F::Extension) == Some(1)
        && value(F::ExtensionId).is_some_and(|v| !matches!(v, 0 | 2 | 6))
    {
        issues |= I::UNSUPPORTED_EXTENSION;
    }
    let samples = blocks
        .and_then(|v| v.checked_add(1))
        .and_then(|v| v.checked_mul(32));
    let window = if normal == Some(1)
        && short == Some(31)
        && blocks.is_some_and(|v| matches!(v, 7 | 15 | 31 | 63 | 127))
        && version.is_some_and(|v| v <= 7)
    {
        match (samples, rate) {
            (Some(n), Some(d)) => Some(Time::new(i128::from(n), u128::from(d))?),
            _ => None,
        }
    } else {
        None
    };
    let dialog = value(F::DialogWord)
        .and_then(|v| i8::try_from(v).ok())
        .and_then(|v| match version {
            Some(6) => Some(-16 - v),
            Some(7) => Some(-v),
            _ => None,
        });
    let channels = value(F::AudioMode)
        .and_then(|v| usize::try_from(v).ok())
        .and_then(|v| {
            [1, 2, 2, 2, 2, 3, 3, 4, 4, 5, 6, 6, 6, 7, 8, 8]
                .get(v)
                .copied()
        });
    let required = if value(F::Sync) == Some(0x7ffe8001) {
        value(F::CrcPresent).map(|v| if v == 0 { 104 } else { 120 })
    } else {
        None
    };
    // position is exactly the actual syntax bits visited, at most104/120.
    // Ignore source body/lookahead and unobserved trailing bits just as the
    // old descriptor array did. This changes neither retained media nor output.
    let mut prefix = [0_u8; 15];
    let bytes = position.div_ceil(8);
    prefix[..bytes].copy_from_slice(&storage[..bytes]);
    if !position.is_multiple_of(8) {
        prefix[bytes - 1] &= u8::MAX << (8 - position % 8);
    }
    Ok(DtsPackedHeader {
        prefix,
        observed_bits: u8::try_from(position).map_err(|_| Error::Overflow)?,
        issues,
        required,
        views: DtsInspectionViews {
            declared_frame_bytes: value(F::FrameSize).and_then(|v| v.checked_add(1)),
            core_rate_hz: rate,
            coded_window_samples: samples,
            full_bandwidth_channels: channels,
            nominal_bitrate: bitrate,
            open_bitrate: value(F::Rate).map(|v| v == 29),
            normal_window: window,
            dialog_gain_db: dialog,
        },
    })
}
fn core_rate(code: u32) -> Option<u32> {
    match code {
        1 => Some(8000),
        2 => Some(16000),
        3 => Some(32000),
        6 => Some(11025),
        7 => Some(22050),
        8 => Some(44100),
        11 => Some(12000),
        12 => Some(24000),
        13 => Some(48000),
        _ => None,
    }
}
fn target_bitrate(code: u32) -> Option<u32> {
    [
        32000, 56000, 64000, 96000, 112000, 128000, 192000, 224000, 256000, 320000, 384000, 448000,
        512000, 576000, 640000, 768000, 960000, 1024000, 1152000, 1280000, 1344000, 1408000,
        1411200, 1472000, 1536000,
    ]
    .get(usize::try_from(code).ok()?)
    .copied()
}
