import Foundation
import Testing
@testable import MacRDPKit

/// Polls until `condition` holds, for state that arrives through async hops.
@MainActor
private func eventually(_ condition: () -> Bool) async -> Bool {
    for _ in 0..<200 {
        if condition() { return true }
        try? await Task.sleep(for: .milliseconds(10))
    }
    return condition()
}

/// Records what the client sends and lets a test answer or crash at will.
@MainActor
private final class ScriptedTransport: HelperTransport {
    var sent: [[String: JSONValue]] = []
    var launches = 0
    private var onLine: (@MainActor @Sendable (Data) -> Void)?
    private var onExit: (@MainActor @Sendable () -> Void)?
    private var previousExit: (@MainActor @Sendable () -> Void)?

    func launch(onLine: @escaping @MainActor @Sendable (Data) -> Void,
                onExit: @escaping @MainActor @Sendable () -> Void) throws {
        launches += 1
        previousExit = self.onExit
        self.onLine = onLine
        self.onExit = onExit
    }
    func send(_ line: Data) throws {
        sent.append(try JSONDecoder().decode([String: JSONValue].self, from: line))
    }
    var shutdowns = 0
    func shutdown() { shutdowns += 1 }
    func emit(_ json: String) { onLine?(Data(json.utf8)) }
    func crash() { onExit?() }
    /// The helper replaced by the latest launch exits.
    func previousExits() { previousExit?() }

    /// What a helper sends for `get_state`, then the reply to it.
    func answerState(_ id: JSONValue?, granted: Bool) {
        emit(#"{"type":"status","running":false,"state":"stopped"}"#)
        emit(#"{"type":"auth","user":"u","enrolled":true,"reenroll_needed":false}"#)
        emit(#"{"type":"permissions","screen_capture":\#(granted),"accessibility":true}"#)
        emit(#"{"type":"reply","id":\#(id?.intValue ?? 0),"ok":true}"#)
    }
}

struct LineFramerTests {
    @Test func splitsLinesAndDropsOverlongOnes() {
        var framer = LineFramer()
        var lines: [String] = []
        let input = Array("a\n\nbc\n".utf8) + Array(repeating: UInt8(ascii: "x"), count: LineFramer.maxLine + 1)
            + Array("\nd\n".utf8)
        for byte in input {
            if let line = framer.push(byte) { lines.append(String(decoding: line, as: UTF8.self)) }
        }
        #expect(lines == ["a", "bc", "d"])
    }
}

struct ParseTests {
    @Test func toleratesUnknownTypesFieldsAndGarbage() {
        #expect(HelperEvent.parse(Data(#"{"type":"future_thing","x":1}"#.utf8)) == nil)
        #expect(HelperEvent.parse(Data("not json".utf8)) == nil)
        let status = HelperEvent.parse(Data(#"{"type":"status","running":true,"state":"listening","port":3390,"addresses":["10.0.0.2"],"extra":{"a":1}}"#.utf8))
        #expect(status == .status(ServerStatus(running: true, state: "listening", port: 3390, addresses: ["10.0.0.2"])))
        let auth = HelperEvent.parse(Data(#"{"type":"auth","user":"u","enrolled":true,"reenroll_needed":true}"#.utf8))
        #expect(auth == .auth(AuthInfo(user: "u", enrolled: true, reenrollNeeded: true)))
        // An unknown state string still decodes and reads as stopped.
        #expect(ServerStatus(state: "warming_up").phase == .stopped)
    }

    @Test func configKeepsSnakeCaseKeysFlatOrNested() {
        let flat = HelperEvent.parse(Data(#"{"type":"config","display_mode":"mirror","audio":{"enabled":false}}"#.utf8))
        #expect(flat == .config(["display_mode": .string("mirror"), "audio": .object(["enabled": .bool(false)])]))
        let nested = HelperEvent.parse(Data(#"{"type":"config","config":{"show_cursor":true}}"#.utf8))
        #expect(nested == .config(["show_cursor": .bool(true)]))
    }

    @Test func metricsDecodeTheRemoteFXQualityWhenPresent() {
        let rfx = HelperEvent.parse(Data(#"{"type":"metrics","fps":30,"latency_ms":12,"rfx_quality":"low"}"#.utf8))
        #expect(rfx == .metrics(MetricsMessage(fps: 30, latencyMs: 12, rfxQuality: "low")))
        let h264 = HelperEvent.parse(Data(#"{"type":"metrics","fps":30}"#.utf8))
        #expect(h264 == .metrics(MetricsMessage(fps: 30)))
    }

    @Test func dottedPathReadsAndWrites() {
        var config: [String: JSONValue] = ["audio": .object(["enabled": .bool(true), "channels": .int(2)])]
        config[path: "audio.enabled"] = .bool(false)
        #expect(config[path: "audio.enabled"] == .bool(false))
        #expect(config[path: "audio.channels"]?.intValue == 2)
    }
}

@MainActor
struct HelperClientTests {
    @Test func requestsCarryIncreasingIdsAndMatchReplies() async throws {
        let transport = ScriptedTransport()
        let client = HelperClient(transport: transport)
        client.launch()
        #expect(await eventually { transport.sent.count == 1 })
        #expect(transport.sent[0]["cmd"] == .string("get_state"))
        #expect(transport.sent[0]["id"] == .int(1))

        let reply = Task { try await client.request("disconnect") }
        #expect(await eventually { transport.sent.count == 2 })
        transport.emit(#"{"type":"reply","id":2,"ok":true}"#)
        #expect(try await reply.value.ok == true)

        let rejected = Task { try await client.disconnect() }
        #expect(await eventually { transport.sent.count == 3 })
        transport.emit(#"{"type":"reply","id":3,"ok":false,"error":"no session"}"#)
        await #expect(throws: HelperError.rejected("no session")) { try await rejected.value }
    }

    @Test func requestTimesOut() async {
        let client = HelperClient(transport: ScriptedTransport())
        client.launch()
        await #expect(throws: HelperError.timedOut) {
            try await client.request("stop", timeout: .milliseconds(50))
        }
    }

    @Test func helperCrashFailsPendingAndRelaunchesThenRestarts() async throws {
        let transport = ScriptedTransport()
        let client = HelperClient(transport: transport, wantsRunning: true, maxBackoff: .milliseconds(20))
        client.launch()
        let pending = Task { try await client.request("stop") }
        #expect(await eventually { transport.sent.count == 2 })
        transport.crash()
        await #expect(throws: HelperError.helperExited) { try await pending.value }
        #expect(client.phase == .starting)
        #expect(await eventually { transport.launches == 2 })
        // The new helper reports a ready Mac; the client restores the running server.
        #expect(await eventually { transport.sent.last?["cmd"] == .string("get_state") })
        transport.answerState(transport.sent.last?["id"], granted: true)
        #expect(await eventually { transport.sent.last?["cmd"] == .string("start") })
    }

    @Test func neverStartsBeforeEnrollmentAndBothPermissions() async throws {
        let transport = ScriptedTransport()
        let client = HelperClient(transport: transport, wantsRunning: true)
        client.launch()
        #expect(await eventually { transport.sent.count == 1 })
        transport.answerState(transport.sent[0]["id"], granted: false)
        #expect(await eventually { client.permissions != nil })
        try await Task.sleep(for: .milliseconds(50))
        #expect(transport.sent.map(\.self["cmd"]) == [.string("get_state")], "start would make the helper prompt")
    }

    @Test func permissionRecheckRunsAFreshHelper() async throws {
        let transport = ScriptedTransport()
        let client = HelperClient(transport: transport)
        client.launch()
        #expect(await eventually { transport.sent.count == 1 })
        client.recheckPermissions()
        #expect(transport.shutdowns == 1)
        #expect(transport.launches == 2)
        #expect(await eventually { transport.sent.last?["cmd"] == .string("get_state") && transport.sent.count == 2 })
        // The old helper's exit is not mistaken for a crash.
        transport.previousExits()
        try await Task.sleep(for: .milliseconds(50))
        #expect(client.link == .ready)
    }

    @Test func shieldDisplaysFollowTheSessionAndClearWhenTheHelperDies() async throws {
        let transport = ScriptedTransport()
        let client = HelperClient(transport: transport, maxBackoff: .seconds(60))
        client.launch()
        transport.emit(#"{"type":"session","active":true,"peer":"10.8.0.3","curtain":true,"shield_displays":[1,4]}"#)
        #expect(client.shieldDisplays == [1, 4])
        transport.emit(#"{"type":"session","active":false,"curtain":false,"shield_displays":[]}"#)
        #expect(client.shieldDisplays.isEmpty)
        transport.emit(#"{"type":"session","active":false,"curtain":true,"shield_displays":[2]}"#)
        #expect(client.shieldDisplays == [2])
        transport.crash()
        #expect(client.shieldDisplays.isEmpty)
    }

    @Test func rfxQualityFollowsMetricsAndClearsWithTheSession() {
        let transport = ScriptedTransport()
        let client = HelperClient(transport: transport, maxBackoff: .seconds(60))
        client.launch()
        transport.emit(#"{"type":"metrics","rfx_quality":"high"}"#)
        #expect(client.rfxQuality == nil, "no session yet")
        transport.emit(#"{"type":"session","active":true,"codec":"rfx-progressive"}"#)
        transport.emit(#"{"type":"metrics","fps":30,"rfx_quality":"low"}"#)
        #expect(client.rfxQuality == "low")
        transport.emit(#"{"type":"session","active":false}"#)
        #expect(client.rfxQuality == nil)
    }

    @Test func fakeHelperRoundTrip() async throws {
        let client = HelperClient(transport: FakeTransport())
        client.launch()
        #expect(await eventually { client.auth != nil && client.config["port"] != nil })
        try await client.start()
        #expect(await eventually { client.phase == .listening })
        try await client.setConfig("port", .int(3390))
        #expect(client.restartRequired)
        #expect(client.config["port"] == .int(3390))
        await #expect(throws: HelperError.rejected("That password doesn't match this Mac account.")) {
            try await client.enroll(password: "wrong")
        }
        try await client.enroll(password: "secret")
        #expect(client.auth?.enrolled == true)
        client.shutdown()
    }

    @Test func processTransportPipesLinesAndSeesExit() async throws {
        let transport = ProcessTransport(executable: URL(filePath: "/bin/cat"), arguments: [])
        var lines: [String] = []
        var exited = false
        try transport.launch(onLine: { lines.append(String(decoding: $0, as: UTF8.self)) },
                             onExit: { exited = true })
        try transport.send(Data("{\"type\":\"log\"}\n".utf8))
        #expect(await eventually { lines == [#"{"type":"log"}"#] })
        transport.shutdown() // closes stdin: cat exits on EOF
        #expect(await eventually { exited })
    }
}

struct StandbyStatusTests {
    @Test func followsTheConfig() {
        #expect(StandbyStatus(config: [:]) == .notTested)
        #expect(StandbyStatus(config: ["curtain_standby_ok": .null]) == .notTested)
        #expect(StandbyStatus(config: ["curtain_standby_ok": .bool(true)]) == .works)
        #expect(StandbyStatus(config: ["curtain_standby_ok": .bool(false)]) == .notSupported)
    }
}

@MainActor
struct StandbyTestFlowTests {
    /// A launched client whose `get_state` went out.
    private func launched() async -> (ScriptedTransport, HelperClient) {
        let transport = ScriptedTransport()
        let client = HelperClient(transport: transport)
        client.launch()
        _ = await eventually { transport.sent.count == 1 }
        return (transport, client)
    }

    private func reply(_ transport: ScriptedTransport, _ extra: String = "") {
        let id = transport.sent.last?["id"]?.intValue ?? 0
        transport.emit(#"{"type":"reply","id":\#(id),"ok":true\#(extra)}"#)
    }

    @Test func aMonitorThatWentDarkMarksStandbyAsWorking() async {
        let (transport, client) = await launched()
        let flow = StandbyTest()
        let run = Task { await flow.run(client) }
        #expect(await eventually { transport.sent.last?["cmd"] == .string("test_standby") })
        #expect(flow.isRunning)
        reply(transport, #","monitors":1"#)
        await run.value
        #expect(!flow.isRunning)
        #expect(flow.isAsking)
        #expect(client.standbyStatus == .notTested, "nothing is saved before the answer")

        let answer = Task { await flow.answer(wentDark: true, client) }
        #expect(await eventually { transport.sent.last?["cmd"] == .string("set_config") })
        #expect(transport.sent.last?["key"] == .string("curtain_standby_ok"))
        #expect(transport.sent.last?["value"] == .bool(true))
        reply(transport)
        await answer.value
        #expect(!flow.isAsking)
        #expect(client.standbyStatus == .works)
    }

    @Test func aMonitorThatStayedOnMarksStandbyUnsupported() async {
        let (transport, client) = await launched()
        let flow = StandbyTest()
        flow.isAsking = true
        let answer = Task { await flow.answer(wentDark: false, client) }
        #expect(await eventually { transport.sent.last?["value"] == .bool(false) })
        reply(transport)
        await answer.value
        #expect(client.standbyStatus == .notSupported)
    }

    @Test func noMonitorAnsweringMarksStandbyUnsupportedWithoutAsking() async {
        let (transport, client) = await launched()
        let flow = StandbyTest()
        let run = Task { await flow.run(client) }
        #expect(await eventually { transport.sent.last?["cmd"] == .string("test_standby") })
        reply(transport, #","monitors":0"#)
        #expect(await eventually { transport.sent.last?["cmd"] == .string("set_config") })
        #expect(transport.sent.last?["value"] == .bool(false))
        reply(transport)
        await run.value
        #expect(!flow.isAsking)
        #expect(flow.message != nil)
        #expect(client.standbyStatus == .notSupported)
    }

    @Test func aRefusedTestShowsWhyAndAsksNothing() async {
        let (transport, client) = await launched()
        let flow = StandbyTest()
        let run = Task { await flow.run(client) }
        #expect(await eventually { transport.sent.last?["cmd"] == .string("test_standby") })
        let id = transport.sent.last?["id"]?.intValue ?? 0
        transport.emit(#"{"type":"reply","id":\#(id),"ok":false,"error":"cannot test monitor standby during a session"}"#)
        await run.value
        #expect(!flow.isRunning)
        #expect(!flow.isAsking)
        #expect(flow.message == "cannot test monitor standby during a session")
        #expect(client.standbyStatus == .notTested)
    }
}
