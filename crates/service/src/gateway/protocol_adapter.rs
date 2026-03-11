use crate::apikey_profile::PROTOCOL_ANTHROPIC_NATIVE;

mod prompt_cache;
mod request_mapping;
mod response_conversion;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResponseAdapter {
    Passthrough,
    AnthropicJson,
    AnthropicSse,
    OpenAiChatJson,
    OpenAiChatSse,
}

#[derive(Debug)]
pub(super) struct AdaptedGatewayRequest {
    pub(super) path: String,
    pub(super) body: Vec<u8>,
    pub(super) response_adapter: ResponseAdapter,
}

pub(super) fn adapt_request_for_protocol(
    protocol_type: &str,
    path: &str,
    body: Vec<u8>,
) -> Result<AdaptedGatewayRequest, String> {
    if protocol_type != PROTOCOL_ANTHROPIC_NATIVE {
        return Ok(AdaptedGatewayRequest {
            path: path.to_string(),
            body,
            response_adapter: ResponseAdapter::Passthrough,
        });
    }

    if path == "/v1/messages" || path.starts_with("/v1/messages?") {
        let (adapted_body, request_stream) =
            request_mapping::convert_anthropic_messages_request(&body)?;
        // 说明：non-stream 也统一走 /v1/responses。
        // 在部分账号/环境下 /v1/responses/compact 更容易触发 challenge 或非预期拦截。
        let adapted_path = "/v1/responses".to_string();
        return Ok(AdaptedGatewayRequest {
            path: adapted_path,
            body: adapted_body,
            response_adapter: if request_stream {
                ResponseAdapter::AnthropicSse
            } else {
                ResponseAdapter::AnthropicJson
            },
        });
    }

    Ok(AdaptedGatewayRequest {
        path: path.to_string(),
        body,
        response_adapter: ResponseAdapter::Passthrough,
    })
}

pub(super) fn maybe_adapt_openai_chat_compat(
    path: &str,
    body: Vec<u8>,
    request_stream: bool,
    explicit_upstream_base: Option<&str>,
) -> Result<Option<AdaptedGatewayRequest>, String> {
    let is_chat_completions =
        path == "/v1/chat/completions" || path.starts_with("/v1/chat/completions?");
    if !is_chat_completions {
        return Ok(None);
    }
    let resolved_base = explicit_upstream_base
        .map(str::to_string)
        .unwrap_or_else(super::upstream::config::resolve_upstream_base_url);
    let normalized_base = super::upstream::config::normalize_upstream_base_url(&resolved_base);
    if !super::upstream::config::is_chatgpt_backend_base(&normalized_base) {
        return Ok(None);
    }
    let (adapted_body, _ignored_request_stream) =
        request_mapping::convert_openai_chat_completions_request(&body)?;
    let adapted_path = if let Some((_, query)) = path.split_once('?') {
        format!("/v1/responses?{query}")
    } else {
        "/v1/responses".to_string()
    };
    Ok(Some(AdaptedGatewayRequest {
        path: adapted_path,
        body: adapted_body,
        response_adapter: if request_stream {
            ResponseAdapter::OpenAiChatSse
        } else {
            ResponseAdapter::OpenAiChatJson
        },
    }))
}

pub(super) fn adapt_upstream_response(
    adapter: ResponseAdapter,
    upstream_content_type: Option<&str>,
    body: &[u8],
) -> Result<(Vec<u8>, &'static str), String> {
    response_conversion::adapt_upstream_response(adapter, upstream_content_type, body)
}

pub(super) fn build_anthropic_error_body(message: &str) -> Vec<u8> {
    response_conversion::build_anthropic_error_body(message)
}

#[cfg(test)]
mod tests {
    use super::{maybe_adapt_openai_chat_compat, ResponseAdapter};

    #[test]
    fn codex_backend_rewrites_chat_completions_to_responses() {
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "gpt-5.3-codex",
            "messages": [{ "role": "user", "content": "hello" }],
            "stream": false
        }))
        .expect("serialize");
        let adapted = maybe_adapt_openai_chat_compat(
            "/v1/chat/completions",
            body,
            false,
            Some("https://chatgpt.com/backend-api/codex"),
        )
        .expect("adapt")
        .expect("compat enabled");
        assert_eq!(adapted.path, "/v1/responses");
        assert_eq!(adapted.response_adapter, ResponseAdapter::OpenAiChatJson);
    }

    #[test]
    fn openai_api_base_keeps_chat_completions_passthrough() {
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "gpt-5.3-codex",
            "messages": [{ "role": "user", "content": "hello" }],
            "stream": true
        }))
        .expect("serialize");
        let adapted = maybe_adapt_openai_chat_compat(
            "/v1/chat/completions",
            body,
            true,
            Some("https://api.openai.com/v1"),
        )
        .expect("adapt");
        assert!(adapted.is_none());
    }
}
