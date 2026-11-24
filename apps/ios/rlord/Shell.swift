// The shell's whole job: start the engine, pass JSON between the page and
// the engine, open the scanner, report lifecycle and network changes, lock
// the app, run the OS's background refresh and post local notifications.
// It reads only the two page messages it serves itself (`scan` and
// `settings`); everything else goes to the engine as it is.

import Foundation
import LocalAuthentication
import Network
import SwiftUI
import UIKit
import UserNotifications

/// Shell settings, kept in UserDefaults.
enum Setting {
    static let appLock = "appLock"
    static let notifications = "notifications"
}

/// Away this long, the app asks to unlock again.
let lockAfter: TimeInterval = 5 * 60

/// The background refresh task's identifier (Info.plist lists it).
let refreshTask = "com.robot-lord.rlord.refresh"

/// Engine callbacks, from any engine thread.
final class PageListener: Listener, @unchecked Sendable {
    weak var shell: Shell?

    func toPage(json: String) {
        DispatchQueue.main.async { [weak self] in self?.shell?.web.send(json) }
    }

    func notify(kind: String, count: UInt32) {
        guard let body = Self.text(kind: kind, count: count),
              UserDefaults.standard.bool(forKey: Setting.notifications)
        else { return }
        let content = UNMutableNotificationContent()
        content.title = "rlord"
        content.body = body
        // One notification at a time: a newer count replaces the last.
        let request = UNNotificationRequest(identifier: "waiting", content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request)
    }

    /// What a notification says: a count, made on the phone, no session
    /// names (nil: nothing to say).
    static func text(kind: String, count: UInt32) -> String? {
        guard kind == "waiting", count > 0 else { return nil }
        return count == 1 ? "1 session needs you" : "\(count) sessions need you"
    }
}

/// The finishing time the OS gives on the way to the background, ended
/// once (by the timer or by the OS's expiry, whichever is first).
@MainActor
final class FlushTask {
    var id: UIBackgroundTaskIdentifier = .invalid

    func end() {
        guard id != .invalid else { return }
        UIApplication.shared.endBackgroundTask(id)
        id = .invalid
    }
}

@MainActor
final class Shell: ObservableObject {
    static let shared = Shell()

    let web = WebController()
    private(set) var engine: Engine?
    @Published var startError: String?
    @Published var scanning = false
    /// The page is covered: the app is locked, or not in front.
    @Published var locked: Bool
    @Published var covered = false
    @Published var needsPasscode = false
    private let listener = PageListener()
    private let monitor = NWPathMonitor()
    private var lastPath: String?
    private var leftAt: Date?
    private var launched = false

    init() {
        UserDefaults.standard.register(defaults: [Setting.appLock: true, Setting.notifications: false])
        locked = UserDefaults.standard.bool(forKey: Setting.appLock)
        listener.shell = self
        web.onMessage = { [weak self] text in self?.fromPage(text) }
        startEngine()
        web.start()
        monitor.pathUpdateHandler = { [weak self] path in
            let online = path.status == .satisfied
            // A changed path (wifi to cellular) redials at once.
            let key = online ? path.availableInterfaces.map(\.name).joined(separator: ",") : "down"
            Task { @MainActor in self?.network(online: online, key: key) }
        }
        monitor.start(queue: DispatchQueue(label: "rlord.path"))
    }

    private func startEngine() {
        #if !DEBUG
        guard Keys.hardware else {
            startError = "This phone has no Secure Enclave; rlord needs one to keep its keys."
            return
        }
        #endif
        do {
            let dir = try Self.engineDir()
            engine = try Engine(
                dir: dir.path, caPem: nil, vault: EnclaveVault(), signers: EnclaveSigners(),
                listener: listener)
        } catch {
            startError = "The app couldn't start: \(error)"
            return
        }
        #if DEBUG
        // UI tests start from nothing (`-rlordReset YES`): the engine
        // forgets its host, keys and files before anything else runs.
        if UserDefaults.standard.bool(forKey: "rlordReset") {
            engine?.wipe()
        }
        // Simulators have no camera: a pairing code can come as a launch
        // argument (`-rlordQR RLORD1:…`), debug builds only.
        if let qr = UserDefaults.standard.string(forKey: "rlordQR"), qr.hasPrefix("RLORD1:") {
            engine?.scanned(qr: qr, defaultName: UIDevice.current.name)
        }
        #endif
    }

    /// The engine's folder: Application Support, out of every backup.
    static func engineDir() throws -> URL {
        var dir = try FileManager.default.url(
            for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true
        ).appendingPathComponent("rlord", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try dir.setResourceValues(values)
        return dir
    }

    // MARK: The page

    func fromPage(_ text: String) {
        switch Self.shellMessage(text) {
        case .scan:
            scanning = true
        case .settings(let appLock, let notifications):
            if let appLock { UserDefaults.standard.set(appLock, forKey: Setting.appLock) }
            if let notifications { setNotifications(notifications) } else { sendSettings() }
        case .paste:
            // Read only when the page asks (its Paste key); iOS shows its
            // own paste notice.
            sendEvent(["event": "paste", "text": UIPasteboard.general.string ?? ""])
        case .copy(let text):
            UIPasteboard.general.string = text
        case nil:
            engine?.pageMessage(json: text)
        }
    }

    enum ShellMessage: Equatable {
        case scan
        case settings(appLock: Bool?, notifications: Bool?)
        /// The terminal's Paste key: the pasteboard's text, to the page.
        case paste
        /// "Copy screen": the page's text, to the pasteboard.
        case copy(String)
    }

    /// The page messages the shell serves itself; nil for the engine's.
    nonisolated static func shellMessage(_ text: String) -> ShellMessage? {
        guard let data = text.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let app = obj["app"] as? String
        else { return nil }
        switch app {
        case "scan": return .scan
        case "settings":
            return .settings(appLock: obj["app_lock"] as? Bool, notifications: obj["notifications"] as? Bool)
        case "paste": return .paste
        case "copy": return (obj["text"] as? String).map { .copy($0) }
        default: return nil
        }
    }

    private func sendEvent(_ obj: [String: Any]) {
        if let data = try? JSONSerialization.data(withJSONObject: obj), let s = String(data: data, encoding: .utf8) {
            web.send(s)
        }
    }

    func sendSettings() {
        let d = UserDefaults.standard
        var obj: [String: Any] = [
            "event": "settings", "app_lock": d.bool(forKey: Setting.appLock),
            "notifications": d.bool(forKey: Setting.notifications), "platform": "ios",
        ]
        #if DEBUG
        // The page mirrors a terminal's text into a hidden element, for
        // the UI tests.
        obj["debug"] = true
        #endif
        if let data = try? JSONSerialization.data(withJSONObject: obj), let s = String(data: data, encoding: .utf8) {
            web.send(s)
        }
    }

    /// Notifications are asked for the first time the user turns them on.
    private func setNotifications(_ on: Bool) {
        guard on else {
            UserDefaults.standard.set(false, forKey: Setting.notifications)
            sendSettings()
            return
        }
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .badge]) { granted, _ in
            Task { @MainActor in
                UserDefaults.standard.set(granted, forKey: Setting.notifications)
                self.sendSettings()
            }
        }
    }

    // MARK: The scanner

    func scanned(_ qr: String) {
        scanning = false
        engine?.scanned(qr: qr, defaultName: UIDevice.current.name)
    }

    // MARK: Lifecycle, network

    func phase(_ phase: ScenePhase) {
        switch phase {
        case .active:
            covered = false
            if let leftAt, Date().timeIntervalSince(leftAt) > lockAfter,
               UserDefaults.standard.bool(forKey: Setting.appLock)
            {
                locked = true
            }
            // Prompt by itself only at launch and back from the background:
            // a cancelled prompt's own sheet also ends in `.active`, and the
            // Unlock button is there for another try.
            let arriving = leftAt != nil || !launched
            launched = true
            leftAt = nil
            if locked && arriving { unlock() }
            engine?.lifecycle(foreground: true)
        case .inactive:
            // Covered in the app switcher too.
            covered = true
        case .background:
            covered = true
            leftAt = leftAt ?? Date()
            goingToBackground()
        @unknown default:
            break
        }
    }

    /// The engine sends what is queued and closes the link cleanly, in the
    /// finishing time the OS gives.
    private func goingToBackground() {
        let app = UIApplication.shared
        let flush = FlushTask()
        flush.id = app.beginBackgroundTask(withName: "rlord.flush") { flush.end() }
        engine?.lifecycle(foreground: false)
        DispatchQueue.main.asyncAfter(deadline: .now() + 5) { flush.end() }
        scheduleRefresh()
    }

    private func network(online: Bool, key: String) {
        guard key != lastPath else { return }
        lastPath = key
        engine?.network(online: online)
    }

    // MARK: App lock

    /// The unlock sheet itself makes the scene inactive and active again:
    /// don't prompt a second time while one is up.
    private var unlocking = false

    func unlock() {
        guard !unlocking else { return }
        let context = LAContext()
        var error: NSError?
        guard context.canEvaluatePolicy(.deviceOwnerAuthentication, error: &error) else {
            // The phone must have a passcode for the app to lock.
            needsPasscode = true
            return
        }
        needsPasscode = false
        unlocking = true
        context.evaluatePolicy(.deviceOwnerAuthentication, localizedReason: "Unlock rlord") { ok, _ in
            Task { @MainActor in
                if ok { self.locked = false }
                self.unlocking = false
            }
        }
    }

    // MARK: Background refresh

    func scheduleRefresh() {
        BackgroundRefresh.schedule()
    }

    /// The OS's refresh: one bounded check (the engine dials, asks the
    /// relay whether anything waits, and only then makes a session, takes
    /// the state and calls `notify` with the sessions newly waiting). Then
    /// the next refresh is asked for. The OS gives about 30 s; the check
    /// gets 20 of them.
    nonisolated func backgroundRefresh() async {
        let engine = await MainActor.run { self.engine }
        if let engine {
            let r = await Task.detached { engine.backgroundCheck(budgetMs: Self.refreshBudgetMs) }.value
            NSLog("rlord: background check: \(r.outcome)\(r.reason.map { " (\($0))" } ?? "")")
        }
        BackgroundRefresh.schedule()
    }

    nonisolated static let refreshBudgetMs: UInt64 = 20_000
}
