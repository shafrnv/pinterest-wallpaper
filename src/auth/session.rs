use aes::Aes128;
use anyhow::{bail, Result};
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use cbc::Decryptor;
use pbkdf2::pbkdf2_hmac;
use rusqlite::Connection;
use sha1::Sha1;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use super::store::SessionCookies;

const LOGIN_URL: &str = "https://www.pinterest.com/login/";

/// Login for any account, including one that does not exist yet.
///
/// Google sign-in does not open inside an embedded WebKit window, so this
/// opens the normal browser (where Google and signup both work) and waits
/// until that browser has a Pinterest session.
pub fn login_with_browser() -> Result<SessionCookies> {
    if let Some(session) = find_session() {
        println!("В браузере уже есть вход Pinterest, забираю сессию.");
        return Ok(session);
    }

    println!("Откроется обычный браузер.");
    println!("Войди или зарегистрируйся — через Google тоже. Cookie подхватятся сами.");
    open::that(LOGIN_URL)?;

    let started = Instant::now();
    loop {
        if let Some(session) = find_session() {
            println!("Сессия появилась.");
            return Ok(session);
        }
        if started.elapsed() > Duration::from_secs(5 * 60) {
            bail!("не дождался входа. Закончи регистрацию в браузере и запусти login-session ещё раз");
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn find_session() -> Option<SessionCookies> {
    let mut cookies = Vec::new();
    for root in firefox_roots() {
        for profile in firefox_profiles(&root) {
            let db = profile.join("cookies.sqlite");
            if db.exists() {
                match read_firefox(&db) {
                    Ok(rows) => cookies.extend(rows),
                    Err(err) => tracing::debug!("firefox cookies {}: {err:#}", db.display()),
                }
            }
        }
    }
    for db in chrome_dbs() {
        match read_chrome(&db) {
            Ok(rows) => cookies.extend(rows),
            Err(err) => tracing::debug!("chrome cookies {}: {err:#}", db.display()),
        }
    }
    session_from_rows(&cookies)
}

fn session_from_rows(rows: &[(String, String, String)]) -> Option<SessionCookies> {
    let mut best: BTreeMap<String, (u8, String)> = BTreeMap::new();
    for (host, name, value) in rows {
        if value.is_empty() {
            continue;
        }
        let rank = host_rank(host);
        if rank == u8::MAX {
            continue;
        }
        best.entry(name.clone())
            .and_modify(|slot| {
                if rank < slot.0 {
                    *slot = (rank, value.clone());
                }
            })
            .or_insert((rank, value.clone()));
    }
    if best.is_empty() {
        return None;
    }

    let auth = best.get("_auth").map(|(_, v)| v.clone()).unwrap_or_default();
    let csrftoken = best
        .get("csrftoken")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let pinterest_sess = best
        .get("_pinterest_sess")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let cookie_header = best
        .iter()
        .map(|(name, (_, value))| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");

    let session = SessionCookies {
        auth,
        csrftoken,
        pinterest_sess,
        cookie_header,
    };
    session.is_logged_in().then_some(session)
}

fn host_rank(host: &str) -> u8 {
    match host {
        "www.pinterest.com" => 0,
        ".pinterest.com" => 1,
        "pinterest.com" => 2,
        _ => u8::MAX,
    }
}

fn firefox_roots() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    [
        ".zen",
        ".mozilla/firefox",
        ".librewolf",
        ".floorp",
        ".var/app/app.zen_browser.zen",
        ".var/app/org.mozilla.firefox/.mozilla/firefox",
    ]
    .into_iter()
    .map(|rel| home.join(rel))
    .filter(|path| path.join("profiles.ini").is_file())
    .collect()
}

fn firefox_profiles(root: &Path) -> Vec<PathBuf> {
    let Ok(text) = fs::read_to_string(root.join("profiles.ini")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut path: Option<String> = None;
    let mut relative = true;
    let flush = |path: &mut Option<String>, relative: bool, out: &mut Vec<PathBuf>| {
        if let Some(path) = path.take() {
            let full = if relative {
                root.join(path)
            } else {
                PathBuf::from(path)
            };
            if full.is_dir() {
                out.push(full);
            }
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            flush(&mut path, relative, &mut out);
            relative = true;
            continue;
        }
        if let Some(value) = line.strip_prefix("Path=") {
            path = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("IsRelative=") {
            relative = value != "0";
        }
    }
    flush(&mut path, relative, &mut out);
    out
}

fn chrome_dbs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let roots = [
        home.join(".config/google-chrome"),
        home.join(".config/chromium"),
        home.join(".config/BraveSoftware/Brave-Browser"),
    ];
    let mut dbs = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let cookies = entry.path().join("Cookies");
            if cookies.is_file() {
                dbs.push(cookies);
            }
        }
    }
    dbs
}

fn read_firefox(db: &Path) -> Result<Vec<(String, String, String)>> {
    let tmp = snapshot_db(db)?;
    let conn = Connection::open(&tmp.main)?;
    let mut stmt = conn.prepare(
        "SELECT host, name, value FROM moz_cookies WHERE host LIKE '%pinterest%'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn read_chrome(db: &Path) -> Result<Vec<(String, String, String)>> {
    let password = chrome_password(db);
    let tmp = snapshot_db(db)?;
    let conn = Connection::open(&tmp.main)?;
    let mut stmt = conn.prepare(
        "SELECT host_key, name, value, encrypted_value FROM cookies WHERE host_key LIKE '%pinterest%'",
    )?;
    let rows = stmt.query_map([], |row| {
        let host: String = row.get(0)?;
        let name: String = row.get(1)?;
        let value: String = row.get(2)?;
        let encrypted: Vec<u8> = row.get(3)?;
        Ok((host, name, value, encrypted))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (host, name, value, encrypted) = row?;
        let value = if !value.is_empty() {
            value
        } else {
            decrypt_chrome(&encrypted, password.as_deref()).unwrap_or_default()
        };
        if !value.is_empty() {
            out.push((host, name, value));
        }
    }
    Ok(out)
}

struct DbSnapshot {
    main: PathBuf,
}

fn snapshot_db(db: &Path) -> Result<DbSnapshot> {
    let main = std::env::temp_dir().join(format!(
        "pinterest-wallpaper-cookies-{}-{}",
        std::process::id(),
        db.file_name().and_then(|n| n.to_str()).unwrap_or("db")
    ));
    fs::copy(db, &main)?;
    for ext in ["-wal", "-shm"] {
        let src = PathBuf::from(format!("{}{ext}", db.display()));
        if src.is_file() {
            let dst = PathBuf::from(format!("{}{ext}", main.display()));
            let _ = fs::copy(src, dst);
        }
    }
    Ok(DbSnapshot { main })
}

fn chrome_password(db: &Path) -> Option<String> {
    let application = if db.display().to_string().contains("chromium") {
        "chromium"
    } else if db.display().to_string().contains("Brave") {
        "brave"
    } else {
        "chrome"
    };
    let output = Command::new("secret-tool")
        .args(["lookup", "application", application])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let password = String::from_utf8(output.stdout).ok()?;
    let password = password.trim().to_string();
    (!password.is_empty()).then_some(password)
}

fn decrypt_chrome(encrypted: &[u8], password: Option<&str>) -> Option<String> {
    let password = password?;
    if encrypted.len() < 19 || !(encrypted.starts_with(b"v10") || encrypted.starts_with(b"v11")) {
        return None;
    }
    let mut key = [0u8; 16];
    pbkdf2_hmac::<Sha1>(password.as_bytes(), b"saltysalt", 1, &mut key);
    let iv = [b' '; 16];
    let plain = Decryptor::<Aes128>::new(&key.into(), &iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(&encrypted[3..])
        .ok()?;
    String::from_utf8(plain).ok()
}
