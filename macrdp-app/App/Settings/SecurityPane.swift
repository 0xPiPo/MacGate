import MacRDPKit
import SwiftUI

struct SecurityPane: View {
    @Environment(HelperClient.self) private var client
    @State private var port: Int?
    @State private var bindAddress = ""

    private static let idleChoices = [0, 300, 900, 1800, 3600, 7200, 14400]

    var body: some View {
        Form {
            Section {
                EnrollmentForm()
            } header: {
                Text("Mac account")
            } footer: {
                Text("Remote Desktop clients sign in with this Mac account. macOS checks the password; macrdp keeps only a derived hash in your Keychain.")
            }

            Section {
                LabeledContent("SHA-256") {
                    CopyChip(value: client.auth?.fingerprint ?? "Not available yet", lines: 2)
                        .frame(maxWidth: 360)
                }
            } header: {
                Text("Certificate")
            } footer: {
                Text("When a client connects for the first time, check that it shows this fingerprint.")
            }

            Section {
                Toggle("Lock this Mac when a session ends", isOn: client.binding("lock_on_disconnect", default: false))
                Toggle(isOn: client.binding("auto_unlock", default: false)) {
                    Text("Unlock automatically")
                    Text("When you connect to a locked Mac, macrdp types the password you signed in with into the lock screen.")
                }
            } header: {
                Text("Lock screen")
            } footer: {
                Text("The password is kept in memory for at most 10 seconds and never saved. Anyone who can sign in could type it themselves, so this only saves retyping. Takes effect after the server restarts.")
            }

            Section("Network") {
                TextField("Port", value: $port, format: .number.grouping(.never))
                    .onSubmit(commitPort)
                TextField("Listen address", text: $bindAddress, prompt: Text("0.0.0.0"))
                    .onSubmit(commitBindAddress)
                Picker("Disconnect when idle for", selection: client.binding("idle_timeout_secs", default: 1800)) {
                    let current = client.config["idle_timeout_secs"]?.intValue ?? 1800
                    ForEach(Array(Set(Self.idleChoices + [current])).sorted(), id: \.self) { seconds in
                        Text(seconds == 0 ? "Never" : Duration.seconds(seconds).formatted(.units(allowed: [.hours, .minutes], width: .wide)))
                            .tag(seconds)
                    }
                }
            }
        }
        .onAppear(perform: loadDrafts)
        .onChange(of: client.config) { loadDrafts() }
    }

    private func loadDrafts() {
        port = client.config["port"]?.intValue ?? 3389
        bindAddress = client.config["bind_address"]?.stringValue ?? "0.0.0.0"
    }

    private func commitPort() {
        guard let port, (1024...65535).contains(port) else {
            client.commandError = "The port must be between 1024 and 65535."
            return loadDrafts()
        }
        client.perform { try await client.setConfig("port", .int(port)) }
    }

    private func commitBindAddress() {
        let address = bindAddress.trimmingCharacters(in: .whitespaces)
        client.perform { try await client.setConfig("bind_address", .string(address.isEmpty ? "0.0.0.0" : address)) }
    }
}

/// Account status plus the password field. The field is cleared the moment the password is sent.
struct EnrollmentForm: View {
    @Environment(HelperClient.self) private var client
    @State private var password = ""
    @State private var busy = false
    @State private var error: String?
    @State private var justEnrolled = false
    @FocusState private var focused: Bool

    var body: some View {
        let auth = client.auth
        LabeledContent("Account", value: auth?.user ?? NSUserName())
        LabeledContent("Status") {
            Label(statusText(auth), systemImage: auth?.isReady == true ? "checkmark.circle.fill" : "exclamationmark.circle.fill")
                .foregroundStyle(auth?.isReady == true ? .green : .orange)
                .symbolEffect(.bounce, value: justEnrolled)
                .contentTransition(.symbolEffect(.replace))
        }
        HStack {
            SecureField("Mac password", text: $password, prompt: Text("Password for \(auth?.user ?? NSUserName())"))
                .focused($focused)
                .onSubmit(enroll)
                .disabled(busy)
            Button(auth?.enrolled == true ? "Re-enroll" : "Enroll", action: enroll)
                .disabled(password.isEmpty || busy)
            if busy { ProgressView().controlSize(.small) }
        }
        if let error {
            Text(error)
                .font(.callout)
                .foregroundStyle(.red)
        }
    }

    private func statusText(_ auth: AuthInfo?) -> String {
        guard let auth else { return "Checking…" }
        if auth.reenrollNeeded == true { return "Password changed: enroll again" }
        return auth.enrolled == true ? "Enrolled" : "Not enrolled"
    }

    private func enroll() {
        guard !password.isEmpty, !busy else { return }
        let secret = password
        password = ""
        busy = true
        error = nil
        Task {
            do {
                try await client.enroll(password: secret)
                justEnrolled.toggle()
            } catch {
                self.error = error.localizedDescription
                focused = true
            }
            busy = false
        }
    }
}

#Preview {
    SecurityPane()
        .formStyle(.grouped)
        .environment(HelperClient.preview(auth: AuthInfo(user: "pipo", enrolled: true, reenrollNeeded: true,
                                                         fingerprint: FakeTransport.fingerprint)))
}
