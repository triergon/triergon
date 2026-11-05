`NativeD32MovTextAss1` is a separate native-compatible packet-to-ASS consumer.
It does not change the source Extractor, ConfigJob or Tx3gSourceSemantics1.
Their exact source grammar, Unicode, errors, styling facts and 79 semantic
regressions remain mandatory and unchanged.

Create `NativeMovText` with a shared Budget, explicitly identified AVCodec
extradata, finite Limits, Options and generation. Poll its bounded open phase
until NeedInput, or provide a packet before that phase completes. Header bytes
are available after opening. Provide transfers one complete immutable Input
and an explicit native Clock; rejection returns that Input intact. Poll with
WorkBudget/cancellation, then take_output when ready. NeedOutput means retained
storage pressure; it is resumable. Finish has no delayed subtitle. Reset clears
pending input, read order, cumulative work/error state and changes generation,
while keeping the opened configuration and previously returned owners valid.

The receiver exposes the native return value and optional complete ASS output:
text format, PTS in microseconds, display interval, one ASS rectangle with its
geometry/type/flags/palette facts and exact ASS bytes. Read order is contextual,
including its advancement when the decoder emits a packet later refused by
native UTF8 validation. Original Input, exact Source Timing, source labels,
configuration/epochs, raw native Clock and full owner capacity remain separate.
No Source-validity token, Source Cue or fabricated Source fields are returned.

Options default to the observed null-options native context: no packet timebase,
no explicit frame size, charset conversion or validation-disabling flag. Clock
conversion only uses an explicitly supplied packet timebase. Native rounding,
unknown-PTS sentinel and uint32 display-duration behavior are named receiver
facts; original signed/unknown Source clocks are never overwritten.

The native view deliberately differs from strict source semantics: truncated
text length is bounded by the physical packet; CR is omitted and LF becomes
ASS line breaks; embedded NUL limits the public C-string view. UTF16 and invalid
UTF8 are not silently converted. Defined native style, highlight, colour, font
and wrapping behavior is emitted. Other modifier bytes remain fully retained
in Input, even where the pinned native decoder supplies no ASS rendering for
them. Fonts in native extradata are native byte strings, not silently decoded
using the separate semantic Unicode-font profile. A malformed native header can
select the evidenced native fallback without granting source validity.

Configuration/font/style scans, funding, ASS copies, allocation-growth copies
and publication preserve cursors through work1, cancellation and pressure.
Variable storage uses BudgetVec/Bytes; zero-work polling allocates nothing.
Data refusals are completed native calls. Resource/policy/overflow failures use
core Error, release private packet construction and remain sticky until reset.
Backpressure and cancellation preserve the pending state.

Behavioral evidence is pinned FFmpeg D32, public AVSubtitle observations and
the existing ETSI TS126245 V18 grammar. The original 177 observations retain
all classifications: 18 mov_text decoder controls lacked the decoder in that
old build. The additional 74 captures are ASS charset controls. New observations
use a separately labelled functional-only D32 movtext-enabled profile; the old
reference/profile/baseline is not replaced. FFmpeg 9.0.2 CLI observations provide
an additional independent full-build check. The safe-Rust implementation is
independently authored; FFmpeg code is isolated research evidence/tooling.

This API and its functional evidence do not establish renderer, full-module,
Streem integration, TV, runtime or memory qualification. Existing numerical
thresholds and frozen reference/gate contracts are unchanged.
