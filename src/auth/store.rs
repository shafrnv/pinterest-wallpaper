use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
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
    /// Pinterest `_auth` cookie value
    pub auth: String,
    /// Pinterest `csrftoken` cookie value
    pub csrftoken: String,
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

    pub fn prompt_session_login() -> Result<SessionCookies> {
        println!("Pinterest session login");
        println!("1. Open https://www.pinterest.com and sign in");
        println!("2. Open DevTools → Application/Storage → Cookies → www.pinterest.com");
        println!("3. Copy the values of `_auth` and `csrftoken`\n");

        let auth = prompt("Paste `_auth` cookie: ")?;
        let csrftoken = prompt("Paste `csrftoken` cookie: ")?;

        if auth.is_empty() || csrftoken.is_empty() {
            anyhow::bail!("both cookies are required");
        }

        Ok(SessionCookies { auth, csrftoken })
    }
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}
