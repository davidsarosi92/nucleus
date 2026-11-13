// swift-tools-version:5.9
// The Swift examples: `swift run quickstart` against a server; `swift run
// anywhere <where>` against a server or a database in the process (that
// one needs drivers/swift/build-xcframework.sh first).  Not in a directory
// named `swift`: SwiftPM names a package by its directory, and the
// driver's is drivers/swift.
import Foundation
import PackageDescription

let embedded = FileManager.default.fileExists(
    atPath: URL(fileURLWithPath: #filePath).deletingLastPathComponent()
        .appendingPathComponent("../../drivers/swift/NucleusEmbedded.xcframework").path)

var targets: [Target] = [
    .executableTarget(name: "quickstart", dependencies: [.product(name: "Nucleus", package: "swift")]),
]
if embedded {
    targets.append(.executableTarget(name: "anywhere",
                                     dependencies: [.product(name: "NucleusEmbedded", package: "swift")]))
}

let package = Package(
    name: "examples",
    platforms: [.macOS(.v12)],
    dependencies: [.package(path: "../../drivers/swift")],
    targets: targets
)
