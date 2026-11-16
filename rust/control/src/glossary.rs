use std::collections::{BTreeMap, BTreeSet};

use crate::database::{Connection, OptionalExtension};
use devcoordinator2_api::glossary::*;
use devcoordinator2_api::{ErrorCode, ProtocolError};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::database::{Database, DatabaseError};
use crate::ids;

const SHARED: &str = "shared";
const PAGE_LIMIT: usize = 25;

#[derive(Clone)]
pub struct GlossaryService {
    database: Database,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Settings {
    baseline_revision: u32,
    languages: Vec<String>,
    guidelines: Vec<Guideline>,
}

#[derive(Default)]
struct Document {
    revision: u32,
    settings: Settings,
    concepts: BTreeMap<String, Concept>,
}

struct Resolved {
    profile: Profile,
    entries: BTreeMap<String, Entry>,
}

impl GlossaryService {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    pub fn list(&self, repository: Option<&str>, params: List) -> Result<Page, ProtocolError> {
        let limit = page_limit(params.limit)?;
        let offset = params.offset.unwrap_or(0);
        let query = normalized(params.query.as_deref().unwrap_or_default());
        text("search", &query, 0, 200)?;
        let language = params.language.as_deref().map(language_tag).transpose()?;
        if params.origin.as_deref().is_some_and(|origin| {
            !["shared", "local", "inherited", "specialized"].contains(&origin)
        }) {
            return Err(invalid("Unknown glossary origin filter"));
        }
        let scope = scope(repository);
        self.database
            .read(move |connection| {
                let resolved = resolve(connection, &scope, params.revision)?;
                if let Some(expected) = params.expected_revision {
                    expect_revision(resolved.profile.revision, expected)?;
                }
                let mut entries: Vec<Entry> = resolved
                    .entries
                    .into_values()
                    .filter(|entry| {
                        let concept = &entry.concept;
                        params
                            .status
                            .as_ref()
                            .is_none_or(|status| status == &concept.status)
                            && params
                                .origin
                                .as_ref()
                                .is_none_or(|origin| origin == &entry.origin)
                            && language
                                .as_ref()
                                .is_none_or(|language| concept.languages.contains_key(language))
                            && (query.is_empty() || searchable(concept).contains(&query))
                    })
                    .collect();
                entries.sort_by(|left, right| {
                    normalized(&left.concept.name)
                        .cmp(&normalized(&right.concept.name))
                        .then(left.concept_id.cmp(&right.concept_id))
                });
                let total = entries.len();
                let mut encoded_size = serialize(&resolved.profile)?.len() + 1024;
                let mut page = Vec::new();
                for entry in entries.into_iter().skip(offset).take(limit) {
                    let size = serialize(&entry)?.len() + 1;
                    if encoded_size + size > 200 * 1024 {
                        break;
                    }
                    encoded_size += size;
                    page.push(entry);
                }
                let entries = page;
                let next = offset.saturating_add(entries.len());
                Ok(Page {
                    profile: resolved.profile,
                    entries,
                    total,
                    next_offset: (next < total).then_some(next),
                })
            })
            .map_err(error)
    }

    pub fn get(&self, repository: Option<&str>, params: Get) -> Result<Detail, ProtocolError> {
        let scope = scope(repository);
        self.database
            .read(move |connection| {
                let mut resolved = resolve(connection, &scope, params.revision)?;
                let entry = resolved
                    .entries
                    .remove(&params.concept_id)
                    .ok_or_else(not_found)?;
                Ok(Detail {
                    profile: resolved.profile,
                    entry,
                })
            })
            .map_err(error)
    }

    pub fn save(
        &self,
        repository: Option<&str>,
        params: Save,
        actor: &str,
        now: &str,
    ) -> Result<Mutation, ProtocolError> {
        let scope = scope(repository);
        let actor = actor.to_owned();
        let now = now.to_owned();
        let concept_id = params.concept_id.clone().unwrap_or(
            ids::glossary_id().map_err(|_| invalid("Cannot allocate concept identity"))?,
        );
        let mut concept = params.concept;
        validate_concept(&mut concept)?;
        self.database
            .transaction(move |connection| {
                require_scope(connection, &scope, true)?;
                let mut document = load(connection, &scope, None)?;
                expect_revision(document.revision, params.expected_revision)?;
                let parent = load(
                    connection,
                    SHARED,
                    Some(document.settings.baseline_revision),
                )?;
                let inherited = (scope != SHARED)
                    .then(|| parent.concepts.get(&concept_id))
                    .flatten();
                if params.concept_id.is_some()
                    && !document.concepts.contains_key(&concept_id)
                    && inherited.is_none()
                {
                    return Err(not_found().into());
                }
                if let Some(shared) = inherited {
                    if shared.rule == Rule::Mandatory {
                        return Err(invalid(
                            "This shared concept is mandatory; edit its shared source instead",
                        )
                        .into());
                    }
                    text(
                        "specialization reason",
                        &concept.specialization_reason,
                        3,
                        1000,
                    )?;
                } else if !concept.specialization_reason.is_empty() {
                    return Err(invalid(
                        "Only inherited concepts can have a specialization reason",
                    )
                    .into());
                }
                if let Some(previous) = document.concepts.get(&concept_id).or(inherited)
                    && (previous.definition != concept.definition
                        || previous.context != concept.context)
                {
                    for (language, term) in &mut concept.languages {
                        if previous.languages.get(language).is_some_and(|old| {
                            old.preferred == term.preferred
                                && old.allowed == term.allowed
                                && old.deprecated == term.deprecated
                                && old.usage == term.usage
                                && old.examples == term.examples
                        }) {
                            term.reviewed = false;
                        }
                    }
                }
                let summary = format!(
                    "{} {}",
                    if document.concepts.contains_key(&concept_id) {
                        "Updated"
                    } else {
                        "Added"
                    },
                    concept.name
                );
                document
                    .concepts
                    .insert(concept_id.clone(), concept.clone());
                validate_document(&document, (scope != SHARED).then_some(&parent))?;
                let body = serialize(&concept)?;
                let revision = append(
                    connection,
                    &scope,
                    document.revision,
                    Change {
                        kind: "concept",
                        identity: &concept_id,
                        body: Some(&body),
                        summary: &summary,
                        actor: &actor,
                        now: &now,
                    },
                )?;
                Ok(Mutation {
                    revision,
                    concept_id: Some(concept_id),
                })
            })
            .map_err(error)
    }

    pub fn configure(
        &self,
        repository: Option<&str>,
        params: Configure,
        actor: &str,
        now: &str,
    ) -> Result<Mutation, ProtocolError> {
        let scope = scope(repository);
        let actor = actor.to_owned();
        let now = now.to_owned();
        let languages = validate_languages(params.languages)?;
        let mut guidelines = params.guidelines;
        if guidelines.len() > 50 {
            return Err(invalid(
                "At most 50 glossary guidelines are supported per scope",
            ));
        }
        let mut keys = BTreeSet::new();
        for guideline in &mut guidelines {
            text("guideline name", &guideline.key, 1, 100)?;
            text("guideline", &guideline.text, 3, 2000)?;
            text(
                "specialization reason",
                &guideline.specialization_reason,
                0,
                1000,
            )?;
            guideline.key = guideline.key.trim().to_owned();
            guideline.text = guideline.text.trim().to_owned();
            if !keys.insert(normalized(&guideline.key)) {
                return Err(invalid("Guideline names must be unique"));
            }
        }
        self.database
            .transaction(move |connection| {
                require_scope(connection, &scope, true)?;
                let mut document = load(connection, &scope, None)?;
                expect_revision(document.revision, params.expected_revision)?;
                if scope == SHARED
                    && params
                        .baseline_revision
                        .is_some_and(|revision| revision != 0)
                {
                    return Err(invalid("The shared glossary has no parent revision").into());
                }
                let baseline = params
                    .baseline_revision
                    .unwrap_or(document.settings.baseline_revision);
                let parent = load(connection, SHARED, Some(baseline))?;
                document.settings = Settings {
                    baseline_revision: baseline,
                    languages,
                    guidelines,
                };
                validate_document(&document, (scope != SHARED).then_some(&parent))?;
                let body = serialize(&document.settings)?;
                if body.len() > 32 * 1024 {
                    return Err(invalid("Keep glossary settings within 32 KiB").into());
                }
                let revision = append(
                    connection,
                    &scope,
                    document.revision,
                    Change {
                        kind: "settings",
                        identity: "",
                        body: Some(&body),
                        summary: "Updated glossary guidance and shared revision",
                        actor: &actor,
                        now: &now,
                    },
                )?;
                Ok(Mutation {
                    revision,
                    concept_id: None,
                })
            })
            .map_err(error)
    }

    pub fn inherit(
        &self,
        repository: Option<&str>,
        params: Inherit,
        actor: &str,
        now: &str,
    ) -> Result<Mutation, ProtocolError> {
        let scope = scope(repository);
        if scope == SHARED {
            return Err(invalid(
                "Only project specializations can return to shared terminology",
            ));
        }
        let actor = actor.to_owned();
        let now = now.to_owned();
        self.database
            .transaction(move |connection| {
                require_scope(connection, &scope, true)?;
                let mut document = load(connection, &scope, None)?;
                expect_revision(document.revision, params.expected_revision)?;
                let parent = load(
                    connection,
                    SHARED,
                    Some(document.settings.baseline_revision),
                )?;
                if !parent.concepts.contains_key(&params.concept_id) {
                    return Err(invalid("This is not an inherited concept").into());
                }
                let previous = document
                    .concepts
                    .remove(&params.concept_id)
                    .ok_or_else(not_found)?;
                validate_document(&document, Some(&parent))?;
                let revision = append(
                    connection,
                    &scope,
                    document.revision,
                    Change {
                        kind: "concept",
                        identity: &params.concept_id,
                        body: None,
                        summary: &format!("Restored shared {}", previous.name),
                        actor: &actor,
                        now: &now,
                    },
                )?;
                Ok(Mutation {
                    revision,
                    concept_id: Some(params.concept_id),
                })
            })
            .map_err(error)
    }

    pub fn history(
        &self,
        repository: Option<&str>,
        params: History,
    ) -> Result<HistoryPage, ProtocolError> {
        let scope = scope(repository);
        let limit = page_limit(params.limit)?;
        self.database.read(move |connection| {
            require_scope(connection, &scope, false)?;
            let mut statement = connection.prepare("SELECT revision,kind,subject_id,summary,actor,created_at FROM glossary_revisions WHERE scope=$1 AND ($2::text IS NULL OR subject_id=$2) AND ($3::bigint IS NULL OR revision<$3) ORDER BY revision DESC LIMIT $4")?;
            let mut revisions = statement.query_map(crate::database::params![scope, params.concept_id, params.before_revision, (limit + 1) as i64], |row| {
                let subject: String = row.get(2)?;
                Ok(Revision { revision: row.get(0)?, kind: row.get(1)?, concept_id: (!subject.is_empty()).then_some(subject), summary: row.get(3)?, actor: row.get(4)?, created_at: row.get(5)? })
            })?.collect::<Result<Vec<_>, _>>()?;
            let more = revisions.len() > limit;
            revisions.truncate(limit);
            let next = more.then(|| revisions.last().map(|revision| revision.revision)).flatten();
            Ok(HistoryPage { revisions, next_before_revision: next })
        }).map_err(error)
    }

    pub fn check(
        &self,
        repository: Option<&str>,
        params: Check,
    ) -> Result<CheckResult, ProtocolError> {
        if params.usages.is_empty() || params.usages.len() > 100 {
            return Err(invalid("Provide between 1 and 100 identified term usages"));
        }
        let scope = scope(repository);
        self.database
            .read(move |connection| {
                let resolved = resolve(connection, &scope, None)?;
                if let Some(expected) = params.expected_revision {
                    expect_revision(resolved.profile.revision, expected)?;
                }
                let mut findings = Vec::new();
                for (index, usage) in params.usages.iter().enumerate() {
                    text("term", &usage.term, 1, 160)?;
                    let language = language_tag(&usage.language)?;
                    let mut preferred = None;
                    let code = match resolved.entries.get(&usage.concept_id) {
                        None => Some("unknown_concept"),
                        Some(entry) if entry.concept.status != Status::Approved => {
                            Some("concept_not_approved")
                        }
                        Some(entry) => match entry.concept.languages.get(&language) {
                            None => Some("missing_language"),
                            Some(term) => {
                                preferred = Some(term.preferred.clone());
                                let actual = canonical(&usage.term);
                                if !term.reviewed {
                                    Some("language_needs_review")
                                } else if term
                                    .deprecated
                                    .iter()
                                    .any(|value| canonical(value) == actual)
                                {
                                    Some("deprecated_term")
                                } else if canonical(&term.preferred) == actual
                                    || term.allowed.iter().any(|value| canonical(value) == actual)
                                {
                                    None
                                } else {
                                    Some("unapproved_term")
                                }
                            }
                        },
                    };
                    if let Some(code) = code {
                        findings.push(CheckFinding {
                            index,
                            code: code.to_owned(),
                            preferred,
                        });
                    }
                }
                let valid = findings.is_empty() && !resolved.profile.adoption_needed;
                Ok(CheckResult {
                    profile: resolved.profile,
                    valid,
                    findings,
                })
            })
            .map_err(error)
    }

    pub fn impact(&self, params: ImpactRequest) -> Result<Impact, ProtocolError> {
        let limit = page_limit(params.limit)?;
        let offset = params.offset.unwrap_or(0);
        self.database.read(move |connection| {
            let shared_revision = latest(connection, SHARED)?;
            let total = connection.query_row("SELECT COUNT(*) FROM repositories WHERE archived_at IS NULL", [], |row| row.get::<_, u32>(0))? as usize;
            let mut statement = connection.prepare("SELECT repository_id,display_name FROM repositories WHERE archived_at IS NULL ORDER BY display_name,repository_id LIMIT $1 OFFSET $2")?;
            let rows = statement.query_map(crate::database::params![limit as i64, i64::try_from(offset).map_err(|_| invalid("Offset is too large"))?], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            let mut projects = Vec::new();
            for (repository_id, display_name) in rows {
                let document = load(connection, &repository_id, None)?;
                projects.push(ProjectImpact { repository_id, display_name, baseline_revision: document.settings.baseline_revision, configured: document.revision > 0 });
            }
            let next = offset.saturating_add(projects.len());
            Ok(Impact { shared_revision, projects, total, next_offset: (next < total).then_some(next) })
        }).map_err(error)
    }
}

fn scope(repository: Option<&str>) -> String {
    repository.unwrap_or(SHARED).to_owned()
}

fn require_scope(
    connection: &Connection,
    scope: &str,
    writing: bool,
) -> Result<String, DatabaseError> {
    if writing {
        connection.query_row(
            "SELECT pg_advisory_xact_lock(hashtextextended('glossary:'||$1,0))",
            [scope],
            |_| Ok(()),
        )?;
    }
    if scope == SHARED {
        return Ok("Shared glossary".to_owned());
    }
    let row: Option<(String, Option<String>)> = connection
        .query_row(
            "SELECT display_name,archived_at FROM repositories WHERE repository_id=$1",
            [scope],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match row {
        Some((_, Some(_))) if writing => Err(ProtocolError::new(
            ErrorCode::RepositoryArchived,
            "Archived project glossaries are read-only",
        )
        .into()),
        Some((name, _)) => Ok(name),
        None => Err(ProtocolError::new(ErrorCode::RepositoryNotFound, "Project not found").into()),
    }
}

fn latest(connection: &Connection, scope: &str) -> Result<u32, DatabaseError> {
    Ok(connection
        .query_row(
            "SELECT revision FROM glossary_profiles WHERE scope=$1",
            [scope],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

fn load(
    connection: &Connection,
    scope: &str,
    revision: Option<u32>,
) -> Result<Document, DatabaseError> {
    let current = latest(connection, scope)?;
    let revision = revision.unwrap_or(current);
    if revision > current {
        return Err(not_found().into());
    }
    let settings: Option<String> = connection.query_row("SELECT body FROM glossary_revisions WHERE scope=$1 AND kind='settings' AND revision<=$2 ORDER BY revision DESC LIMIT 1", crate::database::params![scope, revision], |row| row.get(0)).optional()?;
    let settings = settings
        .map(|body| deserialize(&body))
        .transpose()?
        .unwrap_or_default();
    let mut statement = connection.prepare("SELECT events.subject_id,events.body FROM glossary_revisions events JOIN (SELECT subject_id,MAX(revision) revision FROM glossary_revisions WHERE scope=$1 AND kind='concept' AND revision<=$2 GROUP BY subject_id) newest ON events.scope=$1 AND events.revision=newest.revision WHERE events.body IS NOT NULL")?;
    let rows = statement
        .query_map(crate::database::params![scope, revision], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut concepts = BTreeMap::new();
    for (identity, body) in rows {
        concepts.insert(identity, deserialize(&body)?);
    }
    Ok(Document {
        revision,
        settings,
        concepts,
    })
}

fn resolve(
    connection: &Connection,
    scope: &str,
    revision: Option<u32>,
) -> Result<Resolved, DatabaseError> {
    let display_name = require_scope(connection, scope, false)?;
    let document = load(connection, scope, revision)?;
    let shared_revision = latest(connection, SHARED)?;
    let is_shared = scope == SHARED;
    let parent = if is_shared {
        Document::default()
    } else {
        load(
            connection,
            SHARED,
            Some(document.settings.baseline_revision),
        )?
    };
    validate_document(&document, (!is_shared).then_some(&parent))?;
    let mut entries = BTreeMap::new();
    for (identity, concept) in &parent.concepts {
        entries.insert(
            identity.clone(),
            Entry {
                concept_id: identity.clone(),
                concept: concept.clone(),
                origin: "inherited".to_owned(),
                inherited_from: Some(parent.revision),
                shared_concept: None,
            },
        );
    }
    for (identity, concept) in &document.concepts {
        let shared = parent.concepts.get(identity);
        entries.insert(
            identity.clone(),
            Entry {
                concept_id: identity.clone(),
                concept: concept.clone(),
                origin: if is_shared {
                    "shared"
                } else if shared.is_some() {
                    "specialized"
                } else {
                    "local"
                }
                .to_owned(),
                inherited_from: shared.map(|_| parent.revision),
                shared_concept: shared.cloned(),
            },
        );
    }
    let mut guidelines = BTreeMap::new();
    for guideline in &parent.settings.guidelines {
        guidelines.insert(
            normalized(&guideline.key),
            EffectiveGuideline {
                guideline: guideline.clone(),
                origin: "inherited".to_owned(),
            },
        );
    }
    for guideline in &document.settings.guidelines {
        guidelines.insert(
            normalized(&guideline.key),
            EffectiveGuideline {
                guideline: guideline.clone(),
                origin: if is_shared { "shared" } else { "local" }.to_owned(),
            },
        );
    }
    let available_languages = entries
        .values()
        .flat_map(|entry| entry.concept.languages.keys().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(Resolved {
        profile: Profile {
            repository_id: (!is_shared).then(|| scope.to_owned()),
            display_name,
            revision: document.revision,
            baseline_revision: document.settings.baseline_revision,
            latest_shared_revision: shared_revision,
            adoption_needed: !is_shared && document.settings.baseline_revision != shared_revision,
            languages: document.settings.languages.clone(),
            available_languages,
            local_guidelines: document.settings.guidelines.clone(),
            guidelines: guidelines.into_values().collect(),
        },
        entries,
    })
}

fn validate_document(document: &Document, parent: Option<&Document>) -> Result<(), ProtocolError> {
    if document.concepts.len() > 2000 {
        return Err(invalid(
            "At most 2000 local concepts are supported in one glossary",
        ));
    }
    let languages: BTreeSet<_> = document
        .concepts
        .values()
        .chain(
            parent
                .into_iter()
                .flat_map(|parent| parent.concepts.values()),
        )
        .flat_map(|concept| concept.languages.keys())
        .collect();
    if languages.len() > 64 {
        return Err(invalid(
            "At most 64 language tags are supported in an effective glossary",
        ));
    }
    for (identity, concept) in &document.concepts {
        if let Some(inherited) = parent.and_then(|parent| parent.concepts.get(identity)) {
            if inherited.rule == Rule::Mandatory {
                return Err(invalid(&format!(
                    "Mandatory shared concept '{}' conflicts with a project specialization",
                    inherited.name
                )));
            }
            text(
                "specialization reason",
                &concept.specialization_reason,
                3,
                1000,
            )?;
        }
        for related in &concept.related {
            if identity == related
                || (!document.concepts.contains_key(related)
                    && parent.is_none_or(|parent| !parent.concepts.contains_key(related)))
            {
                return Err(invalid(
                    "Related concepts must exist in the effective glossary and cannot refer to themselves",
                ));
            }
        }
    }
    for guideline in &document.settings.guidelines {
        if let Some(inherited) = parent.and_then(|parent| {
            parent
                .settings
                .guidelines
                .iter()
                .find(|candidate| normalized(&candidate.key) == normalized(&guideline.key))
        }) {
            if inherited.rule == Rule::Mandatory {
                return Err(invalid(&format!(
                    "Mandatory shared guideline '{}' cannot be overridden",
                    inherited.key
                )));
            }
            text(
                "guideline specialization reason",
                &guideline.specialization_reason,
                3,
                1000,
            )?;
        }
    }
    Ok(())
}

fn validate_concept(concept: &mut Concept) -> Result<(), ProtocolError> {
    text("concept name", &concept.name, 1, 120)?;
    text("definition", &concept.definition, 3, 4000)?;
    text("context", &concept.context, 0, 2000)?;
    text(
        "specialization reason",
        &concept.specialization_reason,
        0,
        1000,
    )?;
    concept.name = concept.name.trim().to_owned();
    concept.definition = concept.definition.trim().to_owned();
    if concept.languages.len() > 24 || concept.related.len() > 30 {
        return Err(invalid(
            "A concept supports up to 24 languages and 30 related concepts",
        ));
    }
    let mut languages = BTreeMap::new();
    for (language, mut term) in std::mem::take(&mut concept.languages) {
        let language = language_tag(&language)?;
        text("preferred term", &term.preferred, 1, 160)?;
        text("usage", &term.usage, 0, 2000)?;
        term.preferred = term.preferred.trim().nfc().collect();
        if term.allowed.len() > 12 || term.deprecated.len() > 12 || term.examples.len() > 8 {
            return Err(invalid(
                "Use at most 12 alternative terms and 8 examples per language",
            ));
        }
        let mut terms = BTreeSet::from([canonical(&term.preferred)]);
        for value in term.allowed.iter_mut().chain(term.deprecated.iter_mut()) {
            text("alternative term", value, 1, 160)?;
            *value = value.trim().nfc().collect();
            if !terms.insert(canonical(value)) {
                return Err(invalid(
                    "Preferred, allowed and deprecated forms must not overlap",
                ));
            }
        }
        for example in &term.examples {
            text("example", example, 1, 500)?;
        }
        if languages.insert(language, term).is_some() {
            return Err(invalid(
                "Language tags must be unique regardless of letter case",
            ));
        }
    }
    concept.languages = languages;
    if concept.status == Status::Approved && concept.languages.is_empty() {
        return Err(invalid(
            "An approved concept needs at least one language term",
        ));
    }
    if serialize(concept).map_err(error)?.len() > 24 * 1024 {
        return Err(invalid(
            "A concept must fit within 24 KiB; keep examples and guidance concise",
        ));
    }
    Ok(())
}

fn validate_languages(languages: Vec<String>) -> Result<Vec<String>, ProtocolError> {
    if languages.len() > 24 {
        return Err(invalid("A glossary supports up to 24 declared languages"));
    }
    let mut result = Vec::new();
    for language in languages {
        let language = language_tag(&language)?;
        if result.contains(&language) {
            return Err(invalid("Declared language tags must be unique"));
        }
        result.push(language);
    }
    Ok(result)
}

fn language_tag(value: &str) -> Result<String, ProtocolError> {
    let parts: Vec<_> = value.split('-').collect();
    if value.len() > 63
        || parts.is_empty()
        || !(2..=8).contains(&parts[0].len())
        || !parts[0].bytes().all(|byte| byte.is_ascii_alphabetic())
        || parts.iter().skip(1).any(|part| {
            part.is_empty()
                || part.len() > 8
                || !part.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
    {
        return Err(invalid(
            "Use a language tag such as en, ru, pt-BR or zh-Hant, without spaces or underscores",
        ));
    }
    Ok(value.to_ascii_lowercase())
}

struct Change<'a> {
    kind: &'a str,
    identity: &'a str,
    body: Option<&'a str>,
    summary: &'a str,
    actor: &'a str,
    now: &'a str,
}

fn append(
    connection: &Connection,
    scope: &str,
    previous: u32,
    change: Change<'_>,
) -> Result<u32, DatabaseError> {
    let Change {
        kind,
        identity,
        body,
        summary,
        actor,
        now,
    } = change;
    let revision = previous
        .checked_add(1)
        .ok_or_else(|| invalid("Glossary revision limit reached"))?;
    connection.execute("INSERT INTO glossary_profiles(scope,revision) VALUES($1,$2) ON CONFLICT(scope) DO UPDATE SET revision=excluded.revision", crate::database::params![scope, revision])?;
    connection.execute("INSERT INTO glossary_revisions(scope,revision,kind,subject_id,body,summary,actor,created_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8)", crate::database::params![scope, revision, kind, identity, body, summary, actor, now])?;
    Ok(revision)
}

fn canonical(value: &str) -> String {
    value.trim().nfc().collect()
}
fn normalized(value: &str) -> String {
    canonical(value).to_lowercase()
}
fn searchable(concept: &Concept) -> String {
    let mut value = format!(
        "{} {} {}",
        concept.name, concept.definition, concept.context
    );
    for term in concept.languages.values() {
        value.push_str(&format!(
            " {} {} {} {}",
            term.preferred,
            term.allowed.join(" "),
            term.deprecated.join(" "),
            term.usage
        ));
    }
    normalized(&value)
}
fn text(name: &str, value: &str, minimum: usize, maximum: usize) -> Result<(), ProtocolError> {
    let length = value.trim().chars().count();
    if length < minimum
        || length > maximum
        || value
            .chars()
            .any(|character| character.is_control() && character != '\n' && character != '\t')
    {
        Err(invalid(&format!(
            "{name} must contain {minimum}–{maximum} characters without control characters"
        )))
    } else {
        Ok(())
    }
}
fn page_limit(limit: Option<usize>) -> Result<usize, ProtocolError> {
    let limit = limit.unwrap_or(10);
    if !(1..=PAGE_LIMIT).contains(&limit) {
        Err(invalid("Page size must be between 1 and 25"))
    } else {
        Ok(limit)
    }
}
fn expect_revision(actual: u32, expected: u32) -> Result<(), ProtocolError> {
    if actual == expected {
        Ok(())
    } else {
        Err(ProtocolError::new(
            ErrorCode::GlossaryConflict,
            format!(
                "This glossary changed from revision {expected} to {actual}. Reload it before saving; your draft has not been applied."
            ),
        ))
    }
}
fn invalid(message: &str) -> ProtocolError {
    ProtocolError::new(ErrorCode::ParamsInvalid, message)
}
fn not_found() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::GlossaryNotFound,
        "Glossary concept or revision not found",
    )
}
fn serialize<T: Serialize>(value: &T) -> Result<String, DatabaseError> {
    serde_json::to_string(value).map_err(|_| {
        ProtocolError::new(ErrorCode::InternalError, "Cannot serialize glossary record").into()
    })
}
fn deserialize<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, DatabaseError> {
    serde_json::from_str(value).map_err(|_| {
        ProtocolError::new(
            ErrorCode::InternalError,
            "Stored glossary record is invalid",
        )
        .into()
    })
}
fn error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        _ => ProtocolError::new(ErrorCode::InternalError, "Glossary storage is unavailable"),
    }
}

#[cfg(test)]
#[path = "glossary_tests.rs"]
mod tests;
