//! Borrow the payloads of SMBSF001 scripts and IVF frames before running an operation.

use triergon_core::{Error, Result};

pub(super) enum Codec {
    Avc,
    Hevc,
}

pub(super) enum Event<'a> {
    Packet {
        pts: i64,
        dts: i64,
        duration: i64,
        position: i64,
        flags: u32,
        stream_index: i32,
        data: &'a [u8],
        extra: Option<&'a [u8]>,
    },
    Flush,
    Drain,
}

pub(super) struct Script<'a> {
    pub codec: Codec,
    pub configuration: &'a [u8],
    pub events: Vec<Event<'a>>,
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, size: usize) -> Result<&'a [u8]> {
        let (part, rest) = self.0.split_at_checked(size).ok_or(Error::Truncated)?;
        self.0 = rest;
        Ok(part)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| Error::Truncated)
    }

    fn be_blob(&mut self) -> Result<&'a [u8]> {
        let size = u32::from_be_bytes(self.fixed()?);
        self.take(size as usize)
    }
}

impl<'a> Script<'a> {
    pub fn parse(input: &'a [u8]) -> Result<Self> {
        let mut reader = Reader(input);
        if reader.take(8)? != b"SMBSF001" {
            return Err(Error::Invalid("script magic"));
        }
        let codec = match reader.fixed()? {
            [1] => Codec::Avc,
            [2] => Codec::Hevc,
            _ => return Err(Error::Invalid("script codec")),
        };
        let configuration = reader.be_blob()?;
        let count = u32::from_be_bytes(reader.fixed()?) as usize;
        if count > 64 {
            return Err(Error::Limit {
                requested: count,
                maximum: 64,
            });
        }
        let mut events = Vec::with_capacity(count);
        for _ in 0..count {
            events.push(match reader.fixed()? {
                [0] => {
                    let pts = i64::from_be_bytes(reader.fixed()?);
                    let dts = i64::from_be_bytes(reader.fixed()?);
                    let duration = i64::from_be_bytes(reader.fixed()?);
                    let position = i64::from_be_bytes(reader.fixed()?);
                    let flags = u32::from_be_bytes(reader.fixed()?);
                    let stream_index = i32::from_be_bytes(reader.fixed()?);
                    let size = u32::from_be_bytes(reader.fixed()?);
                    let extra_size = u32::from_be_bytes(reader.fixed()?);
                    let data = reader.take(size as usize)?;
                    let extra = (extra_size != u32::MAX)
                        .then(|| reader.take(extra_size as usize))
                        .transpose()?;
                    Event::Packet {
                        pts,
                        dts,
                        duration,
                        position,
                        flags,
                        stream_index,
                        data,
                        extra,
                    }
                }
                [1] => Event::Flush,
                [2] => Event::Drain,
                _ => return Err(Error::Invalid("script event")),
            });
        }
        if !reader.0.is_empty() {
            return Err(Error::Invalid("script trailing bytes"));
        }
        Ok(Self {
            codec,
            configuration,
            events,
        })
    }
}

pub(super) struct IvfFrame<'a> {
    pub timestamp: u64,
    pub data: &'a [u8],
}

/// Read each physical frame. IVF writers may leave the advisory frame count unset.
pub(super) fn ivf(input: &[u8]) -> Result<impl Iterator<Item = Result<IvfFrame<'_>>>> {
    let mut reader = Reader(input);
    let header = reader.take(32)?;
    if &header[..4] != b"DKIF" || header[4..8] != [0, 0, 32, 0] {
        return Err(Error::Invalid("IVF header"));
    }
    Ok(std::iter::from_fn(move || {
        if reader.0.is_empty() {
            return None;
        }
        let frame = (|| {
            let size = u32::from_le_bytes(reader.fixed()?);
            let timestamp = u64::from_le_bytes(reader.fixed()?);
            let data = reader.take(size as usize)?;
            Ok(IvfFrame { timestamp, data })
        })();
        if frame.is_err() {
            reader.0 = &[];
        }
        Some(frame)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ivf_preserves_physical_frames_and_reports_truncation_once() {
        let mut input = [0; 32].to_vec();
        input[..8].copy_from_slice(b"DKIF\0\0\x20\0");
        input.extend(1_u32.to_le_bytes());
        input.extend(12_u64.to_le_bytes());
        input.extend([17, 3]);
        let mut frames = ivf(&input).unwrap();
        let frame = frames.next().unwrap().unwrap();
        assert_eq!(frame.timestamp, 12);
        assert_eq!(frame.data, [17]);
        assert!(matches!(frames.next(), Some(Err(Error::Truncated))));
        assert!(frames.next().is_none());
    }

    #[test]
    fn script_keeps_empty_side_data_distinct_from_absent_side_data() {
        let mut input = b"SMBSF001\x01\0\0\0\0\0\0\0\x02".to_vec();
        for extra_size in [0_u32, u32::MAX] {
            input.push(0);
            input.extend([0; 40]);
            input.extend(1_u32.to_be_bytes());
            input.extend(extra_size.to_be_bytes());
            input.push(42);
        }
        let script = Script::parse(&input).unwrap();
        assert!(matches!(script.codec, Codec::Avc));
        assert_eq!(script.configuration, []);
        assert!(matches!(
            &script.events[..],
            [
                Event::Packet {
                    data: [42],
                    extra: Some([]),
                    ..
                },
                Event::Packet {
                    data: [42],
                    extra: None,
                    ..
                },
            ]
        ));
        input.push(0);
        assert!(Script::parse(&input).is_err());
    }
}
