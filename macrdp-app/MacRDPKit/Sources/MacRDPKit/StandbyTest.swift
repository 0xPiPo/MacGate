import Observation

/// The "Test monitor standby" flow: the helper sleeps the monitors for 3 s, then the user says
/// whether the monitor went dark (a monitor can accept the command and ignore it), and the
/// answer becomes `curtain_standby_ok`.
@MainActor
@Observable
public final class StandbyTest {
    /// The monitors are asleep for the test; the button stays disabled.
    public private(set) var isRunning = false
    /// The test finished with a monitor answering: ask "Did your monitor go dark?".
    public var isAsking = false
    /// Why the last test gave no question to answer.
    public private(set) var message: String?

    public init() {}

    public func run(_ client: HelperClient) async {
        guard !isRunning else { return }
        isRunning = true
        message = nil
        defer { isRunning = false }
        do {
            let monitors = try await client.testStandby()
            if monitors > 0 {
                isAsking = true
            } else {
                message = "No external monitor answered."
                try await client.setConfig("curtain_standby_ok", .bool(false))
            }
        } catch {
            message = error.localizedDescription
        }
    }

    public func answer(wentDark: Bool, _ client: HelperClient) async {
        isAsking = false
        do {
            try await client.setConfig("curtain_standby_ok", .bool(wentDark))
        } catch {
            message = error.localizedDescription
        }
    }
}
