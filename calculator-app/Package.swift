// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "CalculatorApp",
    platforms: [
        .macOS(.v13)
    ],
    products: [
        .executable(
            name: "CalculatorApp",
            targets: ["CalculatorApp"]
        )
    ],
    dependencies: [
    ],
    targets: [
        .executableTarget(
            name: "CalculatorApp",
            path: "Sources/CalculatorApp"
        )
    ]
)
