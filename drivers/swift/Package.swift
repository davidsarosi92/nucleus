// swift-tools-version:5.9
// The Swift driver for nucleus (drivers/swift): `Nucleus` connects to a
// server; `NucleusEmbedded` opens a database inside the app — the engine
// as a static library in NucleusEmbedded.xcframework, which
// build-xcframework.sh makes (PLAN.md §M14.23).  Without it the package
// has the driver alone.
import Foundation
import PackageDescription

let here = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
let embedded = FileManager.default.fileExists(
    atPath: here.appendingPathComponent("NucleusEmbedded.xcframework").path)

var products: [Product] = [.library(name: "Nucleus", targets: ["Nucleus"])]
var targets: [Target] = [
    // The C driver (drivers/c), compiled as it is.
    .target(name: "CNucleus"),
    .target(name: "Nucleus", dependencies: ["CNucleus"]),
    .testTarget(name: "NucleusTests", dependencies: ["Nucleus"]),
]
if embedded {
    products.append(.library(name: "NucleusEmbedded", targets: ["NucleusEmbedded"]))
    targets += [
        .binaryTarget(name: "NucleusEmbeddedBinary", path: "NucleusEmbedded.xcframework"),
        // nc_connect_io over the engine's nce_send / nce_recv.
        .target(name: "CNucleusEmbedded", dependencies: ["CNucleus", "NucleusEmbeddedBinary"]),
        .target(name: "NucleusEmbedded", dependencies: ["Nucleus", "CNucleusEmbedded", "NucleusEmbeddedBinary"]),
        .testTarget(name: "NucleusEmbeddedTests", dependencies: ["NucleusEmbedded"]),
    ]
}

let package = Package(
    name: "Nucleus",
    platforms: [.iOS(.v15), .macOS(.v12)],
    products: products,
    targets: targets,
    cLanguageStandard: .c99
)
