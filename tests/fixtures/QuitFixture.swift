import AppKit
import Foundation

// A self-contained test app. It opens no user files and writes only its event log.
// Its safety timer ends the fixture itself; the controller never force-terminates it.
guard CommandLine.arguments.count == 3 else { exit(64) }
let mode = CommandLine.arguments[1]
let logURL = URL(fileURLWithPath: CommandLine.arguments[2])
guard ["immediate", "cancel", "later", "unsaved", "cancel-later", "document-cancel", "document-discard", "foreground"].contains(mode) else { exit(64) }

func event(_ name: String, _ extra: [String: Any] = [:]) {
    var row: [String: Any] = [
        "event": name, "mode": mode, "pid": ProcessInfo.processInfo.processIdentifier,
        "timestamp_unix_seconds": Date().timeIntervalSince1970,
    ]
    extra.forEach { row[$0.key] = $0.value }
    do {
        let data = try JSONSerialization.data(withJSONObject: row, options: [.sortedKeys])
        if !FileManager.default.fileExists(atPath: logURL.path) {
            guard FileManager.default.createFile(atPath: logURL.path, contents: nil) else { exit(74) }
        }
        let handle = try FileHandle(forWritingTo: logURL)
        try handle.seekToEnd()
        try handle.write(contentsOf: data + Data([10]))
        try handle.close()
    } catch { exit(74) }
}

func schedule(_ delay: TimeInterval, _ action: @escaping () -> Void) {
    let timer = Timer(timeInterval: delay, repeats: false) { _ in action() }
    RunLoop.main.add(timer, forMode: .common)
    RunLoop.main.add(timer, forMode: .modalPanel)
}

final class InMemoryDocument: NSDocument {
    override func data(ofType typeName: String) throws -> Data {
        Data("Bree fixture unsaved text. No user content.\n".utf8)
    }

    override func makeWindowControllers() {
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 420, height: 150),
                              styleMask: [.titled, .closable], backing: .buffered, defer: false)
        window.title = "Bree disposable \(mode) document"
        let label = NSTextField(labelWithString: "Disposable unsaved fixture text. No user files.")
        label.frame = NSRect(x: 20, y: 65, width: 380, height: 25)
        window.contentView?.addSubview(label)
        addWindowController(NSWindowController(window: window))
    }

    override func canClose(withDelegate delegate: Any, shouldClose shouldCloseSelector: Selector?, contextInfo: UnsafeMutableRawPointer?) {
        event("document_can_close", ["is_document_edited": isDocumentEdited])
        super.canClose(withDelegate: delegate, shouldClose: shouldCloseSelector, contextInfo: contextInfo)
    }
}

func buttons(in view: NSView) -> [NSButton] {
    let current = (view as? NSButton).map { [$0] } ?? []
    return current + view.subviews.flatMap { buttons(in: $0) }
}

final class ProbeDocumentController: NSDocumentController {
    private var reviewTimer: Timer?
    private var buttonClicked = false
    private var lastTitles: [String] = []

    override func reviewUnsavedDocuments(withAlertTitle title: String?, cancellable: Bool, delegate: Any?, didReviewAllSelector: Selector?, contextInfo: UnsafeMutableRawPointer?) {
        event("document_review_started", ["dirty_documents": documents.filter { $0.isDocumentEdited }.count,
                                           "cancellable": cancellable])
        if mode == "document-cancel" || mode == "document-discard" {
            let timer = Timer(timeInterval: 0.1, repeats: true) { [weak self] timer in
                guard let self, !self.buttonClicked else { timer.invalidate(); return }
                let visible = NSApplication.shared.windows.filter { $0.isVisible }
                let candidates = visible.flatMap { $0.contentView.map { buttons(in: $0) } ?? [] }
                let titles = candidates.map { $0.title }.sorted()
                if titles != self.lastTitles {
                    event("document_review_buttons", ["titles": titles])
                    self.lastTitles = titles
                }
                let names = mode == "document-cancel"
                    ? ["cancel", "取消"]
                    : ["discard changes", "don't save", "不保存", "丢弃更改"]
                guard let button = candidates.first(where: { names.contains($0.title.lowercased()) && $0.isEnabled }) else { return }
                self.buttonClicked = true
                timer.invalidate()
                event("document_review_button_clicked", ["title": button.title, "scripted_fixture_input": true])
                // Only a button inside this fixture's own AppKit window is clicked.
                button.performClick(nil)
            }
            reviewTimer = timer
            RunLoop.main.add(timer, forMode: .common)
            RunLoop.main.add(timer, forMode: .modalPanel)
        }
        // Keep AppKit's actual review and callback implementation; no fabricated callback.
        super.reviewUnsavedDocuments(withAlertTitle: title, cancellable: cancellable,
                                     delegate: delegate, didReviewAllSelector: didReviewAllSelector, contextInfo: contextInfo)
        event("document_review_returned", ["dirty_documents": documents.filter { $0.isDocumentEdited }.count])
    }
}

final class QuitFixtureDelegate: NSObject, NSApplicationDelegate {
    private var documents: [InMemoryDocument] = []
    private var foregroundWindow: NSWindow?

    func applicationDidFinishLaunching(_ notification: Notification) {
        if ["unsaved", "document-cancel", "document-discard"].contains(mode) {
            for _ in 0..<(mode == "unsaved" ? 1 : 2) {
                let newDocument = InMemoryDocument()
                newDocument.fileType = "public.plain-text"
                newDocument.updateChangeCount(.changeDone)
                NSDocumentController.shared.addDocument(newDocument)
                documents.append(newDocument)
                if mode != "unsaved" {
                    newDocument.makeWindowControllers()
                    newDocument.showWindows()
                }
                event("unsaved_document_created", ["is_document_edited": newDocument.isDocumentEdited])
            }
        }
        event("document_controller_identity", ["shared_is_probe": documentController == nil ? false : NSDocumentController.shared is ProbeDocumentController])
        if mode == "document-cancel" || mode == "document-discard" {
            // This manually launched fixture installs its own Quit AppleEvent adapter.
            // The adapter enters NSApplication's standard termination pipeline; it does
            // not fake NSDocumentController's review, callback, or the alert's buttons.
            NSAppleEventManager.shared().setEventHandler(self,
                andSelector: #selector(handleQuitAppleEvent(_:withReplyEvent:)),
                forEventClass: 0x61657674, andEventID: 0x71756974)
            event("fixture_quit_adapter_installed")
        }
        event("launched", ["run_loop_mode": RunLoop.current.currentMode?.rawValue ?? "none", "main_thread": Thread.isMainThread])
        let safetyTimer = Timer(timeInterval: 23, repeats: false) { _ in
            event("safety_timer_natural_exit", ["unsaved_document": self.documents.contains { $0.isDocumentEdited }])
            // This exits only this disposable fixture, after the observation window.
            exit(0)
        }
        RunLoop.main.add(safetyTimer, forMode: .common)
        RunLoop.main.add(safetyTimer, forMode: .modalPanel)
        if mode == "foreground" {
            schedule(0.6) {
                let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 350, height: 120),
                                      styleMask: [.titled], backing: .buffered, defer: false)
                window.title = "Bree foreground transition self-test"
                self.foregroundWindow = window
                window.makeKeyAndOrderFront(nil)
                NSApplication.shared.activate(ignoringOtherApps: true)
                event("foreground_activation_requested")
            }
            schedule(5) {
                NSApplication.shared.hide(nil)
                event("fixture_self_hidden")
            }
        }
    }

    func applicationShouldOpenUntitledFile(_ sender: NSApplication) -> Bool {
        event("automatic_untitled_file_disabled")
        return false
    }

    @objc func handleQuitAppleEvent(_ eventDescriptor: NSAppleEventDescriptor, withReplyEvent replyEvent: NSAppleEventDescriptor) {
        event("fixture_quit_apple_event_received", ["adapter": "explicit_self_fixture_handler"])
        NSApplication.shared.terminate(nil)
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        event("application_should_terminate")
        switch mode {
        case "cancel":
            event("reply_cancel")
            return .terminateCancel
        case "unsaved":
            let dirty = documents.contains { $0.isDocumentEdited }
            event("reply_cancel_unsaved", ["is_document_edited": dirty])
            return .terminateCancel
        case "later":
            event("reply_later")
            schedule(1) {
                event("reply_later_accept")
                sender.reply(toApplicationShouldTerminate: true)
            }
            return .terminateLater
        case "cancel-later":
            event("reply_later")
            schedule(1) {
                event("reply_later_cancel")
                sender.reply(toApplicationShouldTerminate: false)
            }
            return .terminateLater
        default:
            event("reply_now")
            return .terminateNow
        }
    }

    func applicationWillTerminate(_ notification: Notification) { event("will_terminate") }
}

// Apple's init contract makes the first document controller the shared instance.
let app = NSApplication.shared
let documentController: ProbeDocumentController? = ["unsaved", "document-cancel", "document-discard"].contains(mode)
    ? ProbeDocumentController() : nil
let delegate = QuitFixtureDelegate()
app.setActivationPolicy(mode == "foreground" ? .regular : .accessory)
app.delegate = delegate
app.run()
