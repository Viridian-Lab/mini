use crate::Config;
use anyhow::{Context, Result};
use base64::Engine;
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

const OPENAI_CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
const OPENAI_CODEX_REDIRECT_PORT: u16 = 1455;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAuth {
    pub auth_mode: String,
    pub tokens: AuthTokens,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthTokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    account_id: Option<String>,
}

pub fn auth_status(app_dir_name: &str) -> Result<Option<StoredAuth>> {
    let Some(paths) = Config::app_paths(app_dir_name) else {
        return Ok(None);
    };
    let path = paths.root.join("auth.json");
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&std::fs::read_to_string(path)?)?))
}

pub fn logout(app_dir_name: &str) -> Result<()> {
    let Some(paths) = Config::app_paths(app_dir_name) else {
        return Ok(());
    };
    let path = paths.root.join("auth.json");
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub fn oauth_login(app_dir_name: &str, show_url: impl FnOnce(&str)) -> Result<StoredAuth> {
    let listener = TcpListener::bind(("127.0.0.1", OPENAI_CODEX_REDIRECT_PORT)).with_context(
        || format!("failed to start localhost OAuth callback listener on port {OPENAI_CODEX_REDIRECT_PORT}"),
    )?;
    let redirect_uri = format!("http://localhost:{OPENAI_CODEX_REDIRECT_PORT}/auth/callback");
    let state = random_token(32);
    let code_verifier = random_token(64);
    let code_challenge = base64_url(Sha256::digest(code_verifier.as_bytes()).as_ref());

    let mut authorize_url = Url::parse(OPENAI_AUTHORIZE_URL)?;
    authorize_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", OPENAI_CODEX_CLIENT_ID)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", OPENAI_SCOPE)
        .append_pair("code_challenge", &code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", &state)
        .append_pair("originator", "codex_cli_rs");

    let authorize_url = authorize_url.to_string().replace('+', "%20");
    show_url(&authorize_url);
    #[cfg(target_os = "macos")]
    let mut browser = {
        let mut command = Command::new("open");
        command.arg(&authorize_url);
        command
    };
    #[cfg(target_os = "windows")]
    let mut browser = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", "", &authorize_url]);
        command
    };
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let mut browser = {
        let mut command = Command::new("xdg-open");
        command.arg(&authorize_url);
        command
    };
    let _ = browser.spawn();

    let code = wait_for_callback(listener, &state)?;
    let token = token_request(
        &[
            ("grant_type", "authorization_code"),
            ("client_id", OPENAI_CODEX_CLIENT_ID),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &code_verifier),
        ],
        None,
        "OAuth token exchange",
    )?;
    let auth = StoredAuth {
        auth_mode: "codex-oauth".to_string(),
        tokens: token,
    };
    save_auth(app_dir_name, &auth)?;
    Ok(auth)
}

pub fn codex_auth(app_dir_name: &str) -> Result<Option<AuthTokens>> {
    let Some(mut auth) = auth_status(app_dir_name)? else {
        return Ok(None);
    };

    let mut changed = false;
    let expires_at = auth.tokens.expires_at.unwrap_or(0);
    if expires_at <= now_secs() + 60
        && let Some(refresh_token) = auth.tokens.refresh_token.clone()
    {
        auth.tokens = token_request(
            &[
                ("grant_type", "refresh_token"),
                ("client_id", OPENAI_CODEX_CLIENT_ID),
                ("refresh_token", &refresh_token),
            ],
            Some(refresh_token.clone()),
            "OAuth token refresh",
        )?;
        changed = true;
    }

    if auth.tokens.account_id.is_none() {
        auth.tokens.account_id = chatgpt_account_id(&auth.tokens.access_token);
        changed = changed || auth.tokens.account_id.is_some();
    }

    if changed {
        save_auth(app_dir_name, &auth)?;
    }

    Ok(Some(auth.tokens))
}

fn token_request(
    form: &[(&str, &str)],
    fallback_refresh_token: Option<String>,
    context: &str,
) -> Result<AuthTokens> {
    let response: TokenResponse = reqwest::blocking::Client::new()
        .post(OPENAI_TOKEN_URL)
        .form(form)
        .send()
        .with_context(|| format!("{context} failed"))?
        .error_for_status()
        .with_context(|| format!("{context} returned an error"))?
        .json()
        .with_context(|| format!("{context} returned invalid JSON"))?;
    let TokenResponse {
        access_token,
        refresh_token,
        id_token,
        token_type,
        expires_in,
        account_id,
    } = response;
    let account_id = account_id.or_else(|| chatgpt_account_id(&access_token));

    Ok(AuthTokens {
        access_token,
        refresh_token: refresh_token.or(fallback_refresh_token),
        id_token,
        token_type,
        expires_at: expires_in.map(|seconds| now_secs() + seconds),
        account_id,
    })
}

fn chatgpt_account_id(access_token: &str) -> Option<String> {
    let payload = access_token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .or_else(|| {
            base64::engine::general_purpose::URL_SAFE
                .decode(payload)
                .ok()
        })?;
    let payload: Value = serde_json::from_slice(&decoded).ok()?;
    payload["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .or_else(|| payload["account_id"].as_str())
        .map(str::to_string)
}

fn save_auth(app_dir_name: &str, auth: &StoredAuth) -> Result<()> {
    let paths = Config::ensure_app_files(app_dir_name)?.context("HOME is not set")?;
    let path = paths.root.join("auth.json");
    // Write to a sibling temp file and rename so a concurrent reader never
    // observes a truncated auth.json mid-write.
    let temp = paths.root.join("auth.json.tmp");
    std::fs::write(&temp, serde_json::to_string_pretty(auth)?)
        .with_context(|| format!("failed to write '{}'", temp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&temp, &path)
        .with_context(|| format!("failed to write '{}'", path.display()))?;
    Ok(())
}

fn wait_for_callback(listener: TcpListener, state: &str) -> Result<String> {
    // Non-blocking accept with a poll loop so the deadline is actually
    // enforced; a blocking accept() would hang forever if the user never
    // completes the browser flow.
    listener.set_nonblocking(true)?;
    let deadline = SystemTime::now() + Duration::from_secs(300);
    loop {
        if SystemTime::now() > deadline {
            anyhow::bail!("OAuth login timed out");
        }
        let (mut stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut request = [0; 8192];
        let read = stream.read(&mut request)?;
        let request = String::from_utf8_lossy(&request[..read]);
        let first_line = request.lines().next().unwrap_or_default();
        let Some(target) = first_line.split_whitespace().nth(1) else {
            continue;
        };
        let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
            continue;
        };
        if url.path() != "/auth/callback" {
            // Ignore favicon requests and other local probes instead of
            // failing the login on their missing state parameter.
            let _ = respond_status(&mut stream, "404 Not Found", "Not found.");
            continue;
        }
        let mut code = None;
        let mut returned_state = None;
        let mut error = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "code" => code = Some(value.into_owned()),
                "state" => returned_state = Some(value.into_owned()),
                "error" => error = Some(value.into_owned()),
                _ => {}
            }
        }

        if let Some(error) = error {
            respond(&mut stream, "Login failed. You can close this tab.")?;
            anyhow::bail!("OAuth login failed: {error}");
        }
        if returned_state.as_deref() != Some(state) {
            respond(
                &mut stream,
                "Login state did not match. You can close this tab.",
            )?;
            anyhow::bail!("OAuth login state did not match");
        }
        let code = code.context("OAuth callback did not include a code")?;
        respond(&mut stream, "Login complete. You can close this tab.")?;
        return Ok(code);
    }
}

fn respond(stream: &mut impl Write, body: &str) -> Result<()> {
    respond_status(stream, "200 OK", body)
}

fn respond_status(stream: &mut impl Write, status: &str, body: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    Ok(())
}

fn random_token(bytes: usize) -> String {
    let mut token = vec![0; bytes];
    OsRng.fill_bytes(&mut token);
    base64_url(&token)
}

fn base64_url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
