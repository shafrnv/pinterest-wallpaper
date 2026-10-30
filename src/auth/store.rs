use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use crate::config::Config;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OauthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionCookies {
    /// Pinterest `_auth` cookie. Logged-in accounts often store just `1` here.
    pub auth: String,
    /// Pinterest `csrftoken` cookie value
    pub csrftoken: String,
    /// Real session token. `_auth=1` without this is not a login.
    #[serde(default)]
    pub pinterest_sess: String,
    /// Cookie header as the browser would send it to www.pinterest.com.
    #[serde(default)]
    pub cookie_header: String,
}

impl SessionCookies {
    pub fn header(&self) -> String {
        if !self.cookie_header.is_empty() {
            return self.cookie_header.clone();
        }
        let mut parts = vec![
            format!("_auth={}", self.auth),
            format!("csrftoken={}", self.csrftoken),
        ];
        if !self.pinterest_sess.is_empty() {
            parts.push(format!("_pinterest_sess={}", self.pinterest_sess));
        }
        parts.join("; ")
    }

    pub fn is_logged_in(&self) -> bool {
        self.csrftoken.len() >= 8
            && (self.pinterest_sess.len() > 20 || self.auth.len() > 8)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthStore {
    pub oauth: Option<OauthTokens>,
    pub session: Option<SessionCookies>,
}

impl AuthStore {
    fn path() -> Result<PathBuf> {
        Ok(Config::config_dir()?.join("auth.json"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path)?;
        let store: AuthStore = serde_json::from_str(&text).context("parse auth.json")?;
        Ok(store)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let text = serde_json::to_string_pretty(self)?;
        fs::write(&path, &text)?;
        let mut perms = fs::metadata(&path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&path, perms)?;
        Ok(())
    }

    pub fn clear_oauth(&mut self) -> Result<()> {
        self.oauth = None;
        self.save()
    }

    pub fn clear_session(&mut self) -> Result<()> {
        self.session = None;
        self.save()
    }

    pub fn clear_all(&mut self) -> Result<()> {
        self.oauth = None;
        self.session = None;
        self.save()
    }
}
