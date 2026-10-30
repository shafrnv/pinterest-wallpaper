use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::Mutex;

const MAX_CACHE_BYTES: u64 = 500 * 1024 * 1024;

use crate::config::Config;

#[derive(Clone)]
pub struct ImageCache {
    dir: PathBuf,
    client: reqwest::Client,
    /// Simple lock so concurrent downloads of the same URL don't race badly
    inflight: Arc<Mutex<()>>,
}

impl ImageCache {
    pub fn new() -> Result<Self> {
        let dir = Config::cache_dir()?.join("images");
        std::fs::create_dir_all(&dir)?;
        let client = reqwest::Client::builder()
            .user_agent("pinterest-wallpaper/0.1")
            .build()?;
        Ok(Self {
            dir,
            client,
            inflight: Arc::new(Mutex::new(())),
        })
    }

    pub fn path_for_url(&self, url: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(url.as_bytes());
        let hash = hex::encode(hasher.finalize());
        let ext = guess_ext(url);
        self.dir.join(format!("{hash}{ext}"))
    }

    /// Ensure image is on disk; returns local path.
    pub async fn fetch(&self, url: &str) -> Result<PathBuf> {
        let path = self.path_for_url(url);
        if path.exists() {
            return Ok(path);
        }

        let _guard = self.inflight.lock().await;
        if path.exists() {
            return Ok(path);
        }

        let bytes = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("status for {url}"))?
            .bytes()
            .await?;

        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        self.evict_if_needed().await;
        Ok(path)
    }

    async fn evict_if_needed(&self) {
        let mut files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
        let mut total = 0u64;
        let mut rd = match tokio::fs::read_dir(&self.dir).await {
            Ok(rd) => rd,
            Err(_) => return,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let len = meta.len();
            total += len;
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            files.push((path, len, modified));
        }
        if total <= MAX_CACHE_BYTES {
            return;
        }
        files.sort_by_key(|(_, _, m)| *m);
        for (path, len, _) in files {
            if total <= MAX_CACHE_BYTES {
                break;
            }
            if tokio::fs::remove_file(&path).await.is_ok() {
                total = total.saturating_sub(len);
            }
        }
    }
}

fn guess_ext(url: &str) -> &'static str {
    let lower = url.to_ascii_lowercase();
    if lower.contains(".png") {
        ".png"
    } else if lower.contains(".webp") {
        ".webp"
    } else if lower.contains(".gif") {
        ".gif"
    } else {
        ".jpg"
    }
}
