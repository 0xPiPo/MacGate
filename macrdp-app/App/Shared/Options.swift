// The config values the UI offers, as the helper spells them (ServerConfig in macrdp-core).

enum DisplayMode: String, CaseIterable, Identifiable {
    case curtain, mirror, extend

    var id: Self { self }

    var title: String {
        switch self {
        case .curtain: "Curtain"
        case .mirror: "Mirror"
        case .extend: "Extend"
        }
    }

    var symbol: String {
        switch self {
        case .curtain: "eye.slash"
        case .mirror: "rectangle.on.rectangle"
        case .extend: "rectangle.split.2x1"
        }
    }

    var blurb: String {
        switch self {
        case .curtain: "Only you see the desktop, and the Mac's keyboard and mouse are locked while you're connected."
        case .mirror: "You see exactly what's on your Mac's display."
        case .extend: "The session is an extra display beside your Mac's."
        }
    }
}

/// `rfx_quality`: the picture quality for clients that use RemoteFX rather than H.264.
/// Auto (the helper's default) picks low, medium or high as the connection allows.
enum VideoQuality: String, CaseIterable, Identifiable {
    case auto, low, medium, high

    var id: Self { self }
    var title: String { rawValue.capitalized }
}

enum LogLevel: String, CaseIterable, Identifiable {
    case error, warn, info, debug, trace

    var id: Self { self }
    var title: String { self == .warn ? "Warning" : rawValue.capitalized }
}
