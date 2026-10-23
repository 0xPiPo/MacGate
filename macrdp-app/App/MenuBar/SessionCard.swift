import Charts
import MacRDPKit
import SwiftUI

/// The connected device, what it receives, and live frame rate and latency.
struct SessionCard: View {
    /// The popover is on screen. Off screen the card reads no metrics, so new samples do not
    /// redraw it.
    var isLive = true

    @Environment(HelperClient.self) private var client

    var body: some View {
        let session = client.session
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .center, spacing: 10) {
                Image(systemName: "iphone.gen3.radiowaves.left.and.right")
                    .font(.title2)
                    .foregroundStyle(.green)
                    .frame(width: 30)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 2) {
                    Text(session.displayName)
                        .font(.headline)
                        .lineLimit(1)
                    Text(details(session))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Spacer(minLength: 8)
                Button("Disconnect", role: .destructive) {
                    client.perform { try await client.disconnect() }
                }
                .buttonStyle(.glass)
                .controlSize(.small)
                .help("End the session. The server keeps listening.")
            }

            if isLive {
                HStack(spacing: 10) {
                    MetricSparkline(title: "Frame rate", unit: "fps", tint: .green,
                                    values: client.metrics.map { ($0.id, $0.fps) })
                    MetricSparkline(title: "Latency", unit: "ms", tint: .orange,
                                    values: client.metrics.map { ($0.id, $0.latencyMs) })
                }
            }
        }
        .padding(12)
        .background(.fill.quinary, in: .rect(cornerRadius: 14))
        .overlay(RoundedRectangle(cornerRadius: 14).strokeBorder(.green.opacity(0.25)))
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Session")
    }

    private func details(_ session: SessionInfo) -> String {
        [session.resolutionLabel, session.codecLabel, qualityLabel].compactMap(\.self).joined(separator: " · ")
    }

    /// RemoteFX sessions: "High", or "Auto (Medium)" while Auto picks the level.
    private var qualityLabel: String? {
        guard let effective = client.rfxQuality.flatMap(VideoQuality.init(rawValue:)) else { return nil }
        let setting = client.config[path: "rfx_quality"]?.stringValue.flatMap(VideoQuality.init(rawValue:)) ?? .auto
        return setting == .auto ? "Auto (\(effective.title))" : effective.title
    }
}

/// A number with a tiny trend line under it.
struct MetricSparkline: View {
    let title: String
    let unit: String
    let tint: Color
    let values: [(Int, Double)]

    var body: some View {
        let latest = values.last?.1
        let low = (values.map(\.1).min() ?? 0) * 0.9
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.caption2)
                .foregroundStyle(.secondary)
            HStack(alignment: .firstTextBaseline, spacing: 2) {
                Text(latest.map { $0.formatted(.number.precision(.fractionLength(0))) } ?? "–")
                    .font(.title3.monospacedDigit().weight(.semibold))
                    .contentTransition(.numericText(value: latest ?? 0))
                    .animation(.snappy, value: latest)
                Text(unit)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Chart(values, id: \.0) { point in
                AreaMark(x: .value("Sample", point.0), yStart: .value(title, low), yEnd: .value(title, point.1))
                    .foregroundStyle(tint.opacity(0.25).gradient)
                    .interpolationMethod(.catmullRom)
                LineMark(x: .value("Sample", point.0), y: .value(title, point.1))
                    .foregroundStyle(tint)
                    .lineStyle(StrokeStyle(lineWidth: 1.5, lineCap: .round))
                    .interpolationMethod(.catmullRom)
            }
            .chartXAxis(.hidden)
            .chartYAxis(.hidden)
            .chartYScale(domain: .automatic(includesZero: false))
            .chartLegend(.hidden)
            .frame(height: 26)
            .accessibilityHidden(true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(8)
        .background(.background.opacity(0.5), in: .rect(cornerRadius: 10))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(title)
        .accessibilityValue(latest.map { "\($0.formatted(.number.precision(.fractionLength(0)))) \(unit)" } ?? "No data")
    }
}

#Preview {
    SessionCard()
        .environment(HelperClient.preview(
            status: ServerStatus(running: true, state: "connected", port: 3389, addresses: ["192.168.1.24"]),
            session: SessionInfo(active: true, peer: "192.168.1.31:51234", clientName: "iPhone",
                                 width: 2556, height: 1179, codec: "rfx-progressive")))
        .padding()
        .frame(width: 340)
}
