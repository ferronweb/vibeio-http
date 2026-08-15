use super::*;

fn parse_ok(pairs: &[(&str, &str)]) -> ParsedRequest {
    let headers = pairs
        .iter()
        .map(|(name, value)| Header::new(name.to_string(), value.to_string()))
        .collect::<Vec<_>>();
    parse_request(&headers).expect("request should parse")
}

#[inline]
fn pair_header(pair: &(&str, &str)) -> Header {
    Header::new(pair.0.to_string(), pair.1.to_string())
}

#[test]
fn parses_a_regular_request() {
    let parsed = parse_ok(&[
        (":method", "GET"),
        (":scheme", "https"),
        (":authority", "example.com"),
        (":path", "/index.html?q=1"),
    ]);
    assert_eq!(parsed.method, Method::GET);
    assert_eq!(parsed.uri, "https://example.com/index.html?q=1");
    assert_eq!(parsed.content_length, None);
    assert!(!parsed.expect_continue);
}

#[test]
fn parses_without_authority() {
    let parsed = parse_ok(&[(":method", "GET"), (":scheme", "http"), (":path", "/")]);
    assert_eq!(parsed.uri, "/");
}

#[test]
fn parses_asterisk_form() {
    let parsed = parse_ok(&[(":method", "OPTIONS"), (":scheme", "http"), (":path", "*")]);
    assert_eq!(parsed.uri, "*");
    assert_eq!(parsed.method, Method::OPTIONS);
}

#[test]
fn parses_connect_with_authority() {
    let parsed = parse_ok(&[(":method", "CONNECT"), (":authority", "example.com:443")]);
    assert_eq!(parsed.method, Method::CONNECT);
    assert!(parsed.is_connect);
    assert_eq!(parsed.uri, "example.com:443");
}

#[test]
fn rejects_unknown_and_response_pseudo_headers() {
    for name in [":test", ":status", ":foo"] {
        assert!(parse_request(&[Header::new(name, "1")]).is_err());
    }
}

#[test]
fn rejects_pseudo_after_regular() {
    let headers = vec![Header::new("x-test", "ok"), Header::new(":method", "GET")];
    assert!(parse_request(&headers).is_err());
}

#[test]
fn rejects_duplicated_pseudo_headers() {
    let base = [(":method", "GET"), (":scheme", "http"), (":path", "/")];
    for dup in [":method", ":scheme", ":path"] {
        let mut pairs = base.to_vec();
        pairs.push((dup, "x"));
        assert!(parse_request(&pairs.iter().map(pair_header).collect::<Vec<_>>()).is_err());
    }
}

#[test]
fn rejects_missing_required_pseudo_headers() {
    let base = [(":method", "GET"), (":scheme", "http"), (":path", "/")];
    for skip in 0..base.len() {
        let pairs = base
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != skip)
            .map(|(_, pair)| *pair)
            .collect::<Vec<_>>();
        assert!(parse_request(&pairs.iter().map(pair_header).collect::<Vec<_>>()).is_err());
    }
    // Empty :path
    assert!(parse_request(&[
        Header::new(":method", "GET"),
        Header::new(":scheme", "http"),
        Header::new(":path", ""),
    ])
    .is_err());
}

#[test]
fn rejects_connection_specific_headers() {
    for name in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "transfer-encoding",
        "upgrade",
    ] {
        let mut pairs = vec![(":method", "GET"), (":scheme", "http"), (":path", "/")];
        pairs.push((name, "x"));
        let headers = pairs.iter().map(pair_header).collect::<Vec<_>>();
        assert!(
            parse_request(&headers).is_err(),
            "{name} should be rejected"
        );
    }
    // TE may only carry "trailers".
    for te in ["gzip", "trailers, deflate", ""] {
        let headers = vec![
            Header::new(":method", "GET"),
            Header::new(":scheme", "http"),
            Header::new(":path", "/"),
            Header::new("te", te),
        ];
        assert!(parse_request(&headers).is_err(), "te: {te:?}");
    }
    assert!(parse_request(&[
        Header::new(":method", "GET"),
        Header::new(":scheme", "http"),
        Header::new(":path", "/"),
        Header::new("te", "trailers"),
    ])
    .is_ok());
}

#[test]
fn rejects_bad_content_lengths() {
    for value in [
        "1 2",
        "abc",
        "0x10",
        "+1",
        "-1",
        "1,2",
        "18446744073709551616",
    ] {
        let headers = vec![
            Header::new(":method", "POST"),
            Header::new(":scheme", "http"),
            Header::new(":path", "/"),
            Header::new("content-length", value),
        ];
        assert!(
            parse_request(&headers).is_err(),
            "content-length: {value:?}"
        );
    }
    // Identical duplicates are legal (RFC 9110 Section 8.6).
    let parsed = parse_request(&[
        Header::new(":method", "POST"),
        Header::new(":scheme", "http"),
        Header::new(":path", "/"),
        Header::new("content-length", "4"),
        Header::new("content-length", "4"),
    ])
    .expect("identical content-lengths are legal");
    assert_eq!(parsed.content_length, Some(4));
}

#[test]
fn rejects_uppercase_and_invalid_header_names_and_values() {
    let headers = vec![
        Header::new(":method", "GET"),
        Header::new(":scheme", "http"),
        Header::new(":path", "/"),
        Header::new("X-Test", "ok"),
    ];
    assert!(parse_request(&headers).is_err());
    let headers = vec![
        Header::new(":method", "GET"),
        Header::new(":scheme", "http"),
        Header::new(":path", "/"),
        Header::new("x-test", "bad\x00value"),
    ];
    assert!(parse_request(&headers).is_err());
}

#[test]
fn connect_rejects_scheme_path_and_protocol_misuse() {
    // :protocol outside CONNECT.
    let headers = vec![
        Header::new(":method", "POST"),
        Header::new(":scheme", "http"),
        Header::new(":path", "/"),
        Header::new(":protocol", "websocket"),
    ];
    assert!(parse_request(&headers).is_err());
    // CONNECT without :authority.
    let headers = vec![Header::new(":method", "CONNECT")];
    assert!(parse_request(&headers).is_err());
}

#[test]
fn trailers_reject_pseudo_headers() {
    assert!(parse_trailers(&[Header::new(":method", "POST")]).is_err());
    assert!(parse_trailers(&[Header::new("x-checksum", "abc")]).is_ok());
}

#[test]
fn content_length_parser() {
    assert_eq!(parse_content_length(b"42"), Ok(42));
    assert_eq!(parse_content_length(b" 42 "), Ok(42));
    assert!(parse_content_length(b"").is_err());
    assert!(parse_content_length(b"-1").is_err());
    assert!(parse_function_overflow());
}

fn parse_function_overflow() -> bool {
    // 2^64 overflows u64: rejected.
    parse_content_length(b"18446744073709551616").is_err()
}
