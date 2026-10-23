import AppKit
import Observation

public enum HelperError: LocalizedError, Equatable {
    case notRunning, timedOut, helperExited, rejected(String)

    public var errorDescription: String? {
        switch self {
        case .notRunning: "The macrdp helper is not running."
        case .timedOut: "The macrdp helper did not answer in time."
        case .helperExited: "The macrdp helper stopped unexpectedly."
        case .rejected(let message): message
        }
    }
}

public enum PermissionKind: String, Sendable {
    case screen, accessibility
}

/// Owns the helper process and mirrors its state for the UI.
@MainActor
@Observable
public final class HelperClient {
    public enum Link: Equatable, Sendable {
        case launching, ready, restarting, failed(String)
    }

    public private(set) var link: Link = .launching
    public private(set) var status = ServerStatus()
    public private(set) var session = SessionInfo()
    public private(set) var metrics: [MetricPoint] = []
    /// The RemoteFX quality in effect for the session (nil for H.264 sessions).
    public private(set) var rfxQuality: String?
    public private(set) var auth: AuthInfo?
    public private(set) var permissions: PermissionsInfo?
    public private(set) var config: [String: JSONValue] = [:]
    public private(set) var lastLog: LogEntry?
    /// The last failed command started through `perform`, for the UI to show and clear.
    public var commandError: String?
    /// A setting changed that only applies after the listener restarts.
    public private(set) var restartRequired = false
    /// The server should be running: set by start/stop, restored after a helper restart.
    public var wantsRunning: Bool

    @ObservationIgnored private let transport: HelperTransport
    @ObservationIgnored private var pending: [Int: CheckedContinuation<Reply, Error>] = [:]
    @ObservationIgnored private var nextID = 0
    @ObservationIgnored private var failures = 0
    @ObservationIgnored private var generation = 0
    @ObservationIgnored private var shuttingDown = false
    @ObservationIgnored private var launched = false
    @ObservationIgnored private var metricSeq = 0
    @ObservationIgnored private let maxBackoff: Duration

    public init(transport: HelperTransport, wantsRunning: Bool = false,
                maxBackoff: Duration = .seconds(30)) {
        self.transport = transport
        self.wantsRunning = wantsRunning
        self.maxBackoff = maxBackoff
    }

    // MARK: Derived state

    public var phase: ServerPhase {
        switch link {
        case .ready: status.phase
        case .launching, .restarting: .starting
        case .failed: .error
        }
    }

    public var errorMessage: String? {
        if case .failed(let message) = link { return message }
        return status.phase == .error ? (status.error ?? "The server reported an error.") : nil
    }

    public var needsSetup: Bool {
        guard let auth, let permissions else { return false }
        return !auth.isReady || !permissions.allGranted
    }

    public var port: Int { status.port ?? config["port"]?.intValue ?? 3389 }

    /// Physical displays to keep black for the curtain; empty when the helper is gone.
    public var shieldDisplays: [UInt32] { session.shieldDisplays ?? [] }

    // MARK: Lifecycle

    /// Starts the helper once. Later calls do nothing.
    public func launch() {
        guard !launched else { return }
        launched = true
        NotificationCenter.default.addObserver(
            forName: NSApplication.willTerminateNotification,
            object: nil, queue: .main
        ) { [weak self] _ in MainActor.assumeIsolated { self?.shutdown() } }
        spawn()
    }

    /// Ends the helper for good (app quit).
    public func shutdown() {
        shuttingDown = true
        transport.shutdown()
        failPending(.helperExited)
    }

    private func spawn() {
        generation += 1
        let gen = generation
        link = failures == 0 ? .launching : .restarting
        do {
            try transport.launch(
                onLine: { [weak self] line in self?.receive(line, generation: gen) },
                onExit: { [weak self] in self?.helperExited(generation: gen) })
            link = .ready
            Task {
                guard (try? await self.request("get_state")) != nil else { return }
                self.startIfReady()
            }
        } catch {
            link = .failed(error.localizedDescription)
            scheduleRelaunch()
        }
    }

    private func helperExited(generation gen: Int) {
        guard gen == generation, !shuttingDown else { return }
        failPending(.helperExited)
        status = ServerStatus()
        session = SessionInfo()
        metrics = []
        rfxQuality = nil
        link = .restarting
        scheduleRelaunch()
    }

    private func scheduleRelaunch() {
        failures += 1
        // 0.5 s, 1 s, 2 s … capped. Reset once the new helper reports a status.
        let delay = min(.milliseconds(500) * (1 << min(failures - 1, 10)), maxBackoff)
        Task {
            try? await Task.sleep(for: delay)
            guard !self.shuttingDown else { return }
            self.spawn()
        }
    }

    private func failPending(_ error: HelperError) {
        let waiting = pending
        pending = [:]
        for continuation in waiting.values { continuation.resume(throwing: error) }
    }

    // MARK: Incoming

    private func receive(_ line: Data, generation gen: Int) {
        guard gen == generation, let event = HelperEvent.parse(line) else { return }
        switch event {
        case .reply(let reply):
            guard let id = reply.id, let continuation = pending.removeValue(forKey: id) else { return }
            if reply.ok == true {
                continuation.resume(returning: reply)
            } else {
                continuation.resume(throwing: HelperError.rejected(reply.error ?? "The request failed."))
            }
        case .status(let new):
            failures = 0
            status = new
            if new.running == false { restartRequired = false }
        case .session(let new):
            if !new.isActive { metrics = []; rfxQuality = nil }
            session = new
        case .metrics(let m):
            guard session.isActive else { return }
            if rfxQuality != m.rfxQuality { rfxQuality = m.rfxQuality }
            metricSeq += 1
            metrics.append(MetricPoint(id: metricSeq, fps: m.fps ?? 0, latencyMs: m.latencyMs ?? 0))
            if metrics.count > 60 { metrics.removeFirst(metrics.count - 60) }
        case .auth(let new): auth = new
        case .permissions(let new): permissions = new
        case .config(let new): config = new
        case .log(let entry): lastLog = entry
        }
    }

    // MARK: Requests

    /// Sends one command and waits for its reply. Throws `rejected` when `ok` is false.
    @discardableResult
    public func request(_ cmd: String, _ fields: [String: JSONValue] = [:],
                        timeout: Duration = .seconds(10)) async throws -> Reply {
        guard link == .ready else { throw HelperError.notRunning }
        nextID += 1
        let id = nextID
        var message = fields
        message["id"] = .int(id)
        message["cmd"] = .string(cmd)
        var line = try JSONEncoder().encode(message)
        line.append(0x0A)
        return try await withCheckedThrowingContinuation { continuation in
            pending[id] = continuation
            do {
                try transport.send(line)
            } catch {
                pending.removeValue(forKey: id)?.resume(throwing: HelperError.notRunning)
                return
            }
            Task {
                try? await Task.sleep(for: timeout)
                self.pending.removeValue(forKey: id)?.resume(throwing: HelperError.timedOut)
            }
        }
    }

    /// Runs a command from a button: failures land in `commandError` instead of being thrown.
    public func perform(_ operation: @escaping @MainActor () async throws -> Void) {
        Task {
            do {
                try await operation()
                commandError = nil
            } catch {
                commandError = error.localizedDescription
            }
        }
    }

    public func start() async throws {
        wantsRunning = true
        try await request("start")
    }

    public func stop() async throws {
        wantsRunning = false
        try await request("stop")
    }

    public func restartServer() async throws {
        try await request("stop")
        try await request("start")
        restartRequired = false
    }

    /// Starts the server if it should run and the Mac is set up: enrolled, with both
    /// permissions. Starting earlier would make the helper's macOS prompts appear before
    /// onboarding explains them. Called after every helper launch and when onboarding ends.
    public func startIfReady() {
        guard wantsRunning, link == .ready, status.phase == .stopped,
              auth?.isReady == true, permissions?.allGranted == true else { return }
        Task { try? await self.request("start") }
    }

    /// Re-reads the permissions from a fresh helper: macOS answers a process's Screen
    /// Recording preflight once, so a grant in System Settings shows only in a new process.
    /// With a session running it only refreshes, so the session survives.
    public func recheckPermissions() {
        guard !session.isActive, !shuttingDown else {
            Task { try? await self.refresh() }
            return
        }
        transport.shutdown()
        failPending(.helperExited)
        spawn() // a new generation: the old helper's exit is ignored
    }

    public func disconnect() async throws { try await request("disconnect") }

    /// Sleeps the external monitors for 3 s, then wakes them. Returns how many accepted it.
    public func testStandby() async throws -> Int {
        try await request("test_standby", timeout: .seconds(15)).monitors ?? 0
    }

    public var standbyStatus: StandbyStatus { StandbyStatus(config: config) }

    public func refresh() async throws { try await request("get_state") }

    /// Applies the value locally at once, then persists it through the helper; reverts on failure.
    public func setConfig(_ key: String, _ value: JSONValue) async throws {
        let previous = config[path: key]
        config[path: key] = value
        do {
            let reply = try await request("set_config", ["key": .string(key), "value": value])
            if reply.restartRequired == true, status.running == true { restartRequired = true }
        } catch {
            config[path: key] = previous
            throw error
        }
    }

    /// The password goes to the helper and nowhere else: it is never logged or kept here.
    public func enroll(password: String) async throws {
        try await request("enroll", ["password": .string(password)], timeout: .seconds(30))
        try? await refresh()
    }

    public func requestPermission(_ which: PermissionKind) async throws {
        try await request("request_permissions", ["which": .string(which.rawValue)])
    }

    public func openPrivacySettings(_ which: PermissionKind) async throws {
        try await request("open_privacy_settings", ["which": .string(which.rawValue)])
    }

    // MARK: Previews

    /// A client frozen in a given state, for previews. It never launches a helper.
    public static func preview(
        status: ServerStatus = ServerStatus(running: true, state: "listening", port: 3389,
                                            addresses: ["192.168.1.24", "10.8.0.2"]),
        session: SessionInfo = SessionInfo(),
        auth: AuthInfo = AuthInfo(user: "pipo", enrolled: true, reenrollNeeded: false,
                                  fingerprint: FakeTransport.fingerprint),
        permissions: PermissionsInfo = PermissionsInfo(screenCapture: true, accessibility: true),
        config: [String: JSONValue] = FakeTransport.defaultConfig,
        restartRequired: Bool = false
    ) -> HelperClient {
        let client = HelperClient(transport: FakeTransport())
        client.link = .ready
        client.status = status
        client.session = session
        client.auth = auth
        client.permissions = permissions
        client.config = config
        client.restartRequired = restartRequired
        if session.codec == "rfx-progressive" { client.rfxQuality = "medium" }
        if session.isActive {
            client.metrics = (0..<60).map { i in
                let t = Double(i)
                return MetricPoint(id: i, fps: 54 + 5 * sin(t / 4) + Double(i % 3),
                                   latencyMs: 14 + 4 * cos(t / 5) + Double(i % 4))
            }
        }
        return client
    }
}
