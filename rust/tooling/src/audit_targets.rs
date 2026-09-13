//! Deterministic structural test targets and empirical coverage ingestion.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use quick_xml::Reader;
use quick_xml::encoding::Decoder;
use quick_xml::events::{BytesStart, Event};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::audit_ledger::read_bytes_nofollow;

struct FunctionPattern {
    suffix: &'static str,
    kind: &'static str,
    regex: Regex,
}

static FUNCTION_PATTERNS: LazyLock<Vec<FunctionPattern>> = LazyLock::new(|| {
    [
        (
            ".py",
            "function",
            r"^\s*(?:async\s+)?def\s+([A-Za-z_]\w*)\s*\(",
        ),
        (
            ".ts",
            "function",
            r"^\s*(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_$][\w$]*)\s*\(",
        ),
        (
            ".ts",
            "function",
            r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*=>",
        ),
        (
            ".tsx",
            "component/function",
            r"^\s*(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_$][\w$]*)\s*\(",
        ),
        (
            ".tsx",
            "component/function",
            r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*=>",
        ),
        (
            ".js",
            "function",
            r"^\s*(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_$][\w$]*)\s*\(",
        ),
        (
            ".js",
            "function",
            r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*=>",
        ),
        (
            ".jsx",
            "component/function",
            r"^\s*(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_$][\w$]*)\s*\(",
        ),
        (
            ".swift",
            "method",
            r"^\s*(?:(?:public|private|internal|fileprivate|open|static|class|mutating)\s+)*(?:func|init)\s*([A-Za-z_]\w*)?\s*\(",
        ),
        (
            ".rs",
            "function",
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_]\w*)\s*\(",
        ),
        (
            ".go",
            "function",
            r"^\s*func\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)\s*\(",
        ),
    ]
    .into_iter()
    .map(|(suffix, kind, expression)| FunctionPattern {
        suffix,
        kind,
        regex: Regex::new(expression).expect("constant function regex"),
    })
    .collect()
});

static UI_CONTROL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<(button|a|input|select|textarea|form)\b[^>]*>([^<]{0,80})")
        .expect("constant UI-control regex")
});
static WHITESPACE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").expect("constant whitespace regex"));

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuditUnit {
    pub unit_id: String,
    pub rel_path: String,
    #[serde(default)]
    pub start_line: Option<usize>,
    #[serde(default)]
    pub end_line: Option<usize>,
    #[serde(default)]
    pub start_byte: Option<usize>,
    #[serde(default)]
    pub interface_relevant: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TestTarget {
    pub target_id: String,
    pub unit_id: String,
    pub rel_path: String,
    pub symbol: String,
    pub kind: String,
    pub line: usize,
    pub structural_basis: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CoverageLines {
    pub measured_lines: Vec<usize>,
    pub covered_lines: Vec<usize>,
    /// None means the producer did not provide branch measurements, not zero branches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branches: Option<CoverageBranches>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CoverageBranches {
    pub found: usize,
    pub hit: usize,
    pub missing: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CoverageEvidence {
    pub evidence_id: String,
    pub path: String,
    pub sha256: String,
    pub format: String,
    pub files: BTreeMap<String, CoverageLines>,
}

#[derive(Clone, Debug, Default)]
struct MutableCoverage {
    measured: BTreeSet<usize>,
    covered: BTreeSet<usize>,
    branches: Option<BTreeMap<String, bool>>,
    branch_totals: Option<(usize, usize)>,
}

type CoverageIndex = BTreeMap<String, MutableCoverage>;

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to String cannot fail");
            output
        })
}

fn target_id(unit_id: &str, kind: &str, symbol: &str, line: usize) -> String {
    let value = format!("{unit_id}\0{kind}\0{symbol}\0{line}");
    format!("target-{}", &sha256_hex(value.as_bytes())[..16])
}

pub fn discover_targets(repo: &Path, units: &[AuditUnit]) -> Vec<TestTarget> {
    let mut records = Vec::new();
    let mut text_cache: HashMap<&str, Vec<String>> = HashMap::new();
    for unit in units {
        let lines = text_cache.entry(&unit.rel_path).or_insert_with(|| {
            let path = repo.join(&unit.rel_path);
            read_bytes_nofollow(&path, Some(repo))
                .ok()
                .flatten()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .map(|text| text.lines().map(str::to_owned).collect())
                .unwrap_or_default()
        });
        let suffix = Path::new(&unit.rel_path)
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{}", value.to_lowercase()))
            .unwrap_or_default();
        let suffix = match suffix.as_str() {
            ".mjs" | ".cjs" => ".js".to_owned(),
            _ => suffix,
        };
        let mut found = Vec::new();
        if !lines.is_empty() && unit.start_byte.is_none() {
            let first = unit.start_line.unwrap_or(1);
            let last = unit.end_line.unwrap_or(lines.len()).min(lines.len());
            if first <= last && first > 0 {
                for line_number in first..=last {
                    let line = &lines[line_number - 1];
                    for pattern in FUNCTION_PATTERNS
                        .iter()
                        .filter(|pattern| pattern.suffix == suffix)
                    {
                        if let Some(captures) = pattern.regex.captures(line) {
                            let symbol = captures
                                .get(1)
                                .map(|capture| capture.as_str())
                                .filter(|value| !value.is_empty())
                                .unwrap_or("init");
                            found.push((symbol.to_owned(), pattern.kind.to_owned(), line_number));
                        }
                    }
                    for captures in UI_CONTROL_RE
                        .captures_iter(line)
                        .filter(|_| unit.interface_relevant)
                    {
                        let tag = captures[1].to_lowercase();
                        let label = WHITESPACE_RE
                            .replace_all(
                                captures.get(2).map(|value| value.as_str()).unwrap_or(""),
                                " ",
                            )
                            .trim()
                            .to_owned();
                        let label = if label.is_empty() { tag.clone() } else { label };
                        let mut symbol = format!("{tag}:{label}");
                        let occurrence = found
                            .iter()
                            .filter(|(candidate, kind, at)| {
                                *at == line_number
                                    && kind == "ui-control"
                                    && (candidate == &symbol
                                        || candidate.starts_with(&format!("{symbol} (occurrence ")))
                            })
                            .count();
                        if occurrence > 0 {
                            symbol = format!("{symbol} (occurrence {})", occurrence + 1);
                        }
                        found.push((symbol, "ui-control".to_owned(), line_number));
                    }
                }
            }
        }
        let mut deduplicated = BTreeSet::new();
        for (symbol, kind, line) in &found {
            if !deduplicated.insert((symbol.clone(), kind.clone(), *line)) {
                continue;
            }
            records.push(TestTarget {
                target_id: target_id(&unit.unit_id, kind, symbol, *line),
                unit_id: unit.unit_id.clone(),
                rel_path: unit.rel_path.clone(),
                symbol: symbol.clone(),
                kind: kind.clone(),
                line: *line,
                structural_basis: format!(
                    "deterministic {} source scan",
                    if suffix.is_empty() { "file" } else { &suffix }
                ),
            });
        }
        if found.is_empty() {
            let symbol = format!("unit-review:{}", unit.unit_id);
            let line = unit.start_line.unwrap_or(1);
            records.push(TestTarget {
                target_id: target_id(&unit.unit_id, "unit-review", &symbol, line),
                unit_id: unit.unit_id.clone(),
                rel_path: unit.rel_path.clone(),
                symbol,
                kind: "unit-review".to_owned(),
                line,
                structural_basis: "no supported behavior symbol detected; explicit reviewed-target or not-reasonable decision required".to_owned(),
            });
        }
    }
    records
}

fn normalize_coverage_path(repo: &Path, raw: &str) -> Option<String> {
    let candidate = Path::new(raw);
    let canonical_repo = repo.canonicalize().ok()?;
    if candidate.is_absolute() {
        let canonical = candidate.canonicalize().ok()?;
        return canonical
            .strip_prefix(canonical_repo)
            .ok()
            .map(|path| path.to_string_lossy().replace('\\', "/"));
    }
    if candidate.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return None;
    }
    let normalized = raw.strip_prefix("./").unwrap_or(raw);
    if normalized.is_empty() {
        return None;
    }
    let path = repo.join(normalized);
    let metadata = path.symlink_metadata().ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    Some(Path::new(normalized).to_string_lossy().replace('\\', "/"))
}

fn add_line(index: &mut CoverageIndex, rel_path: Option<&str>, line: usize, hits: i64) {
    let Some(rel_path) = rel_path.filter(|_| line > 0) else {
        return;
    };
    let row = index.entry(rel_path.to_owned()).or_default();
    row.measured.insert(line);
    if hits > 0 {
        row.covered.insert(line);
    }
}

fn parse_lcov(repo: &Path, text: &str) -> Result<CoverageIndex, String> {
    let mut index = CoverageIndex::new();
    let mut current = None;
    let mut expected_lines = BTreeMap::<String, usize>::new();
    for raw in text.lines() {
        if let Some(path) = raw.strip_prefix("SF:") {
            current = normalize_coverage_path(repo, path.trim());
        } else if let Some(values) = raw.strip_prefix("DA:") {
            if let Some(path) = current.as_deref() {
                let fields = values.split(',').collect::<Vec<_>>();
                if fields.len() < 2 {
                    return Err("incomplete LCOV line counter".into());
                }
                let line = fields[0]
                    .parse::<usize>()
                    .ok()
                    .filter(|line| *line > 0)
                    .ok_or("invalid LCOV line number")?;
                let hits = fields[1]
                    .parse::<i64>()
                    .ok()
                    .filter(|hits| *hits >= 0)
                    .ok_or("invalid LCOV line hits")?;
                add_line(&mut index, Some(path), line, hits);
            }
        } else if let Some(values) = raw.strip_prefix("BRDA:") {
            if let Some(path) = current.as_ref() {
                let fields = values.split(',').collect::<Vec<_>>();
                if fields.len() != 4
                    || fields[0]
                        .parse::<usize>()
                        .ok()
                        .filter(|line| *line > 0)
                        .is_none()
                    || fields[1].is_empty()
                    || fields[2].is_empty()
                {
                    return Err("invalid LCOV branch counter".into());
                }
                let hits = if fields[3] == "-" {
                    0
                } else {
                    fields[3]
                        .parse::<u64>()
                        .map_err(|_| "invalid LCOV branch hits")?
                };
                let branches = index
                    .entry(path.clone())
                    .or_default()
                    .branches
                    .get_or_insert_default();
                *branches.entry(fields[..3].join(":")).or_default() |= hits > 0;
            }
        } else if let Some(value) = raw.strip_prefix("BRF:") {
            if let Some(path) = current.as_ref() {
                let found = value
                    .parse::<usize>()
                    .map_err(|_| "invalid LCOV branch total")?;
                let row = index.entry(path.clone()).or_default();
                row.branch_totals.get_or_insert((0, 0)).0 = found;
                row.branches.get_or_insert_default();
            }
        } else if let Some(value) = raw.strip_prefix("BRH:") {
            if let Some(path) = current.as_ref() {
                let hit = value
                    .parse::<usize>()
                    .map_err(|_| "invalid LCOV covered-branch total")?;
                index
                    .entry(path.clone())
                    .or_default()
                    .branch_totals
                    .get_or_insert((0, 0))
                    .1 = hit;
            }
        } else if let Some(value) = raw.strip_prefix("LF:") {
            if let Some(path) = current.as_ref() {
                let found = value
                    .parse::<usize>()
                    .map_err(|_| "invalid LCOV line total")?;
                let expected = expected_lines.entry(path.clone()).or_default();
                *expected = (*expected).max(found);
            }
        } else if raw == "end_of_record" {
            current = None;
        }
    }
    for (path, row) in &mut index {
        if expected_lines
            .get(path)
            .is_some_and(|expected| *expected > row.measured.len())
        {
            return Err("LCOV omits declared executable line counters".into());
        }
        if let Some(branches) = row
            .branches
            .as_ref()
            .filter(|branches| !branches.is_empty())
        {
            row.branch_totals = Some((
                row.branch_totals
                    .map_or(branches.len(), |(found, _)| found.max(branches.len())),
                branches.values().filter(|hit| **hit).count(),
            ));
        }
    }
    Ok(index)
}

fn xml_attribute(
    start: &BytesStart<'_>,
    name: &[u8],
    decoder: Decoder,
) -> Result<Option<String>, String> {
    for attribute in start.attributes().with_checks(true) {
        let attribute =
            attribute.map_err(|error| format!("invalid coverage XML attribute: {error}"))?;
        if attribute.key.as_ref() == name {
            return attribute
                .decode_and_unescape_value(decoder)
                .map(|value| Some(value.into_owned()))
                .map_err(|error| format!("invalid coverage XML value: {error}"));
        }
    }
    Ok(None)
}

fn add_xml_line(
    index: &mut CoverageIndex,
    current: Option<&str>,
    line: &BytesStart<'_>,
    decoder: Decoder,
) -> Result<(), String> {
    let number =
        xml_attribute(line, b"number", decoder)?.and_then(|value| value.parse::<usize>().ok());
    let hits = xml_attribute(line, b"hits", decoder)?
        .unwrap_or_else(|| "0".to_owned())
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .map(|value| value as i64);
    if number.is_none_or(|number| number == 0) || hits.is_none() {
        return Err("invalid coverage XML line counter".into());
    }
    if let (Some(number), Some(hits)) = (number, hits) {
        add_line(index, current, number, hits);
        if xml_attribute(line, b"branch", decoder)?.as_deref() == Some("true") {
            let value = xml_attribute(line, b"condition-coverage", decoder)?.unwrap_or_default();
            let counts = value
                .split_once('(')
                .and_then(|(_, value)| value.trim_end_matches(')').split_once('/'));
            let (hit, found) = counts
                .and_then(|(hit, found)| {
                    Some((hit.parse::<usize>().ok()?, found.parse::<usize>().ok()?))
                })
                .ok_or_else(|| "branch coverage requires condition-coverage counts".to_owned())?;
            if hit > found {
                return Err("covered branches exceed total branches".to_owned());
            }
            if let Some(path) = current {
                let row = index.entry(path.to_owned()).or_default();
                let totals = row.branch_totals.get_or_insert((0, 0));
                totals.0 += found;
                totals.1 += hit;
                let branches = row.branches.get_or_insert_default();
                for branch in hit..found {
                    branches.insert(format!("line:{number}:missing:{branch}"), false);
                }
            }
        }
    }
    Ok(())
}

fn parse_xml(repo: &Path, data: &[u8]) -> Result<CoverageIndex, String> {
    let mut reader = Reader::from_reader(data);
    reader.config_mut().trim_text(false);
    let mut index = CoverageIndex::new();
    let mut current_class = None;
    let mut class_depth = None;
    let mut depth = 0usize;
    let mut branch_capable = false;
    loop {
        match reader
            .read_event()
            .map_err(|error| format!("coverage XML is invalid: {error}"))?
        {
            Event::Start(start) => {
                depth += 1;
                if start.name().as_ref() == b"coverage" {
                    branch_capable =
                        xml_attribute(&start, b"branch-rate", reader.decoder())?.is_some();
                }
                if start.name().as_ref() == b"class" {
                    current_class = xml_attribute(&start, b"filename", reader.decoder())?
                        .and_then(|path| normalize_coverage_path(repo, &path));
                    class_depth = Some(depth);
                    if branch_capable && let Some(path) = &current_class {
                        let row = index.entry(path.clone()).or_default();
                        row.branches.get_or_insert_default();
                        row.branch_totals.get_or_insert((0, 0));
                    }
                } else if start.name().as_ref() == b"line" {
                    add_xml_line(
                        &mut index,
                        current_class.as_deref(),
                        &start,
                        reader.decoder(),
                    )?;
                }
            }
            Event::Empty(start) => {
                if start.name().as_ref() == b"line" {
                    add_xml_line(
                        &mut index,
                        current_class.as_deref(),
                        &start,
                        reader.decoder(),
                    )?;
                }
            }
            Event::End(_) => {
                if class_depth == Some(depth) {
                    current_class = None;
                    class_depth = None;
                }
                depth = depth.saturating_sub(1);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(index)
}

fn json_lines(value: Option<&Value>) -> Result<BTreeSet<usize>, String> {
    value
        .and_then(Value::as_array)
        .ok_or("coverage line arrays are required")?
        .iter()
        .map(|line| {
            line.as_u64()
                .and_then(|line| usize::try_from(line).ok())
                .filter(|line| *line > 0)
                .ok_or_else(|| "invalid coverage line number".to_owned())
        })
        .collect()
}

fn parse_json(
    repo: &Path,
    payload: &serde_json::Map<String, Value>,
) -> Result<(String, CoverageIndex), String> {
    let mut index = CoverageIndex::new();
    if let Some(files) = payload.get("files").and_then(Value::as_object) {
        let branch_capable = payload
            .get("meta")
            .and_then(|meta| meta.get("branch_coverage"))
            .and_then(Value::as_bool)
            == Some(true);
        for (raw_path, value) in files {
            let Some(row) = value.as_object() else {
                continue;
            };
            let rel_path = normalize_coverage_path(repo, raw_path);
            let executed = json_lines(row.get("executed_lines"))?;
            let missing = json_lines(row.get("missing_lines"))?;
            for line in executed.union(&missing) {
                add_line(
                    &mut index,
                    rel_path.as_deref(),
                    *line,
                    i64::from(executed.contains(line)),
                );
            }
            if let Some(path) = rel_path
                && (branch_capable
                    || row.contains_key("executed_branches")
                    || row.contains_key("missing_branches"))
            {
                let branches = index
                    .entry(path)
                    .or_default()
                    .branches
                    .get_or_insert_default();
                for (field, hit) in [("executed_branches", true), ("missing_branches", false)] {
                    for branch in row
                        .get(field)
                        .and_then(Value::as_array)
                        .ok_or("branch measurement arrays are required")?
                    {
                        if let Some(pair) = branch.as_array().filter(|pair| pair.len() == 2)
                            && let (Some(from), Some(to)) = (pair[0].as_i64(), pair[1].as_i64())
                        {
                            *branches.entry(format!("{from}:{to}")).or_default() |= hit;
                        } else {
                            return Err("invalid coverage branch pair".into());
                        }
                    }
                }
            }
        }
        return Ok(("coverage.py-json".to_owned(), index));
    }
    for (raw_path, value) in payload {
        let Some(row) = value.as_object() else {
            continue;
        };
        let (Some(statement_map), Some(hits)) = (
            row.get("statementMap").and_then(Value::as_object),
            row.get("s").and_then(Value::as_object),
        ) else {
            continue;
        };
        let rel_path = normalize_coverage_path(repo, raw_path);
        for (statement_id, location) in statement_map {
            let line = location
                .get("start")
                .and_then(|value| value.get("line"))
                .and_then(Value::as_u64)
                .and_then(|line| usize::try_from(line).ok())
                .filter(|line| *line > 0)
                .ok_or("invalid Istanbul statement location")?;
            let hits = hits
                .get(statement_id)
                .and_then(Value::as_i64)
                .filter(|hits| *hits >= 0)
                .ok_or("invalid Istanbul statement counter")?;
            add_line(&mut index, rel_path.as_deref(), line, hits);
        }
        if let Some(path) = rel_path
            && let (Some(branch_map), Some(branch_hits)) = (
                row.get("branchMap").and_then(Value::as_object),
                row.get("b").and_then(Value::as_object),
            )
        {
            let branches = index
                .entry(path)
                .or_default()
                .branches
                .get_or_insert_default();
            for (id, branch) in branch_map {
                let locations = branch
                    .get("locations")
                    .and_then(Value::as_array)
                    .filter(|locations| !locations.is_empty())
                    .ok_or("Istanbul branch locations are required")?
                    .len();
                let hits = branch_hits
                    .get(id)
                    .and_then(Value::as_array)
                    .filter(|hits| hits.len() == locations)
                    .ok_or("Istanbul branch counters are incomplete")?;
                for (position, counter) in hits.iter().enumerate() {
                    let hit = counter.as_u64().ok_or("invalid Istanbul branch counter")? > 0;
                    branches.insert(format!("{id}:{position}"), hit);
                }
            }
        }
    }
    Ok(("istanbul-json".to_owned(), index))
}

fn freeze_index(index: CoverageIndex) -> BTreeMap<String, CoverageLines> {
    index
        .into_iter()
        .map(|(path, lines)| {
            (
                path,
                CoverageLines {
                    measured_lines: lines.measured.into_iter().collect(),
                    covered_lines: lines.covered.into_iter().collect(),
                    branches: lines.branches.map(|branches| {
                        let (found, hit) = lines.branch_totals.unwrap_or_else(|| {
                            (
                                branches.len(),
                                branches.values().filter(|hit| **hit).count(),
                            )
                        });
                        CoverageBranches {
                            found,
                            hit,
                            missing: branches
                                .into_iter()
                                .filter_map(|(id, hit)| (!hit).then_some(id))
                                .collect(),
                        }
                    }),
                },
            )
        })
        .collect()
}

pub fn ingest_coverage_reports(
    repo: &Path,
    raw_paths: &[PathBuf],
) -> Result<Vec<CoverageEvidence>, String> {
    let mut records = Vec::new();
    for raw_path in raw_paths {
        let candidate = if raw_path.is_absolute() {
            raw_path.clone()
        } else {
            repo.join(raw_path)
        };
        let data = read_bytes_nofollow(&candidate, None)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("coverage report not found: {}", candidate.display()))?;
        let path = candidate
            .canonicalize()
            .map_err(|error| format!("cannot resolve coverage report: {error}"))?;
        let text = std::str::from_utf8(&data)
            .map_err(|error| format!("coverage report is not UTF-8: {error}"))?;
        let suffix = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_lowercase();
        let trimmed = text.trim_start();
        let (format, index) = if matches!(suffix.as_str(), "info" | "lcov")
            || trimmed.starts_with("TN:")
            || trimmed.starts_with("SF:")
            || text.contains("\nSF:")
        {
            ("lcov".to_owned(), parse_lcov(repo, text)?)
        } else if suffix == "xml"
            || trimmed.starts_with("<?xml")
            || trimmed.starts_with("<coverage")
        {
            ("cobertura-xml".to_owned(), parse_xml(repo, &data)?)
        } else {
            let payload: Value = serde_json::from_slice(&data)
                .map_err(|error| format!("coverage JSON is invalid: {error}"))?;
            let payload = payload
                .as_object()
                .ok_or_else(|| format!("coverage JSON must be an object: {}", path.display()))?;
            parse_json(repo, payload)?
        };
        let path_text = path.to_string_lossy().into_owned();
        records.push(CoverageEvidence {
            evidence_id: format!("coverage-{}", &sha256_hex(path_text.as_bytes())[..12]),
            path: path_text,
            sha256: sha256_hex(&data),
            format,
            files: freeze_index(index),
        });
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_measurements_cannot_shrink_the_coverage_denominator() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.rs"), "one\ntwo\nthree\n").unwrap();
        for (name, text) in [
            (
                "bad.info",
                "SF:app.rs\nDA:1,1\nDA:2,invalid\nBRF:0\nBRH:0\n",
            ),
            (
                "bad.json",
                r#"{"meta":{"branch_coverage":true},"files":{"app.rs":{"executed_lines":[1],"missing_lines":[]}}}"#,
            ),
            (
                "bad-lines.json",
                r#"{"files":{"app.rs":{"executed_lines":[1],"missing_lines":"unreadable"}}}"#,
            ),
            (
                "bad.xml",
                r#"<coverage><class filename="app.rs"><line number="1" hits="1"/><line number="2" hits="NaN"/></class></coverage>"#,
            ),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, text).unwrap();
            assert!(
                ingest_coverage_reports(dir.path(), &[path]).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn branch_formats_distinguish_partial_missing_and_measured_zero() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.rs"), "one\ntwo\nthree\n").unwrap();
        let cases = [
            (
                "partial.xml",
                r#"<coverage branch-rate="0.5"><class filename="app.rs"><line number="1" hits="1" branch="true" condition-coverage="50% (1/2)"/></class></coverage>"#,
                1,
                2,
            ),
            (
                "partial.json",
                r#"{"meta":{"branch_coverage":true},"files":{"app.rs":{"executed_lines":[1],"missing_lines":[],"executed_branches":[[1,2]],"missing_branches":[[1,3]]}}}"#,
                1,
                2,
            ),
            (
                "istanbul.json",
                r#"{"app.rs":{"statementMap":{"0":{"start":{"line":1}}},"s":{"0":1},"branchMap":{"0":{"locations":[{},{}]}},"b":{"0":[1,0]}}}"#,
                1,
                2,
            ),
            (
                "empty.info",
                "TN:test\nSF:app.rs\nDA:1,1\nBRF:0\nBRH:0\nend_of_record\n",
                0,
                0,
            ),
            (
                "contradictory.info",
                "TN:test\nSF:app.rs\nDA:1,1\nBRDA:1,0,0,1\nBRDA:1,0,1,0\nBRF:2\nBRH:2\nend_of_record\n",
                1,
                2,
            ),
        ];
        for (name, body, hit, found) in cases {
            let path = dir.path().join(name);
            std::fs::write(&path, body).unwrap();
            let parsed = ingest_coverage_reports(dir.path(), &[path]).unwrap();
            let branches = parsed[0].files["app.rs"].branches.as_ref().unwrap();
            assert_eq!((branches.hit, branches.found), (hit, found), "{name}");
        }
    }

    #[test]
    fn repeated_control_labels_keep_distinct_target_identities() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("App.tsx"),
            "<button>Save</button><button>Save</button>",
        )
        .unwrap();
        let targets = discover_targets(
            dir.path(),
            &[AuditUnit {
                unit_id: "ui".into(),
                rel_path: "App.tsx".into(),
                start_line: None,
                end_line: None,
                start_byte: None,
                interface_relevant: true,
            }],
        );
        assert_eq!(targets.len(), 2);
        assert_ne!(targets[0].target_id, targets[1].target_id);
    }

    #[test]
    fn regression_all_controls_and_branch_outcomes_remain_visible() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("screen.tsx"), "export function Screen() { return <><button>Save</button><button>Cancel</button></>; }\n").unwrap();
        let units = [AuditUnit {
            unit_id: "screen".into(),
            rel_path: "screen.tsx".into(),
            start_line: None,
            end_line: None,
            start_byte: None,
            interface_relevant: true,
        }];
        let targets = discover_targets(dir.path(), &units);
        assert_eq!(targets.iter().filter(|t| t.kind == "ui-control").count(), 2);
        let report = dir.path().join("coverage.info");
        std::fs::write(&report, "TN:test\nSF:screen.tsx\nDA:1,1\nBRDA:1,0,0,1\nBRDA:1,0,1,0\nBRF:2\nBRH:1\nend_of_record\n").unwrap();
        let evidence = ingest_coverage_reports(dir.path(), &[report]).unwrap();
        let value = serde_json::to_value(&evidence[0]).unwrap();
        assert_eq!(value["files"]["screen.tsx"]["branches"]["found"], 2);
        assert_eq!(value["files"]["screen.tsx"]["branches"]["hit"], 1);
    }

    #[test]
    fn structural_discovery_finds_functions_controls_and_review_fallbacks() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("screen.tsx"),
            "export function Screen() {\n  return <button> Save   now </button>;\n}\n",
        )
        .unwrap();
        let units = vec![
            AuditUnit {
                unit_id: "unit-a".to_owned(),
                rel_path: "screen.tsx".to_owned(),
                start_line: None,
                end_line: None,
                start_byte: None,
                interface_relevant: true,
            },
            AuditUnit {
                unit_id: "unit-b".to_owned(),
                rel_path: "screen.tsx".to_owned(),
                start_line: Some(1),
                end_line: Some(2),
                start_byte: Some(0),
                interface_relevant: true,
            },
        ];
        let targets = discover_targets(directory.path(), &units);
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0].symbol, "Screen");
        assert_eq!(targets[0].target_id, "target-3202da2fab17d888");
        assert_eq!(targets[1].symbol, "button:Save now");
        assert_eq!(targets[1].target_id, "target-558f6d612049b79b");
        assert_eq!(targets[2].kind, "unit-review");
        assert_eq!(targets[2].target_id, "target-7e73774ece6898af");
        assert!(
            targets
                .iter()
                .all(|target| target.target_id.starts_with("target-"))
        );
    }

    #[test]
    fn ingests_lcov_cobertura_coveragepy_and_istanbul() {
        let directory = tempfile::tempdir().unwrap();
        let repo = directory.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let source = repo.join("app.rs");
        std::fs::write(&source, "one\ntwo\nthree\n").unwrap();
        let lcov = repo.join("coverage.info");
        std::fs::write(&lcov, "TN:\nSF:app.rs\nDA:1,1\nDA:2,0\nend_of_record\n").unwrap();
        let xml = repo.join("coverage.xml");
        std::fs::write(
            &xml,
            "<coverage><classes><class filename=\"app.rs\"><lines><line number=\"2\" hits=\"1\"/><line number=\"3\" hits=\"0\"/></lines></class></classes></coverage>",
        )
        .unwrap();
        let coveragepy = repo.join("coverage-python.json");
        std::fs::write(
            &coveragepy,
            r#"{"files":{"app.rs":{"executed_lines":[1],"missing_lines":[2]}}}"#,
        )
        .unwrap();
        let istanbul = repo.join("coverage-istanbul.json");
        std::fs::write(
            &istanbul,
            r#"{"app.rs":{"statementMap":{"0":{"start":{"line":3}}},"s":{"0":1}}}"#,
        )
        .unwrap();
        let records = ingest_coverage_reports(&repo, &[lcov, xml, coveragepy, istanbul]).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.format.as_str())
                .collect::<Vec<_>>(),
            ["lcov", "cobertura-xml", "coverage.py-json", "istanbul-json"]
        );
        assert_eq!(records[0].files["app.rs"].measured_lines, [1, 2]);
        assert_eq!(records[0].files["app.rs"].covered_lines, [1]);
        assert_eq!(records[1].files["app.rs"].covered_lines, [2]);
        assert_eq!(records[3].files["app.rs"].covered_lines, [3]);
    }

    #[test]
    fn coverage_paths_cannot_escape_or_follow_symlinked_reports() {
        let directory = tempfile::tempdir().unwrap();
        let repo = directory.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("app.rs"), "fn main() {}\n").unwrap();
        assert_eq!(normalize_coverage_path(&repo, "../outside.rs"), None);
        let report = repo.join("coverage.info");
        std::fs::write(&report, "TN:\nSF:app.rs\nDA:1,1\n").unwrap();
        let link = repo.join("linked.info");
        std::os::unix::fs::symlink(report, &link).unwrap();
        assert!(ingest_coverage_reports(&repo, &[link]).is_err());
    }
}
