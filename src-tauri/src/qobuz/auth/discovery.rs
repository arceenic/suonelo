use super::super::{JSON_LIMIT, PLAYER, QobuzClient, QobuzError, bounded_body};
use super::{Candidates, validation_error};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use regex::Regex;
use reqwest::StatusCode;
use std::sync::LazyLock;
use url::Url;
use zeroize::{Zeroize, Zeroizing};

static SCRIPTS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<script\b[^>]*?\ssrc\s*=\s*(?:"([^"]+)"|'([^']+)')"#).unwrap()
});
static ALLOWED_BUNDLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/resources/[A-Za-z0-9._-]+(?:/[A-Za-z0-9._-]+)*/bundle\.js$").unwrap()
});
static PRODUCTION_APP_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:^|[,{;\s])(?:production|"production"|'production')\s*:\s*\{\s*(?:api|["']api["'])\s*:\s*\{\s*(?:appId|["']appId["'])\s*:\s*"#).unwrap()
});
static QUOTED_APP_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^(?:"([0-9]{9})"|'([0-9]{9})')\s*[,}]"#).unwrap()
});
static INITIAL_SEED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\.initialSeed\(["']([a-zA-Z0-9+/=_-]+)["'],\s*window\.utimezone\.([a-zA-Z]+)\)"#)
        .unwrap()
});
static ZONE_PARTS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"name\s*:\s*["'][a-zA-Z_]+/([a-zA-Z]+)["'],\s*info\s*:\s*["']([a-zA-Z0-9+/=_-]+)["'],\s*extras\s*:\s*["']([a-zA-Z0-9+/=_-]+)["']"#).unwrap()
});
static EXCHANGE_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"authenticate\(\{\s*privateKey\s*:\s*"([A-Za-z0-9_-]{8,128})"\s*,\s*code\s*:"#)
        .unwrap()
});

impl QobuzClient {
    pub(in crate::qobuz) async fn discover_app_credentials(&self) -> Result<Candidates, QobuzError> {
        let bundle = self.player_bundle().await?;
        parse_player_bundle(&bundle).ok_or_else(discovery_unavailable)
    }

    pub(in crate::qobuz) async fn player_bundle(&self) -> Result<Zeroizing<String>, QobuzError> {
        let page = self.player_text("/login", 2 * 1024 * 1024).await?;
        let path = player_bundle_path(&page).ok_or_else(discovery_unavailable)?;
        self.player_text(path, JSON_LIMIT).await
    }

    async fn player_text(&self, path: &str, limit: usize) -> Result<Zeroizing<String>, QobuzError> {
        #[cfg(all(target_os = "macos", test))]
        if let Some(fixture) = &self.fixture {
            return (fixture.player_text)(path);
        }
        const PLAYER_HOST: &str = "play.qobuz.com";
        if self.recovery.host_is_throttled(PLAYER_HOST) {
            return Err(validation_error(QobuzError::new("rate_limit", "", true)));
        }
        // The API client uses HTTPS and refuses redirects. No auth headers or query are added here.
        let response = self
            .api
            .get(format!("{PLAYER}{path}"))
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await
            .map_err(|_| discovery_unavailable())?;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            self.recovery
                .rate_limited_from_headers(PLAYER_HOST, response.headers());
            return Err(validation_error(QobuzError::new("rate_limit", "", true)));
        }
        if !response.status().is_success() {
            return Err(discovery_unavailable());
        }
        let bytes = Zeroizing::new(
            bounded_body(response, limit)
                .await
                .map_err(|_| discovery_unavailable())?,
        );
        let body = std::str::from_utf8(&bytes).map_err(|_| discovery_unavailable())?;
        Ok(Zeroizing::new(body.to_owned()))
    }
}

pub(super) fn discovery_unavailable() -> QobuzError {
    QobuzError::new(
        "credential_discovery",
        "Suonelo could not complete automatic Qobuz setup. Try signing in again shortly.",
        true,
    )
}

pub(super) fn player_bundle_path(page: &str) -> Option<&str> {
    let mut result = None;
    for script in SCRIPTS_RE.captures_iter(page) {
        let source = script.get(1).or_else(|| script.get(2))?.as_str();
        if !source.split(['?', '#']).next()?.ends_with("/bundle.js") {
            continue;
        }
        if !ALLOWED_BUNDLE_RE.is_match(source)
            || source.split('/').any(|part| matches!(part, "." | ".."))
        {
            return None;
        }
        let url = Url::parse(&format!("{PLAYER}{source}")).ok()?;
        if url.path() != source || result.is_some_and(|previous| previous != source) {
            return None;
        }
        result = Some(source);
    }
    result
}

pub(in crate::qobuz) fn parse_player_bundle(bundle: &str) -> Option<Candidates> {
    let mut app_id = None;
    for field in PRODUCTION_APP_ID_RE.find_iter(bundle) {
        let capture = QUOTED_APP_ID_RE.captures(bundle.get(field.end()..)?)?;
        let id = capture.get(1).or_else(|| capture.get(2))?.as_str();
        if app_id.is_some_and(|previous| previous != id) {
            return None;
        }
        app_id = Some(id);
    }
    let app_id = app_id?.to_owned();
    let mut seeds: Vec<_> = INITIAL_SEED_RE.captures_iter(bundle).collect();
    if seeds.len() > 1 {
        seeds.rotate_left(1);
    }
    let parts: Vec<_> = ZONE_PARTS_RE.captures_iter(bundle).collect();
    let decoded = seeds.iter().filter_map(|seed| {
        let parts = parts
            .iter()
            .find(|part| part[1].eq_ignore_ascii_case(&seed[2]))?;
        let joined = Zeroizing::new([&seed[1], &parts[2], &parts[3]].concat());
        let encoded = joined.get(..joined.len().checked_sub(44)?)?;
        let decoded = Zeroizing::new(
            STANDARD
                .decode(encoded)
                .or_else(|_| STANDARD_NO_PAD.decode(encoded))
                .ok()?,
        );
        let secret = std::str::from_utf8(&decoded).ok()?;
        (!secret.is_empty()).then(|| secret.to_owned())
    });
    let mut secrets = Vec::new();
    for mut secret in decoded {
        if secrets.contains(&secret) {
            secret.zeroize();
        } else {
            secrets.push(secret);
        }
    }
    (!secrets.is_empty()).then_some(Candidates { app_id, secrets })
}

pub(in crate::qobuz) fn exchange_key(bundle: &str) -> Option<Zeroizing<String>> {
    // This exchange value is delivered by Qobuz to every web player.
    // Read it from the same bundle as the app ID instead of embedding a value.
    let mut values = EXCHANGE_KEY_RE
        .captures_iter(bundle)
        .filter_map(|capture| capture.get(1).map(|value| value.as_str()));
    let key = values.next()?;
    values
        .all(|value| value == key)
        .then(|| Zeroizing::new(key.to_owned()))
}
