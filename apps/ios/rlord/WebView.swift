// The WebView that draws the phone page, locked down: the page comes from
// the app bundle through a scheme handler (`rlord-app://page/`), every
// response carries the CSP, a content rule list blocks every URL off that
// scheme, every navigation but the first load and same-document changes
// is cancelled, and nothing else is on (no windows, link previews, data
// detectors or media). The page has no network path: the engine's relay
// link is the app's only one.

import SwiftUI
import WebKit

/// The page's origin. The bridge accepts messages from it only.
let pageScheme = "rlord-app"
let pageHost = "page"

/// The policy on every file (and as a `<meta>` in app.html).
let pagePolicy =
    "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; "
    + "font-src 'self'; connect-src 'none'; base-uri 'none'; form-action 'none'; "
    + "frame-ancestors 'none'; require-trusted-types-for 'script'; trusted-types 'none'"

/// The page's files, from the manifest `cargo xtask web build --app`
/// writes beside them. Anything not listed is 404.
struct PageAssets {
    let root: URL
    let types: [String: String]
    let entry: String

    static func load(bundle: Bundle = .main) -> PageAssets? {
        guard let root = bundle.resourceURL?.appendingPathComponent("page"),
              let data = try? Data(contentsOf: root.appendingPathComponent("manifest.json"))
        else { return nil }
        return parse(root: root, manifest: data)
    }

    /// The manifest `cargo xtask web build --app` writes:
    /// `{"index": "/index.html", "csp": "…", "files": [{"path": "/assets/a.js",
    /// "type": "text/javascript; charset=utf-8", "bytes": 76407}, …]}`.
    static func parse(root: URL, manifest: Data) -> PageAssets? {
        guard let obj = try? JSONSerialization.jsonObject(with: manifest) as? [String: Any],
              let files = obj["files"] as? [[String: Any]]
        else { return nil }
        var types: [String: String] = [:]
        for f in files {
            guard let path = (f["path"] as? String).flatMap(relative), let type = f["type"] as? String
            else { return nil }
            types[path] = type
        }
        guard let entry = (obj["index"] as? String ?? "/index.html").flatMap(relative),
              types[entry] != nil
        else { return nil }
        return PageAssets(root: root, types: types, entry: entry)
    }

    /// `/assets/a.js` as `assets/a.js`; nil for anything that could leave
    /// the page's folder.
    static func relative(_ path: String) -> String? {
        guard path.hasPrefix("/"), !path.hasPrefix("//") else { return nil }
        let rel = String(path.dropFirst())
        guard !rel.isEmpty, !rel.split(separator: "/").contains(where: { $0 == ".." || $0 == "." })
        else { return nil }
        return rel
    }

    /// The file and its type for a request path, or nil (404).
    func lookup(_ urlPath: String) -> (URL, String)? {
        var path = urlPath
        while path.hasPrefix("/") { path.removeFirst() }
        if path.isEmpty { path = entry }
        guard let type = types[path] else { return nil }
        return (root.appendingPathComponent(path), type)
    }
}

/// Serves the bundled page with its headers.
final class PageSchemeHandler: NSObject, WKURLSchemeHandler {
    let assets: PageAssets?

    init(assets: PageAssets?) { self.assets = assets }

    static func headers(type: String) -> [String: String] {
        [
            "Content-Type": type,
            "Content-Security-Policy": pagePolicy,
            "X-Content-Type-Options": "nosniff",
            "Referrer-Policy": "no-referrer",
            "Cache-Control": "no-store",
        ]
    }

    func webView(_ webView: WKWebView, start task: WKURLSchemeTask) {
        guard let url = task.request.url else { return }
        guard url.scheme == pageScheme, url.host == pageHost,
              let (file, type) = assets?.lookup(url.path),
              let data = try? Data(contentsOf: file)
        else {
            task.didReceive(HTTPURLResponse(
                url: url, statusCode: 404, httpVersion: "HTTP/1.1",
                headerFields: Self.headers(type: "text/plain"))!)
            task.didFinish()
            return
        }
        task.didReceive(HTTPURLResponse(
            url: url, statusCode: 200, httpVersion: "HTTP/1.1",
            headerFields: Self.headers(type: type))!)
        task.didReceive(data)
        task.didFinish()
    }

    func webView(_ webView: WKWebView, stop task: WKURLSchemeTask) {}
}

/// Blocks every load that isn't the page's own scheme.
let contentRules = """
[{"trigger":{"url-filter":".*"},"action":{"type":"block"}},
 {"trigger":{"url-filter":"^rlord-app://page/"},"action":{"type":"ignore-previous-rules"}}]
"""

/// Owns the WebView; passes JSON between the page and the shell.
@MainActor
final class WebController: NSObject, WKNavigationDelegate, WKScriptMessageHandler, WKUIDelegate {
    let webView: WKWebView
    private var loaded = false
    /// Messages from the page, for the shell.
    var onMessage: ((String) -> Void)?

    override init() {
        let config = WKWebViewConfiguration()
        config.setURLSchemeHandler(PageSchemeHandler(assets: PageAssets.load()), forURLScheme: pageScheme)
        config.websiteDataStore = .nonPersistent()
        config.preferences.javaScriptCanOpenWindowsAutomatically = false
        config.preferences.isFraudulentWebsiteWarningEnabled = false
        config.preferences.isElementFullscreenEnabled = false
        config.defaultWebpagePreferences.allowsContentJavaScript = true
        config.allowsInlineMediaPlayback = false
        config.allowsAirPlayForMediaPlayback = false
        config.allowsPictureInPictureMediaPlayback = false
        config.mediaTypesRequiringUserActionForPlayback = .all
        config.dataDetectorTypes = []
        config.suppressesIncrementalRendering = false
        webView = WKWebView(frame: .zero, configuration: config)
        super.init()
        config.userContentController.add(self, contentWorld: .page, name: "rlord")
        webView.navigationDelegate = self
        webView.uiDelegate = self
        webView.allowsLinkPreview = false
        webView.allowsBackForwardNavigationGestures = true
        webView.scrollView.contentInsetAdjustmentBehavior = .never
        webView.isOpaque = false
        webView.backgroundColor = .systemBackground
        #if DEBUG
        webView.isInspectable = true
        #endif
    }

    /// Compile the content rules, then load the page.
    func start() {
        WKContentRuleListStore.default().compileContentRuleList(
            forIdentifier: "rlord-page", encodedContentRuleList: contentRules
        ) { [weak self] list, error in
            Task { @MainActor in
                guard let self else { return }
                guard let list else {
                    // Without the rules the page isn't loaded at all.
                    NSLog("rlord: content rules failed: \(String(describing: error))")
                    return
                }
                self.webView.configuration.userContentController.add(list)
                self.webView.load(URLRequest(url: URL(string: "\(pageScheme)://\(pageHost)/")!))
            }
        }
    }

    /// Send one JSON text to the page: one fixed function, the text as its
    /// argument (never a built script string).
    func send(_ json: String) {
        webView.callAsyncJavaScript(
            "window.__rlord_recv && window.__rlord_recv(t)", arguments: ["t": json],
            in: nil, in: .page, completionHandler: nil)
    }

    // MARK: WKScriptMessageHandler

    nonisolated func userContentController(
        _ controller: WKUserContentController, didReceive message: WKScriptMessage
    ) {
        MainActor.assumeIsolated {
            guard Self.fromPage(message.frameInfo), let text = message.body as? String else { return }
            onMessage?(text)
        }
    }

    /// Only the main frame of the page's own origin may talk to the shell.
    static func fromPage(_ frame: WKFrameInfo) -> Bool {
        frame.isMainFrame && frame.securityOrigin.protocol == pageScheme
            && frame.securityOrigin.host == pageHost
    }

    // MARK: WKNavigationDelegate

    nonisolated func webView(
        _ webView: WKWebView, decidePolicyFor action: WKNavigationAction,
        decisionHandler: @escaping @MainActor (WKNavigationActionPolicy) -> Void
    ) {
        MainActor.assumeIsolated {
            decisionHandler(allow(action.request.url, current: webView.url) ? .allow : .cancel)
        }
    }

    /// The first load of the page, and same-document (hash) changes.
    func allow(_ url: URL?, current: URL?) -> Bool {
        guard let url, url.scheme == pageScheme, url.host == pageHost else { return false }
        if !loaded {
            loaded = true
            return true
        }
        guard let current else { return false }
        return Self.sameDocument(url, current)
    }

    static func sameDocument(_ a: URL, _ b: URL) -> Bool {
        var x = URLComponents(url: a, resolvingAgainstBaseURL: false)
        var y = URLComponents(url: b, resolvingAgainstBaseURL: false)
        x?.fragment = nil
        y?.fragment = nil
        return x != nil && x == y
    }

    nonisolated func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        // The page reloads from the bundle; it asks the engine for its state.
        MainActor.assumeIsolated {
            _ = webView.reload()
        }
    }

    // MARK: WKUIDelegate: no windows, no panels.

    nonisolated func webView(
        _ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
        for action: WKNavigationAction, windowFeatures: WKWindowFeatures
    ) -> WKWebView? { nil }
}

/// The WebView in SwiftUI.
struct PageView: UIViewRepresentable {
    let controller: WebController

    func makeUIView(context: Context) -> WKWebView { controller.webView }
    func updateUIView(_ view: WKWebView, context: Context) {}
}
