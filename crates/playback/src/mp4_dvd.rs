//! DVD subtitles in MP4: a `subp` track whose `mp4s` sample entry declares
//! object type 0xE0 (ISO/IEC 14496-1 Table 5, VobSub).
//!
//! [`Mp4DvdSetup`] reads the colour table from the entry's decoder-specific
//! information ([`ObjectTypeScan`]), as FFmpeg's MP4 demuxer hands it to its
//! decoder ([`compat::dvd::mp4_configuration`]). The [`Mp4DvdTrack`] it
//! makes turns the track's packets into [`BitmapInput`]s for the
//! [`DvdReceiver`](crate::bitmap_bridge::DvdReceiver).
use crate::bitmap_bridge::{BitmapInput, BitmapScope, DvdBridgeConfig};
use crate::{Delivery, Mp4Event, Mp4Packet, Output, RawEvent, TrackKey};
use triergon_core::{Error, Result, WorkBudget};
use triergon_isobmff::{ConfigurationLimits, ObjectTypeScan};
use triergon_subtitles::{compat, pgs::PacketClock};

/// The object type of DVD subpicture streams.
const OBJECT_TYPE_VOBSUB: u8 = 0xe0;

/// Reads one DVD subtitle track's sample entry.
pub struct Mp4DvdSetup {
    scan: ObjectTypeScan,
    scope: BitmapScope,
    epoch: u64,
}

impl Mp4DvdSetup {
    /// Bind from a current MP4 packet output of the track.
    pub fn new(output: &Output, limits: ConfigurationLimits) -> Result<Self> {
        let RawEvent::Mp4(Mp4Event::Packet(packet)) = output.event() else {
            return Err(Error::Invalid("MP4 DVD track needs an MP4 packet"));
        };
        let configuration = &packet.configuration;
        if configuration.track.handler != *b"subp" || configuration.description.format != *b"mp4s" {
            return Err(Error::Unsupported(
                "MP4 DVD track requires a subp mp4s entry",
            ));
        }
        Ok(Self {
            scan: ObjectTypeScan::new(&configuration.description, limits)?,
            scope: BitmapScope {
                source: output.source_id(),
                track: u64::from(configuration.track.track_id),
                generation: output.generation(),
                configuration: configuration.revision,
            },
            epoch: output.epoch(),
        })
    }

    /// The track once its entry is read; `None` to yield.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<Option<Mp4DvdTrack>> {
        let Some((object_type, specific)) = self.scan.poll(work)? else {
            return Ok(None);
        };
        if object_type != OBJECT_TYPE_VOBSUB {
            return Err(Error::Unsupported("MP4 subpicture object type"));
        }
        let settings = specific
            .as_deref()
            .and_then(compat::dvd::mp4_configuration)
            .map_or_else(
                || Ok(compat::dvd::Settings::default()),
                |text| compat::dvd::Settings::from_configuration(&text),
            )?;
        Ok(Some(Mp4DvdTrack {
            scope: self.scope,
            epoch: self.epoch,
            config: DvdBridgeConfig {
                settings,
                ..DvdBridgeConfig::default()
            },
        }))
    }
}

/// One DVD subtitle track of an MP4 movie and its decoder settings.
pub struct Mp4DvdTrack {
    scope: BitmapScope,
    epoch: u64,
    config: DvdBridgeConfig,
}

impl Mp4DvdTrack {
    pub fn scope(&self) -> BitmapScope {
        self.scope
    }

    /// The receiver configuration: the track's colour table, if it has one.
    pub fn config(&self) -> DvdBridgeConfig {
        self.config.clone()
    }

    /// The track's packet in `output` on its media clock, `None` for another
    /// track or an unselected one. The packet keeps its source facts.
    pub fn packet(&self, output: &Output, sequence: u64) -> Result<Option<BitmapInput<Mp4Packet>>> {
        let track = u32::try_from(self.scope.track).map_err(|_| Error::Overflow)?;
        if output.delivery() == Delivery::Unselected
            || output.event().track() != Some(TrackKey::Mp4(track))
        {
            return Ok(None);
        }
        let RawEvent::Mp4(Mp4Event::Packet(packet)) = output.event() else {
            return Ok(None);
        };
        if output.source_id() != self.scope.source {
            return Err(Error::SourceMismatch);
        }
        if output.generation() != self.scope.generation {
            return Err(Error::StaleGeneration {
                expected: self.scope.generation,
                received: output.generation(),
            });
        }
        if output.epoch() != self.epoch || packet.configuration.revision != self.scope.configuration
        {
            return Err(Error::Invalid("MP4 DVD configuration changed"));
        }
        let base = packet.configuration.track.base.require()?;
        let ticks = |t: Option<i128>| {
            t.map(i64::try_from)
                .transpose()
                .map_err(|_| Error::Overflow)
        };
        let sample = &packet.source.sample;
        Ok(Some(BitmapInput {
            scope: self.scope,
            sequence,
            raw: packet.source.payload.clone(),
            clock: PacketClock {
                pts: ticks(sample.cts_ticks)?,
                dts: ticks(sample.dts_ticks)?,
                timebase: base,
            },
            duration: Some(i64::from(sample.duration_ticks)),
            metadata: packet.clone(),
        }))
    }
}
