import MacRDPKit
import SwiftUI

/// The menu-bar window: status, the live session, display mode, addresses, and controls.
struct PopoverView: View {
    @Environment(HelperClient.self) private var client
    // The menu-bar window is key exactly while it is open; this gates every running animation.
    @Environment(\.appearsActive) private var isOpen
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            header

            if client.needsSetup {
                Banner(symbol: "sparkles", tint: .accentColor,
                       text: "Finish setting up macrdp so you can connect.",
                       action: ("Set Up…", { show("onboarding") }))
            }
            if let error = client.commandError {
                Banner(symbol: "exclamationmark.triangle.fill", tint: .red, text: error,
                       action: ("Dismiss", { client.commandError = nil }))
            }
            if client.restartRequired {
                Banner(symbol: "arrow.clockwise", tint: .orange,
                       text: "Restart the server to apply your changes.",
                       action: ("Restart", { client.perform { try await client.restartServer() } }))
            }

            if client.session.isActive {
                SessionCard(isLive: isOpen)
                    .transition(reduceMotion ? .opacity : .scale(scale: 0.96, anchor: .top).combined(with: .opacity))
            }

            VStack(alignment: .leading, spacing: 6) {
                Text("Display")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                DisplayModePicker(selection: client.binding("display_mode", default: DisplayMode.curtain))
                if client.session.isActive {
                    Text("Changes apply to the next connection.")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }
            }

            ConnectCard()

            Divider()
            footer
        }
        .padding(16)
        .frame(width: 340)
        .animation(reduceMotion ? .easeOut(duration: 0.2) : .spring(duration: 0.4, bounce: 0.12),
                   value: client.session.isActive)
        .animation(.easeOut(duration: 0.2), value: client.needsSetup)
        .animation(.easeOut(duration: 0.2), value: client.restartRequired)
        .animation(.easeOut(duration: 0.2), value: client.commandError)
    }

    private var header: some View {
        HStack(spacing: 12) {
            StatusOrb(phase: client.phase, isLive: isOpen)
            VStack(alignment: .leading, spacing: 1) {
                Text("macrdp")
                    .font(.title3.weight(.semibold))
                Text(client.headline)
                    .font(.callout)
                    .foregroundStyle(client.phase == .error ? AnyShapeStyle(.red) : AnyShapeStyle(.secondary))
                    .lineLimit(2)
                    .contentTransition(.opacity)
                    .animation(.easeOut(duration: 0.2), value: client.headline)
            }
            Spacer(minLength: 0)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("macrdp, \(client.headline)")
    }

    private var footer: some View {
        GlassEffectContainer {
            HStack(spacing: 8) {
                if client.isRunning {
                    Button("Stop Server", systemImage: "stop.fill") {
                        client.perform { try await client.stop() }
                    }
                    .buttonStyle(.glass)
                } else {
                    Button("Start Server", systemImage: "play.fill") {
                        client.perform { try await client.start() }
                    }
                    .buttonStyle(.glassProminent)
                    .disabled(client.link != .ready)
                }
                Spacer()
                Button("Settings…", systemImage: "gearshape") {
                    show("settings")
                }
                .labelStyle(.iconOnly)
                .buttonStyle(.glass)
                .keyboardShortcut(",")
                .help("Settings")
                Button("Quit macrdp", systemImage: "power") {
                    NSApp.terminate(nil)
                }
                .labelStyle(.iconOnly)
                .buttonStyle(.glass)
                .keyboardShortcut("q")
                .help("Quit macrdp and stop the server")
            }
            .controlSize(.large)
        }
    }

    private func show(_ window: String) {
        NSApp.activate()
        openWindow(id: window)
    }
}

/// A one-line notice with an optional action.
struct Banner: View {
    let symbol: String
    let tint: Color
    let text: String
    var action: (title: String, run: () -> Void)?

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: symbol)
                .foregroundStyle(tint)
                .accessibilityHidden(true)
            Text(text)
                .font(.callout)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 4)
            if let action {
                Button(action.title, action: action.run)
                    .buttonStyle(.glass)
                    .controlSize(.small)
            }
        }
        .padding(10)
        .background(tint.opacity(0.12), in: .rect(cornerRadius: 12))
        .transition(.opacity)
    }
}

#Preview("Listening") {
    PopoverView()
        .environment(HelperClient.preview())
}

#Preview("Connected") {
    PopoverView()
        .environment(HelperClient.preview(
            status: ServerStatus(running: true, state: "connected", port: 3389,
                                 addresses: ["192.168.1.24", "10.8.0.2"]),
            session: SessionInfo(active: true, peer: "192.168.1.31:51234", clientName: "iPhone",
                                 width: 2556, height: 1179, codec: "avc420")))
}

#Preview("Stopped, needs setup") {
    PopoverView()
        .environment(HelperClient.preview(
            status: ServerStatus(running: false, state: "stopped"),
            auth: AuthInfo(user: "pipo", enrolled: false),
            permissions: PermissionsInfo(screenCapture: true, accessibility: false)))
}

#Preview("Error") {
    PopoverView()
        .environment(HelperClient.preview(
            status: ServerStatus(running: false, state: "error", port: 3389,
                                 error: "Port 3389 is already in use."),
            restartRequired: true))
}
