use anyhow::{Context, Result};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
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
    /// One lock per URL, so different pins download together.
    inflight: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl ImageCache {
    pub fn new() -> Result<Self> {
        let dir = Config::cache_dir()?.join("images");
        std::fs::create_dir_all(&dir)?;
        let client = reqwest::Client::builder()
            .user_agent("pinterest-wallpaper/0.1")
            // This network has no IPv6 route. Bind an IPv4 socket and resolve
            // only A records, otherwise the connect dies with ENETUNREACH.
            .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
            .dns_resolver(Arc::new(Ipv4Prefer))
            .build()?;
        Ok(Self {
            dir,
            client,
            inflight: Arc::new(Mutex::new(HashMap::new())),
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

        let gate = {
            let mut map = self.inflight.lock().await;
            map.entry(url.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let guard = gate.lock().await;
        if path.exists() {
            drop(guard);
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
        drop(guard);
        {
            let mut map = self.inflight.lock().await;
            map.remove(url);
        }
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
/// Resolve names to IPv4 when any A record exists.
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
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no addresses",
                )
                .into());
            }
            let iter: Addrs = Box::new(addrs.into_iter());
            Ok(iter)
        })
    }
}

