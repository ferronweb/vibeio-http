//! HPACK fixture corpus: parsing and sanity checks.
//!
//! Story files live in `tests/fixtures/hpack/stories/` — see the
//! `README.md` there for provenance and format. Decoder/encoder tests
//! that consume the corpus are added by the HPACK implementation commits.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;

#[derive(Debug)]
pub(crate) struct Story {
    pub(crate) cases: Vec<Case>,
}

#[derive(Debug)]
pub(crate) struct Case {
    pub(crate) wire: Vec<u8>,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) header_table_size: Option<u32>,
}

/// All story files in the corpus, sorted for deterministic ordering.
pub(crate) fn story_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in fs::read_dir("tests/fixtures/hpack/stories").unwrap() {
        let dir = entry.unwrap();
        if !dir.path().is_dir() {
            continue;
        }
        for file in fs::read_dir(dir.path()).unwrap() {
            let file = file.unwrap();
            if file.path().extension().is_some_and(|e| e == "json") {
                paths.push(file.path());
            }
        }
    }
    paths.sort();
    paths
}

/// Parses a story file into ordered decode cases.
pub(crate) fn parse_story(path: &Path) -> Story {
    let text = fs::read_to_string(path).expect("read story file");
    let json: Value = serde_json::from_str(&text).expect("parse story json");
    let cases = json
        .get("cases")
        .and_then(|v| v.as_array())
        .expect("story cases is an array");

    let mut parsed = Vec::with_capacity(cases.len());
    for case in cases {
        let seqno = case.get("seqno").and_then(Value::as_u64).unwrap_or(0);
        let wire_hex = case
            .get("wire")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("case {seqno}: missing wire"));
        let wire = parse_hex(wire_hex);

        let headers = case
            .get("headers")
            .and_then(|v| v.as_array())
            .expect("case headers is an array");
        let mut header_list = Vec::with_capacity(headers.len());
        for header in headers {
            let obj = header.as_object().expect("header is an object");
            assert_eq!(obj.len(), 1, "case {seqno}: header object has one key");
            let (name, value) = obj.iter().next().unwrap();
            header_list.push((
                name.clone(),
                value.as_str().expect("header value").to_owned(),
            ));
        }

        let header_table_size = case
            .get("header_table_size")
            .and_then(Value::as_u64)
            .map(|v| u32::try_from(v).expect("header_table_size fits in u32"));

        parsed.push(Case {
            wire,
            headers: header_list,
            header_table_size,
        });
    }

    assert!(!parsed.is_empty(), "story {} has no cases", path.display());
    Story { cases: parsed }
}

fn parse_hex(hex: &str) -> Vec<u8> {
    assert!(hex.len() % 2 == 0, "wire hex string has even length");
    hex.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let hi = hexval(pair[0]);
            let lo = hexval(pair[1]);
            (hi << 4) | lo
        })
        .collect()
}

#[inline]
fn hexval(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => panic!("invalid hex digit {b:#x}"),
    }
}

#[test]
fn corpus_parses() {
    let paths = story_paths();
    assert!(
        paths.len() >= 130,
        "expected a substantial fixture corpus, found {} files",
        paths.len()
    );

    let mut total_cases = 0usize;
    let mut total_headers = 0usize;
    for path in &paths {
        let story = parse_story(path);
        total_cases += story.cases.len();
        total_headers += story.cases.iter().map(|c| c.headers.len()).sum::<usize>();
        for case in &story.cases {
            assert!(!case.wire.is_empty(), "{}: empty wire", path.display());
        }
    }

    // Sanity bounds on corpus coverage; prints useful info when run -v.
    println!("stories: {}", paths.len());
    println!("cases: {total_cases}");
    println!("header entries: {total_headers}");
    assert!(
        total_cases > 1000,
        "corpus is too small: {total_cases} cases"
    );
}
