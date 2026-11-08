//! Serial byte fold of recorded public-API observations, independent of the workers.

use super::json::{self, Value};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Work {
    pub packets: u64,
    pub bytes: u64,
    pub digest: u64,
    pub digest_b: u64,
}

pub(super) fn integer(value: &Value, key: &str) -> Result<i128, String> {
    match value.get(key) {
        Some(Value::Number(n)) => n
            .as_i128()
            .ok_or_else(|| format!("{key}: integer required, got {}", n.text())),
        _ => Err(format!("{key}: integer required")),
    }
}

pub(super) fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key}: string required"))
}

pub(super) fn array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value], String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{key}: array required"))
}

pub(super) fn hex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("odd hexadecimal length".into());
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digit = |b: u8| match b {
                b'0'..=b'9' => Ok(b - b'0'),
                b'a'..=b'f' => Ok(b - b'a' + 10),
                _ => Err("invalid hexadecimal digit".to_owned()),
            };
            Ok((digit(pair[0])? << 4) | digit(pair[1])?)
        })
        .collect()
}

impl Work {
    pub(super) fn parse(value: &Value) -> Result<Self, String> {
        let field =
            |key| u64::try_from(integer(value, key)?).map_err(|_| format!("{key}: u64 required"));
        Ok(Self {
            packets: field("packets")?,
            bytes: field("bytes")?,
            digest: field("digest")?,
            digest_b: field("digest_b")?,
        })
    }
}

#[derive(Default)]
struct Fold(u64, u64);

impl Fold {
    fn raw(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = self.0.wrapping_mul(31).wrapping_add(u64::from(byte));
            self.1 = self.1.wrapping_mul(131).wrapping_add(u64::from(byte));
        }
    }

    fn word(&mut self, n: u64) {
        self.raw(&n.to_le_bytes());
    }

    fn field(&mut self, row: &Value, key: &str) -> Result<(), String> {
        let n = integer(row, key)?;
        if !(i128::from(i64::MIN)..=i128::from(u64::MAX)).contains(&n) {
            return Err(format!("{key}: outside a 64-bit word"));
        }
        self.word(n as u64);
        Ok(())
    }

    fn blob(&mut self, bytes: &[u8]) {
        self.word(bytes.len() as u64);
        self.raw(bytes);
    }

    fn packet(&mut self, row: &Value, kind: u64) -> Result<usize, String> {
        self.word(kind);
        for key in [
            "input_event",
            "output",
            "pts",
            "dts",
            "duration",
            "pos",
            "flags",
            "stream",
        ] {
            self.field(row, key)?;
        }
        let time_base = array(row, "time_base")?;
        if time_base.len() != 2 {
            return Err("time base requires two integers".into());
        }
        for value in time_base {
            self.field(&Value::object([("tick", value.clone())]), "tick")?;
        }
        let payload = hex(text(row, "payload")?)?;
        self.blob(&payload);
        let side_data = array(row, "side_data")?;
        self.word(side_data.len() as u64);
        for side in side_data {
            self.field(side, "type")?;
            self.blob(&hex(text(side, "bytes")?)?);
        }
        Ok(payload.len())
    }
}

pub(super) fn trace(records: &str) -> Result<Work, String> {
    let mut fold = Fold::default();
    let mut work = Work::default();
    let (mut initialized, mut closed, mut retained) = (false, false, 0u64);
    for line in records.lines() {
        let row = json::parse_strict(line).map_err(|e| e.to_string())?;
        match text(&row, "event")? {
            "initialize" => {
                if initialized || closed {
                    return Err("repeated or late initialization".into());
                }
                initialized = true;
                if integer(&row, "return")? < 0 {
                    fold.word(4);
                    fold.field(&row, "return")?;
                } else {
                    fold.word(1);
                    fold.blob(&hex(text(&row, "extradata")?)?);
                    let time_base = array(&row, "time_base")?;
                    if time_base.len() != 2 {
                        return Err("time base requires two integers".into());
                    }
                    for value in time_base {
                        fold.field(&Value::object([("tick", value.clone())]), "tick")?;
                    }
                }
            }
            "packet" => {
                if !initialized || closed || integer(&row, "output")? != i128::from(work.packets) {
                    return Err("packet outside session or out of order".into());
                }
                work.bytes += fold.packet(&row, 2)? as u64;
                work.packets += 1;
            }
            "receive" => {
                let code = integer(&row, "return")?;
                if code < 0 && ![-11, -541478725].contains(&code) {
                    fold.word(3);
                    fold.field(&row, "input_event")?;
                    fold.field(&row, "return")?;
                }
            }
            "close" => {
                if !initialized
                    || closed
                    || integer(&row, "outputs")? != i128::from(work.packets)
                    || integer(&row, "wrapper_status")? != 0
                {
                    return Err("invalid close record".into());
                }
                closed = true;
                fold.word(6);
                fold.word(work.packets);
            }
            "retained" => {
                if !closed || integer(&row, "output")? != i128::from(retained) {
                    return Err("retained packet before close or out of order".into());
                }
                fold.packet(&row, 5)?;
                retained += 1;
            }
            "allocate" | "send" | "flush" => (),
            other => return Err(format!("unknown observation event: {other}")),
        }
    }
    if !closed || retained != work.packets {
        return Err("missing close or retained packets".into());
    }
    work.digest = fold.0;
    work.digest_b = fold.1;
    Ok(work)
}

/// Each iteration contributes both whole-operation digest words as LE bytes.
pub(super) fn repeated(once: Work, iterations: u32) -> Result<Work, String> {
    let mut fold = Fold::default();
    for _ in 0..iterations {
        fold.word(once.digest);
        fold.word(once.digest_b);
    }
    Ok(Work {
        packets: once
            .packets
            .checked_mul(u64::from(iterations))
            .ok_or("packet count overflow")?,
        bytes: once
            .bytes
            .checked_mul(u64::from(iterations))
            .ok_or("byte count overflow")?,
        digest: fold.0,
        digest_b: fold.1,
    })
}
