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
}
