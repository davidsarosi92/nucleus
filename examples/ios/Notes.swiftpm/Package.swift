// swift-tools-version: 5.9
// An iOS app with a database inside it (PLAN.md §M14.23): notes kept in
// the app's Application Support directory, found by meaning as well as
// by words.  Open in Xcode (or Swift Playgrounds), or from a terminal:
// examples/ios/run.sh (the iOS simulator).  Needs
// drivers/swift/build-xcframework.sh first.
import AppleProductTypes
import PackageDescription

let package = Package(
    name: "Notes",
    platforms: [.iOS("16.0")],
    products: [
        .iOSApplication(
            name: "Notes",
            targets: ["Notes"],
            bundleIdentifier: "io.github.davidsarosi92.nucleus.notes",
            displayVersion: "1.0",
            bundleVersion: "1",
            appIcon: .placeholder(icon: .note),
            accentColor: .presetColor(.indigo),
            supportedDeviceFamilies: [.phone, .pad],
            supportedInterfaceOrientations: [.portrait, .landscapeLeft, .landscapeRight]
        ),
    ],
    dependencies: [.package(path: "../../../drivers/swift")],
    targets: [
        .executableTarget(
            name: "Notes",
            dependencies: [.product(name: "NucleusEmbedded", package: "swift")],
            path: "Sources"
        ),
    ]
)
