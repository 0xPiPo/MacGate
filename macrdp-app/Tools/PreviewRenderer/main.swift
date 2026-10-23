// Renders the app's views, in their preview states, to PNGs without running the app.
// Built and run by Tools/render-previews.sh. No helper is launched: preview clients are frozen.
import AppKit
import MacRDPKit
import SwiftUI

let outDir = URL(filePath: CommandLine.arguments[1])
let app = NSApplication.shared
app.setActivationPolicy(.prohibited)

@MainActor
func render(_ name: String, dark: Bool = false, _ view: some View) {
    // cacheDisplay skips the window fill, so paint the background into the view itself.
    let host = NSHostingView(rootView: view.fixedSize()
        .background(Color(nsColor: .windowBackgroundColor))
        .environment(\.colorScheme, dark ? .dark : .light))
    host.appearance = NSAppearance(named: dark ? .darkAqua : .aqua)
    let size = host.fittingSize
    let window = NSWindow(contentRect: NSRect(origin: .zero, size: size), styleMask: [.borderless],
                          backing: .buffered, defer: false)
    window.appearance = host.appearance
    window.backgroundColor = dark ? NSColor(white: 0.17, alpha: 1) : NSColor(white: 0.95, alpha: 1)
    window.contentView = host
    host.frame = NSRect(origin: .zero, size: size)
    host.layoutSubtreeIfNeeded()
    RunLoop.main.run(until: .now.addingTimeInterval(0.3))
    guard let rep = host.bitmapImageRepForCachingDisplay(in: host.bounds) else { return }
    host.cacheDisplay(in: host.bounds, to: rep)
    let url = outDir.appending(path: "\(name).png")
    try? rep.representation(using: .png, properties: [:])?.write(to: url)
    print(url.path)
}

let connected = HelperClient.preview(
    status: ServerStatus(running: true, state: "connected", port: 3389, addresses: ["192.168.1.24", "10.8.0.2"]),
    session: SessionInfo(active: true, peer: "192.168.1.31:51234", clientName: "iPhone",
                         width: 2556, height: 1179, codec: "avc420"))
let needsSetup = HelperClient.preview(
    status: ServerStatus(running: false, state: "stopped"),
    auth: AuthInfo(user: "pipo", enrolled: false),
    permissions: PermissionsInfo(screenCapture: true, accessibility: false))
let failing = HelperClient.preview(
    status: ServerStatus(running: false, state: "error", port: 3389, error: "Port 3389 is already in use."),
    restartRequired: true)

render("popover-listening", PopoverView().environment(HelperClient.preview()))
render("popover-connected", PopoverView().environment(connected))
render("popover-connected-dark", dark: true, PopoverView().environment(connected))
render("session-card", SessionCard().padding().frame(width: 340).environment(connected))
render("orbs", HStack(spacing: 16) { ForEach([ServerPhase.stopped, .starting, .listening, .connected, .error], id: \.self) { StatusOrb(phase: $0, isLive: false) } }.padding())
render("popover-setup", PopoverView().environment(needsSetup))
render("popover-error", PopoverView().environment(failing))
render("settings-security", SecurityPane().formStyle(.grouped).frame(width: 520, height: 520).environment(HelperClient.preview()))
render("settings-display", DisplayPane().formStyle(.grouped).frame(width: 520, height: 420).environment(HelperClient.preview()))
render("onboarding-welcome", OnboardingView().environment(needsSetup))
render("onboarding-permissions", PermissionsStep().padding(30).frame(width: 560).environment(needsSetup))
render("onboarding-connect", ConnectStep().padding(30).frame(width: 560).environment(HelperClient.preview()))
