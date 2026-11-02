//! Line-oriented evaluation adapter for the independent Python integer oracle.
#![forbid(unsafe_code)]
#[path = "../src/wide.rs"]
mod wide;
use safe_media_core::{Error, Result, Rounding, Time, TimeBase};
use std::io::{self, BufRead, Write};
use wide::Wide;

fn number<T: std::str::FromStr>(value: &str) -> T {
    value
        .parse()
        .unwrap_or_else(|_| panic!("invalid oracle number: {value}"))
}
fn mode(value: &str) -> Rounding {
    match value {
        "TowardZero" => Rounding::TowardZero,
        "AwayFromZero" => Rounding::AwayFromZero,
        "Floor" => Rounding::Floor,
        "Ceil" => Rounding::Ceil,
        "NearestAway" => Rounding::NearestAway,
        _ => panic!("invalid oracle rounding"),
    }
}
fn ordering(value: std::cmp::Ordering) -> &'static str {
    match value {
        std::cmp::Ordering::Less => "less",
        std::cmp::Ordering::Equal => "equal",
        std::cmp::Ordering::Greater => "greater",
    }
}
fn clock(args: &[&str]) -> Result<Time> {
    Time::new(number(args[0]), number(args[1]))
}
fn base(args: &[&str]) -> Result<TimeBase> {
    TimeBase::new(number(args[0]), number(args[1]))
}
fn rational(result: Result<Time>) -> Result<String> {
    result.map(|v| format!("{}\t{}", v.numerator(), v.denominator()))
}
fn build(args: &[&str]) -> Wide {
    let half = Wide::product(number(args[1]), 1_u128 << 127);
    half.checked_add(half)
        .unwrap()
        .checked_add(Wide::from_u128(number(args[0])))
        .unwrap()
}
fn evaluate(op: &str, a: &[&str]) -> Result<String> {
    match op {
        "new" => rational(clock(a)),
        "base" => base(a).map(|v| format!("{}\t{}", v.numerator(), v.denominator())),
        "from_ticks" => rational(Time::from_ticks(number(a[0]), base(&a[1..])?)),
        "add" => rational(clock(a)?.checked_add(clock(&a[2..])?)),
        "sub" => rational(clock(a)?.checked_sub(clock(&a[2..])?)),
        "cmp" => Ok(ordering(clock(a)?.cmp(&clock(&a[2..])?)).to_owned()),
        "eq" => Ok(u8::from(clock(a)? == clock(&a[2..])?).to_string()),
        "ticks128" => clock(a)?
            .to_ticks(base(&a[2..])?, mode(a[4]))
            .map(|v| v.to_string()),
        "ticks64" => clock(a)?
            .to_i64_ticks(base(&a[2..])?, mode(a[4]))
            .map(|v| v.to_string()),
        "ns64" => clock(a)?.to_nanoseconds(mode(a[2])).map(|v| v.to_string()),
        "rescale64" => base(&a[1..])?
            .rescale(number(a[0]), base(&a[3..])?, mode(a[5]))
            .map(|v| v.to_string()),
        "sort" => {
            let mut values: Vec<(usize, Time)> = a[0]
                .split(';')
                .enumerate()
                .map(|(i, v)| {
                    let (n, d) = v.split_once('/').unwrap_or((v, "1"));
                    Ok((i, Time::new(number(n), number(d))?))
                })
                .collect::<Result<_>>()?;
            values.sort_by_key(|(_, time)| *time);
            Ok(values
                .iter()
                .map(|(i, _)| i.to_string())
                .collect::<Vec<_>>()
                .join(","))
        }
        "w_product" => Ok(format!("{:?}", Wide::product(number(a[0]), number(a[1])))),
        "w_build" => Ok(format!("{:?}", build(a))),
        "w_to128" => build(a)
            .to_u128()
            .ok_or(Error::Overflow)
            .map(|v| v.to_string()),
        "w_add" => build(a)
            .checked_add(build(&a[2..]))
            .ok_or(Error::Overflow)
            .map(|v| format!("{v:?}")),
        "w_sub" => Ok(format!("{:?}", build(a).sub(build(&a[2..])))),
        "w_cmp" => Ok(ordering(build(a).cmp(&build(&a[2..]))).to_owned()),
        "w_div" => {
            let (q, r) = build(a).div_rem(build(&a[2..]));
            Ok(format!("{q:?}\t{r:?}"))
        }
        "w_gcd" => Ok(format!("{:?}", build(a).gcd(build(&a[2..])))),
        _ => panic!("unknown oracle operation: {op}"),
    }
}
fn main() -> io::Result<()> {
    let mut output = io::BufWriter::new(io::stdout().lock());
    for line in io::stdin().lock().lines() {
        let line = line?;
        let fields: Vec<_> = line.split('\t').collect();
        let id = fields.first().expect("oracle case id");
        let value = evaluate(fields[1], &fields[2..]);
        match value {
            Ok(value) => writeln!(output, "{id}\tok\t{value}")?,
            Err(Error::Overflow) => writeln!(output, "{id}\toverflow")?,
            Err(Error::Invalid(_)) => writeln!(output, "{id}\tinvalid")?,
            Err(error) => panic!("unexpected oracle result: {error}"),
        }
    }
    Ok(())
}
