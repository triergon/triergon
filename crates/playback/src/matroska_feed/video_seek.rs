//! Native selected-video admission over the source's complete-cluster replay.
use crate::{Delivery, Output, RawEvent, Result, TrackKey, player::OutputScope};
use triergon::typed::Event;
use triergon_core::Error;

/// Admit the selected video's packets from the resolved keyframe onward.
/// Use the same gate on each output in playback order after the seek receipt.
/// The player keeps complete source replay available for other consumers.
pub struct MatroskaVideoSeekGate {
    scope: OutputScope,
    track: TrackKey,
    landing_ns: i64,
    landed: bool,
}

impl MatroskaVideoSeekGate {
    /// Bind the seek intent and matching Matroska receipt from the player.
    pub fn new(intent: &Output, receipt: &Output) -> Result<Self> {
        let RawEvent::Seek(seek) = intent.event() else {
            return Err(Error::Invalid("Matroska video seek intent").into());
        };
        let TrackKey::Matroska(_) = seek.track else {
            return Err(Error::Invalid("Matroska video seek track").into());
        };
        let RawEvent::Matroska(Event::Seek(landing)) = receipt.event() else {
            return Err(Error::Invalid("Matroska video seek receipt").into());
        };
        let scope = receipt.scope();
        if !scope.matches(intent) {
            return Err(Error::Invalid("Matroska video seek scope").into());
        }
        Ok(Self {
            scope,
            track: seek.track,
            landing_ns: landing.actual_ns,
            landed: false,
        })
    }

    /// Before the keyframe, packets remain available through the source view.
    /// An admitted packet can still be decode-only until the requested time.
    pub fn admits(&mut self, output: &Output) -> Result<bool> {
        if !self.scope.matches(output) {
            return Err(Error::Invalid("Matroska video seek packet scope").into());
        }
        if output.delivery() == Delivery::Unselected || output.event().track() != Some(self.track) {
            return Ok(false);
        }
        let RawEvent::Matroska(Event::Packet(packet)) = output.event() else {
            return Ok(false);
        };
        self.landed |= packet.packet.keyframe && packet.packet.pts_ns == Some(self.landing_ns);
        Ok(self.landed)
    }

    pub fn landed(&self) -> bool {
        self.landed
    }
}
