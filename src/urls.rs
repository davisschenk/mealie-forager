use url::Url;

const TRACKING_PARAMS: &[&str] = &[
    "igsh",
    "igshid",
    "si",
    "feature",
    "fbclid",
    "gclid",
    "_r",
    "_t",
    "is_from_webapp",
    "sender_device",
    "share_app_id",
    "share_link_id",
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
];

/// Hosts whose posts go through yt-dlp and the model instead of Mealie's scraper.
const SOCIAL_HOSTS: &[&str] = &[
    "tiktok.com",
    "instagram.com",
    "youtube.com",
    "youtu.be",
    "facebook.com",
    "fb.watch",
    "pinterest.com",
    "pin.it",
    "x.com",
    "twitter.com",
    "threads.net",
    "threads.com",
    "reddit.com",
    "redd.it",
    "vimeo.com",
    "snapchat.com",
];

/// Where a recipe link comes from, for the source tag: `(host, name)`. Hosts
/// not listed here are "Website".
const SOURCE_NAMES: &[(&str, &str)] = &[
    ("tiktok.com", "TikTok"),
    ("instagram.com", "Instagram"),
    ("youtube.com", "YouTube"),
    ("youtu.be", "YouTube"),
    ("facebook.com", "Facebook"),
    ("fb.watch", "Facebook"),
    ("pinterest.com", "Pinterest"),
    ("pin.it", "Pinterest"),
    ("x.com", "X"),
    ("twitter.com", "X"),
    ("threads.net", "Threads"),
    ("threads.com", "Threads"),
    ("reddit.com", "Reddit"),
    ("redd.it", "Reddit"),
    ("vimeo.com", "Vimeo"),
    ("snapchat.com", "Snapchat"),
];

fn host(raw: &str) -> Option<String> {
    Url::parse(raw)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
}

/// Whether `host` is `site` or one of its subdomains.
fn on_site(host: &str, site: &str) -> bool {
    host == site
        || host.strip_suffix(site).is_some_and(|rest| rest.ends_with('.'))
        // pinterest.co.uk, pinterest.de, …
        || (site.starts_with("pinterest.") && host.split('.').any(|l| l == "pinterest"))
}

/// Whether the URL is a social-media post rather than a recipe web page.
pub fn is_social(raw: &str) -> bool {
    let Some(host) = host(raw) else {
        return false;
    };
    SOCIAL_HOSTS.iter().any(|s| on_site(&host, s))
}

/// The name of the site a recipe link comes from ("TikTok", "Instagram", …),
/// or "Website" for any other page. None when `raw` isn't a URL.
pub fn source_name(raw: &str) -> Option<&'static str> {
    let host = host(raw)?;
    Some(
        SOURCE_NAMES
            .iter()
            .find(|(site, _)| on_site(&host, site))
            .map_or("Website", |(_, name)| *name),
    )
}

/// Pulls the first http(s) URL out of text shared from a mobile app.
pub fn extract(text: &str) -> Option<String> {
    text.split_whitespace()
        .filter_map(|word| {
            let start = word.find("http://").or_else(|| word.find("https://"))?;
            let candidate = word[start..].trim_end_matches(|c: char| {
                matches!(c, '.' | ',' | ')' | ']' | '"' | '\'' | '!' | '?' | '>')
            });
            normalize(candidate)
        })
        .next()
}

/// Drops share-tracking parameters so the same post always maps to one URL.
pub fn normalize(raw: &str) -> Option<String> {
    let mut url = Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    url.set_fragment(None);
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !TRACKING_PARAMS.contains(&k.as_ref()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(kept);
    }
    Some(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tracking_but_keeps_meaningful_params() {
        assert_eq!(
            normalize("https://www.instagram.com/reel/abc/?igsh=xyz&utm_source=ig").unwrap(),
            "https://www.instagram.com/reel/abc/"
        );
        assert_eq!(
            normalize("https://www.youtube.com/watch?v=123&si=foo#t=3").unwrap(),
            "https://www.youtube.com/watch?v=123"
        );
        assert!(normalize("ftp://example.com").is_none());
        assert!(normalize("not a url").is_none());
    }

    #[test]
    fn extracts_url_from_shared_text() {
        assert_eq!(
            extract("Check out this reel! https://www.tiktok.com/@chef/video/42?_r=1.").unwrap(),
            "https://www.tiktok.com/@chef/video/42"
        );
        assert_eq!(
            extract("(https://youtu.be/abc)").unwrap(),
            "https://youtu.be/abc"
        );
        assert!(extract("no links here").is_none());
    }

    #[test]
    fn classifies_social_hosts() {
        assert!(is_social("https://www.tiktok.com/@chef/video/42"));
        assert!(is_social("https://vm.tiktok.com/abc"));
        assert!(is_social("https://youtu.be/abc"));
        assert!(is_social("https://www.pinterest.co.uk/pin/1"));
        assert!(!is_social("https://www.seriouseats.com/garlic-noodles"));
        assert!(!is_social("https://notyoutube.com/x"));
    }

    #[test]
    fn names_link_sources() {
        assert_eq!(source_name("https://vm.tiktok.com/abc"), Some("TikTok"));
        assert_eq!(
            source_name("https://www.instagram.com/reel/abc/"),
            Some("Instagram")
        );
        assert_eq!(source_name("https://youtu.be/abc"), Some("YouTube"));
        assert_eq!(
            source_name("https://www.pinterest.de/pin/1"),
            Some("Pinterest")
        );
        assert_eq!(
            source_name("https://www.seriouseats.com/garlic-noodles"),
            Some("Website")
        );
        assert_eq!(source_name(""), None);
    }
}
