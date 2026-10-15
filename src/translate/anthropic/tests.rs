use super::*;

#[test]
fn request_maps_to_responses() {
    let req = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 100,
        "system": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}],
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "ok"},
                {"type": "tool_use", "id": "toolu_1", "name": "f", "input": {"x": 1}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true,
                 "content": [{"type": "text", "text": "bad"}]}
            ]}
        ],
        "tools": [{"name": "f", "description": "d", "input_schema": {"type": "object"}}],
        "tool_choice": {"type": "any"},
        "thinking": {"type": "enabled", "budget_tokens": 4000}
    });
    let (r, opts) = to_responses(&req, "gpt-5").unwrap();
    assert_eq!(opts.model, "claude-sonnet-4-5");
    assert!(opts.thinking);
    assert_eq!(
        r,
        json!({
            "model": "gpt-5",
            "instructions": "a\n\nb",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]},
                {"type": "function_call", "call_id": "toolu_1", "name": "f", "arguments": "{\"x\":1}"},
                {"type": "function_call_output", "call_id": "toolu_1", "output": "Error: bad"}
            ],
            "tools": [{"type": "function", "name": "f", "description": "d",
                       "parameters": {"type": "object"}, "strict": false}],
            "tool_choice": "required",
            "parallel_tool_calls": true,
            "reasoning": {"effort": "medium", "summary": "auto"}
        })
    );
}

#[test]
fn stream_text_then_tool_call() {
    let mut s = Stream::new(Opts {
        model: "claude-x".into(),
        thinking: false,
    });
    let evs = [
        json!({"type": "response.created", "response": {"id": "resp_abc"}}),
        json!({"type": "response.output_item.added", "item": {"type": "message"}}),
        json!({"type": "response.content_part.added", "part": {"type": "output_text"}}),
        json!({"type": "response.output_text.delta", "delta": "Hi"}),
        json!({"type": "response.output_text.done", "text": "Hi"}),
        json!({"type": "response.output_item.added",
               "item": {"type": "function_call", "call_id": "call_1", "name": "f"}}),
        json!({"type": "response.function_call_arguments.delta", "delta": "{}"}),
        json!({"type": "response.output_item.done",
               "item": {"type": "function_call", "call_id": "call_1", "arguments": "{}"}}),
        json!({"type": "response.completed", "response": {"id": "resp_abc", "output": [
            {"type": "function_call"}],
            "usage": {"input_tokens": 10, "output_tokens": 5,
                      "input_tokens_details": {"cached_tokens": 4}}}}),
    ];
    let mut out = Vec::new();
    for e in &evs {
        s.event(e, &mut out);
    }
    s.finish(&mut out);
    let names: Vec<String> = out
        .iter()
        .map(|b| {
            let t = std::str::from_utf8(b).unwrap();
            t.lines().next().unwrap().trim_start_matches("event: ").to_owned()
        })
        .collect();
    assert_eq!(
        names,
        [
            "message_start",
            "ping",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
    let md = std::str::from_utf8(&out[8]).unwrap();
    assert!(md.contains("\"stop_reason\":\"tool_use\""));
    assert!(md.contains("\"input_tokens\":6"));
    assert!(md.contains("\"cache_read_input_tokens\":4"));
}

#[test]
fn data_path_matches_value_path() {
    let evs = [
        r#"{"type":"response.created","response":{"id":"resp_abc"}}"#,
        r#"{"type":"response.output_item.added","item":{"type":"reasoning"}}"#,
        r#"{"type":"response.reasoning_summary_text.delta","delta":"hm \"q\""}"#,
        r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"sig"}}"#,
        r#"{"type":"response.output_item.added","item":{"type":"message"}}"#,
        r#"{"type":"response.content_part.added","part":{"type":"output_text"}}"#,
        r#"{"type":"response.output_text.delta","delta":"Hi\né 😀"}"#,
        r#"{"type":"response.output_text.delta","delta":""}"#,
        r#"{"type":"response.refusal.delta","delta":"no"}"#,
        r#"{"type":"response.output_text.done","text":"Hi"}"#,
        r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"c","name":"f"}}"#,
        r#"{"type":"response.function_call_arguments.delta","delta":"{\"x\":1}"}"#,
        r#"{"type":"response.function_call_arguments.delta","delta":5}"#,
        r#"{"type":"response.output_item.done","item":{"type":"function_call","arguments":"{}"}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_abc","output":[],"usage":{"input_tokens":3}}}"#,
    ];
    let opts = || Opts {
        model: "claude-x".into(),
        thinking: true,
    };
    let (mut a, mut b) = (Stream::new(opts()), Stream::new(opts()));
    let (mut x, mut y) = (Vec::new(), Vec::new());
    for e in evs {
        a.event_data(e, &mut x);
        b.event(&serde_json::from_str(e).unwrap(), &mut y);
    }
    assert_eq!(x, y);
    assert_eq!(x.len(), 15);
}
