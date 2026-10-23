import Foundation

/// Any JSON value. The helper owns the config schema, so the app keeps it untyped and reads
/// the few keys it shows.
public enum JSONValue: Codable, Hashable, Sendable {
    case null
    case bool(Bool)
    case int(Int)
    case double(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    public init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let v = try? c.decode(Bool.self) { self = .bool(v) }
        else if let v = try? c.decode(Int.self) { self = .int(v) }
        else if let v = try? c.decode(Double.self) { self = .double(v) }
        else if let v = try? c.decode(String.self) { self = .string(v) }
        else if let v = try? c.decode([JSONValue].self) { self = .array(v) }
        else { self = .object(try c.decode([String: JSONValue].self)) }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let v): try c.encode(v)
        case .int(let v): try c.encode(v)
        case .double(let v): try c.encode(v)
        case .string(let v): try c.encode(v)
        case .array(let v): try c.encode(v)
        case .object(let v): try c.encode(v)
        }
    }

    public var boolValue: Bool? { if case .bool(let v) = self { v } else { nil } }
    public var stringValue: String? { if case .string(let v) = self { v } else { nil } }
    public var intValue: Int? {
        switch self {
        case .int(let v): v
        case .double(let v) where v.rounded() == v: Int(v)
        default: nil
        }
    }
}

extension Dictionary where Key == String, Value == JSONValue {
    /// Reads a dotted path such as `audio.enabled`.
    public subscript(path path: String) -> JSONValue? {
        get {
            let parts = path.split(separator: ".").map(String.init)
            var node: JSONValue? = .object(self)
            for part in parts {
                guard case .object(let dict) = node else { return nil }
                node = dict[part]
            }
            return node
        }
        set {
            var parts = path.split(separator: ".").map(String.init)
            let head = parts.removeFirst()
            if parts.isEmpty {
                self[head] = newValue
            } else {
                var child: [String: JSONValue] = if case .object(let d) = self[head] { d } else { [:] }
                child[path: parts.joined(separator: ".")] = newValue
                self[head] = .object(child)
            }
        }
    }
}
