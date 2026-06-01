impl ModelConfig {
    pub fn provider(&self, providers: &BTreeMap<String, ProviderConfig>) -> Result<Provider> {
        let built_in = match self.provider.as_str() {
            "codex" => Some((
                ModelProtocol::OpenAiResponses,
                ModelAuth::CodexOauth,
                None,
                Some("https://chatgpt.com/backend-api/codex"),
                &["gpt-5.5", "gpt-5.4", "gpt-5.4-mini"][..],
            )),
            "openai" | "openai-responses" => Some((
                ModelProtocol::OpenAiResponses,
                ModelAuth::ApiKey,
                Some("OPENAI_API_KEY"),
                Some("https://api.openai.com/v1"),
                &["gpt-5.5", "gpt-5.4", "gpt-5.4-mini"][..],
            )),
            "openai-chat-completions" | "openai-completions" => Some((
                ModelProtocol::OpenAiChatCompletions,
                ModelAuth::ApiKey,
                Some("OPENAI_API_KEY"),
                Some("https://api.openai.com/v1"),
                &["gpt-5.5", "gpt-5.4", "gpt-5.4-mini"][..],
            )),
            "openrouter" => Some((
                ModelProtocol::OpenAiChatCompletions,
                ModelAuth::ApiKey,
                Some("OPENROUTER_API_KEY"),
                Some("https://openrouter.ai/api/v1"),
                &[][..],
            )),
            "anthropic" => Some((
                ModelProtocol::Anthropic,
                ModelAuth::ApiKey,
                Some("ANTHROPIC_API_KEY"),
                Some("https://api.anthropic.com/v1"),
                &[][..],
            )),
            "gemini" => Some((
                ModelProtocol::Gemini,
                ModelAuth::ApiKey,
                Some("GEMINI_API_KEY"),
                Some("https://generativelanguage.googleapis.com/v1beta"),
                &[][..],
            )),
            _ => None,
        }
        .map(|(protocol, auth, api_key_env, base_url, models)| Provider {
            name: self.provider.clone(),
            protocol,
            auth,
            api_key_env: api_key_env.map(str::to_string),
            base_url: base_url.map(str::to_string),
            models: models.iter().map(|model| (*model).to_string()).collect(),
        });
        let configured = providers.get(&self.provider);
        let protocol = self
            .protocol
            .or_else(|| configured.and_then(|provider| provider.protocol))
            .or_else(|| built_in.as_ref().map(|provider| provider.protocol))
            .with_context(|| {
                format!(
                    "unknown provider '{}'; define [providers.{}].protocol or use a built-in provider",
                    self.provider, self.provider
                )
            })?;

        let mut models = built_in
            .as_ref()
            .map(|provider| provider.models.clone())
            .unwrap_or_default();
        if let Some(configured) = configured {
            for model in &configured.models {
                if !models.contains(model) {
                    models.push(model.clone());
                }
            }
        }

        Ok(Provider {
            name: self.provider.clone(),
            protocol,
            auth: self
                .auth
                .or_else(|| configured.and_then(|provider| provider.auth))
                .or_else(|| built_in.as_ref().map(|provider| provider.auth))
                .unwrap_or(ModelAuth::ApiKey),
            api_key_env: self
                .api_key_env
                .clone()
                .or_else(|| configured.and_then(|provider| provider.api_key_env.clone()))
                .or_else(|| {
                    built_in
                        .as_ref()
                        .and_then(|provider| provider.api_key_env.clone())
                }),
            base_url: self
                .base_url
                .clone()
                .or_else(|| configured.and_then(|provider| provider.base_url.clone()))
                .or_else(|| {
                    built_in
                        .as_ref()
                        .and_then(|provider| provider.base_url.clone())
                }),
            models,
        })
    }
}

pub fn list_models(
    config: &ModelConfig,
    providers: &BTreeMap<String, ProviderConfig>,
) -> Result<Vec<String>> {
    let provider = config.provider(providers)?;
    let base_url = provider
        .base_url
        .as_deref()
        .unwrap_or_else(|| provider.protocol.default_base_url())
        .trim_end_matches('/');

    let configured_models = providers
        .get(&config.provider)
        .map(|provider| provider.models.as_slice())
        .unwrap_or_default();

    let mut models = match provider.name.as_str() {
        "codex"
        | "openai"
        | "openai-responses"
        | "openai-chat-completions"
        | "openai-completions" => match provider.protocol {
            ModelProtocol::OpenAiResponses | ModelProtocol::OpenAiChatCompletions => {
                let mut url = format!("{base_url}/models");
                if provider.auth == ModelAuth::CodexOauth {
                    url.push_str("?client_version=");
                    let client_version = CODEX_MODELS_CLIENT_VERSION.get_or_init(|| {
                        reqwest::blocking::Client::builder()
                            .timeout(Duration::from_secs(2))
                            .build()
                            .ok()
                            .and_then(|client| {
                                client
                                    .get("https://registry.npmjs.org/@openai/codex/latest")
                                    .send()
                                    .ok()
                            })
                            .filter(|response| response.status().is_success())
                            .and_then(|response| response.json::<Value>().ok())
                            .and_then(|json| {
                                json["version"].as_str().and_then(|version| {
                                    let version =
                                        version.split(['-', '+']).next().unwrap_or(version);
                                    let mut parts = version.split('.');
                                    let valid = parts.by_ref().take(3).all(|part| {
                                        !part.is_empty()
                                            && part.chars().all(|char| char.is_ascii_digit())
                                    }) && parts.next().is_none();
                                    valid.then(|| version.to_string())
                                })
                            })
                            .unwrap_or_else(|| CODEX_MODELS_CLIENT_VERSION_FALLBACK.to_string())
                    });
                    url.push_str(client_version);
                }
                let response = model_list_response(&provider, &url)?;
                let source = response["data"]
                    .as_array()
                    .or_else(|| response["models"].as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut models = Vec::new();
                for model in source {
                    if model["visibility"].as_str() == Some("hide") {
                        continue;
                    }
                    if let Some(id) = model["id"]
                        .as_str()
                        .or_else(|| model["slug"].as_str())
                        .or_else(|| model["name"].as_str())
                    {
                        let id = id.trim_start_matches("models/").to_string();
                        if !models.contains(&id) {
                            models.push(id);
                        }
                    }
                }
                models
            }
            _ => Vec::new(),
        },
        "anthropic" => {
            if provider.protocol == ModelProtocol::Anthropic {
                let mut models = Vec::new();
                let mut after_id: Option<String> = None;
                loop {
                    let mut url = url::Url::parse(&format!("{base_url}/models"))?;
                    url.query_pairs_mut().append_pair("limit", "1000");
                    if let Some(after_id) = &after_id {
                        url.query_pairs_mut().append_pair("after_id", after_id);
                    }
                    let response = model_list_response(&provider, url.as_str())?;
                    if let Some(data) = response["data"].as_array() {
                        for model in data {
                            if let Some(id) = model["id"].as_str() {
                                let id = id.to_string();
                                if !models.contains(&id) {
                                    models.push(id);
                                }
                            }
                        }
                    }
                    if response["has_more"].as_bool() != Some(true) {
                        break;
                    }
                    after_id = response["last_id"].as_str().map(str::to_string);
                    if after_id.is_none() {
                        break;
                    }
                }
                models
            } else {
                Vec::new()
            }
        }
        "gemini" => {
            if provider.protocol == ModelProtocol::Gemini {
                let mut models = Vec::new();
                let mut page_token: Option<String> = None;
                loop {
                    let mut url = url::Url::parse(&format!("{base_url}/models"))?;
                    url.query_pairs_mut().append_pair("pageSize", "1000");
                    if let Some(page_token) = &page_token {
                        url.query_pairs_mut().append_pair("pageToken", page_token);
                    }
                    let response = model_list_response(&provider, url.as_str())?;
                    if let Some(data) = response["models"].as_array() {
                        for model in data {
                            if let Some(methods) = model["supportedGenerationMethods"].as_array()
                                && !methods
                                    .iter()
                                    .any(|method| method.as_str() == Some("generateContent"))
                            {
                                continue;
                            }
                            if let Some(name) = model["name"].as_str() {
                                let id = name.trim_start_matches("models/").to_string();
                                if !models.contains(&id) {
                                    models.push(id);
                                }
                            }
                        }
                    }
                    page_token = response["nextPageToken"].as_str().map(str::to_string);
                    if page_token.is_none() {
                        break;
                    }
                }
                models
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    };

    for model in configured_models {
        if !models.contains(model) {
            models.push(model.clone());
        }
    }
    Ok(models)
}

