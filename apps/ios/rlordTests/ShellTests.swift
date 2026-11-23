// The shell's own logic: the key and vault wrappers (on the simulator,
// the software fallback), the page's asset table and headers, and the
// bridge's message routing and navigation rules.

import CryptoKit
import Security
import XCTest

@testable import rlord

final class KeysTests: XCTestCase {
    let label = "test-\(UUID().uuidString)"

    override func tearDown() {
        EnclaveSigners().delete(label: label)
    }

    func testSignerMakesAKeyAndSignsDER() throws {
        let s = EnclaveSigners()
        let pub = try s.create(label: label)
        XCTAssertEqual(pub.count, 65)
        XCTAssertEqual(pub.first, 0x04)
        XCTAssertEqual(try s.publicKey(label: label), pub)
        let msg = Data("transcript".utf8)
        let sig = try s.sign(label: label, msg: msg)
        XCTAssertEqual(sig.first, 0x30, "DER: the engine converts it")
        // The signature verifies under the public key.
        let attrs: [String: Any] = [
            kSecAttrKeyType as String: kSecAttrKeyTypeECSECPrimeRandom,
            kSecAttrKeyClass as String: kSecAttrKeyClassPublic,
        ]
        let key = try XCTUnwrap(SecKeyCreateWithData(pub as CFData, attrs as CFDictionary, nil))
        XCTAssertTrue(SecKeyVerifySignature(key, .ecdsaSignatureMessageX962SHA256, msg as CFData, sig as CFData, nil))
    }

    func testCreateReplacesAndDeleteForgets() throws {
        let s = EnclaveSigners()
        let a = try s.create(label: label)
        let b = try s.create(label: label)
        XCTAssertNotEqual(a, b)
        XCTAssertEqual(try s.publicKey(label: label), b)
        s.delete(label: label)
        XCTAssertThrowsError(try s.publicKey(label: label))
        XCTAssertThrowsError(try s.sign(label: label, msg: Data()))
    }

    /// The Apple Silicon simulator passes the Mac's Secure Enclave
    /// through, so the hardware path runs here too when it is there.
    func testHardwareIsTheEnclave() {
        XCTAssertEqual(EnclaveSigners().hardware(), SecureEnclave.isAvailable)
    }

    func testVaultRoundTrips() throws {
        let v = EnclaveVault()
        let plain = Data((0..<300).map { UInt8($0 % 251) })
        let sealed = try v.seal(plain: plain)
        XCTAssertNotEqual(sealed, plain)
        XCTAssertEqual(try v.unseal(sealed: sealed), plain)
        var tampered = sealed
        tampered[tampered.count - 1] ^= 1
        XCTAssertThrowsError(try v.unseal(sealed: tampered))
    }
}

final class PageAssetsTests: XCTestCase {
    let root = URL(fileURLWithPath: "/page")

    func manifest(_ s: String) -> PageAssets? { PageAssets.parse(root: root, manifest: Data(s.utf8)) }

    func testListedFilesOnly() throws {
        let a = try XCTUnwrap(manifest(
            #"{"index":"/index.html","files":[{"path":"/index.html","type":"text/html"},{"path":"/assets/a.js","type":"text/javascript"}]}"#))
        XCTAssertEqual(a.lookup("/")?.1, "text/html")
        XCTAssertEqual(a.lookup("/")?.0.path, "/page/index.html")
        XCTAssertEqual(a.lookup("/assets/a.js")?.1, "text/javascript")
        XCTAssertNil(a.lookup("/assets/b.js"))
        XCTAssertNil(a.lookup("/manifest.json"))
        XCTAssertNil(a.lookup("/../etc/passwd"))
    }

    func testBadManifestsAreRefused() {
        XCTAssertNil(manifest(#"{"files":[{"path":"/../x","type":"text/html"}],"index":"/../x"}"#))
        XCTAssertNil(manifest(#"{"files":[{"path":"relative.html","type":"text/html"}]}"#))
        XCTAssertNil(manifest(#"{"files":[{"path":"//host/x","type":"text/html"}]}"#))
        XCTAssertNil(manifest(#"{"files":[{"path":"/a.html","type":"text/html"}],"index":"/b.html"}"#))
        XCTAssertNil(manifest("not json"))
    }

    func testTheBundledPageIsThere() throws {
        let a = try XCTUnwrap(PageAssets.load(), "the app bundle carries page/manifest.json")
        let (file, type) = try XCTUnwrap(a.lookup("/"))
        XCTAssertTrue(type.hasPrefix("text/html"), type)
        XCTAssertTrue(FileManager.default.fileExists(atPath: file.path))
    }

    /// The shell's header policy is the page's own (app.html, carried in
    /// the manifest), directive for directive.
    func testThePolicyMatchesThePages() throws {
        let url = try XCTUnwrap(Bundle.main.resourceURL?.appendingPathComponent("page/manifest.json"))
        let obj = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let page = try XCTUnwrap(obj["csp"] as? String)
        func directives(_ p: String) -> Set<String> {
            Set(p.split(separator: ";").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty })
        }
        XCTAssertEqual(directives(page), directives(pagePolicy))
    }

    func testEveryResponseCarriesThePolicy() {
        let h = PageSchemeHandler.headers(type: "text/html")
        let csp = h["Content-Security-Policy"] ?? ""
        XCTAssertTrue(csp.contains("connect-src 'none'"))
        XCTAssertTrue(csp.contains("trusted-types 'none'"))
        XCTAssertTrue(csp.contains("require-trusted-types-for 'script'"))
        XCTAssertTrue(csp.contains("default-src 'none'"))
        XCTAssertFalse(csp.contains("unsafe"))
        XCTAssertEqual(h["X-Content-Type-Options"], "nosniff")
        XCTAssertEqual(h["Content-Type"], "text/html")
    }
}

@MainActor
final class BridgeTests: XCTestCase {
    func testTheShellServesOnlyScanAndSettings() {
        XCTAssertEqual(Shell.shellMessage(#"{"app":"scan"}"#), .scan)
        XCTAssertEqual(
            Shell.shellMessage(#"{"app":"settings","app_lock":false}"#),
            .settings(appLock: false, notifications: nil))
        XCTAssertEqual(Shell.shellMessage(#"{"app":"settings"}"#), .settings(appLock: nil, notifications: nil))
        // Everything else is the engine's, untouched.
        XCTAssertNil(Shell.shellMessage(#"{"app":"ready"}"#))
        XCTAssertNil(Shell.shellMessage(#"{"app":"unpair"}"#))
        XCTAssertNil(Shell.shellMessage(#"{"id":1,"op":"snapshot","args":{}}"#))
        XCTAssertNil(Shell.shellMessage("not json"))
    }

    func testNavigationFirstLoadAndSameDocumentOnly() {
        let web = WebController()
        let page = URL(string: "rlord-app://page/")!
        XCTAssertFalse(web.allow(URL(string: "https://example.com/"), current: nil))
        XCTAssertTrue(web.allow(page, current: nil), "the first load")
        XCTAssertFalse(web.allow(URL(string: "rlord-app://page/other.html"), current: page))
        XCTAssertTrue(web.allow(URL(string: "rlord-app://page/#/session/x"), current: page))
        XCTAssertFalse(web.allow(URL(string: "rlord-app://evil/#x"), current: page))
        XCTAssertFalse(web.allow(URL(string: "https://example.com/#x"), current: page))
    }
}
