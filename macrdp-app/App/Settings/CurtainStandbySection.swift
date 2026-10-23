import MacRDPKit
import SwiftUI

/// Curtain controls in Settings > Display: test whether the monitors honour DDC standby, and
/// keep them asleep once they do.
struct CurtainStandbySection: View {
    @Environment(HelperClient.self) private var client
    @State private var test = StandbyTest()

    var body: some View {
        Section {
            LabeledContent("Monitor standby", value: client.standbyStatus.title)

            Button {
                Task { await test.run(client) }
            } label: {
                HStack(spacing: 6) {
                    Text(test.isRunning ? "Testing…" : "Test monitor standby")
                    if test.isRunning {
                        ProgressView().controlSize(.small)
                    }
                }
            }
            .disabled(test.isRunning || client.session.isActive)

            if client.standbyStatus == .works {
                Toggle(isOn: client.binding("curtain_standby_keepalive", default: false)) {
                    Text("Keep monitor asleep")
                    Text("Re-send standby every 5 s, so the monitor's power button can't wake it during a session.")
                }
            }
        } header: {
            Text("Curtain")
        } footer: {
            Text(test.message ?? "The test puts your monitors to sleep for 3 seconds. Without working standby, the curtain covers your displays with black and moves their windows into the session.")
        }
        .alert("Did your monitor go dark?", isPresented: $test.isAsking) {
            Button("Yes") { Task { await test.answer(wentDark: true, client) } }
            Button("No") { Task { await test.answer(wentDark: false, client) } }
        } message: {
            Text("If it did, the curtain puts your monitors to sleep during a session. If not, it covers them with black instead.")
        }
    }
}

#Preview {
    Form { CurtainStandbySection() }
        .formStyle(.grouped)
        .environment(HelperClient.preview())
}
