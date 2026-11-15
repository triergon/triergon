//! Link order: a worker's functions laid out by what a training corpus runs, then
//! grouped by the item that owns them.
//!
//! A trained order lists the functions the training corpus ran in a worker, by
//! demangled name in first-use order ([`first_use`]); one is committed per worker
//! and target under `harness/link-order/TARGET/WORKER.txt`. The ordering file of a
//! binary ([`symbol_order`]) puts its functions that the trained order lists first,
//! in its order, and every other function after them in module order: keyed by its
//! owning path, a free function by its own path, a method by its type's path and
//! name, a closure by its function's, and drop glue by the type it drops, with
//! generic arguments left out. Sorting by that path puts each crate's functions
//! together, and within a crate each module's and each type's. A name the binary
//! does not hold is skipped, so a stale trained order still links. The file is
//! computed from a first link of the worker and applied by linking again
//! ([`super::gate_build::ordered_link_arguments`]); ordering the second link
//! gives the same file.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the trained order of `worker` for `target` is committed.
pub fn trained_path(root: &Path, target: &str, worker: &str) -> PathBuf {
    root.join("harness/link-order")
        .join(target)
        .join(format!("{worker}.txt"))
}

/// The function names of a trained order: one demangled name per line, in
/// first-use order, each once; a line starting with `#` is a comment.
pub fn parse_trained(text: &str) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for (index, line) in text.lines().enumerate() {
        if line.starts_with('#') {
            continue;
        }
        if line.is_empty() || line.trim() != line {
            return Err(format!("line {}: not a function name", index + 1));
        }
        if !seen.insert(line) {
            return Err(format!("line {}: {line} listed twice", index + 1));
        }
        names.push(line.to_owned());
    }
    Ok(names)
}

/// The committed trained order of `worker` for `target`; none when it has none.
pub fn committed_trained(root: &Path, target: &str, worker: &str) -> Result<Vec<String>, String> {
    let path = trained_path(root, target, worker);
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_trained(&text).map_err(|error| format!("{}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// A demangled name without the suffix LLVM gives a local function it promotes,
/// which changes from build to build.
fn stable_name(name: &str) -> &str {
    name.find(".llvm.")
        .map_or(name, |end| name[..end].trim_end_matches(" ("))
}

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

/// The symbol ordering file of `binary`: one symbol per line, those `trained` names
/// first in its order, then the rest by owning path, demangled name and symbol name,
/// without the code the linker makes itself.
pub fn symbol_order(binary: &Path, trained: &[String]) -> Result<String, String> {
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
    let rank: HashMap<&str, usize> = trained
        .iter()
        .enumerate()
        .map(|(index, name)| (name.as_str(), index))
        .collect();
    let mut rows: Vec<(usize, String, &str, &str)> = names
        .iter()
        .zip(&symbols)
        .filter(|(_, symbol)| !linker_made(symbol))
        .map(|(name, symbol)| {
            let rank = rank.get(stable_name(name)).copied().unwrap_or(usize::MAX);
            (rank, owner(name), name.as_str(), symbol.as_str())
        })
        .collect();
    rows.sort_unstable();
    let mut order = String::new();
    let mut previous = None;
    for (_, _, _, symbol) in rows {
        // A local name the binary holds more than once is one line of the file.
        if previous != Some(symbol) {
            order.push_str(symbol);
            order.push('\n');
        }
        previous = Some(symbol);
    }
    Ok(order)
}

/// A function of a binary: where its code lies and its stable demangled name.
struct Function {
    start: u64,
    end: u64,
    name: String,
}

/// The sized text symbols of `binary` with their demangled names, by address.
fn functions(binary: &Path) -> Result<Vec<Function>, String> {
    let list = |demangle: bool| -> Result<Vec<(u64, u64, String)>, String> {
        let mut command = Command::new("nm");
        command.args(["--defined-only", "-p", "-S"]);
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
                let mut fields = line.splitn(4, ' ');
                let address = u64::from_str_radix(fields.next()?, 16).ok()?;
                let size = u64::from_str_radix(fields.next()?, 16).ok()?;
                let (kind, name) = (fields.next()?, fields.next()?);
                (matches!(kind, "t" | "T" | "w" | "W") && size > 0)
                    .then(|| (address, size, name.to_owned()))
            })
            .collect())
    };
    let symbols = list(false)?;
    let names = list(true)?;
    if symbols.len() != names.len() {
        return Err(format!(
            "{}: nm lists {} sized symbols and {} demangled names",
            binary.display(),
            symbols.len(),
            names.len()
        ));
    }
    let mut functions: Vec<Function> = symbols
        .iter()
        .zip(names)
        .filter(|((_, _, symbol), _)| !linker_made(symbol))
        .map(|((address, size, _), (_, _, name))| Function {
            // A Thumb function's symbol holds its address with the lowest bit set.
            start: address & !1,
            end: (address & !1) + size,
            name: stable_name(&name).to_owned(),
        })
        .collect();
    functions.sort_by(|a, b| (a.start, &a.name).cmp(&(b.start, &b.name)));
    Ok(functions)
}

/// The entry address in the ELF header of `binary`.
fn entry(binary: &Path) -> Result<u64, String> {
    let header = std::fs::read(binary).map_err(|error| format!("{}: {error}", binary.display()))?;
    let field = |range: std::ops::Range<usize>| {
        header
            .get(range)
            .ok_or_else(|| format!("{}: no ELF header", binary.display()))
    };
    if field(0..4)? != b"\x7fELF" || field(5..6)? != [1] {
        return Err(format!(
            "{}: not a little-endian ELF file",
            binary.display()
        ));
    }
    let mut bytes = [0; 8];
    match field(4..5)? {
        [1] => bytes[..4].copy_from_slice(field(24..28)?),
        [2] => bytes.copy_from_slice(field(24..32)?),
        _ => return Err(format!("{}: unknown ELF class", binary.display())),
    }
    Ok(u64::from_le_bytes(bytes))
}

/// The trained order of `binary` from logs of its runs under QEMU's user-mode
/// emulator with `-d page,in_asm`: the functions of the binary in the order the runs
/// first execute code in them, the runs in the order of `logs`.
///
/// The emulator translates each block of guest code when it first runs it and logs
/// the block's guest addresses then (`in_asm`); its `page` log gives the address it
/// loaded the binary's entry point at, from which the binary's own addresses follow.
pub fn first_use(binary: &Path, logs: &[PathBuf]) -> Result<Vec<String>, String> {
    let functions = functions(binary)?;
    let entry = entry(binary)?;
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    for log in logs {
        let text =
            std::fs::read_to_string(log).map_err(|error| format!("{}: {error}", log.display()))?;
        let loaded = text
            .lines()
            .find_map(|line| line.strip_prefix("entry"))
            .and_then(|rest| rest.trim().strip_prefix("0x"))
            .and_then(|address| u64::from_str_radix(address, 16).ok())
            .ok_or_else(|| format!("{}: no entry line; run with -d page,in_asm", log.display()))?;
        let bias = loaded.wrapping_sub(entry);
        for line in text.lines() {
            let Some(address) = line
                .strip_prefix("0x")
                .and_then(|rest| rest.split_once(':'))
                .and_then(|(address, _)| u64::from_str_radix(address, 16).ok())
            else {
                continue;
            };
            let address = address.wrapping_sub(bias);
            // The functions starting where the last one at or below the address
            // starts: one, or several names of the same code.
            let after = functions.partition_point(|function| function.start <= address);
            let Some(start) = after.checked_sub(1).map(|last| functions[last].start) else {
                continue;
            };
            let first = functions.partition_point(|function| function.start < start);
            for function in functions[first..after]
                .iter()
                .filter(|function| address < function.end)
            {
                if seen.insert(function.name.as_str()) {
                    order.push(function.name.clone());
                }
            }
        }
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
