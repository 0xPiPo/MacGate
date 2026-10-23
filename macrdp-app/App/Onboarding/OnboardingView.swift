import MacRDPKit
import SwiftUI

enum OnboardingStep: Int, CaseIterable, Identifiable {
    case welcome, permissions, password, connect

    var id: Self { self }

    var symbol: String {
        switch self {
        case .welcome: "display"
        case .permissions: "hand.raised.fill"
        case .password: "key.fill"
        case .connect: "iphone.gen3.radiowaves.left.and.right"
        }
    }
}

/// First run: welcome, permissions, Mac password, then where to connect. The hero morphs
/// from the app icon into each step's symbol, and the page indicator slides between dots.
struct OnboardingView: View {
    @Environment(HelperClient.self) private var client
    @Environment(\.dismissWindow) private var dismissWindow
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @AppStorage("onboardingDone") private var onboardingDone = false

    @State private var step = OnboardingStep.welcome
    @State private var forward = true
    @Namespace private var hero

    var body: some View {
        VStack(spacing: 0) {
            heroView
                .frame(height: 128)
                .padding(.top, 36)

            ZStack {
                stepContent
                    .id(step)
                    .transition(stepTransition)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            .padding(.horizontal, 48)
            .padding(.top, 12)

            controls
                .padding(20)
        }
        .frame(width: 560, height: 520)
        .onAppear { NSApp.activate() }
    }

    // MARK: Pieces

    @ViewBuilder private var heroView: some View {
        if step == .welcome {
            Image(nsImage: NSApp.applicationIconImage)
                .resizable()
                .frame(width: 112, height: 112)
                .matchedGeometryEffect(id: "hero", in: hero)
                .accessibilityHidden(true)
        } else {
            Image(systemName: step.symbol)
                .font(.system(size: 30, weight: .medium))
                .foregroundStyle(.tint)
                .contentTransition(.symbolEffect(.replace))
                .frame(width: 72, height: 72)
                .glassEffect(.regular.tint(.accentColor.opacity(0.15)), in: .circle)
                .matchedGeometryEffect(id: "hero", in: hero)
                .accessibilityHidden(true)
        }
    }

    @ViewBuilder private var stepContent: some View {
        switch step {
        case .welcome:
            StepText(title: "Welcome to macrdp",
                     message: "Use this Mac from your phone, tablet or PC with any Remote Desktop app. Setup takes a minute.")
        case .permissions: PermissionsStep()
        case .password: PasswordStep()
        case .connect: ConnectStep()
        }
    }

    private var controls: some View {
        HStack {
            Button("Back") { go(to: step.rawValue - 1) }
                .opacity(step == .welcome ? 0 : 1)
                .disabled(step == .welcome)
            Spacer()
            PageDots(current: step)
            Spacer()
            Button(step == .connect ? "Done" : "Continue") {
                if step == .connect {
                    onboardingDone = true
                    client.startIfReady()
                    dismissWindow(id: "onboarding")
                } else {
                    go(to: step.rawValue + 1)
                }
            }
            .buttonStyle(.glassProminent)
            .keyboardShortcut(.defaultAction)
            .disabled(step == .permissions && client.permissions?.allGranted != true)
        }
        .controlSize(.large)
    }

    private var stepTransition: AnyTransition {
        guard !reduceMotion else { return .opacity }
        let from: Edge = forward ? .trailing : .leading
        let to: Edge = forward ? .leading : .trailing
        return .asymmetric(insertion: .move(edge: from).combined(with: .opacity),
                           removal: .move(edge: to).combined(with: .opacity))
    }

    private func go(to index: Int) {
        guard let next = OnboardingStep(rawValue: index) else { return }
        forward = next.rawValue > step.rawValue
        withAnimation(reduceMotion ? .easeOut(duration: 0.2) : .spring(duration: 0.45, bounce: 0.12)) {
            step = next
        }
    }
}

/// Four dots; the current one stretches into a pill that slides to the next.
private struct PageDots: View {
    let current: OnboardingStep
    @Namespace private var dots

    var body: some View {
        HStack(spacing: 8) {
            ForEach(OnboardingStep.allCases) { step in
                ZStack {
                    Circle().fill(.quaternary).frame(width: 7, height: 7)
                    if step == current {
                        Capsule()
                            .fill(.tint)
                            .frame(width: 20, height: 7)
                            .matchedGeometryEffect(id: "dot", in: dots)
                    }
                }
                .frame(width: step == current ? 20 : 7)
            }
        }
        .accessibilityElement()
        .accessibilityLabel("Step \(current.rawValue + 1) of \(OnboardingStep.allCases.count)")
    }
}

struct StepText: View {
    let title: String
    let message: String

    var body: some View {
        VStack(spacing: 10) {
            Text(title)
                .font(.largeTitle.weight(.semibold))
                .multilineTextAlignment(.center)
            Text(message)
                .font(.title3)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
        }
        .accessibilityElement(children: .combine)
    }
}

#Preview {
    OnboardingView()
        .environment(HelperClient.preview(
            status: ServerStatus(running: false, state: "stopped"),
            auth: AuthInfo(user: "pipo", enrolled: false),
            permissions: PermissionsInfo(screenCapture: true, accessibility: false)))
}
