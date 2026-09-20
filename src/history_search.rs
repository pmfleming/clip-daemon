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
    model::{EntrySummary, HistoryQuery},
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

#[derive(Clone, Copy)]
struct CursorScope<'a> {
    query: &'a str,
    generation: u64,
    owner: Option<&'a str>,
    collapse: bool,
}

impl<'a> CursorScope<'a> {
    fn new(
        params: &'a crate::actions::QueryParams,
        owner: Option<&'a str>,
        collapse: bool,
    ) -> Self {
        Self {
            query: &params.query,
            generation: params.generation,
            owner,
            collapse,
        }
    }
}

struct Catalog {
    entries: Vec<EntrySummary>,
    current: Option<EntrySummary>,
    limited: bool,
}

impl Cursor {
    fn check(&self, token: u64) -> Result<(), ApiError> {
        (self.token == token).then_some(()).ok_or_else(stale)
    }
}

impl HistorySearch {
    fn mac(&self, cursor: &str, scope: CursorScope<'_>) -> Result<Hmac<Sha256>, ApiError> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).map_err(search_failed)?;
        mac.update(
            &serde_json::to_vec(&(
                cursor,
                scope.query,
                scope.generation,
                scope.owner,
                scope.collapse,
            ))
            .map_err(search_failed)?,
        );
        Ok(mac)
    }
    fn decode(&self, value: &str, scope: CursorScope<'_>) -> Result<Cursor, ApiError> {
        if value.len() > 512 {
            return Err(stale());
        }
        let (body, tag) = value.rsplit_once('|').ok_or_else(stale)?;
        self.mac(body, scope)?
            .verify_slice(&hex::decode(tag).map_err(|_| stale())?)
            .map_err(|_| stale())?;
        let cursor: Cursor = serde_json::from_str(body).map_err(|_| stale())?;
        if cursor.expires <= self.started.elapsed().as_secs() || cursor.offset >= MAX_ENTRIES {
            return Err(stale());
        }
        Ok(cursor)
    }
    fn encode(&self, cursor: Cursor, scope: CursorScope<'_>) -> Result<String, ApiError> {
        let body = serde_json::to_string(&cursor).map_err(search_failed)?;
        let tag = self.mac(&body, scope)?.finalize().into_bytes();
        Ok(format!("{body}|{}", hex::encode(tag)))
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
        let cursor = match params.cursor.as_deref() {
            Some(value) => self.decode(value, CursorScope::new(&params, owner, collapse))?,
            None => Cursor {
                token,
                offset: 0,
                expires: self.started.elapsed().as_secs() + CURSOR_SECONDS,
            },
        };
        cursor.check(token)?;
        let mut catalog = catalog(backend, params.generation, collapse).await?;
        cursor.check(backend.change_token().await?)?;
        let (catalog, params) = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            catalog.entries = rank_entries(catalog.entries, &params.query)?;
            Ok::<_, ApiError>((catalog, params))
        })
        .await
        .map_err(search_failed)??;
        // Never return a stale page after expensive ranking, either.
        cursor.check(backend.change_token().await?)?;
        self.page(catalog, cursor, params, owner, collapse)
    }

    fn page(
        &self,
        catalog: Catalog,
        cursor: Cursor,
        params: crate::actions::QueryParams,
        owner: Option<&str>,
        collapse: bool,
    ) -> Result<Value, ApiError> {
        let total = catalog.entries.len();
        let offset = cursor.offset;
        let token = cursor.token;
        let page: Vec<_> = catalog
            .entries
            .into_iter()
            .skip(offset)
            .take(params.limit)
            .collect();
        let consumed = offset + page.len();
        let has_more = consumed < total;
        let next = has_more
            .then(|| {
                self.encode(
                    Cursor {
                        token,
                        offset: consumed,
                        expires: cursor.expires,
                    },
                    CursorScope::new(&params, owner, collapse),
                )
            })
            .transpose()?;
        Ok(
            json!({ "history": { "revision": token, "snapshot_revision": token.to_string(), "generation": params.generation,
            "current": catalog.current, "entries": page, "has_more": has_more, "next_cursor": next,
            "total": total, "search_limited": catalog.limited, "offset": offset } }),
        )
    }
}
fn stale() -> ApiError {
    ApiError::new(
        "stale-cursor",
        "Clipboard history changed or the cursor expired; refresh the query",
    )
}

async fn catalog(backend: &Backend, generation: u64, collapse: bool) -> Result<Catalog, ApiError> {
    let mut entries = Vec::new();
    let mut current = None;
    loop {
        let page = backend
            .query(HistoryQuery {
                query: String::new(),
                generation,
                offset: entries.len(),
                limit: 200,
                collapse_self_echoes: collapse,
            })
            .await?;
        current = current.or(page.current);
        if page.has_more && page.entries.is_empty() {
            return Err(stale());
        }
        entries.extend(page.entries);
        if !page.has_more || entries.len() >= MAX_ENTRIES {
            return Ok(Catalog {
                entries,
                current,
                limited: page.has_more,
            });
        }
    }
}

fn search_failed(_: impl std::fmt::Display) -> ApiError {
    ApiError::new("search-failed", "Clipboard search failed")
}

fn rank_entries(entries: Vec<EntrySummary>, query: &str) -> Result<Vec<EntrySummary>, ApiError> {
    let count = entries.len();
    let items = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let kind = serde_json::to_value(entry.kind)
                .map_err(search_failed)?
                .as_str()
                .unwrap_or("binary")
                .to_owned();
            let label = format!("{}{}", kind[..1].to_uppercase(), &kind[1..]);
            let title = if entry.preview.is_empty() {
                format!("{label} clipboard entry")
            } else {
                entry.preview.clone()
            };
            Ok(shelllist_search::SearchItem {
                key: entry.id.clone(),
                title,
                subtitle: format!("{label} · {} · {} bytes", entry.mime, entry.byte_size),
                // The matcher always gives title matches a higher weight than keywords.
                keywords: vec![entry.mime.clone(), kind],
                score: (count - index) as i64,
                provider_priority: 100,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    let keys = shelllist_search::rank(String::new(), 0, query, &items).keys;
    let mut by_id: HashMap<_, _> = items
        .into_iter()
        .zip(entries)
        .map(|(item, entry)| (item.key, entry))
        .collect();
    Ok(keys
        .into_iter()
        .filter_map(|key| by_id.remove(&key))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{Cursor, CursorScope, HistorySearch};
    #[test]
    fn cursors_bind_owner_query_generation_policy_and_process_lifetime() {
        let search = HistorySearch::default();
        let scope = CursorScope {
            query: "cafe",
            generation: 7,
            owner: Some(":1.2"),
            collapse: true,
        };
        let encode = |expires| {
            search
                .encode(
                    Cursor {
                        token: 9,
                        offset: 200,
                        expires,
                    },
                    scope,
                )
                .unwrap()
        };
        let cursor = encode(120);
        assert_eq!(search.decode(&cursor, scope).unwrap().offset, 200);
        for changed in [
            CursorScope {
                query: "tea",
                ..scope
            },
            CursorScope {
                generation: 8,
                ..scope
            },
            CursorScope {
                owner: Some(":1.3"),
                ..scope
            },
            CursorScope {
                owner: None,
                ..scope
            },
            CursorScope {
                collapse: false,
                ..scope
            },
        ] {
            assert!(search.decode(&cursor, changed).is_err());
        }
        assert!(HistorySearch::default().decode(&cursor, scope).is_err());
        assert!(search.decode(&encode(0), scope).is_err());
        assert!(search.decode(&cursor.replace("200", "201"), scope).is_err());
    }
}
