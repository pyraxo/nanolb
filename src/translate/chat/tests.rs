use super::*;

#[test]
fn request_maps_to_responses() {
    let req = json!({
        "model":"openai/gpt-5:online",
        "messages":[
            {"role":"system","content":"be brief"},
            {"role":"user","content":"weather?"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"wx","arguments":"{\"c\":\"SF\"}"}}]},
            {"role":"tool","tool_call_id":"call_1","content":"sunny"}
        ],
        "tools":[{"type":"function","function":{"name":"wx","description":"d","parameters":{"type":"object"}}}],
        "tool_choice":{"type":"function","function":{"name":"wx"}},
        "reasoning_effort":"low"
    });
    let (r, opts) = to_responses(&req, true).unwrap();
    assert_eq!(r["model"], "gpt-5");
    assert_eq!(r["instructions"], "be brief");
    assert_eq!(r["input"][0]["content"][0], json!({"type":"input_text","text":"weather?"}));
    assert_eq!(
        r["input"][1],
        json!({"type":"function_call","call_id":"call_1","name":"wx","arguments":"{\"c\":\"SF\"}"})
    );
    assert_eq!(
        r["input"][2],
        json!({"type":"function_call_output","call_id":"call_1","output":"sunny"})
    );
    assert_eq!(r["input"].as_array().unwrap().len(), 3);
    assert_eq!(
        r["tools"][0],
        json!({"type":"function","name":"wx","description":"d","parameters":{"type":"object"}})
    );
    assert_eq!(r["tool_choice"], json!({"type":"function","name":"wx"}));
    assert_eq!(r["reasoning"], json!({"effort":"low","summary":"auto"}));
    assert_eq!(opts.model, "openai/gpt-5:online");
    assert!(opts.include_reasoning);
}

#[test]
fn stream_text_then_tool_call() {
    let mut s = Stream::new(Opts {
        openrouter: false,
        include_usage: true,
        include_reasoning: false,
        model: "gpt-5".into(),
        rates: None,
    });
    let mut out = Vec::new();
    let evs = [
        json!({"type":"response.created","response":{"id":"resp_abc","created_at":1_700_000_000}}),
        json!({"type":"response.output_text.delta","delta":"Hi"}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_9","name":"wx"}}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{}"}),
        json!({"type":"response.completed","response":{"output":[{"type":"function_call"}],"usage":{"input_tokens":5,"output_tokens":7,"total_tokens":12,"input_tokens_details":{"cached_tokens":2},"output_tokens_details":{"reasoning_tokens":3}}}}),
    ];
    for e in &evs {
        s.event(e, &mut out);
    }
    s.finish(&mut out);
    let frames: Vec<String> = out.iter().map(|b| String::from_utf8(b.to_vec()).unwrap()).collect();
    let parse = |i: usize| -> Value { serde_json::from_str(frames[i].strip_prefix("data: ").unwrap().trim()).unwrap() };
    assert_eq!(parse(0)["id"], "chatcmpl-abc");
    assert_eq!(parse(0)["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(parse(1)["choices"][0]["delta"]["content"], "Hi");
    let start = parse(2);
    assert_eq!(start["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
    assert_eq!(start["choices"][0]["delta"]["tool_calls"][0]["id"], "call_9");
    assert_eq!(parse(3)["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{}");
    assert_eq!(parse(4)["choices"][0]["finish_reason"], "tool_calls");
    let u = parse(5);
    assert_eq!(u["choices"], json!([]));
    assert_eq!(
        u["usage"],
        json!({"prompt_tokens":5,"completion_tokens":7,"total_tokens":12,
            "prompt_tokens_details":{"cached_tokens":2},
            "completion_tokens_details":{"reasoning_tokens":3}})
    );
    assert_eq!(frames[6], "data: [DONE]\n\n");
    assert_eq!(frames.len(), 7);
}
