//! The status tables of `docs/status.md`, from data. [`ROWS`] names each table (its
//! heading, gate target and default run) and each row (its area, the targets whose
//! tables show it, and the gate results its Runtime and Memory cells show); the cells
//! are rendered from those results' measured records in the evidence index
//! ([`Measured`]). `cargo xtask check-design` renders them again and fails on any
//! difference; `cargo xtask status` prints the tables as rendered, or writes them.
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
    Parts(Vec<Part<'a>>),
}

struct Row<'a> {
    area: &'a str,
    targets: Vec<&'a str>,
    /// The runs the row cites instead of its tables' runs, one per target at most.
    runs: Vec<&'a str>,
    cells: Cells<'a>,
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
            let cells = match (text(row, "text"), row.get("results"), row.get("parts")) {
                (Some(shown), None, None) => Cells::Text(shown),
                (None, Some(_), None) => Cells::Parts(vec![Part {
                    label: None,
                    results: strings(row, "results").unwrap_or_default(),
                    targets: None,
                }]),
                (None, None, Some(Value::Array(list))) => {
                    let parts = parts(list, &mut error);
                    if parts.iter().any(|p| p.label.is_none_or(str::is_empty)) {
                        error(format!("{area}: each of several parts has a label"));
                    }
                    Cells::Parts(parts)
                }
                _ => {
                    error(format!("{area}: one of text, results and parts"));
                    continue;
                }
            };
            if let Cells::Parts(parts) = &cells
                && parts.iter().any(|p| p.results.is_empty())
            {
                error(format!("{area}: names no results"));
            }
            rows.push(Row {
                area,
                targets: row_targets,
                runs: row_runs,
                cells,
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
    columns: (usize, usize),
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
        columns,
        rows,
    })
}

/// Each table's rows as [`ROWS`] renders them, beside the page's: per table, the page's
/// row lines and the rendered cells of each (`None` where open).
type Rendered<'p> = Vec<(PageTable<'p>, Vec<Option<(String, String)>>)>;

fn render<'p>(
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
            match rows.cells(index, table, row) {
                Ok(cell) => cells.push(cell),
                Err(row_errors) => {
                    errors.extend(row_errors);
                    cells.push(None);
                }
            }
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
    let rendered = match render(&rows, &index, &lines) {
        Ok(rendered) => rendered,
        Err(errors) => return errors,
    };
    let mut errors = Vec::new();
    for (table, cells) in &rendered {
        for ((line, written), cell) in table.rows.iter().zip(cells) {
            let Some((runtime, memory)) = cell else {
                continue;
            };
            let (r, m) = table.columns;
            for (column, shown, name) in [(r, runtime, "Runtime"), (m, memory, "Memory")] {
                if written.get(column).copied() != Some(shown.as_str()) {
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

/// `cargo xtask status [--write]`: the page's tables with their Runtime and Memory
/// cells rendered from the evidence index, the open ones as written; with `--write`,
/// the page is rewritten with them instead.
pub fn tables(root: &Path, write: bool) -> Result<String, String> {
    let sets = crate::evidence::sets(root)?;
    let index: Index<'_> = sets
        .iter()
        .filter_map(|set| Some((text(set, "name")?, set)))
        .collect();
    let document = json::read(&root.join(ROWS)).map_err(|e| format!("{ROWS}: {e}"))?;
    let rows = Rows::parse(&document).map_err(|errors| errors.join("\n"))?;
    let page = fs::read_to_string(root.join(PAGE)).map_err(|e| format!("{PAGE}: {e}"))?;
    let lines: Vec<&str> = page.lines().collect();
    let rendered = render(&rows, &index, &lines).map_err(|errors| errors.join("\n"))?;
    let mut rewritten: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
    let mut printed = Vec::new();
    for ((table, cells), heading) in rendered.iter().zip(rows.tables.iter().map(|t| t.heading)) {
        printed.push(format!("## {heading}\n"));
        printed.push(lines[table.header].to_owned());
        printed.push(lines[table.header + 1].to_owned());
        for ((line, written), cell) in table.rows.iter().zip(cells) {
            if let Some((runtime, memory)) = cell {
                let mut written: Vec<&str> = written.clone();
                written[table.columns.0] = runtime;
                written[table.columns.1] = memory;
                rewritten[*line] = format!("| {} |", written.join(" | "));
            }
            printed.push(rewritten[*line].clone());
        }
        printed.push(String::new());
    }
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
