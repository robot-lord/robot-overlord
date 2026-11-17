//! The page, embedded (`build.rs`): built from `ui/` by `cargo xtask web
//! build`, or a one-line placeholder without it. Static files need no
//! token: open-source bytes, no secrets.
//!
//! Debug builds read `RLORD_WEB_ASSETS=<dir>` from disk instead, for work
//! on the page (`cargo xtask web watch`).

use std::borrow::Cow;

pub struct Asset {
    pub path: &'static str,
    pub content_type: &'static str,
    pub bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

/// The page's build id: a page from another build reloads.
pub fn build_id() -> String {
    format!("{}-{ASSETS_HASH}", env!("CARGO_PKG_VERSION"))
}

/// The file at URL path `path` (`/` is `/index.html`): its content type
/// and bytes.
pub fn find(path: &str) -> Option<(&'static str, Cow<'static, [u8]>)> {
    let path = if path == "/" { "/index.html" } else { path };
    #[cfg(debug_assertions)]
    if let Some(dir) = std::env::var_os("RLORD_WEB_ASSETS").filter(|d| !d.is_empty()) {
        return from_disk(std::path::Path::new(&dir), path);
    }
    ASSETS
        .iter()
        .find(|a| a.path == path)
        .map(|a| (a.content_type, Cow::Borrowed(a.bytes)))
}

/// A file under `dir` (never outside it).
#[cfg(debug_assertions)]
fn from_disk(dir: &std::path::Path, path: &str) -> Option<(&'static str, Cow<'static, [u8]>)> {
    let rel = path.strip_prefix('/')?;
    if rel
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == ".." || c.contains('\\'))
    {
        return None;
    }
    let root = dir.canonicalize().ok()?;
    let file = root.join(rel).canonicalize().ok()?;
    if !file.starts_with(&root) || !file.is_file() {
        return None;
    }
    let bytes = std::fs::read(&file).ok()?;
    Some((content_type(rel), Cow::Owned(bytes)))
}

/// A content type by extension (as `build.rs` gives them).
#[cfg(debug_assertions)]
fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The values of `attr="…"` attributes in `html`.
#[cfg(test)]
fn attrs<'a>(html: &'a str, attr: &str) -> Vec<&'a str> {
    let pat = format!("{attr}=\"");
    let mut v = vec![];
    let mut rest = html;
    while let Some(i) = rest.find(&pat) {
        rest = &rest[i + pat.len()..];
        if let Some(end) = rest.find('"') {
            v.push(&rest[..end]);
            rest = &rest[end..];
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> String {
        let a = ASSETS.iter().find(|a| a.path == "/index.html").unwrap();
        String::from_utf8(a.bytes.to_vec()).unwrap()
    }

    #[test]
    fn every_file_the_page_names_is_embedded_and_local() {
        let html = index();
        for url in attrs(&html, "src").into_iter().chain(attrs(&html, "href")) {
            assert!(
                !url.contains("//") && !url.contains(':'),
                "not a local file in index.html: {url}"
            );
            let path = url.trim_start_matches('.');
            let path = if path.starts_with('/') {
                path.to_string()
            } else {
                format!("/{path}")
            };
            assert!(
                ASSETS.iter().any(|a| a.path == path),
                "index.html names {url}, not embedded"
            );
        }
    }

    #[test]
    fn no_inline_script_and_no_outside_url() {
        let html = index();
        let mut rest = html.as_str();
        while let Some(i) = rest.find("<script") {
            let tag_end = rest[i..].find('>').unwrap() + i;
            let tag = &rest[i..=tag_end];
            assert!(tag.contains("src="), "inline script in index.html: {tag}");
            let close = rest[tag_end..].find("</script>").map(|c| c + tag_end);
            if let Some(c) = close {
                assert!(rest[tag_end + 1..c].trim().is_empty(), "inline script body");
            }
            rest = &rest[tag_end..];
        }
        for a in ASSETS {
            if a.content_type.starts_with("text/html") || a.content_type.starts_with("text/css") {
                let text = String::from_utf8_lossy(a.bytes);
                assert!(
                    !text.contains("http://") && !text.contains("https://"),
                    "{} names an outside URL",
                    a.path
                );
            }
        }
    }

    #[test]
    fn root_is_the_page_and_unknown_paths_are_not_found() {
        let (ct, bytes) = find("/").unwrap();
        assert!(ct.starts_with("text/html"));
        assert!(!bytes.is_empty());
        assert!(find("/nope.js").is_none());
        assert!(build_id().starts_with(env!("CARGO_PKG_VERSION")));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn from_disk_stays_in_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<p>dev</p>").unwrap();
        std::fs::write(dir.path().join("../secret.txt"), "no").ok();
        let (ct, b) = from_disk(dir.path(), "/index.html").unwrap();
        assert!(ct.starts_with("text/html"));
        assert_eq!(&*b, b"<p>dev</p>");
        assert!(from_disk(dir.path(), "/../secret.txt").is_none());
        assert!(from_disk(dir.path(), "//etc/passwd").is_none());
        assert!(from_disk(dir.path(), "/missing.js").is_none());
    }
}
