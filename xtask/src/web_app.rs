//! The phone app's page (`crates/web/ui/dist-app/`, from `cargo xtask web
//! build --app`): finishing the build, its asset table for the apps'
//! shells, and the checks on what it may contain. See
//! docs/working/web-ui-remote.md, "The WebView: CSP, Trusted Types, no
//! network".
//!
//! The page runs in the app's WebView with no network: its only way out is
//! the shell's message bridge to the native engine. The checks hold the
//! built files to that: every file local, no URL with a network scheme
//! anywhere, no network API in its scripts, the CSP `<meta>` with
//! `connect-src 'none'` and `trusted-types 'none'`, and neither xterm.js nor
//! the local page's transport bundled. And the local page's bundle holds
//! none of the phone's bridge code.
//!
//! Neither bundle holds the dev bridge (`src/devBridge.ts`, the shell
//! played over `rlord-device bridge`'s loopback websocket): it is built
//! only into `dist-app-dev/` (`cargo xtask app dev`), whose page has the
//! phone page's policy but for `connect-src`, which names that one
//! websocket ([`check_dev`]).

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// The asset table the shells read (iOS's scheme handler, Android's asset
/// loader): every file the page may load, with its content type.
pub const MANIFEST: &str = "manifest.json";

/// XML namespace names: they look like URLs but are only names (Preact
/// creates SVG and MathML elements in them). Nothing is fetched.
const NAMESPACES: &[&str] = &[
    "http://www.w3.org/2000/svg",
    "http://www.w3.org/1999/xhtml",
    "http://www.w3.org/1998/Math/MathML",
    "http://www.w3.org/1999/xlink",
    "http://www.w3.org/XML/1998/namespace",
];

/// Ways a script could reach the network (or start code that could).
const NETWORK_APIS: &[&str] = &[
    "WebSocket",
    "XMLHttpRequest",
    "EventSource",
    "sendBeacon",
    "RTCPeerConnection",
    "WebTransport",
    "importScripts",
    "fetch(",
    "Worker(",
];

/// Each transport carries one of these (`static marker`), so a bundle
/// shows which it has even minified.
const LOCAL_MARKER: &str = "rlord-local-transport";
const REMOTE_MARKER: &str = "rlord-remote-transport";
/// The dev bridge's (`DEV_MARKER` in `src/devBridge.ts`).
const DEV_MARKER: &str = "rlord-dev-bridge";

/// CSP directives the phone page's `<meta>` must have, exactly.
const CSP_REQUIRED: &[(&str, &str)] = &[
    ("default-src", "'none'"),
    ("script-src", "'self'"),
    ("style-src", "'self'"),
    ("connect-src", "'none'"),
    ("base-uri", "'none'"),
    ("form-action", "'none'"),
    ("require-trusted-types-for", "'script'"),
    ("trusted-types", "'none'"),
];

/// After `vite build --mode app`: the page as `index.html` (what the
/// shells load at the root), the asset table, then the checks.
pub fn finish(dist: &Path) -> Result<()> {
    let built = dist.join("app.html");
    if built.exists() {
        std::fs::rename(&built, dist.join("index.html"))
            .with_context(|| format!("renaming {}", built.display()))?;
    }
    let files = read(dist)?;
    let index = files
        .iter()
        .find(|(p, _)| p == "/index.html")
        .context("the phone page's build has no index.html")?;
    let meta = csp_meta(&String::from_utf8_lossy(&index.1))
        .context("index.html has no Content-Security-Policy <meta>")?;
    let sizes: Vec<(String, usize)> = files.iter().map(|(p, b)| (p.clone(), b.len())).collect();
    let text = serde_json::to_string_pretty(&manifest(&sizes, &header_csp(&meta)))? + "\n";
    std::fs::write(dist.join(MANIFEST), text)
        .with_context(|| format!("writing {}", dist.join(MANIFEST).display()))?;
    check(dist)
}

/// The phone page's checks, on a build already there.
pub fn check(dist: &Path) -> Result<()> {
    let files = read(dist)?;
    let found = problems(&files);
    if !found.is_empty() {
        bail!(
            "the phone page ({}) breaks its rules:\n  {}",
            dist.display(),
            found.join("\n  ")
        );
    }
    println!(
        "checked {}: {} files, all local, no network",
        dist.display(),
        files.len()
    );
    Ok(())
}

/// The local page's bundle: none of the phone's bridge code.
pub fn check_local(dist: &Path) -> Result<()> {
    let found = local_problems(&read(dist)?);
    if !found.is_empty() {
        bail!(
            "the local page ({}) carries the phone's code:\n  {}",
            dist.display(),
            found.join("\n  ")
        );
    }
    Ok(())
}

/// The dev page (`dist-app-dev/app-dev.html`, built for a desktop
/// browser by `cargo xtask app dev`): the phone page's policy, but
/// `connect-src` names the bridge's websocket on `port` and nothing else.
pub fn check_dev(dist: &Path, port: u16) -> Result<()> {
    let page = dist.join("app-dev.html");
    let html =
        std::fs::read_to_string(&page).with_context(|| format!("reading {}", page.display()))?;
    let policy = csp_meta(&html).context("app-dev.html has no Content-Security-Policy <meta>")?;
    let found = csp_problems_for(&policy, &dev_connect(port));
    if !found.is_empty() {
        bail!("the dev page's CSP is wrong:\n  {}", found.join("\n  "));
    }
    Ok(())
}

/// What the dev page may connect to: the bridge, nothing else.
fn dev_connect(port: u16) -> String {
    format!("ws://127.0.0.1:{port}")
}

/// Every file under `dist` but the manifest, as (`/path`, bytes), sorted.
fn read(dist: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
        for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let p = entry?.path();
            if p.is_dir() {
                walk(root, &p, out)?;
                continue;
            }
            let rel = p.strip_prefix(root)?.to_string_lossy().replace('\\', "/");
            if rel == MANIFEST {
                continue;
            }
            let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
            out.push((format!("/{rel}"), bytes));
        }
        Ok(())
    }
    if !dist.is_dir() {
        bail!(
            "{} is missing: run `cargo xtask web build{}`",
            dist.display(),
            if dist.ends_with("dist-app") {
                " --app"
            } else {
                ""
            }
        );
    }
    let mut out = vec![];
    walk(dist, dist, &mut out)?;
    out.sort();
    Ok(out)
}

/// A file's content type, by its extension (`crates/web/build.rs`'s table).
pub fn content_type(path: &str) -> &'static str {
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
        "txt" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The asset table: the page's root file, the CSP the shell sends as a
/// header, and each file's path (from the root, with a leading `/`),
/// content type and size.
pub fn manifest(files: &[(String, usize)], csp: &str) -> Value {
    let files: Vec<Value> = files
        .iter()
        .map(|(p, n)| json!({"path": p, "type": content_type(p), "bytes": n}))
        .collect();
    json!({"index": "/index.html", "csp": csp, "files": files})
}

/// The policy as a response header: the `<meta>`'s, plus what only a
/// header can say.
pub fn header_csp(meta: &str) -> String {
    format!(
        "{}; frame-ancestors 'none'",
        meta.trim().trim_end_matches(';')
    )
}

/// The `content` of the page's Content-Security-Policy `<meta>`.
pub fn csp_meta(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find("<meta") {
        let start = from + i;
        let end = start + lower[start..].find('>')?;
        let tag = &html[start..=end];
        if tag
            .to_ascii_lowercase()
            .contains("http-equiv=\"content-security-policy\"")
        {
            return attr(tag, "content");
        }
        from = end;
    }
    None
}

/// A double-quoted attribute's value in a tag.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let pat = format!("{name}=\"");
    let mut from = 0;
    while let Some(i) = lower[from..].find(&pat) {
        let at = from + i;
        // A whole attribute name, not the end of another (`data-src`).
        let before = lower[..at].chars().next_back();
        if before.is_some_and(char::is_whitespace) {
            let v = &tag[at + pat.len()..];
            return Some(v[..v.find('"')?].to_string());
        }
        from = at + pat.len();
    }
    None
}

/// Every `name="…"` value in `html`.
fn attrs(html: &str, name: &str) -> Vec<String> {
    let mut out = vec![];
    let mut rest = html;
    while let Some(i) = rest.find('<') {
        rest = &rest[i..];
        let Some(end) = rest.find('>') else { break };
        if let Some(v) = attr(&rest[..=end], name) {
            out.push(v);
        }
        rest = &rest[end..];
    }
    out
}

/// What is wrong with the CSP `policy` for the phone page.
fn csp_problems(policy: &str) -> Vec<String> {
    csp_problems_for(policy, "'none'")
}

/// [`csp_problems`] with `connect` as `connect-src` (the dev page's).
fn csp_problems_for(policy: &str, connect: &str) -> Vec<String> {
    let mut out = vec![];
    let directives: Vec<(String, String)> = policy
        .split(';')
        .filter_map(|d| {
            let mut words = d.split_whitespace();
            let name = words.next()?.to_ascii_lowercase();
            Some((name, words.collect::<Vec<_>>().join(" ")))
        })
        .collect();
    for &(name, want) in CSP_REQUIRED {
        let want = if name == "connect-src" { connect } else { want };
        match directives.iter().find(|(n, _)| n == name) {
            Some((_, v)) if v == want => {}
            Some((_, v)) => out.push(format!("CSP {name} is `{v}`, not `{want}`")),
            None => out.push(format!("CSP has no {name} (want `{want}`)")),
        }
    }
    for bad in [
        "'unsafe-inline'",
        "'unsafe-eval'",
        "'wasm-unsafe-eval'",
        "*",
    ] {
        if directives
            .iter()
            .any(|(_, v)| v.split_whitespace().any(|w| w == bad))
        {
            out.push(format!("CSP allows {bad}"));
        }
    }
    out
}

/// Every `scheme://…` in `text` that isn't an XML namespace name.
fn network_urls(text: &str) -> Vec<String> {
    let mut out = vec![];
    let mut from = 0;
    while let Some(i) = text[from..].find("://") {
        let at = from + i;
        let scheme_start = text[..at]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map_or(0, |j| j + 1);
        let end = text[at..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ')' | '<' | '>'))
            .map_or(text.len(), |j| at + j);
        let url = &text[scheme_start..end];
        if !NAMESPACES.contains(&url) {
            out.push(url.chars().take(80).collect());
        }
        from = at + 3;
    }
    out
}

/// What is wrong with the phone page's files (`/path`, bytes).
pub fn problems(files: &[(String, Vec<u8>)]) -> Vec<String> {
    let mut out = vec![];
    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    match files.iter().find(|(p, _)| p == "/index.html") {
        None => out.push("no /index.html".to_string()),
        Some((_, bytes)) => {
            let html = String::from_utf8_lossy(bytes);
            match csp_meta(&html) {
                None => out.push("index.html has no Content-Security-Policy <meta>".into()),
                Some(policy) => out.extend(csp_problems(&policy)),
            }
            for url in attrs(&html, "src").into_iter().chain(attrs(&html, "href")) {
                if url.contains(':') || url.starts_with("//") {
                    out.push(format!("index.html names a file that isn't local: {url}"));
                    continue;
                }
                let path = format!("/{}", url.trim_start_matches("./").trim_start_matches('/'));
                if !paths.contains(&path.as_str()) {
                    out.push(format!("index.html names {url}, which isn't in the build"));
                }
            }
            if inline_script(&html) {
                out.push("index.html has an inline script".into());
            }
        }
    }
    let mut remote = false;
    for (path, bytes) in files {
        let text = String::from_utf8_lossy(bytes);
        if content_type(path) == "application/octet-stream" {
            out.push(format!("{path}: a file of unknown type"));
        }
        for url in network_urls(&text) {
            out.push(format!("{path} names a URL: {url}"));
        }
        if text.to_ascii_lowercase().contains("xterm") {
            out.push(format!("{path} carries xterm.js"));
        }
        if text.contains(LOCAL_MARKER) {
            out.push(format!("{path} carries LocalTransport"));
        }
        if text.contains(DEV_MARKER) {
            out.push(format!("{path} carries the dev bridge"));
        }
        remote |= text.contains(REMOTE_MARKER);
        if path.ends_with(".js") || path.ends_with(".mjs") || path.ends_with(".html") {
            for api in NETWORK_APIS {
                if text.contains(api) {
                    out.push(format!("{path} uses {api}"));
                }
            }
        }
    }
    if !remote {
        out.push("no file carries RemoteTransport: is this the phone page?".into());
    }
    out
}

/// Whether `html` has a `<script>` without `src` or with a body.
fn inline_script(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    let mut rest = lower.as_str();
    while let Some(i) = rest.find("<script") {
        rest = &rest[i..];
        let Some(tag_end) = rest.find('>') else {
            return true;
        };
        if attr(&rest[..=tag_end], "src").is_none() {
            return true;
        }
        let body = &rest[tag_end + 1..];
        match body.find("</script>") {
            Some(c) if body[..c].trim().is_empty() => rest = &body[c..],
            _ => return true,
        }
    }
    false
}

/// What of the phone's code is in the local page's files.
pub fn local_problems(files: &[(String, Vec<u8>)]) -> Vec<String> {
    let mut out = vec![];
    for (path, bytes) in files {
        let text = String::from_utf8_lossy(bytes);
        for mark in [REMOTE_MARKER, DEV_MARKER, "__rlord_recv", "messageHandlers"] {
            if text.contains(mark) {
                out.push(format!("{path} carries {mark}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const META: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
        img-src 'self' data:; font-src 'self'; connect-src 'none'; base-uri 'none'; \
        form-action 'none'; require-trusted-types-for 'script'; trusted-types 'none'";

    fn page(policy: &str, head: &str) -> String {
        format!(
            "<!doctype html><html><head>\n<meta\n  http-equiv=\"Content-Security-Policy\"\n  \
             content=\"{policy}\"\n/>{head}</head><body><div id=\"app\"></div></body></html>"
        )
    }

    /// A good build: the page, its script and stylesheet.
    fn good() -> Vec<(String, Vec<u8>)> {
        let head = "<script type=\"module\" crossorigin src=\"/assets/app-1.js\"></script>\
                    <link rel=\"stylesheet\" crossorigin href=\"/assets/app-1.css\">";
        vec![
            (
                "/assets/app-1.css".into(),
                b".a{color:red}".to_vec(),
            ),
            (
                "/assets/app-1.js".into(),
                b"var n=\"http://www.w3.org/2000/svg\";class R{static marker=\"rlord-remote-transport\"};\
                  window.webkit.messageHandlers.rlord.postMessage(x);e.innerHTML"
                    .to_vec(),
            ),
            ("/index.html".into(), page(META, head).into_bytes()),
        ]
    }

    fn with(files: &[(String, Vec<u8>)], path: &str, bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut v: Vec<_> = files.iter().filter(|(p, _)| p != path).cloned().collect();
        v.push((path.to_string(), bytes.to_vec()));
        v
    }

    #[test]
    fn a_good_build_passes() {
        assert_eq!(problems(&good()), Vec::<String>::new());
    }

    #[test]
    fn urls_with_a_network_scheme_fail() {
        for planted in [
            "fetch_url=\"https://evil.example/x\"",
            "u='wss://relay.example'",
            "x=`ws://127.0.0.1:9/ws`",
            "background:url(http://cdn.example/a.png)",
            "s=\"ftp://files\"",
        ] {
            let js = format!("class R{{static marker=\"rlord-remote-transport\"}};{planted}");
            let got = problems(&with(&good(), "/assets/app-1.js", js.as_bytes()));
            assert!(
                got.iter().any(|p| p.contains("names a URL")),
                "{planted}: {got:?}"
            );
        }
        // In a stylesheet, and in the licenses file too: every file.
        let got = problems(&with(
            &good(),
            "/THIRD-PARTY-LICENSES.md",
            b"see https://example.com",
        ));
        assert!(got.iter().any(|p| p.contains("LICENSES.md names a URL")));
        // A namespace name is not a URL to anything.
        assert!(network_urls("a=\"http://www.w3.org/1999/xhtml\"").is_empty());
        assert_eq!(
            network_urls("a=\"http://www.w3.org/2000/svgx\""),
            ["http://www.w3.org/2000/svgx"]
        );
    }

    #[test]
    fn network_apis_xterm_and_the_local_transport_fail() {
        for (planted, want) in [
            ("new WebSocket(u)", "uses WebSocket"),
            ("fetch(u)", "uses fetch("),
            ("new XMLHttpRequest", "uses XMLHttpRequest"),
            ("navigator.sendBeacon(u)", "uses sendBeacon"),
            ("new Worker(u)", "uses Worker("),
            ("class X{}/* xterm-helper-textarea */", "carries xterm.js"),
            (
                "static marker=\"rlord-local-transport\"",
                "carries LocalTransport",
            ),
            ("const D=\"rlord-dev-bridge\"", "carries the dev bridge"),
        ] {
            let js = format!("class R{{static marker=\"rlord-remote-transport\"}};{planted}");
            let got = problems(&with(&good(), "/assets/app-1.js", js.as_bytes()));
            assert!(got.iter().any(|p| p.contains(want)), "{planted}: {got:?}");
        }
        // Not the phone page at all.
        let got = problems(&with(&good(), "/assets/app-1.js", b"var a=1"));
        assert!(got.iter().any(|p| p.contains("RemoteTransport")), "{got:?}");
    }

    #[test]
    fn the_csp_must_shut_the_network_and_trusted_types() {
        let head = "<script type=\"module\" src=\"/assets/app-1.js\"></script>";
        for (policy, want) in [
            (
                META.replace("connect-src 'none'", "connect-src 'self'"),
                "connect-src",
            ),
            (
                META.replace("; trusted-types 'none'", ""),
                "no trusted-types",
            ),
            (
                META.replace("style-src 'self'", "style-src 'self' 'unsafe-inline'"),
                "'unsafe-inline'",
            ),
            (
                META.replace("require-trusted-types-for 'script'; ", ""),
                "require-trusted-types-for",
            ),
            (
                META.replace("default-src 'none'", "default-src *"),
                "default-src",
            ),
        ] {
            let got = problems(&with(
                &good(),
                "/index.html",
                page(&policy, head).as_bytes(),
            ));
            assert!(got.iter().any(|p| p.contains(want)), "{want}: {got:?}");
        }
        let got = problems(&with(&good(), "/index.html", b"<html><head></head></html>"));
        assert!(got.iter().any(|p| p.contains("no Content-Security-Policy")));
    }

    #[test]
    fn the_page_names_only_local_files_it_has() {
        for (head, want) in [
            (
                "<script type=\"module\" src=\"https://cdn.example/app.js\"></script>",
                "isn't local",
            ),
            (
                "<link rel=\"stylesheet\" href=\"//cdn.example/a.css\">",
                "isn't local",
            ),
            (
                "<script type=\"module\" src=\"/assets/gone.js\"></script>",
                "isn't in the build",
            ),
            ("<script>alert(1)</script>", "inline script"),
            (
                "<script src=\"/assets/app-1.js\">alert(1)</script>",
                "inline script",
            ),
        ] {
            let got = problems(&with(&good(), "/index.html", page(META, head).as_bytes()));
            assert!(got.iter().any(|p| p.contains(want)), "{head}: {got:?}");
        }
        // `data-src` isn't `src`.
        assert_eq!(
            attr("<img data-src=\"x\" src=\"y\">", "src").as_deref(),
            Some("y")
        );
        let got = problems(&with(&good(), "/assets/blob.bin", b"x"));
        assert!(got.iter().any(|p| p.contains("unknown type")), "{got:?}");
    }

    #[test]
    fn the_local_page_has_no_bridge() {
        let local = vec![(
            "/assets/index-1.js".to_string(),
            b"class L{static marker=\"rlord-local-transport\"};new WebSocket(u)".to_vec(),
        )];
        assert!(local_problems(&local).is_empty());
        let mixed = with(
            &local,
            "/assets/index-1.js",
            b"window.__rlord_recv=f;class R{static marker=\"rlord-remote-transport\"}",
        );
        assert_eq!(local_problems(&mixed).len(), 2);
    }

    #[test]
    fn the_manifest_lists_every_file_and_the_header_policy() {
        let m = manifest(
            &[
                ("/assets/app-1.js".into(), 10),
                ("/index.html".into(), 3),
                ("/THIRD-PARTY-LICENSES.md".into(), 5),
            ],
            &header_csp(META),
        );
        assert_eq!(m["index"], "/index.html");
        assert_eq!(
            m["files"][0],
            json!({"path": "/assets/app-1.js", "type": "text/javascript; charset=utf-8", "bytes": 10})
        );
        assert_eq!(m["files"][1]["type"], "text/html; charset=utf-8");
        assert_eq!(m["files"][2]["type"], "text/markdown; charset=utf-8");
        let csp = m["csp"].as_str().unwrap();
        assert!(csp.starts_with("default-src 'none';"));
        assert!(csp.ends_with("; frame-ancestors 'none'"));
        assert!(csp_problems(csp).is_empty());
        assert_eq!(
            csp_meta(&page(META, "")).as_deref(),
            Some(META),
            "the meta's content, read back"
        );
    }

    #[test]
    fn the_dev_page_connects_to_the_bridge_only() {
        let dev = META.replace("connect-src 'none'", "connect-src ws://127.0.0.1:4100");
        assert!(csp_problems_for(&dev, &dev_connect(4100)).is_empty());
        // Another port, more than the bridge, or the phone page's policy.
        for policy in [
            META.replace("connect-src 'none'", "connect-src ws://127.0.0.1:4101"),
            META.replace(
                "connect-src 'none'",
                "connect-src ws://127.0.0.1:4100 'self'",
            ),
            META.to_string(),
        ] {
            let got = csp_problems_for(&policy, &dev_connect(4100));
            assert!(
                got.iter().any(|p| p.contains("connect-src")),
                "{policy}: {got:?}"
            );
        }
        // The rest stays the phone page's.
        let loose = dev.replace("script-src 'self'", "script-src 'self' 'unsafe-inline'");
        assert!(!csp_problems_for(&loose, &dev_connect(4100)).is_empty());
        // The dev page's policy is never the phone page's.
        assert!(!csp_problems(&dev).is_empty());
    }

    #[test]
    fn the_local_page_has_no_dev_bridge() {
        let local = vec![(
            "/assets/index-1.js".to_string(),
            b"const D=\"rlord-dev-bridge\"".to_vec(),
        )];
        assert_eq!(local_problems(&local).len(), 1);
    }

    /// The real build, when there is one (`cargo xtask web build --app`).
    #[test]
    fn the_built_phone_page_if_any() {
        let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/web/ui/dist-app");
        if dist.join("index.html").exists() {
            check(&dist).unwrap();
        }
    }
}
