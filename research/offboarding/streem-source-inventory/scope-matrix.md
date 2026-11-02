# Concrete O00 scope matrix

All Streem coordinates below are **verified one-based lines** under
`$STREEM_ROOT` at
`9c7c0fb5f7316c92ab8086beb19db03d5855dd6e`; each file's full SHA-256 is in
`source-files.json`. Ranges identify inspected responsibilities; the complete
individual imported-symbol call ledger is `runtime-symbols.json` / `.tsv`.

Legend: **R** = TV release and ordinary development library runtime; **D** =
`devtriggers` opt-in development runtime; **H** = `hostsim`; **B** = build/package;
**X** = explicitly external tools/tests. Source-enabled is not device-verified.
Every replacement below is pending integration/qualification, not implemented
by this preparation lane.

## Library, input, demux and probe responsibilities

| ID / milestone | Enabled responsibility | Context and replacement boundary | Exact source evidence |
|---|---|---|---|
| INV01 / O70 | Runtime import tables: `avformat`, `avcodec`, `avutil`, optional `swscale` | R/H; remove all 50 dynamic imports after their duties migrate. Capture API symbols remain declared in the first three tables even when their registry components are absent in release. | `rust-modules/src/ff.rs:374–533`; every declaration/call in `runtime-symbols.json` |
| INV02 / O70 | Absolute app-directory loads; util→codec→format order; optional scale; major checks 63/63/61; names reported in diagnostics | R/H; consumer boot and diagnostics removal/replacement. This is dynamic loading, not direct FFmpeg link-time imports. | `rust-modules/src/ff.rs:1598–1649,1668–1730`; `rust-modules/base/src/dynlib.rs:40–55,82–112,337`; `rust-modules/src/app/boot.rs:738–740` |
| INV03 / O10,O50 | Custom AVIO read/seek/size callbacks, 64 KiB reader buffer, source ownership/freeing | R/H; shared core receives bounded source windows/capabilities; consumer performs HTTP/TLS/local I/O. | `rust-modules/src/ff.rs:2393–2464,2777–3000,3002–3151,3201–3215,9247–9278` |
| INV04 / O10,O50 | Sized end parking, transport-fault publication, abort and read retry distinction | R/H; preserve exact EOF versus starvation/failure/cancel semantics and seek after EOF. Consumer retains reconnect/retry policy. | `rust-modules/src/ff.rs:3002–3056,3152–3162`; `rust-modules/src/ff.rs:2777–3000` |
| INV05 / O20 | `matroska` demuxer (Matroska/WebM family), open/probe/packet allocation/read/close | R/H; generic M01 preserves full selected source/track/packet/seek facts, not only what today's player consumes. | `ci/build-ffmpeg.sh:198`; `rust-modules/src/ff.rs:9273,9284,9312–9340,9620,9683,9691,10064–10074` |
| INV06 / O30 | `mov` demuxer (MP4/MOV family); front/end metadata, seekable reads including chapter-induced end positioning | R/H; generic M03; regular/fragmented/edit-list details still need a frozen combination matrix. | `ci/build-ffmpeg.sh:198`; `rust-modules/src/ff.rs:3036–3056,9273–9288,9680–9691` |
| INV07 / O31 | `mpegts` demuxer plus fresh forward-only HLS segment open/probe/read | R/H; M04 handles media bytes. HLS playlist, fetch, retry, ABR, encoder identity and clock normalization stay in consumer. Current HLS branch explicitly requires H.264 video and AAC audio. | `ci/build-ffmpeg.sh:198`; `rust-modules/src/ff.rs:4830–4930,5025,5160–5195,5216–5305` |
| INV08 / O31 | Enabled raw `h264` and `hevc` elementary-stream demuxers | Registry enabled in R/H; development/raw sample paths are distinct from normal MKV/MP4/TS. No authorization to drop these components because development triggers are release-disabled. | `ci/build-ffmpeg.sh:146–148,198`; `Makefile:2150–2161`; `rust-modules/src/ff.rs:1736–1792,9273–9278` |
| INV09 / O21 | Probing/parameter completion through `avformat_find_stream_info`; parsers `h264,hevc,aac,ac3,dvdsub,dvbsub` | R/H; parser/parameter work is separate from demux-only correctness and from movie decode. These registry flags do not enumerate all codec profiles. | `ci/build-ffmpeg.sh:199`; `rust-modules/src/ff.rs:1754,4909,9284` |
| INV10 / O21,O50 | Best-stream lookup, chosen Nth audio, codec-name matching, track names/languages, raster and duration publication | R/H; generic core exposes facts; consumer owns user selection and negotiated-codec validation. Preserve source order and identity. Server-runtime fallback is consumer policy. | `rust-modules/src/ff.rs:129–159,680–718,771–811,9290–9340,9358–9399,9408–9429,9513–9516` |
| INV11 / O10 | Rational rescale, signed timestamps; queue-facing PTS fallback to DTS then zero; text-duration default | R/H; core retains distinct unknown/signed source PTS and DTS. Existing presentation fallbacks belong in the adapter, not canonical source comparison. | `rust-modules/src/ff.rs:3166–3198,5260–5263,9680`; `rust-modules/src/ff.rs:3180–3188` |
| INV12 / O20,O30,O21 | Coded-side-data reads: DV config, colour transfer/primaries/space/range, mastering display/content light; codec-private payloads | R/H; preserve typed plus bounded opaque facts. VP9 HDR rewrite and DV route decisions remain consumer policies. AVPacket side-data fields being declared is not evidence of exhaustive packet-side-data consumption. | `rust-modules/src/ff.rs:194–276,603–678,9468–9491,9552–9598`; `rust-modules/src/player/vp9_hdr.rs` |
| INV13 / O20,O30,O40 | Font attachments and ASS headers copied before format close; track dictionary `filename`, `title`, `name`, `language` | R/H; core preserves all selected metadata/attachments; consumer filters fonts and owns libass. General chapters/tags/attachments are broader frozen scope even though this file exposes only selected reads. | `rust-modules/src/ff.rs:680–718,1336–1397`; no chapter-access binding or AVFormatContext chapter field in `rust-modules/src/ff.rs:294–305` |
| INV14 / O21,O70 | In-process development `ffprobe` trigger; direct `avformat_open_input`, stream log and BSF presence check | D; this is not an `ffprobe` subprocess. Despite its historical HTTP comment, declared FFmpeg has `file` only and network disabled. Replacement needs bounded local inspection/diagnostic semantics or an explicit separate scope. | `rust-modules/src/ff.rs:1724–1792`; `ci/build-ffmpeg.sh:193,202`; `rust-modules/base/src/devtrig.rs:129–138` |
| INV15 / O21,O70 | BSFs enabled: `h264_mp4toannexb,hevc_mp4toannexb,extract_extradata` | R/H registry; application binds only `av_bsf_get_by_name`, used twice for diagnostics. No bound `av_bsf_alloc/send_packet/receive_packet` path. Internal libavformat usage/configure-expanded dependencies were not executed or inspected. | `ci/build-ffmpeg.sh:200`; `rust-modules/src/ff.rs:449,1788–1789`; complete binding ledger |

## Helpers already implemented in GPL Rust

“Already Rust” does **not** mean independently licensed, pure-safe audited, complete
parity, or eligible for transfer. Preserve in the consumer; independently implement
only selected generic functionality from acquired specifications.

| ID / milestone | Existing responsibility | Safe-media / consumer disposition | Exact source evidence |
|---|---|---|---|
| INV16 / O21 | AVC/HEVC codec-private parsing; NAL bounds; parameter sets; Annex-B conversion; H.264 intra/keyframe handling | Compressed-packet framing in Rust already; independent generic contract only if selected. No movie decoder here. | `rust-modules/src/ff.rs:3487–3571,3754–3947`; call `rust-modules/src/ff.rs:9579,9719` |
| INV17 / O21 | AV1 LEB128/OBU/config processing and temporal units; VP9 frame pass-through | Already Rust; retain headers/metadata and keyframe source facts. No simulator AV1/VP9 decoder follows from these helpers. | `rust-modules/src/ff.rs:3235–3433,3478–3485,9585–9598,9700–9718` |
| INV18 / O21 | ASC/ADTS frequency/channel/header preparation, ADTS duration and HLS audio stamp repair; DTS core extraction | Already Rust; AAC core-rate distinction and intentional DTS-HD extension removal are hardware-adapter rules, not universal demux normalization. AC3/EAC3 probing still uses the enabled FFmpeg parser. | `rust-modules/src/ff.rs:3952–3990,4056–4122,4210–4231,4958–5020,5278–5287,9384–9405,9810–9837` |
| INV19 / O21,O50 | DV P7→P8.1 RPU/payload policy, enhancement-layer removal, active-area repair, HDR10+ SEI policy | Existing GPL consumer implementation. Inventory records what this exact baseline does; it does not claim FEL composition or reuse another P7 worktree's behavior. Need explicit independent/generic-versus-consumer scope before reproducing. | `rust-modules/src/ff.rs:3573–3751,3808–3947,9547–9572`; `rust-modules/src/ff/dovi.rs:14,40` |
| INV20 / O40,O50 | `text_ass`, `ass_source`, external `sidecar`, charset detection/conversion | Text adaptation/cache is Rust consumer work. `sub_charset` already depends on `chardetng`/`encoding_rs` in Streem; this admits neither package into safe-media. Preserve original bytes/language/configuration in core. | `rust-modules/src/player/text_ass.rs:85–161`; `rust-modules/src/player/ass_source.rs:243–279`; `rust-modules/src/player/sidecar.rs:87,295,363`; `rust-modules/src/player/sub_charset.rs:22–23,55,144–150` |
| INV21 / O50 | HTTP/TLS, HLS/ABR, queue backpressure, clocks, selection, lifecycle, retry and watch-state | Existing consumer orchestration; do not put URLs/accounts/Starfish/queue policy in the generic core. Bundled intro bytes are an additional bounded local source. | `rust-modules/src/ff.rs:1818–1863,9008–9030,9187–9193`; `rust-modules/src/player/engine.rs:1493–1532`; `rust-modules/src/player/intro_asset.rs:269`; `rust-modules/src/aq.rs`; `rust-modules/src/hls.rs`; `rust-modules/src/curlio.rs` |

## Subtitle decode and rendering boundary

| ID / milestone | Enabled responsibility | Replacement boundary | Exact source evidence |
|---|---|---|---|
| INV22 / O40 | Decoder registry `pgssub,dvdsub,dvbsub,ass,srt,subrip,text,mov_text,webvtt` | All compiled selections must be ledgered; only bitmap-classified tracks create decoder contexts in this caller. Do not call the enabled text decoder registry an actively used text-decoding API. | `ci/build-ffmpeg.sh:201`; `rust-modules/src/ff.rs:1287–1302,9502–9517` |
| INV23 / O40 | PGS/VobSub/DVB bitmap software decode, clear events, multiple rectangles, indexed pixels/palette/alpha, decoder authoring canvas | Independent M05 decoding; retain timing/geometry/canvas and bounded failure semantics. Existing consumer limits rect sets and store capacity; these do not permit dropping required selected input in canonical parity. | `rust-modules/src/ff.rs:1404–1425,1447–1459,1485–1519,1527–1576,9917–9925` |
| INV24 / O40 | Plain/ASS/mov_text raw packet dispatch, ASS codec private and font attachment supply, language-hinted charset | Text packets bypass `avcodec_decode_subtitle2`. `mov_text` prefix removal and four-second missing-duration display are adapter behavior. Classifier names more formats than the enabled decoder list; unknown kinds fall to decoder lookup and can fail. Freeze required formats explicitly. | `rust-modules/src/ff.rs:1294–1330,1336–1397,3180–3188,9928–9943` |
| INV25 / retained boundary | Pinned libass rendering/font stack and GPU subtitle composition | Existing consumer/platform output; no FFmpeg renderer to replace here. Pure-safe core remains dependency-free and does not inherit native libass. | `Makefile:761–770`; `src/ass.c`; `rust-modules/src/player/ass.rs:159,195,304,674` |

## Simulator application runtime (O61)

| ID / milestone | Enabled responsibility | Concrete behavior and open contract | Exact source evidence |
|---|---|---|---|
| INV26 / O61 | Host feature and clock sink gate | H plus D `clocksink` and respective `simvideo` / `simaudio` flags; no TV inclusion. Ordinary macOS simulator launches do not arm these by default; Linux run target arms all three. | `rust-modules/src/player/mod.rs:43–46`; `rust-modules/Cargo.toml:54–76,116–129`; `rust-modules/src/player/ffi_host.rs:258–279`; `Makefile:2102–2114` |
| INV27 / O61 | System FFmpeg video subprocess | Binary from `NJ_SIM_FFMPEG` or PATH, raw `h264`/`hevc` via stdin, decode plus scale/pad/format, RGBA rawvideo stdout. A second, full-feature system build, not bundled subtitle-only libraries. | `rust-modules/src/player/sim_video.rs:141–159` |
| INV28 / O61 | Video output and integration lifecycle | 1920×1080 RGBA, aspect-preserving decrease and centered pad, passthrough frame cadence, PTS min-heap and sink clock, generation reset, child kill/wait, GL upload/composite. Current channels are unbounded; one frame-ahead comment is not a global queue-memory bound. Codec/profile/bit depth/HDR/SAR/rotation/output tolerances unresolved. | `rust-modules/src/player/sim_video.rs:40–43,60–86,92–139,145–211,216–233`; `rust-modules/src/player/ffi_host.rs:501–502,643–644,718–721,737–738`; `rust-modules/src/app/run.rs:524–526` |
| INV29 / O61 | System FFmpeg audio subprocess | Raw `ac3,eac3,aac,dts` mapping; input probe limits; decode plus stereo conversion (`-ac 2`) and device output. Output codec/sample format/rate are not selected explicitly. No Opus/TrueHD mapping. | `rust-modules/src/player/sim_audio.rs:49–59,117–139` |
| INV30 / O61 | Audio output and pacing | Pulse default, optional ALSA; Pulse buffer duration 60 ms; default lead 120 ms clamped 0–2000; PTS-paced writer, generation reset, kill/wait. Native Mac audio behavior is unverified; this source contains no CoreAudio output selection. | `rust-modules/src/player/sim_audio.rs:35–46,62–98,121–174`; `rust-modules/src/player/ffi_host.rs:502,644,721,738` |

## Development capture application runtime (O62)

| ID / milestone | Enabled responsibility | Concrete behavior and replacement boundary | Exact source evidence |
|---|---|---|---|
| INV31 / O62 | Capture gate/protocol and encoder lifecycle | D `capture` trigger; `PXR2` kind 1 selects MPEG1-in-TS. Width selects 480×270 or 960×540; rate byte in 100 kbps, zero defaults 2.5 Mbps. New/displaced clients, geometry/gap restart, encode and keepalive are runtime requirements. | `rust-modules/src/capture.rs:140–155,363–409,589,713–740,790–806`; Rust feature gate `rust-modules/base/src/devtrig.rs:129–138` |
| INV32 / O62 conversion | FFmpeg `sws_getContext`, `sws_scale`, `sws_freeContext`; RGBA→NV12 | Same source/destination dimensions, `SWS_BILINEAR`, signed input stride for bottom-up. No `sws_setColorspaceDetails` override bound. Freeze actual matrix/range/chroma/rounding/alpha expectations before independent conversion. | `rust-modules/src/ff.rs:521–532,956–958,1071–1082,1164–1193,1266` |
| INV33 / existing consumer | NV12 chroma deinterleave and GL downsample/readback | NV12→YUV420P U/V split is already Rust, not another FFmpeg filter. Geometry reduction happens before Venc in `gfx::cap_cycle`; retain GLES boundary separately. | `rust-modules/src/ff.rs:1194–1205`; `rust-modules/gfx/src/gfx.rs:6331`; `rust-modules/src/capture.rs:194–244` |
| INV34 / O62 encoder | MPEG-1 video encoder open/send/receive/frame storage/teardown | YUV420P, 1/30 timebase, GOP 30, B frames 0, requested bitrate, maxrate 7/5, buffer size b/7, fastint DCT; no movie video decoder involved. Encoder acceptance needs conformance and matched bitrate/quality/latency, not byte identity. | `rust-modules/src/ff.rs:938–1058,1059–1067,1208–1223,1255–1276`; `ci/build-ffmpeg.sh:207` |
| INV35 / O62 mux | MPEG-TS output context/new stream/header/write/flush, custom AVIO writer | One encoded video stream, rescaled packet timebase, per-frame AVIO flush, socket callback and partial-write failure. Idle keepalive re-encodes instead of replaying stale TS timing/continuity. | `rust-modules/src/ff.rs:881–895,1087–1149,1224–1233,1269–1272`; `rust-modules/src/capture.rs:790–806` |
| INV36 / distinct existing path | JPEG capture, native turbojpeg with Rust JPEG fallback | Non-FFmpeg path. Retain existing JPEG wire mode and consumer dependency boundaries; disabling MPEG1/TS would leave O62 incomplete unless explicitly respecified. | `rust-modules/src/capture.rs:3–7,457–557,650–701` |

## Build, packaging and retained output boundaries

| ID / milestone | Enabled responsibility | Disposition | Exact source evidence |
|---|---|---|---|
| INV37 / O70 | Bundled FFmpeg source pin, shared libraries and explicit zlib | Version 9.0; tar SHA `7f607a00dd0d28a729d5a4811205812eef01cf6ef6155025febb6f36a9062d52`. Native zlib is an existing FFmpeg dependency, not admitted into new core. Declared configure selection, not inspected installed binary dependencies. | `ci/build-ffmpeg.sh:63–64,188–208,387,401–407`; zlib selection `ci/build-ffmpeg.sh:197` |
| INV38 / O70 | TV release/development package library sets | Release ships `libavutil-plx.so.61`, `libavcodec-plx.so.63`, `libavformat-plx.so.63`. Development adds `libswscale-plx.so.10`, MPEG1 encoder and TS mux. Network/programs/avdevice/avfilter/swresample disabled; `file` only. | `ci/build-ffmpeg.sh:188–208`; `Makefile:754–759,796–813,914,1029–1047,1106,1690–1740` |
| INV39 / O70 | Host macOS/Linux staged libraries and ABI gate | `libavutil-plx.61.dylib`, `libavcodec-plx.63.dylib`, `libavformat-plx.63.dylib`, `libswscale-plx.10.dylib`; Linux same ELF names in `pkg/linux-host/`. Host rules use development component list; explicit stagers request all four. Do not infer `RELEASE=1` makes every host target consistent. | `Makefile:1973–1997,2019–2024,2037–2039,2087–2096`; `ci/stage-host-ffmpeg.sh:19–34`; `ci/stage-host-ffmpeg-linux.sh:12–18`; `ci/ffabi-assert.c` |
| INV40 / O70, scope decision | Native macOS `.app` packaging differs from development simulator | Builds hostsim with no default features, stages libass/fonts/startup media, and recursively bundles normal load dependencies. No explicit FFmpeg dlopen-library staging in assembler; FFmpeg is dynamically loaded and so not proven captured by dependency walking. Static packaging gap, not demonstrated runtime failure. | `ci/mkmacapp.py:118–130,183–205,267–299`; compare `rust-modules/src/ff.rs:1598–1649` |
| INV41 / retained boundary | LG Starfish/ACB video, ordinary audio decode, native TrueHD/MAT output | Existing consumer/platform systems. No new unsafe/native dependency in safe-media. Keep exact packet/configuration contract; TV evidence not inferred from simulator. TrueHD C boundary loads LG decoder/sink, ALSA and resource-manager libraries separately from FFmpeg. | `rust-modules/src/player/CLAUDE.md:1–43,149–180`; `src/starfish.c`; `src/truehd.c:80–82,134,217`; `rust-modules/src/player/truehd.rs:36,140`; `rust-modules/src/player/truehd_mat.rs:42–72` |
| INV42 / X separate scope | Fixture/oracle/test and screenshot/site-generation FFmpeg | External reference tools may remain, separately pinned and rights-reviewed. These are not in-process runtime escape hatches. This lane executed none. the supplemental `.research/parallel-streem-inventory/reference-scan.json` records complete lexical file/identifier/line candidates including CI and tests. | `rust-modules/src/ff_eof_seek_tests.rs:823,903,922`; `rust-modules/src/ff_late_audio_tests.rs:106,153,1137`; `rust-modules/src/ff_subtitle_charset_tests.rs:104`; `tests/fixtures/make_ass_fixture.py:97,182`; `tools/screenshots.py:231,263`; `tools/demo_library.py:105–106`; `tools/render-site-glows.py:71`; `.github/workflows/simulators.yml:130` |

## Gates and open coverage

The parent reviewed and accepted the O00 source inventory for this revision.
O10–O70 are not closed by this report.
Runtime imports/calls, selected component registry and package payload are three
different checks; replacing the demux loop alone closes none of the complete
offboarding target. Source inspection cannot prove which auto-selected internal
FFmpeg components a built binary contains. That needs the parent-controlled build
and final binary/package audit after the integration revision is selected.

Frozen feature manifests must enumerate profiles, levels, bit depths, channel
layouts, container/codec combinations, raw-stream paths, subtitle variants and
error/resource policies. Generic standards coverage exceeds today's direct
consumer reads. No required lane may disappear through an unsupported skip,
platform-only success, disabled feature or aggregate benchmark average.
