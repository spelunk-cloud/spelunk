// The dump format's own guarantees: a dump is whole or it is refused.
//
// Every case here is a way a dump could be wrong while still looking
// plausible. The point of the footer is that none of them can pass quietly,
// because the failure this format guards against is silent partial loss.

use spelunk_export::dump::{self, Dump, MemoryEntry, Relationship};

fn entry(reference: &str, title: &str, created_at: i64) -> MemoryEntry {
    MemoryEntry {
        record: "entity".into(),
        kind_tag: "memory_entry".into(),
        reference: reference.into(),
        uuid: None,
        kind: "decision".into(),
        title: title.into(),
        body: "body".into(),
        tags: vec![],
        linked_files: vec![],
        created_at,
        status: None,
        source_ref: None,
        valid_at: None,
        invalid_at: None,
        entity_id: None,
        remote_id: None,
        namespace: None,
    }
}

fn sample() -> Dump {
    Dump {
        entries: vec![entry("e1", "first", 100), entry("e2", "second", 200)],
        relationships: vec![Relationship::new(
            "supersedes",
            "e2".into(),
            "e1".into(),
            Some(300),
        )],
        ..Dump::default()
    }
}

fn rendered() -> String {
    sample().render(1_700_000_000).unwrap().0
}

#[test]
fn a_well_formed_dump_verifies() {
    let read_back = dump::verify_rendered(&rendered()).unwrap();
    assert_eq!(read_back.counts.entity["memory_entry"], 2);
    assert_eq!(read_back.counts.relationship["supersedes"], 1);
}

#[test]
fn an_empty_file_is_refused() {
    let err = dump::verify_rendered("").unwrap_err().to_string();
    assert!(err.contains("empty"), "got: {err}");
}

#[test]
fn a_dump_without_a_footer_is_refused_as_truncated() {
    let text = rendered();
    let truncated: String = text
        .lines()
        .filter(|l| !l.contains("\"footer\""))
        .map(|l| format!("{l}\n"))
        .collect();
    let err = dump::verify_rendered(&truncated).unwrap_err().to_string();
    assert!(err.contains("truncated"), "got: {err}");
}

#[test]
fn a_dump_cut_short_mid_stream_is_refused() {
    let text = rendered();
    let cut = &text[..text.len() / 2];
    assert!(dump::verify_rendered(cut).is_err());
}

#[test]
fn a_single_altered_byte_is_caught() {
    let text = rendered().replace("first", "firsT");
    let err = dump::verify_rendered(&text).unwrap_err().to_string();
    assert!(
        err.contains("counts") || err.contains("digest"),
        "got: {err}"
    );
}

#[test]
fn a_removed_record_is_caught_by_the_counts() {
    let text: String = rendered()
        .lines()
        .filter(|l| !l.contains("\"second\""))
        .map(|l| format!("{l}\n"))
        .collect();
    let err = dump::verify_rendered(&text).unwrap_err().to_string();
    assert!(err.contains("counts"), "got: {err}");
}

#[test]
fn reordered_records_are_caught_even_though_the_counts_still_agree() {
    let text = rendered();
    let lines: Vec<&str> = text.lines().collect();
    let mut reordered = lines.clone();
    reordered.swap(1, 2);
    let text: String = reordered.iter().map(|l| format!("{l}\n")).collect();
    let err = dump::verify_rendered(&text).unwrap_err().to_string();
    assert!(err.contains("digest"), "got: {err}");
}

#[test]
fn a_tampered_header_is_caught_by_the_same_check_as_a_tampered_entity() {
    let text = rendered().replace("\"generated_at\":1700000000", "\"generated_at\":1");
    let err = dump::verify_rendered(&text).unwrap_err().to_string();
    assert!(err.contains("digest"), "got: {err}");
}

#[test]
fn an_unsupported_format_version_is_refused_rather_than_parsed_optimistically() {
    let text = rendered().replace("\"format_version\":1", "\"format_version\":99");
    let err = dump::verify_rendered(&text).unwrap_err().to_string();
    assert!(err.contains("not supported"), "got: {err}");
}

#[test]
fn a_foreign_format_is_refused() {
    let text = rendered().replace("portable-dump", "something-else");
    let err = dump::verify_rendered(&text).unwrap_err().to_string();
    assert!(err.contains("unknown dump format"), "got: {err}");
}

#[test]
fn records_after_the_footer_are_refused() {
    let mut text = rendered();
    text.push_str(&serde_json::to_string(&entry("e3", "late", 400)).unwrap());
    text.push('\n');
    let err = dump::verify_rendered(&text).unwrap_err().to_string();
    assert!(err.contains("not whole"), "got: {err}");
}

#[test]
fn an_unknown_record_kind_is_refused() {
    let mut text = rendered();
    text = text.replace(
        "\"record\":\"footer\"",
        "\"record\":\"surprise\",\"was\":\"footer\"",
    );
    assert!(dump::verify_rendered(&text).is_err());
}

// The writer cannot currently produce either of the next two, which is the
// reason to check them in the reader: the self-verification a run publishes on
// is this function, so a writer change that broke a reference or an endpoint
// would otherwise be certified rather than caught.

#[test]
fn two_entities_sharing_a_reference_are_refused() {
    let dump = Dump {
        entries: vec![entry("e1", "first", 100), entry("e1", "second", 200)],
        ..Dump::default()
    };
    let text = dump.render(1_700_000_000).unwrap().0;
    let err = dump::verify_rendered(&text).unwrap_err().to_string();
    assert!(err.contains("share the reference"), "got: {err}");
}

#[test]
fn a_relationship_naming_an_absent_entity_is_refused() {
    let dump = Dump {
        entries: vec![entry("e1", "first", 100)],
        relationships: vec![Relationship::new(
            "supersedes",
            "e2".into(),
            "e1".into(),
            None,
        )],
        ..Dump::default()
    };
    let text = dump.render(1_700_000_000).unwrap().0;
    let err = dump::verify_rendered(&text).unwrap_err().to_string();
    assert!(err.contains("not an entity in this dump"), "got: {err}");
}

#[test]
fn a_relationship_may_precede_the_entity_it_names() {
    let text = rendered();
    let lines: Vec<&str> = text.lines().collect();
    let reordered: String = [lines[0], lines[3], lines[1], lines[2], lines[4]]
        .iter()
        .map(|l| format!("{l}\n"))
        .collect();
    // The digest is order sensitive, so a reordered dump cannot verify either
    // way. Failing on the digest rather than on the endpoint is the assertion:
    // the format puts no ordering constraint between the two record kinds, so
    // an endpoint check made while reading would reject a legal dump.
    let err = dump::verify_rendered(&reordered).unwrap_err().to_string();
    assert!(err.contains("digest"), "got: {err}");
}

#[test]
fn the_header_is_first_and_the_footer_is_last() {
    let text = rendered();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].contains("\"record\":\"header\""));
    assert!(lines[lines.len() - 1].contains("\"record\":\"footer\""));
    assert_eq!(lines.iter().filter(|l| l.contains("\"header\"")).count(), 1);
}
