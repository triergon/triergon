//! The status tables of `docs/status.md`, from data. [`ROWS`] names each table (its
//! heading, gate target and default run) and each row (its area, the targets whose
//! tables show it, and the gate results its Runtime and Memory cells show); the cells
//! are rendered from those results' measured records in the evidence index
//! ([`Measured`]). `cargo xtask check-design` renders them again and fails on any
//! difference; `cargo xtask status` prints the tables as rendered, or writes them.
//!
//! The page's table of listed differences ([`LISTED`]) is rendered from the registry:
//! per comparison, how many cases each form of [`crate::harness::registry`] lists.
//!
//! A row's cells stay as written only while `open_cells` lists the row on that target,
//! which it may only while every result the row cites predates the measured record (its
//! index entry records no target): `cargo xtask evidence measure` records the target
//! and what was measured, and the row is rendered from then on. New results record
//! both when they are indexed, so the list can only shrink.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::harness::measured::Measured;
use crate::harness::registry::Form;
use crate::json::{self, Value};

/// Which results each status row shows, relative to the repository root.
pub const ROWS: &str = "harness/status-rows.json";
/// The status page, relative to the repository root.
pub const PAGE: &str = "docs/status.md";

/// A gate run a row cites: the set named so, or the sets named so followed by
/// `-shard-N`, measured on one target at one revision.
struct Run<'a> {
    target: &'a str,
    revision: &'a str,
}

struct Table<'a> {
    heading: &'a str,
    target: &'a str,
    run: &'a str,
}

/// Results shown together, by the directory each lies in (its last component): in a
/// gate run, the item whose timing, memory and grade share it.
struct Part<'a> {
    label: Option<&'a str>,
    results: Vec<&'a str>,
    /// The targets the part is shown on; every target of its row when `None`.
    targets: Option<Vec<&'a str>>,
}

enum Cells<'a> {
    /// The same text in both cells, for a row with no measurement of its own.
    Text(&'a str),
    Unmeasured(&'a str),
    Parts(Vec<Part<'a>>),
}

struct Row<'a> {
    area: &'a str,
    targets: Vec<&'a str>,
    implemented: Option<&'a str>,
    /// The runs the row cites instead of its tables' runs, one per target at most.
    runs: Vec<&'a str>,
    cells: Cells<'a>,
    parity: BTreeMap<&'a str, &'a str>,
    open: Option<&'a str>,
}

struct Rows<'a> {
    tables: Vec<Table<'a>>,
    runs: BTreeMap<&'a str, Run<'a>>,
    rows: Vec<Row<'a>>,
    /// The rows, by area and target, whose cells are written by hand.
    open: BTreeSet<(&'a str, &'a str)>,
}

/// The evidence index's sets by name.
type Index<'a> = BTreeMap<&'a str, &'a Value>;

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn strings<'a>(value: &'a Value, key: &str) -> Option<Vec<&'a str>> {
    value
        .get(key)?
        .as_array()?
        .iter()
        .map(Value::as_str)
        .collect()
}

impl<'a> Rows<'a> {
    fn parse(document: &'a Value) -> Result<Self, Vec<String>> {
        let mut errors = Vec::new();
        let mut error = |message: String| errors.push(format!("{ROWS}: {message}"));
        let targets = |value: &'a Value, what: &str, error: &mut dyn FnMut(String)| {
            let named = strings(value, "targets").unwrap_or_default();
            if named.is_empty() || named.iter().any(|t| !crate::gate::TARGETS.contains(t)) {
                error(format!(
                    "{what}: targets must be some of the gate's targets"
                ));
            }
            named
        };
        let mut runs = BTreeMap::new();
        if let Some(Value::Object(listed)) = document.get("runs") {
            for (name, run) in listed {
                let (Some(target), Some(revision)) = (text(run, "target"), text(run, "revision"))
                else {
                    error(format!("run {name}: names no target and revision"));
                    continue;
                };
                if !crate::gate::TARGETS.contains(&target) {
                    error(format!("run {name}: {target} is not a gate target"));
                }
                if !(7..=40).contains(&revision.len())
                    || !revision
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                {
                    error(format!(
                        "run {name}: {revision} is not an abbreviated revision"
                    ));
                }
                runs.insert(name.as_str(), Run { target, revision });
            }
        } else {
            error("no runs".into());
        }
        let mut tables = Vec::new();
        for table in document
            .get("tables")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let (Some(heading), Some(target), Some(run)) = (
                text(table, "heading"),
                text(table, "target"),
                text(table, "run"),
            ) else {
                error("a table names its heading, target and run".into());
                continue;
            };
            if runs.get(run).is_none_or(|r: &Run<'_>| r.target != target) {
                error(format!("{heading}: {run} is not a run on {target}"));
            }
            tables.push(Table {
                heading,
                target,
                run,
            });
        }
        let mut shown: Vec<&str> = tables.iter().map(|t| t.target).collect();
        shown.sort_unstable();
        let mut gated = crate::gate::TARGETS.to_vec();
        gated.sort_unstable();
        if shown != gated {
            error("each gate target has one table".into());
        }
        let mut rows = Vec::new();
        let mut areas = BTreeSet::new();
        for row in document
            .get("rows")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let Some(area) = text(row, "area").filter(|a| !a.is_empty()) else {
                error("a row names its area".into());
                continue;
            };
            if !areas.insert(area) {
                error(format!("{area}: listed twice"));
            }
            let row_targets = targets(row, area, &mut error);
            let row_runs = strings(row, "runs").unwrap_or_default();
            let mut run_targets = BTreeSet::new();
            for run in &row_runs {
                match runs.get(run) {
                    Some(r) if row_targets.contains(&r.target) && run_targets.insert(r.target) => {}
                    _ => error(format!(
                        "{area}: {run} is not a run on one of its targets, one per target"
                    )),
                }
            }
            let parts = |list: &'a [Value], error: &mut dyn FnMut(String)| -> Vec<Part<'a>> {
                list.iter()
                    .map(|part| {
                        let results = strings(part, "results").unwrap_or_default();
                        if results.is_empty() {
                            error(format!("{area}: a part names the results it shows"));
                        }
                        let on = part.get("targets").map(|_| targets(part, area, error));
                        Part {
                            label: text(part, "label"),
                            results,
                            targets: on,
                        }
                    })
                    .collect()
            };
            let cells = match (
                text(row, "text"),
                text(row, "unmeasured"),
                row.get("results"),
                row.get("parts"),
            ) {
                (Some(shown), None, None, None) => Cells::Text(shown),
                (None, Some(feature), None, None) => Cells::Unmeasured(feature),
                (None, None, Some(_), None) => Cells::Parts(vec![Part {
                    label: None,
                    results: strings(row, "results").unwrap_or_default(),
                    targets: None,
                }]),
                (None, None, None, Some(Value::Array(list))) => {
                    let parts = parts(list, &mut error);
                    if parts.iter().any(|p| p.label.is_none_or(str::is_empty)) {
                        error(format!("{area}: each of several parts has a label"));
                    }
                    Cells::Parts(parts)
                }
                _ => {
                    error(format!(
                        "{area}: one of text, unmeasured, results and parts"
                    ));
                    continue;
                }
            };
            if let Cells::Parts(parts) = &cells
                && parts.iter().any(|p| p.results.is_empty())
            {
                error(format!("{area}: names no results"));
            }
            let mut parity = BTreeMap::new();
            if let Some(value) = row.get("parity") {
                if let Value::Object(entries) = value {
                    for (target, cell) in entries {
                        match cell.as_str() {
                            Some(cell)
                                if row_targets.contains(&target.as_str())
                                    && !cell.is_empty()
                                    && !cell.contains(['|', '\n']) =>
                            {
                                parity.insert(target.as_str(), cell);
                            }
                            _ => error(format!("{area}: invalid parity cell for {target}")),
                        }
                    }
                } else {
                    error(format!("{area}: parity cells must be keyed by target"));
                }
            }
            rows.push(Row {
                area,
                targets: row_targets,
                implemented: text(row, "implemented"),
                runs: row_runs,
                cells,
                parity,
                open: text(row, "open"),
            });
        }
        let mut open = BTreeSet::new();
        for entry in document
            .get("open_cells")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let area = text(entry, "area").unwrap_or_default();
            let on = targets(entry, area, &mut error);
            match rows.iter().find(|row| row.area == area) {
                Some(Row {
                    cells: Cells::Parts(_),
                    targets: shown_on,
                    ..
                }) => {
                    for target in on {
                        if !shown_on.contains(&target) || !open.insert((area, target)) {
                            error(format!("open cells of {area}: {target} once, of its row's"));
                        }
                    }
                }
                _ => error(format!("open cells of {area}: not a row with results")),
            }
        }
        if errors.is_empty() {
            Ok(Self {
                tables,
                runs,
                rows,
                open,
            })
        } else {
            Err(errors)
        }
    }

    /// The Runtime and Memory cells of `row` on `table`'s target: `None` for open cells,
    /// which must cite results that record no target.
    fn cells(
        &self,
        index: &Index<'_>,
        table: &Table<'_>,
        row: &Row<'_>,
    ) -> Result<Option<(String, String)>, Vec<String>> {
        let at = format!("{} on {}", row.area, table.target);
        let parts = match &row.cells {
            Cells::Text(shown) => return Ok(Some(((*shown).to_owned(), (*shown).to_owned()))),
            Cells::Unmeasured(_) => return Err(vec![format!("{at}: missing bound declarations")]),
            Cells::Parts(parts) => parts,
        };
        let name = row
            .runs
            .iter()
            .copied()
            .find(|run| self.runs[run].target == table.target)
            .unwrap_or(table.run);
        let run = &self.runs[name];
        let results = run_results(index, name);
        let shown: Vec<&Part<'_>> = parts
            .iter()
            .filter(|part| {
                part.targets
                    .as_ref()
                    .is_none_or(|targets| targets.contains(&table.target))
            })
            .collect();
        let mut errors = Vec::new();
        let mut rendered = Vec::new();
        let open = self.open.contains(&(row.area, table.target));
        for part in &shown {
            let mut records = Vec::new();
            for &directory in &part.results {
                let held: Vec<&Value> = results
                    .iter()
                    .filter(|result| result_directory(result) == Some(directory))
                    .copied()
                    .collect();
                if held.is_empty() {
                    errors.push(format!("{at}: run {name} holds no result in {directory}/"));
                    continue;
                }
                for result in &held {
                    match result.get("target") {
                        None => {}
                        Some(Value::String(target)) if target == table.target => {}
                        Some(other) => errors.push(format!(
                            "{at}: {directory}/ was measured on {}, not {}",
                            json::canonical(other),
                            table.target
                        )),
                    }
                }
                let recorded = held.iter().any(|result| result.get("target").is_some());
                match (open, recorded) {
                    (true, true) => errors.push(format!(
                        "{at}: {directory}/ records what it measured; render the cells and \
                         take the row out of open_cells"
                    )),
                    (true, false) => {}
                    (false, _) => match measured(directory, &held) {
                        Ok(record) => records.push(record),
                        Err(error) => errors.push(format!("{at}: {error}")),
                    },
                }
            }
            if !open {
                rendered.push((part.label, Measured::join(&records)));
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        if open {
            return Ok(None);
        }
        let labelled = rendered.len() > 1;
        let mut runtime = Vec::new();
        let mut memory = Vec::new();
        for (label, record) in &rendered {
            let label = match label {
                Some(label) if labelled => format!("{label} "),
                _ => String::new(),
            };
            match (&record.runtime, &record.memory) {
                (Some(timed), Some(measured)) => {
                    runtime.push(format!("{label}{}", runtime_cell(timed, run.revision)));
                    memory.push(format!("{label}{}", memory_cell(measured, run.revision)));
                }
                _ => errors.push(format!("{at}: its results measure no runtime or no memory")),
            }
        }
        if errors.is_empty() {
            Ok(Some((runtime.join("<br>"), memory.join("<br>"))))
        } else {
            Err(errors)
        }
    }
}

/// The directory of a result, by its last component.
fn result_directory(result: &Value) -> Option<&str> {
    let path = text(result, "path")?;
    let directory = path.rsplit_once('/').map_or("", |(directory, _)| directory);
    Some(directory.rsplit('/').next().unwrap_or(directory))
}

/// The sets of run `name`: the set named so, or the sets named so followed by
/// `-shard-N`.
fn run_sets<'a>(index: &Index<'a>, name: &str) -> Vec<&'a Value> {
    let shard = |set: &str| {
        set.strip_prefix(name)
            .and_then(|rest| rest.strip_prefix("-shard-"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    index
        .iter()
        .filter(|(set, _)| **set == name || shard(set))
        .map(|(_, set)| *set)
        .collect()
}

/// Every result of the sets of run `name`.
fn run_results<'a>(index: &Index<'a>, name: &str) -> Vec<&'a Value> {
    run_sets(index, name)
        .into_iter()
        .flat_map(|set| {
            set.get("results")
                .and_then(Value::as_array)
                .unwrap_or_default()
        })
        .collect()
}

/// Why run `name` cannot be cited: it has no set, a set that is not kept, or one that
/// records other source revisions than `run`'s.
fn run_errors(index: &Index<'_>, name: &str, run: &Run<'_>) -> Vec<String> {
    let sets = run_sets(index, name);
    if sets.is_empty() {
        return vec![format!("{ROWS}: run {name}: no evidence set is named so")];
    }
    let mut errors = Vec::new();
    for set in sets {
        let set_name = text(set, "name").unwrap_or_default();
        if text(set, "retention") != Some("retained") {
            errors.push(format!("{ROWS}: run {name}: {set_name} is not kept"));
        }
        let revisions = recorded_revisions(set);
        if !revisions.is_empty() && !revisions.iter().any(|r| r.starts_with(run.revision)) {
            errors.push(format!(
                "{ROWS}: run {name}: {set_name} records another source revision than {}",
                run.revision
            ));
        }
    }
    errors
}

/// What the results in `directory` measured together: the runtime of its grade, or of
/// its one result that timed, and likewise its memory.
fn measured(directory: &str, held: &[&Value]) -> Result<Measured, String> {
    let mut records = Vec::new();
    for result in held {
        if let Some(record) = result.get("measured") {
            let record = Measured::parse(record).map_err(|e| format!("{directory}/: {e}"))?;
            records.push((text(result, "command") == Some("grade"), record));
        }
    }
    let pick = |has: fn(&Measured) -> bool, what: &str| {
        let with: Vec<&(bool, Measured)> = records.iter().filter(|(_, r)| has(r)).collect();
        if let Some((_, graded)) = with.iter().find(|(grade, _)| *grade) {
            return Ok(Some(graded));
        }
        match with.as_slice() {
            [] => Ok(None),
            [only] => Ok(Some(&only.1)),
            _ => Err(format!(
                "{directory}/: {} results measure its {what} and no grade joins them",
                with.len()
            )),
        }
    };
    let runtime = pick(|r| r.runtime.is_some(), "runtime")?;
    let memory = pick(|r| r.memory.is_some(), "memory")?;
    if runtime.is_none() && memory.is_none() {
        return Err(format!(
            "{directory}/ records nothing it measured: run `cargo xtask evidence measure` \
             for its set on a host that reaches the archive"
        ));
    }
    Ok(Measured {
        runtime: runtime.and_then(|r| r.runtime.clone()),
        memory: memory.and_then(|r| r.memory.clone()),
    })
}

/// An upper bound as a cell states it: rounded up to four decimals, so it still bounds.
fn bound(value: f64) -> String {
    format!("{:.4}", ((value * 1e4) - 1e-6).ceil() / 1e4)
}

fn runtime_cell(runtime: &crate::harness::measured::Runtime, revision: &str) -> String {
    let counts = format!("{}/{} pass", runtime.passing, runtime.cases);
    match &runtime.worst {
        Some((upper, id)) => format!("{} ({counts}, worst `{id}`) at `{revision}`", bound(*upper)),
        None => format!("no valid timing ({counts}) at `{revision}`"),
    }
}

fn memory_cell(memory: &crate::harness::measured::Memory, revision: &str) -> String {
    let shown =
        |value: Option<f64>| value.map_or_else(|| " invalid".into(), |v| format!(" ≤{}", bound(v)));
    format!(
        "heap{}, total RSS{} ({}/{} lower) at `{revision}`",
        shown(memory.heap),
        shown(memory.total_rss),
        memory.lower,
        memory.cases
    )
}

/// One table of the page: the line its header is on, the columns of its Runtime and
/// Memory cells, and each row's line and cells.
struct PageTable<'p> {
    header: usize,
    width: usize,
    implemented: Option<usize>,
    columns: (usize, usize),
    parity: Option<usize>,
    open: Option<usize>,
    rows: Vec<(usize, Vec<&'p str>)>,
}

fn page_table<'p>(lines: &[&'p str], heading: &str) -> Result<PageTable<'p>, String> {
    let start = lines
        .iter()
        .position(|line| {
            let title = line.trim_start_matches('#');
            title.len() < line.len() && title.trim() == heading
        })
        .ok_or_else(|| format!("{PAGE}: no heading {heading}"))?;
    let header = (start + 1..lines.len())
        .find(|&i| lines[i].starts_with("| Area "))
        .ok_or_else(|| format!("{PAGE}: no Area table under {heading}"))?;
    if lines[start + 1..header]
        .iter()
        .any(|line| line.starts_with('#'))
    {
        return Err(format!("{PAGE}: no Area table under {heading}"));
    }
    let names: Vec<&str> = crate::checks::pipe_cells(lines[header])
        .map(str::trim)
        .collect();
    let column = |name: &str| {
        names
            .iter()
            .position(|n| *n == name)
            .ok_or_else(|| format!("{PAGE}: {heading}: no {name} column"))
    };
    let columns = (column("Runtime")?, column("Memory")?);
    let parity = column("Verified against the reference").ok();
    let rows = (header + 2..lines.len())
        .take_while(|&i| lines[i].starts_with('|'))
        .map(|i| {
            (
                i,
                crate::checks::pipe_cells(lines[i]).map(str::trim).collect(),
            )
        })
        .collect();
    Ok(PageTable {
        header,
        width: names.len(),
        implemented: column("Implemented").ok(),
        columns,
        parity,
        open: column("Open").ok(),
        rows,
    })
}

/// Each table's rows as [`ROWS`] renders them, beside the page's: per table, the page's
/// row lines and the rendered cells of each (`None` where open).
type Rendered<'p> = Vec<(PageTable<'p>, Vec<RenderedRow>)>;

struct RenderedRow {
    bounds: Option<(String, String)>,
    parity: Option<String>,
    implemented: Option<String>,
    open: Option<String>,
}

impl RenderedRow {
    fn cells(&self, table: &PageTable<'_>) -> impl Iterator<Item = (usize, &str, &'static str)> {
        let (r, m) = table.columns;
        self.bounds
            .as_ref()
            .into_iter()
            .flat_map(move |(runtime, memory)| {
                [
                    (r, runtime.as_str(), "Runtime"),
                    (m, memory.as_str(), "Memory"),
                ]
            })
            .chain(
                table
                    .parity
                    .zip(self.parity.as_deref())
                    .map(|(index, cell)| (index, cell, "Verified against the reference")),
            )
            .chain(
                table
                    .implemented
                    .zip(self.implemented.as_deref())
                    .map(|(index, cell)| (index, cell, "Implemented")),
            )
            .chain(
                table
                    .open
                    .zip(self.open.as_deref())
                    .map(|(index, cell)| (index, cell, "Open")),
            )
    }
}

fn unmeasured_cells(root: &Path, id: &str, target: &str) -> Result<(String, String), String> {
    use crate::harness::bounds::{Bound, Unmeasured};

    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("unmeasured status needs a feature ID".into());
    }
    let directory = root.join("harness/features").join(id);
    let contract = json::read(&directory.join("contract.json"))?;
    let (manifest, _) = crate::harness::manifest::read(root, &directory.join("corpus.json"))?;
    let unmeasured = Unmeasured::read(&contract, &manifest, target)?;
    let cases = manifest
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("missing corpus cases")?;
    let cell = |bounds: &[Bound], parity_only| -> Result<String, String> {
        let causes: BTreeSet<_> = cases
            .iter()
            .filter_map(|case| text(case, "id"))
            .flat_map(|case| unmeasured.causes(case))
            .filter_map(|(bound, cause)| bounds.contains(&bound).then_some(cause))
            .collect();
        if causes.is_empty() {
            return Err(format!(
                "{id}: unmeasured status must cite bounds for {target}"
            ));
        }
        let label = if parity_only {
            "parity only"
        } else {
            "not measured"
        };
        let causes = causes
            .into_iter()
            .map(|cause| cause.replace('|', "\\|"))
            .collect::<Vec<_>>();
        Ok(format!("{label} ({})", causes.join(", ")))
    };
    let parity_only =
        unmeasured.all_skip(&manifest, "timing") && unmeasured.all_skip(&manifest, "memory");
    Ok((
        cell(&[Bound::Runtime], parity_only)?,
        cell(&[Bound::Heap, Bound::TotalRss], parity_only)?,
    ))
}

fn render<'p>(
    root: &Path,
    rows: &Rows<'_>,
    index: &Index<'_>,
    page: &[&'p str],
) -> Result<Rendered<'p>, Vec<String>> {
    let mut errors: Vec<String> = rows
        .runs
        .iter()
        .flat_map(|(name, run)| run_errors(index, name, run))
        .collect();
    if !errors.is_empty() {
        return Err(errors);
    }
    let mut rendered = Vec::new();
    for table in &rows.tables {
        let shown: Vec<&Row<'_>> = rows
            .rows
            .iter()
            .filter(|row| row.targets.contains(&table.target))
            .collect();
        let page_table = match page_table(page, table.heading) {
            Ok(page_table) => page_table,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        let areas: Vec<&str> = page_table
            .rows
            .iter()
            .map(|(_, cells)| cells.first().copied().unwrap_or_default())
            .collect();
        let expected: Vec<&str> = shown.iter().map(|row| row.area).collect();
        if areas != expected {
            let differs = areas
                .iter()
                .zip(&expected)
                .position(|(a, b)| a != b)
                .unwrap_or(areas.len().min(expected.len()));
            errors.push(format!(
                "{PAGE}: {}: row {} is {:?}, {ROWS} names {:?}",
                table.heading,
                differs + 1,
                areas.get(differs).copied().unwrap_or("nothing"),
                expected.get(differs).copied().unwrap_or("nothing")
            ));
            continue;
        }
        let mut cells = Vec::new();
        for row in shown {
            let parity = row.parity.get(table.target).map(|text| (*text).to_owned());
            if parity.is_some() && page_table.parity.is_none() {
                errors.push(format!("{PAGE}: {} has no parity column", table.heading));
            }
            let cell = match row.cells {
                Cells::Unmeasured(id) => unmeasured_cells(root, id, table.target)
                    .map(Some)
                    .map_err(|error| vec![format!("{}: {error}", row.area)]),
                _ => rows.cells(index, table, row),
            };
            let bounds = match cell {
                Ok(cell) => cell,
                Err(row_errors) => {
                    errors.extend(row_errors);
                    None
                }
            };
            cells.push(RenderedRow {
                bounds,
                parity,
                implemented: row.implemented.map(str::to_owned),
                open: row.open.map(str::to_owned),
            });
        }
        rendered.push((page_table, cells));
    }
    if errors.is_empty() {
        Ok(rendered)
    } else {
        Err(errors)
    }
}

/// The source revisions a set's entry records, if any.
fn recorded_revisions(set: &Value) -> Vec<&str> {
    let mut revisions: Vec<&str> = text(set, "source_revision").into_iter().collect();
    match set.get("source_revisions") {
        Some(Value::Array(list)) => revisions.extend(list.iter().filter_map(Value::as_str)),
        Some(Value::Object(members)) => {
            revisions.extend(members.iter().filter_map(|(_, value)| value.as_str()));
        }
        _ => {}
    }
    revisions
}

/// The heading of the page's table of listed differences.
pub const LISTED: &str = "Listed differences";

/// The forms in the order the table shows them.
const SHOWN: [Form; 5] = [
    Form::Departure,
    Form::Policy,
    Form::Unimplemented,
    Form::Refusal,
    Form::Defect,
];

/// The link a comparison's row shows for registry file `file`, when it is one: a
/// feature's contract, a container pin file or a codec manifest.
fn comparison(file: &str) -> Option<String> {
    let link = |label: String| format!("[{label}](../{file})");
    if let Some(rest) = file.strip_prefix("harness/features/") {
        let (id, name) = rest.split_once('/')?;
        return match name {
            "contract.json" => Some(link(format!("`{id}`"))),
            "container-pins.json" => Some(link(format!("`{id}` container seeks"))),
            _ => None,
        };
    }
    let name = file
        .strip_prefix("harness/codecs/")?
        .strip_suffix(".json")?;
    (!name.contains('/')).then(|| link(format!("`codecs/{name}`")))
}

/// The cases each form of `document` lists on `target`: per shown form the ids, a
/// case of two forms in both.
fn listed_on<'a>(document: &'a Value, target: &str) -> [BTreeSet<&'a str>; 5] {
    SHOWN.map(|form| {
        let mut ids = BTreeSet::new();
        for entry in document
            .get(form.list())
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let scoped = strings(entry, "targets");
            if scoped.is_some_and(|targets| !targets.contains(&target)) {
                continue;
            }
            // An open case names itself; the other forms group their cases.
            let own = entry
                .as_str()
                .or_else(|| text(entry, "id"))
                .or_else(|| text(entry, "name"));
            let grouped = entry
                .get("cases")
                .and_then(Value::as_array)
                .unwrap_or_default()
                .iter()
                .filter_map(|case| case.as_str().or_else(|| text(case, "id")));
            match entry.get("cases") {
                Some(_) => ids.extend(grouped),
                None => ids.extend(own),
            }
        }
        ids
    })
}

/// A count on the gate's targets, AArch64 first: one number where they agree, the
/// numbers separated by `/` where they differ, nothing where none is listed.
fn count_cell(counts: &[usize]) -> String {
    match counts {
        [first, rest @ ..] if rest.iter().all(|count| count == first) => {
            if *first == 0 {
                String::new()
            } else {
                first.to_string()
            }
        }
        _ => counts
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(" / "),
    }
}

/// The table of listed differences as the registry files of `data` give it: its
/// header, one row for each comparison that lists a case, by path, and the totals.
/// Each row counts the cases of each form, the cases listed at all and, last, the
/// cases the comparison holds.
pub fn listed_table(data: &[(String, Value)]) -> Vec<String> {
    // AArch64 before ARMv7, as the page orders its tables.
    let mut targets = crate::gate::TARGETS.to_vec();
    targets.sort_unstable();
    let mut lines = vec![
        "| Comparison | departs | policy | unimplemented | refuses | defect | listed | cases |"
            .to_owned(),
        "|---|---|---|---|---|---|---|---|".to_owned(),
    ];
    let mut totals = vec![[0usize; 6]; targets.len()];
    let mut total_cases = 0;
    let mut files: Vec<(&String, &Value)> = data
        .iter()
        .filter(|(file, _)| comparison(file).is_some())
        .map(|(file, document)| (file, document))
        .collect();
    files.sort_by(|a, b| a.0.cmp(b.0));
    for (file, document) in files {
        let per_target: Vec<[usize; 6]> = targets
            .iter()
            .map(|target| {
                let forms = listed_on(document, target);
                let all: BTreeSet<&str> = forms.iter().flatten().copied().collect();
                let mut counts = [0; 6];
                for (count, ids) in counts.iter_mut().zip(&forms) {
                    *count = ids.len();
                }
                counts[5] = all.len();
                counts
            })
            .collect();
        if per_target.iter().all(|counts| counts[5] == 0) {
            continue;
        }
        for (total, counts) in totals.iter_mut().zip(&per_target) {
            for (sum, count) in total.iter_mut().zip(counts) {
                *sum += count;
            }
        }
        let cases = case_count(data, file, document);
        total_cases += cases.unwrap_or(0);
        lines.push(table_row(
            &comparison(file).unwrap_or_default(),
            &per_target,
            cases,
        ));
    }
    lines.push(table_row("**Total**", &totals, Some(total_cases)));
    lines
}

/// How many cases the comparison of registry file `file` holds, where one file of the
/// harness data says: the rows of a feature's manifest or the pins of a generated
/// one, and the pins of a container pin file. A codec manifest's lanes share their
/// vectors with other manifests, so it has no such count.
fn case_count(data: &[(String, Value)], file: &str, document: &Value) -> Option<usize> {
    let beside = |name: &str| {
        let (directory, _) = file.rsplit_once('/')?;
        let path = format!("{directory}/{name}");
        data.iter()
            .find(|(other, _)| *other == path)
            .map(|(_, document)| document)
    };
    let len = |value: Option<&Value>| match value? {
        Value::Array(items) => Some(items.len()),
        Value::Object(members) => Some(members.len()),
        _ => None,
    };
    if file.ends_with("/contract.json") {
        len(beside("corpus.json").and_then(|corpus| corpus.get("cases")))
            .or_else(|| len(beside("pins.json").and_then(|pins| pins.get("pins"))))
    } else if file.ends_with("/container-pins.json") {
        len(document.get("pins"))
    } else {
        None
    }
}

fn table_row(label: &str, per_target: &[[usize; 6]], cases: Option<usize>) -> String {
    let mut cells: Vec<String> = (0..6)
        .map(|column| {
            let counts: Vec<usize> = per_target.iter().map(|counts| counts[column]).collect();
            count_cell(&counts)
        })
        .collect();
    cells.push(cases.map_or_else(String::new, |cases| cases.to_string()));
    format!("| {label} | {} |", cells.join(" | "))
}

/// The lines of the page's table of listed differences: the first and one past the
/// last.
fn listed_lines(page: &[&str]) -> Result<(usize, usize), String> {
    let start = page
        .iter()
        .position(|line| {
            let title = line.trim_start_matches('#');
            title.len() < line.len() && title.trim() == LISTED
        })
        .ok_or_else(|| format!("{PAGE}: no heading {LISTED}"))?;
    let first = (start + 1..page.len())
        .take_while(|&i| !page[i].starts_with('#'))
        .find(|&i| page[i].starts_with("| Comparison "))
        .ok_or_else(|| format!("{PAGE}: no Comparison table under {LISTED}"))?;
    let end = (first..page.len())
        .find(|&i| !page[i].starts_with('|'))
        .unwrap_or(page.len());
    Ok((first, end))
}

/// Why the page's table of listed differences is not the one `data` gives.
fn listed_errors(data: &[(String, Value)], page: &[&str]) -> Vec<String> {
    match listed_lines(page) {
        Ok((first, end)) if page[first..end] == listed_table(data) => Vec::new(),
        Ok(_) => vec![format!(
            "{PAGE}: the {LISTED} table is not what the registry lists; write it with \
             `cargo xtask status --write`"
        )],
        Err(error) => vec![error],
    }
}

/// The registry files under `root`, parsed, by their path from it.
fn registry_files(root: &Path) -> Result<Vec<(String, Value)>, String> {
    let mut files = Vec::new();
    for path in crate::files::walk(&root.join("harness"), &|rel| {
        rel.extension().is_some_and(|e| e == "json")
    }) {
        let rel = path
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .display()
            .to_string();
        let feature_cases = rel.starts_with("harness/features/")
            && (rel.ends_with("/corpus.json") || rel.ends_with("/pins.json"));
        if comparison(&rel).is_some() || feature_cases {
            files.push((rel, json::read(&path)?));
        }
    }
    Ok(files)
}

/// Why the page's tables differ from what [`ROWS`] renders from `data`, the parsed
/// JSON files under `harness/` by their path from the repository root.
pub fn check(root: &Path, data: &[(String, Value)]) -> Vec<String> {
    let Some((_, document)) = data.iter().find(|(path, _)| path == ROWS) else {
        return vec![format!("{ROWS}: missing")];
    };
    let rows = match Rows::parse(document) {
        Ok(rows) => rows,
        Err(errors) => return errors,
    };
    let index = data_index(data);
    let page = match fs::read_to_string(root.join(PAGE)) {
        Ok(page) => page,
        Err(error) => return vec![format!("{PAGE}: {error}")],
    };
    let lines: Vec<&str> = page.lines().collect();
    let rendered = match render(root, &rows, &index, &lines) {
        Ok(rendered) => rendered,
        Err(errors) => return errors,
    };
    let mut errors = listed_errors(data, &lines);
    for (table, cells) in &rendered {
        for ((line, written), cell) in table.rows.iter().zip(cells) {
            for (column, shown, name) in cell.cells(table) {
                if written.get(column).copied() != Some(shown) {
                    errors.push(format!(
                        "{PAGE}:{}: {} {name} is not what its results give: {shown}",
                        line + 1,
                        written.first().copied().unwrap_or_default()
                    ));
                }
            }
        }
    }
    errors
}

fn data_index(data: &[(String, Value)]) -> Index<'_> {
    data.iter()
        .filter_map(|(path, set)| {
            let name = path
                .strip_prefix(crate::evidence::SETS)?
                .strip_prefix('/')?
                .strip_suffix(".json")?;
            Some((name, set))
        })
        .collect()
}

/// Insert rows whose scope is supplied by the registry. Existing rows must
/// already be in registry order and are preserved with their editorial cells.
fn add_rows(rows: &Rows<'_>, page: &str) -> Result<String, String> {
    let original: Vec<_> = page.lines().collect();
    let mut lines: Vec<_> = original.iter().map(|line| (*line).to_owned()).collect();
    let mut insertions = Vec::new();
    for table in &rows.tables {
        let layout = page_table(&original, table.heading)?;
        let mut existing = layout.rows.iter().peekable();
        for row in rows
            .rows
            .iter()
            .filter(|row| row.targets.contains(&table.target))
        {
            if existing
                .peek()
                .is_some_and(|(_, cells)| cells.first() == Some(&row.area))
            {
                existing.next();
            } else if row.implemented.is_some() {
                let at = existing
                    .peek()
                    .map_or(layout.header + 2 + layout.rows.len(), |(line, _)| *line);
                let mut cells = vec![""; layout.width];
                cells[0] = row.area;
                insertions.push((at, format!("| {} |", cells.join(" | "))));
            }
        }
        if let Some((line, _)) = existing.next() {
            return Err(format!("{PAGE}:{}: row is out of registry order", line + 1));
        }
    }
    insertions.sort_by_key(|(line, _)| *line);
    for (at, row) in insertions.into_iter().rev() {
        lines.insert(at, row);
    }
    Ok(lines.join("\n"))
}

/// `cargo xtask status [--write]`: the page's tables with their Runtime and Memory
/// cells rendered from the evidence index, the open ones as written; with `--write`,
/// the page is rewritten with them instead.
pub fn tables(root: &Path, write: bool) -> Result<String, String> {
    let sets = crate::evidence::sets(root)?;
    let index: Index<'_> = sets
        .iter()
        .filter_map(|set| Some((text(set, "name")?, set)))
        .collect();
    let document = json::read(&root.join(ROWS))?;
    let rows = Rows::parse(&document).map_err(|errors| errors.join("\n"))?;
    let page = fs::read_to_string(root.join(PAGE)).map_err(|e| format!("{PAGE}: {e}"))?;
    let page = add_rows(&rows, &page)?;
    let lines: Vec<&str> = page.lines().collect();
    let rendered = render(root, &rows, &index, &lines).map_err(|errors| errors.join("\n"))?;
    let mut rewritten: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
    let mut printed = Vec::new();
    for ((table, cells), heading) in rendered.iter().zip(rows.tables.iter().map(|t| t.heading)) {
        printed.push(format!("## {heading}\n"));
        printed.push(lines[table.header].to_owned());
        printed.push(lines[table.header + 1].to_owned());
        for ((line, written), cell) in table.rows.iter().zip(cells) {
            if cell.cells(table).next().is_some() {
                let mut written: Vec<&str> = written.clone();
                for (column, shown, _) in cell.cells(table) {
                    written[column] = shown;
                }
                rewritten[*line] = format!("| {} |", written.join(" | "));
            }
            printed.push(rewritten[*line].clone());
        }
        printed.push(String::new());
    }
    let listed = listed_table(&registry_files(root)?);
    let (first, end) = listed_lines(&lines)?;
    rewritten.splice(first..end, listed.iter().cloned());
    printed.push(format!("## {LISTED}\n"));
    printed.extend(listed);
    printed.push(String::new());
    if write {
        let mut text = rewritten.join("\n");
        text.push('\n');
        fs::write(root.join(PAGE), text).map_err(|e| format!("{PAGE}: {e}"))?;
        return Ok(format!("{PAGE}: tables written"));
    }
    Ok(printed.join("\n"))
}

#[cfg(test)]
mod tests;
