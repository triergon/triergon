use std::collections::BTreeMap;

use super::*;

enum Allocation {
    Reservation(Reservation),
    Bytes(Bytes),
    Vector(BudgetVec<u8>),
}

fn verdict(result: Result<(), Error>) -> u64 {
    match result {
        Ok(()) => 0,
        Err(Error::Limit { .. }) => 1,
        Err(Error::Backpressure(_)) => 2,
        Err(Error::Invalid(_)) => 3,
        Err(Error::Overflow) => 4,
        Err(_) => 5,
    }
}

fn step<'a>(
    pool: &Budget,
    live: &mut BTreeMap<&'a str, Allocation>,
    fields: &[&'a str],
) -> Result<u64, String> {
    let result = match fields {
        ["reserve", name, count] => pool
            .reserve(number(Some(count))?)
            .map(|charge| live.insert(*name, Allocation::Reservation(charge))),
        ["bytes", name, count] => {
            let count: usize = number(Some(count))?;
            Bytes::copy_from_slice(pool, &vec![0x5a; count])
                .map(|value| live.insert(*name, Allocation::Bytes(value)))
        }
        ["vector", name] => Ok(live.insert(*name, Allocation::Vector(BudgetVec::new(pool)))),
        ["release", name] if live.contains_key(name) => Ok(live.remove(name)),
        ["resize", name, count] => {
            let Some(Allocation::Reservation(charge)) = live.get_mut(name) else {
                return Err("resize needs a reservation".into());
            };
            charge.resize_to(number(Some(count))?).map(|()| None)
        }
        ["grow", name, count] => {
            let Some(Allocation::Vector(vector)) = live.get_mut(name) else {
                return Err("grow needs a vector".into());
            };
            vector
                .try_reserve_exact(number(Some(count))?)
                .map(|()| None)
        }
        ["fill", name, count] => {
            let Some(Allocation::Vector(vector)) = live.get_mut(name) else {
                return Err("fill needs a vector".into());
            };
            vector
                .try_extend_from_slice(&vec![0x5a; number(Some(count))?])
                .map(|()| None)
        }
        ["clone", name, source] => {
            let copy = match live.get(source) {
                Some(Allocation::Bytes(bytes)) => Ok(Allocation::Bytes(bytes.clone())),
                Some(Allocation::Vector(vector)) => vector.try_clone().map(Allocation::Vector),
                _ => return Err("clone needs bytes or a vector".into()),
            };
            copy.map(|copy| live.insert(*name, copy))
        }
        ["split", name, source, start, end] => {
            let Some(Allocation::Bytes(bytes)) = live.get(source) else {
                return Err("split needs bytes".into());
            };
            bytes
                .slice(number(Some(start))?..number(Some(end))?)
                .map(|part| live.insert(*name, Allocation::Bytes(part)))
        }
        ["freeze", name] => {
            let Some(Allocation::Vector(vector)) = live.remove(name) else {
                return Err("freeze needs a vector".into());
            };
            match vector.freeze() {
                Ok(bytes) => Ok(live.insert(*name, Allocation::Bytes(bytes))),
                Err(refused) => {
                    live.insert(*name, Allocation::Vector(refused.value));
                    Err(refused.error)
                }
            }
        }
        _ => return Err(format!("invalid storage step: {}", fields.join(" "))),
    };
    Ok(verdict(result.map(drop)))
}

pub(super) fn prepare(input: &[u8]) -> Result<Operation<'_>, String> {
    let mut lines = text(input)?.lines();
    let header: Vec<_> = lines
        .next()
        .ok_or("missing budget script header")?
        .split_ascii_whitespace()
        .collect();
    let [kind @ ("storage" | "owner" | "bare" | "work"), limit] = header.as_slice() else {
        return Err("invalid budget script header".into());
    };
    let limit: u64 = number(Some(limit))?;
    let kind = *kind;
    let steps: Vec<Vec<_>> = lines
        .map(|line| line.split_ascii_whitespace().collect())
        .collect();
    Ok(Operation {
        name: "budget",
        run: Box::new(move |work, mut trace| {
            if kind == "work" {
                let mut budget = WorkBudget::new(limit);
                for step in &steps {
                    let outcome = match step.as_slice() {
                        ["charge", n] => budget.take(number(Some(n))?),
                        ["try_charge", n] => u64::from(budget.try_charge(number(Some(n))?)),
                        _ => return Err("invalid work budget step".into()),
                    };
                    row(
                        &mut trace,
                        work,
                        3,
                        &[outcome, budget.spent(), budget.remaining()],
                        &[],
                    )?;
                }
                return Ok(());
            }

            let limit = usize::try_from(limit).map_err(|_| "storage limit exceeds usize")?;
            let control = Budget::control_storage_bytes();
            let owner = if kind == "owner" {
                Bytes::owner_overhead_bytes()
            } else {
                0
            };
            let limit = if kind == "bare" {
                limit
            } else {
                limit
                    .checked_add(control + owner)
                    .ok_or("storage budget overflow")?
            };
            let budget = match Budget::new(limit) {
                Ok(budget) => budget,
                Err(error) => return row(&mut trace, work, 8, &[verdict(Err(error))], &[]),
            };
            let mut live = BTreeMap::new();
            let shared_pool = budget.clone();
            row(
                &mut trace,
                work,
                3,
                &[
                    u64::from(budget.same_pool(&shared_pool)),
                    0,
                    (limit - control - owner) as u64,
                ],
                &[],
            )?;
            for fields in &steps {
                let outcome = step(&shared_pool, &mut live, fields)?;
                let has_owner = live
                    .values()
                    .any(|value| matches!(value, Allocation::Bytes(_)));
                // Owner scripts keep at most one backing allocation. Check the owner
                // charge while projecting the target-dependent header out of the trace.
                let live_header = if has_owner { owner } else { 0 };
                let charged = budget
                    .used()
                    .checked_sub(control + live_header)
                    .ok_or("missing control or shared-owner charge")?;
                let remaining = budget
                    .available()
                    .checked_sub(owner - live_header)
                    .ok_or("storage charge consumed the reserved owner header")?;
                row(
                    &mut trace,
                    work,
                    3,
                    &[outcome, charged as u64, remaining as u64],
                    &[],
                )?;
            }
            Ok(())
        }),
    })
}
