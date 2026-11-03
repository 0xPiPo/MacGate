import Foundation

/// A running helper: lines go in through `send`, lines and the exit come back through the
/// launch callbacks, on the main actor.
@MainActor
public protocol HelperTransport: AnyObject {
    func launch(onLine: @escaping @MainActor @Sendable (Data) -> Void,
                onExit: @escaping @MainActor @Sendable () -> Void) throws
    func send(_ line: Data) throws
    /// Ends the helper and waits for it (app quit).
    func shutdown()
    /// Ends the helper without blocking the caller, then calls `done` once it is gone (so a
    /// replacement can bind the same port).
    func retire(then done: @escaping @MainActor @Sendable () -> Void)
}

public extension HelperTransport {
    func retire(then done: @escaping @MainActor @Sendable () -> Void) {
        shutdown()
        done()
    }
}

/// Splits a byte stream into lines, dropping any line longer than `maxLine` (the protocol
/// caps messages at 64 KiB).
public struct LineFramer: Sendable {
    public static let maxLine = 64 * 1024
    private var buffer: [UInt8] = []
    private var overflowed = false

    public init() {}

    /// Feeds one byte; returns a complete line (without the newline) when one ends.
    public mutating func push(_ byte: UInt8) -> Data? {
        guard byte == 0x0A else {
            if buffer.count < Self.maxLine { buffer.append(byte) } else { overflowed = true }
            return nil
        }
        defer { buffer.removeAll(keepingCapacity: true); overflowed = false }
        return overflowed || buffer.isEmpty ? nil : Data(buffer)
    }
}

/// Runs `macrdp-server control` with JSON Lines on stdin/stdout. stderr is inherited.
@MainActor
public final class ProcessTransport: HelperTransport {
    private let executable: URL
    private let arguments: [String]
    private var process: Process?
    private var stdin: FileHandle?
    /// Helpers asked to exit, kept alive until they have: Foundation reaps a child only while
    /// its `Process` exists, and `reap` watches the pid until that happens.
    private var retiring: [ObjectIdentifier: Process] = [:]

    public init(executable: URL, arguments: [String] = ["control"]) {
        self.executable = executable
        self.arguments = arguments
    }

    public func launch(onLine: @escaping @MainActor @Sendable (Data) -> Void,
                       onExit: @escaping @MainActor @Sendable () -> Void) throws {
        let process = Process()
        process.executableURL = executable
        process.arguments = arguments
        let input = Pipe(), output = Pipe()
        process.standardInput = input
        process.standardOutput = output
        process.terminationHandler = { [weak self] exited in
            let id = ObjectIdentifier(exited)
            Task { @MainActor in
                self?.retiring[id] = nil
                onExit()
            }
        }
        try process.run()
        self.process = process
        stdin = input.fileHandleForWriting

        let reader = output.fileHandleForReading
        Task.detached {
            var framer = LineFramer()
            do {
                for try await byte in reader.bytes {
                    if let line = framer.push(byte) { await onLine(line) }
                }
            } catch {}
        }
    }

    public func send(_ line: Data) throws {
        guard let stdin else { throw HelperError.notRunning }
        try stdin.write(contentsOf: line)
    }

    public func shutdown() {
        // Helpers retired moments ago are still being reaped on another thread, which the
        // app is about to end: reap them here too, so none outlives the app.
        let pids = [close()].compactMap { $0 } + retiring.values.filter(\.isRunning).map(\.processIdentifier)
        for pid in pids { Self.reap(pid) }
    }

    /// Off the main thread: the UI never waits for the old helper to go.
    public func retire(then done: @escaping @MainActor @Sendable () -> Void) {
        guard let pid = close() else { return done() }
        DispatchQueue.global(qos: .utility).async {
            Self.reap(pid)
            Task { @MainActor in done() }
        }
    }

    /// EOF on stdin is the helper's signal to clean up (restore displays) and exit.
    private func close() -> pid_t? {
        try? stdin?.close()
        stdin = nil
        guard let process else { return nil }
        self.process = nil
        guard process.isRunning else { return nil }
        retiring[ObjectIdentifier(process)] = process
        return process.processIdentifier
    }

    /// Gives the helper 2 s to exit on EOF, then SIGTERM, then SIGKILL: a helper wedged in
    /// a blocking call must not outlive the app holding the RDP port. The pid is gone once
    /// Foundation reaps the child (its `Process` is kept in `retiring` until then).
    private nonisolated static func reap(_ pid: pid_t) {
        func waitExit(_ seconds: Double) -> Bool {
            let deadline = Date.now.addingTimeInterval(seconds)
            while Date.now < deadline {
                if kill(pid, 0) != 0 { return true }
                usleep(20_000)
            }
            return kill(pid, 0) != 0
        }
        if waitExit(2) { return }
        kill(pid, SIGTERM)
        if waitExit(1) { return }
        kill(pid, SIGKILL)
    }
}
