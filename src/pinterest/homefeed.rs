use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::auth::SessionCookies;

use super::feed::PinItem;

/// Unofficial web resource used by pinterest.com for the home feed.
/// May break when Pinterest changes their frontend.
const HOMEFEED_URL: &str =
    "https://www.pinterest.com/resource/UserHomefeedResource/get/";

#[derive(Clone)]
pub struct HomefeedClient {
    client: reqwest::Client,
    cookies: SessionCookies,
}

impl HomefeedClient {
    pub fn new(cookies: SessionCookies) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36",
            )
            .build()?;
        Ok(Self { client, cookies })
    }

    pub async fn page(&self, bookmarks: Option<&str>) -> Result<(Vec<PinItem>, Option<String>)> {
        let options = json!({
            "isPrefetch": false,
            "field_set_key": "hf_grid",
            "in_nux": false,
            "bookmarks": bookmarks.map(|b| vec![b]).unwrap_or_default(),
        });
        let data = json!({
            "options": options,
            "context": {}
        });

        let url = format!(
            "{HOMEFEED_URL}?source_url={}&data={}",
            urlencoding::encode("/"),
            urlencoding::encode(&data.to_string())
        );

        let resp = self
            .client
            .get(&url)
            .header(
                "Cookie",
                format!(
                    "_auth={}; csrftoken={}",
                    self.cookies.auth, self.cookies.csrftoken
                ),
            )
            .header("X-CSRFToken", &self.cookies.csrftoken)
            .header("X-Requested-With", "XMLHttpRequest")
            .header("Accept", "application/json, text/javascript, */*; q=0.01")
            .header("Referer", "https://www.pinterest.com/")
            .send()
            .await
            .context("homefeed request")?;

        let status = resp.status();
        let body = resp.text().await?;
        if status.as_u16() == 401 || status.as_u16() == 403 {
            bail!("session expired or invalid — run `login-session` again");
        }
        if !status.is_success() {
            bail!("homefeed HTTP {status}: {}", truncate(&body, 200));
        }

        let v: Value = serde_json::from_str(&body).context("homefeed JSON")?;
        let resource = v
            .pointer("/resource_response")
            .cloned()
            .or_else(|| v.get("resource_response").cloned())
            .unwrap_or(v);

        if let Some(err) = resource.get("error").filter(|e| !e.is_null()) {
            bail!("homefeed API error: {err}");
        }

        let data = resource.get("data").cloned().unwrap_or(Value::Null);
        let pins = extract_pins(&data);
        let next = resource
            .pointer("/bookmark")
            .or_else(|| resource.pointer("/data/bookmark"))
            .and_then(|b| b.as_str())
            .filter(|s| !s.is_empty() && *s != "-end-")
            .map(|s| s.to_string())
            .or_else(|| find_bookmark(&resource));

        Ok((pins, next))
    }
}

fn extract_pins(data: &Value) -> Vec<PinItem> {
    let mut out = Vec::new();
    collect_pins(data, &mut out);
    out
}

fn collect_pins(v: &Value, out: &mut Vec<PinItem>) {
    match v {
        Value::Array(arr) => {
            for item in arr {
                if let Some(pin) = try_pin(item) {
                    out.push(pin);
                } else {
                    collect_pins(item, out);
                }
            }
        }
        Value::Object(map) => {
            if let Some(pin) = try_pin(v) {
                out.push(pin);
                return;
            }
            // Common wrappers: objects with "type":"pin", or nested "pins"/"data"
            for (k, child) in map {
                if matches!(k.as_str(), "pins" | "data" | "results" | "objects" | "feed")
                    || child.is_array()
                {
                    collect_pins(child, out);
                }
            }
        }
        _ => {}
    }
}

fn try_pin(v: &Value) -> Option<PinItem> {
    let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if !typ.is_empty() && typ != "pin" {
        return None;
    }

    let id = v
        .get("id")
        .or_else(|| v.get("pin_id"))
        .and_then(|x| x.as_str().map(|s| s.to_string()).or_else(|| x.as_u64().map(|n| n.to_string())))?;

    let images = v
        .get("images")
        .or_else(|| v.pointer("/image"))
        .or_else(|| v.pointer("/data/images"))?;

    let image_url = ["orig", "1200x", "736x", "564x", "474x", "236x"]
        .iter()
        .find_map(|k| {
            images
                .get(*k)
                .and_then(|i| i.get("url"))
                .and_then(|u| u.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            images
                .get("url")
                .and_then(|u| u.as_str())
                .map(|s| s.to_string())
        })?;

    let (width, height) = ["orig", "1200x", "736x", "564x"]
        .iter()
        .find_map(|k| {
            let i = images.get(*k)?;
            Some((
                i.get("width")?.as_u64()? as u32,
                i.get("height")?.as_u64()? as u32,
            ))
        })
        .unwrap_or((600, 800));

    if width == 0 || height == 0 {
        return None;
    }

    let link = v
        .get("link")
        .and_then(|l| l.as_str())
        .map(|s| s.to_string())
        .or_else(|| Some(format!("https://www.pinterest.com/pin/{id}/")));

    Some(PinItem {
        id,
        image_url,
        width,
        height,
        link,
    })
}

fn find_bookmark(v: &Value) -> Option<String> {
    match v {
        Value::Object(m) => {
            if let Some(b) = m.get("bookmark").and_then(|x| x.as_str()) {
                if !b.is_empty() && b != "-end-" {
                    return Some(b.to_string());
                }
            }
            for child in m.values() {
                if let Some(b) = find_bookmark(child) {
                    return Some(b);
                }
            }
            None
        }
        Value::Array(a) => a.iter().find_map(find_bookmark),
        _ => None,
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}
