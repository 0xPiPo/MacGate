import MacRDPKit
import SwiftUI

struct AdvancedPane: View {
    @Environment(HelperClient.self) private var client

    var body: some View {
        Form {
            Section {
                Toggle("Play the Mac's sound on the client", isOn: client.binding("audio.enabled", default: true))
                Toggle("Share the clipboard", isOn: client.binding("clipboard.enabled", default: true))
            }

            Section("Diagnostics") {
                Picker("Log level", selection: client.binding("log_level", default: LogLevel.warn)) {
                    ForEach(LogLevel.allCases) { Text($0.title).tag($0) }
                }
                LabeledContent("Log file") {
                    Button("Open Log", action: openLog)
                }
                if let message = client.lastLog?.message {
                    LabeledContent("Last message") {
                        Text(message)
                            .font(.callout.monospaced())
                            .textSelection(.enabled)
                            .lineLimit(3)
                    }
                }
            }
        }
    }

    private func openLog() {
        let dir = URL.libraryDirectory.appending(path: "Logs/macrdp")
        let file = dir.appending(path: "macrdp.log")
        NSWorkspace.shared.open(FileManager.default.fileExists(atPath: file.path) ? file : dir)
    }
}

#Preview {
    AdvancedPane()
        .formStyle(.grouped)
        .environment(HelperClient.preview())
}
