//! The registry of differences: the lists in which a contract, a container pin file
//! or a codec manifest files every case whose result differs from the reference's,
//! each list a statement of what the difference is ([`Form`]), and how a listed case
//! is judged on a target ([`Listed`]). `docs/quality.md` ("When a result differs")
//! describes the forms; [`crate::harness::report::ratchet`] judges a run by them and
//! `cargo xtask check-design` checks the registry itself.

use std::collections::{BTreeMap, BTreeSet};

use crate::json::Value;

/// What a listed difference is. Each form is one list of the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Form {
    /// Our defect, not yet fixed.
    Defect,
    /// The input is valid and the crate README lists the feature as not implemented.
    Unimplemented,
    /// The reference departs from a cited clause and the candidate keeps to it.
    Departure,
    /// No standard decides: the candidate's documented rule differs from the
    /// reference's observed rule.
    Policy,
    /// A clause forbids the input or lets a reader refuse it, and the candidate does.
    Refusal,
}

impl Form {
    /// Every form, in the order the lists are read.
    pub const ALL: [Self; 5] = [
        Self::Defect,
        Self::Unimplemented,
        Self::Departure,
        Self::Policy,
        Self::Refusal,
    ];

    /// The forms whose entries name their cases under `cases`.
    pub const GROUPS: [Self; 4] = [
        Self::Unimplemented,
        Self::Departure,
        Self::Policy,
        Self::Refusal,
    ];

    /// The list that holds the form's entries.
    pub const fn list(self) -> &'static str {
        match self {
            Self::Defect => "open_cases",
            Self::Unimplemented => "open_unimplemented",
            Self::Departure => "open_deviations",
            Self::Policy => "open_policies",
            Self::Refusal => "open_refusals",
        }
    }

    /// The word the status page counts the form's cases under.
    pub const fn word(self) -> &'static str {
        match self {
            Self::Defect => "defect",
            Self::Unimplemented => "unimplemented",
            Self::Departure => "departs",
            Self::Policy => "policy",
            Self::Refusal => "refuses",
        }
    }

    /// The fields an entry states in one line each, and those it may state.
    const fn lines(self) -> (&'static [&'static str], &'static [&'static str]) {
        match self {
            Self::Defect => (&[], &[]),
            Self::Unimplemented => (
                &["feature", "documented", "clause", "candidate", "reference"],
                &["detail"],
            ),
            Self::Departure => (
                &["reading", "clause", "candidate", "reference"],
                &["compat"],
            ),
            Self::Policy => (
                &[
                    "policy",
                    "undecided",
                    "rule",
                    "documented",
                    "reference_rule",
                    "compat",
                ],
                &["detail"],
            ),
            Self::Refusal => (&["reason", "clause", "class", "detail"], &[]),
        }
    }

    /// The field that names an entry in a message.
    const fn title(self) -> &'static str {
        match self {
            Self::Defect => "id",
            Self::Unimplemented => "feature",
            Self::Departure => "reading",
            Self::Policy => "policy",
            Self::Refusal => "reason",
        }
    }
}

/// The list of the cases whose candidate fold cannot be recorded on the host it is
/// due on: `cases`, the `cause` in one line and optional `targets`.
pub const UNPINNED: &str = "open_unpinned";

/// The classes of `triergon_core::Error` a refusal may be of.
pub const CLASSES: [&str; 5] = ["invalid", "truncated", "unsupported", "limit", "policy"];

/// The class `detail` shows when it is a rendering of a `triergon_core::Error`: its
/// `Display` or its `Debug` form.
fn rendered_class(detail: &str) -> Option<&'static str> {
    let starts = |prefixes: &[&str]| prefixes.iter().any(|prefix| detail.starts_with(prefix));
    // A decoder's own error names its codec: `invalid HEVC data: …`.
    let decoder_invalid = detail
        .strip_prefix("invalid ")
        .and_then(|rest| rest.split_once(" data: "))
        .is_some_and(|(codec, _)| !codec.contains(' '));
    if decoder_invalid || starts(&["invalid data: ", "Invalid("]) {
        Some("invalid")
    } else if detail == "truncated input" || detail == "Truncated" {
        Some("truncated")
    } else if (detail.starts_with("capacity request ") && detail.contains(" exceeds policy "))
        || detail.starts_with("Limit {")
    {
        Some("limit")
    } else if starts(&["resource limit: ", "Policy("]) {
        Some("policy")
    } else if starts(&["unsupported: ", "Unsupported("]) {
        Some("unsupported")
    } else {
        None
    }
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// The field `key` of `entry` when it is one nonempty line.
fn line<'a>(entry: &'a Value, key: &str) -> Option<&'a str> {
    text(entry, key).filter(|text| !text.trim().is_empty() && !text.contains('\n'))
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).unwrap_or_default()
}

/// Where an entry's detail is read: on the candidate's result line, which names it
/// as `unsupported` when the report asks with `--report-refusal`, or in its trace.
/// A refusal has a detail; a feature not implemented, or a policy whose rule is to
/// refuse, may name the one its candidate reports.
fn evidence(entry: &Value) -> Result<bool, &'static str> {
    match entry.get("evidence").map(Value::as_str) {
        None | Some(Some("result")) => Ok(false),
        Some(Some("trace")) => Ok(true),
        Some(_) => Err("evidence is \"result\" or \"trace\""),
    }
}

/// Why `entry` is not an entry of `form`'s list: a field of the form that is missing
/// or not one line, a field of another form, or a refusal whose class its detail
/// does not show. `strict` also rejects a field the form does not have; the codec
/// manifests keep fields of their own.
pub fn entry_error(form: Form, entry: &Value, strict: bool) -> Option<String> {
    let list = form.list();
    let named = line(entry, form.title()).unwrap_or("an entry");
    let (required, optional) = form.lines();
    if let Some(missing) = required.iter().find(|key| line(entry, key).is_none()) {
        return Some(format!("{list}: {named}: {missing} is one nonempty line"));
    }
    if let Some(wrong) = optional
        .iter()
        .find(|key| entry.get(key).is_some() && line(entry, key).is_none())
    {
        return Some(format!("{list}: {named}: {wrong}, when given, is one line"));
    }
    let trace = match evidence(entry) {
        Ok(trace) => trace,
        Err(error) => return Some(format!("{list}: {named}: {error}")),
    };
    if trace && line(entry, "detail").is_none() {
        return Some(format!("{list}: {named}: trace evidence needs a detail"));
    }
    match form {
        Form::Defect | Form::Departure => {}
        Form::Policy => {
            let compat = line(entry, "compat").unwrap_or_default();
            let none = compat
                .strip_prefix("none: ")
                .is_some_and(|reason| !reason.trim().is_empty());
            if !none && !compat.contains("compat::") {
                return Some(format!(
                    "{list}: {named}: compat names the compat:: item that gives the \
                     reference's rule, or begins `none: ` and says why there is none"
                ));
            }
        }
        Form::Unimplemented => {
            if entry
                .get("not_comparable")
                .is_some_and(|flag| *flag != Value::Bool(true))
            {
                return Some(format!(
                    "{list}: {named}: not_comparable, when given, is true"
                ));
            }
        }
        Form::Refusal => {
            let (class, detail) = (
                text(entry, "class").unwrap_or_default(),
                text(entry, "detail").unwrap_or_default(),
            );
            // A bare detail is what a result line names of an `Unsupported` error; a
            // trace shows the error whole.
            let shown = match (rendered_class(detail), trace) {
                (Some(shown), _) => Some(shown),
                (None, false) => Some("unsupported"),
                (None, true) => None,
            };
            if !CLASSES.contains(&class) || shown != Some(class) {
                return Some(format!(
                    "{list}: {named}: class is one of {} and the one its detail shows{}",
                    CLASSES.join(", "),
                    if trace {
                        " (under trace evidence the detail is the error as the trace prints it)"
                    } else {
                        ""
                    }
                ));
            }
        }
    }
    if strict && let Value::Object(fields) = entry {
        let known = |key: &str| {
            required.contains(&key)
                || optional.contains(&key)
                || matches!(key, "cases" | "targets")
                || (form == Form::Defect && matches!(key, "id" | "requirement"))
                || (form == Form::Unimplemented && key == "not_comparable")
                || (matches!(form, Form::Unimplemented | Form::Policy | Form::Refusal)
                    && key == "evidence")
        };
        if let Some((key, _)) = fields.iter().find(|(key, _)| !known(key)) {
            return Some(format!("{list}: {named}: no field {key}"));
        }
    }
    None
}

/// Every entry of the lists that group cases, with its form, in list order.
pub fn groups(document: &Value) -> impl Iterator<Item = (Form, &Value)> {
    Form::GROUPS
        .into_iter()
        .flat_map(move |form| array(document, form.list()).iter().map(move |e| (form, e)))
}

/// The detail a listed case's candidate must name, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Named {
    /// The text: on a result line the whole `unsupported` value, in a trace a part
    /// of its output.
    pub detail: String,
    /// Whether it is read in the candidate's trace instead of on its result line.
    pub trace: bool,
}

/// How one listed case is judged on a target.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Case {
    /// The forms it is listed under, in [`Form::ALL`] order.
    pub forms: Vec<Form>,
    /// The detail its candidate must name.
    pub named: Option<Named>,
    /// Whether nothing of the reference is compared: a refusal, or a feature
    /// neither side implements.
    pub candidate_only: bool,
    /// Whether [`UNPINNED`] registers that its candidate fold is not yet recorded.
    pub unpinned: bool,
}

impl Case {
    /// Whether it is a refusal the specification justifies: the one listed form
    /// whose proof counts as a declared refusal.
    pub fn refusal(&self) -> bool {
        self.forms == [Form::Refusal]
    }

    /// The lists that name it.
    pub fn lists(&self) -> Vec<&'static str> {
        self.forms.iter().map(|form| form.list()).collect()
    }
}

/// The cases a registry file lists as differing from the reference on one target.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Listed {
    cases: BTreeMap<String, Case>,
    /// The ids in the order the lists name them first.
    order: Vec<String>,
}

impl Listed {
    /// Whether `id` is listed.
    pub fn contains(&self, id: &str) -> bool {
        self.cases.contains_key(id)
    }

    /// How `id` is judged, when it is listed.
    pub fn get(&self, id: &str) -> Option<&Case> {
        self.cases.get(id)
    }

    /// The listed ids, in the order the lists name them first.
    pub fn ids(&self) -> &[String] {
        &self.order
    }
}

/// Whether `entry` is open on `target`: on every target without `targets`, else on
/// those of `required` it names.
fn open_on(entry: &Value, required: &[Value], target: &str) -> Result<bool, &'static str> {
    let Some(scope) = entry.get("targets") else {
        return Ok(true);
    };
    let scope = scope.as_array().unwrap_or_default();
    if scope.is_empty() || scope.iter().any(|t| !required.contains(t)) {
        return Err("its targets must be some of the required targets");
    }
    Ok(scope.iter().any(|t| t.as_str() == Some(target)))
}

/// The cases `document` (a contract, or the lists of a container pin file with the
/// `required_targets` they are scoped by) lists on `target`, among the cases of
/// `manifest`. Every entry has the fields of its form ([`entry_error`]); every id is
/// a case of the manifest, named once in a list; a case may be listed in one entry
/// of each of `open_cases`, `open_unimplemented`, `open_deviations` and
/// `open_policies` when it has more than one cause, and a refusal or a case not
/// comparable with the reference in no other list. An entry with `targets` is open
/// on those of the contract's `required_targets` only, and its cases must pass
/// elsewhere.
pub fn read(document: &Value, manifest: &Value, target: &str) -> Result<Listed, String> {
    let ids: BTreeSet<&str> = array(manifest, "cases")
        .iter()
        .filter_map(|case| text(case, "id"))
        .collect();
    listed(document, Some(&ids), target)
}

/// [`read`] without a manifest to check the listed ids against.
pub fn read_unchecked(document: &Value, target: &str) -> Result<Listed, String> {
    listed(document, None, target)
}

fn listed(document: &Value, ids: Option<&BTreeSet<&str>>, target: &str) -> Result<Listed, String> {
    let required = array(document, "required_targets");
    let pending = array(document, "pending_module_requirements");
    let known = |id: &str| ids.is_none_or(|ids| ids.contains(id));
    let mut listed = Listed::default();
    let mut seen = BTreeSet::new();
    for form in Form::ALL {
        let list = form.list();
        for entry in array(document, list) {
            if let Some(error) = entry_error(form, entry, true) {
                return Err(error);
            }
            let named = line(entry, form.title()).unwrap_or_default();
            let cases: Vec<&str> = if form == Form::Defect {
                let (Some(id), Some(requirement)) = (text(entry, "id"), entry.get("requirement"))
                else {
                    return Err(format!(
                        "{list}: an open case needs an id and a requirement"
                    ));
                };
                if !pending.contains(requirement) {
                    return Err(format!("{list}: {id}: its requirement is not pending"));
                }
                vec![id]
            } else {
                let cases = array(entry, "cases");
                if cases.is_empty() {
                    return Err(format!("{list}: {named}: it names no case"));
                }
                cases
                    .iter()
                    .map(|id| id.as_str())
                    .collect::<Option<_>>()
                    .ok_or_else(|| format!("{list}: {named}: a case is not an id"))?
            };
            let open = open_on(entry, required, target)
                .map_err(|error| format!("{list}: {named}: {error}"))?;
            let detail = (form != Form::Defect)
                .then(|| line(entry, "detail"))
                .flatten()
                .map(|detail| Named {
                    detail: detail.to_owned(),
                    trace: evidence(entry) == Ok(true),
                });
            let candidate_only =
                form == Form::Refusal || entry.get("not_comparable") == Some(&Value::Bool(true));
            for id in cases {
                if !known(id) {
                    return Err(format!("{list}: {id}: not a case of the manifest"));
                }
                if !seen.insert((form, id)) {
                    return Err(format!("{list}: {id}: listed twice"));
                }
                if !open {
                    continue;
                }
                if !listed.cases.contains_key(id) {
                    listed.order.push(id.to_owned());
                }
                let case = listed.cases.entry(id.to_owned()).or_default();
                case.forms.push(form);
                case.candidate_only |= candidate_only;
                if detail.is_some() {
                    case.named.clone_from(&detail);
                }
            }
        }
    }
    if let Some((id, _)) = listed
        .cases
        .iter()
        .find(|(_, case)| case.candidate_only && case.forms.len() > 1)
    {
        return Err(format!(
            "{id}: a refusal, or a case not comparable with the reference, is listed in no \
             other list"
        ));
    }
    for entry in array(document, UNPINNED) {
        if line(entry, "cause").is_none() {
            return Err(format!("{UNPINNED}: an entry needs a one-line cause"));
        }
        let open = open_on(entry, required, target).map_err(|e| format!("{UNPINNED}: {e}"))?;
        let cases = array(entry, "cases");
        if cases.is_empty() {
            return Err(format!("{UNPINNED}: an entry names no case"));
        }
        for id in cases {
            let id = id.as_str().unwrap_or_default();
            if !known(id) {
                return Err(format!("{UNPINNED}: {id}: not a case of the manifest"));
            }
            if !open {
                continue;
            }
            match listed.cases.get_mut(id) {
                Some(case) => case.unpinned = true,
                None => {
                    return Err(format!(
                        "{UNPINNED}: {id}: not a case another list names on {target}"
                    ));
                }
            }
        }
    }
    Ok(listed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    const TARGET: &str = "t";

    fn manifest() -> Value {
        parse(r#"{"cases": [{"id": "a"}, {"id": "b"}, {"id": "c"}]}"#).unwrap()
    }

    fn contract(lists: &str) -> Value {
        parse(&format!(
            r#"{{"required_targets": ["t", "u"],
                "pending_module_requirements": ["fix it"], {lists}}}"#
        ))
        .unwrap()
    }

    const DEVIATION: &str =
        r#"{"reading": "r", "clause": "§1", "candidate": "x", "reference": "y", "cases": ["a"]}"#;
    const POLICY: &str = r#"{"policy": "q", "undecided": "§2 defines the field and not the recovery",
        "rule": "ours", "documented": "crates/x/README.md#rules", "reference_rule": "theirs",
        "compat": "none: the reference's choice is a heuristic", "cases": ["a"]}"#;

    #[test]
    fn each_form_has_its_own_fields() {
        let entry = |text: &str| parse(text).unwrap();
        assert_eq!(entry_error(Form::Departure, &entry(DEVIATION), true), None);
        assert_eq!(entry_error(Form::Policy, &entry(POLICY), true), None);
        // A deviation's fields do not make a policy, nor the reverse.
        assert!(entry_error(Form::Policy, &entry(DEVIATION), true).is_some());
        assert!(entry_error(Form::Departure, &entry(POLICY), true).is_some());
        // A policy says whether compat offers the reference's rule.
        for (compat, holds) in [
            ("triergon_x::compat::pes::Observer", true),
            ("none: ", false),
            ("no policy exists", false),
        ] {
            let policy = POLICY.replace("none: the reference's choice is a heuristic", compat);
            assert_eq!(
                entry_error(Form::Policy, &entry(&policy), true).is_none(),
                holds,
                "{compat}"
            );
        }
        let unimplemented = r#"{"feature": "f", "documented": "crates/x/README.md#scope",
            "clause": "§3", "candidate": "refuses", "reference": "decodes", "cases": ["a"]}"#;
        assert_eq!(
            entry_error(Form::Unimplemented, &entry(unimplemented), true),
            None
        );
        let stale =
            unimplemented.replace(r#""cases""#, r#""reference_pin_targets": ["t"], "cases""#);
        assert_eq!(
            entry_error(Form::Unimplemented, &entry(&stale), true).unwrap(),
            "open_unimplemented: f: no field reference_pin_targets"
        );
        // The codec manifests keep fields of their own.
        assert_eq!(
            entry_error(Form::Unimplemented, &entry(&stale), false),
            None
        );
        let two_lines = DEVIATION.replace(r#""§1""#, r#""§1\n§2""#);
        assert!(entry_error(Form::Departure, &entry(&two_lines), true).is_some());
    }

    #[test]
    fn a_refusal_states_the_class_its_detail_shows() {
        let refusal = |class: &str, detail: &str, evidence: &str| {
            parse(&format!(
                r#"{{"reason": "r", "clause": "§4", "class": "{class}", "detail": "{detail}",
                    {evidence} "cases": ["a"]}}"#
            ))
            .unwrap()
        };
        let trace = r#""evidence": "trace","#;
        for (class, detail, evidence, holds) in [
            ("unsupported", "byte-reversed UTF-16", "", true),
            ("invalid", "byte-reversed UTF-16", "", false),
            ("invalid", "invalid data: PES timestamp markers", "", true),
            ("invalid", "invalid HEVC data: bitstream overrun", "", true),
            (
                "unsupported",
                "invalid data: PES timestamp markers",
                "",
                false,
            ),
            (
                "limit",
                "capacity request 33216 exceeds policy 16384",
                "",
                true,
            ),
            ("truncated", "truncated input", trace, true),
            (
                "invalid",
                r#"Invalid(\"zero-length NAL unit\")"#,
                trace,
                true,
            ),
            // A trace shows the error whole: a bare detail does not show its class.
            ("unsupported", "byte-reversed UTF-16", trace, false),
            (
                "unsupported",
                "unsupported: byte-reversed UTF-16",
                trace,
                true,
            ),
            ("fatal", "byte-reversed UTF-16", "", false),
        ] {
            assert_eq!(
                entry_error(Form::Refusal, &refusal(class, detail, evidence), true).is_none(),
                holds,
                "{class} {detail} {evidence}"
            );
        }
        let other = refusal("unsupported", "x", r#""evidence": "stderr","#);
        assert!(entry_error(Form::Refusal, &other, true).is_some());
    }

    #[test]
    fn a_case_is_listed_once_in_a_list_and_may_have_two_causes() {
        let both = contract(&format!(
            r#""open_deviations": [{DEVIATION}], "open_policies": [{POLICY}]"#
        ));
        let listed = read(&both, &manifest(), TARGET).unwrap();
        assert_eq!(listed.ids(), ["a"]);
        assert_eq!(
            listed.get("a").unwrap().lists(),
            ["open_deviations", "open_policies"]
        );
        assert!(!listed.get("a").unwrap().candidate_only);
        let twice = contract(&format!(r#""open_deviations": [{DEVIATION}, {DEVIATION}]"#));
        assert_eq!(
            read(&twice, &manifest(), TARGET).unwrap_err(),
            "open_deviations: a: listed twice"
        );
        let stranger = contract(&format!(
            r#""open_deviations": [{}]"#,
            DEVIATION.replace(r#"["a"]"#, r#"["z"]"#)
        ));
        assert_eq!(
            read(&stranger, &manifest(), TARGET).unwrap_err(),
            "open_deviations: z: not a case of the manifest"
        );
    }

    #[test]
    fn a_refusal_is_listed_nowhere_else_and_compares_no_reference() {
        let refusal = r#"{"reason": "r", "clause": "§4", "class": "unsupported",
            "detail": "d", "cases": ["a"]}"#;
        let alone = contract(&format!(r#""open_refusals": [{refusal}]"#));
        let listed = read(&alone, &manifest(), TARGET).unwrap();
        let case = listed.get("a").unwrap();
        assert!(case.refusal() && case.candidate_only);
        assert_eq!(
            case.named,
            Some(Named {
                detail: "d".into(),
                trace: false
            })
        );
        let also = contract(&format!(
            r#""open_refusals": [{refusal}], "open_deviations": [{DEVIATION}]"#
        ));
        assert!(read(&also, &manifest(), TARGET).is_err());
        // A feature neither side implements is not a refusal, and compares no
        // reference either.
        let neither = contract(
            r#""open_unimplemented": [{"feature": "f", "documented": "crates/x/README.md#scope",
                "clause": "§3", "candidate": "refuses", "reference": "conceals",
                "detail": "d", "not_comparable": true, "cases": ["b"]}]"#,
        );
        let listed = read(&neither, &manifest(), TARGET).unwrap();
        let case = listed.get("b").unwrap();
        assert!(case.candidate_only && !case.refusal());
    }

    #[test]
    fn entries_are_scoped_to_targets_and_open_cases_to_requirements() {
        let scoped = |targets: &str| {
            contract(&format!(
                r#""open_cases": [{{"id": "b", "requirement": "fix it", "targets": {targets}}}],
                    "open_unpinned": [{{"cases": ["b"], "cause": "recorded on that host only",
                    "targets": {targets}}}]"#
            ))
        };
        let listed = read(&scoped(r#"["t"]"#), &manifest(), TARGET).unwrap();
        assert!(listed.get("b").unwrap().unpinned);
        assert_eq!(listed.get("b").unwrap().forms, [Form::Defect]);
        assert!(
            !read(&scoped(r#"["u"]"#), &manifest(), TARGET)
                .unwrap()
                .contains("b")
        );
        for wrong in ["[]", r#"["v"]"#] {
            assert!(
                read(&scoped(wrong), &manifest(), TARGET).is_err(),
                "{wrong}"
            );
        }
        let other = contract(r#""open_cases": [{"id": "b", "requirement": "another"}]"#);
        assert!(read(&other, &manifest(), TARGET).is_err());
        // A missing pin is registered for a listed case only.
        let unlisted = contract(r#""open_unpinned": [{"cases": ["c"], "cause": "x"}]"#);
        assert!(read(&unlisted, &manifest(), TARGET).is_err());
    }
}
