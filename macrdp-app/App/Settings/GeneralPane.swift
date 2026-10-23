import MacRDPKit
import ServiceManagement
import SwiftUI

struct GeneralPane: View {
    @Environment(HelperClient.self) private var client
    @AppStorage("startOnLaunch") private var startOnLaunch = true
    @State private var openAtLogin = SMAppService.mainApp.status == .enabled
    @State private var loginError: String?

    var body: some View {
        Form {
            Section {
                Toggle("Open macrdp at login", isOn: $openAtLogin)
                    .onChange(of: openAtLogin) { _, on in setOpenAtLogin(on) }
                Toggle("Start the server when macrdp opens", isOn: $startOnLaunch)
            } footer: {
                if let loginError {
                    Text(loginError).foregroundStyle(.red)
                }
            }

            Section("Server") {
                LabeledContent("Status") {
                    HStack(spacing: 8) {
                        StatusOrb(phase: client.phase, isLive: false, size: 18)
                        Text(client.headline)
                    }
                }
                LabeledContent("Server") {
                    if client.isRunning {
                        Button("Stop") { client.perform { try await client.stop() } }
                    } else {
                        Button("Start") { client.perform { try await client.start() } }
                            .disabled(client.link != .ready)
                    }
                }
            }
        }
    }

    private func setOpenAtLogin(_ on: Bool) {
        do {
            if on { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
            loginError = nil
        } catch {
            loginError = error.localizedDescription
        }
        let enabled = SMAppService.mainApp.status == .enabled
        if enabled != openAtLogin { openAtLogin = enabled }
    }
}

#Preview {
    GeneralPane()
        .formStyle(.grouped)
        .environment(HelperClient.preview())
}
