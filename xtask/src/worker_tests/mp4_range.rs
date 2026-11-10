use super::{Context, Outcome, Scratch, TIMEOUT, argv, finish};

pub(super) fn opus_recovery_refuses_incomplete_roll_maps(cx: &mut Context<'_>) -> Outcome {
    let seed = include_bytes!(
        "../../../crates/isobmff/tests/fixtures/range-encoded-edits/avc-opus-edit.mp4"
    );
    let scratch = Scratch::new()?;
    let complete = scratch.file("complete-roll.mp4", seed)?;
    let mut incomplete = seed.to_vec();
    cx.same(
        &incomplete[2902..2906],
        &157_u32.to_be_bytes()[..],
        "final roll run",
    )?;
    incomplete[2902..2906].copy_from_slice(&156_u32.to_be_bytes());
    let incomplete = scratch.file("incomplete-roll.mp4", &incomplete)?;
    for (binary, refusal) in [
        (cx.root.join("target/release/playback-worker"), 2),
        (cx.root.join("target/harness/ffmpeg-formats-worker"), 1),
    ] {
        let args: &[&dyn std::fmt::Display] = &[&32_000, &"opus-roll"];
        let accepted = finish(&argv(&binary, "mp4-range-fold", &complete, args), TIMEOUT)?;
        cx.same(
            accepted.code,
            0,
            format!("{} complete map", binary.display()),
        )?;
        let refused = finish(&argv(&binary, "mp4-range-fold", &incomplete, args), TIMEOUT)?;
        cx.same(
            refused.code,
            refusal,
            format!("{} incomplete map", binary.display()),
        )?;
        cx.check(refused.stdout.is_empty(), || {
            format!(
                "{} emitted a result for the incomplete map",
                binary.display()
            )
        })?;
    }
    Ok(())
}
