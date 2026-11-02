# Existing projects: pinned comparison and disposition

Date: 2026-10-30. This is a scoped source/API audit plus a tiny functional spike, not a security certification, exhaustive ecosystem census or performance comparison. Repository metadata is not a substitute for package/file license declarations. Exact revisions and dates are in `research/source-pins.json`; source-download hashes are in `research/source-downloads.json`.

## Drop-in has three meanings

1. **ABI drop-in:** substitute the bundled libavformat/libavcodec shared objects and existing ABI calls. None of the Rust projects was established as providing this. A compatible CLI name does not provide an ABI.
2. **Streem backend replacement:** preserve compressed packet, timing, seek, metadata, subtitle, resource and cancellation behavior through an adapter. None passes the complete contract today.
3. **Useful implementation reference or leaf:** a bounded component can be reused only after passing the owner’s strict-safe, exact-fit and bootstrap-justification gates. No candidate is admitted yet.

## Candidate matrix

| Project / checked source | Version sampled | Declared license | Relevant scope | Decision |
|---|---|---|---|---|
| [pdeljanov/Symphonia](https://github.com/pdeljanov/Symphonia/tree/ee35874b571a35a9a6e15d3bc9a3aaf8f11fbeee) | 0.6.1 | MPL-2.0 | Container/audio framework; experimental video/subtitle API | Comparison only; no whole-graph safe admission; timestamp/random-access/seek differences |
| [OxideAV/oxideav-workspace](https://github.com/OxideAV/oxideav-workspace/tree/102f6d9f245abba129dac7912ab60f91084a84da) | workspace snapshot | MIT (reported) | Broad codec/container framework | Not a base: separately versioned components, claims need individual evidence |
| [OxideAV/oxideav-core](https://github.com/OxideAV/oxideav-core/tree/c1d9313fb4a16901f6793b2410cc824563446221) | 0.1.42 | MIT | Core packets, registries and allocation | Fails current strict-safe source gate: unsafe arena and Send/Sync implementations |
| [OxideAV/oxideav-mkv](https://github.com/OxideAV/oxideav-mkv/tree/2f086f7c05321e02bca923d2cb64847f55ade969) | 0.0.11 | MIT | Matroska reader/writer | Reference; imports rejected core; not independently runtime-tested here |
| [OxideAV/oxideav-mp4](https://github.com/OxideAV/oxideav-mp4/tree/8d8176aa2ea2664d68d5a85f959ef73f8b5fdbf0) | snapshot | MIT | MP4 reader/writer | Reference; imports core; no local parity/performance proof |
| [OxideAV/oxideav-mpegts](https://github.com/OxideAV/oxideav-mpegts/tree/5bb701d05459595bfc5d26dbd77c17a01af8748f) | snapshot | MIT | Transport stream implementation | Reference; no local parity/performance proof |
| [Remade-With-Rust/remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/tree/525bc79e72b0989b75efbf98f26f98f8bf0dd8f1) | snapshot | Apache-2.0 | Broad replacement plus formats | Not a base: public Demuxer trait only has header/packet reads; no time-seek method |
| [rust-av/rust-av](https://github.com/rust-av/rust-av/tree/d44bb083ffc543d80397c8244f2e8c534f609308) | snapshot | MIT | Multimedia primitives | Reference; unsafe frame code observed in selected data source |
| [rust-av/matroska](https://github.com/rust-av/matroska/tree/bbb048926d1cae46d7c7194ac9b9ac7c409e5147) | snapshot | MIT | Rust-AV container module | Reference; carries framework coupling and no local parity proof |
| [hasenbanck/matroska-demuxer](https://github.com/hasenbanck/matroska-demuxer/tree/e038e97b659ec2750b045a5f7a42e60b73c9491f) | 0.8.1 | Zlib OR MIT OR Apache-2.0 | Small standalone MKV/WebM demuxer | Closest narrow fit to study; unsigned timestamps and forward seek semantics need attention; not admitted |
| [mozilla/mp4parse-rust](https://github.com/mozilla/mp4parse-rust/tree/d3a9abfc93b22687e5e5cd9df86697adcb7d5887) | snapshot | MPL-2.0 | ISO BMFF parsing building block | Evaluate as narrow reference; not a complete packet/seek/playback replacement |
| [alfg/mp4-rust](https://github.com/alfg/mp4-rust/tree/35560e94f5e871a2b2d88bfe964013b39af131e8) | snapshot | MIT | MP4 reader/writer | Unsafe CStr construction observed; not admitted |
| [dholroyd/mpeg2ts-reader](https://github.com/dholroyd/mpeg2ts-reader/tree/e9f5f5a83621b71675fae1b3df09e4c6ae5a12fd) | snapshot | Apache-2.0 | Low-level TS/PES reader and application filters | Possible leaf/reference, not a complete stream reader with our metadata/seeking contract |
| [dholroyd/h264-reader](https://github.com/dholroyd/h264-reader/tree/6036744ebe8892a56fe7a9d447876659b669670e) | snapshot | Apache-2.0 | AVC bitstream reader | Possible leaf/reference, no full graph or target benchmark acceptance |
| [imazen/rav1d-safe](https://github.com/imazen/rav1d-safe/tree/25c1e89e0fc60e5f769728b38cf1d2bd074928ab) | snapshot | BSD-2-Clause (repository metadata) | AV1 software decoder with safe-mode goals | Future codec evaluation only; source/target/feature/graph audit and performance proof still required |

The API and observed behavior drive these dispositions. A recent commit or a large advertised codec list does not establish independent adoption, correctness, optimality or sustained maintenance. No reliable development-velocity forecast was established; owning our API avoids making the delivery plan depend on upstream throughput.

## Symphonia: promising research target, not the selected foundation

The pinned 0.6.1 source exposes signed presentation/decode timestamps and video/subtitle parameters behind experimental features. MKV codec-private and block-addition mappings exist; it is inaccurate to dismiss the current version as audio-only. However, `Packet` exposes no random-access/keyframe flag, which matters for a hardware decoder's seek/start contract. Its core directly depends on `bytemuck`; that crate contains unsafe implementation code. First-party `forbid(unsafe_code)` does not satisfy the owner's whole-selected-graph requirement.

Source evidence: [packet API](https://github.com/pdeljanov/Symphonia/blob/ee35874b571a35a9a6e15d3bc9a3aaf8f11fbeee/symphonia-core/src/packet.rs), [experimental features and dependencies](https://github.com/pdeljanov/Symphonia/blob/ee35874b571a35a9a6e15d3bc9a3aaf8f11fbeee/symphonia-core/Cargo.toml), [MKV codec parameters](https://github.com/pdeljanov/Symphonia/blob/ee35874b571a35a9a6e15d3bc9a3aaf8f11fbeee/symphonia-format-mkv/src/codecs.rs).

The reproducible **published-version** spike uses exact Cargo versions and its recorded lockfile, rather than pretending that a package build proves an entire Git revision. Five four-second synthetic inputs exercise AVC/AAC/subtitle MKV, HEVC/AAC MKV, regular MP4, fragmented MP4 and TS. Both seekable and unknown-length sequential inputs are tried. There are 12 candidate/input invocations; the raw records distinguish opening, full demux, fresh seeks and seeks after EOF.

| Observed case | Result and limit |
|---|---|
| Symphonia, two seekable MKVs | Packet payload sequences and presentation timestamps matched ffprobe for video/audio and the SRT track where present. This is a small positive result, not full Matroska coverage. |
| Symphonia, sequential MKVs | The complete packet payload/timestamp sequences matched; physical end was reported as an I/O error instead of clean EOF in these runs. That distinction is retained in the report. |
| Symphonia, regular/fragmented MP4 | Payload sequences matched; some PTS comparisons differed. Composition offsets/edit-list semantics need investigation; no broad correctness conclusion is drawn. |
| Symphonia, fresh MKV seek | On the tested 1.5s request, an accurate video-track seek reported a 2.0s landing. That is not our required previous-random-access receipt. |
| Symphonia, seek after EOF | The two MKVs returned an EBML ancestry error; the MP4 seek returned a receipt but the next packet read errored. Fresh MP4 seeks did return packets, so this must not be described as all MP4 reads or all seeks failing. |
| Symphonia, regular MP4 on sequential input | End-positioned moov was not found. Supporting this may require a declared source restriction or a bounded cache strategy, not pretending every MP4 is streamable. |
| Symphonia, TS | No reader was registered in the selected 0.6.1 configuration. |
| matroska-demuxer, two MKVs | Video/audio/subtitle payload sequences matched. Audio timestamp comparison differed through this direct adapter; track delay/timing semantics need a proper normalization investigation. This alone is not proof the library is wrong. |

No performance timing or memory benchmark was run. No claim about HDR correctness, arbitrary real-world remuxes, corruption handling or TV playback follows from these fixtures. The observed gaps plus the owner's independent-core preference rule out an immediate swap or a blanket fork. Upstream bug reports have not been sent.

## Other concrete findings

- OxideAV core contains raw allocation/deallocation and unsafe trait implementations in [the arena module](https://github.com/OxideAV/oxideav-core/blob/c1d9313fb4a16901f6793b2410cc824563446221/src/arena/mod.rs) and [its synchronized variant](https://github.com/OxideAV/oxideav-core/blob/c1d9313fb4a16901f6793b2410cc824563446221/src/arena/sync.rs). This is evidence of a policy mismatch, **not** a claim that the unsafe code is unsound.
- Remade FFmpeg’s [public format trait](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/525bc79e72b0989b75efbf98f26f98f8bf0dd8f1/crates/rff-format/src/lib.rs) does not define seeking. Its [MKV reader](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/525bc79e72b0989b75efbf98f26f98f8bf0dd8f1/crates/rff-format-mkv/src/lib.rs) has fixed header/block limits rather than our consumer-owned shared budget. It may be useful research, but is not this interface contract.
- matroska-demuxer has a small dependency-free package manifest and `forbid(unsafe_code)`, but [Frame](https://github.com/hasenbanck/matroska-demuxer/blob/e038e97b659ec2750b045a5f7a42e60b73c9491f/src/lib.rs) uses unsigned timestamps, and its seek API is documented to search for a frame at/after the request rather than guaranteeing a preceding video random-access point. Its [block timestamp conversion](https://github.com/hasenbanck/matroska-demuxer/blob/e038e97b659ec2750b045a5f7a42e60b73c9491f/src/block.rs) saturates subtraction below zero. Those are important design differences for a generic core.
- alfg/mp4-rust’s sampled [emsg parser](https://github.com/alfg/mp4-rust/blob/35560e94f5e871a2b2d88bfe964013b39af131e8/src/mp4box/emsg.rs) constructs a CStr using an unsafe unchecked function. The Mozilla parser is a more relevant contrasting building block, but its own dependency graph and packet/seek adapter still need evaluation.

NihAV is explicitly a research framework and uses AGPL in its default distribution; treat it as a separately licensed research candidate, not a permissive or drop-in foundation ([author’s introduction](https://nihav.org/intro.html)). `ffmpeg-next` is a safe API wrapper over native FFmpeg, not a pure-Rust replacement ([upstream](https://github.com/zmwangx/rust-ffmpeg)). rav1e/rav1d-family projects concern codecs, not container replacement; Rust implementations with assembly or unsafe internals do not automatically pass this policy. Later software-codec selection needs a fresh audit.

## Reuse versus rebuilding recommendation

Bootstrap the core and container slice; retain these projects as independent comparison implementations. Do not reject every future dependency by ideology: a small, mature, fully safe and precisely fitting component may justify itself. But the evidence must show an advantage over our bounded implementation and validation cost, not merely save initial typing. Today, no candidate has demonstrated all three of full selected parity, at least 20% less time and lower memory on our targets.
