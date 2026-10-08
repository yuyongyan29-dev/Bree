import AppKit
import Foundation

// A self-contained test app. P2 opens only dedicated files under .artifacts/p2.
// Recovery is a separately logged self-exit, never evidence of a successful quit.
guard [3, 4].contains(CommandLine.arguments.count) else { exit(64) }
let mode = CommandLine.arguments[1]
let logURL = URL(fileURLWithPath: CommandLine.arguments[2])
let fileModes = ["file-save", "file-fail", "file-panel-cancel", "file-wait", "file-save-plain"]
guard (["immediate", "cancel", "later", "unsaved", "cancel-later", "document-cancel", "document-discard", "foreground"] + fileModes).contains(mode) else { exit(64) }
let p2Root: URL? = CommandLine.arguments.count == 4
    ? URL(fileURLWithPath: CommandLine.arguments[3]).resolvingSymlinksInPath() : nil
if let root = p2Root {
    guard root.path.contains("/.artifacts/p2/"),
          root == logURL.deletingLastPathComponent().resolvingSymlinksInPath() else { exit(64) }
}
if fileModes.contains(mode) && p2Root == nil { exit(64) }

func event(_ name: String, _ extra: [String: Any] = [:]) {
    var row: [String: Any] = [
        "event": name, "mode": mode, "pid": ProcessInfo.processInfo.processIdentifier,
        "process_group": getpgrp(), "parent_pid": getppid(),
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

// Uses NSDocument's actual read, safe write, save, and canClose implementations.
// Only the content serialization and scripted input inside this app are supplied.
final class FileDocument: NSDocument {
    var text = ""
    override class var autosavesInPlace: Bool { false }
    override func read(from data: Data, ofType typeName: String) throws {
        guard let value = String(data: data, encoding: .utf8) else {
            throw CocoaError(.fileReadCorruptFile)
        }
        text = value
        event("file_read", ["content": text])
    }
    override func data(ofType typeName: String) throws -> Data { Data(text.utf8) }
    override func makeWindowControllers() {
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 460, height: 160),
                              styleMask: [.titled, .closable], backing: .buffered, defer: false)
        window.title = "Bree dedicated P2 \(mode) document"
        let label = NSTextField(labelWithString: text)
        label.frame = NSRect(x: 20, y: 60, width: 420, height: 70)
        window.contentView?.addSubview(label)
        addWindowController(NSWindowController(window: window))
    }
    override func canClose(withDelegate delegate: Any, shouldClose selector: Selector?, contextInfo: UnsafeMutableRawPointer?) {
        event("file_can_close", ["dirty": isDocumentEdited, "memory_content": text])
        super.canClose(withDelegate: delegate, shouldClose: selector, contextInfo: contextInfo)
    }
    override func save(to url: URL, ofType typeName: String, for operation: NSDocument.SaveOperationType,
                       completionHandler: @escaping (Error?) -> Void) {
        event("file_save_started", ["destination": url.path, "memory_content": text])
        super.save(to: url, ofType: typeName, for: operation) { error in
            let nsError = error as NSError?
            event("file_save_completed", ["success": error == nil, "dirty": self.isDocumentEdited,
                                          "error_domain": nsError?.domain ?? "none", "error_code": nsError?.code ?? 0,
                                          "disk_content": (try? String(contentsOf: url, encoding: .utf8)) ?? "unreadable"])
            completionHandler(error)
        }
    }
    override func prepareSavePanel(_ savePanel: NSSavePanel) -> Bool {
        event("file_save_panel_prepared")
        savePanel.directoryURL = p2Root?.appendingPathComponent("documents")
        let timer = Timer(timeInterval: 0.1, repeats: true) { timer in
            guard savePanel.isVisible, mode == "file-panel-cancel" else { return }
            timer.invalidate()
            event("file_save_panel_cancelled", ["scripted_fixture_input": true, "panel_visible": true])
            savePanel.cancel(nil)
        }
        RunLoop.main.add(timer, forMode: .common)
        RunLoop.main.add(timer, forMode: .modalPanel)
        return super.prepareSavePanel(savePanel)
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
        if mode == "document-cancel" || mode == "document-discard" || fileModes.contains(mode) {
            let timer = Timer(timeInterval: 0.1, repeats: true) { [weak self] timer in
                guard let self, !self.buttonClicked else { timer.invalidate(); return }
                let visible = NSApplication.shared.windows.filter { $0.isVisible }
                let candidates = visible.flatMap { $0.contentView.map { buttons(in: $0) } ?? [] }
                let titles = candidates.map { $0.title }.sorted()
                if titles != self.lastTitles {
                    event("document_review_buttons", ["titles": titles])
                    self.lastTitles = titles
                }
                if mode == "file-wait" { return } // no answer throughout the real observation window
                let names: [String]
                if mode == "document-cancel" { names = ["cancel", "取消"] }
                else if mode == "document-discard" { names = ["discard changes", "don't save", "不保存", "丢弃更改"] }
                else { names = ["save", "保存"] }
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
    private var fileDocument: FileDocument?
    private var foregroundWindow: NSWindow?

    func applicationDidFinishLaunching(_ notification: Notification) {
        if fileModes.contains(mode), let root = p2Root {
            do {
                let documentURL = root.appendingPathComponent("documents/original.txt")
                let document = try FileDocument(contentsOf: documentURL, ofType: "public.plain-text")
                document.text = "Bree P2 edited text.\n"
                document.updateChangeCount(.changeDone)
                if mode == "file-panel-cancel" { document.fileURL = nil }
                NSDocumentController.shared.addDocument(document)
                fileDocument = document
                document.makeWindowControllers()
                // Keep the initial target in the background; showing windows can activate it.
                event("file_document_ready", ["original_path": documentURL.path, "dirty": document.isDocumentEdited,
                                              "memory_content": document.text, "untitled": document.fileURL == nil])
                if mode == "file-fail" {
                    try FileManager.default.setAttributes([.posixPermissions: 0o500],
                                                         ofItemAtPath: documentURL.deletingLastPathComponent().path)
                    event("file_destination_read_only", ["directory_mode": "0500", "real_filesystem_failure": true])
                }
            } catch { event("file_setup_failed", ["error": String(describing: error)]); exit(74) }
        }
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
        if mode == "document-cancel" || mode == "document-discard" || (p2Root != nil && mode != "file-save-plain") {
            // This manually launched fixture installs its own Quit AppleEvent adapter.
            // The adapter enters NSApplication's standard termination pipeline; it does
            // not fake NSDocumentController's review, callback, or the alert's buttons.
            NSAppleEventManager.shared().setEventHandler(self,
                andSelector: #selector(handleQuitAppleEvent(_:withReplyEvent:)),
                forEventClass: 0x61657674, andEventID: 0x71756974)
            event("fixture_quit_adapter_installed")
        }
        event("launched", ["run_loop_mode": RunLoop.current.currentMode?.rawValue ?? "none", "main_thread": Thread.isMainThread,
                           "regular": NSApplication.shared.activationPolicy() == .regular, "p2": p2Root != nil])
        if let root = p2Root {
            let recovery = Timer(timeInterval: 0.1, repeats: true) { _ in
                guard FileManager.default.fileExists(atPath: root.appendingPathComponent("reap").path) else { return }
                self.retentionCheckpoint("p2_cleanup_exit")
                try? FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: root.appendingPathComponent("documents").path)
                exit(0)
            }
            RunLoop.main.add(recovery, forMode: .common)
            RunLoop.main.add(recovery, forMode: .modalPanel)
            schedule(16) { self.retentionCheckpoint("p2_retention_checkpoint") }
        }
        let safetyTimer = Timer(timeInterval: p2Root == nil ? 23 : 60, repeats: false) { _ in
            self.retentionCheckpoint("safety_timer_natural_exit")
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
            schedule(p2Root == nil ? 5 : 40) {
                NSApplication.shared.hide(nil)
                event("fixture_self_hidden")
            }
        }
    }

    private func retentionCheckpoint(_ name: String) {
        if let document = fileDocument, let root = p2Root {
            try? document.text.write(to: root.appendingPathComponent("retained-edits.txt"), atomically: true, encoding: .utf8)
            event(name, ["dirty": document.isDocumentEdited, "memory_content": document.text, "unsaved_document": document.isDocumentEdited,
                         "recovery_only": name != "p2_retention_checkpoint"])
        } else {
            event(name, ["unsaved_document": documents.contains { $0.isDocumentEdited }, "recovery_only": true])
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
let documentController: ProbeDocumentController? = (["unsaved", "document-cancel", "document-discard"] + fileModes).contains(mode)
    ? ProbeDocumentController() : nil
let delegate = QuitFixtureDelegate()
app.setActivationPolicy(mode == "foreground" || p2Root != nil ? .regular : .accessory)
app.delegate = delegate
app.run()
