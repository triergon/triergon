# License, dependency and corpus provenance

Date checked: 2026-10-30. This records the upstream terms relevant to architecture and distribution; final release inventories must check actual files and locked packages, not only repository badges.

## Independently written code

Proposed policy for the future library is `MIT OR Apache-2.0` for independently authored files, subject to the owner's selection before publication. No production library license grant or public repository has been created by this design phase. The research package is unpublished. Do not import third-party implementation files and describe them as independently licensed code.

Streem's current repository carries GPL-3.0 text. Its existing packet-framing helpers are therefore references/integration code, not automatically reusable under a new permissive license. Keep the consumer adapter in Streem; independently implement generic algorithms from specifications and original tests, or resolve rights before copying anything. Likewise, translating or mechanically rewriting FFmpeg implementation code into Rust does not by itself remove its source-license obligations. The project plan is specification-first bootstrapping, not an automatic C-to-Rust translation.

## Candidate terms

| Candidate | Source declaration | Consequence for the proposal |
|---|---|---|
| Symphonia / Mozilla mp4parse | MPL-2.0 | Commercial use is allowed. Preserve notices and provide recipients access to covered source when distributing compiled code; distributed edits to covered files remain MPL. New independent files can use another license, and static linking does not automatically require disclosing the entire larger application. |
| matroska-demuxer | Zlib OR MIT OR Apache-2.0 | The manifest gives an actual choice; use a selected permitted license and keep required notices. This is more precise than GitHub's single Apache label. |
| OxideAV components / Rust-AV / alfg mp4 | MIT in sampled source/metadata | Permissive source terms do not establish safety, correctness or performance. Preserve applicable notices if any code is eventually incorporated. |
| Remade FFmpeg / TS/H.264 reader candidates | Apache-2.0 in sampled declarations | Preserve license/notices and required modification notices; consider any NOTICE file. No obligation to open independent files merely from linking. |
| NihAV | AGPL in author's stated default distribution | A separate copyleft candidate; not an assumed permissive foundation. The author discusses possible relicensing, but that is not a license grant to this project. |
| rav1d-safe | BSD-2-Clause in checked repository metadata | Future per-file and enabled-feature audit still needed; neither codec patents nor all transitive terms are settled by a repository badge. |

Primary guidance: [MPL FAQ Q8–Q11](https://www.mozilla.org/en-US/MPL/2.0/FAQ/), [MPL text](https://www.mozilla.org/en-US/MPL/2.0/), [Apache-2.0 section 4](https://www.apache.org/licenses/LICENSE-2.0), [matroska-demuxer package declaration](https://github.com/hasenbanck/matroska-demuxer/blob/e038e97b659ec2750b045a5f7a42e60b73c9491f/Cargo.toml), [NihAV author's introduction](https://nihav.org/intro.html). These terms make some reuse possible; the owner's stricter safe-Rust and perfect-fit policy may still reject it.

## FATE and other sample assets

FFmpeg source licenses do not establish a blanket redistribution license for every sample available on FATE or other public media servers. Record the author/source, license or permission, required attribution and content hash for each input. Keep unreviewed or nonredistributable assets outside the repository; block affected release coverage until an authorized equivalent or permitted acquisition is available. Do not quietly omit those cases from coverage.

Store upstream test IDs/source locations as provenance, and implement our own adapter and result normalization. If code or test scripts are imported later, preserve their actual per-file terms. The initial index downloads no samples and executes no upstream make commands; the later explicit FATE runner keeps its selectively acquired samples in an ignored cache. Local benchmark fixtures are generated from synthetic test signals, authored captions and authored EBML structures; generation commands, tool versions and resulting hashes are recorded. Format/codec patent and trademark questions are separate from source copyright licensing and require their own release review for any selected codec implementation.
