// swift-tools-version: 6.2
// The app's model layer: the helper control protocol, the client that drives the helper,
// and a fake helper. It is a package so `swift test` covers it without launching the app.
import PackageDescription

let package = Package(
    name: "MacRDPKit",
    platforms: [.macOS(.v26)],
    products: [.library(name: "MacRDPKit", targets: ["MacRDPKit"])],
    targets: [
        .target(name: "MacRDPKit"),
        .testTarget(name: "MacRDPKitTests", dependencies: ["MacRDPKit"]),
    ]
)
