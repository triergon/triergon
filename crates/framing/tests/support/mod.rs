use std::sync::atomic::AtomicBool;
use triergon_core::{Bytes, InputToken, InputWindow, Result, SourceId, Step, WorkBudget};
use triergon_framing::{
    BuildStep,
    ac3::{Ac3Frame, Ac3FrameReader},
    av1::{Av1Obu, Av1ObuReader},
    nal::{NalReader, NalUnit},
};

pub(super) trait Chunked {
    type Item;

    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step<Self::Item>>;
    fn provide(&mut self, window: &InputWindow) -> Result<()>;
    fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()>;
}

macro_rules! chunked {
    ($reader:ty, $item:ty) => {
        impl Chunked for $reader {
            type Item = $item;

            fn step(
                &mut self,
                work: &mut WorkBudget,
                cancel: &AtomicBool,
            ) -> Result<Step<Self::Item>> {
                Self::step(self, work, cancel)
            }

            fn provide(&mut self, window: &InputWindow) -> Result<()> {
                Self::provide(self, window)
            }

            fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
                Self::finish(self, source, token)
            }
        }
    };
}

chunked!(Ac3FrameReader, Ac3Frame);
chunked!(Av1ObuReader, Av1Obu);
chunked!(NalReader, NalUnit);

pub(super) fn read_chunks<R: Chunked>(
    reader: &mut R,
    source: &Bytes,
    chunk: usize,
    units: u64,
    mut check: impl FnMut(&R::Item),
) -> Result<Vec<R::Item>> {
    let cancel = AtomicBool::new(false);
    let mut items = Vec::new();
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(units);
        match reader.step(&mut work, &cancel)? {
            Step::Ready(item) => {
                check(&item);
                items.push(item);
            }
            Step::NeedInput(request) => {
                let start = usize::try_from(request.offset).unwrap();
                if start == source.len() {
                    reader.finish(request.source, request.token)?;
                } else {
                    let end = (start + chunk.min(request.maximum)).min(source.len());
                    reader.provide(&InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        source.slice(start..end)?,
                    )?)?;
                }
            }
            Step::Yield => {}
            Step::End(location) => {
                assert_eq!(location.offset, source.len() as u64);
                return Ok(items);
            }
        }
        assert!(work.spent() <= units);
    }
    panic!("reader made no bounded progress");
}

pub(super) fn build<T>(
    units: u64,
    steps: usize,
    mut step: impl FnMut(&mut WorkBudget, &AtomicBool) -> Result<BuildStep<T>>,
) -> Result<T> {
    let cancel = AtomicBool::new(false);
    for _ in 0..steps {
        match step(&mut WorkBudget::new(units), &cancel)? {
            BuildStep::Yield => {}
            BuildStep::Ready(value) => return Ok(value),
            BuildStep::Complete => panic!("job completed without output"),
        }
    }
    panic!("job made no bounded progress");
}

#[cfg(feature = "reference-trace")]
use triergon_framing::reference_trace::dts::ParserOperationLimits;
use triergon_framing::{
    aac::{AacAdtsStreamLimits, AacConfigLimits},
    ac3::Ac3FrameLimits,
    av1::Av1ObuLimits,
    avc::AvcConfigLimits,
    compat::nal::NalPacketLimits,
    dts::{Dts14CarrierLimits, Dts14SyncLimits, DtsCoreFrameLimits, DtsPackingLimits},
    hevc::HvcConfigLimits,
    nal::{DeclaredConfigLimits, ReaderLimits},
};

pub(super) const fn adts_stream_limits() -> AacAdtsStreamLimits {
    AacAdtsStreamLimits {
        maximum_frame_bytes: 8191,
        maximum_frames: 1000,
        maximum_input_window: 8192,
    }
}

pub(super) const fn asc_wire_limits() -> AacConfigLimits {
    AacConfigLimits {
        maximum_record_bytes: 100_000,
        ..asc_compat_limits()
    }
}

pub(super) const fn ac3_frame_limits() -> Ac3FrameLimits {
    Ac3FrameLimits {
        maximum_frame_bytes: 4096,
        maximum_frames: 32,
        maximum_input_window: 31,
    }
}

pub(super) const fn av1_obu_limits() -> Av1ObuLimits {
    Av1ObuLimits {
        maximum_obu_bytes: 1024,
        maximum_obus: 64,
        maximum_input_window: 17,
    }
}

pub(super) const fn avc_wire_limits() -> AvcConfigLimits {
    AvcConfigLimits {
        maximum_record_bytes: 100_000,
        maximum_nals: 600,
        maximum_nal_bytes: 65535,
    }
}

pub(super) const fn declared_config_limits() -> DeclaredConfigLimits {
    DeclaredConfigLimits {
        maximum_record_bytes: 4096,
        maximum_arrays: 8,
        maximum_nals: 16,
        maximum_nal_bytes: 256,
    }
}

pub(super) const fn dts14_carrier_limits() -> Dts14CarrierLimits {
    Dts14CarrierLimits {
        maximum_packet_bytes: 60000,
        maximum_packets: 4096,
        maximum_input_window: 4096,
    }
}

pub(super) const fn dts14_sync_limits() -> Dts14SyncLimits {
    Dts14SyncLimits {
        maximum_physical_frame_bytes: 4096,
        maximum_canonical_frame_bytes: 3584,
        maximum_frames: 8,
        maximum_input_window: 4096,
    }
}

pub(super) const fn dts_core_limits() -> DtsCoreFrameLimits {
    DtsCoreFrameLimits {
        maximum_frame_bytes: 16384,
        maximum_frames: 16,
        maximum_input_window: 31,
    }
}

#[cfg(feature = "reference-trace")]
pub(super) const fn dts_operation_limits() -> ParserOperationLimits {
    ParserOperationLimits {
        source: Dts14SyncLimits {
            maximum_physical_frame_bytes: 8192,
            maximum_canonical_frame_bytes: 7168,
            maximum_frames: 32,
            maximum_input_window: 4096,
        },
        parser: Dts14CarrierLimits {
            maximum_packet_bytes: 65536,
            maximum_packets: 32,
            maximum_input_window: 4096,
        },
    }
}

pub(super) const fn dts_packing_limits() -> DtsPackingLimits {
    DtsPackingLimits {
        maximum_source_bytes: 4096,
        maximum_output_bytes: 4096,
    }
}

pub(super) const fn nal_reader_limits() -> ReaderLimits {
    ReaderLimits {
        maximum_nal_bytes: 256,
        maximum_padding_bytes: 16,
        maximum_nal_units: 32,
        maximum_input_window: 32,
    }
}

pub(super) const fn asc_compat_limits() -> AacConfigLimits {
    AacConfigLimits {
        maximum_record_bytes: 10000,
        maximum_pce_elements: 70,
        maximum_comment_bytes: 255,
    }
}

pub(super) const fn hevc_record_limits() -> HvcConfigLimits {
    HvcConfigLimits {
        maximum_record_bytes: 1_000_000,
        maximum_arrays: 255,
        maximum_nals: 65535,
        maximum_nal_bytes: 65535,
    }
}

pub(super) const fn large_nal_packet_limits() -> NalPacketLimits {
    NalPacketLimits {
        maximum_packet_bytes: 65536,
        maximum_output_bytes: 131072,
        maximum_nals: 64,
        maximum_segments: 256,
    }
}

pub(super) const fn nal_packet_with_output_limits(maximum_output_bytes: usize) -> NalPacketLimits {
    NalPacketLimits {
        maximum_packet_bytes: 4096,
        maximum_output_bytes,
        maximum_nals: 64,
        maximum_segments: 128,
    }
}

pub(super) const fn nal_packet_limits() -> NalPacketLimits {
    nal_packet_with_output_limits(8192)
}
