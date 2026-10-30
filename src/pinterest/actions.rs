use anyhow::{bail, Context, Result};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;

use crate::auth::{AuthStore, SessionCookies};

#[derive(Clone, Debug)]
pub struct BoardDest {
    pub id: String,
    pub name: String,
    /// Small cover, when the board picker sent one.
    pub cover: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct SaveCatalog {
    /// Board Pinterest would use for the one-click Save button.
    pub quick: Option<BoardDest>,
    pub boards: Vec<BoardDest>,
}

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(UA)
        .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
        .dns_resolver(Arc::new(Ipv4Prefer))
        .timeout(std::time::Duration::from_secs(20))
        .build()?)
}

struct Ipv4Prefer;

impl Resolve for Ipv4Prefer {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_string();
            let looked = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|err| -> Box<dyn std::error::Error + Send + Sync> { Box::new(err) })?;
            let mut v4 = Vec::new();
            let mut v6 = Vec::new();
            for addr in looked {
                if addr.is_ipv4() {
                    v4.push(addr);
                } else {
                    v6.push(addr);
                }
            }
            let addrs: Vec<SocketAddr> = if v4.is_empty() { v6 } else { v4 };
            if addrs.is_empty() {
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no addresses").into());
            }
            let iter: Addrs = Box::new(addrs.into_iter());
            Ok(iter)
        })
    }
}

fn session() -> Result<SessionCookies> {
    let store = AuthStore::load()?;
    store
        .session
        .filter(|s| s.is_logged_in())
        .context("нет сессии Pinterest — выполните login-session")
}

pub async fn load_catalog() -> Result<SaveCatalog> {
    let cookies = session()?;
    let http = client()?;
    let data = json!({
        "options": {"filter": "all", "field_set_key": "board_picker"},
        "context": {}
    });
    let url = format!(
        "https://www.pinterest.com/resource/BoardPickerBoardsResource/get/?source_url={}&data={}",
        urlencoding::encode("https://www.pinterest.com/"),
        urlencoding::encode(&data.to_string())
    );
    let body = http
        .get(url)
        .headers(headers(&cookies, "/"))
        .send()
        .await
        .context("список досок")?
        .text()
        .await?;
    let v: Value = serde_json::from_str(&body).context("доски JSON")?;
    check_error(&v)?;
    let data = v
        .pointer("/resource_response/data")
        .cloned()
        .unwrap_or(Value::Null);
    let boards = parse_boards(data.get("all_boards"));
    let quick = parse_boards(data.get("boards_shortlist"))
        .into_iter()
        .next()
        .or_else(|| boards.first().cloned());
    Ok(SaveCatalog { quick, boards })
}

/// `board == None` saves onto the profile (Pinterest files that under Quick saves).
pub async fn save_pin(pin_id: &str, board: Option<&BoardDest>) -> Result<String> {
    let cookies = session()?;
    let http = client()?;
    let mut options = serde_json::Map::new();
    options.insert("pin_id".into(), json!(pin_id));
    if let Some(board) = board {
        options.insert("board_id".into(), json!(board.id));
    }
    let data = json!({"options": options, "context": {}});
    let source = format!("/pin/{pin_id}/");
    let form = format!(
        "source_url={}&data={}",
        urlencoding::encode(&source),
        urlencoding::encode(&data.to_string())
    );
    let resp = http
        .post("https://www.pinterest.com/resource/RepinResource/create/")
        .headers(headers(&cookies, &source))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Origin", "https://www.pinterest.com")
        .body(form)
        .send()
        .await
        .context("сохранение пина")?;
    let status = resp.status();
    let body = resp.text().await?;
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if let Err(e) = check_error(&v) {
        return Err(e);
    }
    if !status.is_success() {
        bail!("Pinterest ответил {status}");
    }
    let label = board
        .map(|b| format!("Сохранено на «{}»", b.name))
        .unwrap_or_else(|| "Сохранено в профиль".to_string());
    Ok(label)
}

fn headers(cookies: &SessionCookies, source: &str) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderValue};
    let mut map = HeaderMap::new();
    map.insert(
        "Cookie",
        HeaderValue::from_str(&cookies.header()).unwrap_or(HeaderValue::from_static("")),
    );
    if let Ok(v) = HeaderValue::from_str(&cookies.csrftoken) {
        map.insert("X-CSRFToken", v);
    }
    map.insert(
        "X-Requested-With",
        HeaderValue::from_static("XMLHttpRequest"),
    );
    map.insert("X-Pinterest-AppState", HeaderValue::from_static("active"));
    if let Ok(v) = HeaderValue::from_str(source) {
        map.insert("X-Pinterest-Source-Url", v);
    }
    map.insert(
        "X-Pinterest-PWS-Handler",
        HeaderValue::from_static("www/index.js"),
    );
    map.insert(
        "Accept",
        HeaderValue::from_static("application/json, text/javascript, */*; q=0.01"),
    );
    map.insert(
        "Referer",
        HeaderValue::from_static("https://www.pinterest.com/"),
    );
    map
}

fn check_error(v: &Value) -> Result<()> {
    match v.pointer("/resource_response/error") {
        Some(Value::Object(map)) => {
            let msg = map
                .get("message")
                .and_then(|m| m.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("не удалось сохранить");
            bail!("{msg}");
        }
        _ => Ok(()),
    }
}

fn parse_boards(v: Option<&Value>) -> Vec<BoardDest> {
    let Some(Value::Array(items)) = v else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let id = item
            .get("id")
            .and_then(|v| {
                v.as_str()
                    .map(|s| s.to_string())
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            })
            .unwrap_or_default();
        let name = item
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if id.is_empty() || name.is_empty() {
            continue;
        }
        if out.iter().any(|b: &BoardDest| b.id == id) {
            continue;
        }
        out.push(BoardDest {
            id,
            name,
            cover: cover_url(item),
        });
    }
    out
}

fn cover_url(item: &Value) -> Option<String> {
    for key in ["image_cover_url", "image_thumbnail_url", "image_cover_hd_url"] {
        if let Some(url) = http_url(item.get(key)) {
            return Some(url);
        }
    }
    for path in [
        "/images/236x/url",
        "/images/170x/url",
        "/cover_pin/images/236x/url",
        "/cover_pin/images/170x/url",
    ] {
        if let Some(url) = http_url(item.pointer(path)) {
            return Some(url);
        }
    }
    None
}

fn http_url(v: Option<&Value>) -> Option<String> {
    let raw = v?.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    let url = if let Some(rest) = raw.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        raw.to_string()
    };
    url.starts_with("http").then_some(url)
}

pub async fn create_board(name: &str) -> Result<BoardDest> {
    let name = name.trim();
    if name.is_empty() {
        bail!("введите название");
    }
    let cookies = session()?;
    let http = client()?;
    let data = json!({
        "options": {
            "name": name,
            "privacy": "public"
        },
        "context": {}
    });
    let form = format!(
        "source_url={}&data={}",
        urlencoding::encode("/"),
        urlencoding::encode(&data.to_string())
    );
    let resp = http
        .post("https://www.pinterest.com/resource/BoardResource/create/")
        .headers(headers(&cookies, "/"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Origin", "https://www.pinterest.com")
        .body(form)
        .send()
        .await
        .context("создание доски")?;
    let status = resp.status();
    let body = resp.text().await?;
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    check_error(&v)?;
    if !status.is_success() {
        bail!("Pinterest ответил {status}");
    }
    let created = v.pointer("/resource_response/data").unwrap_or(&Value::Null);
    let parsed = parse_boards(Some(&Value::Array(vec![created.clone()])));
    Ok(parsed.into_iter().next().unwrap_or(BoardDest {
        id: String::new(),
        name: name.to_string(),
        cover: None,
    }))
}

#[derive(Clone, Debug)]
pub struct PinComment {
    pub id: String,
    pub author: String,
    pub text: String,
    pub when: String,
    pub likes: i64,
}

#[derive(Clone, Debug)]
pub struct SharePerson {
    pub id: String,
    pub name: String,
    pub handle: String,
}

#[derive(Clone, Debug)]
pub struct CollageDest {
    pub id: String,
    pub name: String,
}

/// Heart on the pin. The site posts through `ApiResource` to
/// `/v3/pins/{id}/react/` with reaction type 1 (love). The old
/// `ReactionsResource` path answers 404.
pub async fn set_liked(pin_id: &str, on: bool) -> Result<()> {
    let source = format!("/pin/{pin_id}/");
    let url = format!("/v3/pins/{pin_id}/react/");
    let options = if on {
        json!({"url": url, "data": {"reaction_type": 1}})
    } else {
        json!({"url": url, "data": {}})
    };
    let verb = if on { "create" } else { "delete" };
    submit(&format!("ApiResource/{verb}/"), &source, options)
        .await
        .map(|_| ())
}

pub async fn load_comments(pin_id: &str) -> Result<(String, Vec<PinComment>)> {
    let source = format!("/pin/{pin_id}/");
    let mut object_id = pin_id.to_string();
    if let Ok(pin) = query(
        "PinResource/get/",
        &source,
        json!({"id": pin_id, "field_set_key": "detailed"}),
    )
    .await
    {
        if let Some(id) = pin
            .pointer("/resource_response/data/aggregated_pin_data/id")
            .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| v.as_u64().map(|n| n.to_string())))
        {
            object_id = id;
        }
        let found = parse_comments(&pin);
        if !found.is_empty() {
            return Ok((object_id, found));
        }
    }
    let feed = query(
        "AggregatedCommentFeedResource/get/",
        &source,
        json!({"objectId": object_id, "page_size": 30, "isReversed": false}),
    )
    .await?;
    Ok((object_id, parse_comments(&feed)))
}

pub async fn post_comment(object_id: &str, text: &str, parent: Option<&str>) -> Result<()> {
    let source = "/";
    let mut options = json!({"objectId": object_id, "text": text, "tags": "[]"});
    if let Some(parent) = parent {
        options["parentCommentId"] = json!(parent);
    }
    submit("AggregatedCommentResource/create/", source, options)
        .await
        .map(|_| ())
}

pub async fn share_people(pin_id: &str, query_text: &str) -> Result<Vec<SharePerson>> {
    let source = format!("/pin/{pin_id}/");
    let q = query_text.trim();
    let value = if q.is_empty() {
        query(
            "ShareSuggestionsResource/get/",
            &source,
            json!({"pin": pin_id, "page_size": 12}),
        )
        .await?
    } else {
        query(
            "TypeaheadResource/get/",
            &source,
            json!({"term": q, "pin_scope": "pins"}),
        )
        .await?
    };
    Ok(parse_people(&value))
}

pub async fn send_pin(pin_id: &str, user_id: &str) -> Result<()> {
    let source = format!("/pin/{pin_id}/");
    submit(
        "ConversationMessagesResource/create/",
        &source,
        json!({"user_ids": [user_id], "pin": pin_id, "text": ""}),
    )
    .await
    .map(|_| ())
}

pub async fn pin_feedback(pin_id: &str, kind: &str, reason: Option<&str>, undo: bool) -> Result<()> {
    let source = format!("/pin/{pin_id}/");
    let feedback_type = match kind {
        "see_more" => "positive",
        "see_less" => "negative",
        other => other,
    };
    let mut data = json!({ "feedback_type": feedback_type });
    if let Some(reason) = reason {
        if !reason.is_empty() {
            data["feedback_reason"] = json!(reason);
        }
    }
    // The old UserHomefeedPFYPinFeedbackResource and PinFeedbackResource
    // paths answer 404. The site posts this through ApiResource.
    let options = json!({
        "url": format!("/v3/pins/{pin_id}/feedback/"),
        "data": data
    });
    let verb = if undo { "delete" } else { "create" };
    submit(&format!("ApiResource/{verb}/"), &source, options)
        .await
        .map(|_| ())
}

pub async fn report_pin(pin_id: &str, reason: &str) -> Result<()> {
    let source = format!("/pin/{pin_id}/");
    let options = json!({"pin_id": pin_id, "reason": reason});
    let first = submit("PinReportResource/create/", &source, options.clone()).await;
    match first {
        Ok(_) => Ok(()),
        Err(err) if err.to_string().contains("404") => {
            submit("ReportPinResource/create/", &source, options)
                .await
                .map(|_| ())
        }
        Err(err) => Err(err),
    }
}

pub async fn load_collages() -> Result<Vec<CollageDest>> {
    let value = query("CollageResource/get/", "/", json!({"filter": "all"})).await?;
    Ok(parse_collages(&value))
}

pub async fn create_collage(name: &str, pin_id: &str) -> Result<CollageDest> {
    let source = format!("/pin/{pin_id}/");
    let value = submit(
        "CollageResource/create/",
        &source,
        json!({"name": name, "title": name, "pin_id": pin_id, "pin_ids": [pin_id]}),
    )
    .await?;
    let data = value
        .pointer("/resource_response/data")
        .cloned()
        .unwrap_or(Value::Null);
    let mut parsed = parse_collages(&json!({"resource_response": {"data": [data]}}));
    Ok(parsed.pop().unwrap_or(CollageDest {
        id: String::new(),
        name: name.to_string(),
    }))
}

pub async fn add_to_collage(collage_id: &str, pin_id: &str) -> Result<()> {
    let source = format!("/pin/{pin_id}/");
    submit(
        "CollagePinsResource/create/",
        &source,
        json!({"collage_id": collage_id, "collage": collage_id, "pin_id": pin_id}),
    )
    .await
    .map(|_| ())
}

async fn submit(resource: &str, source: &str, options: Value) -> Result<Value> {
    let cookies = session()?;
    let http = client()?;
    let data = json!({"options": options, "context": {}});
    let form = format!(
        "source_url={}&data={}",
        urlencoding::encode(source),
        urlencoding::encode(&data.to_string())
    );
    let resp = http
        .post(format!("https://www.pinterest.com/resource/{resource}"))
        .headers(headers(&cookies, source))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Origin", "https://www.pinterest.com")
        .body(form)
        .send()
        .await
        .with_context(|| resource.to_string())?;
    let status = resp.status();
    let body = resp.text().await?;
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    check_error(&v)?;
    if !status.is_success() {
        bail!("Pinterest ответил {status}");
    }
    Ok(v)
}

async fn query(resource: &str, source: &str, options: Value) -> Result<Value> {
    let cookies = session()?;
    let http = client()?;
    let data = json!({"options": options, "context": {}});
    let url = format!(
        "https://www.pinterest.com/resource/{resource}?source_url={}&data={}",
        urlencoding::encode(source),
        urlencoding::encode(&data.to_string())
    );
    let body = http
        .get(url)
        .headers(headers(&cookies, source))
        .send()
        .await
        .with_context(|| resource.to_string())?
        .text()
        .await?;
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    check_error(&v)?;
    Ok(v)
}

fn parse_comments(root: &Value) -> Vec<PinComment> {
    let mut out = Vec::new();
    collect_comments(root, &mut out, 0);
    let mut seen = std::collections::HashSet::new();
    out.retain(|comment| comment.id.is_empty() || seen.insert(comment.id.clone()));
    out
}

fn collect_comments(value: &Value, out: &mut Vec<PinComment>, depth: u32) {
    if depth > 7 {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                collect_comments(item, out, depth + 1);
            }
        }
        Value::Object(map) => {
            let text = map.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
            let user = map.get("user").and_then(|v| v.as_object());
            if !text.is_empty() && user.is_some() {
                let user = user.unwrap();
                let author = user
                    .get("full_name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .or_else(|| user.get("username").and_then(|v| v.as_str()))
                    .unwrap_or("пользователь")
                    .to_string();
                let id = json_id(map.get("id"));
                let likes = map
                    .get("reaction_counts")
                    .and_then(|v| v.get("1"))
                    .and_then(|v| v.as_i64())
                    .or_else(|| map.get("like_count").and_then(|v| v.as_i64()))
                    .unwrap_or(0);
                out.push(PinComment {
                    id,
                    author,
                    text: text.to_string(),
                    when: comment_when(map),
                    likes,
                });
            }
            for (key, child) in map {
                if key == "user" {
                    continue;
                }
                collect_comments(child, out, depth + 1);
            }
        }
        _ => {}
    }
}

fn comment_when(map: &serde_json::Map<String, Value>) -> String {
    for key in ["relative_created_at", "relative_timestamp", "created_at_relative"] {
        if let Some(text) = map.get(key).and_then(|v| v.as_str()) {
            if !text.is_empty() {
                return text.to_string();
            }
        }
    }
    String::new()
}

fn parse_people(root: &Value) -> Vec<SharePerson> {
    let mut out = Vec::new();
    collect_people(root, &mut out, 0);
    let mut seen = std::collections::HashSet::new();
    out.retain(|person| !person.id.is_empty() && seen.insert(person.id.clone()));
    out.truncate(12);
    out
}

fn collect_people(value: &Value, out: &mut Vec<SharePerson>, depth: u32) {
    if depth > 6 {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                collect_people(item, out, depth + 1);
            }
        }
        Value::Object(map) => {
            let handle = map
                .get("username")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let name = map
                .get("full_name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&handle)
                .to_string();
            let id = json_id(map.get("id").or_else(|| map.get("entity_id")));
            if !handle.is_empty() && !id.is_empty() {
                out.push(SharePerson { id, name, handle });
            }
            for (key, child) in map {
                if key == "user" {
                    collect_people(child, out, depth + 1);
                } else if !matches!(key.as_str(), "username" | "full_name") {
                    collect_people(child, out, depth + 1);
                }
            }
        }
        _ => {}
    }
}

fn parse_collages(root: &Value) -> Vec<CollageDest> {
    let mut out = Vec::new();
    collect_collages(root, &mut out, 0);
    let mut seen = std::collections::HashSet::new();
    out.retain(|item| !item.id.is_empty() && seen.insert(item.id.clone()));
    out
}

fn collect_collages(value: &Value, out: &mut Vec<CollageDest>, depth: u32) {
    if depth > 6 {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                collect_collages(item, out, depth + 1);
            }
        }
        Value::Object(map) => {
            let name = map
                .get("name")
                .or_else(|| map.get("title"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let id = json_id(map.get("id"));
            let looks = map.contains_key("pin_ids")
                || map.contains_key("collage_type")
                || map.get("type").and_then(|v| v.as_str()) == Some("collage");
            if looks && !id.is_empty() && !name.is_empty() {
                out.push(CollageDest { id, name });
            }
            for child in map.values() {
                collect_collages(child, out, depth + 1);
            }
        }
        _ => {}
    }
}

fn json_id(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(num)) => num.to_string(),
        _ => String::new(),
    }
}

pub fn pin_url(pin_id: &str) -> String {
    format!("https://www.pinterest.com/pin/{pin_id}/")
}

pub fn is_pinterest_pin(pin_id: &str) -> bool {
    !pin_id.is_empty() && pin_id.chars().all(|c| c.is_ascii_digit())
}
