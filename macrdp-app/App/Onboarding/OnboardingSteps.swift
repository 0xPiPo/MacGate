import MacRDPKit
import SwiftUI

/// Screen Recording and Accessibility, with checkmarks that update while the step is open.
struct PermissionsStep: View {
    @Environment(HelperClient.self) private var client

    var body: some View {
        VStack(spacing: 18) {
            StepText(title: "Allow access",
                     message: "macOS asks you to approve two things before anyone can see or control this Mac.")
            VStack(spacing: 10) {
                PermissionRow(kind: .screen, title: "Screen Recording",
                              detail: "Sends your screen to the device you connect from.",
                              granted: client.permissions?.screenCapture ?? false)
                PermissionRow(kind: .accessibility, title: "Accessibility",
                              detail: "Lets that device move the pointer and type.",
                              granted: client.permissions?.accessibility ?? false)
            }
        }
        // Poll only while this step is on screen: permissions change in System Settings.
        .task {
            while !Task.isCancelled {
                recheck()
                try? await Task.sleep(for: .seconds(2))
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            recheck()
        }
    }

    /// A Screen Recording grant shows only in a fresh helper; Accessibility reads live.
    private func recheck() {
        if client.permissions?.screenCapture == true {
            Task { try? await client.refresh() }
        } else {
            client.recheckPermissions()
        }
    }
}

private struct PermissionRow: View {
    let kind: PermissionKind
    let title: String
    let detail: String
    let granted: Bool

    @Environment(HelperClient.self) private var client
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: granted ? "checkmark.circle.fill" : "circle.dashed")
                .font(.title2)
                .foregroundStyle(granted ? AnyShapeStyle(.green) : AnyShapeStyle(.tertiary))
                .contentTransition(.symbolEffect(.replace))
                .symbolEffect(.bounce, value: granted)
                .symbolEffectsRemoved(reduceMotion)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.headline)
                Text(detail).font(.callout).foregroundStyle(.secondary)
            }
            Spacer()
            if !granted {
                Button("Allow") { client.perform { try await client.requestPermission(kind) } }
                    .buttonStyle(.glass)
                Button("Open Settings", systemImage: "arrow.up.forward.app") {
                    client.perform { try await client.openPrivacySettings(kind) }
                }
                .labelStyle(.iconOnly)
                .buttonStyle(.glass)
                .help("Open \(title) in System Settings")
            }
        }
        .padding(12)
        .background(.fill.quinary, in: .rect(cornerRadius: 12))
        .animation(.snappy, value: granted)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("\(title), \(granted ? "allowed" : "not allowed")")
    }
}

struct PasswordStep: View {
    @Environment(HelperClient.self) private var client

    var body: some View {
        VStack(spacing: 18) {
            StepText(title: "Sign in with this Mac",
                     message: "Remote Desktop apps sign in with your Mac account. Enter its password once so macrdp can check it.")
            Form { EnrollmentForm() }
                .formStyle(.grouped)
                .scrollDisabled(true)
                .frame(height: 170)
        }
    }
}

struct ConnectStep: View {
    @Environment(HelperClient.self) private var client

    var body: some View {
        VStack(spacing: 16) {
            StepText(title: "Connect from your phone",
                     message: "In Windows App (Remote Desktop), add a PC with one of these addresses and sign in with your Mac account.")
            VStack(alignment: .leading, spacing: 10) {
                ConnectCard()
                if !client.isRunning {
                    Button("Start Server", systemImage: "play.fill") {
                        client.perform { try await client.start() }
                    }
                    .buttonStyle(.glass)
                }
                if let fingerprint = client.auth?.fingerprint {
                    Text("Certificate fingerprint")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                    CopyChip(value: fingerprint, lines: 2)
                }
            }
            .frame(maxWidth: 400)
        }
    }
}

#Preview("Permissions") {
    PermissionsStep()
        .environment(HelperClient.preview(permissions: PermissionsInfo(screenCapture: true, accessibility: false)))
        .padding()
        .frame(width: 520)
}

#Preview("Connect") {
    ConnectStep()
        .environment(HelperClient.preview())
        .padding()
        .frame(width: 520)
}
