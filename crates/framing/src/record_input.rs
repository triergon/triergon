//! Private bounded feeding of an already owned codec record. No I/O or codec
//! interpretation. Source offsets and tokens remain those of the core cursor.
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputRequest, InputWindow, Result, SourceCapabilities,
    SourceId,
};

pub(crate) fn check_record(
    pool: &Budget,
    record: &Bytes,
    minimum: usize,
    maximum: usize,
) -> Result<()> {
    if maximum == 0 {
        return Err(Error::Invalid("zero codec record limit"));
    }
    if !record.belongs_to(pool) {
        return Err(Error::ForeignPool);
    }
    if record.len() > maximum {
        return Err(Error::Limit {
            requested: record.len(),
            maximum,
        });
    }
    if record.len() < minimum {
        return Err(Error::Truncated);
    }
    Ok(())
}

pub(crate) fn cursor(
    pool: &Budget,
    source: SourceId,
    record: &Bytes,
    window: usize,
) -> Result<InputCursor> {
    InputCursor::new(
        pool,
        source,
        SourceCapabilities {
            length: Some(u64::try_from(record.len()).map_err(|_| Error::Overflow)?),
            random_access: false,
        },
        window,
    )
}

pub(crate) fn window(record: &Bytes, request: &InputRequest) -> Result<Option<InputWindow>> {
    let start = usize::try_from(request.offset).map_err(|_| Error::Overflow)?;
    let remaining = record
        .len()
        .checked_sub(start)
        .ok_or(Error::Invalid("record input offset"))?;
    if remaining == 0 {
        return Ok(None);
    }
    let count = remaining.min(request.maximum);
    if count == 0 {
        return Err(Error::Invalid("zero record input request"));
    }
    let end = start.checked_add(count).ok_or(Error::Overflow)?;
    Ok(Some(InputWindow::new(
        request.source,
        request.token.clone(),
        request.offset,
        record.slice(start..end)?,
    )?))
}
