# Explicit receiver v2: GNU EUC-KR A2E8

`AssPacketJob::new_v2` and `receiver_charset_v2` explicitly select the additional
A2E8/U+327E mapping for the `euc-kr` label. Every other label delegates to the
unchanged v1 resolver; unavailable aliases and absent declarations stay refused.
The original constructor, resolver, historical Strict EucKr and ReceiverEucKrC1
remain unchanged. No source bytes, labels, physical ranges, epochs or clocks are
rewritten, and there is no UTF8/default profile inference.

The8387-record DATA is the existing licensed CP949-restricted repertoire and C1
identities plus this one mapping fact. It agrees with the complete GNU glibc2.39
EUC-KR primary charmap (SHA25658d4117d28430681c3cc15aecdbf33a7d21a92cd7f1943a55b2911b7fee0e232).
No GNU implementation body or production dependency is copied or translated.
This uses the owner's licensed-implementation inference decision, not a normative
national-standard claim or universal alias equivalence.

The approved additive D32 observer actually returned23 for22 offered source bytes
on the required A2E8 ASS packet, retained both literal UTF8 payload snapshots and
completed successfully. A NUL-tail control retains25 source bytes/full27 UTF8
bytes while the native string is only its23-byte prefix. A truncated-tail control
actually refuses conversion. These facts remain separate from opaque native
returns; no source-consumed offset or Rust-to-native error mapping is invented.
The decoder's exact source, output, work0/1, cancellation and retained-owner
contracts are those of v1. Whole native operation/runtime/memory qualification
and all original51 nonclean outcomes remain separate and unmodified.
