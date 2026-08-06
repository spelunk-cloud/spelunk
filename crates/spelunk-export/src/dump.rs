//! The portable dump format: record types, serialisation, and integrity.
//!
//! The format is line-delimited JSON and is expressed as **entities and
//! relationships**, never as a copy of any database's tables. Tables are an
//! implementation detail that moves between releases; entities and
//! relationships are what the data is. That choice is what lets a reader of
//! this format be written against the format alone, with no knowledge of the
//! store that produced it.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const FORMAT: &str = "portable-dump";
pub const FORMAT_VERSION: u32 = 1;

/// Records are digested individually and then folded into a whole-file digest.
///
/// Every record *preceding* the footer contributes, header included, so a
/// tampered header is caught by the same check as a tampered entity. The fold
/// is over the hex digests in file order, which makes the result sensitive to
/// record order as well as content: a reordered dump is a different dump.
#[derive(Default)]
pub struct Digester {
    per_record: Vec<String>,
}

impl Digester {
    pub fn push_line(&mut self, line: &str) {
        let mut h = Sha256::new();
        h.update(line.as_bytes());
        self.per_record.push(hex(&h.finalize()));
    }

    pub fn per_record(&self) -> &[String] {
        &self.per_record
    }

    pub fn finish(&self) -> String {
        let mut h = Sha256::new();
        for d in &self.per_record {
            h.update(d.as_bytes());
        }
        format!("sha256:{}", hex(&h.finalize()))
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[derive(Serialize, Deserialize)]
pub struct Header {
    pub record: String,
    pub format: String,
    pub format_version: u32,
    pub generated_at: i64,
    pub generator: String,
}

impl Header {
    pub fn new(generated_at: i64) -> Self {
        Self {
            record: "header".into(),
            format: FORMAT.into(),
            format_version: FORMAT_VERSION,
            generated_at,
            generator: format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        }
    }
}

/// A memory entry.
///
/// `reference` is unique within one dump and exists only to wire relationships.
/// It is not identity, it is not stable across dumps, and a reader must not
/// persist it.
///
/// `uuid` is carried verbatim when the source row has one and omitted when it
/// does not. This tool never mints an identifier: identity policy belongs to
/// whatever reads the dump, and `created_at` is mandatory precisely so a reader
/// can seed a time ordered identifier from the entry's own creation time rather
/// than from the clock at import.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct MemoryEntry {
    pub record: String,
    #[serde(rename = "type")]
    pub kind_tag: String,
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    pub kind: String,
    pub title: String,
    pub body: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub linked_files: Vec<String>,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// A registered project.
///
/// The store path the registry keeps beside the root is deliberately absent:
/// every production registration derives it from the root, so carrying it would
/// carry a path that is wrong for any reader that lays its stores out
/// differently. A reader derives it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Project {
    pub record: String,
    #[serde(rename = "type")]
    pub kind_tag: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub root_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registered_at: Option<i64>,
}

/// One recorded command invocation.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct CommandUsage {
    pub record: String,
    #[serde(rename = "type")]
    pub kind_tag: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub command: String,
    pub at: i64,
}

/// A link between two entities, by their dump local references.
///
/// `supersedes` is oriented **from successor to predecessor**, matching the
/// edge table's own orientation. The lifecycle column that encodes the same
/// fact sits on the predecessor and points the other way, so it is inverted on
/// the way out. A writer that emitted both without inverting one would produce
/// two contradictory edges for a single fact, and a fixture carrying only one
/// of the two encodings would not notice.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub struct Relationship {
    pub record: String,
    #[serde(rename = "type")]
    pub kind_tag: String,
    pub from: String,
    pub to: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
}

impl Relationship {
    pub fn new(kind: &str, from: String, to: String, created_at: Option<i64>) -> Self {
        Self {
            record: "relationship".into(),
            kind_tag: kind.into(),
            from,
            to,
            created_at,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Footer {
    pub record: String,
    pub counts: Counts,
    pub digest: String,
}

#[derive(Serialize, Deserialize, Default, PartialEq, Eq, Debug)]
pub struct Counts {
    pub entity: BTreeMap<String, usize>,
    pub relationship: BTreeMap<String, usize>,
}

impl Counts {
    pub fn add_entity(&mut self, kind: &str) {
        *self.entity.entry(kind.to_string()).or_default() += 1;
    }
    pub fn add_relationship(&mut self, kind: &str) {
        *self.relationship.entry(kind.to_string()).or_default() += 1;
    }
}

/// Everything one dump carries, in the order it is written.
#[derive(Default)]
pub struct Dump {
    pub entries: Vec<MemoryEntry>,
    pub projects: Vec<Project>,
    pub usage: Vec<CommandUsage>,
    pub relationships: Vec<Relationship>,
}

impl Dump {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
            && self.projects.is_empty()
            && self.usage.is_empty()
            && self.relationships.is_empty()
    }

    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for e in &self.entries {
            c.add_entity(&e.kind_tag);
        }
        for p in &self.projects {
            c.add_entity(&p.kind_tag);
        }
        for u in &self.usage {
            c.add_entity(&u.kind_tag);
        }
        for r in &self.relationships {
            c.add_relationship(&r.kind_tag);
        }
        c
    }

    /// Serialise to the exact bytes of the dump file, alongside the per record
    /// digests that back the footer.
    pub fn render(&self, generated_at: i64) -> Result<(String, Vec<String>, Footer)> {
        let mut out = String::new();
        let mut dig = Digester::default();

        let push = |out: &mut String, dig: &mut Digester, line: String| {
            dig.push_line(&line);
            out.push_str(&line);
            out.push('\n');
        };

        push(
            &mut out,
            &mut dig,
            serde_json::to_string(&Header::new(generated_at))?,
        );
        for e in &self.entries {
            push(&mut out, &mut dig, serde_json::to_string(e)?);
        }
        for p in &self.projects {
            push(&mut out, &mut dig, serde_json::to_string(p)?);
        }
        for u in &self.usage {
            push(&mut out, &mut dig, serde_json::to_string(u)?);
        }
        for r in &self.relationships {
            push(&mut out, &mut dig, serde_json::to_string(r)?);
        }

        let footer = Footer {
            record: "footer".into(),
            counts: self.counts(),
            digest: dig.finish(),
        };
        out.push_str(&serde_json::to_string(&footer)?);
        out.push('\n');
        Ok((out, dig.per_record().to_vec(), footer))
    }
}

/// What re-reading a dump file establishes: that it is structurally whole, that
/// its footer agrees with its contents, and what each record hashed to.
#[derive(Debug)]
pub struct ReadBack {
    pub per_record: Vec<String>,
    pub counts: Counts,
}

/// Re-read a rendered dump, recomputing everything the footer claims.
///
/// A dump is refused whole or accepted whole. There is no partial read: the
/// failure this guards against is silent partial loss, so anything less than a
/// loud refusal defeats the purpose.
pub fn verify_rendered(text: &str) -> Result<ReadBack> {
    let mut lines = text.lines();
    let Some(header_line) = lines.next() else {
        bail!("dump is empty: expected a header record");
    };
    let header: Header =
        serde_json::from_str(header_line).map_err(|e| anyhow::anyhow!("unreadable header: {e}"))?;
    if header.record != "header" {
        bail!("first record is '{}', expected 'header'", header.record);
    }
    if header.format != FORMAT {
        bail!("unknown dump format '{}'", header.format);
    }
    if header.format_version != FORMAT_VERSION {
        bail!(
            "dump format version {} is not supported by this build (supports {FORMAT_VERSION})",
            header.format_version
        );
    }

    let mut dig = Digester::default();
    dig.push_line(header_line);
    let mut counts = Counts::default();
    let mut footer: Option<Footer> = None;

    for line in lines {
        if footer.is_some() {
            bail!("records follow the footer; the dump is not whole");
        }
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|e| anyhow::anyhow!("unreadable record: {e}"))?;
        match value.get("record").and_then(|v| v.as_str()) {
            Some("footer") => {
                footer = Some(serde_json::from_str(line)?);
            }
            Some("entity") => {
                let t = value
                    .get("type")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("entity record without a type"))?;
                counts.add_entity(t);
                dig.push_line(line);
            }
            Some("relationship") => {
                let t = value
                    .get("type")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("relationship record without a type"))?;
                counts.add_relationship(t);
                dig.push_line(line);
            }
            other => bail!("unknown record kind {other:?}"),
        }
    }

    let Some(footer) = footer else {
        bail!("dump has no footer; it is truncated");
    };
    if footer.counts != counts {
        bail!("footer counts do not match the records present");
    }
    if footer.digest != dig.finish() {
        bail!("dump digest does not match its contents");
    }
    Ok(ReadBack {
        per_record: dig.per_record().to_vec(),
        counts,
    })
}
