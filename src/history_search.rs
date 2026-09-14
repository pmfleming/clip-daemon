//! Bounded native catalog search with authenticated, revision-bound cursors.
//! Cursors retain no server-side rows or leases: a changed store invalidates them.
use std::{collections::HashMap, sync::Arc, time::Instant};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::{
    actions::{ApiError, Backend},
    backend::HistoryQuery,
    model::EntrySummary,
};

const MAX_ENTRIES: usize = 5000;
const CURSOR_SECONDS: u64 = 120;

pub(crate) struct HistorySearch {
    key: [u8; 16],
    started: Instant,
    admission: Arc<Semaphore>,
}
impl Default for HistorySearch {
    fn default() -> Self {
        Self {
            key: *Uuid::new_v4().as_bytes(),
            started: Instant::now(),
            admission: Arc::new(Semaphore::new(2)),
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    token: u64,
    offset: usize,
    expires: u64,
}

impl HistorySearch {
    fn mac(
        &self,
        cursor: &str,
        query: &str,
        generation: u64,
        owner: Option<&str>,
        collapse: bool,
    ) -> Hmac<Sha256> {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(
            &serde_json::to_vec(&(cursor, query, generation, owner, collapse))
                .expect("serialize cursor context"),
        );
        mac
    }
    fn decode(
        &self,
        value: &str,
        query: &str,
        generation: u64,
        owner: Option<&str>,
        collapse: bool,
    ) -> Result<Cursor, ApiError> {
        if value.len() > 512 {
            return Err(stale());
        }
        let (body, tag) = value.rsplit_once('|').ok_or_else(stale)?;
        self.mac(body, query, generation, owner, collapse)
            .verify_slice(&hex::decode(tag).map_err(|_| stale())?)
            .map_err(|_| stale())?;
        let cursor: Cursor = serde_json::from_str(body).map_err(|_| stale())?;
        if cursor.expires <= self.started.elapsed().as_secs() || cursor.offset >= MAX_ENTRIES {
            return Err(stale());
        }
        Ok(cursor)
    }
    fn encode(
        &self,
        cursor: Cursor,
        query: &str,
        generation: u64,
        owner: Option<&str>,
        collapse: bool,
    ) -> String {
        let body = serde_json::to_string(&cursor).expect("serialize cursor");
        let tag = self
            .mac(&body, query, generation, owner, collapse)
            .finalize()
            .into_bytes();
        format!("{body}|{}", hex::encode(tag))
    }
    pub(crate) async fn query(
        &self,
        backend: &Backend,
        params: crate::actions::QueryParams,
        owner: Option<&str>,
        collapse: bool,
    ) -> Result<Value, ApiError> {
        if params.query.chars().count() > 256 {
            return Err(ApiError::validation(
                "Fuzzy search query exceeds 256 characters",
            ));
        }
        let permit = self
            .admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::new("busy", "Clipboard search is busy"))?;
        let token = backend.change_token().await?;
        let cursor = params
            .cursor
            .as_deref()
            .map(|value| self.decode(value, &params.query, params.generation, owner, collapse))
            .transpose()?;
        if cursor.as_ref().is_some_and(|cursor| cursor.token != token) {
            return Err(stale());
        }
        let offset = cursor.as_ref().map_or(0, |cursor| cursor.offset);
        let expires = cursor.as_ref().map_or(
            self.started.elapsed().as_secs() + CURSOR_SECONDS,
            |cursor| cursor.expires,
        );
        let mut entries = Vec::new();
        let mut current = None;
        let mut limited = false;
        loop {
            let page = backend
                .query(HistoryQuery {
                    query: String::new(),
                    generation: params.generation,
                    offset: entries.len(),
                    limit: 200,
                    collapse_self_echoes: collapse,
                })
                .await?;
            if current.is_none() {
                current = page.current;
            }
            if page.has_more && page.entries.is_empty() {
                return Err(stale());
            }
            entries.extend(page.entries);
            if !page.has_more {
                break;
            }
            if entries.len() >= MAX_ENTRIES {
                limited = true;
                break;
            }
        }
        if backend.change_token().await? != token {
            return Err(stale());
        }
        let query = params.query.clone();
        let ranked = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            rank_entries(entries, &query)
        })
        .await
        .map_err(|_| ApiError::new("search-failed", "Clipboard search failed"))?;
        // Never return a stale page after expensive ranking, either.
        if backend.change_token().await? != token {
            return Err(stale());
        }
        let total = ranked.len();
        let page: Vec<_> = ranked.into_iter().skip(offset).take(params.limit).collect();
        let consumed = offset + page.len();
        let has_more = consumed < total;
        let next = has_more.then(|| {
            self.encode(
                Cursor {
                    token,
                    offset: consumed,
                    expires,
                },
                &params.query,
                params.generation,
                owner,
                collapse,
            )
        });
        Ok(
            json!({ "history": { "revision": token, "snapshot_revision": token.to_string(), "generation": params.generation,
            "current": current, "entries": page, "has_more": has_more, "next_cursor": next,
            "total": total, "search_limited": limited, "offset": offset } }),
        )
    }
}
fn stale() -> ApiError {
    ApiError::new(
        "stale-cursor",
        "Clipboard history changed or the cursor expired; refresh the query",
    )
}

fn rank_entries(entries: Vec<EntrySummary>, query: &str) -> Vec<EntrySummary> {
    let count = entries.len();
    let items: Vec<_> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let kind = serde_json::to_value(entry.kind)
                .expect("serialize kind")
                .as_str()
                .unwrap_or("binary")
                .to_owned();
            let label = format!("{}{}", kind[..1].to_uppercase(), &kind[1..]);
            let title = if entry.preview.is_empty() {
                format!("{label} clipboard entry")
            } else {
                entry.preview.clone()
            };
            shelllist_search::SearchItem {
                key: entry.id.clone(),
                title: title.clone(),
                subtitle: format!("{label} · {} · {} bytes", entry.mime, entry.byte_size),
                keywords: vec![title, entry.mime.clone(), kind],
                score: (count - index) as i64,
                provider_priority: 100,
            }
        })
        .collect();
    let keys = shelllist_search::rank(String::new(), 0, query, &items).keys;
    let mut by_id: HashMap<_, _> = entries
        .into_iter()
        .map(|entry| (entry.id.clone(), entry))
        .collect();
    keys.into_iter()
        .filter_map(|key| by_id.remove(&key))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cursors_bind_owner_query_generation_policy_and_process_lifetime() {
        let search = HistorySearch::default();
        let cursor = search.encode(
            Cursor {
                token: 9,
                offset: 200,
                expires: 120,
            },
            "cafe",
            7,
            Some(":1.2"),
            true,
        );
        assert_eq!(
            search
                .decode(&cursor, "cafe", 7, Some(":1.2"), true)
                .unwrap()
                .offset,
            200
        );
        for (query, generation, owner, collapse) in [
            ("tea", 7, Some(":1.2"), true),
            ("cafe", 8, Some(":1.2"), true),
            ("cafe", 7, Some(":1.3"), true),
            ("cafe", 7, Some(":1.2"), false),
        ] {
            assert!(
                search
                    .decode(&cursor, query, generation, owner, collapse)
                    .is_err()
            );
        }
        assert!(
            HistorySearch::default()
                .decode(&cursor, "cafe", 7, Some(":1.2"), true)
                .is_err()
        );
        let expired = search.encode(
            Cursor {
                token: 9,
                offset: 200,
                expires: 0,
            },
            "cafe",
            7,
            None,
            true,
        );
        assert!(search.decode(&expired, "cafe", 7, None, true).is_err());
        assert!(
            search
                .decode(&cursor.replace("200", "201"), "cafe", 7, Some(":1.2"), true)
                .is_err()
        );
    }
}
