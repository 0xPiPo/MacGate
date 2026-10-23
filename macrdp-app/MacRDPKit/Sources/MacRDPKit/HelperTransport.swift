import Foundation

/// A running helper: lines go in through `send`, lines and the exit come back through the
/// launch callbacks, on the main actor.
@MainActor
public protocol HelperTransport: AnyObject {
    func launch(onLine: @escaping @MainActor @Sendable (Data) -> Void,
                onExit: @escaping @MainActor @Sendable () -> Void) throws
    func send(_ line: Data) throws
    /// Ends the helper. Called on app quit and before a relaunch.
    func shutdown()
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
        process.terminationHandler = { _ in Task { @MainActor in onExit() } }
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
        // EOF on stdin is the helper's signal to clean up (restore displays) and exit.
        try? stdin?.close()
        stdin = nil
        guard let process else { return }
        self.process = nil
        let deadline = Date.now.addingTimeInterval(2)
        while process.isRunning, Date.now < deadline { usleep(20_000) }
        if process.isRunning { process.terminate() }
    }
}
