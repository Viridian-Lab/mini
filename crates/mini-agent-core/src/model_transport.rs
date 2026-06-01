use crate::{
    Config, ModelMessage, ModelProtocol, ModelResponse, ModelToolCall,
    model::{auth_token, request_body, request_body_without_tools},
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) fn call_model(
    system: &str,
    config: &Config,
    messages: &[ModelMessage],
    on_delta: impl FnMut(&str),
) -> Result<ModelResponse> {
    call_model_interruptible(system, config, messages, on_delta, None)
}

pub(crate) fn call_model_interruptible(
    system: &str,
    config: &Config,
    messages: &[ModelMessage],
    on_delta: impl FnMut(&str),
    interrupted: Option<Arc<AtomicBool>>,
) -> Result<ModelResponse> {
    call_model_with_body(system, config, messages, true, on_delta, interrupted)
}

pub(crate) fn call_model_without_tools(
    system: &str,
    config: &Config,
    messages: &[ModelMessage],
) -> Result<ModelResponse> {
    call_model_with_body(system, config, messages, false, |_| {}, None)
}

fn call_model_with_body(
    _system: &str,
    config: &Config,
    messages: &[ModelMessage],
    tools: bool,
    on_delta: impl FnMut(&str),
    interrupted: Option<Arc<AtomicBool>>,
) -> Result<ModelResponse> {
    let provider = config.model.provider(&config.providers)?;
    let (token, chatgpt_account_id) = auth_token(&provider)?;
    let body = if tools {
        request_body(_system, messages, &config.model, &provider)
    } else {
        request_body_without_tools(_system, messages, &config.model, &provider)
    };
    let client = reqwest::blocking::Client::new();
    let mut builder = client
        .post(provider.endpoint(&config.model.model))
        .json(&body);
    for (name, value) in provider.headers(token.as_deref(), chatgpt_account_id.as_deref()) {
        builder = builder.header(name, value);
    }

    if interrupted
        .as_ref()
        .is_some_and(|interrupted| interrupted.load(Ordering::Relaxed))
    {
        anyhow::bail!("model request interrupted");
    }

    let response = builder.send().context("model request failed")?;
    let status = response.status();
    if !status.is_success() {
        if interrupted
            .as_ref()
            .is_some_and(|interrupted| interrupted.load(Ordering::Relaxed))
        {
            anyhow::bail!("model request interrupted");
        }

        let response_text = response.text().context("failed to read model response")?;
        anyhow::bail!("model request failed with status {status}: {response_text}");
    }

    if provider.protocol != ModelProtocol::Gemini {
        let response =
            streamed_assistant_response(provider.protocol, response, on_delta, interrupted)?;
        if !tools && !response.tool_calls.is_empty() {
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
    if !tools && !response.tool_calls.is_empty() {
        anyhow::bail!("model returned tool calls during no-tools request");
    }
    Ok(response)
}

#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

fn tool_input(value: &Value) -> Value {
    if let Some(arguments) = value.as_str() {
        serde_json::from_str(arguments).unwrap_or_else(|_| json!({ "command": arguments }))
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
                    if event["type"].as_str() == Some("content_block_start")
                        && event["content_block"]["type"].as_str() == Some("tool_use")
                    {
                        let index = event["index"].as_u64().unwrap_or(0) as usize;
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
                    } else if event["delta"]["type"].as_str() == Some("input_json_delta") {
                        let index = event["index"].as_u64().unwrap_or(0) as usize;
                        if let Some(partial_json) = event["delta"]["partial_json"].as_str()
                            && let Some(pending) = anthropic_tools.get_mut(index)
                        {
                            pending.arguments.push_str(partial_json);
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

    if assistant.is_empty() && tool_calls.is_empty() {
        anyhow::bail!("model stream did not contain assistant text or tool calls");
    }

    Ok(ModelResponse {
        text: assistant,
        tool_calls,
    })
}

fn assistant_response(protocol: ModelProtocol, response: &Value) -> Result<ModelResponse> {
    let mut tool_calls = Vec::new();
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
                    for item in response["output"].as_array()? {
                        for content in item["content"].as_array()? {
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
                if part["type"].as_str() == Some("tool_use")
                    && let Some(name) = part["name"].as_str()
                {
                    tool_calls.push(ModelToolCall {
                        id: part["id"].as_str().unwrap_or("toolu").to_string(),
                        name: name.to_string(),
                        input: part["input"].clone(),
                    });
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
    Ok(ModelResponse { text, tool_calls })
}
