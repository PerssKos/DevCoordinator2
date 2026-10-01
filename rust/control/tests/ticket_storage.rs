//! Acceptance at the persistent storage boundary. These checks do not claim
//! that the pending Console, federation transport or MCP adapters are complete.
use devcoordinator2_control::database::Database;
use devcoordinator2_control::tickets::{
    AddComment, Author, CreateTicket, FileInput, MAX_FILE_CHUNK, TicketError, TicketStore,
};

const NOW: &str = "2026-09-29T22:00:00Z";

fn origin() -> Author {
    Author {
        server: "https://atlas.example".into(),
        name: "Atlas operator".into(),
    }
}
fn upstream() -> Author {
    Author {
        server: "https://upstream.example".into(),
        name: "Upstream maintainer".into(),
    }
}
fn file(name: &str, bytes: &[u8]) -> FileInput {
    FileInput {
        name: name.into(),
        bytes: bytes.to_vec(),
    }
}
fn request(key: &str) -> CreateTicket {
    CreateTicket {
        request_key: key.into(),
        author: origin(),
        title: "Scheduled backups".into(),
        body: "Let us choose the backup schedule.".into(),
        files: vec![
            file("request.txt", b"Initial requirements"),
            file("plan.pdf", b"%PDF-1.7\nrequest document"),
        ],
    }
}

#[test]
fn ticket_and_every_comments_files_survive_restart_with_exact_message_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("authority.sqlite3");
    let database = Database::open(&path).unwrap();
    let store = TicketStore::new(database.clone(), "upstream.example").unwrap();
    let initial = store.create(request("new-request"), NOW).unwrap();
    assert_eq!(initial.summary.upstream, "https://upstream.example");
    assert_eq!(initial.attachments.len(), 2);
    assert_eq!(store.create(request("new-request"), NOW).unwrap(), initial);

    let big_file = vec![b'a'; MAX_FILE_CHUNK * 2 + 7];
    let first_input = AddComment {
        request_key: "origin-comment".into(),
        author: origin(),
        ticket_id: initial.summary.id.clone(),
        body: "Here is the configuration and a picture.".into(),
        files: vec![
            file("configuration.txt", &big_file),
            file("picture.png", b"\x89PNG\r\n\x1a\nfixture bytes"),
        ],
    };
    let first = store.add_comment(first_input.clone(), NOW).unwrap();
    assert_eq!(first.attachments.len(), 2);
    assert_eq!(store.add_comment(first_input, NOW).unwrap(), first);
    let second = store
        .add_comment(
            AddComment {
                request_key: "upstream-comment".into(),
                author: upstream(),
                ticket_id: initial.summary.id.clone(),
                body: "".into(),
                files: vec![
                    file("proposal.pdf", b"%PDF-1.7\nupstream proposal"),
                    file("notes.md", b"# Discussion\nKeep this beside the reply."),
                ],
            },
            NOW,
        )
        .unwrap();
    assert_eq!(second.attachments.len(), 2);
    assert_eq!(second.attachments[0].content_type, "application/pdf");
    assert_eq!(second.attachments[1].content_type, "text/plain");
    assert_eq!(
        store
            .get(&initial.summary.id)
            .unwrap()
            .summary
            .comment_count,
        2
    );

    let first_page = store.comments(&initial.summary.id, 0, 1).unwrap();
    assert_eq!(first_page.items, vec![first.clone()]);
    assert_eq!(first_page.next_offset, Some(1));
    let second_page = store.comments(&initial.summary.id, 1, 1).unwrap();
    assert_eq!(second_page.items, vec![second.clone()]);
    assert_eq!(second_page.next_offset, None);

    // The file's exact ticket AND comment must match, even for a valid ID.
    for wrong_comment in [None, Some(second.id.as_str())] {
        assert!(matches!(
            store.file(
                &initial.summary.id,
                wrong_comment,
                &first.attachments[0].id,
                0,
                32
            ),
            Err(TicketError::NotFound)
        ));
    }
    let another = store.create(request("other-request"), NOW).unwrap();
    assert!(matches!(
        store.file(
            &another.summary.id,
            Some(&first.id),
            &first.attachments[0].id,
            0,
            32
        ),
        Err(TicketError::NotFound)
    ));

    database.close().unwrap();
    drop(store);
    let reopened_database = Database::open(&path).unwrap();
    let reopened = TicketStore::new(reopened_database.clone(), "upstream.example").unwrap();
    assert_eq!(
        reopened.comments(&initial.summary.id, 0, 25).unwrap().items,
        vec![first.clone(), second]
    );
    assert_eq!(
        reopened.get(&initial.summary.id).unwrap().attachments,
        initial.attachments
    );
    let mut offset = 0;
    let mut downloaded = Vec::new();
    loop {
        let chunk = reopened
            .file(
                &initial.summary.id,
                Some(&first.id),
                &first.attachments[0].id,
                offset,
                MAX_FILE_CHUNK,
            )
            .unwrap();
        assert!(chunk.bytes.len() <= MAX_FILE_CHUNK);
        assert_eq!(chunk.attachment, first.attachments[0]);
        downloaded.extend(chunk.bytes);
        match chunk.next_offset {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(downloaded, big_file);
    assert!(matches!(
        reopened.file(
            &initial.summary.id,
            Some(&first.id),
            &first.attachments[0].id,
            big_file.len() as u64 + 1,
            32
        ),
        Err(TicketError::Invalid(_))
    ));
    reopened_database.close().unwrap();
}

#[test]
fn settings_edits_closure_and_removal_preserve_owner_and_reject_stale_updates() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(directory.path().join("authority.sqlite3")).unwrap();
    let store = TicketStore::new(database.clone(), "upstream.example").unwrap();
    let settings = store.settings().unwrap();
    assert_eq!(settings.upstream, "https://vr.ae");
    let saved = store
        .set_upstream("NEXT.example/", settings.revision)
        .unwrap();
    assert_eq!(saved.upstream, "https://next.example");
    assert!(matches!(
        store.set_upstream("elsewhere.example", settings.revision),
        Err(TicketError::Conflict)
    ));
    for invalid in [
        "https://user:secret@example.test",
        "https://example.test/path",
        "https://example.test/?token=value",
        "https://example.test/#part",
        "file:///tmp/state",
        "",
    ] {
        assert!(
            matches!(
                store.set_upstream(invalid, saved.revision),
                Err(TicketError::Invalid(_))
            ),
            "accepted {invalid}"
        );
    }
    let created = store.create(request("request"), NOW).unwrap();
    store
        .set_upstream("https://different.example", saved.revision)
        .unwrap();
    let edited = store
        .edit(
            &created.summary.id,
            "Backups with retention",
            "Keep seven backups.",
            1,
            upstream(),
            NOW,
        )
        .unwrap();
    assert_eq!(edited.summary.upstream, "https://upstream.example");
    assert_eq!(edited.summary.origin, "https://atlas.example");
    assert_eq!(edited.attachments, created.attachments);
    assert!(matches!(
        store.edit(
            &created.summary.id,
            "Old draft",
            "Stale description",
            1,
            origin(),
            NOW
        ),
        Err(TicketError::Conflict)
    ));
    assert_eq!(store.get(&created.summary.id).unwrap(), edited);
    let closed = store
        .set_closed(
            &created.summary.id,
            true,
            edited.summary.revision,
            upstream(),
            NOW,
        )
        .unwrap();
    assert!(closed.summary.closed);
    assert_eq!(
        store
            .list(Some("https://atlas.example"), Some(true), 0, 25)
            .unwrap()
            .items
            .len(),
        1
    );
    assert!(
        store
            .list(Some("https://other.example"), None, 0, 25)
            .unwrap()
            .items
            .is_empty()
    );
    let reopened = store
        .set_closed(
            &created.summary.id,
            false,
            closed.summary.revision,
            upstream(),
            NOW,
        )
        .unwrap();
    assert!(!reopened.summary.closed);
    store
        .remove(
            &created.summary.id,
            reopened.summary.revision,
            origin(),
            NOW,
        )
        .unwrap();
    assert!(matches!(
        store.get(&created.summary.id),
        Err(TicketError::NotFound)
    ));
    assert!(store.list(None, None, 0, 25).unwrap().items.is_empty());
    assert!(matches!(
        store.file(&created.summary.id, None, &created.attachments[0].id, 0, 32),
        Err(TicketError::NotFound)
    ));
    assert!(matches!(
        store.create(request("request"), NOW),
        Err(TicketError::NotFound)
    ));
    assert!(matches!(
        store.add_comment(
            AddComment {
                request_key: "late-comment".into(),
                author: origin(),
                ticket_id: created.summary.id,
                body: "Late reply".into(),
                files: vec![]
            },
            NOW
        ),
        Err(TicketError::NotFound)
    ));
    database.close().unwrap();
}

#[test]
fn failed_attachment_write_rolls_back_the_entire_comment_and_all_safe_checks_continue() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(directory.path().join("authority.sqlite3")).unwrap();
    let store = TicketStore::new(database.clone(), "upstream.example").unwrap();
    let parent = store.create(request("request"), NOW).unwrap();
    database.call(|db| {
        db.execute_batch("CREATE TRIGGER reject_fixture_attachment BEFORE INSERT ON ticket_attachments WHEN NEW.name='rejected.pdf' BEGIN SELECT RAISE(FAIL,'fixture attachment failure'); END;")?;
        Ok(())
    }).unwrap();
    let failed = AddComment {
        request_key: "retryable-comment".into(),
        author: origin(),
        ticket_id: parent.summary.id.clone(),
        body: "Files must save with this reply.".into(),
        files: vec![
            file("first.txt", b"first file"),
            file("rejected.pdf", b"%PDF-1.7\nrejected"),
        ],
    };
    assert!(matches!(
        store.add_comment(failed.clone(), NOW),
        Err(TicketError::Sqlite(_))
    ));
    assert_eq!(store.get(&parent.summary.id).unwrap(), parent);
    assert!(
        store
            .comments(&parent.summary.id, 0, 25)
            .unwrap()
            .items
            .is_empty()
    );
    database
        .call(|db| {
            assert_eq!(
                db.query_row("SELECT COUNT(*) FROM ticket_attachments", [], |r| r
                    .get::<_, u32>(0))?,
                2
            );
            assert_eq!(
                db.query_row("SELECT COUNT(*) FROM ticket_history", [], |r| r
                    .get::<_, u32>(0))?,
                1
            );
            db.execute_batch("DROP TRIGGER reject_fixture_attachment;")?;
            Ok(())
        })
        .unwrap();
    let saved = store.add_comment(failed.clone(), NOW).unwrap();
    assert_eq!(saved.attachments.len(), 2);
    let mut changed = failed;
    changed.body = "Different content with the old request key".into();
    assert!(matches!(
        store.add_comment(changed, NOW),
        Err(TicketError::IdempotencyConflict)
    ));
    assert_eq!(
        store.comments(&parent.summary.id, 0, 25).unwrap().items,
        vec![saved]
    );
    assert_eq!(store.get(&parent.summary.id).unwrap().summary.revision, 2);
    database.close().unwrap();
}

#[test]
fn files_remain_data_and_invalid_messages_never_partially_save() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(directory.path().join("authority.sqlite3")).unwrap();
    let store = TicketStore::new(database.clone(), "https://upstream.example").unwrap();
    let mut draft = request("request");
    draft.files = vec![
        file("screen.html", b"<script>alert('not executable')</script>"),
        file("document.docx", b"PK\x03\x04\0binary document"),
    ];
    let parent = store.create(draft.clone(), NOW).unwrap();
    assert_eq!(parent.attachments[0].content_type, "text/plain");
    assert_eq!(
        parent.attachments[1].content_type,
        "application/octet-stream"
    );
    draft.title = "Changed payload under same key".into();
    assert!(matches!(
        store.create(draft, NOW),
        Err(TicketError::IdempotencyConflict)
    ));
    for (key, files, body) in [
        ("bad-path", vec![file("../file.txt", b"data")], "reply"),
        ("empty-file", vec![file("empty.txt", b"")], "reply"),
        (
            "control-name",
            vec![file("two\nlines.txt", b"data")],
            "reply",
        ),
        (
            "duplicate-names",
            vec![file("same.txt", b"a"), file("same.txt", b"b")],
            "reply",
        ),
        ("no-content", vec![], "   "),
    ] {
        assert!(matches!(
            store.add_comment(
                AddComment {
                    request_key: key.into(),
                    author: origin(),
                    ticket_id: parent.summary.id.clone(),
                    body: body.into(),
                    files
                },
                NOW
            ),
            Err(TicketError::Invalid(_))
        ));
    }
    assert_eq!(store.get(&parent.summary.id).unwrap(), parent);
    assert!(
        store
            .comments(&parent.summary.id, 0, 25)
            .unwrap()
            .items
            .is_empty()
    );
    assert!(matches!(
        store.comments(&parent.summary.id, 0, 26),
        Err(TicketError::Invalid(_))
    ));
    // Long replies remain within the protocol response ceiling, and continuation
    // must not skip items when the byte limit produces a shorter page.
    for index in 0..25 {
        store
            .add_comment(
                AddComment {
                    request_key: format!("long-{index}"),
                    author: origin(),
                    ticket_id: parent.summary.id.clone(),
                    body: "\"".repeat(8192),
                    files: vec![],
                },
                NOW,
            )
            .unwrap();
    }
    let first = store.comments(&parent.summary.id, 0, 25).unwrap();
    assert!(serde_json::to_vec(&first).unwrap().len() < devcoordinator2_api::MAX_RESPONSE_BYTES);
    assert!(first.next_offset.is_some());
    let mut ids = first
        .items
        .iter()
        .map(|item| item.id.clone())
        .collect::<Vec<_>>();
    let mut next = first.next_offset;
    while let Some(offset) = next {
        let page = store.comments(&parent.summary.id, offset, 25).unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() < devcoordinator2_api::MAX_RESPONSE_BYTES);
        ids.extend(page.items.into_iter().map(|item| item.id));
        next = page.next_offset;
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 25);
    database.close().unwrap();
}
