import MacRDPKit
import SwiftUI

enum SettingsPane: String, CaseIterable, Identifiable {
    case general, display, video, security, advanced, about

    var id: Self { self }
    var title: String { rawValue.capitalized }

    var symbol: String {
        switch self {
        case .general: "gearshape"
        case .display: "display"
        case .video: "video"
        case .security: "lock.shield"
        case .advanced: "slider.horizontal.3"
        case .about: "info.circle"
        }
    }
}

struct SettingsView: View {
    @State private var pane: SettingsPane? = .general
    @Environment(HelperClient.self) private var client

    var body: some View {
        NavigationSplitView {
            List(SettingsPane.allCases, selection: $pane) { pane in
                Label(pane.title, systemImage: pane.symbol)
            }
            .navigationSplitViewColumnWidth(170)
            .toolbar(removing: .sidebarToggle)
        } detail: {
            Group {
                switch pane ?? .general {
                case .general: GeneralPane()
                case .display: DisplayPane()
                case .video: VideoPane()
                case .security: SecurityPane()
                case .advanced: AdvancedPane()
                case .about: AboutPane()
                }
            }
            .formStyle(.grouped)
            .navigationTitle((pane ?? .general).title)
            .safeAreaInset(edge: .bottom) {
                if client.restartRequired {
                    RestartBar()
                }
            }
        }
        .frame(minWidth: 680, minHeight: 470)
    }
}

/// Settings that only take effect after the listener restarts say so here.
private struct RestartBar: View {
    @Environment(HelperClient.self) private var client

    var body: some View {
        Banner(symbol: "arrow.clockwise", tint: .orange,
               text: "Some changes apply after the server restarts.",
               action: ("Restart Now", { client.perform { try await client.restartServer() } }))
            .padding()
    }
}

#Preview {
    SettingsView()
        .environment(HelperClient.preview(restartRequired: true))
}
