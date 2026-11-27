use url::Url;

pub fn is_https_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
}

pub fn parse_https_url(raw: &str) -> Option<Url> {
    Url::parse(raw).ok().filter(is_https_url)
}

pub fn is_qobuz_host(url: &Url) -> bool {
    url.host_str()
        .is_some_and(|host| host == "qobuz.com" || host.ends_with(".qobuz.com"))
}
