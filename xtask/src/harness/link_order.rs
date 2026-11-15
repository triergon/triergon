//! Module order: a worker's functions laid out grouped by the item that owns them.
//!
//! The order comes from the binary's own symbol names alone. Each function is keyed
//! by its owning path: a free function by its own path, a method by its type's path
//! and name, a closure by its function's, and drop glue by the type it drops, with
//! generic arguments left out. Sorting by that path puts each crate's functions
//! together, and within a crate each module's and each type's, so the code one
//! operation runs lies in fewer of the kernel's fault-around windows. The order is
//! computed from a first link of the worker, written as a symbol ordering file, and
//! applied by linking again ([`super::gate_build::module_order_link_arguments`]);
//! ordering the second link gives the same file.

use std::path::Path;
use std::process::Command;

/// The text symbols of `binary` as `nm` lists them, in symbol-table order.
fn text_symbols(binary: &Path, demangle: bool) -> Result<Vec<String>, String> {
    let mut command = Command::new("nm");
    command.args(["--defined-only", "-p"]);
    if demangle {
        command.arg("-C");
    }
    let output = command
        .arg(binary)
        .output()
        .map_err(|error| format!("nm: {error}"))?;
    if !output.status.success() {
        return Err(format!("nm {}: {}", binary.display(), output.status));
    }
    let text = String::from_utf8(output.stdout).map_err(|error| format!("nm: {error}"))?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, ' ');
            let (_, kind, name) = (fields.next()?, fields.next()?, fields.next()?);
            matches!(kind, "t" | "T" | "w" | "W").then(|| name.to_owned())
        })
        .collect())
}

/// Code the linker makes itself where the layout needs it: branch thunks for targets
/// out of a branch's range and patches for ARM core errata (lld's `__*Thunk_*`,
/// `__CortexA53843419_*` and `__CortexA8657417_*`). Which exist depends on where
/// functions lie, so they are left out of the order.
fn linker_made(symbol: &str) -> bool {
    symbol.starts_with("__")
        && (symbol.contains("Thunk_")
            || symbol.starts_with("__CortexA53843419_")
            || symbol.starts_with("__CortexA8657417_"))
}

/// The symbol ordering file of `binary`: one symbol per line, ordered by owning
/// path, then by demangled name, then by symbol name, without the code the linker
/// makes itself.
pub fn symbol_order(binary: &Path) -> Result<String, String> {
    let symbols = text_symbols(binary, false)?;
    let names = text_symbols(binary, true)?;
    if symbols.len() != names.len() {
        return Err(format!(
            "{}: nm lists {} symbols and {} demangled names",
            binary.display(),
            symbols.len(),
            names.len()
        ));
    }
    let mut rows: Vec<(String, &str, &str)> = names
        .iter()
        .zip(&symbols)
        .filter(|(_, symbol)| !linker_made(symbol))
        .map(|(name, symbol)| (owner(name), name.as_str(), symbol.as_str()))
        .collect();
    rows.sort_unstable();
    let mut order = String::new();
    let mut previous = None;
    for (_, _, symbol) in rows {
        // A local name the binary holds more than once is one line of the file.
        if previous != Some(symbol) {
            order.push_str(symbol);
            order.push('\n');
        }
        previous = Some(symbol);
    }
    Ok(order)
}

/// The path of the item a function belongs to, from its demangled name.
fn owner(name: &str) -> String {
    for glue in ["core::ptr::drop_glue::", "core::ptr::drop_in_place::"] {
        if let Some(argument) = name.strip_prefix(glue).filter(|rest| rest.starts_with('<')) {
            let (dropped, _) = split_angle(argument);
            return format!("{}::{{drop}}", path_of(dropped));
        }
    }
    path_of(name)
}

/// The path a type or item name denotes: references, pointers, slices and trait
/// objects by what they point to, `<T as Trait>::item` by `T`'s path and the item,
/// and generic arguments left out.
fn path_of(name: &str) -> String {
    let mut name = name.trim();
    while let Some(rest) = ["&mut ", "&", "*const ", "*mut ", "dyn ", "[", "("]
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))
    {
        name = rest;
    }
    if name.starts_with('<') {
        let (inner, rest) = split_angle(name);
        return format!("{}{}", path_of(self_type(inner)), without_generics(rest));
    }
    let mut depth = 0usize;
    let end = name
        .char_indices()
        .find(|&(_, c)| {
            match c {
                '<' => depth += 1,
                '>' => depth = depth.saturating_sub(1),
                ';' | ',' | ')' | ']' | ' ' if depth == 0 => return true,
                _ => {}
            }
            false
        })
        .map_or(name.len(), |(index, _)| index);
    without_generics(&name[..end])
}

/// `<inner>rest` split at the `>` that closes the first `<`.
fn split_angle(name: &str) -> (&str, &str) {
    let mut depth = 0usize;
    for (index, c) in name.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return (&name[1..index], &name[index + 1..]);
                }
            }
            _ => {}
        }
    }
    (name.get(1..).unwrap_or_default(), "")
}

/// `T` of `T as Trait`, or the whole name of an inherent `<T>`.
fn self_type(inner: &str) -> &str {
    let mut depth = 0usize;
    for (index, c) in inner.char_indices() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth = depth.saturating_sub(1),
            ' ' if depth == 0 && inner[index..].starts_with(" as ") => return &inner[..index],
            _ => {}
        }
    }
    inner
}

/// `name` with every generic argument list left out.
fn without_generics(name: &str) -> String {
    let mut path = String::with_capacity(name.len());
    let mut depth = 0usize;
    for c in name.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => path.push(c),
            _ => {}
        }
    }
    while path.contains("::::") {
        path = path.replace("::::", "::");
    }
    path.trim_end_matches(':').to_owned()
}

#[cfg(test)]
mod tests {
    use super::{linker_made, owner};

    #[test]
    fn code_the_linker_makes_for_the_layout_is_not_ordered() {
        for symbol in [
            "__CortexA53843419_C3008",
            "__ThumbV7PILongThunk_malloc",
            "__AArch64ADRPThunk_memcpy",
            "__ARMv7PILongThunk__RNvCs1_4core3fmt5write",
        ] {
            assert!(linker_made(symbol), "{symbol}");
        }
        for symbol in ["__aeabi_uidiv", "__libc_csu_init", "_RNvCs1_7example4main"] {
            assert!(!linker_made(symbol), "{symbol}");
        }
    }

    #[test]
    fn functions_belong_to_their_item_without_generic_arguments() {
        for (name, expected) in [
            (
                "playback_worker::ts_program::run",
                "playback_worker::ts_program::run",
            ),
            (
                "playback_worker::matroska_video::session::{closure#6}",
                "playback_worker::matroska_video::session::{closure#6}",
            ),
            (
                "gimli::read::line::parse_attribute::<gimli::read::EndianSlice<gimli::LittleEndian>>",
                "gimli::read::line::parse_attribute",
            ),
            (
                "<triergon_core::bytes::BudgetVec<triergon_isobmff::fragment::Trex>>::reallocate",
                "triergon_core::bytes::BudgetVec::reallocate",
            ),
            (
                "<triergon_isobmff::mov_cursor::PacketOccurrence as core::clone::Clone>::clone",
                "triergon_isobmff::mov_cursor::PacketOccurrence::clone",
            ),
            (
                "<triergon_playback::player::Player<triergon_playback::input::ByteSource>>::accept_event::<true>",
                "triergon_playback::player::Player::accept_event",
            ),
            (
                "core::ptr::drop_glue::<triergon_isobmff::encoded_stream::EncodedStreamJob>",
                "triergon_isobmff::encoded_stream::EncodedStreamJob::{drop}",
            ),
            (
                "core::ptr::drop_glue::<alloc::vec::Vec<addr2line::line::LineSequence>>",
                "alloc::vec::Vec::{drop}",
            ),
            (
                "<<std::sys::backtrace::BacktraceLock>::print::DisplayBacktrace as core::fmt::Display>::fmt",
                "std::sys::backtrace::BacktraceLock::print::DisplayBacktrace::fmt",
            ),
            ("<bool as core::fmt::Display>::fmt", "bool::fmt"),
            ("<&u8 as core::fmt::Debug>::fmt", "u8::fmt"),
            (
                "<[triergon_isobmff::mov_index::Entry]>::sort_unstable_by::<F>",
                "triergon_isobmff::mov_index::Entry::sort_unstable_by",
            ),
            ("__aeabi_uidiv", "__aeabi_uidiv"),
            ("_Unwind_GetGR.constprop.0", "_Unwind_GetGR.constprop.0"),
        ] {
            assert_eq!(owner(name), expected, "{name}");
        }
    }
}
