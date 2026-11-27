use crate::qobuz::search::{Kind, Row};
use serde::Deserialize;
use std::{cmp::Ordering, collections::HashMap};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    #[default]
    Relevance,
    NameAsc,
    NameDesc,
    Date,
}

pub type Scores = HashMap<String, (i64, i64)>;

fn is_latin(ch: char) -> bool {
    ch.is_ascii_alphabetic()
        || matches!(
            ch as u32,
            0x00C0..=0x00D6
                | 0x00D8..=0x00F6
                | 0x00F8..=0x024F
                | 0x0250..=0x02AF
                | 0x1D00..=0x1DBF
                | 0x1E00..=0x1EFF
                | 0x2C60..=0x2C7F
                | 0xA720..=0xA7FF
                | 0xAB30..=0xAB6F
        )
}

pub fn words(value: &str) -> Vec<String> {
    if value.is_ascii() {
        return value
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .filter(|part| !part.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
    }
    let mut text = String::with_capacity(value.len());
    let mut latin = false;
    for ch in value.nfkd().flat_map(char::to_lowercase) {
        if is_combining_mark(ch) {
            if !latin && !text.ends_with(' ') && !text.is_empty() {
                text.push(ch);
            }
        } else if ch.is_alphanumeric() {
            latin = is_latin(ch);
            text.push(ch);
        } else {
            text.push(' ');
            latin = false;
        }
    }
    text.nfc()
        .collect::<String>()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Match {
    direct: bool,
    tier: u8,
    credit: bool,
}

fn in_word(term: &str, word: &str) -> bool {
    word == term
        || (word.starts_with(term)
            && term.graphemes(true).nth(2).is_some()
            && word
                .graphemes(true)
                .zip(term.graphemes(true))
                .all(|(a, b)| a == b))
}

fn in_fields(term: &str, fields: &[Vec<String>]) -> bool {
    fields.iter().flatten().any(|word| in_word(term, word))
}

fn score(
    query: &[String],
    title: &[String],
    fields: &[Vec<String>],
    credits: &[Vec<String>],
    direct: bool,
) -> Option<Match> {
    if query.is_empty() || !query.iter().all(|term| in_fields(term, fields)) {
        return None;
    }
    let tier = if title == query {
        4
    } else if fields
        .iter()
        .any(|field| field.windows(query.len()).any(|window| window == query))
    {
        3
    } else if query
        .iter()
        .all(|term| fields.iter().flatten().any(|word| word == term))
    {
        2
    } else {
        1
    };
    Some(Match {
        direct,
        tier,
        credit: query.iter().any(|term| in_fields(term, credits)),
    })
}

fn own_fields(row: &Row) -> (Vec<Vec<String>>, Vec<Vec<String>>) {
    let credits = row
        .credits
        .iter()
        .map(|person| words(&person.name))
        .collect::<Vec<_>>();
    let mut fields = vec![words(&row.title)];
    fields.extend(credits.iter().cloned());
    fields.extend(row.edition.as_deref().map(words));
    (fields, credits)
}

pub fn matching(query: &[String], row: &Row) -> Option<Match> {
    let (mut fields, mut credits) = own_fields(row);
    if let Some(direct) = score(query, &fields[0], &fields, &credits, true) {
        return Some(direct);
    }
    let parent = row.parent.as_ref()?;
    let (parent_fields, parent_credits) = own_fields(parent);
    fields.extend(parent_fields);
    credits.extend(parent_credits);
    score(query, &fields[0], &fields, &credits, false)
}

pub fn artist_match(query: &[String], row: &Row, releases: &[Match]) -> Option<Match> {
    matching(query, row).or_else(|| {
        releases.iter().copied().max().map(|mut value| {
            value.direct = false;
            value
        })
    })
}

fn natural_lower(a: &str, b: &str) -> Ordering {
    fn chunks(mut input: &str) -> impl Iterator<Item = &str> {
        std::iter::from_fn(move || {
            let digit = input.as_bytes().first()?.is_ascii_digit();
            let end = input
                .char_indices()
                .find(|(_, ch)| ch.is_ascii_digit() != digit)
                .map_or(input.len(), |(index, _)| index);
            let (chunk, remaining) = input.split_at(end);
            input = remaining;
            Some(chunk)
        })
    }
    let mut left = chunks(a);
    let mut right = chunks(b);
    loop {
        match (left.next(), right.next()) {
            (Some(a), Some(b)) => {
                let comparison =
                    if a.as_bytes()[0].is_ascii_digit() && b.as_bytes()[0].is_ascii_digit() {
                        let a = a.trim_start_matches('0');
                        let b = b.trim_start_matches('0');
                        a.len().cmp(&b.len()).then_with(|| a.cmp(b))
                    } else {
                        a.cmp(b)
                    };
                if comparison != Ordering::Equal {
                    return comparison;
                }
            }
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (None, None) => return Ordering::Equal,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Ranked<'a> {
    pub row: &'a Row,
    pub relevance: Match,
    pub date: Option<&'a str>,
}

impl<'a> Ranked<'a> {
    pub fn new(row: &'a Row, relevance: Match) -> Self {
        let date = row
            .parent
            .as_ref()
            .and_then(|parent| parent.date.as_deref())
            .or(row.date.as_deref());
        Self {
            row,
            relevance,
            date,
        }
    }

    fn activity(&self, scores: &Scores) -> (i64, i64) {
        let id = if self.row.key.kind == Kind::Artist {
            Some(&self.row.key.id)
        } else {
            self.row.primary_credit.as_ref()
        };
        id.and_then(|id| scores.get(id))
            .copied()
            .unwrap_or_default()
    }
}

pub fn order(rows: &mut [Ranked<'_>], sort: Sort, scores: &Scores) {
    let mut keyed: Vec<_> = rows
        .iter()
        .map(|item| (*item, item.activity(scores), item.row.title.to_lowercase()))
        .collect();
    keyed.sort_by(|(a, a_act, a_title), (b, b_act, b_title)| {
        b_act
            .cmp(a_act)
            .then_with(|| match sort {
                Sort::Relevance => b
                    .relevance
                    .cmp(&a.relevance)
                    .then_with(|| natural_lower(a_title, b_title)),
                Sort::NameAsc => natural_lower(a_title, b_title),
                Sort::NameDesc => natural_lower(b_title, a_title),
                Sort::Date => b
                    .date
                    .cmp(&a.date)
                    .then_with(|| natural_lower(a_title, b_title)),
            })
            .then_with(|| a.row.key.id.cmp(&b.row.key.id))
    });
    for (dst, (item, _, _)) in rows.iter_mut().zip(keyed) {
        *dst = item;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qobuz::search::row;
    use serde_json::json;

    fn album(id: &str, title: &str, artist: &str, artist_id: u64) -> Row {
        row(
            Kind::Album,
            &json!({"id":id,"title":title,"artist":{"id":artist_id,"name":artist}}),
            None,
        )
        .unwrap()
    }

    #[test]
    fn widths_latin_marks_other_scripts_and_prefixes() {
        assert_eq!(words("Ｂｅｙｏｎｃé—Ⅳ"), ["beyonce", "iv"]);
        assert_ne!(words("عَرَب"), words("عرب"));
        assert_ne!(words("か\u{3099}"), words("か"));
        let candidate = album("01", "Lunar Landscape", "Ensemble", 1);
        assert!(matching(&words("lun ens"), &candidate).is_some());
        assert!(matching(&words("lu"), &candidate).is_none());
        assert!(matching(&words("lu"), &album("2", "Lu", "Ensemble", 1)).is_some());
    }

    #[test]
    fn credit_tie_break_keeps_exact_title_and_activity_precedence() {
        let credited = album("01", "Petal", "Ariana Grande", 1);
        let karaoke = album(
            "02",
            "Petal (Originally Performed by Ariana Grande)",
            "Karaoke Band",
            2,
        );
        let exact = album("03", "Petal Ariana", "Someone Else", 3);
        let query = words("petal ariana");
        let mut rows = [&karaoke, &credited, &exact]
            .map(|row| Ranked::new(row, matching(&query, row).unwrap()));
        order(&mut rows, Sort::Relevance, &Scores::new());
        assert_eq!(
            rows.map(|item| item.row.key.id.as_str()),
            ["03", "01", "02"]
        );
        for mode in [Sort::Relevance, Sort::NameAsc, Sort::NameDesc, Sort::Date] {
            let mut rows = [&karaoke, &credited, &exact]
                .map(|row| Ranked::new(row, matching(&query, row).unwrap()));
            order(
                &mut rows,
                mode,
                &Scores::from([("1".into(), (2, 1)), ("3".into(), (1, 2))]),
            );
            assert_eq!(rows[0].row.key.id, "01");
        }
    }
}
