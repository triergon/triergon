# Native Mac ARM64 iteration 2 receipt

Exact workers: `identity.json`; source/configuration snapshot: `source/`. Each
report preserves all required cases and raw paired time/memory observations.
All 31 raw parity-matching cases pass the unchanged <=0.80 runtime upper bound
and both lower-memory bounds; 30 pairs per case. K0/K1 slices are accepted.
K2/K3 remain unaccepted: empty decoded frames, mixed compression and codec-delay
seek output still disagree. There are no case exclusions or gate relaxations.

Both native workers retain the original ordered polynomial checksum and consume
all metadata/payload/time fields inside timing. Its equivalent 8-byte expansion
is checked by independent scalar tests. Source/raw trace hashes and the preceding
receipts preserve the audit trail. These results qualify this snapshot only.
No complete module, deferred target or Streem/TV integration is qualified.
