use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use tracing::{info, warn};

use crate::auth::AuthStore;
use crate::config::{Config, FeedSource};
use crate::pinterest::boards::BoardsClient;
use crate::pinterest::homefeed::HomefeedClient;
use crate::pinterest::oauth;

#[derive(Debug, Clone)]
pub struct PinItem {
    pub id: String,
    pub image_url: String,
    pub width: u32,
    pub height: u32,
    pub link: Option<String>,
}

enum SourceState {
    Homefeed {
        client: HomefeedClient,
        bookmark: Option<String>,
    },
    Boards {
        client: BoardsClient,
        board_ids: Vec<String>,
        board_idx: usize,
        bookmark: Option<String>,
    },
    Local {
        paths: Vec<PathBuf>,
        cursor: usize,
    },
}

pub struct FeedManager {
    state: SourceState,
    exhausted: bool,
}

impl FeedManager {
    pub async fn from_config(cfg: &Config) -> Result<Self> {
        let state = match &cfg.feed {
            FeedSource::Homefeed => {
                let store = AuthStore::load()?;
                let session = store
                    .session
                    .clone()
                    .context("no session — run `login-session`")?;
                let client = HomefeedClient::new(session)?;
                SourceState::Homefeed {
                    client,
                    bookmark: None,
                }
            }
            FeedSource::AllBoards => {
                let client = oauth_client(cfg)?;
                let board_ids = client.list_board_ids().await?;
                if board_ids.is_empty() {
                    bail!("no boards found on this account");
                }
                info!("loaded {} boards", board_ids.len());
                SourceState::Boards {
                    client,
                    board_ids,
                    board_idx: 0,
                    bookmark: None,
                }
            }
            FeedSource::Board { id } => {
                let client = oauth_client(cfg)?;
                SourceState::Boards {
                    client,
                    board_ids: vec![id.clone()],
                    board_idx: 0,
                    bookmark: None,
                }
            }
            FeedSource::Local { path } => {
                let paths = list_local_images(path)?;
                if paths.is_empty() {
                    bail!("no images in {}", path.display());
                }
                SourceState::Local { paths, cursor: 0 }
            }
        };

        Ok(Self {
            state,
            exhausted: false,
        })
    }

    pub async fn next_page(&mut self) -> Result<Vec<PinItem>> {
        if self.exhausted {
            return Ok(Vec::new());
        }

        match &mut self.state {
            SourceState::Homefeed { client, bookmark } => {
                let (pins, next) = client.page(bookmark.as_deref()).await?;
                *bookmark = next;
                if bookmark.is_none() {
                    self.exhausted = true;
                }
                Ok(pins)
            }
            SourceState::Boards {
                client,
                board_ids,
                board_idx,
                bookmark,
            } => {
                loop {
                    if *board_idx >= board_ids.len() {
                        self.exhausted = true;
                        return Ok(Vec::new());
                    }
                    let board_id = &board_ids[*board_idx];
                    match client.board_pins_page(board_id, bookmark.as_deref()).await {
                        Ok((pins, next)) => {
                            *bookmark = next;
                            if bookmark.is_none() {
                                *board_idx += 1;
                                *bookmark = None;
                            }
                            if pins.is_empty() {
                                continue;
                            }
                            return Ok(pins);
                        }
                        Err(e) => {
                            warn!("board {board_id}: {e:#}");
                            *board_idx += 1;
                            *bookmark = None;
                        }
                    }
                }
            }
            SourceState::Local { paths, cursor } => {
                if *cursor >= paths.len() {
                    self.exhausted = true;
                    return Ok(Vec::new());
                }
                // Serve in chunks of 30
                let end = (*cursor + 30).min(paths.len());
                let chunk = &paths[*cursor..end];
                *cursor = end;
                if *cursor >= paths.len() {
                    self.exhausted = true;
                }
                let pins = chunk
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let url = format!("file://{}", p.display());
                        PinItem {
                            id: format!("local-{}", *cursor - chunk.len() + i),
                            image_url: url,
                            width: 600,
                            height: 800,
                            link: None,
                        }
                    })
                    .collect();
                Ok(pins)
            }
        }
    }
}

fn oauth_client(cfg: &Config) -> Result<BoardsClient> {
    let mut store = AuthStore::load()?;
    if let Some(oauth) = cfg.oauth.as_ref() {
        if !oauth.client_secret.is_empty() {
            oauth::refresh_if_needed(oauth, &mut store)?;
        }
    }
    let tokens = store
        .oauth
        .as_ref()
        .context("not logged in — run `login-oauth` or paste a trial access token")?;
    if tokens.refresh_token.is_none() {
        if let Some(exp) = tokens.expires_at {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            if now >= exp {
                anyhow::bail!(
                    "trial access token expired — create a new one in the Pinterest developer console"
                );
            }
        }
    }
    Ok(BoardsClient::new(tokens.access_token.clone())?)
}

fn list_local_images(dir: &std::path::Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(
            ext.as_str(),
            "jpg" | "jpeg" | "png" | "webp" | "gif" | "avif"
        ) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}
