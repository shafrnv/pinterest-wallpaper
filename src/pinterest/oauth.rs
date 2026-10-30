use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

use crate::auth::{AuthStore, OauthTokens};
use crate::config::OauthConfig;

const AUTH_URL: &str = "https://www.pinterest.com/oauth/";
const TOKEN_URL: &str = "https://api.pinterest.com/v5/oauth/token";
const SCOPES: &str = "boards:read,pins:read,user_accounts:read";

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
}

pub fn login(oauth: &OauthConfig) -> Result<OauthTokens> {
    let redirect = Url::parse(&oauth.redirect_uri).context("redirect_uri")?;
    let host = redirect.host_str().unwrap_or("127.0.0.1");
    let port = redirect.port().unwrap_or(8765);
    let path = redirect.path();

    let state = random_state();
    let mut auth = Url::parse(AUTH_URL)?;
    {
        let mut q = auth.query_pairs_mut();
        q.append_pair("client_id", &oauth.client_id);
        q.append_pair("redirect_uri", &oauth.redirect_uri);
        q.append_pair("response_type", "code");
        q.append_pair("scope", SCOPES);
        q.append_pair("state", &state);
    }

    let listener = TcpListener::bind((host, port))
        .with_context(|| format!("bind {host}:{port} for OAuth callback"))?;
    println!("Opening browser for Pinterest OAuth…");
    println!("If it does not open, visit:\n{}\n", auth);
    let _ = open::that(auth.as_str());

    let (tx, rx) = mpsc::channel::<Result<String>>();
    let expected_path = path.to_string();
    let expected_state = state.clone();

    thread::spawn(move || {
        let result = (|| {
            let (mut stream, _) = listener.accept()?;
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf)?;
            let req = String::from_utf8_lossy(&buf[..n]);
            let first_line = req.lines().next().unwrap_or("");
            // GET /callback?code=...&state=... HTTP/1.1
            let target = first_line.split_whitespace().nth(1).unwrap_or("/");
            let full = format!("http://127.0.0.1{target}");
            let u = Url::parse(&full)?;
            if u.path() != expected_path {
                bail!("unexpected callback path {}", u.path());
            }
            let code = u
                .query_pairs()
                .find(|(k, _)| k == "code")
                .map(|(_, v)| v.to_string())
                .context("missing code in callback")?;
            let got_state = u
                .query_pairs()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.to_string());
            if got_state.as_deref() != Some(expected_state.as_str()) {
                bail!("state mismatch");
            }

            let body = b"<!doctype html><html><body><h1>Logged in</h1><p>You can close this tab.</p></body></html>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(resp.as_bytes())?;
            stream.write_all(body)?;
            Ok(code)
        })();
        let _ = tx.send(result);
    });

    let code = rx
        .recv_timeout(std::time::Duration::from_secs(300))
        .context("OAuth timed out (5 min)")??;

    exchange_code(oauth, &code)
}

fn exchange_code(oauth: &OauthConfig, code: &str) -> Result<OauthTokens> {
    let client = reqwest::blocking::Client::new();
    let basic = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("{}:{}", oauth.client_id, oauth.client_secret),
    );

    let resp = client
        .post(TOKEN_URL)
        .header("Authorization", format!("Basic {basic}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", oauth.redirect_uri.as_str()),
            ("continuous_refresh", "true"),
        ])
        .send()
        .context("token exchange")?
        .error_for_status()
        .context("token exchange status")?;

    let token: TokenResponse = resp.json()?;
    let expires_at = token.expires_in.map(|secs| now_epoch() + secs);

    Ok(OauthTokens {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at,
        scope: token.scope,
    })
}

pub fn refresh_if_needed(oauth: &OauthConfig, store: &mut AuthStore) -> Result<()> {
    let Some(tokens) = store.oauth.as_ref() else {
        bail!("not logged in via OAuth — run `login-oauth`");
    };

    let needs_refresh = match tokens.expires_at {
        Some(exp) => now_epoch() >= exp - 60,
        None => false,
    };

    if !needs_refresh {
        return Ok(());
    }

    let refresh = tokens
        .refresh_token
        .clone()
        .context("access token expired and no refresh_token")?;

    let client = reqwest::blocking::Client::new();
    let basic = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("{}:{}", oauth.client_id, oauth.client_secret),
    );

    let resp = client
        .post(TOKEN_URL)
        .header("Authorization", format!("Basic {basic}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("continuous_refresh", "true"),
        ])
        .send()?
        .error_for_status()?;

    let token: TokenResponse = resp.json()?;
    store.oauth = Some(OauthTokens {
        access_token: token.access_token,
        refresh_token: token.refresh_token.or(Some(refresh)),
        expires_at: token.expires_in.map(|s| now_epoch() + s),
        scope: token.scope,
    });
    store.save()?;
    Ok(())
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn random_state() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 16];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}
