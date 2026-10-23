import AppKit
import SwiftUI

/// Black windows over the physical displays while a curtain is up. The app draws them because
/// it is a GUI process: the helper's CGDisplayCapture failed live, and powering a display off
/// once left it dark until reboot. The windows die with the app, so a quit or crash never
/// leaves a screen black; the helper's own exit empties the list through `HelperClient`.
@MainActor
final class CurtainShield {
    private var wanted: [CGDirectDisplayID] = []
    private var windows: [CGDirectDisplayID: NSWindow] = [:]

    init() {
        // The session display joining or leaving moves the physical screens.
        NotificationCenter.default.addObserver(
            forName: NSApplication.didChangeScreenParametersNotification, object: nil, queue: .main
        ) { [weak self] _ in MainActor.assumeIsolated { self?.layout() } }
    }

    /// Covers exactly `displays` (empty: uncover all).
    func cover(_ displays: [CGDirectDisplayID]) {
        wanted = displays
        layout()
    }

    private func layout() {
        var screens: [CGDirectDisplayID: NSScreen] = [:]
        for screen in NSScreen.screens {
            if let id = screen.displayID { screens[id] = screen }
        }
        for (id, window) in windows where !wanted.contains(id) || screens[id] == nil {
            window.orderOut(nil)
            windows[id] = nil
        }
        for id in wanted {
            guard let screen = screens[id] else { continue }
            let window = windows[id] ?? Self.makeWindow()
            window.setFrame(screen.frame, display: true)
            window.orderFrontRegardless()
            windows[id] = window
        }
    }

    private static func makeWindow() -> NSWindow {
        let window = NSWindow(contentRect: .zero, styleMask: .borderless, backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.level = NSWindow.Level(rawValue: Int(CGShieldingWindowLevel()))
        window.collectionBehavior = [.canJoinAllSpaces, .stationary, .ignoresCycle]
        window.ignoresMouseEvents = true
        window.backgroundColor = .black
        window.hasShadow = false
        window.animationBehavior = .none
        window.contentView = NSHostingView(rootView: ShieldView())
        return window
    }
}

private struct ShieldView: View {
    var body: some View {
        Text("This Mac is in use remotely")
            .font(.title3)
            .foregroundStyle(Color(white: 0.4))
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(.black)
    }
}

private extension NSScreen {
    var displayID: CGDirectDisplayID? {
        (deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.uint32Value
    }
}
