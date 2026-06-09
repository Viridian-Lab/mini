fn bash_tool_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The shell command to run with bash -lc."
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
}

/// Render a single tool definition into the wire shape for `protocol`. `strict`
/// only applies to the OpenAI Responses API and is reserved for the built-in
/// bash tool whose schema satisfies strict-mode constraints; mounted tools
/// (e.g. MCP) carry arbitrary JSON Schemas, so they are sent non-strict.
fn tool_schema_for(
    protocol: ModelProtocol,
    name: &str,
    description: &str,
    parameters: &Value,
    strict: bool,
) -> Value {
    match protocol {
        ModelProtocol::OpenAiResponses => {
            let mut tool = json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": parameters,
            });
            if strict {
                tool["strict"] = json!(true);
            }
            tool
        }
        ModelProtocol::OpenAiChatCompletions => json!({
            "type": "function",
            "function": {
                "name": name,
                "description": description,
                "parameters": parameters,
            }
        }),
        ModelProtocol::Anthropic => json!({
            "name": name,
            "description": description,
            "input_schema": parameters,
        }),
        ModelProtocol::Gemini => json!({
            "name": name,
            "description": description,
            "parameters": parameters,
        }),
    }
}

/// The full tool list for a request: the always-present built-in `bash` plus
/// any mounted tools.
fn tool_schemas(protocol: ModelProtocol, tools: &[ToolSpec]) -> Vec<Value> {
    let mut schemas = vec![tool_schema_for(
        protocol,
        "bash",
        "Run a shell command with bash -lc in the current workspace.",
        &bash_tool_parameters(),
        true,
    )];
    for tool in tools {
        schemas.push(tool_schema_for(
            protocol,
            &tool.name,
            &tool.description,
            &tool.input_schema,
            false,
        ));
    }
    schemas
}

fn role_name(role: ModelRole, protocol: ModelProtocol) -> &'static str {
    match (role, protocol) {
        (ModelRole::User, _) => "user",
        (ModelRole::Assistant, ModelProtocol::Gemini) => "model",
        (ModelRole::Assistant, _) => "assistant",
    }
}

/// For providers without a structured tool-result error flag (OpenAI, Gemini),
/// prefix a marker so the model still recognizes a failed tool call. Anthropic
/// carries `is_error` on the tool_result block instead.
fn marked_tool_content(content: &str, is_error: bool) -> String {
    if is_error {
        format!("[tool error]\n{content}")
    } else {
        content.to_string()
    }
}

pub(crate) fn request_body(
    system: &str,
    messages: &[ModelMessage],
    config: &ModelConfig,
    provider: &Provider,
    tools: &[ToolSpec],
) -> Value {
    request_body_with_tools(system, messages, config, provider, Some(tools), true)
}

pub(crate) fn request_body_without_tools(
    system: &str,
    messages: &[ModelMessage],
    config: &ModelConfig,
    provider: &Provider,
) -> Value {
    request_body_with_tools(system, messages, config, provider, None, true)
}

fn request_body_with_tools(
    system: &str,
    messages: &[ModelMessage],
    config: &ModelConfig,
    provider: &Provider,
    tools: Option<&[ToolSpec]>,
    stream: bool,
) -> Value {
    match provider.protocol {
        ModelProtocol::OpenAiResponses => {
            let mut input = Vec::new();
            for message in messages {
                if let Some(result) = &message.tool_result {
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": result.id,
                        "output": marked_tool_content(&result.content, result.is_error),
                    }));
                    continue;
                }
                if !message.text.is_empty() {
                    input.push(json!({
                        "role": role_name(message.role, ModelProtocol::OpenAiResponses),
                        "content": message.text,
                    }));
                }
                for call in &message.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.input.to_string(),
                    }));
                }
            }
            let mut body = json!({
                "model": config.model,
                "input": input,
                "stream": stream,
                "store": false,
            });
            if let Some(tools) = tools {
                body["tools"] = json!(tool_schemas(provider.protocol, tools));
            }
            if !system.trim().is_empty() {
                body["instructions"] = json!(system);
            }
            if let Some(max_output_tokens) = config.max_output_tokens {
                body["max_output_tokens"] = json!(max_output_tokens);
            }
            if let Some(temperature) = config.temperature {
                body["temperature"] = json!(temperature);
            }
            if let Some(effort) = reasoning_effort(config) {
                let effort = match effort {
                    "on" | "true" | "yes" => "medium",
                    "xhigh" => "high",
                    effort => effort,
                };
                body["reasoning"] = json!({ "effort": effort });
            }
            body
        }
        ModelProtocol::OpenAiChatCompletions => {
            let mut items = Vec::new();
            if !system.trim().is_empty() {
                items.push(json!({
                    "role": "developer",
                    "content": system,
                }));
            }
            items.extend(messages.iter().map(|message| {
                if let Some(result) = &message.tool_result {
                    return json!({
                        "role": "tool",
                        "tool_call_id": result.id,
                        "content": marked_tool_content(&result.content, result.is_error),
                    });
                }
                let mut item = json!({
                    "role": role_name(message.role, ModelProtocol::OpenAiChatCompletions),
                    "content": message.text,
                });
                if !message.tool_calls.is_empty() {
                    item["tool_calls"] = json!(
                        message
                            .tool_calls
                            .iter()
                            .map(|call| {
                                json!({
                                    "id": call.id,
                                    "type": "function",
                                    "function": {
                                        "name": call.name,
                                        "arguments": call.input.to_string(),
                                    }
                                })
                            })
                            .collect::<Vec<_>>()
                    );
                }
                item
            }));

            let mut body = json!({
                "model": config.model,
                "messages": items,
                "stream": stream,
            });
            if let Some(tools) = tools {
                body["tools"] = json!(tool_schemas(provider.protocol, tools));
            }
            if let Some(max_output_tokens) = config.max_output_tokens {
                body["max_completion_tokens"] = json!(max_output_tokens);
            }
            if let Some(temperature) = config.temperature {
                body["temperature"] = json!(temperature);
            }
            if let Some(effort) = reasoning_effort(config) {
                body["reasoning_effort"] = json!(match effort {
                    "on" | "true" | "yes" => "medium",
                    "xhigh" => "high",
                    effort => effort,
                });
            }
            body
        }
        ModelProtocol::Anthropic => {
            let mut max_tokens = config.max_output_tokens.unwrap_or(4096);
            let mut items = Vec::new();
            let mut index = 0;
            while index < messages.len() {
                if messages[index].tool_result.is_some() {
                    let mut content = Vec::new();
                    while let Some(result) = messages
                        .get(index)
                        .and_then(|message| message.tool_result.as_ref())
                    {
                        let mut block = json!({
                            "type": "tool_result",
                            "tool_use_id": result.id,
                            "content": result.content,
                        });
                        if result.is_error {
                            block["is_error"] = json!(true);
                        }
                        content.push(block);
                        index += 1;
                    }
                    items.push(json!({
                        "role": "user",
                        "content": content,
                    }));
                    continue;
                }

                let message = &messages[index];
                // Replay captured thinking blocks (with their signatures) as the
                // leading content. The API requires this before tool_result
                // blocks when thinking is enabled. Only do it when thinking is
                // currently enabled, so disabling reasoning mid-session does not
                // send stale thinking blocks the API would reject.
                let replay_thinking =
                    reasoning_effort(config).is_some() && !message.thinking.is_empty();
                if !message.tool_calls.is_empty() || replay_thinking {
                    let mut content = Vec::new();
                    if replay_thinking {
                        content.extend(message.thinking.iter().cloned());
                    }
                    if !message.text.is_empty() {
                        content.push(json!({ "type": "text", "text": message.text }));
                    }
                    content.extend(message.tool_calls.iter().map(|call| {
                        json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.name,
                            "input": call.input,
                        })
                    }));
                    items.push(json!({
                        "role": role_name(message.role, ModelProtocol::Anthropic),
                        "content": content,
                    }));
                } else {
                    items.push(json!({
                        "role": role_name(message.role, ModelProtocol::Anthropic),
                        "content": message.text,
                    }));
                }
                index += 1;
            }
            let mut body = json!({
                "model": config.model,
                "messages": items,
                "stream": stream,
            });
            if let Some(tools) = tools {
                body["tools"] = json!(tool_schemas(provider.protocol, tools));
            }
            if let Some(effort) = reasoning_effort(config) {
                let budget_tokens: u32 = effort
                    .parse::<u32>()
                    .map(|tokens| tokens.clamp(1024, 200_000))
                    .unwrap_or(match effort {
                        "on" | "true" | "yes" | "medium" => 3072,
                        "minimal" => 1024,
                        "low" => 2048,
                        "high" => 8192,
                        "xhigh" => 16384,
                        _ => 3072,
                    });
                max_tokens = max_tokens.max(budget_tokens.saturating_add(1024));
                body["thinking"] = json!({
                    "type": "enabled",
                    "budget_tokens": budget_tokens,
                });
            }
            body["max_tokens"] = json!(max_tokens);
            if !system.trim().is_empty() {
                body["system"] = json!(system);
            }
            // Anthropic rejects a temperature other than 1 when extended
            // thinking is enabled, so only forward it when thinking is off.
            if let Some(temperature) = config.temperature
                && reasoning_effort(config).is_none()
            {
                body["temperature"] = json!(temperature);
            }
            body
        }
        ModelProtocol::Gemini => {
            let mut contents = Vec::new();
            let mut index = 0;
            while index < messages.len() {
                if messages[index].tool_result.is_some() {
                    let mut parts = Vec::new();
                    while let Some(result) = messages
                        .get(index)
                        .and_then(|message| message.tool_result.as_ref())
                    {
                        parts.push(json!({
                            "functionResponse": {
                                "name": result.name,
                                "response": {
                                    "output": marked_tool_content(&result.content, result.is_error),
                                },
                            }
                        }));
                        index += 1;
                    }
                    contents.push(json!({
                        "role": "user",
                        "parts": parts,
                    }));
                    continue;
                }

                let message = &messages[index];
                let mut parts = Vec::new();
                if !message.text.is_empty() {
                    parts.push(json!({ "text": message.text }));
                }
                parts.extend(message.tool_calls.iter().map(|call| {
                    json!({
                        "functionCall": {
                            "name": call.name,
                            "args": call.input,
                        }
                    })
                }));
                contents.push(json!({
                    "role": role_name(message.role, ModelProtocol::Gemini),
                    "parts": parts,
                }));
                index += 1;
            }
            let mut body = json!({
                "contents": contents,
            });
            if let Some(tools) = tools {
                body["tools"] = json!([{
                    "functionDeclarations": tool_schemas(provider.protocol, tools)
                }]);
            }

            if !system.trim().is_empty() {
                body["systemInstruction"] = json!({
                    "parts": [{ "text": system }]
                });
            }

            let mut generation_config = serde_json::Map::new();
            if let Some(max_output_tokens) = config.max_output_tokens {
                generation_config.insert("maxOutputTokens".to_string(), json!(max_output_tokens));
            }
            if let Some(temperature) = config.temperature {
                generation_config.insert("temperature".to_string(), json!(temperature));
            }
            if let Some(effort) = reasoning_effort(config) {
                let thinking_config = if let Ok(thinking_budget) = effort.parse::<i32>() {
                    json!({ "thinkingBudget": thinking_budget })
                } else {
                    match effort {
                        "on" | "true" | "yes" => json!({ "thinkingLevel": "medium" }),
                        "minimal" => json!({ "thinkingBudget": 512 }),
                        "low" | "medium" | "high" => json!({ "thinkingLevel": effort }),
                        "xhigh" => json!({ "thinkingBudget": 24576 }),
                        effort => json!({ "thinkingLevel": effort }),
                    }
                };
                generation_config.insert("thinkingConfig".to_string(), thinking_config);
            }
            if !generation_config.is_empty() {
                body["generationConfig"] = Value::Object(generation_config);
            }

            body
        }
    }
}
