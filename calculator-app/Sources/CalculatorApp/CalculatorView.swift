import SwiftUI

struct CalculatorView: View {
    private static let layoutMetrics = LayoutMetrics(rows: 5, columns: 4)

    static var defaultWindowSize: CGSize { layoutMetrics.defaultWindowSize }

    @StateObject private var viewModel = CalculatorViewModel()

    private var layout: LayoutMetrics { Self.layoutMetrics }

    private var buttonLayout: [[CalculatorButtonType]] {
        [
            [.clear, .toggleSign, .percent, .operation(.divide)],
            [.digit("7"), .digit("8"), .digit("9"), .operation(.multiply)],
            [.digit("4"), .digit("5"), .digit("6"), .operation(.subtract)],
            [.digit("1"), .digit("2"), .digit("3"), .operation(.add)],
            [.digit("0"), .decimal, .equals]
        ]
    }

    private var gridColumns: [GridItem] {
        Array(
            repeating: GridItem(.fixed(layout.buttonSize), spacing: layout.buttonSpacing),
            count: layout.buttonColumns
        )
    }

    var body: some View {
        ZStack(alignment: .topTrailing) {
            backgroundGradient
                .ignoresSafeArea()

            VStack(alignment: .trailing, spacing: layout.verticalSpacing) {
                glassDisplay

                LazyVGrid(columns: gridColumns, spacing: layout.buttonSpacing) {
                    ForEach(Array(buttonLayout.joined().enumerated()), id: \.offset) { _, item in
                        button(for: item)
                    }
                }
                .frame(height: layout.buttonAreaHeight, alignment: .topTrailing)
            }
            .padding(.top, layout.topPadding)
            .padding(.horizontal, layout.horizontalPadding)
            .padding(.bottom, layout.bottomPadding)
        }
        .frame(width: layout.defaultWindowSize.width, height: layout.defaultWindowSize.height, alignment: .topTrailing)
    }

    private var backgroundGradient: some View {
        LinearGradient(
            gradient: Gradient(colors: [
                Color(red: 38 / 255, green: 38 / 255, blue: 67 / 255),
                Color(red: 17 / 255, green: 24 / 255, blue: 39 / 255),
                Color(red: 8 / 255, green: 11 / 255, blue: 22 / 255)
            ]),
            startPoint: .topLeading,
            endPoint: .bottomTrailing
        )
        .overlay(
            AngularGradient(
                gradient: Gradient(colors: [
                    Color(red: 0.99, green: 0.53, blue: 0.82).opacity(0.25),
                    Color(red: 0.53, green: 0.63, blue: 1.0).opacity(0.15),
                    Color(red: 0.39, green: 0.85, blue: 0.86).opacity(0.22),
                    Color(red: 0.99, green: 0.53, blue: 0.82).opacity(0.25)
                ]),
                center: .center
            )
            .blur(radius: 160)
        )
    }

    private var glassDisplay: some View {
        RoundedRectangle(cornerRadius: layout.displayCornerRadius, style: .continuous)
            .fill(Color.white.opacity(0.08))
            .overlay(
                RoundedRectangle(cornerRadius: layout.displayCornerRadius, style: .continuous)
                    .strokeBorder(Color.white.opacity(0.22), lineWidth: 1.2)
                    .blendMode(.overlay)
            )
            .background(
                RoundedRectangle(cornerRadius: layout.displayCornerRadius, style: .continuous)
                    .fill(Color.white.opacity(0.04))
                    .blur(radius: 24)
            )
            .shadow(color: Color.black.opacity(0.35), radius: 30, x: 0, y: 22)
            .overlay(
                RoundedRectangle(cornerRadius: layout.displayCornerRadius, style: .continuous)
                    .stroke(
                        LinearGradient(
                            gradient: Gradient(colors: [
                                Color.white.opacity(0.6),
                                Color.white.opacity(0.1)
                            ]),
                            startPoint: .topLeading,
                            endPoint: .bottomTrailing
                        ),
                        lineWidth: 0.9
                    )
                    .blendMode(.overlay)
            )
            .overlay(
                VStack(alignment: .trailing, spacing: layout.displayContentSpacing) {
                    Text(viewModel.secondaryDisplay)
                        .font(.system(size: 18, weight: .medium, design: .rounded))
                        .foregroundStyle(Color.white.opacity(0.6))
                        .frame(maxWidth: .infinity, alignment: .trailing)

                    Text(viewModel.display)
                        .font(.system(size: 64, weight: .bold, design: .rounded))
                        .foregroundStyle(Color.white.opacity(0.95))
                        .minimumScaleFactor(0.5)
                        .lineLimit(1)
                        .frame(maxWidth: .infinity, alignment: .trailing)
                }
                .padding(.horizontal, layout.displayHorizontalPadding)
                .padding(.vertical, layout.displayVerticalPadding),
                alignment: .bottomTrailing
            )
            .frame(maxWidth: .infinity)
            .frame(height: layout.displayHeight, alignment: .bottomTrailing)
    }

    private func button(for type: CalculatorButtonType) -> some View {
        Button(action: { handleTap(for: type) }) {
            Text(type.title)
                .font(.system(size: type == .equals ? 30 : 28, weight: .semibold, design: .rounded))
                .kerning(0.5)
                .frame(maxWidth: .infinity, minHeight: layout.buttonSize, maxHeight: layout.buttonSize)
        }
        .buttonStyle(CalculatorButtonStyle(type: type))
        .accessibilityIdentifier(type.accessibilityIdentifier)
        .gridCellColumns(type.gridSpan)
    }

    private func handleTap(for type: CalculatorButtonType) {
        withAnimation(.spring(response: 0.38, dampingFraction: 0.82, blendDuration: 0.25)) {
            switch type {
            case .digit(let value):
                viewModel.inputDigit(value)
            case .decimal:
                viewModel.inputDecimal()
            case .operation(let operation):
                viewModel.setOperation(operation)
            case .equals:
                viewModel.evaluate()
            case .clear:
                viewModel.reset()
            case .toggleSign:
                viewModel.toggleSign()
            case .percent:
                viewModel.percent()
            }
        }
    }
}

private struct CalculatorButtonStyle: ButtonStyle {
    let type: CalculatorButtonType

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .foregroundStyle(foregroundColor)
            .background(
                RoundedRectangle(cornerRadius: 28, style: .continuous)
                    .fill(backgroundGradient)
                    .overlay(
                        RoundedRectangle(cornerRadius: 28, style: .continuous)
                            .strokeBorder(Color.white.opacity(0.18), lineWidth: 1.1)
                            .blendMode(.screen)
                    )
                    .shadow(color: shadowColor, radius: 18, x: 0, y: 12)
                    .shadow(color: Color.black.opacity(0.35), radius: 24, x: 0, y: 18)
                    .overlay(
                        RoundedRectangle(cornerRadius: 28, style: .continuous)
                            .fill(Color.white.opacity(configuration.isPressed ? 0.12 : 0.02))
                    )
            )
            .scaleEffect(configuration.isPressed ? 0.95 : 1.0)
            .animation(.spring(response: 0.26, dampingFraction: 0.86), value: configuration.isPressed)
    }

    private var foregroundColor: Color {
        switch type {
        case .clear, .toggleSign, .percent:
            return Color.white.opacity(0.92)
        case .operation, .equals:
            return Color.white
        case .digit, .decimal:
            return Color.white.opacity(0.95)
        }
    }

    private var backgroundGradient: LinearGradient {
        switch type {
        case .operation(let op):
            return LinearGradient(
                colors: op == .divide || op == .multiply
                    ? [Color(red: 1.0, green: 0.58, blue: 0.35), Color(red: 0.98, green: 0.28, blue: 0.46)]
                    : [Color(red: 1.0, green: 0.74, blue: 0.3), Color(red: 0.99, green: 0.42, blue: 0.31)],
                startPoint: .topLeading,
                endPoint: .bottomTrailing
            )
        case .equals:
            return LinearGradient(
                colors: [
                    Color(red: 0.64, green: 0.55, blue: 1.0),
                    Color(red: 0.34, green: 0.79, blue: 0.98)
                ],
                startPoint: .topLeading,
                endPoint: .bottomTrailing
            )
        case .clear, .toggleSign, .percent:
            return LinearGradient(
                colors: [
                    Color(red: 0.27, green: 0.33, blue: 0.54),
                    Color(red: 0.19, green: 0.25, blue: 0.41)
                ],
                startPoint: .topLeading,
                endPoint: .bottomTrailing
            )
        case .digit, .decimal:
            return LinearGradient(
                colors: [
                    Color(red: 0.29, green: 0.55, blue: 0.99),
                    Color(red: 0.56, green: 0.35, blue: 0.99)
                ],
                startPoint: .topLeading,
                endPoint: .bottomTrailing
            )
        }
    }

    private var shadowColor: Color {
        switch type {
        case .clear, .toggleSign, .percent:
            return Color.white.opacity(0.08)
        case .digit, .decimal, .operation, .equals:
            return Color.white.opacity(0.12)
        }
    }
}

enum CalculatorButtonType: Hashable {
    case digit(String)
    case decimal
    case operation(CalculatorOperation)
    case equals
    case clear
    case toggleSign
    case percent

    var title: String {
        switch self {
        case .digit(let value):
            return value
        case .decimal:
            return "."
        case .operation(let operation):
            return operation.rawValue
        case .equals:
            return "="
        case .clear:
            return "AC"
        case .toggleSign:
            return "±"
        case .percent:
            return "%"
        }
    }

    var accessibilityIdentifier: String {
        switch self {
        case .digit(let value):
            return "button-\(value)"
        case .decimal:
            return "button-decimal"
        case .operation(let operation):
            return "button-\(operation.rawValue)"
        case .equals:
            return "button-equals"
        case .clear:
            return "button-clear"
        case .toggleSign:
            return "button-toggleSign"
        case .percent:
            return "button-percent"
        }
    }

    var gridSpan: Int {
        switch self {
        case .digit(let value) where value == "0":
            return 2
        default:
            return 1
        }
    }
}

enum CalculatorOperation: String, Hashable {
    case add = "+"
    case subtract = "−"
    case multiply = "×"
    case divide = "÷"
}

private struct LayoutMetrics {
    let baseWidth: CGFloat = 360
    let topPadding: CGFloat = 36
    let bottomPadding: CGFloat = 32
    let horizontalPadding: CGFloat = 28
    let verticalSpacing: CGFloat = 20
    let buttonSpacing: CGFloat = 16
    let displayHeight: CGFloat = 168
    let displayHorizontalPadding: CGFloat = 32
    let displayVerticalPadding: CGFloat = 28
    let displayContentSpacing: CGFloat = 12
    let displayCornerRadius: CGFloat = 30
    let buttonRows: Int
    let buttonColumns: Int

    init(rows: Int, columns: Int) {
        self.buttonRows = rows
        self.buttonColumns = columns
    }

    var buttonSize: CGFloat {
        let totalSpacing = buttonSpacing * CGFloat(buttonColumns - 1)
        let usableWidth = baseWidth - (horizontalPadding * 2) - totalSpacing
        return usableWidth / CGFloat(buttonColumns)
    }

    var buttonAreaHeight: CGFloat {
        (CGFloat(buttonRows) * buttonSize) + (CGFloat(buttonRows - 1) * buttonSpacing)
    }

    var defaultWindowSize: CGSize {
        CGSize(
            width: baseWidth,
            height: topPadding + displayHeight + verticalSpacing + buttonAreaHeight + bottomPadding
        )
    }
}
