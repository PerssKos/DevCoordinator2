use super::*;
use tempfile::TempDir;

const PROJECT: &str = "r1111111111111111";

fn world() -> (TempDir, Database, GlossaryService) {
    let temporary = tempfile::tempdir().unwrap();
    let database = Database::open(temporary.path().join("glossary.sqlite3")).unwrap();
    database.transaction(|connection| {
        connection.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES($1,'/glossary-fixture','Vocabulary project','now',1000,'now')", [PROJECT])?;
        Ok(())
    }).unwrap();
    let service = GlossaryService::new(database.clone());
    (temporary, database, service)
}

fn concept(name: &str, rule: Rule) -> Concept {
    Concept {
        name: name.to_owned(),
        definition: "One execution of a declared test.".to_owned(),
        rule,
        status: Status::Approved,
        languages: BTreeMap::from([
            (
                "en".to_owned(),
                LanguageTerm {
                    preferred: "Test run".to_owned(),
                    allowed: vec!["Test runs".to_owned()],
                    deprecated: vec!["Job".to_owned()],
                    reviewed: true,
                    ..Default::default()
                },
            ),
            (
                "ru".to_owned(),
                LanguageTerm {
                    preferred: "Запуск теста".to_owned(),
                    allowed: vec!["Запуски теста".to_owned()],
                    reviewed: true,
                    ..Default::default()
                },
            ),
        ]),
        ..Default::default()
    }
}

fn save(
    service: &GlossaryService,
    repository: Option<&str>,
    revision: u32,
    identity: Option<&str>,
    concept: Concept,
) -> Result<Mutation, ProtocolError> {
    service.save(
        repository,
        Save {
            expected_revision: revision,
            concept_id: identity.map(str::to_owned),
            concept,
            ..Default::default()
        },
        "fixture-owner",
        "2026-09-05T12:00:00Z",
    )
}

fn adopt(
    service: &GlossaryService,
    revision: u32,
    baseline: u32,
) -> Result<Mutation, ProtocolError> {
    service.configure(
        Some(PROJECT),
        Configure {
            expected_revision: revision,
            baseline_revision: Some(baseline),
            languages: vec!["en".to_owned(), "ru".to_owned()],
            ..Default::default()
        },
        "owner",
        "now",
    )
}

fn get(
    service: &GlossaryService,
    repository: Option<&str>,
    identity: &str,
    revision: Option<u32>,
) -> Detail {
    service
        .get(
            repository,
            Get {
                concept_id: identity.to_owned(),
                revision,
                ..Default::default()
            },
        )
        .unwrap()
}

#[test]
fn glossary_reads_are_empty_truthful_and_do_not_create_state() {
    let (_temporary, database, service) = world();
    assert_eq!(service.list(None, List::default()).unwrap().total, 0);
    assert_eq!(
        service
            .list(Some(PROJECT), List::default())
            .unwrap()
            .profile
            .revision,
        0
    );
    let count: i64 = database
        .call(|connection| {
            Ok(
                connection.query_row("SELECT COUNT(*) FROM glossary_profiles", [], |row| {
                    row.get(0)
                })?,
            )
        })
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        service
            .list(Some("missing"), List::default())
            .unwrap_err()
            .code,
        ErrorCode::RepositoryNotFound
    );
}

#[test]
fn glossary_revisions_persist_and_stale_writes_do_not_change_history() {
    let (temporary, database, service) = world();
    let created = save(
        &service,
        None,
        0,
        None,
        concept("Test execution", Rule::Default),
    )
    .unwrap();
    let identity = created.concept_id.unwrap();
    assert_eq!(created.revision, 1);
    assert_eq!(
        save(
            &service,
            None,
            0,
            Some(&identity),
            concept("Changed", Rule::Default)
        )
        .unwrap_err()
        .code,
        ErrorCode::GlossaryConflict
    );
    assert_eq!(
        service
            .history(None, History::default())
            .unwrap()
            .revisions
            .len(),
        1
    );
    drop(service);
    database.close().unwrap();
    let reopened = Database::open(temporary.path().join("glossary.sqlite3")).unwrap();
    assert_eq!(
        get(&GlossaryService::new(reopened), None, &identity, None)
            .entry
            .concept
            .name,
        "Test execution"
    );
}

#[test]
fn glossary_projects_pin_shared_versions_and_cannot_override_mandatory_concepts() {
    let (_temporary, _database, service) = world();
    let identity = save(
        &service,
        None,
        0,
        None,
        concept("Execution", Rule::Mandatory),
    )
    .unwrap()
    .concept_id
    .unwrap();
    assert!(
        service
            .list(Some(PROJECT), List::default())
            .unwrap()
            .profile
            .adoption_needed
    );
    assert_eq!(
        service.list(Some(PROJECT), List::default()).unwrap().total,
        0
    );
    adopt(&service, 0, 1).unwrap();
    let detail = get(&service, Some(PROJECT), &identity, None);
    assert_eq!(detail.entry.origin, "inherited");
    assert_eq!(
        save(
            &service,
            Some(PROJECT),
            1,
            Some(&identity),
            concept("Hidden override", Rule::Mandatory)
        )
        .unwrap_err()
        .code,
        ErrorCode::ParamsInvalid
    );
    let mut updated = concept("New name", Rule::Mandatory);
    updated.context = "Updated shared context".to_owned();
    save(&service, None, 1, Some(&identity), updated).unwrap();
    assert_eq!(
        get(&service, Some(PROJECT), &identity, None)
            .entry
            .concept
            .name,
        "Execution"
    );
    assert!(
        service
            .list(Some(PROJECT), List::default())
            .unwrap()
            .profile
            .adoption_needed
    );
    adopt(&service, 1, 2).unwrap();
    assert_eq!(
        get(&service, Some(PROJECT), &identity, None)
            .entry
            .concept
            .name,
        "New name"
    );
    assert_eq!(
        get(&service, Some(PROJECT), &identity, Some(1))
            .entry
            .concept
            .name,
        "Execution"
    );
}

#[test]
fn glossary_specialization_requires_reason_and_conflicting_adoption_is_atomic() {
    let (_temporary, _database, service) = world();
    let identity = save(&service, None, 0, None, concept("Execution", Rule::Default))
        .unwrap()
        .concept_id
        .unwrap();
    adopt(&service, 0, 1).unwrap();
    let mut local = concept("Project execution", Rule::Default);
    assert!(save(&service, Some(PROJECT), 1, Some(&identity), local.clone()).is_err());
    local.specialization_reason = "This product presents grouped executions.".to_owned();
    save(&service, Some(PROJECT), 1, Some(&identity), local).unwrap();
    assert_eq!(
        get(&service, Some(PROJECT), &identity, None).entry.origin,
        "specialized"
    );
    save(
        &service,
        None,
        1,
        Some(&identity),
        concept("Execution", Rule::Mandatory),
    )
    .unwrap();
    assert!(adopt(&service, 2, 2).is_err());
    assert_eq!(
        service
            .list(Some(PROJECT), List::default())
            .unwrap()
            .profile
            .revision,
        2
    );
    service
        .inherit(
            Some(PROJECT),
            Inherit {
                concept_id: identity.clone(),
                expected_revision: 2,
                ..Default::default()
            },
            "owner",
            "now",
        )
        .unwrap();
    adopt(&service, 3, 2).unwrap();
    assert_eq!(
        get(&service, Some(PROJECT), &identity, None).entry.origin,
        "inherited"
    );
    assert_eq!(
        get(&service, Some(PROJECT), &identity, Some(2))
            .entry
            .origin,
        "specialized"
    );
}

#[test]
fn glossary_semantic_changes_invalidate_unchanged_language_review() {
    let (_temporary, _database, service) = world();
    let identity = save(&service, None, 0, None, concept("Execution", Rule::Default))
        .unwrap()
        .concept_id
        .unwrap();
    let mut updated = get(&service, None, &identity, None).entry.concept;
    updated.definition = "One execution of an entire test group.".to_owned();
    save(&service, None, 1, Some(&identity), updated).unwrap();
    let mut reviewed = get(&service, None, &identity, None).entry.concept;
    assert!(reviewed.languages.values().all(|term| !term.reviewed));
    for term in reviewed.languages.values_mut() {
        term.reviewed = true;
    }
    save(&service, None, 2, Some(&identity), reviewed).unwrap();
    assert!(
        get(&service, None, &identity, None)
            .entry
            .concept
            .languages
            .values()
            .all(|term| term.reviewed)
    );
}

#[test]
fn glossary_checks_are_concept_scoped_and_accept_declared_inflections_and_unicode() {
    let (_temporary, _database, service) = world();
    let mut value = concept("Execution", Rule::Default);
    value.languages.insert(
        "fr".to_owned(),
        LanguageTerm {
            preferred: "Exécution".to_owned(),
            reviewed: true,
            ..Default::default()
        },
    );
    let identity = save(&service, None, 0, None, value)
        .unwrap()
        .concept_id
        .unwrap();
    let usages = [
        ("en", "Test runs"),
        ("ru", "Запуски теста"),
        ("fr", "Exe\u{301}cution"),
        ("en", "Job"),
        ("de", "Testlauf"),
        ("en", "Anything"),
    ]
    .into_iter()
    .map(|(language, term)| Usage {
        concept_id: identity.clone(),
        language: language.to_owned(),
        term: term.to_owned(),
    })
    .collect();
    let checked = service
        .check(
            None,
            Check {
                usages,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!checked.valid);
    assert_eq!(
        checked
            .findings
            .iter()
            .map(|finding| (finding.index, finding.code.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (3, "deprecated_term"),
            (4, "missing_language"),
            (5, "unapproved_term")
        ]
    );
    let mut different = concept("Employment", Rule::Default);
    different.languages.get_mut("en").unwrap().preferred = "Job".to_owned();
    different
        .languages
        .get_mut("en")
        .unwrap()
        .deprecated
        .clear();
    let different_id = save(&service, None, 1, None, different)
        .unwrap()
        .concept_id
        .unwrap();
    assert!(
        service
            .check(
                None,
                Check {
                    usages: vec![Usage {
                        concept_id: different_id,
                        language: "en".to_owned(),
                        term: "Job".to_owned()
                    }],
                    ..Default::default()
                }
            )
            .unwrap()
            .valid
    );
}

#[test]
fn glossary_search_pagination_and_validation_cover_real_language_variants() {
    let (_temporary, _database, service) = world();
    save(&service, None, 0, None, concept("Alpha", Rule::Default)).unwrap();
    save(&service, None, 1, None, concept("Beta", Rule::Default)).unwrap();
    let first = service
        .list(
            None,
            List {
                query: Some("запуски".to_owned()),
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(first.total, 2);
    assert_eq!(first.next_offset, Some(1));
    assert_eq!(
        service
            .list(
                None,
                List {
                    offset: Some(1),
                    limit: Some(1),
                    expected_revision: Some(2),
                    ..Default::default()
                }
            )
            .unwrap()
            .entries[0]
            .concept
            .name,
        "Beta"
    );
    assert_eq!(
        service
            .list(
                None,
                List {
                    expected_revision: Some(1),
                    ..Default::default()
                }
            )
            .unwrap_err()
            .code,
        ErrorCode::GlossaryConflict
    );
    let mut conflicting = concept("Conflict", Rule::Default);
    conflicting
        .languages
        .get_mut("en")
        .unwrap()
        .deprecated
        .push("Test run".to_owned());
    assert!(save(&service, None, 2, None, conflicting).is_err());
    let mut invalid_language = concept("Bad locale", Rule::Default);
    invalid_language
        .languages
        .insert("en_US".to_owned(), LanguageTerm::default());
    assert!(save(&service, None, 2, None, invalid_language).is_err());
    assert!(
        service
            .list(
                None,
                List {
                    limit: Some(0),
                    ..Default::default()
                }
            )
            .is_err()
    );
}

#[test]
fn glossary_guidance_inherits_without_allowing_mandatory_shadowing() {
    let (_temporary, _database, service) = world();
    let guideline = Guideline {
        key: "Truthful status".to_owned(),
        text: "Use names that describe the observed result.".to_owned(),
        rule: Rule::Mandatory,
        specialization_reason: String::new(),
    };
    service
        .configure(
            None,
            Configure {
                expected_revision: 0,
                languages: vec!["en".to_owned()],
                guidelines: vec![guideline.clone()],
                ..Default::default()
            },
            "owner",
            "now",
        )
        .unwrap();
    adopt(&service, 0, 1).unwrap();
    assert_eq!(
        service
            .list(Some(PROJECT), List::default())
            .unwrap()
            .profile
            .guidelines[0]
            .origin,
        "inherited"
    );
    let mut overridden = guideline;
    overridden.key = "truthful status".to_owned();
    overridden.specialization_reason = "A purported exception".to_owned();
    assert!(
        service
            .configure(
                Some(PROJECT),
                Configure {
                    expected_revision: 1,
                    guidelines: vec![overridden],
                    ..Default::default()
                },
                "owner",
                "now"
            )
            .is_err()
    );
    assert_eq!(
        service.impact(ImpactRequest::default()).unwrap().projects[0].baseline_revision,
        1
    );
}

#[test]
fn glossary_schema_upgrade_preserves_existing_rows_and_archive_blocks_writes() {
    let (temporary, database, service) = world();
    drop(service);
    database
        .call(|connection| {
            connection.execute("UPDATE meta SET value='16' WHERE key='schema_version'", [])?;
            Ok(())
        })
        .unwrap();
    database.close().unwrap();
    let database = Database::open(temporary.path().join("glossary.sqlite3")).unwrap();
    let service = GlossaryService::new(database.clone());
    assert_eq!(
        service.impact(ImpactRequest::default()).unwrap().projects[0].repository_id,
        PROJECT
    );
    database
        .call(|connection| {
            connection.execute(
                "UPDATE repositories SET archived_at='now' WHERE repository_id=$1",
                [PROJECT],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        save(
            &service,
            Some(PROJECT),
            0,
            None,
            concept("Local", Rule::Default)
        )
        .unwrap_err()
        .code,
        ErrorCode::RepositoryArchived
    );
    assert!(service.list(Some(PROJECT), List::default()).is_ok());
}

#[test]
fn glossary_pages_fit_the_edge_frame_without_losing_large_concepts() {
    let (_temporary, _database, service) = world();
    for index in 0..25 {
        let mut value = concept(&format!("Large concept {index:02}"), Rule::Default);
        value.definition = "Meaning ".repeat(480);
        value.context = "Context ".repeat(240);
        for term in value.languages.values_mut() {
            term.usage = "Usage ".repeat(300);
            term.examples = (0..8)
                .map(|number| format!("{number} {}", "Example ".repeat(60)))
                .collect();
        }
        save(&service, None, index, None, value).unwrap();
    }
    let mut offset = Some(0);
    let mut identities = BTreeSet::new();
    while let Some(current) = offset {
        let page = service
            .list(
                None,
                List {
                    offset: Some(current),
                    limit: Some(25),
                    expected_revision: Some(25),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() < 220 * 1024);
        assert!(!page.entries.is_empty());
        for entry in page.entries {
            assert!(identities.insert(entry.concept_id));
        }
        offset = page.next_offset;
    }
    assert_eq!(identities.len(), 25);
}
