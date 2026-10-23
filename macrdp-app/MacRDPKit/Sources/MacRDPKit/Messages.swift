import Foundation

// Helper -> app messages (docs/superpowers/specs/2026-10-17-swiftui-app-design.md).
// Every field is optional so a message with missing or extra fields still decodes.

public enum ServerPhase: String, Sendable {
    case stopped, starting, listening, connected, error
}

public struct ServerStatus: Decodable, Equatable, Sendable {
    public var running: Bool?
    public var state: String?
    public var port: Int?
    public var addresses: [String]?
    public var error: String?

    public init(running: Bool? = nil, state: String? = nil, port: Int? = nil,
                addresses: [String]? = nil, error: String? = nil) {
        self.running = running; self.state = state; self.port = port
        self.addresses = addresses; self.error = error
    }

    public var phase: ServerPhase { state.flatMap(ServerPhase.init(rawValue:)) ?? .stopped }
}

public struct SessionInfo: Decodable, Equatable, Sendable {
    public var active: Bool?
    public var peer: String?
    public var clientName: String?
    public var width: Int?
    public var height: Int?
    public var codec: String?
    public var displayMode: String?
    public var since: String?
    /// A curtain is up: the app covers `shieldDisplays` with black windows.
    public var curtain: Bool?
    /// Physical CGDirectDisplayIDs to cover. Can outlast `active` briefly (a replaced session
    /// keeps its curtain for the next one); empty once the displays are restored.
    public var shieldDisplays: [UInt32]?

    public init(active: Bool? = nil, peer: String? = nil, clientName: String? = nil,
                width: Int? = nil, height: Int? = nil, codec: String? = nil,
                displayMode: String? = nil, since: String? = nil,
                curtain: Bool? = nil, shieldDisplays: [UInt32]? = nil) {
        self.active = active; self.peer = peer; self.clientName = clientName
        self.width = width; self.height = height; self.codec = codec
        self.displayMode = displayMode; self.since = since
        self.curtain = curtain; self.shieldDisplays = shieldDisplays
    }

    public var isActive: Bool { active ?? false }
}

public struct MetricsMessage: Decodable, Equatable, Sendable {
    public var fps: Double?
    public var latencyMs: Double?
    public var bitrateKbps: Double?
    /// RemoteFX sessions: the quality in effect ("low", "medium", "high"), auto's level included.
    public var rfxQuality: String?
}

/// One sparkline sample. `id` is a running sequence number, the chart's x axis.
public struct MetricPoint: Identifiable, Equatable, Sendable {
    public var id: Int
    public var fps: Double
    public var latencyMs: Double
    public init(id: Int, fps: Double, latencyMs: Double) {
        self.id = id; self.fps = fps; self.latencyMs = latencyMs
    }
}

public struct AuthInfo: Decodable, Equatable, Sendable {
    public var user: String?
    public var enrolled: Bool?
    public var reenrollNeeded: Bool?
    public var fingerprint: String?

    public init(user: String? = nil, enrolled: Bool? = nil, reenrollNeeded: Bool? = nil,
                fingerprint: String? = nil) {
        self.user = user; self.enrolled = enrolled
        self.reenrollNeeded = reenrollNeeded; self.fingerprint = fingerprint
    }

    public var isReady: Bool { (enrolled ?? false) && !(reenrollNeeded ?? false) }
}

public struct PermissionsInfo: Decodable, Equatable, Sendable {
    public var screenCapture: Bool?
    public var accessibility: Bool?

    public init(screenCapture: Bool? = nil, accessibility: Bool? = nil) {
        self.screenCapture = screenCapture; self.accessibility = accessibility
    }

    public var allGranted: Bool { (screenCapture ?? false) && (accessibility ?? false) }
}

public struct LogEntry: Decodable, Equatable, Sendable {
    public var level: String?
    public var message: String?
}

public struct Reply: Decodable, Equatable, Sendable {
    public var id: Int?
    public var ok: Bool?
    public var error: String?
    public var restartRequired: Bool?
    /// `test_standby`: external monitors that accepted the standby command.
    public var monitors: Int?
}

public enum HelperEvent: Equatable, Sendable {
    case reply(Reply)
    case status(ServerStatus)
    case session(SessionInfo)
    case metrics(MetricsMessage)
    case auth(AuthInfo)
    case permissions(PermissionsInfo)
    case config([String: JSONValue])
    case log(LogEntry)

    private struct Envelope: Decodable { var type: String }

    /// Decodes one line. Unknown types and malformed lines return nil and are ignored.
    public static func parse(_ line: Data) -> HelperEvent? {
        let snake = JSONDecoder()
        snake.keyDecodingStrategy = .convertFromSnakeCase
        guard let type = try? JSONDecoder().decode(Envelope.self, from: line).type else { return nil }
        func decode<T: Decodable>(_: T.Type) -> T? { try? snake.decode(T.self, from: line) }
        switch type {
        case "reply": return decode(Reply.self).map(HelperEvent.reply)
        case "status": return decode(ServerStatus.self).map(HelperEvent.status)
        case "session": return decode(SessionInfo.self).map(HelperEvent.session)
        case "metrics": return decode(MetricsMessage.self).map(HelperEvent.metrics)
        case "auth": return decode(AuthInfo.self).map(HelperEvent.auth)
        case "permissions": return decode(PermissionsInfo.self).map(HelperEvent.permissions)
        case "log": return decode(LogEntry.self).map(HelperEvent.log)
        case "config":
            // Plain decoder: the snake_case strategy would rewrite the config's own keys.
            guard var dict = try? JSONDecoder().decode([String: JSONValue].self, from: line) else { return nil }
            // The spec leaves open whether fields are flat or nested under "config"; take both.
            if case .object(let nested) = dict["config"] { return .config(nested) }
            dict["type"] = nil
            return .config(dict)
        default: return nil
        }
    }
}
