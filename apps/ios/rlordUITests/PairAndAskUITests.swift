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

        // The overlord's terminal: type into its pane with the keyboard.
        let overlord = page().links.matching(NSPredicate(format: "label CONTAINS 'overlord'")).firstMatch
        waitFor(overlord, "the overlord row")
        overlord.tap()
        waitFor(shows("fake overlord"), "the fake overlord's pane drawn", timeout: 45)
        typeIntoTerminal("hi rlord")
        let answer = "Fake overlord here: you said \"hi rlord\""
        waitFor(shows(answer), "the overlord's answer in its pane")

        // The key row: Enter from the row, after typing on the keyboard.
        typeIntoTerminal("again", enter: false)
        let enterKey = page().buttons.matching(NSPredicate(format: "label == '⏎' OR label == 'Enter'")).firstMatch
        waitFor(enterKey, "the key row's Enter")
        enterKey.tap()
        waitFor(shows("Fake overlord here: you said \"again\""), "the key row's Enter")

        // Away and back: the pane is opened again and drawn whole.
        XCUIDevice.shared.press(.home)
        Thread.sleep(forTimeInterval: 2)
        app.activate()
        connected()
        waitFor(shows(answer), "the pane redrawn after coming back", timeout: 45)
    }

    /// Tap the terminal (its keyboard comes up, unless it is up) and type
    /// `text`. (xterm.js's text area is a few points wide: tap the screen.)
    func typeIntoTerminal(_ text: String, enter: Bool = true) {
        if !app.keyboards.firstMatch.exists {
            page().coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.4)).tap()
        }
        let kb = app.keyboards.firstMatch
        XCTAssertTrue(kb.waitForExistence(timeout: 10), "no keyboard for the terminal")
        app.typeText(enter ? text + "\n" : text)
    }
}
