use super::{
    Credentials, Params, QobuzClient, QobuzError, f64_value, nonempty,
    resources::{identity, thumbnail_source},
    u32_value,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

pub const PAGE_SIZE: usize = 20;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Album,
    Track,
    Artist,
}

impl Kind {
    pub const ALL: [Self; 3] = [Self::Album, Self::Track, Self::Artist];

    fn group(self) -> &'static str {
        match self {
            Self::Album => "albums",
            Self::Track => "tracks",
            Self::Artist => "artists",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Track => "track",
            Self::Artist => "artist",
        }
    }

    fn search_endpoint(self) -> &'static str {
        match self {
            Self::Album => "album/search",
            Self::Track => "track/search",
            Self::Artist => "artist/search",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct Id {
    pub kind: Kind,
    pub id: String,
}

impl Id {
    pub fn valid(&self) -> bool {
        match self.kind {
            Kind::Album => identity(&Value::String(self.id.clone())).as_ref() == Some(&self.id),
            Kind::Track | Kind::Artist => self
                .id
                .parse::<u64>()
                .is_ok_and(|id| id > 0 && id.to_string() == self.id),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Credit {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub key: Id,
    pub title: String,
    pub credits: Vec<Credit>,
    pub byline: Option<String>,
    pub edition: Option<String>,
    pub date: Option<String>,
    pub genre_id: Option<u32>,
    pub bit_depth: Option<u32>,
    pub sample_rate: Option<f64>,
    pub hires: Option<bool>,
    pub explicit: Option<bool>,
    pub streamable: Option<bool>,
    pub track_count: Option<u32>,
    pub disc: Option<u32>,
    pub number: Option<u32>,
    pub duration: Option<u32>,
    #[serde(skip)]
    pub artwork: Option<String>,
    #[serde(skip)]
    pub primary_credit: Option<String>,
    #[serde(skip)]
    pub parent: Option<Arc<Row>>,
    pub release: Option<String>,
}

fn display(value: &Value) -> Option<String> {
    nonempty(value)
        .map(|s| {
            s.chars()
                .filter(|c| !c.is_control())
                .take(512)
                .collect::<String>()
        })
        .filter(|s| !s.trim().is_empty())
}

fn credit(value: &Value) -> Option<Credit> {
    let id = identity(&value["id"])?;
    if !(Id {
        kind: Kind::Artist,
        id: id.clone(),
    })
    .valid()
    {
        return None;
    }
    Some(Credit {
        id,
        name: display(&value["name"])?,
    })
}

fn main_credit(value: &Value) -> bool {
    let roles = &value["roles"];
    let is_main = |role: &Value| {
        role.as_str().is_some_and(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "main-artist" | "main artist"
            )
        })
    };
    is_main(roles)
        || roles
            .as_array()
            .is_some_and(|roles| roles.iter().any(is_main))
}

pub fn row(kind: Kind, value: &Value, parent: Option<Arc<Row>>) -> Option<Row> {
    if value["displayable"].as_bool() == Some(false) {
        return None;
    }
    let key = Id {
        kind,
        id: identity(&value["id"])?,
    };
    if !key.valid() {
        return None;
    }
    let primary = &value[match kind {
        Kind::Album => "artist",
        Kind::Track => "performer",
        Kind::Artist => "name",
    }];
    let primary_credit = credit(primary);
    let mut credits: Vec<_> = primary_credit.iter().cloned().collect();
    for item in value["artists"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| main_credit(item))
    {
        if let Some(person) = credit(item)
            && !credits.iter().any(|current| current.id == person.id)
        {
            credits.push(person);
        }
    }
    let parent = if kind == Kind::Track {
        parent.or_else(|| row(Kind::Album, &value["album"], None).map(Arc::new))
    } else {
        None
    };
    let release = parent.as_ref().map(|row| row.key.id.clone()).or_else(|| {
        (kind == Kind::Track)
            .then(|| identity(&value["album"]["id"]))
            .flatten()
    });
    let date = display(&value["release_date_original"]).filter(|text| {
        let bytes = text.as_bytes();
        bytes.len() == 10
            && bytes.iter().enumerate().all(|(i, b)| {
                if i == 4 || i == 7 {
                    *b == b'-'
                } else {
                    b.is_ascii_digit()
                }
            })
    });
    Some(Row {
        key,
        title: display(
            &value[if kind == Kind::Artist {
                "name"
            } else {
                "title"
            }],
        )?,
        byline: display(&primary["name"]),
        credits,
        edition: display(&value["version"]),
        date,
        genre_id: u32_value(&value["genre"]["id"]),
        bit_depth: u32_value(&value["maximum_bit_depth"]),
        sample_rate: f64_value(&value["maximum_sampling_rate"])
            .filter(|rate| rate.is_finite() && *rate > 0.0),
        hires: value["hires"].as_bool(),
        explicit: value["parental_warning"].as_bool(),
        streamable: value["streamable"].as_bool(),
        track_count: u32_value(&value["tracks_count"]),
        disc: u32_value(&value["media_number"]),
        number: u32_value(&value["track_number"]),
        duration: u32_value(&value["duration"]),
        artwork: thumbnail_source(value)
            .filter(|source| {
                let Some(url) = crate::net_url::parse_https_url(source) else {
                    return false;
                };
                crate::net_url::is_qobuz_host(&url)
                    && !url.query_pairs().any(|(name, _)| {
                        matches!(
                            name.to_ascii_lowercase().as_str(),
                            "user_auth_token"
                                | "token"
                                | "access_token"
                                | "app_secret"
                                | "request_sig"
                        )
                    })
            })
            .or_else(|| parent.as_ref()?.artwork.clone()),
        primary_credit: primary_credit.map(|c| c.id),
        parent,
        release,
    })
}

pub struct Page {
    pub rows: Vec<Row>,
    pub offset: usize,
    pub length: usize,
    pub total: Option<usize>,
    pub ids: Vec<Option<String>>,
}

impl Page {
    fn read(
        container: &Value,
        kind: Kind,
        offset: usize,
        parent: Option<Arc<Row>>,
    ) -> Result<Self, QobuzError> {
        let items = container["items"]
            .as_array()
            .ok_or_else(|| QobuzError::invalid_response("Qobuz omitted a catalog results page."))?;
        let total = container["total"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok());
        let reported_offset = container["offset"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok());
        if reported_offset.is_some_and(|reported| reported != offset) && !items.is_empty() {
            return Err(QobuzError::invalid_response(
                "Qobuz returned a different catalog page than requested.",
            ));
        }
        Ok(Self {
            rows: items
                .iter()
                .filter_map(|item| row(kind, item, parent.clone()))
                .collect(),
            offset,
            length: items.len(),
            total,
            ids: items.iter().map(|item| identity(&item["id"])).collect(),
        })
    }
}

fn paging(offset: usize) -> Params {
    vec![
        ("limit", PAGE_SIZE.to_string()),
        ("offset", offset.to_string()),
    ]
}

impl QobuzClient {
    pub async fn search_catalog(
        &self,
        query: &str,
        account: &Credentials,
    ) -> Result<Vec<(Kind, Result<Page, QobuzError>)>, QobuzError> {
        let mut params = paging(0);
        params.push(("query", query.to_owned()));
        let response = self.metadata("catalog/search", params, account).await?;
        Ok(Kind::ALL
            .into_iter()
            .map(|kind| (kind, Page::read(&response[kind.group()], kind, 0, None)))
            .collect())
    }

    pub async fn search_page(
        &self,
        query: &str,
        kind: Kind,
        offset: usize,
        account: &Credentials,
    ) -> Result<Page, QobuzError> {
        let mut params = paging(offset);
        params.push(("query", query.to_owned()));
        let response = self
            .metadata(kind.search_endpoint(), params, account)
            .await?;
        Page::read(&response[kind.group()], kind, offset, None)
    }

    pub async fn search_detail(
        &self,
        key: &Id,
        offset: usize,
        account: &Credentials,
    ) -> Result<(Row, Option<Page>), QobuzError> {
        let mut params = paging(offset);
        let (endpoint, id_param, collection) = match key.kind {
            Kind::Album => ("album/get", "album_id", Some(Kind::Track)),
            Kind::Track => ("track/get", "track_id", None),
            Kind::Artist => {
                params.push(("extra", "albums".into()));
                ("artist/get", "artist_id", Some(Kind::Album))
            }
        };
        params.push((id_param, key.id.clone()));
        let response = self.metadata(endpoint, params, account).await?;
        let detail = row(key.kind, &response, None)
            .filter(|row| row.key == *key)
            .ok_or_else(|| {
                QobuzError::invalid_response("Qobuz did not return the requested catalog item.")
            })?;
        let page = collection
            .map(|kind| {
                let parent = (key.kind == Kind::Album).then(|| Arc::new(detail.clone()));
                Page::read(&response[kind.group()], kind, offset, parent)
            })
            .transpose()?;
        Ok((detail, page))
    }
}

#[cfg(test)]
mod tests;
