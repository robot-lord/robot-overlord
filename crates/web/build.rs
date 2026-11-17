//! Embeds the built page (`ui/dist/`, from `cargo xtask web build`) as a
//! table of (path, content type, bytes) in `$OUT_DIR/assets.rs`. File
//! names carry content hashes, so there is no hand-kept list.
//!
//! No `dist/` (a Rust-only build, CI's `check` job): a one-line page that
//! says how to build the real one. `RLORD_WEB_REQUIRE_ASSETS=1` (the
//! release build) makes a missing `dist/` an error.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const PLACEHOLDER: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>rlord</title></head>\
<body><p>This rlord-web was built without its page: run <code>cargo xtask web build</code>, then rebuild rlord-web.</p></body></html>\n";

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let dist = manifest.join("ui").join("dist");
    // (A path that doesn't exist would rerun this on every build: without
    // dist/, watch ui/, where it will appear.)
    let watch = if dist.exists() {
        dist.clone()
    } else {
        manifest.join("ui")
    };
    println!("cargo:rerun-if-changed={}", watch.display());
    println!("cargo:rerun-if-env-changed=RLORD_WEB_REQUIRE_ASSETS");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    let mut files = Vec::new();
    if dist.join("index.html").is_file() {
        walk(&dist, &dist, &mut files);
        files.sort();
    } else if std::env::var("RLORD_WEB_REQUIRE_ASSETS").as_deref() == Ok("1") {
        panic!(
            "RLORD_WEB_REQUIRE_ASSETS=1 but {} has no index.html: run `cargo xtask web build`",
            dist.display()
        );
    }

    let mut table = String::from("pub static ASSETS: &[Asset] = &[\n");
    let mut hash = Fnv::new();
    if files.is_empty() {
        let p = out.join("placeholder.html");
        if std::fs::read_to_string(&p).ok().as_deref() != Some(PLACEHOLDER) {
            std::fs::write(&p, PLACEHOLDER).unwrap();
        }
        files.push(("/index.html".to_string(), p));
    }
    for (path, file) in &files {
        if file.starts_with(&dist) {
            println!("cargo:rerun-if-changed={}", file.display());
        }
        let bytes = std::fs::read(file).unwrap();
        hash.add(path.as_bytes());
        hash.add(&[0]);
        hash.add(&bytes);
        writeln!(
            table,
            "    Asset {{ path: {path:?}, content_type: {:?}, bytes: include_bytes!({:?}) }},",
            content_type(path),
            file.display().to_string()
        )
        .unwrap();
    }
    table.push_str("];\n");
    writeln!(
        table,
        "/// Hash of the table: with the version, the page's build id.\npub const ASSETS_HASH: &str = \"{:016x}\";",
        hash.0
    )
    .unwrap();
    std::fs::write(out.join("assets.rs"), table).unwrap();
}

/// Every file under `dir`, as (`/relative/path`, absolute path).
fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.is_dir() {
            walk(root, &p, out);
        } else {
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.push((format!("/{rel}"), p));
        }
    }
}

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

/// FNV-1a, 64-bit: stable, and plenty for telling builds apart.
struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf29ce484222325)
    }

    fn add(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }
}
