//! Complete source text packets/documents. Unicode and native views stay distinct.
use crate::{Mp4Packet, Output, RawEvent, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, SourceId, SourceLocation, Time, WorkBudget};
use triergon_subtitles::text::{self, Extractor, Input, RawClock, Timing};
#[derive(Clone, Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "bounded source owner is inline; all backing storage remains charged"
)]
enum Record {
    Mp4(Mp4Packet),
    Matroska(triergon_matroska::typed::Packet),
    Document(Input),
}
#[derive(Clone, Debug)]
pub struct TextSource {
    record: Record,
    source: SourceId,
    pool: Budget,
}
pub struct RejectedOutput {
    pub output: Output,
    pub error: Error,
}
impl TextSource {
    #[expect(
        clippy::result_large_err,
        reason = "return exact charged producer output"
    )]
    pub fn from_output(output: Output) -> std::result::Result<Self, RejectedOutput> {
        if !matches!(
            output.event(),
            RawEvent::Mp4(crate::Mp4Event::Packet(_))
                | RawEvent::Matroska(triergon_matroska::typed::Event::Packet(_))
        ) {
            return Err(RejectedOutput {
                output,
                error: Error::Invalid("text handoff requires complete source packet"),
            });
        }
        let source = output.source_id();
        let pool = output.pool().clone();
        let record = match output.into_event() {
            RawEvent::Mp4(crate::Mp4Event::Packet(p)) => Record::Mp4(p),
            RawEvent::Matroska(triergon_matroska::typed::Event::Packet(p)) => Record::Matroska(p),
            _ => unreachable!(),
        };
        Ok(Self {
            record,
            source,
            pool,
        })
    }
    pub fn document(pool: &Budget, input: Input) -> Self {
        Self {
            source: input.source_id,
            pool: pool.clone(),
            record: Record::Document(input),
        }
    }
    pub fn mp4(&self) -> Option<&Mp4Packet> {
        if let Record::Mp4(p) = &self.record {
            Some(p)
        } else {
            None
        }
    }
    pub fn matroska(&self) -> Option<&triergon_matroska::typed::Packet> {
        if let Record::Matroska(p) = &self.record {
            Some(p)
        } else {
            None
        }
    }
}
pub struct TextOutput {
    pub source: TextSource,
    pub event: text::Event,
}
#[expect(
    clippy::large_enum_variant,
    reason = "bounded inline source event retains all charged owners"
)]
pub enum TextStep {
    Yield,
    Event(TextOutput),
    NeedPacket,
    Finished,
}
#[derive(Debug)]
pub struct TextRejection {
    pub source: TextSource,
    pub error: crate::Error,
}
struct CopyStage {
    fields: [BudgetVec<u8>; 4],
    ready: [Option<Bytes>; 4],
    field: usize,
    addition: Option<usize>,
    scan: usize,
    scanned: bool,
}
pub struct TextBridge {
    extractor: Extractor,
    pool: Budget,
    source: SourceId,
    track: u64,
    generation: u64,
    limits: text::Limits,
    pending: Option<TextSource>,
    copy: Option<CopyStage>,
    sequence: u64,
    finishing: bool,
}
fn exact_ns(t: triergon_matroska::timing::ExactTime) -> Result<Time> {
    Time::new(t.numerator(), u128::from(t.denominator()) * 1_000_000_000)
}
impl TextBridge {
    pub fn new(
        pool: &Budget,
        source: SourceId,
        track: u64,
        generation: u64,
        limits: text::Limits,
    ) -> Self {
        Self {
            extractor: Extractor::tx3g_iso_boxes(pool, limits, generation),
            pool: pool.clone(),
            source,
            track,
            generation,
            limits,
            pending: None,
            copy: None,
            sequence: 0,
            finishing: false,
        }
    }
    pub fn reset(&mut self, generation: u64) {
        self.extractor.reset(generation);
        self.generation = generation;
        self.pending = None;
        self.copy = None;
        self.sequence = 0;
        self.finishing = false;
    }
    pub fn finish(&mut self) {
        self.finishing = true;
        if self.copy.is_none() {
            self.extractor.finish_input();
        }
    }
    fn direct_input(&self, source: &TextSource) -> Result<Option<Input>> {
        if source.source != self.source {
            return Err(Error::SourceMismatch);
        }
        if !source.pool.same_pool(&self.pool) {
            return Err(Error::ForeignPool);
        }
        match &source.record {
            Record::Document(input) => Ok(Some(input.clone())),
            Record::Matroska(p) => {
                let track = p
                    .configuration
                    .tracks()
                    .get(p.packet.track)
                    .ok_or(Error::Invalid("text track index"))?;
                if track.number != self.track {
                    return Err(Error::Invalid("selected text track"));
                }
                if !matches!(
                    track.codec.as_ref(),
                    "S_TEXT/UTF8" | "S_TEXT/WEBVTT" | "S_TEXT/ASS" | "S_TEXT/SSA"
                ) {
                    return Err(Error::Unsupported("source text codec mapping"));
                }
                if p.configuration.generation() != self.generation {
                    return Err(Error::StaleGeneration {
                        expected: self.generation,
                        received: p.configuration.generation(),
                    });
                }
                for (n, max) in [
                    (p.packet.data.len(), self.limits.max_input_bytes),
                    (track.codec.len(), self.limits.max_label_bytes),
                    (
                        p.configuration
                            .initialization(p.packet.track)
                            .ok_or(Error::Invalid("text initialization"))?
                            .data
                            .len(),
                        self.limits.max_config_bytes,
                    ),
                ] {
                    if n > max {
                        return Err(Error::Limit {
                            requested: n,
                            maximum: max,
                        });
                    }
                }
                Ok(None)
            }
            Record::Mp4(p) => {
                if u64::from(p.configuration.track.track_id) != self.track
                    || p.configuration.description.format != *b"tx3g"
                {
                    return Err(Error::Invalid("selected source tx3g track"));
                }
                let base = p.configuration.track.base.require()?;
                let sample = &p.source.sample;
                Ok(Some(Input {
                    format: text::Format::Tx3g,
                    encoding: text::Encoding::Tx3gInBand,
                    payload: p.source.payload.clone(),
                    codec_id: None,
                    codec_private: Some(p.configuration.description.raw().bytes().clone()),
                    charset_label: None,
                    addition: None,
                    timing: Timing::from_ticks(RawClock {
                        numerator: base.numerator(),
                        denominator: base.denominator(),
                        pts_ticks: sample.cts_ticks,
                        dts_ticks: sample.dts_ticks,
                        duration_ticks: Some(i128::from(sample.duration_ticks)),
                    })?,
                    source_id: p.source.source,
                    generation: p.source.token.generation(),
                    sequence: sample.ordinal,
                    configuration_epoch: p.configuration.revision,
                    container_position: Some(SourceLocation {
                        source: p.source.source,
                        offset: sample.range.start,
                    }),
                }))
            }
        }
    }
    #[expect(
        clippy::result_large_err,
        reason = "rejection returns exact source owner inline"
    )]
    pub fn provide(&mut self, source: TextSource) -> std::result::Result<(), TextRejection> {
        if self.pending.is_some() || self.finishing {
            return Err(TextRejection {
                source,
                error: Error::Invalid("text packet pending or finished"),
            });
        }
        let input = match self.direct_input(&source) {
            Ok(x) => x,
            Err(error) => return Err(TextRejection { source, error }),
        };
        if let Some(input) = input {
            if let Err(rejected) = self.extractor.provide(input) {
                return Err(TextRejection {
                    source,
                    error: rejected.error,
                });
            }
        } else {
            self.copy = Some(CopyStage {
                fields: std::array::from_fn(|_| BudgetVec::new(&self.pool)),
                ready: std::array::from_fn(|_| None),
                field: 0,
                addition: None,
                scan: 0,
                scanned: false,
            });
        }
        self.pending = Some(source);
        Ok(())
    }
    fn copy_step(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<bool> {
        let source = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("text copy source"))?;
        let Record::Matroska(p) = &source.record else {
            return Err(Error::Invalid("text copy kind"));
        };
        let stage = self
            .copy
            .as_mut()
            .ok_or(Error::Invalid("text copy stage"))?;
        let track = &p.configuration.tracks()[p.packet.track];
        if !stage.scanned {
            if !w.try_charge(1) {
                return Ok(false);
            }
            if let Some(group) = p.group.as_ref()
                && let Some(a) = group.additions().nth(stage.scan)
            {
                if a.id() == 1 {
                    if stage.addition.is_some() {
                        return Err(Error::Invalid("duplicate source text BlockAdditional1"));
                    }
                    stage.addition = Some(stage.scan);
                }
                stage.scan += 1;
                return Ok(false);
            }
            stage.scanned = true;
            return Ok(false);
        }
        if stage.field < 4 {
            if c.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let bytes: &[u8] = match stage.field {
                0 => p.packet.data.as_ref(),
                1 => track.codec.as_bytes(),
                2 => {
                    p.configuration
                        .initialization(p.packet.track)
                        .ok_or(Error::Invalid("text init"))?
                        .data
                }
                _ => match (stage.addition, p.group.as_ref()) {
                    (Some(i), Some(g)) => g
                        .additions()
                        .nth(i)
                        .ok_or(Error::Invalid("text addition"))?
                        .data(),
                    _ => &[],
                },
            };
            let maximum = match stage.field {
                0 => self.limits.max_input_bytes,
                1 => self.limits.max_label_bytes,
                2 => self.limits.max_config_bytes,
                _ => self.limits.max_addition_bytes,
            };
            if bytes.len() > maximum {
                return Err(Error::Limit {
                    requested: bytes.len(),
                    maximum,
                });
            }
            let buffer = &mut stage.fields[stage.field];
            if buffer.capacity() == 0 && !bytes.is_empty() {
                if !w.try_charge(1) {
                    return Ok(false);
                }
                buffer.try_reserve_exact(bytes.len())?;
                return Ok(false);
            }
            while buffer.len() < bytes.len() {
                if c.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                if !w.try_charge(1) {
                    return Ok(false);
                }
                buffer.try_push(bytes[buffer.len()]).map_err(|e| e.error)?;
            }
            if !w.try_charge(1) {
                return Ok(false);
            }
            let done = std::mem::replace(buffer, BudgetVec::new(&self.pool));
            let frozen = match done.freeze() {
                Ok(b) => b,
                Err(f) => {
                    *buffer = f.buffer;
                    return Err(f.error);
                }
            };
            stage.ready[stage.field] = Some(frozen);
            stage.field += 1;
            return Ok(false);
        }
        if !w.try_charge(1) {
            return Ok(false);
        }
        let timing = p.packet.timing.source()?;
        self.sequence = self.sequence.checked_add(1).ok_or(Error::Overflow)?;
        let input = Input {
            format: match track.codec.as_ref() {
                "S_TEXT/UTF8" => text::Format::Utf8Packet,
                "S_TEXT/WEBVTT" => text::Format::MatroskaWebVtt,
                _ => text::Format::OpaqueAss,
            },
            encoding: if matches!(track.codec.as_ref(), "S_TEXT/ASS" | "S_TEXT/SSA") {
                text::Encoding::Opaque
            } else {
                text::Encoding::Utf8
            },
            payload: stage.ready[0]
                .take()
                .ok_or(Error::Invalid("text payload"))?,
            codec_id: stage.ready[1].take(),
            codec_private: stage.ready[2].take(),
            charset_label: None,
            addition: if stage.addition.is_some() {
                Some(text::Addition {
                    id: 1,
                    bytes: stage.ready[3]
                        .take()
                        .ok_or(Error::Invalid("text addition"))?,
                })
            } else {
                None
            },
            timing: Timing::exact(
                timing.pts.map(exact_ns).transpose()?,
                timing.duration.map(exact_ns).transpose()?,
            ),
            source_id: self.source,
            generation: p.configuration.generation(),
            sequence: self.sequence,
            configuration_epoch: p.configuration.revision(),
            container_position: None,
        };
        self.extractor.provide(input).map_err(|e| e.error)?;
        self.copy = None;
        if self.finishing {
            self.extractor.finish_input();
        }
        Ok(true)
    }
    pub fn step(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<TextStep> {
        if c.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if w.remaining() == 0 {
            return Ok(TextStep::Yield);
        }
        if self.copy.is_some() {
            self.copy_step(w, c)?;
            return Ok(TextStep::Yield);
        }
        Ok(match self.extractor.step(w, c)? {
            text::Status::Yield | text::Status::NeedOutput => TextStep::Yield,
            text::Status::NeedInput => {
                self.pending = None;
                TextStep::NeedPacket
            }
            text::Status::Finished => {
                self.pending = None;
                TextStep::Finished
            }
            text::Status::Event(event) => TextStep::Event(TextOutput {
                source: self
                    .pending
                    .as_ref()
                    .ok_or(Error::Invalid("text event without source"))?
                    .clone(),
                event,
            }),
        })
    }
}
