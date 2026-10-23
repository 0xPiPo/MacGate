import MacRDPKit
import SwiftUI

extension ServerPhase {
    var tint: Color {
        switch self {
        case .stopped: .gray
        case .starting: .orange
        case .listening: .accentColor
        case .connected: .green
        case .error: .red
        }
    }
}

extension StandbyStatus {
    var title: String {
        switch self {
        case .works: "Works"
        case .notSupported: "Not supported"
        case .notTested: "Not tested"
        }
    }
}

extension HelperClient {
    /// One line under the title: what the server is doing right now.
    var headline: String {
        switch phase {
        case .stopped: "Server is off"
        case .starting: link == .restarting ? "Restarting the helper…" : "Starting…"
        case .listening: "Ready on port \(port.formatted(.number.grouping(.never)))"
        case .connected: "Connected to \(session.displayName)"
        case .error: errorMessage ?? "Something went wrong"
        }
    }

    var isRunning: Bool { status.running ?? false }

    func binding(_ key: String, default fallback: Bool) -> Binding<Bool> {
        Binding(
            get: { self.config[path: key]?.boolValue ?? fallback },
            set: { value in self.perform { try await self.setConfig(key, .bool(value)) } })
    }

    func binding(_ key: String, default fallback: Int) -> Binding<Int> {
        Binding(
            get: { self.config[path: key]?.intValue ?? fallback },
            set: { value in self.perform { try await self.setConfig(key, .int(value)) } })
    }

    func binding<T: RawRepresentable & Sendable>(_ key: String, default fallback: T) -> Binding<T> where T.RawValue == String {
        Binding(
            get: { self.config[path: key]?.stringValue.flatMap(T.init(rawValue:)) ?? fallback },
            set: { value in self.perform { try await self.setConfig(key, .string(value.rawValue)) } })
    }
}

extension SessionInfo {
    var displayName: String { clientName ?? peer?.split(separator: ":").first.map(String.init) ?? "a remote device" }

    var codecLabel: String? {
        switch codec {
        case "avc420": "H.264"
        case "rfx-progressive": "RemoteFX"
        case "uncompressed": "Uncompressed"
        default: codec
        }
    }

    var resolutionLabel: String? {
        guard let width, let height else { return nil }
        return "\(width.formatted(.number.grouping(.never)))×\(height.formatted(.number.grouping(.never)))"
    }
}
