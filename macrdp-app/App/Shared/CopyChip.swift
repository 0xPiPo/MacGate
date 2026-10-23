import AppKit
import SwiftUI

/// A value you can click to copy; the icon turns into a checkmark for a moment.
struct CopyChip: View {
    let value: String
    var monospaced = true
    var lines = 1

    @State private var copied = false

    var body: some View {
        Button(action: copy) {
            HStack(spacing: 8) {
                Text(value)
                    .font(monospaced ? .callout.monospaced() : .callout)
                    .lineLimit(lines)
                    .truncationMode(.middle)
                    .fixedSize(horizontal: false, vertical: lines > 1)
                Spacer(minLength: 4)
                Image(systemName: copied ? "checkmark" : "doc.on.doc")
                    .contentTransition(.symbolEffect(.replace))
                    .foregroundStyle(copied ? AnyShapeStyle(.green) : AnyShapeStyle(.secondary))
                    .fontWeight(copied ? .bold : .regular)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .background(.fill.quaternary, in: .rect(cornerRadius: 9))
            .contentShape(.rect(cornerRadius: 9))
        }
        .buttonStyle(PressableButtonStyle())
        .help("Copy")
        .accessibilityLabel("Copy \(value)")
        .accessibilityValue(copied ? "Copied" : "")
        .task(id: copied) {
            guard copied else { return }
            try? await Task.sleep(for: .seconds(1.6))
            copied = false
        }
    }

    private func copy() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(value, forType: .string)
        withAnimation(.snappy(duration: 0.2)) { copied = true }
        AccessibilityNotification.Announcement("Copied").post()
    }
}

/// Shrinks a touch on press, so a click is felt at once.
struct PressableButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(configuration.isPressed ? 0.97 : 1)
            .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
    }
}

#Preview {
    VStack {
        CopyChip(value: "192.168.1.24:3389")
        CopyChip(value: "10.8.0.2:3389")
    }
    .padding()
    .frame(width: 300)
}
