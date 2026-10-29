//! Bounded native catalog search with authenticated, revision-bound cursors.
//! Cursors retain no server-side rows or leases: a changed store invalidates them.
use std::{sync::Arc, time::Instant};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::{
    actions::{ApiError, Backend},
    model::{HistoryPage, HistoryQuery},
};

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
        if cursor.expires <= self.started.elapsed().as_secs() {
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
        let _permit = self
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
        let page = backend
            .query(HistoryQuery {
                query: params.query.clone(),
                generation: params.generation,
                offset: cursor.offset,
                limit: params.limit,
                collapse_self_echoes: collapse,
            })
            .await?;
        // The query engine selects only this page; no full-catalog reconstruction
        // or frontend-specific Rust ranking can bypass database search/limits.
        cursor.check(backend.change_token().await?)?;
        self.page(page, cursor, params, owner, collapse)
    }

    fn page(
        &self,
        page: HistoryPage,
        cursor: Cursor,
        params: crate::actions::QueryParams,
        owner: Option<&str>,
        collapse: bool,
    ) -> Result<Value, ApiError> {
        let offset = cursor.offset;
        let token = cursor.token;
        let consumed = offset.saturating_add(page.entries.len());
        let has_more = page.has_more;
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
            "current": page.current, "entries": page.entries, "has_more": has_more, "next_cursor": next,
            "total": page.total, "search_limited": false, "offset": offset } }),
        )
    }
}
fn stale() -> ApiError {
    ApiError::new(
        "stale-cursor",
        "Clipboard history changed or the cursor expired; refresh the query",
    )
}

fn search_failed(_: impl std::fmt::Display) -> ApiError {
    ApiError::new("search-failed", "Clipboard search failed")
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
