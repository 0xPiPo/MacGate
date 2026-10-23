import MacRDPKit
import SwiftUI

struct DisplayPane: View {
    @Environment(HelperClient.self) private var client

    private var displayMode: DisplayMode {
        client.config["display_mode"]?.stringValue.flatMap(DisplayMode.init(rawValue:)) ?? .curtain
    }

    var body: some View {
        Form {
            Section {
                Picker("Display mode", selection: client.binding("display_mode", default: DisplayMode.curtain)) {
                    ForEach(DisplayMode.allCases) { mode in
                        VStack(alignment: .leading, spacing: 2) {
                            Label(mode.title, systemImage: mode.symbol)
                            Text(mode.blurb)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                        .tag(mode)
                    }
                }
                .pickerStyle(.radioGroup)
            } footer: {
                Text("Applies to the next connection.")
            }

            if displayMode == .curtain {
                CurtainStandbySection()
            }

            Section {
                Toggle("Show the pointer in the session", isOn: client.binding("show_cursor", default: true))
            }
        }
    }
}

#Preview {
    DisplayPane()
        .formStyle(.grouped)
        .environment(HelperClient.preview())
}
