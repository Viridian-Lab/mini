use crate::{
    Config, ModelMessage, ModelProtocol, ModelResponse, ModelToolCall, ToolSpec,
    model::{auth_token, request_body, request_body_without_tools},
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub(crate) fn call_model(
    system: &str,
    config: &Config,
    messages: &[ModelMessage],
    tools: &[ToolSpec],
    on_delta: impl FnMut(&str),
) -> Result<ModelResponse> {
    call_model_interruptible(system, config, messages, tools, on_delta, None)
}

pub(crate) fn call_model_interruptible(
    system: &str,
    config: &Config,
    messages: &[ModelMessage],
    tools: &[ToolSpec],
    on_delta: impl FnMut(&str),
    interrupted: Option<Arc<AtomicBool>>,
) -> Result<ModelResponse> {
    call_model_with_body(system, config, messages, Some(tools), on_delta, interrupted)
}

pub(crate) fn call_model_without_tools(
    system: &str,
    config: &Config,
    messages: &[ModelMessage],
) -> Result<ModelResponse> {
    call_model_with_body(system, config, messages, None, |_| {}, None)
}

fn call_model_with_body(
    system: &str,
    config: &Config,
    messages: &[ModelMessage],
    tools: Option<&[ToolSpec]>,
    on_delta: impl FnMut(&str),
    interrupted: Option<Arc<AtomicBool>>,
) -> Result<ModelResponse> {
    let provider = config.model.provider(&config.providers)?;
    let (token, chatgpt_account_id) = auth_token(config, &provider)?;
    let body = if let Some(tools) = tools {
        request_body(system, messages, &config.model, &provider, tools)
    } else {
        request_body_without_tools(system, messages, &config.model, &provider)
    };
    // The blocking client's default 30s timeout covers the whole request,
    // including reading the streamed body; model responses regularly run longer.
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(None)
        .build()
        .context("failed to build HTTP client")?;
    let endpoint = provider.endpoint(&config.model.model);
    let headers = provider.headers(token.as_deref(), chatgpt_account_id.as_deref());

    let response = send_with_retry(&client, &endpoint, &body, &headers, &interrupted)?;

    if provider.protocol != ModelProtocol::Gemini {
        let response =
            streamed_assistant_response(provider.protocol, response, on_delta, interrupted)?;
        if tools.is_none() && !response.tool_calls.is_empty() {
            anyhow::bail!("model returned tool calls during no-tools request");
        }
        return Ok(response);
    }

    if interrupted
        .as_ref()
        .is_some_and(|interrupted| interrupted.load(Ordering::Relaxed))
    {
        anyhow::bail!("model request interrupted");
    }

    let response_text = response.text().context("failed to read model response")?;
    let response_json: Value =
        serde_json::from_str(&response_text).context("model response was not JSON")?;
    let response = assistant_response(provider.protocol, &response_json)?;
    if tools.is_none() && !response.tool_calls.is_empty() {
        anyhow::bail!("model returned tool calls during no-tools request");
    }
    Ok(response)
}

/// Maximum number of streamed tool calls we will buffer. The `index` field is
/// server-supplied; without a bound a malformed event could drive unbounded
/// `Vec` growth and exhaust memory.
const MAX_STREAM_TOOL_CALLS: usize = 256;

/// Bounded retry for transient model API failures (connect errors, 408/429/5xx
/// including Anthropic's 529 overloaded). Retries only happen before any
/// response body is read, so no partial stream is ever replayed.
fn send_with_retry(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    body: &Value,
    headers: &[(&'static str, String)],
    interrupted: &Option<Arc<AtomicBool>>,
) -> Result<reqwest::blocking::Response> {
    const MAX_ATTEMPTS: u32 = 4;
    let is_interrupted = || {
        interrupted
            .as_ref()
            .is_some_and(|interrupted| interrupted.load(Ordering::Relaxed))
    };
    let mut attempt = 0;
    loop {
        attempt += 1;
        if is_interrupted() {
            anyhow::bail!("model request interrupted");
        }

        let mut builder = client.post(endpoint).json(body);
        for (name, value) in headers {
            builder = builder.header(*name, value);
        }

        let last = attempt >= MAX_ATTEMPTS;
        match builder.send() {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return Ok(response);
                }
                let retryable =
                    status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error();
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<u64>().ok());
                if !retryable || last {
                    let response_text = response.text().context("failed to read model response")?;
                    anyhow::bail!("model request failed with status {status}: {response_text}");
                }
                backoff(attempt, retry_after, interrupted)?;
            }
            Err(err) => {
                if last || !(err.is_connect() || err.is_timeout() || err.is_request()) {
                    return Err(err).context("model request failed");
                }
                backoff(attempt, None, interrupted)?;
            }
        }
    }
}

fn backoff(
    attempt: u32,
    retry_after_secs: Option<u64>,
    interrupted: &Option<Arc<AtomicBool>>,
) -> Result<()> {
    let base = retry_after_secs
        .map(|secs| Duration::from_secs(secs.min(30)))
        .unwrap_or_else(|| Duration::from_millis(500 * (1u64 << (attempt - 1).min(5))));
    // Sleep in short slices so an interrupt is honored promptly.
    let mut waited = Duration::ZERO;
    while waited < base {
        if interrupted
            .as_ref()
            .is_some_and(|interrupted| interrupted.load(Ordering::Relaxed))
        {
            anyhow::bail!("model request interrupted");
        }
        std::thread::sleep(Duration::from_millis(100));
        waited += Duration::from_millis(100);
    }
    Ok(())
}

#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// A streamed Anthropic `thinking`/`redacted_thinking` block, assembled from
/// `content_block_start` plus `thinking_delta`/`signature_delta` events.
struct PendingThinking {
    kind: String,
    thinking: String,
    signature: String,
    data: String,
}

impl PendingThinking {
    fn into_block(self) -> Value {
        if self.kind == "redacted_thinking" {
            json!({ "type": "redacted_thinking", "data": self.data })
        } else {
            json!({
                "type": "thinking",
                "thinking": self.thinking,
                "signature": self.signature,
            })
        }
    }
}

fn tool_input(value: &Value) -> Value {
    if let Some(arguments) = value.as_str() {
        let trimmed = arguments.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            // Looks like JSON. If it fails to parse it is malformed or
            // truncated; surface it as missing input rather than executing the
            // raw fragment as a shell command.
            serde_json::from_str(arguments).unwrap_or(Value::Null)
        } else {
            // A bare (non-JSON) argument string is left as-is, not wrapped in a
            // bash-specific `{ "command": ... }` shape. The bash dispatch reads
            // it via `input.as_str()`; a mounted tool that received a bare
            // string sees non-object input rather than a bogus `command` key.
            Value::String(arguments.to_string())
        }
    } else {
        value.clone()
    }
}

fn streamed_assistant_response(
    protocol: ModelProtocol,
    response: reqwest::blocking::Response,
    mut on_delta: impl FnMut(&str),
    interrupted: Option<Arc<AtomicBool>>,
) -> Result<ModelResponse> {
    let mut assistant = String::new();
    let mut final_response = None;
    let mut openai_tools = Vec::<PendingToolCall>::new();
    let mut anthropic_tools = Vec::<PendingToolCall>::new();
    let mut anthropic_thinking = Vec::<Option<PendingThinking>>::new();
    let mut data = String::new();

    {
        let mut handle_data = |data: &str| -> Result<bool> {
            let data = data.trim();
            if data.is_empty() {
                return Ok(false);
            }
            if data == "[DONE]" {
                return Ok(true);
            }

            let event: Value = serde_json::from_str(data)
                .with_context(|| format!("model stream event was not JSON: {data}"))?;

            if let Some(message) = event["error"]["message"]
                .as_str()
                .or_else(|| event["response"]["error"]["message"].as_str())
            {
                anyhow::bail!("model stream failed: {message}");
            }

            match protocol {
                ModelProtocol::OpenAiResponses => {
                    let event_type = event["type"].as_str().unwrap_or_default();
                    if event_type == "response.output_text.delta" {
                        if let Some(delta) = event["delta"].as_str() {
                            assistant.push_str(delta);
                            on_delta(delta);
                        }
                    } else if event_type == "response.completed" {
                        final_response = assistant_response(protocol, &event["response"]).ok();
                    } else if event_type == "response.failed" {
                        let message = event["response"]["error"]["message"]
                            .as_str()
                            .unwrap_or("response failed");
                        anyhow::bail!("model stream failed: {message}");
                    } else if event_type == "response.output_item.done" {
                        let item = &event["item"];
                        if item["type"].as_str() == Some("function_call")
                            && let Some(name) = item["name"].as_str()
                        {
                            openai_tools.push(PendingToolCall {
                                id: item["call_id"]
                                    .as_str()
                                    .or_else(|| item["id"].as_str())
                                    .unwrap_or("call")
                                    .to_string(),
                                name: name.to_string(),
                                arguments: item["arguments"].as_str().unwrap_or("{}").to_string(),
                            });
                        }
                    }
                }
                ModelProtocol::OpenAiChatCompletions => {
                    if let Some(delta) = event["choices"][0]["delta"]["content"].as_str() {
                        assistant.push_str(delta);
                        on_delta(delta);
                    }
                    if let Some(calls) = event["choices"][0]["delta"]["tool_calls"].as_array() {
                        for call in calls {
                            let index = call["index"].as_u64().unwrap_or(0) as usize;
                            if index >= MAX_STREAM_TOOL_CALLS {
                                anyhow::bail!("model stream tool call index {index} out of range");
                            }
                            while openai_tools.len() <= index {
                                openai_tools.push(PendingToolCall::default());
                            }
                            let pending = &mut openai_tools[index];
                            if let Some(id) = call["id"].as_str() {
                                pending.id = id.to_string();
                            }
                            if let Some(name) = call["function"]["name"].as_str() {
                                pending.name = name.to_string();
                            }
                            if let Some(arguments) = call["function"]["arguments"].as_str() {
                                pending.arguments.push_str(arguments);
                            }
                        }
                    }
                }
                ModelProtocol::Anthropic => {
                    if event["type"].as_str() == Some("error") {
                        let message = event["error"]["message"]
                            .as_str()
                            .unwrap_or("anthropic stream failed");
                        anyhow::bail!("model stream failed: {message}");
                    }
                    let block_type = event["content_block"]["type"].as_str();
                    if event["type"].as_str() == Some("content_block_start")
                        && block_type == Some("tool_use")
                    {
                        let index = event["index"].as_u64().unwrap_or(0) as usize;
                        if index >= MAX_STREAM_TOOL_CALLS {
                            anyhow::bail!("model stream tool call index {index} out of range");
                        }
                        while anthropic_tools.len() <= index {
                            anthropic_tools.push(PendingToolCall::default());
                        }
                        anthropic_tools[index] = PendingToolCall {
                            id: event["content_block"]["id"]
                                .as_str()
                                .unwrap_or("toolu")
                                .to_string(),
                            name: event["content_block"]["name"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                            arguments: String::new(),
                        };
                    } else if event["type"].as_str() == Some("content_block_start")
                        && matches!(block_type, Some("thinking") | Some("redacted_thinking"))
                    {
                        let index = event["index"].as_u64().unwrap_or(0) as usize;
                        if index >= MAX_STREAM_TOOL_CALLS {
                            anyhow::bail!("model stream thinking block index {index} out of range");
                        }
                        while anthropic_thinking.len() <= index {
                            anthropic_thinking.push(None);
                        }
                        anthropic_thinking[index] = Some(PendingThinking {
                            kind: block_type.unwrap_or("thinking").to_string(),
                            thinking: event["content_block"]["thinking"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                            signature: event["content_block"]["signature"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                            data: event["content_block"]["data"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                        });
                    } else if event["delta"]["type"].as_str() == Some("input_json_delta") {
                        let index = event["index"].as_u64().unwrap_or(0) as usize;
                        if let Some(partial_json) = event["delta"]["partial_json"].as_str()
                            && let Some(pending) = anthropic_tools.get_mut(index)
                        {
                            pending.arguments.push_str(partial_json);
                        }
                    } else if event["delta"]["type"].as_str() == Some("thinking_delta") {
                        let index = event["index"].as_u64().unwrap_or(0) as usize;
                        if let Some(thinking) = event["delta"]["thinking"].as_str()
                            && let Some(Some(pending)) = anthropic_thinking.get_mut(index)
                        {
                            pending.thinking.push_str(thinking);
                        }
                    } else if event["delta"]["type"].as_str() == Some("signature_delta") {
                        let index = event["index"].as_u64().unwrap_or(0) as usize;
                        if let Some(signature) = event["delta"]["signature"].as_str()
                            && let Some(Some(pending)) = anthropic_thinking.get_mut(index)
                        {
                            pending.signature.push_str(signature);
                        }
                    } else if let Some(delta) = event["delta"]["text"].as_str() {
                        assistant.push_str(delta);
                        on_delta(delta);
                    }
                }
                ModelProtocol::Gemini => {
                    if let Some(delta) = assistant_response(protocol, &event)
                        .ok()
                        .map(|response| response.text)
                        .filter(|text| !text.is_empty())
                    {
                        assistant.push_str(&delta);
                        on_delta(&delta);
                    }
                }
            }

            Ok(false)
        };

        for line in BufReader::new(response).lines() {
            if interrupted
                .as_ref()
                .is_some_and(|interrupted| interrupted.load(Ordering::Relaxed))
            {
                anyhow::bail!("model request interrupted");
            }
            let line = line.map_err(|err| anyhow::anyhow!("failed to read model stream: {err}"))?;
            if line.is_empty() {
                if handle_data(&data)? {
                    break;
                }
                data.clear();
                continue;
            }
            if let Some(line) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(line.trim_start());
            }
        }
        if !data.is_empty() {
            handle_data(&data)?;
        }
    }

    if let Some(mut response) = final_response {
        if response.text.is_empty() {
            response.text = assistant;
        } else if assistant.is_empty() {
            on_delta(&response.text);
        }
        return Ok(response);
    }

    let tool_calls = match protocol {
        ModelProtocol::OpenAiResponses | ModelProtocol::OpenAiChatCompletions => openai_tools
            .into_iter()
            .enumerate()
            .filter(|(_, call)| !call.name.is_empty())
            .map(|(index, call)| ModelToolCall {
                id: if call.id.is_empty() {
                    format!("call_{index}")
                } else {
                    call.id
                },
                name: call.name,
                input: tool_input(&Value::String(call.arguments)),
            })
            .collect(),
        ModelProtocol::Anthropic => anthropic_tools
            .into_iter()
            .filter(|call| !call.name.is_empty())
            .map(|call| ModelToolCall {
                id: call.id,
                name: call.name,
                input: tool_input(&Value::String(call.arguments)),
            })
            .collect(),
        ModelProtocol::Gemini => Vec::new(),
    };

    let thinking: Vec<Value> = anthropic_thinking
        .into_iter()
        .flatten()
        .map(PendingThinking::into_block)
        .collect();

    if assistant.is_empty() && tool_calls.is_empty() {
        anyhow::bail!("model stream did not contain assistant text or tool calls");
    }

    Ok(ModelResponse {
        text: assistant,
        tool_calls,
        thinking,
    })
}

fn assistant_response(protocol: ModelProtocol, response: &Value) -> Result<ModelResponse> {
    let mut tool_calls = Vec::new();
    let mut thinking = Vec::new();
    let text = match protocol {
        ModelProtocol::OpenAiResponses => {
            for (index, item) in response["output"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                if item["type"].as_str() == Some("function_call")
                    && let Some(name) = item["name"].as_str()
                {
                    tool_calls.push(ModelToolCall {
                        id: item["call_id"]
                            .as_str()
                            .or_else(|| item["id"].as_str())
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("call_{index}")),
                        name: name.to_string(),
                        input: tool_input(&item["arguments"]),
                    });
                }
            }
            response["output_text"]
                .as_str()
                .map(str::to_string)
                .or_else(|| {
                    let mut text = String::new();
                    for item in response["output"].as_array().into_iter().flatten() {
                        for content in item["content"].as_array().into_iter().flatten() {
                            if let Some(part) = content["text"].as_str() {
                                text.push_str(part);
                            }
                        }
                    }
                    (!text.is_empty()).then_some(text)
                })
        }
        ModelProtocol::OpenAiChatCompletions => {
            if let Some(calls) = response["choices"][0]["message"]["tool_calls"].as_array() {
                for (index, call) in calls.iter().enumerate() {
                    if let Some(name) = call["function"]["name"].as_str() {
                        tool_calls.push(ModelToolCall {
                            id: call["id"]
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| format!("call_{index}")),
                            name: name.to_string(),
                            input: tool_input(&call["function"]["arguments"]),
                        });
                    }
                }
            }
            response["choices"][0]["message"]["content"]
                .as_str()
                .map(str::to_string)
        }
        ModelProtocol::Anthropic => {
            for part in response["content"].as_array().into_iter().flatten() {
                match part["type"].as_str() {
                    Some("tool_use") => {
                        if let Some(name) = part["name"].as_str() {
                            tool_calls.push(ModelToolCall {
                                id: part["id"].as_str().unwrap_or("toolu").to_string(),
                                name: name.to_string(),
                                input: part["input"].clone(),
                            });
                        }
                    }
                    Some("thinking") | Some("redacted_thinking") => thinking.push(part.clone()),
                    _ => {}
                }
            }
            response["content"]
                .as_array()
                .map(|content| {
                    content
                        .iter()
                        .filter(|part| part["type"].as_str().unwrap_or("text") == "text")
                        .filter_map(|part| part["text"].as_str())
                        .collect::<String>()
                })
                .filter(|text| !text.is_empty())
        }
        ModelProtocol::Gemini => {
            if let Some(parts) = response["candidates"][0]["content"]["parts"].as_array() {
                for (index, part) in parts.iter().enumerate() {
                    if let Some(name) = part["functionCall"]["name"].as_str() {
                        tool_calls.push(ModelToolCall {
                            id: format!("gemini_call_{index}"),
                            name: name.to_string(),
                            input: part["functionCall"]["args"].clone(),
                        });
                    }
                }
            }
            response["candidates"][0]["content"]["parts"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<String>()
                })
                .filter(|text| !text.is_empty())
        }
    };

    let text = text.unwrap_or_default();
    if text.is_empty() && tool_calls.is_empty() {
        anyhow::bail!("model response did not contain assistant text or tool calls");
    }
    Ok(ModelResponse {
        text,
        tool_calls,
        thinking,
    })
}

#[cfg(test)]
mod transport_tests {
    use super::*;

    #[test]
    fn tool_input_parses_valid_json_object() {
        let input = tool_input(&Value::String("{\"command\": \"ls -la\"}".to_string()));
        assert_eq!(input["command"], "ls -la");
    }

    #[test]
    fn tool_input_keeps_a_bare_string_as_is() {
        // A bare string is no longer coerced into a bash `command` object; the
        // bash dispatch reads it via `as_str()`, mounted tools see non-object.
        let input = tool_input(&Value::String("echo hi".to_string()));
        assert_eq!(input.as_str(), Some("echo hi"));
        assert!(input.get("command").is_none());
    }

    #[test]
    fn tool_input_does_not_execute_malformed_json() {
        // A truncated JSON object must not be run verbatim as a shell command.
        let input = tool_input(&Value::String("{\"command\": \"rm -rf build".to_string()));
        assert!(input.is_null());
        assert!(input.get("command").is_none());
    }
}
