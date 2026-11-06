//! An independent MPEG transport stream walk and the contract's mux rules.

use std::collections::BTreeMap;

const PACKET: usize = 188;

pub struct Pes {
    pub pid: u16,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub pcr: Option<i64>,
    pub rai: bool,
    pub stream_id: u8,
    pub length: u16,
    pub data: Vec<u8>,
}

pub struct Table {
    pub pid: u16,
    pub section: String,
    pub pes_index: usize,
}

/// Facts of one transport stream: walk errors, PES packets, PSI sections.
pub struct Facts {
    pub errors: Vec<String>,
    pub pes: Vec<Pes>,
    pub tables: Vec<Table>,
    pub packets: usize,
}

fn at(data: &[u8], index: usize) -> Result<u8, String> {
    data.get(index)
        .copied()
        .ok_or_else(|| "index out of range".to_owned())
}

fn from(data: &[u8], start: usize) -> &[u8] {
    &data[start.min(data.len())..]
}

fn range(data: &[u8], start: usize, end: usize) -> &[u8] {
    &data[start.min(data.len())..end.min(data.len())]
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

/// A 33-bit PES timestamp.
fn timestamp(b: &[u8]) -> Result<i64, String> {
    let v = |i| at(b, i).map(i64::from);
    Ok((((v(0)? >> 1) & 7) << 30)
        | (((v(1)? << 7) | (v(2)? >> 1)) << 15)
        | ((v(3)? << 7) | (v(4)? >> 1)))
}

/// Independent TS walk: tables, PES, PCR, continuity.
pub fn parse(data: &[u8]) -> Result<Facts, String> {
    let mut facts = Facts {
        errors: Vec::new(),
        pes: Vec::new(),
        tables: Vec::new(),
        packets: data.len() / PACKET,
    };
    if !data.len().is_multiple_of(PACKET) {
        facts.errors.push("length not a multiple of 188".into());
    }
    let mut continuity: BTreeMap<u16, u8> = BTreeMap::new();
    let mut current: Option<usize> = None;
    for i in 0..data.len() / PACKET {
        let p = &data[i * PACKET..(i + 1) * PACKET];
        if p[0] != 0x47 {
            facts.errors.push(format!("sync {i}"));
            continue;
        }
        let pusi = (p[1] >> 6) & 1 == 1;
        let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
        let afc = (p[3] >> 4) & 3;
        let c = p[3] & 15;
        if afc & 1 != 0 {
            if continuity
                .get(&pid)
                .is_some_and(|&last| (last + 1) & 15 != c)
            {
                facts
                    .errors
                    .push(format!("continuity pid {pid:#x} packet {i}"));
            }
            continuity.insert(pid, c);
        }
        let mut offset = 4;
        let mut pcr = None;
        let mut rai = false;
        if afc & 2 != 0 {
            let length = usize::from(p[4]);
            offset = 5 + length;
            if length != 0 {
                let flags = p[5];
                rai = flags & 0x40 != 0;
                if flags & 0x10 != 0 {
                    let b: Vec<i64> = p[6..12].iter().map(|&x| i64::from(x)).collect();
                    let base =
                        (b[0] << 25) | (b[1] << 17) | (b[2] << 9) | (b[3] << 1) | (b[4] >> 7);
                    pcr = Some(base * 300 + (((b[4] & 1) << 8) | b[5]));
                }
            }
        }
        let payload = from(p, offset);
        if matches!(pid, 0x0000 | 0x0011 | 0x1000) {
            if pusi {
                let pointer = usize::from(at(payload, 0)?);
                let section = from(payload, 1 + pointer);
                let length =
                    (usize::from(at(section, 1)? & 0x0F) << 8) | usize::from(at(section, 2)?);
                facts.tables.push(Table {
                    pid,
                    section: hex(range(section, 0, 3 + length)),
                    pes_index: facts.pes.len(),
                });
            }
            continue;
        }
        if pusi {
            if range(payload, 0, 3) != [0, 0, 1] {
                facts.errors.push(format!("pes start code {i}"));
                continue;
            }
            let flags = at(payload, 7)?;
            let header = usize::from(at(payload, 8)?);
            let pts = if flags & 0x80 != 0 {
                Some(timestamp(range(payload, 9, 14))?)
            } else {
                None
            };
            let dts = if flags & 0x40 != 0 {
                Some(timestamp(range(payload, 14, 19))?)
            } else {
                pts
            };
            facts.pes.push(Pes {
                pid,
                pts,
                dts,
                pcr,
                rai,
                stream_id: at(payload, 3)?,
                length: (u16::from(at(payload, 4)?) << 8) | u16::from(at(payload, 5)?),
                data: from(payload, 9 + header).to_vec(),
            });
            current = Some(facts.pes.len() - 1);
        } else if let Some(index) = current
            && facts.pes[index].pid == pid
        {
            if pcr.is_some() {
                facts.errors.push(format!("pcr inside pes {i}"));
            }
            facts.pes[index].data.extend_from_slice(payload);
        }
    }
    Ok(facts)
}

/// `picture_coding_type` of the first picture header of an elementary stream.
pub fn picture_type(es: &[u8]) -> Option<u8> {
    let i = es.windows(4).position(|w| w == [0, 0, 1, 0])?;
    if i + 6 > es.len() {
        None
    } else {
        Some((es[i + 5] >> 3) & 7)
    }
}

fn optional(value: Option<i64>) -> String {
    value.map_or_else(|| "None".to_owned(), |v| v.to_string())
}

fn list(values: &[u16]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

const NONE_ARITHMETIC: &str = "unsupported operand type(s) for -: 'NoneType' and 'int'";

/// The contract's `mux` cadence and timing rules on one stream. Key pictures
/// are read from each picture header (I = 1), so scene-change I pictures count.
pub fn check_mux(facts: &Facts) -> Result<Vec<String>, String> {
    let mut problems = facts.errors.clone();
    let (mut last_pat, mut last_sdt): (Option<i64>, Option<i64>) = (None, None);
    let mut previous_key = false;
    let mut seen: BTreeMap<usize, Vec<u16>> = BTreeMap::new();
    for t in &facts.tables {
        seen.entry(t.pes_index).or_default().push(t.pid);
    }
    for (k, pes) in facts.pes.iter().enumerate() {
        let k64 = k as i64;
        let (dts, pts, pcr) = (pes.dts, pes.pts, pes.pcr);
        if dts != Some(3000 * k64) || pts != Some(3000 * (k64 + 1)) {
            problems.push(format!(
                "frame {k}: pts {} dts {}",
                optional(pts),
                optional(dts)
            ));
        }
        let dts = dts.ok_or("unsupported operand type(s) for *: 'NoneType' and 'int'")?;
        if pcr != Some(dts * 300) {
            problems.push(format!("frame {k}: pcr {}", optional(pcr)));
        }
        let key = picture_type(&pes.data) == Some(1);
        if pes.rai != key {
            problems.push(format!(
                "frame {k}: random access {}",
                if pes.rai { "True" } else { "False" }
            ));
        }
        if pes.stream_id != 0xE0 || pes.length != 0 {
            problems.push(format!("frame {k}: pes header"));
        }
        let mut want: Vec<u16> = Vec::new();
        let sdt_due = match last_sdt {
            None => true,
            Some(last) => pcr.ok_or(NONE_ARITHMETIC)? - last >= 13_500_000,
        };
        if sdt_due {
            want.push(0x11);
            last_sdt = match last_sdt {
                None => pcr,
                Some(last) => Some(last.max(pcr.ok_or(NONE_ARITHMETIC)?)),
            };
        }
        let pat_due = (key && !previous_key)
            || match last_pat {
                None => true,
                Some(last) => pcr.ok_or(NONE_ARITHMETIC)? - last >= 2_700_000,
            };
        if pat_due {
            want.extend([0x00, 0x1000]);
            last_pat = match last_pat {
                None => pcr,
                Some(last) => Some(last.max(
                    pcr.ok_or("'>' not supported between instances of 'NoneType' and 'int'")?,
                )),
            };
        }
        let tables = seen.get(&k);
        if tables.map_or(&[][..], Vec::as_slice) != want.as_slice() {
            problems.push(format!(
                "frame {k}: tables {} want {}",
                tables.map_or_else(|| "None".to_owned(), |t| list(t)),
                list(&want)
            ));
        }
        previous_key = key;
    }
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One TS packet with an adaptation field: PID, payload start, continuity,
    /// optional PCR, random access, payload.
    fn packet(
        pid: u16,
        start: bool,
        cc: u8,
        pcr: Option<i64>,
        rai: bool,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut p = vec![
            0x47,
            (u8::from(start) << 6) | (pid >> 8) as u8,
            pid as u8,
            0x30 | cc,
        ];
        let mut field = vec![if pcr.is_some() { 0x10 } else { 0 } | if rai { 0x40 } else { 0 }];
        if let Some(pcr) = pcr {
            let (base, ext) = (pcr / 300, pcr % 300);
            field.extend([
                (base >> 25) as u8,
                (base >> 17) as u8,
                (base >> 9) as u8,
                (base >> 1) as u8,
                (((base & 1) << 7) as u8) | 0x7E | ((ext >> 8) as u8),
                ext as u8,
            ]);
        }
        field.resize(PACKET - 5 - payload.len(), 0xFF);
        p.push(field.len() as u8);
        p.extend(field);
        p.extend(payload);
        assert_eq!(p.len(), PACKET);
        p
    }

    fn pes(pts: i64, dts: i64, es: &[u8]) -> Vec<u8> {
        let ts = |prefix: u8, v: i64| {
            [
                (prefix << 4) | (((v >> 30) & 7) << 1) as u8 | 1,
                (v >> 22) as u8,
                (((v >> 15) & 0x7F) << 1) as u8 | 1,
                (v >> 7) as u8,
                ((v & 0x7F) << 1) as u8 | 1,
            ]
        };
        let mut out = vec![0, 0, 1, 0xE0, 0, 0, 0x80, 0xC0, 10];
        out.extend(ts(3, pts));
        out.extend(ts(1, dts));
        out.extend(es);
        out
    }

    #[test]
    fn walks_pes_timing_and_tables() {
        let section = [0x00, 0xB0, 0x0D, 1, 2, 3];
        let mut psi = vec![0];
        psi.extend(section);
        let mut stream = packet(0x11, true, 0, None, false, &psi);
        stream.extend(packet(0, true, 0, None, false, &psi));
        stream.extend(packet(0x1000, true, 0, None, false, &psi));
        let picture = [0, 0, 1, 0, 0, 0x08];
        stream.extend(packet(
            0x100,
            true,
            0,
            Some(0),
            true,
            &pes(3000, 0, &picture),
        ));
        stream.extend(packet(0x100, false, 2, None, false, &[7; 10]));
        let facts = parse(&stream).unwrap();
        assert_eq!(facts.errors, ["continuity pid 0x100 packet 4"]);
        assert_eq!(facts.pes.len(), 1);
        assert_eq!(
            (facts.pes[0].pts, facts.pes[0].dts, facts.pes[0].pcr),
            (Some(3000), Some(0), Some(0))
        );
        assert_eq!(picture_type(&facts.pes[0].data), Some(1));
        assert_eq!(facts.tables.len(), 3);
        assert_eq!(facts.tables[0].section.len(), 2 * 6);
        assert_eq!(
            check_mux(&facts).unwrap(),
            ["continuity pid 0x100 packet 4"]
        );
    }
}
