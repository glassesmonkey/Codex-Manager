use serde_json::{json, Value};

use super::prompt_cache;

const DEFAULT_ANTHROPIC_MODEL: &str = "gpt-5.3-codex";
const DEFAULT_ANTHROPIC_REASONING: &str = "high";
const DEFAULT_ANTHROPIC_INSTRUCTIONS: &str =
    "You are Codex, a coding assistant that responds clearly and safely.";
const MAX_ANTHROPIC_TOOLS: usize = 16;
const OPENAI_UNSUPPORTED_CHAT_FIELDS: [&str; 13] = [
    "audio",
    "modalities",
    "logprobs",
    "top_logprobs",
    "n",
    "temperature",
    "top_p",
    "presence_penalty",
    "frequency_penalty",
    "seed",
    "logit_bias",
    "response_format",
    "stop",
];

pub(super) fn convert_openai_chat_completions_request(
    body: &[u8],
) -> Result<(Vec<u8>, bool), String> {
    let payload: Value =
        serde_json::from_slice(body).map_err(|_| "invalid openai chat request json".to_string())?;
    let Some(obj) = payload.as_object() else {
        return Err("openai chat request body must be an object".to_string());
    };
    reject_unsupported_openai_chat_fields(obj)?;

    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "openai chat request model is required".to_string())?;
    let source_messages = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "openai chat request messages field is required".to_string())?;

    let mut instructions_parts = Vec::new();
    let mut input_items = Vec::new();
    for message in source_messages {
        let Some(message_obj) = message.as_object() else {
            return Err("invalid openai chat message item".to_string());
        };
        let role = message_obj
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| "openai chat message role is required".to_string())?;
        let content = message_obj.get("content").unwrap_or(&Value::Null);
        match role {
            "system" | "developer" => {
                let text = extract_openai_instruction_text(content)?;
                if !text.trim().is_empty() {
                    instructions_parts.push(text);
                }
            }
            "user" => append_openai_user_message(&mut input_items, content)?,
            "assistant" => append_openai_assistant_message(&mut input_items, message_obj, content)?,
            "tool" => append_openai_tool_message(&mut input_items, message_obj, content)?,
            other => return Err(format!("unsupported openai chat message role: {other}")),
        }
    }

    let mut out = serde_json::Map::new();
    out.insert("model".to_string(), Value::String(model.to_string()));
    let instructions = instructions_parts.join("\n\n");
    out.insert("instructions".to_string(), Value::String(instructions));
    out.insert(
        "text".to_string(),
        json!({
            "format": {
                "type": "text",
            }
        }),
    );
    if let Some(reasoning) = map_openai_reasoning(obj) {
        out.insert("reasoning".to_string(), reasoning);
    }
    if let Some(max_output_tokens) = obj
        .get("max_completion_tokens")
        .or_else(|| obj.get("max_tokens"))
        .and_then(Value::as_i64)
    {
        if max_output_tokens > 0 {
            out.insert(
                "max_output_tokens".to_string(),
                Value::Number(max_output_tokens.into()),
            );
        }
    }
    out.insert("input".to_string(), Value::Array(input_items));

    if let Some(tools) = obj.get("tools") {
        let mapped_tools = map_openai_tools(tools)?;
        if !mapped_tools.is_empty() {
            out.insert("tools".to_string(), Value::Array(mapped_tools));
        }
    }
    if let Some(tool_choice) = obj.get("tool_choice") {
        if !tool_choice.is_null() {
            if let Some(mapped_tool_choice) = map_openai_tool_choice(tool_choice)? {
                out.insert("tool_choice".to_string(), mapped_tool_choice);
            }
        }
    }
    if let Some(parallel_tool_calls) = obj.get("parallel_tool_calls").and_then(Value::as_bool) {
        out.insert(
            "parallel_tool_calls".to_string(),
            Value::Bool(parallel_tool_calls),
        );
    } else if out
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty())
    {
        out.insert("parallel_tool_calls".to_string(), Value::Bool(true));
    }

    let request_stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);
    out.insert("stream".to_string(), Value::Bool(true));
    out.insert("store".to_string(), Value::Bool(false));
    out.insert(
        "include".to_string(),
        Value::Array(vec![Value::String(
            "reasoning.encrypted_content".to_string(),
        )]),
    );

    serde_json::to_vec(&Value::Object(out))
        .map(|bytes| (bytes, request_stream))
        .map_err(|err| format!("convert openai chat request failed: {err}"))
}

fn reject_unsupported_openai_chat_fields(
    obj: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    for field in OPENAI_UNSUPPORTED_CHAT_FIELDS {
        if obj.contains_key(field) {
            return Err(format!(
                "unsupported openai chat field for codex compatibility: {field}"
            ));
        }
    }
    Ok(())
}

fn map_openai_reasoning(obj: &serde_json::Map<String, Value>) -> Option<Value> {
    obj.get("reasoning")
        .and_then(Value::as_object)
        .and_then(|value| value.get("effort"))
        .and_then(Value::as_str)
        .and_then(crate::reasoning_effort::normalize_reasoning_effort)
        .map(|value| {
            json!({
                "effort": value,
            })
        })
        .or_else(|| {
            obj.get("reasoning_effort")
                .and_then(Value::as_str)
                .and_then(crate::reasoning_effort::normalize_reasoning_effort)
                .map(|value| {
                    json!({
                        "effort": value,
                    })
                })
        })
}

fn extract_openai_instruction_text(value: &Value) -> Result<String, String> {
    if let Some(text) = value.as_str() {
        return Ok(text.to_string());
    }
    let Some(items) = value.as_array() else {
        if value.is_null() {
            return Ok(String::new());
        }
        return Err("unsupported openai instruction content".to_string());
    };
    let mut parts = Vec::new();
    for item in items {
        let Some(item_obj) = item.as_object() else {
            return Err("invalid openai instruction content item".to_string());
        };
        match item_obj
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" => {
                let text = item_obj
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "openai text content missing text".to_string())?;
                parts.push(text.to_string());
            }
            other => {
                return Err(format!(
                    "unsupported openai instruction content block type: {other}"
                ))
            }
        }
    }
    Ok(parts.join(""))
}

fn append_openai_user_message(input_items: &mut Vec<Value>, content: &Value) -> Result<(), String> {
    if let Some(text) = content.as_str() {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            input_items.push(json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": trimmed }]
            }));
        }
        return Ok(());
    }
    let Some(items) = content.as_array() else {
        if content.is_null() {
            return Ok(());
        }
        return Err("unsupported openai user content".to_string());
    };
    let mut message_content = Vec::new();
    for item in items {
        let Some(item_obj) = item.as_object() else {
            return Err("invalid openai user content item".to_string());
        };
        match item_obj
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" => {
                let text = item_obj
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                if let Some(text) = text {
                    message_content.push(json!({
                        "type": "input_text",
                        "text": text
                    }));
                }
            }
            "image_url" => {
                let image_obj = item_obj
                    .get("image_url")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        "openai image_url content missing image_url object".to_string()
                    })?;
                let url = image_obj
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "openai image_url content missing url".to_string())?;
                let mut mapped = serde_json::Map::new();
                mapped.insert("type".to_string(), Value::String("input_image".to_string()));
                mapped.insert("image_url".to_string(), Value::String(url.to_string()));
                if let Some(detail) = image_obj
                    .get("detail")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    mapped.insert("detail".to_string(), Value::String(detail.to_string()));
                }
                message_content.push(Value::Object(mapped));
            }
            other => {
                return Err(format!(
                    "unsupported openai user content block type: {other}"
                ))
            }
        }
    }
    if !message_content.is_empty() {
        input_items.push(json!({
            "type": "message",
            "role": "user",
            "content": message_content
        }));
    }
    Ok(())
}

fn append_openai_assistant_message(
    input_items: &mut Vec<Value>,
    message_obj: &serde_json::Map<String, Value>,
    content: &Value,
) -> Result<(), String> {
    let text = extract_openai_assistant_text_content(content)?;
    if !text.trim().is_empty() {
        input_items.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text.trim() }]
        }));
    }

    let tool_calls = if let Some(array) = message_obj.get("tool_calls").and_then(Value::as_array) {
        array.clone()
    } else if let Some(function_call) = message_obj.get("function_call").and_then(Value::as_object)
    {
        vec![json!({
            "id": message_obj
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("call_0"),
            "type": "function",
            "function": function_call
        })]
    } else {
        Vec::new()
    };

    for (index, tool_call) in tool_calls.iter().enumerate() {
        let Some(tool_obj) = tool_call.as_object() else {
            return Err("invalid openai tool call item".to_string());
        };
        let call_id = tool_obj
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("call_{index}"));
        let Some(function_name) = tool_obj
            .get("function")
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Err("openai tool call missing function name".to_string());
        };
        let arguments = tool_obj
            .get("function")
            .and_then(|value| value.get("arguments"))
            .map(|value| {
                if let Some(text) = value.as_str() {
                    text.to_string()
                } else {
                    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
                }
            })
            .unwrap_or_else(|| "{}".to_string());
        input_items.push(json!({
            "type": "function_call",
            "call_id": call_id,
            "name": function_name,
            "arguments": arguments
        }));
    }
    Ok(())
}

fn extract_openai_assistant_text_content(value: &Value) -> Result<String, String> {
    if value.is_null() {
        return Ok(String::new());
    }
    if let Some(text) = value.as_str() {
        return Ok(text.to_string());
    }
    let Some(items) = value.as_array() else {
        return Err("unsupported openai assistant content".to_string());
    };
    let mut parts = Vec::new();
    for item in items {
        let Some(item_obj) = item.as_object() else {
            return Err("invalid openai assistant content item".to_string());
        };
        match item_obj
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" => {
                let text = item_obj
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "openai assistant text content missing text".to_string())?;
                parts.push(text.to_string());
            }
            other => {
                return Err(format!(
                    "unsupported openai assistant content block type: {other}"
                ))
            }
        }
    }
    Ok(parts.join(""))
}

fn append_openai_tool_message(
    input_items: &mut Vec<Value>,
    message_obj: &serde_json::Map<String, Value>,
    content: &Value,
) -> Result<(), String> {
    let tool_call_id = message_obj
        .get("tool_call_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "openai tool message missing tool_call_id".to_string())?;
    let output = extract_tool_result_content(Some(content))?;
    input_items.push(json!({
        "type": "function_call_output",
        "call_id": tool_call_id,
        "output": output
    }));
    Ok(())
}

fn map_openai_tools(value: &Value) -> Result<Vec<Value>, String> {
    let Some(items) = value.as_array() else {
        return Err("openai tools field must be an array".to_string());
    };
    let mut mapped = Vec::new();
    for item in items.iter().take(MAX_ANTHROPIC_TOOLS) {
        let Some(item_obj) = item.as_object() else {
            return Err("invalid openai tool item".to_string());
        };
        let tool_type = item_obj
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("function");
        if tool_type != "function" {
            return Err(format!("unsupported openai tool type: {tool_type}"));
        }
        let function = item_obj
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(|| "openai function tool missing function object".to_string())?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "openai function tool missing name".to_string())?;
        let description = function
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let parameters = function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
        let mut tool_obj = serde_json::Map::new();
        tool_obj.insert("type".to_string(), Value::String("function".to_string()));
        tool_obj.insert("name".to_string(), Value::String(name.to_string()));
        if !description.is_empty() {
            tool_obj.insert("description".to_string(), Value::String(description));
        }
        tool_obj.insert("parameters".to_string(), parameters);
        mapped.push(Value::Object(tool_obj));
    }
    Ok(mapped)
}

fn map_openai_tool_choice(value: &Value) -> Result<Option<Value>, String> {
    if let Some(text) = value.as_str() {
        return Ok(Some(Value::String(text.to_string())));
    }
    let Some(obj) = value.as_object() else {
        return Err("openai tool_choice must be a string or object".to_string());
    };
    let choice_type = obj
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function");
    if choice_type != "function" {
        return Err(format!(
            "unsupported openai tool_choice type: {choice_type}"
        ));
    }
    let name = obj
        .get("function")
        .and_then(Value::as_object)
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "openai tool_choice missing function name".to_string())?;
    Ok(Some(json!({
        "type": "function",
        "name": name
    })))
}

pub(super) fn convert_anthropic_messages_request(body: &[u8]) -> Result<(Vec<u8>, bool), String> {
    let payload: Value =
        serde_json::from_slice(body).map_err(|_| "invalid claude request json".to_string())?;
    let Some(obj) = payload.as_object() else {
        return Err("claude request body must be an object".to_string());
    };

    let mut messages = Vec::new();

    if let Some(system) = obj.get("system") {
        let system_text = extract_text_content(system)?;
        if !system_text.trim().is_empty() {
            messages.push(json!({
                "role": "system",
                "content": system_text,
            }));
        }
    }

    let source_messages = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "claude messages field is required".to_string())?;
    for message in source_messages {
        let Some(message_obj) = message.as_object() else {
            return Err("invalid claude message item".to_string());
        };
        let role = message_obj
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| "claude message role is required".to_string())?;
        let content = message_obj
            .get("content")
            .ok_or_else(|| "claude message content is required".to_string())?;
        match role {
            "assistant" => append_assistant_messages(&mut messages, content)?,
            "user" => append_user_messages(&mut messages, content)?,
            "tool" => append_tool_role_message(&mut messages, message_obj, content)?,
            other => return Err(format!("unsupported claude message role: {other}")),
        }
    }

    let (instructions, input_items) = convert_chat_messages_to_responses_input(&messages)?;
    let mut out = serde_json::Map::new();
    let resolved_model = resolve_anthropic_upstream_model(obj);
    out.insert("model".to_string(), Value::String(resolved_model));
    let resolved_instructions = instructions
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_ANTHROPIC_INSTRUCTIONS);
    out.insert(
        "instructions".to_string(),
        Value::String(resolved_instructions.to_string()),
    );
    out.insert(
        "text".to_string(),
        json!({
            "format": {
                "type": "text",
            }
        }),
    );
    let resolved_reasoning = obj
        .get("reasoning")
        .and_then(Value::as_object)
        .and_then(|value| value.get("effort"))
        .and_then(Value::as_str)
        .and_then(crate::reasoning_effort::normalize_reasoning_effort)
        .unwrap_or(DEFAULT_ANTHROPIC_REASONING)
        .to_string();
    out.insert(
        "reasoning".to_string(),
        json!({
            "effort": resolved_reasoning,
        }),
    );
    out.insert("input".to_string(), Value::Array(input_items));

    // 中文注释：参考 CLIProxyAPI 的行为：Claude 入口需要一个稳定的 prompt_cache_key，
    // 并在上游请求头把 Session_id/Conversation_id 与之对齐，才能显著降低 challenge 命中率。
    if let Some(prompt_cache_key) = prompt_cache::resolve_prompt_cache_key(obj, out.get("model")) {
        out.insert(
            "prompt_cache_key".to_string(),
            Value::String(prompt_cache_key),
        );
    }
    // 中文注释：上游 codex responses 对低体积请求携带采样参数时更容易触发 challenge，
    // 这里对 anthropic 入口统一不透传 temperature/top_p，优先稳定性。
    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        let mapped_tools = tools
            .iter()
            .filter_map(map_anthropic_tool_definition)
            .take(MAX_ANTHROPIC_TOOLS)
            .collect::<Vec<_>>();
        if !mapped_tools.is_empty() {
            out.insert("tools".to_string(), Value::Array(mapped_tools));
            if !obj.contains_key("tool_choice") {
                out.insert("tool_choice".to_string(), Value::String("auto".to_string()));
            }
        }
    }
    if let Some(tool_choice) = obj.get("tool_choice") {
        if !tool_choice.is_null() {
            if let Some(mapped_tool_choice) = map_anthropic_tool_choice(tool_choice) {
                out.insert("tool_choice".to_string(), mapped_tool_choice);
            }
        }
    }
    let request_stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(true);
    // 说明：即使 Claude 请求 stream=false，也统一以 stream=true 请求 upstream，
    // 再在网关侧将 SSE 聚合为 Anthropic JSON，降低 upstream challenge 命中率。
    out.insert("stream".to_string(), Value::Bool(true));
    out.insert("parallel_tool_calls".to_string(), Value::Bool(true));
    out.insert("store".to_string(), Value::Bool(false));
    out.insert(
        "include".to_string(),
        Value::Array(vec![Value::String(
            "reasoning.encrypted_content".to_string(),
        )]),
    );

    serde_json::to_vec(&Value::Object(out))
        .map(|bytes| (bytes, request_stream))
        .map_err(|err| format!("convert claude request failed: {err}"))
}

fn resolve_anthropic_upstream_model(source: &serde_json::Map<String, Value>) -> String {
    let requested_model = source
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match requested_model {
        Some(model) if model.to_ascii_lowercase().contains("codex") => model.to_string(),
        _ => DEFAULT_ANTHROPIC_MODEL.to_string(),
    }
}

fn append_assistant_messages(messages: &mut Vec<Value>, content: &Value) -> Result<(), String> {
    if let Some(text) = content.as_str() {
        messages.push(json!({
            "role": "assistant",
            "content": text,
        }));
        return Ok(());
    }

    let blocks = if let Some(array) = content.as_array() {
        array.to_vec()
    } else if content.is_object() {
        vec![content.clone()]
    } else {
        return Err("unsupported assistant content".to_string());
    };

    let mut text_content = String::new();
    let mut tool_calls = Vec::new();

    for block in blocks {
        let Some(block_obj) = block.as_object() else {
            return Err("invalid assistant content block".to_string());
        };
        let block_type = block_obj
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| "assistant content block missing type".to_string())?;
        match block_type {
            "text" => {
                if let Some(text) = block_obj.get("text").and_then(Value::as_str) {
                    text_content.push_str(text);
                }
            }
            "tool_use" => {
                let id = block_obj
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("toolu_{}", tool_calls.len()));
                let Some(name) = block_obj
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                else {
                    continue;
                };
                let input = block_obj.get("input").cloned().unwrap_or_else(|| json!({}));
                let arguments = serde_json::to_string(&input)
                    .map_err(|err| format!("serialize tool_use input failed: {err}"))?;
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments,
                    }
                }));
            }
            _ => continue,
        }
    }

    let mut message_obj = serde_json::Map::new();
    message_obj.insert("role".to_string(), Value::String("assistant".to_string()));
    message_obj.insert("content".to_string(), Value::String(text_content));
    if !tool_calls.is_empty() {
        message_obj.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    messages.push(Value::Object(message_obj));
    Ok(())
}

fn append_user_messages(messages: &mut Vec<Value>, content: &Value) -> Result<(), String> {
    if let Some(text) = content.as_str() {
        if !text.trim().is_empty() {
            messages.push(json!({
                "role": "user",
                "content": text,
            }));
        }
        return Ok(());
    }

    let blocks = if let Some(array) = content.as_array() {
        array.to_vec()
    } else if content.is_object() {
        vec![content.clone()]
    } else {
        return Err("unsupported user content".to_string());
    };

    let mut pending_text = String::new();
    for block in blocks {
        let Some(block_obj) = block.as_object() else {
            return Err("invalid user content block".to_string());
        };
        let block_type = block_obj
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| "user content block missing type".to_string())?;
        match block_type {
            "text" => {
                if let Some(text) = block_obj.get("text").and_then(Value::as_str) {
                    pending_text.push_str(text);
                }
            }
            "tool_result" => {
                flush_user_text(messages, &mut pending_text);
                let tool_use_id = block_obj
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .or_else(|| block_obj.get("id").and_then(Value::as_str))
                    .unwrap_or_default();
                if tool_use_id.is_empty() {
                    continue;
                }
                let mut tool_content = extract_tool_result_content(block_obj.get("content"))?;
                if block_obj
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    tool_content = format!("[tool_error] {tool_content}");
                }
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": tool_content,
                }));
            }
            _ => continue,
        }
    }
    flush_user_text(messages, &mut pending_text);
    Ok(())
}

fn append_tool_role_message(
    messages: &mut Vec<Value>,
    message_obj: &serde_json::Map<String, Value>,
    content: &Value,
) -> Result<(), String> {
    let tool_call_id = message_obj
        .get("tool_call_id")
        .or_else(|| message_obj.get("tool_use_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| "tool role message missing tool_call_id".to_string())?;
    let tool_content = extract_tool_result_content(Some(content))?;
    messages.push(json!({
        "role": "tool",
        "tool_call_id": tool_call_id,
        "content": tool_content,
    }));
    Ok(())
}

fn flush_user_text(messages: &mut Vec<Value>, pending_text: &mut String) {
    if pending_text.trim().is_empty() {
        pending_text.clear();
        return;
    }
    messages.push(json!({
        "role": "user",
        "content": pending_text.clone(),
    }));
    pending_text.clear();
}

fn convert_chat_messages_to_responses_input(
    messages: &[Value],
) -> Result<(Option<String>, Vec<Value>), String> {
    let mut instructions_parts = Vec::new();
    let mut input_items = Vec::new();

    for message in messages {
        let Some(message_obj) = message.as_object() else {
            continue;
        };
        let Some(role) = message_obj.get("role").and_then(Value::as_str) else {
            continue;
        };
        match role {
            "system" => {
                if let Some(content) = message_obj.get("content").and_then(Value::as_str) {
                    if !content.trim().is_empty() {
                        instructions_parts.push(content.to_string());
                    }
                }
            }
            "user" => {
                if let Some(content) = message_obj.get("content").and_then(Value::as_str) {
                    let trimmed = content.trim();
                    if !trimmed.is_empty() {
                        input_items.push(json!({
                            "type": "message",
                            "role": "user",
                            "content": [{ "type": "input_text", "text": trimmed }]
                        }));
                    }
                }
            }
            "assistant" => {
                if let Some(content) = message_obj.get("content").and_then(Value::as_str) {
                    let trimmed = content.trim();
                    if !trimmed.is_empty() {
                        input_items.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": trimmed }]
                        }));
                    }
                }
                if let Some(tool_calls) = message_obj.get("tool_calls").and_then(Value::as_array) {
                    for (index, tool_call) in tool_calls.iter().enumerate() {
                        let Some(tool_obj) = tool_call.as_object() else {
                            continue;
                        };
                        let call_id = tool_obj
                            .get("id")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("call_{index}"));
                        let Some(function_name) = tool_obj
                            .get("function")
                            .and_then(|value| value.get("name"))
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                        else {
                            continue;
                        };
                        let arguments = tool_obj
                            .get("function")
                            .and_then(|value| value.get("arguments"))
                            .map(|value| {
                                if let Some(text) = value.as_str() {
                                    text.to_string()
                                } else {
                                    serde_json::to_string(value)
                                        .unwrap_or_else(|_| "{}".to_string())
                                }
                            })
                            .unwrap_or_else(|| "{}".to_string());
                        input_items.push(json!({
                            "type": "function_call",
                            "call_id": call_id,
                            "name": function_name,
                            "arguments": arguments
                        }));
                    }
                }
            }
            "tool" => {
                let call_id = message_obj
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "tool role message missing tool_call_id".to_string())?;
                let output = message_obj
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                input_items.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output
                }));
            }
            _ => {}
        }
    }

    let instructions = if instructions_parts.is_empty() {
        None
    } else {
        Some(instructions_parts.join("\n\n"))
    };
    Ok((instructions, input_items))
}

fn extract_tool_result_content(value: Option<&Value>) -> Result<String, String> {
    let Some(value) = value else {
        return Ok(String::new());
    };
    if value.is_null() {
        return Ok(String::new());
    }
    if let Some(text) = value.as_str() {
        return Ok(text.to_string());
    }
    if let Some(array) = value.as_array() {
        let mut out = String::new();
        for item in array {
            if let Some(text) = item.as_str() {
                out.push_str(text);
                continue;
            }
            if let Some(item_obj) = item.as_object() {
                let item_type = item_obj
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if item_type == "text" {
                    if let Some(text) = item_obj.get("text").and_then(Value::as_str) {
                        out.push_str(text);
                        continue;
                    }
                }
            }
            out.push_str(&serde_json::to_string(item).unwrap_or_else(|_| "".to_string()));
        }
        return Ok(out);
    }
    if let Some(item_obj) = value.as_object() {
        let item_type = item_obj
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if item_type == "text" {
            if let Some(text) = item_obj.get("text").and_then(Value::as_str) {
                return Ok(text.to_string());
            }
        }
    }
    serde_json::to_string(value)
        .map_err(|err| format!("serialize tool_result content failed: {err}"))
}

fn map_anthropic_tool_definition(value: &Value) -> Option<Value> {
    let Some(obj) = value.as_object() else {
        return None;
    };
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| obj.get("type").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let description = obj
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let parameters = obj
        .get("input_schema")
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));

    let mut tool_obj = serde_json::Map::new();
    tool_obj.insert("type".to_string(), Value::String("function".to_string()));
    tool_obj.insert("name".to_string(), Value::String(name.to_string()));
    if !description.is_empty() {
        tool_obj.insert("description".to_string(), Value::String(description));
    }
    tool_obj.insert("parameters".to_string(), parameters);

    Some(Value::Object(tool_obj))
}

fn map_anthropic_tool_choice(value: &Value) -> Option<Value> {
    if let Some(text) = value.as_str() {
        return Some(Value::String(text.to_string()));
    }
    let Some(obj) = value.as_object() else {
        return None;
    };
    let choice_type = obj.get("type").and_then(Value::as_str).unwrap_or("auto");
    match choice_type {
        "auto" => Some(Value::String("auto".to_string())),
        "any" => Some(Value::String("required".to_string())),
        "none" => Some(Value::String("none".to_string())),
        "tool" => {
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())?;
            Some(json!({
                "type": "function",
                "name": name
            }))
        }
        _ => None,
    }
}

fn extract_text_content(value: &Value) -> Result<String, String> {
    if let Some(text) = value.as_str() {
        return Ok(text.to_string());
    }

    if let Some(block) = value.as_object() {
        return extract_text_from_block(block);
    }

    if let Some(array) = value.as_array() {
        let mut parts = Vec::new();
        for item in array {
            let Some(block) = item.as_object() else {
                return Err("invalid claude content block".to_string());
            };
            parts.push(extract_text_from_block(block)?);
        }
        return Ok(parts.join(""));
    }

    Err("unsupported claude content".to_string())
}

fn extract_text_from_block(block: &serde_json::Map<String, Value>) -> Result<String, String> {
    let block_type = block
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "claude content block missing type".to_string())?;
    if block_type != "text" {
        return Err(format!(
            "unsupported claude content block type: {block_type}"
        ));
    }
    block
        .get("text")
        .and_then(Value::as_str)
        .map(|v| v.to_string())
        .ok_or_else(|| "claude text block missing text".to_string())
}

#[cfg(test)]
mod tests {
    use super::convert_openai_chat_completions_request;
    use serde_json::json;

    #[test]
    fn openai_chat_request_maps_text_images_and_tools_to_responses() {
        let source = json!({
            "model": "gpt-5.3-codex",
            "messages": [
                { "role": "system", "content": "Be precise." },
                { "role": "user", "content": [
                    { "type": "text", "text": "look at this" },
                    { "type": "image_url", "image_url": { "url": "https://example.com/a.png", "detail": "high" } }
                ]},
                { "role": "assistant", "content": "I'll inspect it." },
                { "role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\":\"README.md\"}"
                    }
                }]},
                { "role": "tool", "tool_call_id": "call_1", "content": "file content" }
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" }
                        }
                    }
                }
            }],
            "tool_choice": {
                "type": "function",
                "function": { "name": "read_file" }
            },
            "stream": false
        });
        let (body, request_stream) = convert_openai_chat_completions_request(
            &serde_json::to_vec(&source).expect("serialize"),
        )
        .expect("convert");
        assert!(!request_stream);
        let payload: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(payload["instructions"], "Be precise.");
        assert_eq!(payload["stream"], true);
        assert_eq!(payload["store"], false);
        assert_eq!(payload["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(payload["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(payload["input"][0]["content"][1]["detail"], "high");
        assert_eq!(payload["input"][2]["type"], "function_call");
        assert_eq!(payload["input"][3]["type"], "function_call_output");
        assert_eq!(payload["tools"][0]["name"], "read_file");
        assert_eq!(payload["tool_choice"]["name"], "read_file");
    }

    #[test]
    fn openai_chat_request_rejects_unsupported_fields() {
        let source = json!({
            "model": "gpt-5.3-codex",
            "messages": [{ "role": "user", "content": "hello" }],
            "temperature": 0.5
        });
        let err = convert_openai_chat_completions_request(
            &serde_json::to_vec(&source).expect("serialize"),
        )
        .expect_err("should reject");
        assert!(err.contains("unsupported openai chat field"));
        assert!(err.contains("temperature"));
    }
}
