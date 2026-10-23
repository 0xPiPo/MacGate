import MacRDPKit
import SwiftUI

@main
struct MacRDPApp: App {
    @State private var client: HelperClient
    @AppStorage("onboardingDone") private var onboardingDone = false

    init() {
        // A write to a helper that just died must fail with EPIPE, not kill the app.
        signal(SIGPIPE, SIG_IGN)
        let startOnLaunch = UserDefaults.standard.object(forKey: "startOnLaunch") as? Bool ?? true
        let client = HelperClient(transport: Self.makeTransport(), wantsRunning: startOnLaunch)
        client.launch()
        _client = State(initialValue: client)
        let shield = CurtainShield()
        Task {
            for await displays in Observations({ client.shieldDisplays }) { shield.cover(displays) }
        }
    }

    var body: some Scene {
        MenuBarExtra {
            PopoverView()
                .environment(client)
        } label: {
            MenuBarLabel(phase: client.phase)
        }
        .menuBarExtraStyle(.window)

        // A plain window, not the Settings scene: Settings has no toolbar, so the split view's
        // sidebar and titles scrolled under the traffic lights after a bounce.
        Window("macrdp Settings", id: "settings") {
            SettingsView()
                .environment(client)
        }
        .windowResizability(.contentMinSize)

        Window("Welcome to macrdp", id: "onboarding") {
            OnboardingView()
                .environment(client)
        }
        .windowStyle(.hiddenTitleBar)
        .windowResizability(.contentSize)
        .defaultLaunchBehavior(onboardingDone ? .suppressed : .presented)
    }

    /// The bundled helper, or the in-process fake with `MACRDP_FAKE_HELPER=1` (and in Debug
    /// builds that have no helper next to them).
    @MainActor
    private static func makeTransport() -> HelperTransport {
        if ProcessInfo.processInfo.environment["MACRDP_FAKE_HELPER"] == "1" { return FakeTransport() }
        let helper = Bundle.main.bundleURL.appending(path: "Contents/MacOS/macrdp-server")
        #if DEBUG
        if !FileManager.default.isExecutableFile(atPath: helper.path) { return FakeTransport() }
        #endif
        return ProcessTransport(executable: helper)
    }
}

private struct MenuBarLabel: View {
    let phase: ServerPhase

    var body: some View {
        switch phase {
        case .connected: Image("MenuBarActive").accessibilityLabel("macrdp, connected")
        case .error: Image(systemName: "display.trianglebadge.exclamationmark").accessibilityLabel("macrdp, error")
        default: Image("MenuBar").accessibilityLabel("macrdp")
        }
    }
}
