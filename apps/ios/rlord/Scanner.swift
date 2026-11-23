// The native QR scanner. The pairing code goes from here straight to the
// engine (`Engine.scanned`); it never passes through the WebView. Only a
// string starting `RLORD1:` is taken. Debug builds also take one pasted
// (simulators have no camera).

import AVFoundation
import SwiftUI
import UIKit

/// The prefix of every rlord pairing code.
let pairingPrefix = "RLORD1:"

struct ScannerSheet: View {
    let onCode: (String) -> Void
    let onCancel: () -> Void
    @State private var denied = false
    #if DEBUG
    @State private var pasted = ""
    #endif

    var body: some View {
        NavigationStack {
            VStack(spacing: 16) {
                if denied {
                    Text("rlord needs the camera to scan the pairing code. Allow it in Settings, then try again.")
                        .multilineTextAlignment(.center)
                        .padding()
                } else {
                    CameraView(onCode: onCode, onDenied: { denied = true })
                        .aspectRatio(1, contentMode: .fit)
                        .clipShape(RoundedRectangle(cornerRadius: 12))
                        .padding()
                    Text("Scan the code your computer shows (`rlord relay pair`, or Devices in rlord).")
                        .font(.footnote)
                        .multilineTextAlignment(.center)
                        .padding(.horizontal)
                }
                #if DEBUG
                HStack {
                    TextField("Paste a pairing code (debug)", text: $pasted)
                        .textFieldStyle(.roundedBorder)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                        .accessibilityIdentifier("paste-qr")
                    Button("Use") {
                        if pasted.hasPrefix(pairingPrefix) { onCode(pasted) }
                    }
                }
                .padding(.horizontal)
                #endif
                Spacer()
            }
            .navigationTitle("Pair with a computer")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel", action: onCancel) }
            }
        }
    }
}

/// The camera preview and QR detection.
struct CameraView: UIViewControllerRepresentable {
    let onCode: (String) -> Void
    let onDenied: () -> Void

    func makeUIViewController(context: Context) -> CameraController {
        let c = CameraController()
        c.onCode = onCode
        c.onDenied = onDenied
        return c
    }

    func updateUIViewController(_ c: CameraController, context: Context) {}
}

final class CameraController: UIViewController, AVCaptureMetadataOutputObjectsDelegate {
    var onCode: ((String) -> Void)?
    var onDenied: (() -> Void)?
    private let session = AVCaptureSession()
    private var preview: AVCaptureVideoPreviewLayer?
    private var done = false

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black
        // The camera permission is asked here, with the reason from
        // Info.plist (NSCameraUsageDescription).
        AVCaptureDevice.requestAccess(for: .video) { granted in
            DispatchQueue.main.async {
                if granted { self.configure() } else { self.onDenied?() }
            }
        }
    }

    private func configure() {
        guard let device = AVCaptureDevice.default(for: .video),
              let input = try? AVCaptureDeviceInput(device: device),
              session.canAddInput(input)
        else { return }
        session.addInput(input)
        let output = AVCaptureMetadataOutput()
        guard session.canAddOutput(output) else { return }
        session.addOutput(output)
        output.setMetadataObjectsDelegate(self, queue: .main)
        output.metadataObjectTypes = [.qr]
        let layer = AVCaptureVideoPreviewLayer(session: session)
        layer.videoGravity = .resizeAspectFill
        layer.frame = view.bounds
        view.layer.addSublayer(layer)
        preview = layer
        DispatchQueue.global(qos: .userInitiated).async { self.session.startRunning() }
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        preview?.frame = view.bounds
    }

    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        if session.isRunning { session.stopRunning() }
    }

    func metadataOutput(
        _ output: AVCaptureMetadataOutput, didOutput objects: [AVMetadataObject],
        from connection: AVCaptureConnection
    ) {
        guard !done else { return }
        for case let code as AVMetadataMachineReadableCodeObject in objects {
            if let s = code.stringValue, s.hasPrefix(pairingPrefix) {
                done = true
                session.stopRunning()
                onCode?(s)
                return
            }
        }
    }
}
