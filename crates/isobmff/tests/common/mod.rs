#![allow(dead_code)]
pub mod fixtures;

use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_isobmff::{BoxStep, BoxView, BuildStatus, Limits, SampleTable, SampleTableBuilder};

pub fn bytes(budget: &Budget, input: &[u8]) -> Bytes {
    Bytes::copy_from_slice(budget, input).expect("test fixture budget")
}

pub fn children(parent: &BoxView, skip: usize) -> Result<Vec<BoxView>, Error> {
    let mut cursor = parent.children(skip, Limits::default())?;
    let mut output = Vec::new();
    loop {
        match cursor.poll(&mut WorkBudget::new(7))? {
            BoxStep::Box(value) => output.push(value),
            BoxStep::End => return Ok(output),
            BoxStep::Yield => {}
        }
    }
}

pub fn child(parent: &BoxView, kind: &[u8; 4]) -> Result<BoxView, Error> {
    let mut matches = children(parent, 0)?
        .into_iter()
        .filter(|b| b.header().kind == *kind);
    let first = matches.next().ok_or(Error::Invalid("missing test box"))?;
    if matches.next().is_some() {
        return Err(Error::Invalid("duplicate test box"));
    }
    Ok(first)
}

pub fn roots(budget: &Budget, input: &[u8]) -> Result<Vec<BoxView>, Error> {
    let storage = bytes(budget, input);
    let mut output = Vec::new();
    let mut position = 0usize;
    while position < storage.len() {
        let end = u64::try_from(storage.len()).map_err(|_| Error::Overflow)?;
        let status = triergon_isobmff::BoxHeader::parse(
            &storage[position..],
            position as u64,
            Some(end),
            true,
        )?;
        let triergon_isobmff::HeaderRead::Ready(header) = status else {
            return Err(Error::Truncated);
        };
        let count =
            usize::try_from(header.size.ok_or(Error::Truncated)?).map_err(|_| Error::Overflow)?;
        let next = position.checked_add(count).ok_or(Error::Overflow)?;
        output.push(BoxView::new(
            storage.slice(position..next)?,
            position as u64,
        )?);
        position = next;
    }
    Ok(output)
}

pub fn table(raw: BoxView, budget: &Budget, quantum: u64) -> Result<SampleTable, Error> {
    let mut builder = SampleTableBuilder::new(raw, budget.clone(), Limits::default())?;
    for _ in 0..100_000 {
        if builder.poll(&mut WorkBudget::new(quantum))? == BuildStatus::Ready {
            return builder.finish();
        }
    }
    panic!("fixture builder failed to make bounded progress");
}
