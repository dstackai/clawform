import SwiftUI

@main
struct CalculatorApp: App {
    private let defaultWindowSize = CalculatorView.defaultWindowSize

    var body: some Scene {
        WindowGroup {
            CalculatorView()
                .frame(width: defaultWindowSize.width, height: defaultWindowSize.height)
        }
        .defaultSize(width: defaultWindowSize.width, height: defaultWindowSize.height)
        .windowStyle(.hiddenTitleBar)
        .windowResizability(.contentSize)
    }
}
