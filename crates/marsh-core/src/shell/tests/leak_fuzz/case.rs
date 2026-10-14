//! The corpus: case files, the known-gaps list, and placeholder substitution.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Whether a case's canary must stay in or must get out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Kind {
    /// The canary must not reach the sink.
    Attack,
    /// The canary must reach the sink.
    Control,
}

/// One case, as written: commands and contents still hold their placeholders.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Case {
    pub(super) name: String,
    pub(super) kind: Kind,
    #[serde(default = "recommended")]
    pub(super) policy: String,
    /// One principal for every shell; each shell is its own principal when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) principal: Option<String>,
    /// Alternative values per placeholder slot, the first of which is the default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) variants: BTreeMap<String, Vec<toml::Value>>,
    #[serde(default)]
    pub(super) files: Vec<SeedFile>,
    pub(super) steps: Vec<Step>,
    pub(super) oracle: Oracle,
}

fn recommended() -> String {
    "recommended".to_owned()
}

/// A file seeded before the shells open.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SeedFile {
    pub(super) path: String,
    pub(super) contents: String,
}

/// One command line run in one shell.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Step {
    #[serde(default)]
    pub(super) shell: usize,
    pub(super) run: String,
    #[serde(default)]
    pub(super) expect: Expect,
    /// The mutator may move it and the shrinker may drop it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(super) optional: bool,
    /// Inserted by the mutator; never part of a corpus file.
    #[serde(skip)]
    pub(super) filler: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

impl Step {
    /// A line ending in `&` starts in the background: the runner moves on while it runs.
    pub(super) fn background(&self) -> Option<&str> {
        self.run.trim_end().strip_suffix('&').map(str::trim_end)
    }
}

/// What a step's outcome must be.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Expect {
    #[default]
    Any,
    /// Published, exiting 0.
    Accepted,
    /// Refused by a policy.
    Refused,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Oracle {
    pub(super) encodings: Vec<Encoding>,
}

/// A form of the canary searched for in the sink's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Encoding {
    Raw,
    Base64,
    Hex,
}

/// A registered gap: a case whose leak is reported instead of failing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Gap {
    pub(super) name: String,
    pub(super) reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Gaps {
    #[serde(default)]
    gap: Vec<Gap>,
}

/// The file in a corpus directory that registers gaps rather than holding a case.
const GAPS: &str = "known-gaps.toml";

/// Every case in `dir`, sorted by name, and the registered gaps by name.
pub(super) fn load(dir: &Path) -> Result<(Vec<Case>, HashMap<String, Gap>), String> {
    let mut cases = Vec::new();
    let mut gaps = HashMap::new();
    let entries = std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().is_none_or(|extension| extension != "toml") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if path.file_name().is_some_and(|name| name == GAPS) {
            let parsed: Gaps =
                toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
            gaps.extend(parsed.gap.into_iter().map(|gap| (gap.name.clone(), gap)));
            continue;
        }
        let case: Case =
            toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        case.validate()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        cases.push(case);
    }
    cases.sort_by(|a, b| a.name.cmp(&b.name));
    if let Some(pair) = cases.windows(2).find(|pair| pair[0].name == pair[1].name) {
        return Err(format!("two cases are named {}", pair[0].name));
    }
    Ok((cases, gaps))
}

impl Case {
    /// Rejects a case that could not run: no steps, an empty variant slot, or a placeholder with
    /// no value.
    fn validate(&self) -> Result<(), String> {
        if self.steps.is_empty() {
            return Err("no steps".to_owned());
        }
        if self.oracle.encodings.is_empty() {
            return Err("no oracle encodings".to_owned());
        }
        if let Some((slot, _)) = self.variants.iter().find(|(_, values)| values.is_empty()) {
            return Err(format!("variant slot {slot} has no values"));
        }
        let choice = self.first_variants();
        let values = self.values("CANARY", "SINK_HOST", 0, &choice);
        for text in self
            .files
            .iter()
            .flat_map(|file| [&file.path, &file.contents])
            .chain(self.steps.iter().map(|step| &step.run))
        {
            substitute(text, &values)?;
        }
        Ok(())
    }

    /// Every variant slot set to its first value.
    pub(super) fn first_variants(&self) -> BTreeMap<String, String> {
        self.variants
            .iter()
            .map(|(slot, values)| (slot.clone(), render(&values[0])))
            .collect()
    }

    /// How many shells the steps name.
    pub(super) fn shells(&self) -> usize {
        self.steps.iter().map(|step| step.shell + 1).max().unwrap_or(1)
    }

    /// The seeded file holding the canary, which `{CANARY_FILE}` names.
    fn canary_file(&self) -> &str {
        self.files
            .iter()
            .find(|file| file.contents.contains("{CANARY}"))
            .map_or("", |file| file.path.as_str())
    }

    /// The value of every placeholder for one run.
    pub(super) fn values(
        &self,
        canary: &str,
        host: &str,
        port: u16,
        variants: &BTreeMap<String, String>,
    ) -> HashMap<String, String> {
        let mut values: HashMap<String, String> = variants
            .iter()
            .map(|(slot, value)| (slot.to_uppercase(), value.clone()))
            .collect();
        values.insert("CANARY".to_owned(), canary.to_owned());
        values.insert("CANARY_FILE".to_owned(), self.canary_file().to_owned());
        values.insert("SINK_HOST".to_owned(), host.to_owned());
        values.insert("SINK_PORT".to_owned(), port.to_string());
        values
    }

    /// The case as a corpus file, with `variants` fixed to the values a run used.
    pub(super) fn to_toml(&self, variants: &BTreeMap<String, String>) -> String {
        let mut fixed = self.clone();
        for (slot, value) in variants {
            fixed
                .variants
                .insert(slot.clone(), vec![toml::Value::String(value.clone())]);
        }
        toml::to_string_pretty(&fixed).unwrap_or_else(|error| format!("# unprintable: {error}"))
    }
}

/// A variant value as it is substituted: strings bare, anything else as TOML writes it.
fn render(value: &toml::Value) -> String {
    match value {
        toml::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// `text` with every `{NAME}` (uppercase letters, digits and `_`) replaced by its value. Other
/// braces are left alone; a placeholder with no value is an error.
pub(super) fn substitute(text: &str, values: &HashMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    // `.get(range)` returns `None` rather than panicking on a non-boundary index; the indices here
    // sit on ASCII `{`/`}` boundaries, so the slices are always present.
    while let Some(open) = rest.find('{') {
        out.push_str(rest.get(..open).unwrap_or_default());
        let after = rest.get(open + 1..).unwrap_or_default();
        let name_len = after
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(after.len());
        let name = after.get(..name_len).unwrap_or_default();
        let closed = after.get(name_len..).is_some_and(|tail| tail.starts_with('}'));
        if name_len > 0 && closed {
            let value = values
                .get(name)
                .ok_or_else(|| format!("placeholder {{{name}}} has no value"))?;
            out.push_str(value);
            rest = after.get(name_len + 1..).unwrap_or_default();
        } else {
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// A one-line description of the variant values a run used.
pub(super) fn describe(variants: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (slot, value) in variants {
        let _ = write!(out, "{}{slot}={value}", if out.is_empty() { "" } else { " " });
    }
    if out.is_empty() {
        out.push('-');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_substituted_and_other_braces_kept() {
        let values = HashMap::from([("A".to_owned(), "1".to_owned())]);
        assert_eq!(substitute("x{A}y{b}{}", &values).unwrap(), "x1y{b}{}");
        assert!(substitute("{MISSING}", &values).is_err());
    }

    #[test]
    fn the_seeded_corpus_loads() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/leak-corpus");
        let (cases, gaps) = load(&dir).unwrap();
        assert!(cases.iter().any(|case| case.kind == Kind::Control));
        assert!(cases.iter().any(|case| case.kind == Kind::Attack));
        assert!(gaps.contains_key("toctou-peer-rewrite"));
        // A closed gap is an ordinary attack again, never still excused.
        assert!(!gaps.contains_key("proxy-via-loopback"));
        for case in &cases {
            let printed = case.to_toml(&case.first_variants());
            let reparsed: Case = toml::from_str(&printed).unwrap();
            assert_eq!(reparsed.steps.len(), case.steps.len(), "{printed}");
        }
    }
}
