use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use super::feed::PinItem;

const API: &str = "https://api.pinterest.com/v5";

#[derive(Debug, Deserialize)]
struct ListResponse {
    items: Vec<Value>,
    bookmark: Option<String>,
}

#[derive(Clone)]
pub struct BoardsClient {
    client: reqwest::Client,
    access_token: String,
}

impl BoardsClient {
    pub fn new(access_token: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("pinterest-wallpaper/0.1")
            .build()?;
        Ok(Self {
            client,
            access_token,
        })
    }

    async fn get_list(&self, path: &str, bookmark: Option<&str>) -> Result<ListResponse> {
        let mut url = format!("{API}{path}");
        if let Some(b) = bookmark {
            url.push_str(&format!(
                "{}bookmark={}",
                if path.contains('?') { "&" } else { "?" },
                urlencoding::encode(b)
            ));
        }
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("status {url}"))?;
        Ok(resp.json().await?)
    }

    pub async fn list_board_ids(&self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let mut bookmark = None;
        loop {
            let page = self.get_list("/boards?page_size=25", bookmark.as_deref()).await?;
            for item in page.items {
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    ids.push(id.to_string());
                }
            }
            match page.bookmark {
                Some(b) if !b.is_empty() => bookmark = Some(b),
                _ => break,
            }
        }
        Ok(ids)
    }

    pub async fn board_pins_page(
        &self,
        board_id: &str,
        bookmark: Option<&str>,
    ) -> Result<(Vec<PinItem>, Option<String>)> {
        let path = format!("/boards/{board_id}/pins?page_size=25");
        let page = self.get_list(&path, bookmark).await?;
        let pins = page
            .items
            .into_iter()
            .filter_map(pin_from_api)
            .collect();
        Ok((pins, page.bookmark.filter(|b| !b.is_empty())))
    }
}

fn pin_from_api(v: Value) -> Option<PinItem> {
    let id = v.get("id")?.as_str()?.to_string();
    let media = v.get("media")?;
    let images = media.get("images")?;
    // Prefer larger originals
    let image_url = ["orig", "1200x", "600x", "400x300"]
        .iter()
        .find_map(|k| {
            images
                .get(*k)
                .and_then(|i| i.get("url"))
                .and_then(|u| u.as_str())
                .map(|s| s.to_string())
        })?;

    let width = images
        .get("orig")
        .or_else(|| images.get("1200x"))
        .and_then(|i| i.get("width"))
        .and_then(|w| w.as_u64())
        .unwrap_or(600) as u32;
    let height = images
        .get("orig")
        .or_else(|| images.get("1200x"))
        .and_then(|i| i.get("height"))
        .and_then(|h| h.as_u64())
        .unwrap_or(800) as u32;

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
