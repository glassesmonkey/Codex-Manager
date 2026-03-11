use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::json_conversion::extract_function_call_arguments_raw;

#[derive(Debug, Clone, Default)]
struct StreamingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

#[derive(Debug, Clone, Default)]
struct StreamingResponsesState {
    response_id: Option<String>,
    model: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    total_tokens: Option<i64>,
    content_text: String,
    tool_calls: BTreeMap<usize, StreamingToolCall>,
    completed_response: Option<Value>,
    error_payload: Option<Value>,
}

pub(super) fn convert_responses_json_to_chat_json(
    body: &[u8],
) -> Result<(Vec<u8>, &'static str), String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| "invalid upstream json response".to_string())?;
    let payload = build_chat_completion_from_responses(&value)?;
    serde_json::to_vec(&payload)
        .map(|bytes| (bytes, "application/json"))
        .map_err(|err| format!("serialize openai chat response failed: {err}"))
}

pub(super) fn convert_responses_sse_to_chat_json(
    body: &[u8],
) -> Result<(Vec<u8>, &'static str), String> {
    let text = std::str::from_utf8(body).map_err(|_| "invalid upstream sse bytes".to_string())?;
    let payload = build_chat_completion_from_responses_stream(text)?;
    serde_json::to_vec(&payload)
        .map(|bytes| (bytes, "application/json"))
        .map_err(|err| format!("serialize openai chat response failed: {err}"))
}

pub(super) fn convert_responses_json_to_chat_sse(
    body: &[u8],
) -> Result<(Vec<u8>, &'static str), String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| "invalid upstream json response".to_string())?;
    let payload = build_chat_completion_from_responses(&value)?;
    if payload.get("error").is_some() {
        return serde_json::to_vec(&payload)
            .map(|bytes| (bytes, "application/json"))
            .map_err(|err| format!("serialize openai error response failed: {err}"));
    }
    build_chat_sse_from_chat_completion(&payload)
}

pub(super) fn convert_responses_sse_to_chat_sse(
    body: &[u8],
) -> Result<(Vec<u8>, &'static str), String> {
    let (json_body, content_type) = convert_responses_sse_to_chat_json(body)?;
    if content_type != "application/json" {
        return Ok((json_body, content_type));
    }
    let payload: Value = serde_json::from_slice(&json_body)
        .map_err(|_| "invalid synthesized openai chat json response".to_string())?;
    if payload.get("error").is_some() {
        return Ok((json_body, "application/json"));
    }
    build_chat_sse_from_chat_completion(&payload)
}

pub(super) fn build_chat_completion_from_responses(value: &Value) -> Result<Value, String> {
    let source = value.get("response").unwrap_or(value);
    if let Some(error_payload) = map_responses_error_to_openai(source) {
        return Ok(error_payload);
    }
    let model = source
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let id = source
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("chatcmpl_codexmanager");
    let (text_content, tool_calls) = extract_response_message_content(source)?;
    let finish_reason = infer_finish_reason(source, !tool_calls.is_empty());
    let mut message = serde_json::Map::new();
    message.insert("role".to_string(), Value::String("assistant".to_string()));
    message.insert("content".to_string(), Value::String(text_content));
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }

    let usage = build_openai_usage(source);
    Ok(json!({
        "id": id,
        "object": "chat.completion",
        "created": current_unix_seconds(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason
        }],
        "usage": usage
    }))
}

fn build_chat_completion_from_responses_stream(text: &str) -> Result<Value, String> {
    let mut state = StreamingResponsesState::default();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if !line.starts_with("data:") {
            continue;
        }
        let payload = line.trim_start_matches("data:").trim();
        if payload == "[DONE]" {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        state.capture_meta(&value);
        if let Some(error_payload) = map_responses_error_to_openai(&value) {
            state.error_payload = Some(error_payload);
            break;
        }
        let Some(event_type) = value.get("type").and_then(Value::as_str) else {
            continue;
        };
        match event_type {
            "response.output_text.delta" => {
                if let Some(fragment) = value.get("delta").and_then(Value::as_str) {
                    state.content_text.push_str(fragment);
                }
            }
            "response.output_item.done" => state.capture_function_call(&value),
            "response.completed" => {
                if let Some(response) = value.get("response") {
                    state.completed_response = Some(response.clone());
                }
            }
            _ => {}
        }
    }

    if let Some(error_payload) = state.error_payload {
        return Ok(error_payload);
    }
    if let Some(mut completed_response) = state.completed_response.clone() {
        merge_stream_accumulator_into_response(&mut completed_response, &state)?;
        return build_chat_completion_from_responses(&completed_response);
    }
    let synthesized = synthesize_responses_payload_from_stream(&state)?;
    build_chat_completion_from_responses(&synthesized)
}

fn build_chat_sse_from_chat_completion(payload: &Value) -> Result<(Vec<u8>, &'static str), String> {
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("chatcmpl_codexmanager");
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let created = payload
        .get("created")
        .and_then(Value::as_i64)
        .unwrap_or_else(current_unix_seconds);
    let choice = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| "missing openai chat choice".to_string())?;
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| "missing openai chat message".to_string())?;
    let finish_reason = choice
        .get("finish_reason")
        .cloned()
        .unwrap_or(Value::String("stop".to_string()));
    let usage = payload.get("usage").cloned().unwrap_or_else(|| json!({}));

    let mut out = String::new();
    append_openai_sse_chunk(
        &mut out,
        &json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": { "role": "assistant" },
                "finish_reason": Value::Null
            }]
        }),
    );

    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            append_openai_sse_chunk(
                &mut out,
                &json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "delta": { "content": text },
                        "finish_reason": Value::Null
                    }]
                }),
            );
        }
    }

    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (index, tool_call) in tool_calls.iter().enumerate() {
            append_openai_sse_chunk(
                &mut out,
                &json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [{
                                "index": index,
                                "id": tool_call.get("id").cloned().unwrap_or(Value::String(format!("call_{index}"))),
                                "type": "function",
                                "function": tool_call.get("function").cloned().unwrap_or_else(|| json!({}))
                            }]
                        },
                        "finish_reason": Value::Null
                    }]
                }),
            );
        }
    }

    append_openai_sse_chunk(
        &mut out,
        &json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": finish_reason
            }],
            "usage": usage
        }),
    );
    out.push_str("data: [DONE]\n\n");
    Ok((out.into_bytes(), "text/event-stream"))
}

fn map_responses_error_to_openai(value: &Value) -> Option<Value> {
    let error_value = if let Some(error) = value.get("error") {
        Some(error)
    } else if value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("error"))
    {
        value.get("error").or_else(|| Some(value))
    } else {
        value
            .pointer("/response/error")
            .or_else(|| value.pointer("/response/status_details/error"))
    }?;
    let error = error_value.as_object()?;
    let message = error
        .get("message")
        .or_else(|| error.get("error"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("upstream request failed");
    let error_type = error
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("api_error");
    let mut mapped_error = serde_json::Map::new();
    mapped_error.insert("message".to_string(), Value::String(message.to_string()));
    mapped_error.insert("type".to_string(), Value::String(error_type.to_string()));
    if let Some(code) = error
        .get("code")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        mapped_error.insert("code".to_string(), Value::String(code.to_string()));
    }
    if let Some(param) = error
        .get("param")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        mapped_error.insert("param".to_string(), Value::String(param.to_string()));
    }
    Some(json!({
        "error": Value::Object(mapped_error)
    }))
}

fn extract_response_message_content(value: &Value) -> Result<(String, Vec<Value>), String> {
    let mut text_from_output_items = String::new();
    let mut saw_output_text = false;
    let mut tool_calls = Vec::new();

    if let Some(output_items) = value.get("output").and_then(Value::as_array) {
        for output_item in output_items {
            let Some(item_obj) = output_item.as_object() else {
                continue;
            };
            let item_type = item_obj
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match item_type {
                "message" => {
                    if let Some(content) = item_obj.get("content").and_then(Value::as_array) {
                        for block in content {
                            let Some(block_obj) = block.as_object() else {
                                continue;
                            };
                            let block_type = block_obj
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            if (block_type == "output_text" || block_type == "text")
                                && block_obj.get("text").and_then(Value::as_str).is_some()
                            {
                                saw_output_text = true;
                                text_from_output_items.push_str(
                                    block_obj.get("text").and_then(Value::as_str).unwrap_or(""),
                                );
                            }
                        }
                    }
                }
                "function_call" => {
                    tool_calls.push(build_openai_tool_call(item_obj, tool_calls.len())?);
                }
                _ => {}
            }
        }
    }

    let text_content = if saw_output_text {
        text_from_output_items
    } else {
        value
            .get("output_text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    Ok((text_content, tool_calls))
}

fn build_openai_tool_call(
    item_obj: &serde_json::Map<String, Value>,
    index: usize,
) -> Result<Value, String> {
    let tool_use_id = item_obj
        .get("call_id")
        .or_else(|| item_obj.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("call_{index}"));
    let function_name = item_obj
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "missing responses function_call name".to_string())?;
    let arguments =
        extract_function_call_arguments_raw(item_obj).unwrap_or_else(|| "{}".to_string());
    Ok(json!({
        "id": tool_use_id,
        "type": "function",
        "function": {
            "name": function_name,
            "arguments": arguments
        }
    }))
}

fn build_openai_usage(value: &Value) -> Value {
    let prompt_tokens = value
        .get("usage")
        .and_then(|usage| usage.get("input_tokens"))
        .or_else(|| {
            value
                .get("usage")
                .and_then(|usage| usage.get("prompt_tokens"))
        })
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let completion_tokens = value
        .get("usage")
        .and_then(|usage| usage.get("output_tokens"))
        .or_else(|| {
            value
                .get("usage")
                .and_then(|usage| usage.get("completion_tokens"))
        })
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let total_tokens = value
        .get("usage")
        .and_then(|usage| usage.get("total_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(prompt_tokens.saturating_add(completion_tokens));
    json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": total_tokens
    })
}

fn infer_finish_reason(value: &Value, has_tool_calls: bool) -> &'static str {
    if has_tool_calls {
        return "tool_calls";
    }
    let incomplete_reason = value
        .pointer("/incomplete_details/reason")
        .or_else(|| value.pointer("/status_details/reason"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if incomplete_reason.contains("max_output_tokens") || incomplete_reason.contains("length") {
        return "length";
    }
    "stop"
}

fn current_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn append_openai_sse_chunk(buffer: &mut String, payload: &Value) {
    let data = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    buffer.push_str("data: ");
    buffer.push_str(&data);
    buffer.push_str("\n\n");
}

impl StreamingResponsesState {
    fn capture_meta(&mut self, value: &Value) {
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            self.response_id = Some(id.to_string());
        }
        if let Some(model) = value.get("model").and_then(Value::as_str) {
            self.model = Some(model.to_string());
        }
        let usage_source = value
            .get("response")
            .unwrap_or(value)
            .get("usage")
            .and_then(Value::as_object);
        if let Some(usage) = usage_source {
            self.input_tokens = usage
                .get("input_tokens")
                .or_else(|| usage.get("prompt_tokens"))
                .and_then(Value::as_i64)
                .or(self.input_tokens);
            self.output_tokens = usage
                .get("output_tokens")
                .or_else(|| usage.get("completion_tokens"))
                .and_then(Value::as_i64)
                .or(self.output_tokens);
            self.total_tokens = usage
                .get("total_tokens")
                .and_then(Value::as_i64)
                .or(self.total_tokens);
        }
        if let Some(response) = value.get("response").and_then(Value::as_object) {
            if let Some(id) = response.get("id").and_then(Value::as_str) {
                self.response_id = Some(id.to_string());
            }
            if let Some(model) = response.get("model").and_then(Value::as_str) {
                self.model = Some(model.to_string());
            }
        }
    }

    fn capture_function_call(&mut self, value: &Value) {
        let Some(item_obj) = value.get("item").and_then(Value::as_object) else {
            return;
        };
        if item_obj
            .get("type")
            .and_then(Value::as_str)
            .is_none_or(|kind| kind != "function_call")
        {
            return;
        }
        let index = value
            .get("output_index")
            .or_else(|| item_obj.get("index"))
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(self.tool_calls.len());
        let entry = self.tool_calls.entry(index).or_default();
        if let Some(id) = item_obj
            .get("call_id")
            .or_else(|| item_obj.get("id"))
            .and_then(Value::as_str)
        {
            entry.id = Some(id.to_string());
        }
        if let Some(name) = item_obj.get("name").and_then(Value::as_str) {
            entry.name = Some(name.to_string());
        }
        if let Some(arguments) = extract_function_call_arguments_raw(item_obj) {
            entry.arguments = arguments;
        }
    }
}

fn merge_stream_accumulator_into_response(
    response: &mut Value,
    state: &StreamingResponsesState,
) -> Result<(), String> {
    let Some(response_obj) = response.as_object_mut() else {
        return Err("invalid response.completed payload".to_string());
    };
    if response_obj
        .get("output_text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .is_empty()
        && !state.content_text.trim().is_empty()
    {
        response_obj.insert(
            "output_text".to_string(),
            Value::String(state.content_text.clone()),
        );
    }

    let mut output_items = response_obj
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut existing_call_ids = BTreeSet::new();
    for item in &output_items {
        if let Some(call_id) = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
        {
            existing_call_ids.insert(call_id.to_string());
        }
    }
    for (index, tool_call) in &state.tool_calls {
        let call_id = tool_call
            .id
            .clone()
            .unwrap_or_else(|| format!("call_{index}"));
        if existing_call_ids.contains(&call_id) {
            continue;
        }
        let name = tool_call.name.clone().unwrap_or_else(|| "tool".to_string());
        output_items.push(json!({
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": tool_call.arguments
        }));
    }
    response_obj.insert("output".to_string(), Value::Array(output_items));
    Ok(())
}

fn synthesize_responses_payload_from_stream(
    state: &StreamingResponsesState,
) -> Result<Value, String> {
    let mut output = Vec::new();
    if !state.content_text.is_empty() {
        output.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": state.content_text
            }]
        }));
    }
    for (index, tool_call) in &state.tool_calls {
        let name = tool_call.name.clone().unwrap_or_else(|| "tool".to_string());
        output.push(json!({
            "type": "function_call",
            "call_id": tool_call.id.clone().unwrap_or_else(|| format!("call_{index}")),
            "name": name,
            "arguments": tool_call.arguments
        }));
    }
    Ok(json!({
        "id": state.response_id.clone().unwrap_or_else(|| "resp_codexmanager".to_string()),
        "model": state.model.clone().unwrap_or_else(|| "unknown".to_string()),
        "output_text": state.content_text,
        "output": output,
        "usage": {
            "input_tokens": state.input_tokens.unwrap_or(0),
            "output_tokens": state.output_tokens.unwrap_or(0),
            "total_tokens": state
                .total_tokens
                .unwrap_or_else(|| state.input_tokens.unwrap_or(0).saturating_add(state.output_tokens.unwrap_or(0)))
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        build_chat_completion_from_responses, convert_responses_json_to_chat_json,
        convert_responses_sse_to_chat_json, convert_responses_sse_to_chat_sse,
    };
    use serde_json::json;

    #[test]
    fn responses_json_maps_to_chat_completion() {
        let source = json!({
            "id": "resp_1",
            "model": "gpt-5.3-codex",
            "output_text": "hello",
            "usage": {
                "input_tokens": 7,
                "output_tokens": 3,
                "total_tokens": 10
            }
        });
        let payload = build_chat_completion_from_responses(&source).expect("convert");
        assert_eq!(payload["object"], "chat.completion");
        assert_eq!(payload["choices"][0]["message"]["content"], "hello");
        assert_eq!(payload["choices"][0]["finish_reason"], "stop");
        assert_eq!(payload["usage"]["prompt_tokens"], 7);
        assert_eq!(payload["usage"]["completion_tokens"], 3);
    }

    #[test]
    fn responses_json_maps_function_calls() {
        let source = json!({
            "id": "resp_2",
            "model": "gpt-5.3-codex",
            "output": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "read_file",
                "input": { "path": "README.md" }
            }]
        });
        let payload = build_chat_completion_from_responses(&source).expect("convert");
        assert_eq!(payload["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            payload["choices"][0]["message"]["tool_calls"][0]["id"],
            "call_1"
        );
        assert_eq!(
            payload["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"README.md\"}"
        );
    }

    #[test]
    fn responses_sse_maps_to_chat_completion_json() {
        let source = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.3-codex\",\"usage\":{\"input_tokens\":4,\"output_tokens\":2,\"total_tokens\":6}}}\n\n",
            "data: [DONE]\n\n"
        );
        let (body, content_type) =
            convert_responses_sse_to_chat_json(source.as_bytes()).expect("convert");
        assert_eq!(content_type, "application/json");
        let payload: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(payload["choices"][0]["message"]["content"], "hello");
    }

    #[test]
    fn responses_sse_maps_to_chat_completion_sse() {
        let source = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.3-codex\",\"usage\":{\"input_tokens\":4,\"output_tokens\":2,\"total_tokens\":6}}}\n\n",
            "data: [DONE]\n\n"
        );
        let (body, content_type) =
            convert_responses_sse_to_chat_sse(source.as_bytes()).expect("convert");
        assert_eq!(content_type, "text/event-stream");
        let text = String::from_utf8(body).expect("utf8");
        assert!(text.contains("\"object\":\"chat.completion.chunk\""));
        assert!(text.contains("\"content\":\"hello\""));
        assert!(text.contains("data: [DONE]"));
    }

    #[test]
    fn responses_json_error_maps_to_openai_error() {
        let source = json!({
            "error": {
                "message": "Model not found",
                "type": "invalid_request_error",
                "code": "model_not_found"
            }
        });
        let (body, content_type) =
            convert_responses_json_to_chat_json(&serde_json::to_vec(&source).expect("serialize"))
                .expect("convert");
        assert_eq!(content_type, "application/json");
        let payload: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(payload["error"]["message"], "Model not found");
        assert_eq!(payload["error"]["code"], "model_not_found");
    }
}
