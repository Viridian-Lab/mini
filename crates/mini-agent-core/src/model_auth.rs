impl Provider {
    pub fn endpoint(&self, model: &str) -> String {
        let base_url = self
            .base_url
            .as_deref()
            .unwrap_or_else(|| self.protocol.default_base_url())
            .trim_end_matches('/');

        match self.protocol {
            ModelProtocol::OpenAiResponses => format!("{base_url}/responses"),
            ModelProtocol::OpenAiChatCompletions => format!("{base_url}/chat/completions"),
            ModelProtocol::Anthropic => format!("{base_url}/messages"),
            ModelProtocol::Gemini => format!("{base_url}/models/{model}:generateContent"),
        }
    }

    pub fn headers(
        &self,
        token: Option<&str>,
        chatgpt_account_id: Option<&str>,
    ) -> Vec<(&'static str, String)> {
        let mut headers = Vec::new();
        match self.protocol {
            ModelProtocol::OpenAiResponses | ModelProtocol::OpenAiChatCompletions => {
                if let Some(token) = token {
                    headers.push(("Authorization", format!("Bearer {token}")));
                }
                if self.auth == ModelAuth::CodexOauth
                    && let Some(account_id) = chatgpt_account_id
                {
                    headers.push(("ChatGPT-Account-Id", account_id.to_string()));
                }
            }
            ModelProtocol::Anthropic => {
                if let Some(token) = token {
                    headers.push(("x-api-key", token.to_string()));
                }
                headers.push(("anthropic-version", "2023-06-01".to_string()));
            }
            ModelProtocol::Gemini => {
                if let Some(token) = token {
                    headers.push(("x-goog-api-key", token.to_string()));
                }
            }
        }
        headers
    }
}

pub fn auth_token(config: &Config, provider: &Provider) -> Result<(Option<String>, Option<String>)> {
    match provider.auth {
        ModelAuth::ApiKey => {
            let api_key_env = provider
                .api_key_env
                .as_deref()
                .unwrap_or_else(|| provider.protocol.default_api_key_env());
            if let Ok(api_key) = std::env::var(api_key_env)
                && !api_key.trim().is_empty()
            {
                return Ok((Some(api_key), None));
            }
            anyhow::bail!("missing API key environment variable {api_key_env}")
        }
        ModelAuth::CodexOauth => {
            let auth_app_dir_name = config
                .auth_app_dir_name
                .as_deref()
                .unwrap_or(&config.app_dir_name);
            let auth = codex_auth(auth_app_dir_name)?.with_context(|| {
                format!("missing Codex OAuth token in '{auth_app_dir_name}'")
            })?;
            let account_id = auth.account_id.with_context(|| {
                format!(
                    "Codex OAuth token in '{}' did not include a ChatGPT account id",
                    auth_app_dir_name
                )
            })?;
            Ok((Some(auth.access_token), Some(account_id)))
        }
        ModelAuth::None => Ok((None, None)),
    }
}

fn model_list_response(config: &Config, provider: &Provider, url: &str) -> Result<Value> {
    let (token, chatgpt_account_id) = auth_token(config, provider)?;
    let client = reqwest::blocking::Client::new();
    let mut builder = client.get(url);
    for (name, value) in provider.headers(token.as_deref(), chatgpt_account_id.as_deref()) {
        builder = builder.header(name, value);
    }

    let response = builder.send().context("model list request failed")?;
    let status = response.status();
    if !status.is_success() {
        let response_text = response
            .text()
            .context("failed to read model list response")?;
        anyhow::bail!("model list request failed with status {status}: {response_text}");
    }

    response
        .json()
        .context("model list response was not valid JSON")
}

impl ModelProtocol {
    pub fn default_base_url(&self) -> &'static str {
        match self {
            Self::OpenAiResponses | Self::OpenAiChatCompletions => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com/v1",
            Self::Gemini => "https://generativelanguage.googleapis.com/v1beta",
        }
    }

    pub fn default_api_key_env(&self) -> &'static str {
        match self {
            Self::OpenAiResponses | Self::OpenAiChatCompletions => "OPENAI_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Gemini => "GEMINI_API_KEY",
        }
    }
}
