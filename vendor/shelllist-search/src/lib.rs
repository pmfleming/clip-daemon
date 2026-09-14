use std::{
    collections::HashMap,
    io::{self, BufRead, Write},
    sync::mpsc,
    thread,
};

use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};
use serde::{Deserialize, Serialize};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum SearchCommand {
    Catalog {
        owner: String,
        #[serde(default)]
        items: Vec<SearchItem>,
    },
    Query {
        owner: String,
        generation: u64,
        #[serde(default)]
        query: String,
    },
}

#[derive(Debug, Deserialize)]
pub struct SearchItem {
    pub key: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub score: i64,
    #[serde(default, rename = "providerPriority")]
    pub provider_priority: i64,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub owner: String,
    pub generation: u64,
    pub keys: Vec<String>,
}

#[derive(Debug)]
struct RankedItem<'a> {
    item: &'a SearchItem,
    normalized_title: String,
    match_score: i64,
}

fn normalize(value: &str) -> String {
    value
        .nfkd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .collect()
}

fn words(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
}

fn allowed_typos(length: usize) -> usize {
    match length {
        0..=3 => 0,
        4..=6 => 1,
        7..=10 => 2,
        _ => 3,
    }
}

fn token_score(field: &str, token: &str) -> Option<i64> {
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
    let typo_limit = allowed_typos(token_length);
    for word in words(field) {
        let distance = strsim::osa_distance(word, token);
        if distance <= typo_limit {
            let score = 9_000
                - distance as i64 * 900
                - word.chars().count().abs_diff(token_length) as i64 * 25;
            best = Some(best.map_or(score, |current| current.max(score)));
        }
    }
    best
}

fn best_token_score(title: &str, subtitle: &str, keywords: &[String], token: &str) -> Option<i64> {
    let keyword = keywords
        .iter()
        .filter_map(|field| token_score(field, token))
        .max();
    [
        token_score(title, token).map(|score| score + 1_800),
        token_score(subtitle, token).map(|score| score + 600),
        keyword,
    ]
    .into_iter()
    .flatten()
    .max()
}

fn title_bonus(title: &str, query: &str) -> i64 {
    if title == query {
        20_000
    } else if title.starts_with(query) {
        12_000
    } else if title.contains(query) {
        8_000
    } else {
        0
    }
}

fn item_match(item: &SearchItem, query: &str) -> Option<(i64, String)> {
    let title = normalize(&item.title);
    let tokens = words(query).collect::<Vec<_>>();
    if tokens.is_empty() {
        return Some((0, title));
    }

    let subtitle = normalize(&item.subtitle);
    let keywords = item
        .keywords
        .iter()
        .map(|value| normalize(value))
        .collect::<Vec<_>>();
    let score = tokens.into_iter().try_fold(0, |total, token| {
        best_token_score(&title, &subtitle, &keywords, token).map(|score| total + score)
    })?;
    Some((score + title_bonus(&title, query), title))
}

pub fn rank(owner: String, generation: u64, query: &str, items: &[SearchItem]) -> SearchResponse {
    let query = normalize(query);
    let query = query.trim();
    let mut ranked = items
        .iter()
        .filter_map(|item| {
            let (match_score, normalized_title) = item_match(item, query)?;
            Some(RankedItem {
                item,
                normalized_title,
                match_score,
            })
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .match_score
            .cmp(&left.match_score)
            .then_with(|| right.item.score.cmp(&left.item.score))
            .then_with(|| {
                right
                    .item
                    .provider_priority
                    .cmp(&left.item.provider_priority)
            })
            .then_with(|| left.normalized_title.cmp(&right.normalized_title))
            .then_with(|| left.item.key.cmp(&right.item.key))
    });
    SearchResponse {
        owner,
        generation,
        keys: ranked
            .into_iter()
            .map(|ranked| ranked.item.key.clone())
            .collect(),
    }
}

fn write_line(output: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *output, value)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn apply_command(
    command: Result<SearchCommand, String>,
    catalogs: &mut HashMap<String, Vec<SearchItem>>,
    pending: &mut HashMap<String, (u64, String)>,
    output: &mut impl Write,
) -> io::Result<()> {
    match command {
        Ok(SearchCommand::Catalog { owner, items }) => {
            catalogs.insert(owner, items);
        }
        Ok(SearchCommand::Query {
            owner,
            generation,
            query,
        }) => {
            if pending
                .get(&owner)
                .is_none_or(|(current, _)| generation >= *current)
            {
                pending.insert(owner, (generation, query));
            }
        }
        Err(error) => write_line(output, &serde_json::json!({ "error": error }))?,
    }
    Ok(())
}

fn send_pending(
    catalogs: &HashMap<String, Vec<SearchItem>>,
    pending: &mut HashMap<String, (u64, String)>,
    output: &mut impl Write,
) -> io::Result<()> {
    for (owner, (generation, query)) in std::mem::take(pending) {
        let items: &[SearchItem] = catalogs.get(&owner).map_or(&[], Vec::as_slice);
        write_line(output, &rank(owner, generation, &query, items))?;
    }
    Ok(())
}

fn serve_commands(
    commands: mpsc::Receiver<Result<SearchCommand, String>>,
    mut output: impl Write,
) -> io::Result<()> {
    let mut catalogs = HashMap::<String, Vec<SearchItem>>::new();
    let mut pending = HashMap::<String, (u64, String)>::new();

    while let Ok(first) = commands.recv() {
        for command in std::iter::once(first).chain(commands.try_iter()) {
            apply_command(command, &mut catalogs, &mut pending, &mut output)?;
        }
        send_pending(&catalogs, &mut pending, &mut output)?;
    }
    Ok(())
}

pub fn serve() -> io::Result<()> {
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let stdout = io::stdout();
        serve_commands(receiver, io::BufWriter::new(stdout.lock()))
    });

    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let command = serde_json::from_str::<SearchCommand>(&line)
            .map_err(|error| format!("invalid search request: {error}"));
        if sender.send(command).is_err() {
            break;
        }
    }
    drop(sender);
    worker
        .join()
        .map_err(|_| io::Error::other("search worker panicked"))?
}

#[cfg(test)]
mod tests {
    use super::{SearchCommand, SearchItem, rank, serve_commands};
    use std::sync::mpsc;

    fn item(key: &str, title: &str) -> SearchItem {
        SearchItem {
            key: key.into(),
            title: title.into(),
            subtitle: String::new(),
            keywords: Vec::new(),
            score: 0,
            provider_priority: 0,
        }
    }

    fn keys(query: &str, items: Vec<SearchItem>) -> Vec<String> {
        rank("test".into(), 1, query, &items).keys
    }

    #[test]
    fn matches_normalized_substrings_and_rejects_unrelated_items() {
        assert_eq!(
            keys("fox", vec![item("firefox", "Mozilla Firefox")]),
            ["firefox"]
        );
        assert_eq!(keys("ffx", vec![item("firefox", "Firefox")]), ["firefox"]);
        assert_eq!(keys("cafe", vec![item("cafe", "Café")]), ["cafe"]);
        assert!(keys("terminal", vec![item("files", "Files")]).is_empty());
    }

    #[test]
    fn tolerates_omissions_substitutions_and_transpositions() {
        let values = vec![item("firefox", "Firefox"), item("files", "Files")];
        assert_eq!(keys("firfox", values), ["firefox"]);
        for (query, key, title) in [
            ("chorme", "chrome", "Google Chrome"),
            ("firwfox", "firefox", "Firefox"),
        ] {
            assert_eq!(keys(query, vec![item(key, title)]), [key]);
        }
    }

    #[test]
    fn all_tokens_must_match_and_strong_matches_rank_first() {
        let values = vec![
            item("code", "Visual Studio Code"),
            item("codium", "VSCodium"),
            item("contacts", "Google Contacts"),
        ];
        assert_eq!(keys("studio code", values), ["code"]);
        let ranked = keys(
            "code",
            vec![item("middle", "Barcode Tool"), item("exact", "Code")],
        );
        assert_eq!(ranked, ["exact", "middle"]);
    }

    #[test]
    fn reuses_catalog_and_coalesces_waiting_queries() -> Result<(), Box<dyn std::error::Error>> {
        let (sender, receiver) = mpsc::channel();
        sender.send(Ok(SearchCommand::Catalog {
            owner: "test".into(),
            items: vec![item("firefox", "Firefox"), item("files", "Files")],
        }))?;
        sender.send(Ok(SearchCommand::Query {
            owner: "test".into(),
            generation: 1,
            query: "files".into(),
        }))?;
        sender.send(Ok(SearchCommand::Query {
            owner: "test".into(),
            generation: 2,
            query: "fire".into(),
        }))?;
        drop(sender);

        let mut output = Vec::new();
        serve_commands(receiver, &mut output)?;
        let lines = String::from_utf8(output)?;
        let responses = lines.lines().collect::<Vec<_>>();
        assert_eq!(responses.len(), 1);
        let response: serde_json::Value = serde_json::from_str(responses[0])?;
        assert_eq!(response["generation"], 2);
        assert_eq!(response["keys"], serde_json::json!(["firefox"]));
        Ok(())
    }
}
