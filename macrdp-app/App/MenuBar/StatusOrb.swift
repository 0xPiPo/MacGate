import MacRDPKit
import SwiftUI

/// A glowing dot that says what the server is doing. Listening breathes slowly, connected sends
/// out a pulse ring, the rest hold still. The clock only runs while `isLive` (popover on screen)
/// and Reduce Motion is off, so a closed popover costs nothing.
struct StatusOrb: View {
    let phase: ServerPhase
    var isLive = true
    var size: CGFloat = 40

    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var moves: Bool {
        isLive && !reduceMotion && [.starting, .listening, .connected].contains(phase)
    }

    var body: some View {
        TimelineView(.animation(minimumInterval: 1.0 / 30, paused: !moves)) { context in
            let t = moves ? context.date.timeIntervalSinceReferenceDate : 0
            orb(at: t)
        }
        .frame(width: size, height: size)
        .animation(.smooth(duration: 0.4), value: phase)
        .accessibilityHidden(true)
    }

    private func orb(at t: TimeInterval) -> some View {
        let tint = phase.tint
        // 0…1 breath; a 3.2 s cycle for listening, quicker while starting.
        let period = phase == .starting ? 1.2 : 3.2
        let breath = phase == .connected || !moves ? 0.6 : 0.5 + 0.5 * sin(2 * .pi * t / period)
        // Connected: a ring that leaves the orb every 1.8 s and fades as it grows.
        let pulse = phase == .connected && moves ? (t.truncatingRemainder(dividingBy: 1.8)) / 1.8 : 1
        let glows = phase != .stopped

        return ZStack {
            if phase == .connected {
                Circle()
                    .stroke(tint, lineWidth: 2)
                    .scaleEffect(moves ? 0.6 + 0.6 * pulse : 1)
                    .opacity(moves ? 0.7 * (1 - pulse) : 0.35)
            }
            Circle()
                .fill(tint.gradient)
                .overlay {
                    // Specular highlight: light catching the top of a glass bead.
                    Ellipse()
                        .fill(.white.opacity(0.35))
                        .frame(width: size * 0.24, height: size * 0.1)
                        .blur(radius: 1)
                        .offset(y: -size * 0.16)
                }
                .frame(width: size * 0.56, height: size * 0.56)
                .scaleEffect(0.95 + 0.05 * breath)
                .shadow(color: glows ? tint.opacity(0.25 + 0.45 * breath) : .clear,
                        radius: glows ? 3 + 7 * breath : 0)
        }
    }
}

#Preview("All phases") {
    HStack(spacing: 20) {
        ForEach([ServerPhase.stopped, .starting, .listening, .connected, .error], id: \.self) { phase in
            VStack {
                StatusOrb(phase: phase)
                Text(phase.rawValue).font(.caption)
            }
        }
    }
    .padding(30)
}
