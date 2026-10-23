import Foundation

/// A helper that lives in-process and speaks the real protocol, so the app runs, and the
/// client is exercised end to end, without the Rust side. Set `MACRDP_FAKE_HELPER=1` to use it.
/// Starting the server simulates a phone connecting a few seconds later.
@MainActor
public final class FakeTransport: HelperTransport {
    public static let fingerprint =
        "3F:A2:9C:11:7B:E4:0D:58:C6:21:9A:F0:4E:B7:33:D2:8C:65:1F:A9:E0:47:5B:CC:12:D8:6E:93:0A:F5:7C:B4"
    public static let defaultConfig: [String: JSONValue] = [
        "port": .int(3389), "bind_address": .string("0.0.0.0"), "frame_rate": .int(60),
        "idle_timeout_secs": .int(1800), "lock_on_disconnect": .bool(false), "auto_unlock": .bool(false),
        "rfx_quality": .string("auto"), "display_mode": .string("curtain"),
        "show_cursor": .bool(true), "log_level": .string("warn"),
        "audio": .object(["enabled": .bool(true)]), "clipboard": .object(["enabled": .bool(true)]),
    ]

    private var onLine: (@MainActor @Sendable (Data) -> Void)?
    private var config = FakeTransport.defaultConfig
    private var running = false
    private var enrolled = false
    private var permissions = (screen: false, accessibility: false)
    private var sessionTask: Task<Void, Never>?

    public init() {}

    public func launch(onLine: @escaping @MainActor @Sendable (Data) -> Void,
                       onExit: @escaping @MainActor @Sendable () -> Void) throws {
        self.onLine = onLine
    }

    public func shutdown() {
        sessionTask?.cancel()
        onLine = nil
    }

    public func send(_ line: Data) throws {
        guard onLine != nil else { throw HelperError.notRunning }
        guard let message = try? JSONDecoder().decode([String: JSONValue].self, from: line),
              let id = message["id"]?.intValue else { return }
        let cmd = message["cmd"]?.stringValue ?? ""
        // Replies arrive asynchronously, as they would through a pipe.
        Task { await self.handle(cmd, id: id, message: message) }
    }

    private func handle(_ cmd: String, id: Int, message: [String: JSONValue]) async {
        switch cmd {
        case "get_state":
            reply(id)
            emitState()
        case "start":
            reply(id)
            guard !running else { return }
            running = true
            emitStatus("starting")
            try? await Task.sleep(for: .milliseconds(600))
            emitStatus("listening")
            simulateSession()
        case "stop":
            reply(id)
            running = false
            endSession()
            emitStatus("stopped")
        case "disconnect":
            reply(id)
            endSession()
            if running { emitStatus("listening") }
        case "set_config":
            guard let key = message["key"]?.stringValue, let value = message["value"] else {
                return reply(id, error: "key and value are required")
            }
            config[path: key] = value
            reply(id, extra: ["restart_required": .bool(key == "port" || key == "bind_address")])
            emit("config", .object(config))
        case "enroll":
            try? await Task.sleep(for: .milliseconds(900))
            guard message["password"]?.stringValue?.isEmpty == false,
                  message["password"]?.stringValue != "wrong" else {
                return reply(id, error: "That password doesn't match this Mac account.")
            }
            enrolled = true
            reply(id)
            emitAuth()
        case "request_permissions":
            reply(id)
            try? await Task.sleep(for: .seconds(1))
            if message["which"]?.stringValue == "screen" { permissions.screen = true }
            else { permissions.accessibility = true }
            emitPermissions()
        case "open_privacy_settings":
            reply(id)
        case "test_standby":
            try? await Task.sleep(for: .seconds(3))
            reply(id, extra: ["monitors": .int(1)])
        default:
            reply(id, error: "unknown command: \(cmd)")
        }
    }

    // MARK: Simulated session

    private func simulateSession() {
        sessionTask?.cancel()
        sessionTask = Task {
            try? await Task.sleep(for: .seconds(4))
            guard !Task.isCancelled, running else { return }
            emitStatus("connected")
            emit("session", .object([
                "active": .bool(true), "peer": .string("192.168.1.31:51234"),
                "client_name": .string("iPhone"), "width": .int(2556), "height": .int(1179),
                "codec": .string("avc420"), "display_mode": config["display_mode"] ?? .null,
                "since": .string(Date.now.formatted(.iso8601)),
            ]))
            var t = 0.0
            while !Task.isCancelled {
                t += 1
                emit("metrics", .object([
                    "fps": .int(Int(55 + 5 * sin(t / 4)) + Int.random(in: 0...2)),
                    "latency_ms": .double(14 + 4 * cos(t / 5) + Double.random(in: 0...3)),
                    "bitrate_kbps": .int(8000 + Int.random(in: 0...1500)),
                ]))
                try? await Task.sleep(for: .milliseconds(500))
            }
        }
    }

    private func endSession() {
        sessionTask?.cancel()
        sessionTask = nil
        emit("session", .object(["active": .bool(false)]))
    }

    // MARK: Emitting

    private func emitState() {
        emitStatus(running ? "listening" : "stopped")
        emitAuth()
        emit("config", .object(config))
        emitPermissions()
        emit("session", .object(["active": .bool(false)]))
    }

    private func emitStatus(_ state: String) {
        emit("status", .object([
            "running": .bool(running), "state": .string(state),
            "port": config["port"] ?? .int(3389),
            "addresses": .array(running ? [.string("192.168.1.24"), .string("10.8.0.2")] : []),
        ]))
    }

    private func emitAuth() {
        emit("auth", .object([
            "user": .string(NSUserName()), "enrolled": .bool(enrolled),
            "reenroll_needed": .bool(false), "fingerprint": .string(Self.fingerprint),
        ]))
    }

    private func emitPermissions() {
        emit("permissions", .object([
            "screen_capture": .bool(permissions.screen),
            "accessibility": .bool(permissions.accessibility),
        ]))
    }

    private func reply(_ id: Int, error: String? = nil, extra: [String: JSONValue] = [:]) {
        var fields = extra
        fields["id"] = .int(id)
        fields["ok"] = .bool(error == nil)
        if let error { fields["error"] = .string(error) }
        emit("reply", .object(fields))
    }

    private func emit(_ type: String, _ payload: JSONValue) {
        guard case .object(var fields) = payload, let onLine else { return }
        fields["type"] = .string(type)
        if let data = try? JSONEncoder().encode(fields) { onLine(data) }
    }
}
