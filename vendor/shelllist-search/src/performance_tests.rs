use super::*;
use std::{hint::black_box, time::Instant};

// Frozen scoring reference before the length-bound optimization. Keep this in
// tests only: accelerated paths must retain Unicode and fuzzy/typo ordering.
fn reference_token_score(field: &str, token: &str) -> Option<i64> {
    if field == token {
        return Some(14_000);
    }
    if field.starts_with(token) {
        return Some(12_500 - field.chars().count().min(500) as i64);
    }
    if let Some(index) = words(field).position(|word| word.starts_with(token)) {
        return Some(11_500 - index as i64 * 20);
    }
    if let Some(index) = field.find(token) {
        return Some(10_000 - index.min(500) as i64);
    }
    let mut best = SkimMatcherV2::default()
        .fuzzy_match(field, token)
        .map(|score| 7_000 + score);
    let token_length = token.chars().count();
    for word in words(field) {
        let distance = strsim::osa_distance(word, token);
        if distance <= allowed_typos(token_length) {
            let score = 9_000
                - distance as i64 * 900
                - word.chars().count().abs_diff(token_length) as i64 * 25;
            best = Some(best.map_or(score, |current| current.max(score)));
        }
    }
    best
}

#[test]
fn optimized_token_scores_match_reference() {
    let mut state = 17_u64;
    let alphabet = ['a', 'b', 'c', 'd', 'é', '界', ' '];
    for _ in 0..3000 {
        let mut make = |length| {
            (0..length)
                .map(|_| {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    alphabet[(state >> 32) as usize % alphabet.len()]
                })
                .collect::<String>()
        };
        let field = make(18);
        for length in [2, 3, 4, 6, 7, 10, 11, 20] {
            let token = make(length);
            assert_eq!(
                token_score(&field, &token),
                reference_token_score(&field, &token),
                "{field:?} / {token:?}"
            );
        }
    }
}

#[test]
fn prepared_catalog_reuses_fields_without_changing_keys_or_ties() {
    let items = catalog();
    let prepared = PreparedCatalog::new(&items);
    for query in ["", "—", "Café", "résumé edit", "devlop", "xy", "applcation"] {
        assert_eq!(
            prepared.rank("owner".into(), 7, query).keys,
            rank("owner".into(), 7, query, &items).keys
        );
    }
    let first = PreparedCatalog::new(&[SearchItem {
        key: "first".into(),
        title: "Café".into(),
        subtitle: String::new(),
        keywords: vec![],
        score: 1,
        provider_priority: 0,
    }]);
    assert_eq!(first.rank("owner".into(), 1, "cafe").keys, ["first"]);
    let replacement = PreparedCatalog::new(&[]);
    assert!(replacement.rank("owner".into(), 2, "cafe").keys.is_empty());
}

#[test]
#[ignore = "manual prepared-catalog query probe; setup excluded"]
fn profile_prepared_search_work() {
    let items = PreparedCatalog::new(&catalog());
    let start = Instant::now();
    for iteration in 0..25 {
        let query = ["applcation", "term", "xy", "résumé edit", "devlop"][iteration % 5];
        black_box(items.rank("bench".into(), iteration as u64, query));
    }
    println!(
        "25 rankings / 1000 normalized catalog rows: {:.3} ms",
        start.elapsed().as_secs_f64() * 1000.0
    );
}

pub(super) fn catalog() -> Vec<SearchItem> {
    (0..1000)
        .map(|index| SearchItem {
            key: format!("app-{index}"),
            title: format!("Application {index} — Résumé editor"),
            subtitle: "A desktop application for document development and collaboration".into(),
            keywords: vec![
                "Utility".into(),
                "Development".into(),
                format!("application-{index:04}"),
            ],
            score: index % 10,
            provider_priority: 0,
        })
        .collect()
}

#[test]
#[ignore = "manual synthetic performance probe; no UI/presentation timing"]
fn profile_search_work() {
    let items = catalog();
    let start = Instant::now();
    for iteration in 0..25 {
        let query = ["applcation", "term", "xy", "résumé edit", "devlop"][iteration % 5];
        black_box(rank("bench".into(), iteration as u64, query, &items));
    }
    println!(
        "25 rankings / 1000 prepared raw rows: {:.3} ms",
        start.elapsed().as_secs_f64() * 1000.0
    );
}
