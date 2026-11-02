# Original full-DVB nonmodifying regressions

These12 independently authored sources cover depth2/4/8, literal/run encoding
and flag0/1. Every5x2 region is explicitly filled with CLUT3. The source codes
are11112 on each row. Flag0 writes11112; flag1 leaves33332. Full nominal CLUTs,
current ODS and original source/index/RGBA bytes are retained.

Primary: [EN300743v1.6.1 §7.2.5 and Tables22/24/26](https://www.etsi.org/deliver/etsi_en/300700_300799/300743/01.06.01_60/en_300743v010601p.pdf),
PDF SHA-256 `1a5b211303397c52b771eb11e8f9f844d88727ecccd3fe0a5e8a6ac961d93e87`.
The parent ran all12 source tests successfully before promotion. All12 default
native observations returned0, but all six flag1 index planes differ from these
source expectations; all12 full SourceNearest palette views also differ. These
findings remain strict nonpasses. Native bytes/order are not normalized.

The ignored `nonmodifying-batch-1/` lane holds the original author, fixture hashes,
source proof, Matroska carriers and raw-native comparison evidence. Only fixture
include paths and expansion of the original case list changed during promotion.
No semantic decoder fix was made for behavior already correct on these sources.
