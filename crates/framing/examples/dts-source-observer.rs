//! Diagnostic source observer; no timing, native dependency or native answers.
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, SourceId, Step, WorkBudget};
use triergon_framing::{
    Dts14BitUnpacker, DtsCoreField, DtsHeaderInspector, DtsInspectionIdentity,
    DtsInspectionSourceStatus, DtsPackingLimits, DtsWordOrder, FieldObservation,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn decode(text: &str) -> Result<Vec<u8>, Error> {
    if text == "~" {
        return Ok(Vec::new());
    }
    if text.len() > 8192 || text.len() % 2 != 0 || !text.is_ascii() {
        return Err(Error::Invalid("diagnostic hex input"));
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            u8::from_str_radix(
                std::str::from_utf8(pair).map_err(|_| Error::Invalid("hex"))?,
                16,
            )
            .map_err(|_| Error::Invalid("hex"))
        })
        .collect()
}
fn run() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4
        || args[2].is_empty()
        || !args[2]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_:-./".contains(&byte))
    {
        return Err(Error::Invalid("mode id hex arguments"));
    }
    let raw = decode(&args[3])?;
    let pool = Budget::new(1 << 20)?;
    let source = Bytes::copy_from_slice(&pool, &raw)?;
    let identity = DtsInspectionIdentity {
        source: SourceId(1414),
        generation: 1,
        offset: 0,
    };
    let cancel = AtomicBool::new(false);
    if args[1] == "inspect" {
        let mut job = DtsHeaderInspector::new(&pool, identity, source, 4096)?;
        let output = loop {
            match job.step(&mut WorkBudget::new(64), &cancel)? {
                Step::Ready(value) => break value,
                Step::Yield => {}
                _ => return Err(Error::Invalid("diagnostic inspection state")),
            }
        };
        let status = match output.source_status() {
            DtsInspectionSourceStatus::NormalHeader => "ok",
            DtsInspectionSourceStatus::Invalid => "invalid",
            DtsInspectionSourceStatus::Truncated => "truncated",
            DtsInspectionSourceStatus::Unsupported => "unsupported",
        };
        print!(
            "{{\"schema\":1,\"operation\":\"dts-core-source-inspect-v1\",\"id\":\"{}\",\"logical_hex\":\"{}\",\"logical_size\":{},\"source_status\":\"{}\",\"issues\":{},\"fields\":{{",
            args[2],
            hex(output.source_bytes().as_ref()),
            raw.len(),
            status,
            output.issues().bits()
        );
        let names = [
            "sync",
            "normal",
            "short",
            "cpf",
            "nblks",
            "fsize",
            "amode",
            "sfreq",
            "rate",
            "fixed",
            "dynamic",
            "timestamp",
            "auxiliary",
            "hdcd",
            "extension_id",
            "extension",
            "audio_sync",
            "lfe",
            "history",
            "hcrc",
            "filter",
            "version",
            "copy_history",
            "pcm_resolution",
            "sum_front",
            "sum_surround",
            "dialog_word",
        ];
        for (index, (name, field)) in names.into_iter().zip(DtsCoreField::ALL).enumerate() {
            if index != 0 {
                print!(",");
            }
            print!("\"{name}\":");
            match output.field(field) {
                FieldObservation::NotReached => print!("{{\"kind\":\"not-reached\"}}"),
                FieldObservation::NotInSyntax => print!("{{\"kind\":\"not-in-syntax\"}}"),
                FieldObservation::Bits {
                    offset,
                    width,
                    observed_bits,
                    prefix,
                } => {
                    let value = output
                        .field(field)
                        .value()
                        .map_or_else(|| "null".to_owned(), |value| value.to_string());
                    print!(
                        "{{\"kind\":\"bits\",\"offset\":{offset},\"width\":{width},\"observed_bits\":{observed_bits},\"prefix\":{prefix},\"value\":{value}}}"
                    );
                }
            }
        }
        println!("}},\"qualification\":false}}");
    } else {
        let order = match args[1].as_str() {
            "unpack14-be" => DtsWordOrder::BigEndian,
            "unpack14-le" => DtsWordOrder::LittleEndian,
            _ => return Err(Error::Invalid("diagnostic mode")),
        };
        let mut job = Dts14BitUnpacker::new(
            &pool,
            identity,
            source,
            order,
            DtsPackingLimits {
                maximum_source_bytes: 4096,
                maximum_output_bytes: 4096,
            },
        )?;
        let output = loop {
            match job.step(&mut WorkBudget::new(64), &cancel)? {
                Step::Ready(value) => break value,
                Step::Yield => {}
                _ => return Err(Error::Invalid("diagnostic unpack state")),
            }
        };
        println!(
            "{{\"schema\":1,\"operation\":\"dts-low14-word-payload-v1\",\"id\":\"{}\",\"mode\":\"{}\",\"logical_hex\":\"{}\",\"logical_size\":{},\"canonical_hex\":\"{}\",\"payload_bits\":{},\"residual_bits\":{},\"residual_value\":{},\"qualification\":false}}",
            args[2],
            args[1],
            hex(output.source_bytes().as_ref()),
            raw.len(),
            hex(output.canonical_bytes().as_ref()),
            output.payload_bits(),
            output.residual_bits(),
            output.residual_value()
        );
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
