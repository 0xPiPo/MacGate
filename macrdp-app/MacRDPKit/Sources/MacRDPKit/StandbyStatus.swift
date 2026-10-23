/// Whether the curtain can put the monitors to sleep, from config `curtain_standby_ok`.
public enum StandbyStatus: Equatable, Sendable {
    /// The user saw the monitor go dark in the standby test.
    case works
    /// The monitor stayed on, or no monitor answered: the curtain uses the black shield.
    case notSupported
    /// Never tested: the curtain uses the black shield.
    case notTested

    public init(config: [String: JSONValue]) {
        switch config["curtain_standby_ok"]?.boolValue {
        case true?: self = .works
        case false?: self = .notSupported
        case nil: self = .notTested
        }
    }
}
