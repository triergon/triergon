//! Scripted observations of the Store API and its budget charges.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, PoisonError};

use triergon_core::Budget;
use triergon_font::{Built, Store, StoreKey};
use triergon_harness_protocol::digest::{fold, signed};
use triergon_harness_runner::{Invocation, Mode, Totals, measure, write_summary};

use crate::Result;

const MAX_SCRIPT_BYTES: u64 = 64 << 10;
const OPERATION: &str = "font-store";

#[derive(PartialEq, Eq, Hash)]
struct Key(u32);

impl StoreKey for Key {
    type Value = Value;
}

type Drops = Arc<Mutex<Vec<u32>>>;

struct Value {
    key: u32,
    drops: Drops,
}

impl Drop for Value {
    fn drop(&mut self) {
        self.drops
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(self.key);
    }
}

fn entry_bytes() -> Result<usize> {
    let budget = Budget::new(usize::MAX)?;
    let store = Store::new(&budget);
    let before = budget.used();
    let value = store.get_or_build(Key(0), || {
        Ok(Built {
            value: Value {
                key: 0,
                drops: Drops::default(),
            },
            bytes: 0,
        })
    })?;
    let charge = budget.used() - before;
    if charge == 0 || charge != store.retained_bytes() || Arc::strong_count(&value) != 2 {
        return Err("cannot establish the zero-payload entry charge".into());
    }
    Ok(charge)
}

#[derive(Clone, Copy)]
enum Access {
    Get(u32, u32),
    Hold(u32),
    Release(u32),
}

struct Script {
    capacity: usize,
    accesses: Vec<Access>,
}

fn load(path: &str, overhead: usize) -> Result<Script> {
    let mut text = String::new();
    File::open(path)?
        .take(MAX_SCRIPT_BYTES + 1)
        .read_to_string(&mut text)?;
    if !text.is_ascii() || text.len() as u64 > MAX_SCRIPT_BYTES {
        return Err("store script exceeds its ASCII input bounds".into());
    }
    let mut lines = text.lines();
    let header = lines.next().ok_or("missing store budget")?;
    let words: Vec<_> = header.split_ascii_whitespace().collect();
    let ["budget", slots, extra] = words.as_slice() else {
        return Err("expected budget SLOTS BYTES".into());
    };
    let slots = slots.parse::<u32>()? as usize;
    let extra = extra.parse::<u32>()? as usize;
    let capacity = slots
        .checked_mul(overhead)
        .and_then(|n| n.checked_add(extra))
        .and_then(|n| n.checked_add(Budget::control_storage_bytes()))
        .ok_or("store budget overflows")?;
    let mut accesses = Vec::new();
    for (index, line) in lines.enumerate() {
        let words: Vec<_> = line.split_ascii_whitespace().collect();
        let access = match words.as_slice() {
            ["get", key, bytes] => Access::Get(key.parse()?, bytes.parse()?),
            ["hold", key] => Access::Hold(key.parse()?),
            ["release", key] => Access::Release(key.parse()?),
            [] => continue,
            _ => return Err(format!("invalid store access on line {}", index + 2).into()),
        };
        accesses.push(access);
    }
    Ok(Script { capacity, accesses })
}

#[derive(Clone, Copy)]
enum Outcome {
    Hit,
    Kept,
    Unkept,
    Held,
    Missing,
    Released,
    Unheld,
}

impl Outcome {
    const fn name(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Kept => "built-kept",
            Self::Unkept => "built-unkept",
            Self::Held => "held",
            Self::Missing => "missing",
            Self::Released => "released",
            Self::Unheld => "unheld",
        }
    }
}

fn observe(
    store: &Store,
    held: &mut BTreeMap<u32, Arc<Value>>,
    drops: &Drops,
    access: Access,
) -> Result<(u32, Outcome)> {
    Ok(match access {
        Access::Get(key, bytes) => {
            let mut built = false;
            let value = store.get_or_build(Key(key), || {
                built = true;
                Ok(Built {
                    value: Value {
                        key,
                        drops: Arc::clone(drops),
                    },
                    bytes: bytes as usize,
                })
            })?;
            let held_here = held.get(&key).is_some_and(|pin| Arc::ptr_eq(&value, pin));
            let kept = Arc::strong_count(&value) > 1 + usize::from(held_here);
            let outcome = match (built, kept) {
                (false, true) => Outcome::Hit,
                (true, true) => Outcome::Kept,
                (true, false) => Outcome::Unkept,
                (false, false) => {
                    return Err("lookup hit returned a value outside the store".into());
                }
            };
            drop(value);
            if matches!(outcome, Outcome::Unkept) {
                // The caller's drop of an unkept result follows any evictions.
                if drops.lock().unwrap_or_else(PoisonError::into_inner).pop() != Some(key) {
                    return Err("unkept store value did not drop".into());
                }
            }
            (key, outcome)
        }
        Access::Hold(key) => {
            let outcome = if let Some(value) = store.get(&Key(key)) {
                held.insert(key, value);
                Outcome::Held
            } else {
                Outcome::Missing
            };
            (key, outcome)
        }
        Access::Release(key) => {
            let outcome = if held.remove(&key).is_some() {
                Outcome::Released
            } else {
                Outcome::Unheld
            };
            (key, outcome)
        }
    })
}

fn execute(script: &Script, overhead: usize, mut trace: Option<&mut dyn Write>) -> Result<Totals> {
    let budget = Budget::new(script.capacity)?;
    let store = Store::new(&budget);
    let drops = Drops::default();
    let mut held = BTreeMap::new();
    let mut entries = 0u64;
    let mut totals = Totals::default();
    for &access in &script.accesses {
        let (key, outcome) = observe(&store, &mut held, &drops, access)?;
        let mut dropped = drops.lock().unwrap_or_else(PoisonError::into_inner);
        entries += u64::from(matches!(outcome, Outcome::Kept));
        entries = entries
            .checked_sub(dropped.len() as u64)
            .ok_or("more store values dropped than were kept")?;
        let used = budget.used() - Budget::control_storage_bytes();
        let payload = i64::try_from(used)? - i64::try_from(entries * overhead as u64)?;
        let command = match access {
            Access::Get(..) => 0,
            Access::Hold(_) => 1,
            Access::Release(_) => 2,
        };
        let fixed = [
            3,
            command,
            u64::from(key),
            outcome as u64,
            dropped.len() as u64,
        ];
        for word in fixed {
            fold(&mut totals.digest, word);
        }
        for &victim in &*dropped {
            fold(&mut totals.digest, u64::from(victim));
        }
        fold(&mut totals.digest, entries);
        fold(&mut totals.digest, signed(payload));
        totals.packets += 1;

        if let Some(out) = trace.as_deref_mut() {
            let name = ["get", "hold", "release"][command as usize];
            write!(out, "3 {name} {key} {} {}", outcome.name(), dropped.len())?;
            for victim in &*dropped {
                write!(out, " {victim}")?;
            }
            writeln!(out, " {entries} {payload}")?;
        }
        dropped.clear();
    }
    Ok(totals)
}

pub(super) fn run(args: &[String]) -> Result<()> {
    if args == ["store-overhead"] {
        println!("{}", entry_bytes()?);
        return Ok(());
    }
    let invocation = Invocation::parse(args, "store-", " ORACLE_ENTRY_BYTES", 1..=1, true)?;
    invocation.arguments[0].parse::<NonZeroU32>()?;
    let overhead = entry_bytes()?;
    let script = load(invocation.path, overhead)?;
    if invocation.mode == Mode::Trace {
        let mut out = BufWriter::new(io::stdout().lock());
        let mut totals = execute(&script, overhead, Some(&mut out))?;
        let mut digest = 0;
        fold(&mut digest, totals.digest);
        totals.digest = digest;
        return Ok(write_summary(&mut out, OPERATION, 1, 0, &totals)?);
    }
    measure(&invocation, OPERATION, None, || {
        execute(&script, overhead, None)
    })
}
