//! Compare the text handoff's source ownership and restart contract.

use triergon_core::{Error, container::PacketInfo};
use triergon_harness_protocol::md5;
use triergon_playback::Output;

use super::{
    Budget, Bytes, CANCEL, Case, Codec, Event, GRANT, ID, Invocation, MAX_INPUT, Mode, Mp4Event,
    Records, Result, TextBridge, TextSource, TextStep, Totals, WorkBudget, measure, next, player,
    text,
};

fn pending_packet(case: &Case<'_>, pool: &Budget, bytes: &Bytes) -> Result<(Output, bool)> {
    let mut player = player(case, pool, bytes)?;
    let mut refused_event = false;
    while let Some(output) = next(&mut player)? {
        if output.packet().is_none() {
            let source = output.source_id();
            let refusal = TextSource::from_output(output)
                .err()
                .ok_or("accepted an event without a complete text packet")?;
            if refusal.error != Error::Invalid("text handoff requires complete source packet")
                || refusal.output.source_id() != source
                || refusal.output.packet().is_some()
            {
                return Err("text handoff did not return the original non-packet event".into());
            }
            refused_event = true;
            continue;
        }
        let is_text = match output.event() {
            Event::Mp4(Mp4Event::Packet(packet)) => {
                packet.configuration.description.format == *b"tx3g"
            }
            Event::Matroska(triergon_matroska::Event::Packet(packet)) => {
                packet.configuration.tracks()[packet.packet.track]
                    .codec
                    .as_ref()
                    == "S_TEXT/UTF8"
            }
            _ => false,
        };
        if is_text {
            return Ok((output, refused_event));
        }
    }
    Err("text lifecycle control has no packet".into())
}

fn validate_source(
    source: &TextSource,
    codec: Codec,
    track: u64,
    generation: u64,
    payload: [u8; 16],
) -> Result<()> {
    let (id, epoch, bytes) = match (codec, source.mp4(), source.matroska()) {
        (Codec::Tx3g, Some(packet), None) => (
            u64::from(packet.configuration.track.track_id),
            packet.source.token.generation(),
            packet.source.payload.as_ref(),
        ),
        (Codec::Utf8, None, Some(packet)) => (
            packet.configuration.tracks()[packet.packet.track].number,
            packet.configuration.generation(),
            packet.packet.data.as_ref(),
        ),
        _ => return Err("text handoff changed the packet variant".into()),
    };
    if id != track || epoch != generation || md5::digest(bytes) != payload {
        return Err("text handoff changed the original packet".into());
    }
    Ok(())
}

fn step(bridge: &mut TextBridge) -> Result<TextStep> {
    loop {
        let result = bridge.step(&mut WorkBudget::new(GRANT), &CANCEL)?;
        if !matches!(result, TextStep::Yield) {
            return Ok(result);
        }
    }
}

fn check(case: &Case<'_>, pool: &Budget, bytes: &Bytes, trace: bool) -> Result<Totals> {
    let mut records = Records {
        totals: Totals {
            packets: 0,
            bytes: 0,
            digest: 0,
            refusal: None,
            unsupported: None,
        },
        trace,
    };
    let (output, refused_event) = pending_packet(case, pool, bytes)?;
    let generation = output.generation();
    let packet = output.packet().ok_or("missing selected text packet")?;
    let track = packet.track().0;
    let payload = md5::digest(packet.payload());
    let source = TextSource::from_output(output).map_err(|failure| failure.error)?;
    validate_source(&source, case.codec, track, generation, payload)?;
    let format = if case.codec == Codec::Tx3g { 1 } else { 2 };
    records.record(1, &[format, u64::from(refused_event)], &[]);

    let mut bridge = TextBridge::new(pool, ID, track, generation, text::Limits::default());
    bridge
        .provide(source.clone())
        .map_err(|failure| failure.error)?;
    let new_generation = generation.checked_add(1).ok_or(Error::Overflow)?;
    bridge.reset(new_generation);
    let refusal = bridge
        .provide(source.clone())
        .err()
        .ok_or("reset accepted a packet from a stale generation")?;
    if refusal.error
        != (Error::StaleGeneration {
            expected: new_generation,
            received: generation,
        })
    {
        return Err(format!("unexpected stale packet refusal: {}", refusal.error).into());
    }
    validate_source(&refusal.source, case.codec, track, generation, payload)?;
    bridge.finish();
    if !matches!(step(&mut bridge)?, TextStep::Finished) {
        return Err("reset did not discard the pending text packet".into());
    }
    records.record(2, &[1], &[]);

    bridge.reset(generation);
    bridge.provide(source).map_err(|failure| failure.error)?;
    let TextStep::Cue(output) = step(&mut bridge)? else {
        return Err("reset did not reopen the text bridge".into());
    };
    validate_source(&output.source, case.codec, track, generation, payload)?;
    if output.cue.input.generation != generation
        || md5::digest(&output.cue.input.payload) != payload
        || !matches!(step(&mut bridge)?, TextStep::NeedPacket)
    {
        return Err("restarted text bridge changed the packet or cue count".into());
    }
    records.record(3, &[1], &[]);

    let mut input = output.cue.input;
    input.generation = new_generation;
    input.sequence = 22;
    let document = TextSource::document(pool, input);
    if document.mp4().is_some() || document.matroska().is_some() {
        return Err("document exposes a container packet".into());
    }
    bridge.reset(new_generation);
    bridge.provide(document).map_err(|failure| failure.error)?;
    let TextStep::Cue(output) = step(&mut bridge)? else {
        return Err("text document did not emit a cue".into());
    };
    if output.source.mp4().is_some()
        || output.source.matroska().is_some()
        || output.cue.input.generation != new_generation
        || output.cue.input.sequence != 22
        || md5::digest(&output.cue.input.payload) != payload
    {
        return Err("document lost its input identity".into());
    }
    records.record(4, &[output.cue.input.sequence], &[]);
    Ok(records.totals)
}

pub(super) fn run(args: &[String]) -> Result<()> {
    let invocation = Invocation::parse(
        args,
        "text-control-",
        " text CONTAINER CODEC 0 0",
        5..=5,
        true,
    )?;
    let case = Case::parse(invocation.arguments, true)?;
    if !matches!(case.codec, Codec::Tx3g | Codec::Utf8) || case.ordinal != 0 {
        return Err("text lifecycle needs the first tx3g or Matroska UTF-8 track".into());
    }
    let pool = Budget::new(64 << 20)?;
    let bytes =
        Bytes::try_from_vec(&pool, invocation.input(MAX_INPUT)?).map_err(|error| error.error)?;
    let trace = invocation.mode == Mode::Trace;
    measure(&invocation, "bridge-text-control", Some(&pool), || {
        check(&case, &pool, &bytes, trace)
    })
}
