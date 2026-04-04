import SwiftUI

@MainActor
final class CalculatorViewModel: ObservableObject {
    @Published var display: String = "0"
    @Published var secondaryDisplay: String = " "

    private var accumulator: Double = 0
    private var pendingOperation: CalculatorOperation?
    private var repeatOperation: CalculatorOperation?
    private var repeatOperand: Double?
    private var isTypingNumber: Bool = false
    private var currentInput: String = "0"
    private var lastButtonWasEquals: Bool = false

    func inputDigit(_ digit: String) {
        guard digit.range(of: #"^[0-9]$"#, options: .regularExpression) != nil else { return }

        if display == "Error" {
            reset()
        }

        if lastButtonWasEquals && pendingOperation == nil {
            reset()
        }

        if !isTypingNumber {
            currentInput = digit
            isTypingNumber = true
        } else {
            if currentInput == "0" {
                currentInput = digit
            } else if currentInput == "-0" {
                currentInput = "-\(digit)"
            } else if currentInput.count < 16 {
                currentInput.append(digit)
            }
        }

        display = currentInput
        lastButtonWasEquals = false
        secondaryDisplay = pendingOperation.map { "\(formatNumber(accumulator)) \($0.rawValue)" } ?? secondaryDisplay
    }

    func inputDecimal() {
        if display == "Error" {
            reset()
        }

        if lastButtonWasEquals && pendingOperation == nil {
            reset()
        }

        if !isTypingNumber {
            currentInput = "0."
            isTypingNumber = true
        } else if !currentInput.contains(".") {
            currentInput.append(".")
        }

        display = currentInput
        lastButtonWasEquals = false
    }

    func setOperation(_ operation: CalculatorOperation) {
        let value = currentValue

        if isTypingNumber {
            if let existing = pendingOperation {
                accumulator = perform(existing, lhs: accumulator, rhs: value)
            } else {
                accumulator = value
            }
            showResult(accumulator)
            isTypingNumber = false
        } else if lastButtonWasEquals {
            accumulator = currentDisplayValue
        } else if pendingOperation == nil {
            accumulator = value
        }

        pendingOperation = operation
        repeatOperation = nil
        repeatOperand = nil
        lastButtonWasEquals = false
        secondaryDisplay = "\(formatNumber(accumulator)) \(operation.rawValue)"
    }

    func evaluate() {
        let value = currentValue
        var operand = value

        if let pendingOperation {
            if !isTypingNumber, lastButtonWasEquals, let repeatOperand {
                operand = repeatOperand
            } else {
                repeatOperand = value
            }

            let result = perform(pendingOperation, lhs: accumulator, rhs: operand)
            showResult(result)
            repeatOperation = pendingOperation
            self.pendingOperation = nil
            secondaryDisplay = "\(formatNumber(accumulator))"
        } else if lastButtonWasEquals, let repeatOperation, let repeatOperand {
            operand = repeatOperand
            let lhs = currentDisplayValue
            let result = perform(repeatOperation, lhs: lhs, rhs: operand)
            showResult(result)
            secondaryDisplay = "\(formatNumber(accumulator))"
        }

        lastButtonWasEquals = true
        isTypingNumber = false
    }

    func reset() {
        display = "0"
        secondaryDisplay = " "
        accumulator = 0
        pendingOperation = nil
        repeatOperation = nil
        repeatOperand = nil
        isTypingNumber = false
        currentInput = "0"
        lastButtonWasEquals = false
    }

    func toggleSign() {
        if display == "Error" {
            reset()
            return
        }

        if isTypingNumber {
            if currentInput.hasPrefix("-") {
                currentInput.removeFirst()
            } else {
                currentInput = "-\(currentInput)"
            }
            display = currentInput
        } else {
            let value = -currentDisplayValue
            showResult(value)
        }
    }

    func percent() {
        if display == "Error" {
            reset()
            return
        }

        var value = currentValue

        if pendingOperation != nil {
            value = (accumulator * value) / 100
        } else {
            value = value / 100
        }

        currentInput = sanitizedNumericString(for: value)
        display = formatNumber(value)
        if pendingOperation == nil {
            accumulator = value
        }

        isTypingNumber = true
        lastButtonWasEquals = false
    }

    // MARK: - Helpers

    private var currentValue: Double {
        if isTypingNumber {
            return Double(currentInput) ?? 0
        } else {
            return currentDisplayValue
        }
    }

    private var currentDisplayValue: Double {
        let sanitized = display.replacingOccurrences(of: ",", with: "")
        return Double(sanitized) ?? 0
    }

    private func perform(_ operation: CalculatorOperation, lhs: Double, rhs: Double) -> Double {
        switch operation {
        case .add:
            return lhs + rhs
        case .subtract:
            return lhs - rhs
        case .multiply:
            return lhs * rhs
        case .divide:
            return rhs == 0 ? .nan : lhs / rhs
        }
    }

    private func showResult(_ value: Double) {
        if value.isNaN || value.isInfinite {
            display = "Error"
            secondaryDisplay = " "
            pendingOperation = nil
            repeatOperation = nil
            repeatOperand = nil
            accumulator = 0
            currentInput = "0"
            isTypingNumber = false
            lastButtonWasEquals = false
            return
        }

        accumulator = value
        display = formatNumber(value)
        currentInput = sanitizedNumericString(for: value)
    }

    private func sanitizedNumericString(for value: Double) -> String {
        if value.isNaN || value.isInfinite {
            return "0"
        }

        if value == floor(value) {
            return String(format: "%.0f", value)
        } else {
            let formatter = NumberFormatter()
            formatter.maximumFractionDigits = 8
            formatter.minimumFractionDigits = 0
            formatter.decimalSeparator = "."
            formatter.groupingSeparator = ""
            formatter.numberStyle = .decimal
            return formatter.string(from: NSNumber(value: value)) ?? "\(value)"
        }
    }

    private func formatNumber(_ number: Double) -> String {
        if number.isNaN || number.isInfinite {
            return "Error"
        }

        let formatter = NumberFormatter()
        formatter.maximumFractionDigits = 8
        formatter.minimumFractionDigits = 0
        formatter.numberStyle = .decimal
        formatter.locale = Locale(identifier: "en_US")

        return formatter.string(from: NSNumber(value: number)) ?? "\(number)"
    }
}
