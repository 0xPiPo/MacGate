import MacRDPKit
import SwiftUI

struct VideoPane: View {
    @Environment(HelperClient.self) private var client

    var body: some View {
        Form {
            Section {
                Picker("Quality", selection: client.binding("rfx_quality", default: VideoQuality.auto)) {
                    ForEach(VideoQuality.allCases) { Text($0.title).tag($0) }
                }
                .pickerStyle(.segmented)
                Picker("Frame rate", selection: client.binding("frame_rate", default: 60)) {
                    ForEach([15, 30, 60], id: \.self) { Text("\($0) fps").tag($0) }
                }
                .pickerStyle(.segmented)
            } footer: {
                Text("Quality applies to clients that use RemoteFX instead of H.264. Auto starts at Medium, drops a level when the connection falls behind and climbs back after 10 seconds without trouble. Low stays responsive on slow networks; High keeps text crisp.")
            }
        }
    }
}

#Preview {
    VideoPane()
        .formStyle(.grouped)
        .environment(HelperClient.preview())
}
