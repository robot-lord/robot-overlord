// rlord's phone app: a thin SwiftUI shell around the phone page and the
// Rust device engine (crates/e2e-client through crates/e2e-ffi). See
// "The phone app" in docs/architecture.md.

import BackgroundTasks
import SwiftUI

@main
struct RlordApp: App {
    // One shell for the process: a background launch for the OS's refresh
    // runs the same engine the window would.
    @StateObject private var shell = Shell.shared
    @Environment(\.scenePhase) private var phase

    var body: some Scene {
        WindowGroup {
            RootView(shell: shell)
                .onChange(of: phase, initial: true) { _, now in shell.phase(now) }
        }
        .backgroundTask(.appRefresh(refreshTask)) {
            await Shell.shared.backgroundRefresh()
        }
    }
}

enum BackgroundRefresh {
    /// Ask the OS for a refresh; when it grants one is its choice.
    static func schedule() {
        let request = BGAppRefreshTaskRequest(identifier: refreshTask)
        request.earliestBeginDate = Date(timeIntervalSinceNow: 15 * 60)
        try? BGTaskScheduler.shared.submit(request)
    }
}

struct RootView: View {
    @ObservedObject var shell: Shell

    var body: some View {
        ZStack {
            if let error = shell.startError {
                Message(title: "rlord can't start", text: error)
            } else {
                PageView(controller: shell.web)
                    .ignoresSafeArea()
            }
            if shell.locked || shell.covered {
                Cover(shell: shell)
            }
        }
        .sheet(isPresented: $shell.scanning) {
            ScannerSheet(onCode: { shell.scanned($0) }, onCancel: { shell.scanning = false })
        }
    }
}

/// Covers the page while the app is locked or out of front (also in the
/// app switcher).
struct Cover: View {
    @ObservedObject var shell: Shell

    var body: some View {
        ZStack {
            Color(.systemBackground).ignoresSafeArea()
            VStack(spacing: 16) {
                Image(systemName: "lock.fill").font(.largeTitle)
                if shell.needsPasscode {
                    Text("Set a passcode on this phone to use rlord: the app locks itself with it.")
                        .multilineTextAlignment(.center)
                        .padding(.horizontal)
                }
                if shell.locked {
                    Button("Unlock") { shell.unlock() }
                        .buttonStyle(.borderedProminent)
                        .accessibilityIdentifier("unlock")
                }
            }
        }
    }
}

struct Message: View {
    let title: String
    let text: String

    var body: some View {
        VStack(spacing: 12) {
            Text(title).font(.headline)
            Text(text).multilineTextAlignment(.center)
        }
        .padding()
    }
}
