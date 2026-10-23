import MacRDPKit
import SwiftUI

/// Where to point a Remote Desktop client: each address with the port, one click to copy.
struct ConnectCard: View {
    @Environment(HelperClient.self) private var client

    var body: some View {
        let addresses = client.status.addresses ?? []
        VStack(alignment: .leading, spacing: 6) {
            Text("Connect to")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            if addresses.isEmpty {
                Text(client.isRunning ? "Looking for this Mac's addresses…" : "Start the server to see this Mac's addresses.")
                    .font(.callout)
                    .foregroundStyle(.tertiary)
                    .padding(.vertical, 4)
            } else {
                ForEach(addresses, id: \.self) { address in
                    CopyChip(value: "\(address):\(client.port.formatted(.number.grouping(.never)))")
                }
            }
        }
    }
}

#Preview {
    ConnectCard()
        .environment(HelperClient.preview())
        .padding()
        .frame(width: 340)
}
