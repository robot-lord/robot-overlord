// The app end to end on a simulator, against the loopback host stack
// (`crates/web/examples/remote_stack.rs`) that `cargo xtask app test-ios`
// starts: the real relay, the real plugin, a daemon with fixture
// sessions. The simulator shares the Mac's 127.0.0.1. The stack's pairing
// code and control folder reach this runner as TEST_RUNNER_RLORD_QR and
// TEST_RUNNER_RLORD_CONTROL (xcodebuild strips the prefix).

import XCTest

final class PairAndAskUITests: XCTestCase {
    var app: XCUIApplication!
    var control: URL!

    override func setUpWithError() throws {
        continueAfterFailure = false
        let env = ProcessInfo.processInfo.environment
        guard let qr = env["RLORD_QR"], let dir = env["RLORD_CONTROL"] else {
            throw XCTSkip("run by `cargo xtask app test-ios`, which starts the host stack")
        }
        control = URL(fileURLWithPath: dir, isDirectory: true)
        app = XCUIApplication()
        // A fresh engine, the stack's code as if scanned, no app lock (a
        // simulator has no passcode).
        app.launchArguments = ["-rlordReset", "YES", "-rlordQR", qr, "-appLock", "NO"]
    }

    /// Runs one stack command (`<name>.cmd` in its control folder) and
    /// waits for its `.done`.
    func stack(_ command: String) throws {
        let name = "\(Int(Date().timeIntervalSince1970 * 1000))-\(command)"
        let cmd = control.appendingPathComponent("\(name).cmd")
        try command.write(to: cmd, atomically: true, encoding: .utf8)
        let done = control.appendingPathComponent("\(name).done")
        let end = Date().addingTimeInterval(60)
        while !FileManager.default.fileExists(atPath: done.path) {
            XCTAssertLessThan(Date(), end, "the stack never ran \(command)")
            Thread.sleep(forTimeInterval: 0.2)
        }
    }

    func page() -> XCUIElement { app.webViews.firstMatch }

    /// Any element in the page whose label contains `text`.
    func shows(_ text: String) -> XCUIElement {
        page().descendants(matching: .any)
            .matching(NSPredicate(format: "label CONTAINS %@", text)).firstMatch
    }

    func waitFor(_ e: XCUIElement, _ what: String, timeout: TimeInterval = 30) {
        XCTAssertTrue(e.waitForExistence(timeout: timeout), "never saw \(what)")
    }

    func connected() {
        waitFor(shows(": connected"), "the connection line say connected", timeout: 45)
    }

    func testPairSeeTheListAskAndRecover() throws {
        app.launch()

        // Pairing: the code arrives as if scanned; name the phone, continue.
        let next = page().buttons["Continue"]
        waitFor(next, "the pairing name step")
        next.tap()
        // The stack confirms on the host by itself (--auto-confirm).
        connected()
        waitFor(shows("fixture session 0"), "a fixture session in the list")

        // Away and back: a fresh session, the list still there.
        XCUIDevice.shared.press(.home)
        Thread.sleep(forTimeInterval: 2)
        app.activate()
        connected()
        waitFor(shows("fixture session 0"), "the list after coming back")

        // The relay restarts: the app recovers by itself.
        try stack("restart-relay")
        connected()

        // Ask the overlord.
        let overlord = page().links.matching(NSPredicate(format: "label CONTAINS 'overlord'")).firstMatch
        waitFor(overlord, "the overlord row")
        overlord.tap()
        let box = page().textViews["tell the overlord"]
        waitFor(box, "the overlord's composer")
        box.tap()
        box.typeText("hello from the simulator")
        page().buttons["Send"].tap()
        assertAnswered("hello from the simulator")
    }

    /// The ask left the outbox (a pending bubble has a Cancel button) and
    /// the stack's fake overlord's answer shows in the chat.
    func assertAnswered(_ text: String) {
        let cancel = page().buttons["Cancel"]
        let end = Date().addingTimeInterval(30)
        while cancel.exists, Date() < end { Thread.sleep(forTimeInterval: 0.2) }
        XCTAssertFalse(cancel.exists, "the ask is still pending")
        waitFor(shows("Fake overlord here: you said \"\(text)\""), "the overlord's answer")
    }
}
