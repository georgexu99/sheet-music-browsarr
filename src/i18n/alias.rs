use std::collections::HashMap;
use std::sync::OnceLock;

/// CSV file embedded at compile time. Columns: en,simplified,traditional,pinyin.
const ALIASES_CSV: &str = include_str!("../../assets/zh_aliases.csv");

/// The four scripts/forms we know how to translate between. Order is
/// stable and matches `AliasRow` field order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Script {
    En,
    Simplified,
    Traditional,
    Pinyin,
}

const SCRIPTS: [Script; 4] = [
    Script::En,
    Script::Simplified,
    Script::Traditional,
    Script::Pinyin,
];

struct AliasRow {
    en: String,
    simplified: String,
    traditional: String,
    pinyin: String,
}

impl AliasRow {
    fn variant(&self, script: Script) -> &str {
        match script {
            Script::En => &self.en,
            Script::Simplified => &self.simplified,
            Script::Traditional => &self.traditional,
            Script::Pinyin => &self.pinyin,
        }
    }
}

/// Case-insensitive token → row lookup. The token can be any of the
/// row's four variants; the index points back to the canonical row.
struct AliasIndex {
    rows: Vec<AliasRow>,
    by_token: HashMap<String, usize>,
}

impl AliasIndex {
    fn load() -> Self {
        let mut rows = Vec::new();
        for line in ALIASES_CSV.lines().skip(1) {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let cols: Vec<&str> = line.split(',').collect();
            if cols.len() < 4 {
                continue;
            }
            rows.push(AliasRow {
                en: cols[0].trim().to_string(),
                simplified: cols[1].trim().to_string(),
                traditional: cols[2].trim().to_string(),
                pinyin: cols[3].trim().to_string(),
            });
        }

        let mut by_token = HashMap::new();
        for (i, row) in rows.iter().enumerate() {
            for s in &SCRIPTS {
                let v = row.variant(*s);
                if !v.is_empty() {
                    // Earlier rows win on collision; predictable.
                    by_token.entry(v.to_lowercase()).or_insert(i);
                }
            }
        }

        Self { rows, by_token }
    }

    fn lookup(&self, token: &str) -> Option<&AliasRow> {
        self.by_token
            .get(&token.to_lowercase())
            .and_then(|i| self.rows.get(*i))
    }
}

fn aliases() -> &'static AliasIndex {
    static IDX: OnceLock<AliasIndex> = OnceLock::new();
    IDX.get_or_init(AliasIndex::load)
}

/// Tokenize a user query on whitespace + light punctuation, preserving
/// the original character segments (no normalization).
fn tokenize(query: &str) -> Vec<&str> {
    query
        .split(|c: char| {
            c.is_whitespace() || matches!(c, ',' | '.' | ';' | ':' | '!' | '?' | '(' | ')')
        })
        .filter(|t| !t.is_empty())
        .collect()
}

/// Expand a search query into up to 4 variants (one per script). If no
/// known alias is found anywhere in the query, returns the original
/// query unchanged.
///
/// Examples:
/// - "Chopin nocturne" → ["Chopin nocturne", "肖邦 夜曲", "蕭邦 夜曲", "xiaobang yequ"]
/// - "肖邦"              → ["Chopin", "肖邦", "蕭邦", "xiaobang"]
/// - "random query"     → ["random query"] (no expansion)
pub fn expand_query(query: &str) -> Vec<String> {
    let idx = aliases();
    let tokens = tokenize(query);
    if tokens.is_empty() {
        return vec![query.to_string()];
    }

    // Look every token up once.
    let token_rows: Vec<Option<&AliasRow>> = tokens.iter().map(|t| idx.lookup(t)).collect();
    let any_matched = token_rows.iter().any(|r| r.is_some());

    if !any_matched {
        return vec![query.to_string()];
    }

    let mut out: Vec<String> = Vec::with_capacity(SCRIPTS.len());
    for script in &SCRIPTS {
        let rendered: Vec<String> = tokens
            .iter()
            .zip(token_rows.iter())
            .map(|(orig, row)| match row {
                Some(r) => r.variant(*script).to_string(),
                None => orig.to_string(),
            })
            .collect();
        let joined = rendered.join(" ");
        if !out.contains(&joined) {
            out.push(joined);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_composer_expands_to_four_scripts() {
        let v = expand_query("Chopin Nocturne");
        assert!(v.contains(&"Chopin nocturne".to_string()) || v.contains(&"Chopin Nocturne".to_string()) || v.iter().any(|q| q.to_lowercase().contains("chopin nocturne")));
        assert!(v.iter().any(|q| q.contains("肖邦")));
        assert!(v.iter().any(|q| q.contains("蕭邦")));
        assert!(v.iter().any(|q| q.contains("xiaobang")));
    }

    #[test]
    fn pinyin_input_expands_to_hanzi() {
        let v = expand_query("xiaobang");
        assert!(v.iter().any(|q| q.contains("肖邦")));
        assert!(v.iter().any(|q| q.contains("Chopin")));
    }

    #[test]
    fn traditional_input_expands() {
        let v = expand_query("蕭邦 夜曲");
        assert!(v.iter().any(|q| q.contains("Chopin") && q.contains("nocturne")));
    }

    #[test]
    fn unknown_query_passes_through() {
        let v = expand_query("some random thing");
        assert_eq!(v, vec!["some random thing".to_string()]);
    }

    #[test]
    fn empty_query_passes_through() {
        let v = expand_query("");
        assert_eq!(v, vec!["".to_string()]);
    }
}
