//! Capture case options shared by the correctness, timing and memory commands.

use crate::json::Value;

pub(super) struct Config<'a> {
    pub gop: Option<u32>,
    pub max_rate: Option<u32>,
    pub vbv_bits: Option<u32>,
    pub mux: Option<Mux<'a>>,
}

pub(super) struct Mux<'a> {
    pub transport_stream_id: u16,
    pub original_network_id: u16,
    pub program_number: u16,
    pub pmt_pid: u16,
    pub video_pid: u16,
    pub stream_type: u8,
    pub provider_name: &'a str,
    pub service_name: &'a str,
}

fn number<T: TryFrom<i128>>(value: &Value, key: &str) -> Result<Option<T>, String> {
    value
        .get(key)
        .map(|value| {
            let number = match value {
                Value::Number(number) => number.as_i128(),
                _ => None,
            };
            number
                .and_then(|n| T::try_from(n).ok())
                .ok_or_else(|| format!("capture case {key}: invalid integer"))
        })
        .transpose()
}

impl<'a> Config<'a> {
    pub fn read(case: &'a Value) -> Result<Self, String> {
        Ok(Self {
            gop: number(case, "gop")?,
            max_rate: number(case, "max_rate")?,
            vbv_bits: number(case, "vbv_bits")?,
            mux: case.get("mux").map(Mux::read).transpose()?,
        })
    }

    pub fn arguments(&self) -> Vec<String> {
        let mut args = Vec::new();
        for (key, value) in [
            ("--gop", self.gop),
            ("--max-rate", self.max_rate),
            ("--vbv-bits", self.vbv_bits),
        ] {
            if let Some(value) = value {
                args.extend([key.to_owned(), value.to_string()]);
            }
        }
        if let Some(mux) = &self.mux {
            args.extend([
                "--transport-stream-id".into(),
                mux.transport_stream_id.to_string(),
                "--original-network-id".into(),
                mux.original_network_id.to_string(),
                "--program-number".into(),
                mux.program_number.to_string(),
                "--pmt-pid".into(),
                mux.pmt_pid.to_string(),
                "--video-pid".into(),
                mux.video_pid.to_string(),
                "--stream-type".into(),
                mux.stream_type.to_string(),
                "--provider-name".into(),
                mux.provider_name.into(),
                "--service-name".into(),
                mux.service_name.into(),
            ]);
        }
        args
    }
}

impl<'a> Mux<'a> {
    fn read(value: &'a Value) -> Result<Self, String> {
        let Value::Object(fields) = value else {
            return Err("capture case mux: expected an object".into());
        };
        for (key, _) in fields {
            if !matches!(
                key.as_str(),
                "transport_stream_id"
                    | "original_network_id"
                    | "program_number"
                    | "pmt_pid"
                    | "video_pid"
                    | "stream_type"
                    | "provider_name"
                    | "service_name"
            ) {
                return Err(format!("capture case mux: unknown field {key}"));
            }
        }
        let name = |key, default| match value.get(key) {
            None => Ok(default),
            Some(value) => value
                .as_str()
                .ok_or_else(|| format!("capture case mux {key}: expected a string")),
        };
        Ok(Self {
            transport_stream_id: number(value, "transport_stream_id")?.unwrap_or(1),
            original_network_id: number(value, "original_network_id")?.unwrap_or(0xff01),
            program_number: number(value, "program_number")?.unwrap_or(1),
            pmt_pid: number(value, "pmt_pid")?.unwrap_or(0x1000),
            video_pid: number(value, "video_pid")?.unwrap_or(0x0100),
            stream_type: number(value, "stream_type")?.unwrap_or(1),
            provider_name: name("provider_name", "Triergon")?,
            service_name: name("service_name", "Service01")?,
        })
    }
}
