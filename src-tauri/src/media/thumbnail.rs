use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use url::Url;

use super::{Media, assets::image_data_info};

const MAX_IMAGE_BYTES: usize = 512 * 1024;
const MAX_CACHED_BYTES: usize = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 256;
const DEADLINE: Duration = Duration::from_secs(10);

struct CachedImage {
    key: [u8; 32],
    image: Option<Arc<str>>,
    expires: Instant,
}

#[derive(Default)]
struct CacheEntries {
    entries: VecDeque<CachedImage>,
    cached_bytes: usize,
}

pub(super) struct ThumbnailCache {
    entries: Mutex<CacheEntries>,
    requests: Semaphore,
}

impl Default for ThumbnailCache {
    fn default() -> Self {
        Self {
            entries: Mutex::new(CacheEntries::default()),
            requests: Semaphore::new(2),
        }
    }
}

fn entry_bytes(image: &Option<Arc<str>>) -> usize {
    image.as_ref().map_or(0, |value| value.len())
}

impl ThumbnailCache {
    fn get(&self, key: &[u8; 32]) -> Option<Option<Arc<str>>> {
        let mut cache = self.entries.lock().unwrap();
        let now = Instant::now();
        let mut removed_bytes = 0;
        cache.entries.retain(|entry| {
            let keep = entry.expires > now;
            if !keep {
                removed_bytes += entry_bytes(&entry.image);
            }
            keep
        });
        cache.cached_bytes -= removed_bytes;
        let index = cache.entries.iter().position(|entry| &entry.key == key)?;
        let entry = cache.entries.remove(index)?;
        let image = entry.image.clone();
        cache.entries.push_back(entry);
        Some(image)
    }

    fn put(&self, key: [u8; 32], image: Option<Arc<str>>) {
        let mut cache = self.entries.lock().unwrap();
        let now = Instant::now();
        let mut removed_bytes = 0;
        cache.entries.retain(|entry| {
            let keep = entry.key != key && entry.expires > now;
            if !keep {
                removed_bytes += entry_bytes(&entry.image);
            }
            keep
        });
        cache.cached_bytes = cache.cached_bytes.saturating_sub(removed_bytes);
        let expires = now
            + if image.is_some() {
                Duration::from_secs(3_600)
            } else {
                Duration::from_secs(30)
            };
        cache.cached_bytes += entry_bytes(&image);
        cache.entries.push_back(CachedImage {
            key,
            image,
            expires,
        });
        while cache.entries.len() > MAX_ENTRIES || cache.cached_bytes > MAX_CACHED_BYTES {
            let Some(removed) = cache.entries.pop_front() else {
                break;
            };
            cache.cached_bytes = cache
                .cached_bytes
                .saturating_sub(entry_bytes(&removed.image));
        }
    }
}

fn source_url(source: &str) -> Option<Url> {
    (source.len() <= 4_096)
        .then(|| crate::net_url::parse_https_url(source))
        .flatten()
}

fn data_uri(bytes: &[u8]) -> Option<Arc<str>> {
    if bytes.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let image = image_data_info(bytes).ok()?;
    if image
        .dimensions
        .is_none_or(|(width, height)| width.max(height) > 1_200)
    {
        return None;
    }
    let mime = match image.extension {
        "jpg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    };
    Some(Arc::from(format!(
        "data:{mime};base64,{}",
        STANDARD.encode(bytes)
    )))
}

enum LookupResult {
    Cached(Option<Arc<str>>),
    Fetched(Option<Arc<str>>),
}

impl Media {
    /// The webview only receives a bounded image data URI, never its source URL.
    pub async fn thumbnail(&self, source: &str) -> Option<Arc<str>> {
        let url = source_url(source)?;
        let key: [u8; 32] = Sha256::digest(source.as_bytes()).into();
        if let Some(image) = self.thumbnails.get(&key) {
            return image;
        }
        let result = tokio::time::timeout(DEADLINE, async {
            let _permit = self.thumbnails.requests.acquire().await.ok()?;
            if let Some(image) = self.thumbnails.get(&key) {
                return Some(LookupResult::Cached(image));
            }
            Some(LookupResult::Fetched(self.fetch_thumbnail(url).await))
        })
        .await
        .ok()
        .flatten();
        match result {
            Some(LookupResult::Cached(image)) => image,
            Some(LookupResult::Fetched(image)) => {
                self.thumbnails.put(key, image.clone());
                image
            }
            None => {
                self.thumbnails.put(key, None);
                None
            }
        }
    }

    async fn fetch_thumbnail(&self, mut url: Url) -> Option<Arc<str>> {
        for _ in 0..=5 {
            let mut response = self
                .client
                .get(url.clone())
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .send()
                .await
                .ok()?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)?
                    .to_str()
                    .ok()?;
                let next = response.url().join(location).ok()?;
                url = source_url(next.as_str())?;
                continue;
            }
            if response.status() != reqwest::StatusCode::OK
                || response
                    .content_length()
                    .is_some_and(|size| size == 0 || size > MAX_IMAGE_BYTES as u64)
            {
                return None;
            }
            let capacity = response
                .content_length()
                .map_or(0, |size| (size as usize).min(MAX_IMAGE_BYTES));
            let mut body = Vec::with_capacity(capacity);
            while let Some(chunk) = response.chunk().await.ok()? {
                if body.len() + chunk.len() > MAX_IMAGE_BYTES {
                    return None;
                }
                body.extend_from_slice(&chunk);
            }
            return data_uri(&body);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnails_are_bounded_data_images_with_fixed_cache_limits() {
        let image = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5u0AAAAASUVORK5CYII=").unwrap();
        assert!(
            data_uri(&image)
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert!(data_uri(b"<svg onload='unexpected'>").is_none());
        assert!(data_uri(&vec![0; MAX_IMAGE_BYTES + 1]).is_none());
        assert!(source_url("https://user:secret@example.com/image.jpg").is_none());
        assert!(source_url("file:///image.jpg").is_none());
        let cache = ThumbnailCache::default();
        for index in 0..MAX_ENTRIES + 10 {
            let key: [u8; 32] = Sha256::digest(index.to_le_bytes()).into();
            cache.put(key, Some(Arc::from("a".repeat(90_000))));
        }
        let guard = cache.entries.lock().unwrap();
        assert!(guard.entries.len() <= MAX_ENTRIES);
        assert!(guard.cached_bytes <= MAX_CACHED_BYTES);
    }
}
