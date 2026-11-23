// The real phone page, loaded the way the app loads it (the bundled files
// through the scheme handler, the content rules, the CSP), checked from
// inside: it runs, and it has no network path of its own.

import WebKit
import XCTest

@testable import rlord

@MainActor
final class LivePageTests: XCTestCase {
    var web: WebController!

    override func setUp() async throws {
        web = WebController()
        // In the host app's window, so WebKit runs it as the app does.
        let scene = UIApplication.shared.connectedScenes.first as? UIWindowScene
        if let window = scene?.windows.first {
            window.addSubview(web.webView)
            web.webView.frame = window.bounds
        }
        web.start()
        try await until("the page's bridge is up") {
            try await self.js("return typeof window.__rlord_recv") as? String == "function"
        }
    }

    override func tearDown() async throws {
        web.webView.removeFromSuperview()
        web = nil
    }

    func js(_ body: String) async throws -> Any? {
        try await web.webView.callAsyncJavaScript(body, arguments: [:], in: nil, contentWorld: .page)
    }

    func until(_ what: String, _ check: @escaping () async throws -> Bool) async throws {
        let end = Date().addingTimeInterval(20)
        while Date() < end {
            if (try? await check()) == true { return }
            try await Task.sleep(nanoseconds: 100_000_000)
        }
        XCTFail("timed out: \(what)")
        throw CancellationError()
    }

    func testThePageLoadsFromTheBundle() async throws {
        let origin = try await js("return location.protocol + '//' + location.host") as? String
        XCTAssertEqual(origin, "rlord-app://page")
        let scripts = try await js("return document.scripts.length") as? Int
        XCTAssertGreaterThan(scripts ?? 0, 0)
    }

    /// `fetch` is refused, off the page's scheme and on it.
    func testFetchIsBlocked() async throws {
        for url in ["https://example.com/", "http://127.0.0.1:9/", "rlord-app://page/manifest.json"] {
            let r = try await js("""
                try { await fetch(\(quoted(url))); return 'reached' }
                catch (e) { return 'blocked: ' + e.name }
                """) as? String
            XCTAssertEqual(r, "blocked: TypeError", url)
        }
    }

    /// A websocket never opens.
    func testWebSocketIsBlocked() async throws {
        let r = try await js("""
            return await new Promise((done) => {
              let ws
              try { ws = new WebSocket('ws://127.0.0.1:9/') } catch (e) { return done('blocked: ' + e.name) }
              ws.onopen = () => done('open')
              ws.onerror = () => done('blocked: error')
              setTimeout(() => done('blocked: never opened'), 3000)
            })
            """) as? String
        XCTAssertTrue(r?.hasPrefix("blocked") == true, r ?? "nil")
    }

    /// Trusted Types: a string into `innerHTML` throws.
    func testInnerHTMLIsRefused() async throws {
        let r = try await js("""
            try { document.createElement('div').innerHTML = '<b>x</b>'; return 'allowed' }
            catch (e) { return 'refused: ' + e.name }
            """) as? String
        XCTAssertEqual(r, "refused: TypeError")
    }

    func quoted(_ s: String) -> String {
        "'" + s.replacingOccurrences(of: "'", with: "\\'") + "'"
    }
}
