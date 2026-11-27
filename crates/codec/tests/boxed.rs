//! A boxed decoder forwards every call, the provided ones included, and converts only
//! its refusals.
use triergon_codec::{AudioDecoder, AudioFrame, DownmixLevels, DynAudioDecoder, Error};

struct Refusal;

impl From<Refusal> for Error {
    fn from(Refusal: Refusal) -> Error {
        Error::Invalid("empty access unit")
    }
}

/// One sample per byte of each access unit, a start delay, and samples held back.
struct Holding;

impl AudioDecoder for Holding {
    type Error = Refusal;

    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, Refusal> {
        if au.is_empty() {
            return Err(Refusal);
        }
        out.samples_per_channel = au.len();
        Ok(Some(DownmixLevels::default()))
    }
    fn delay(&self) -> usize {
        312
    }
    fn flush(&mut self, out: &mut AudioFrame) -> Result<(), Refusal> {
        out.samples_per_channel = 24;
        Ok(())
    }
    fn reset(&mut self) {}
}

#[test]
fn a_boxed_audio_decoder_forwards_every_call_and_converts_refusals() {
    let mut decoder: Box<DynAudioDecoder> = Holding.boxed();
    let mut frame = AudioFrame::default();
    assert_eq!(decoder.delay(), 312);
    assert_eq!(
        decoder.decode(&[1, 2], &mut frame),
        Ok(Some(DownmixLevels::default()))
    );
    assert_eq!(frame.samples_per_channel, 2);
    assert_eq!(
        decoder.decode(&[], &mut frame),
        Err(Error::Invalid("empty access unit"))
    );
    assert_eq!(decoder.flush(&mut frame), Ok(()));
    assert_eq!(frame.samples_per_channel, 24);
}
