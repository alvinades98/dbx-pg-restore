//! Parses `pg_restore -l` output and filters it into a `-L` list for selective restores.

use std::collections::{BTreeMap, BTreeSet, HashSet};

// TOC entry types made of several words; every other type is a single word. Longer variants come first.
const MULTI_WORD_TYPES: &[&str] = &[
    "MATERIALIZED VIEW DATA", "MATERIALIZED VIEW", "PUBLICATION TABLES IN SCHEMA", "PUBLICATION TABLE",
    "TEXT SEARCH CONFIGURATION", "TEXT SEARCH DICTIONARY", "TEXT SEARCH PARSER", "TEXT SEARCH TEMPLATE",
    "FOREIGN DATA WRAPPER", "FOREIGN TABLE", "FOREIGN SERVER", "SEQUENCE OWNED BY", "SEQUENCE SET",
    "TABLE DATA", "TABLE ATTACH", "INDEX ATTACH", "FK CONSTRAINT", "CHECK CONSTRAINT", "DEFAULT ACL",
    "USER MAPPING", "EVENT TRIGGER", "LARGE OBJECT", "BLOB METADATA", "OPERATOR CLASS", "OPERATOR FAMILY",
    "ACCESS METHOD", "PROCEDURAL LANGUAGE", "ROW SECURITY", "SECURITY LABEL", "STATISTICS DATA",
    "DATABASE PROPERTIES",
];

// Definitions that tables in other schemas commonly depend on (e.g. an enum in `public` used by `akademik`).
// They are restored even from unselected schemas; tables, data and the rest of those schemas are not.
const SUPPORT_TYPES: &[&str] = &[
    "TYPE", "DOMAIN", "FUNCTION", "PROCEDURE", "AGGREGATE", "OPERATOR", "OPERATOR CLASS", "OPERATOR FAMILY",
    "COLLATION", "CONVERSION", "TEXT SEARCH CONFIGURATION", "TEXT SEARCH DICTIONARY",
];

// Types whose tag is a single name followed by the owner, so the owner can be read reliably.
const OWNED_TYPES: &[&str] = &["SCHEMA", "TABLE", "SEQUENCE", "VIEW", "MATERIALIZED VIEW", "TYPE", "DOMAIN"];

#[derive(Debug)]
pub struct Entry {
    pub line: String,
    pub kind: String,
    /// Schema of the object, or "-" for database-level objects (schemas themselves, extensions, ...).
    pub ns: String,
    /// Object name, possibly followed by more words and the owner.
    pub tag: String,
}

impl Entry {
    fn word(&self, n: usize) -> &str {
        self.tag.split_whitespace().nth(n).unwrap_or("")
    }

    pub fn owner(&self) -> Option<&str> {
        let words: Vec<&str> = self.tag.split_whitespace().collect();
        (OWNED_TYPES.contains(&self.kind.as_str()) && words.len() == 2).then(|| words[1])
    }
}

#[derive(Debug, Default)]
pub struct Toc {
    pub dumped_by: Option<String>,
    pub dumped_from: Option<String>,
    pub entries: Vec<Entry>,
}

pub fn parse(text: &str) -> Toc {
    let mut toc = Toc::default();
    for line in text.lines() {
        if let Some(comment) = line.strip_prefix(';') {
            let comment = comment.trim();
            if let Some(v) = comment.strip_prefix("Dumped by pg_dump version:") {
                toc.dumped_by = Some(v.trim().to_string());
            } else if let Some(v) = comment.strip_prefix("Dumped from database version:") {
                toc.dumped_from = Some(v.trim().to_string());
            }
        } else if let Some(entry) = parse_entry(line) {
            toc.entries.push(entry);
        }
    }
    toc
}

fn parse_entry(line: &str) -> Option<Entry> {
    // "<id>; <tableoid> <oid> <TYPE> <schema|-> <tag...> <owner>"
    let (_, rest) = line.split_once("; ")?;
    let body = rest.splitn(3, ' ').nth(2)?;
    let kind = MULTI_WORD_TYPES
        .iter()
        .find(|t| body.starts_with(&format!("{t} ")))
        .map(|t| t.to_string())
        .unwrap_or_else(|| body.split(' ').next().unwrap_or("").to_string());
    let after = body.get(kind.len() + 1..).unwrap_or("");
    let (ns, tag) = after.split_once(' ').unwrap_or((after, ""));
    Some(Entry { line: line.to_string(), kind, ns: ns.to_string(), tag: tag.to_string() })
}

/// What to restore: `schemas` (None = all) and tables whose data should be skipped.
pub struct Selection {
    pub schemas: Option<HashSet<String>>,
    pub skip_data: HashSet<(String, String)>,
}

impl Selection {
    pub fn is_everything(&self) -> bool {
        self.schemas.is_none() && self.skip_data.is_empty()
    }

    fn has_schema(&self, name: &str) -> bool {
        self.schemas.as_ref().map_or(true, |s| s.contains(name))
    }

    fn keeps(&self, e: &Entry, support: &HashSet<&str>) -> bool {
        if e.kind == "SCHEMA" {
            return self.has_schema(e.word(0)) || support.contains(e.word(0));
        }
        if e.ns == "-" {
            // Comments/ACLs on a schema live at database level but belong to that schema.
            return e.word(0) != "SCHEMA" || self.has_schema(e.word(1));
        }
        if !self.has_schema(&e.ns) {
            return SUPPORT_TYPES.contains(&e.kind.as_str());
        }
        !(e.kind == "TABLE DATA" && self.skip_data.contains(&(e.ns.clone(), e.word(0).to_string())))
    }

    /// Entries to restore, in TOC order.
    pub fn select<'a>(&self, toc: &'a Toc) -> Vec<&'a Entry> {
        // Unselected schemas that hold support definitions still need their CREATE SCHEMA.
        let support: HashSet<&str> = toc.entries.iter()
            .filter(|e| e.ns != "-" && !self.has_schema(&e.ns) && SUPPORT_TYPES.contains(&e.kind.as_str()))
            .map(|e| e.ns.as_str())
            .collect();
        toc.entries.iter().filter(|e| self.keeps(e, &support)).collect()
    }
}

/// Schemas with their tables, for the selection tree in the UI.
pub fn schemas(toc: &Toc) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for e in &toc.entries {
        match e.kind.as_str() {
            "SCHEMA" => { map.entry(e.word(0).to_string()).or_default(); }
            "TABLE" => map.entry(e.ns.clone()).or_default().push(e.word(0).to_string()),
            _ => {}
        }
    }
    map
}

pub fn owners<'a>(entries: impl Iterator<Item = &'a Entry>) -> BTreeSet<String> {
    entries.filter_map(Entry::owner).map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = ";
;     Dumped from database version: 14.24 (Ubuntu 14.24-0ubuntu0.22.04.1)
;     Dumped by pg_dump version: 18.6
;
5; 2615 156987 SCHEMA - akademik user_remote
19; 2615 156985 SCHEMA - public user_remote
8092; 0 0 COMMENT - SCHEMA public user_remote
8093; 0 0 ACL - SCHEMA public user_remote
2; 3079 1 EXTENSION - pgcrypto 
7000; 1259 157075 TABLE akademik document_headers user_remote
7001; 1259 157076 TABLE akademik logs app
7609; 0 157075 TABLE DATA akademik document_headers user_remote
7610; 0 157076 TABLE DATA akademik logs app
8094; 0 0 COMMENT akademik COLUMN document_headers.title user_remote
8105; 0 0 SEQUENCE OWNED BY akademik document_headers_id_seq user_remote
6323; 2606 160189 FK CONSTRAINT akademik document_headers akademik_fk user_remote
7100; 1259 157500 TABLE public users user_remote
7101; 1259 157501 TYPE public jenis_kelamin user_remote
7102; 0 0 COMMENT public TYPE jenis_kelamin user_remote
20; 2615 156990 SCHEMA - unused user_remote
";

    #[test]
    fn parses_header_types_and_owners() {
        let toc = parse(SAMPLE);
        assert_eq!(toc.dumped_by.as_deref(), Some("18.6"));
        assert_eq!(toc.dumped_from.as_deref(), Some("14.24 (Ubuntu 14.24-0ubuntu0.22.04.1)"));
        assert_eq!(toc.entries.len(), 16);
        let kinds: Vec<&str> = toc.entries.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&"TABLE DATA") && kinds.contains(&"SEQUENCE OWNED BY") && kinds.contains(&"FK CONSTRAINT"));
        let fk = toc.entries.iter().find(|e| e.kind == "FK CONSTRAINT").unwrap();
        assert_eq!((fk.ns.as_str(), fk.word(0)), ("akademik", "document_headers"));
        assert_eq!(owners(toc.entries.iter()).into_iter().collect::<Vec<_>>(), ["app", "user_remote"]);
        let tree = schemas(&toc);
        assert_eq!(tree["akademik"], ["document_headers", "logs"]);
        assert_eq!(tree["public"], ["users"]);
    }

    #[test]
    fn selection_filters_schemas_and_table_data() {
        let toc = parse(SAMPLE);
        let sel = Selection {
            schemas: Some(HashSet::from(["akademik".to_string()])),
            skip_data: HashSet::from([("akademik".to_string(), "logs".to_string())]),
        };
        let kept: Vec<&str> = sel.select(&toc).iter().map(|e| e.line.split(';').next().unwrap()).collect();
        // akademik schema + objects and the extension are kept; logs data is skipped. From public only its
        // schema and the enum type akademik depends on survive (no comment/ACL/table); "unused" is dropped.
        assert_eq!(kept, ["5", "19", "2", "7000", "7001", "7609", "8094", "8105", "6323", "7101"]);
        let all = Selection { schemas: None, skip_data: HashSet::new() };
        assert!(all.is_everything());
        assert_eq!(all.select(&toc).len(), toc.entries.len());
    }
}
