import SwiftUI

/// Curtain / Mirror / Extend as a segmented control: the selection pill slides between
/// segments and the chosen symbol gives a small bounce.
struct DisplayModePicker: View {
    @Binding var selection: DisplayMode

    @Namespace private var pill
    @State private var bounces: [DisplayMode: Int] = [:]
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 2) {
                ForEach(DisplayMode.allCases) { mode in
                    segment(mode)
                }
            }
            .padding(3)
            .background(.fill.tertiary, in: .rect(cornerRadius: 12))

            Text(selection.blurb)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .id(selection)
                .transition(.opacity)
        }
        .accessibilityRepresentation {
            Picker("Display mode", selection: $selection) {
                ForEach(DisplayMode.allCases) { Text($0.title).tag($0) }
            }
            .accessibilityHint(selection.blurb)
        }
    }

    private func segment(_ mode: DisplayMode) -> some View {
        let selected = selection == mode
        return Button {
            bounces[mode, default: 0] += 1
            withAnimation(reduceMotion ? .easeOut(duration: 0.15) : .snappy(duration: 0.28)) {
                selection = mode
            }
        } label: {
            VStack(spacing: 3) {
                Image(systemName: mode.symbol)
                    .font(.system(size: 15, weight: .medium))
                    .symbolEffect(.bounce.down, value: bounces[mode, default: 0])
                    .symbolEffectsRemoved(reduceMotion)
                    .frame(height: 18)
                Text(mode.title)
                    .font(.caption.weight(selected ? .semibold : .regular))
            }
            .frame(maxWidth: .infinity)
            .padding(.vertical, 7)
            .foregroundStyle(selected ? AnyShapeStyle(.tint) : AnyShapeStyle(.secondary))
            .background {
                if selected {
                    RoundedRectangle(cornerRadius: 9)
                        .fill(.background)
                        .shadow(color: .black.opacity(0.12), radius: 2, y: 1)
                        .matchedGeometryEffect(id: "pill", in: pill)
                }
            }
            .contentShape(.rect(cornerRadius: 9))
        }
        .buttonStyle(PressableButtonStyle())
        .help(mode.blurb)
    }
}

#Preview {
    @Previewable @State var mode = DisplayMode.curtain
    DisplayModePicker(selection: $mode)
        .padding()
        .frame(width: 340)
}
