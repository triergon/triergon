use super::{Context, Outcome, Scratch, TIMEOUT, argv, finish};

pub(super) fn opus_recovery_checks_roll_maps(cx: &mut Context<'_>) -> Outcome {
    let seed = include_bytes!(
        "../../../crates/isobmff/tests/fixtures/range-encoded-edits/avc-opus-edit.mp4"
    );
    let scratch = Scratch::new()?;
    let complete = scratch.file("complete-roll.mp4", seed)?;
    let mut partial = seed.to_vec();
    cx.same(
        &partial[2902..2906],
        &157_u32.to_be_bytes()[..],
        "final roll run",
    )?;
    partial[2902..2906].copy_from_slice(&156_u32.to_be_bytes());
    let partial = scratch.file("partial-roll.mp4", &partial)?;
    let mut flagged = seed.to_vec();
    flagged[2885] = 1;
    let flagged = scratch.file("flagged-roll.mp4", &flagged)?;
    let mut parameter = seed.to_vec();
    parameter[2874..2878].copy_from_slice(&32_u32.to_be_bytes());
    parameter[2882] = 1;
    for (at, value) in [(2890, 1_u32), (2894, 1), (2898, 161), (2902, 1)] {
        parameter[at..at + 4].copy_from_slice(&value.to_be_bytes());
    }
    // Fill the remainder of the movie with one padding box so the chunk
    // offsets stay valid.
    for at in [1384, 1520, 1605, 1665] {
        let size = u32::from_be_bytes(seed[at..at + 4].try_into().expect("box size"));
        parameter[at..at + 4].copy_from_slice(&(size - 4).to_be_bytes());
    }
    parameter[2906..2971].fill(0);
    parameter[2906..2910].copy_from_slice(&65_u32.to_be_bytes());
    parameter[2910..2914].copy_from_slice(b"free");
    let parameter = scratch.file("parameterised-roll.mp4", &parameter)?;
    for (binary, refusal, accepts_partial) in [
        (
            cx.root
                .join(crate::harness::gate_build::release_dir()?)
                .join("playback-worker"),
            2,
            true,
        ),
        (
            cx.root.join("target/harness/ffmpeg-formats-worker"),
            1,
            false,
        ),
    ] {
        let args: &[&dyn std::fmt::Display] = &[&32_000, &"opus-roll"];
        let accepted = finish(&argv(&binary, "mp4-range-fold", &complete, args), TIMEOUT)?;
        cx.same(
            accepted.code,
            0,
            format!("{} complete map", binary.display()),
        )?;
        let mut invalid = vec![&flagged, &parameter];
        if accepts_partial {
            // ISO/IEC 14496-12 §8.9.2.3 allows runs to cover a prefix.
            // The uncovered final sample does not change this recovery.
            let shortened = finish(&argv(&binary, "mp4-range-fold", &partial, args), TIMEOUT)?;
            cx.same(shortened.code, 0, "the candidate accepts a partial map")?;
            cx.same(&shortened.stdout, &accepted.stdout, "partial map fold")?;
            cx.same(
                &shortened.stderr,
                &accepted.stderr,
                "partial map diagnostics",
            )?;
        } else {
            invalid.push(&partial);
        }
        for input in invalid {
            let refused = finish(&argv(&binary, "mp4-range-fold", input, args), TIMEOUT)?;
            cx.same(
                refused.code,
                refusal,
                format!("{} refuses {}", binary.display(), input.display()),
            )?;
            cx.check(refused.stdout.is_empty(), || {
                format!(
                    "{} emitted a result for {}",
                    binary.display(),
                    input.display()
                )
            })?;
        }
    }
    Ok(())
}
