use atr::providers::{cache_hit_ratio, parse_blob, parse_line, session_id, ParsedEvent};

fn fixture(name: &str) -> String {
    let p = format!("{}/tests/fixtures/usage/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(p).unwrap()
}

#[test]
fn claude_fixture() {
    let line = fixture("claude.json");
    match parse_line("claude", line.trim()).unwrap() {
        ParsedEvent::Usage { usage, .. } => {
            assert_eq!(usage.input, 100);
            assert_eq!(usage.cache_write, 20);
            assert_eq!(usage.cache_read, 800);
            assert_eq!(usage.output, 50);
            assert_eq!(usage.cost_usd, Some(0.0123));
            let hit = cache_hit_ratio("claude", &usage).unwrap();
            assert!((hit - 800.0 / 920.0).abs() < 1e-9);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        session_id(&serde_json::from_str(&line).unwrap()).as_deref(),
        Some("claude-sess-1")
    );
}

#[test]
fn grok_json_fixture() {
    let blob = fixture("grok.json");
    let evs = parse_blob("grok", blob.trim());
    let usage = evs
        .iter()
        .find_map(|e| match e {
            ParsedEvent::Usage { usage, .. } => Some(usage.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(usage.input, 10);
    assert_eq!(usage.cache_read, 90);
    assert_eq!(usage.cache_write, 5);
    assert_eq!(usage.output, 12);
    assert_eq!(usage.cost_usd, Some(0.004));
}

#[test]
fn system_init_carries_session() {
    let line = r#"{"type":"system","subtype":"init","session_id":"claude-sess-early"}"#;
    assert_eq!(
        parse_line("claude", line),
        Some(ParsedEvent::Session("claude-sess-early".into()))
    );
}

#[test]
fn grok_streaming_result_line() {
    let line = r#"{"type":"result","session_id":"g-stream","total_cost_usd":0.004,"usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":5,"output_tokens":12,"reasoning_tokens":3,"total_tokens":120}}"#;
    match parse_line("grok", line).unwrap() {
        ParsedEvent::Usage { usage, .. } => {
            assert_eq!(usage.cache_read, 90);
            assert_eq!(usage.cost_usd, Some(0.004));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn cursor_fixture() {
    let line = fixture("cursor.json");
    match parse_line("cursor", line.trim()).unwrap() {
        ParsedEvent::Usage { usage, .. } => {
            assert_eq!(usage.input, 80);
            assert_eq!(usage.output, 40);
            assert_eq!(usage.cache_read, 400);
            assert_eq!(usage.cache_write, 15);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn codex_fixtures() {
    assert_eq!(
        parse_line("codex", fixture("codex-started.json").trim()),
        Some(ParsedEvent::Session("codex-thread-1".into()))
    );
    match parse_line("codex", fixture("codex-completed.json").trim()).unwrap() {
        ParsedEvent::Usage { usage, .. } => {
            assert_eq!(usage.input, 40, "uncached = input - cached");
            assert_eq!(usage.cache_read, 160);
            assert_eq!(usage.cache_write, 10);
            assert_eq!(usage.output, 30);
            assert!((cache_hit_ratio("codex", &usage).unwrap() - 160.0 / 200.0).abs() < 1e-9);
        }
        other => panic!("{other:?}"),
    }
    match parse_line("codex", fixture("codex-failed.json").trim()).unwrap() {
        ParsedEvent::Failed(m) => assert!(m.contains("rate limit")),
        other => panic!("{other:?}"),
    }
}
