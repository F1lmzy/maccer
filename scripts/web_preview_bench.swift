// Web preview cost harness for docs/web-preview-performance.md.
//
//   swiftc -O scripts/web_preview_bench.swift -o /tmp/web_preview_bench
//   /tmp/web_preview_bench cold      # first Google search, no warm-up
//   /tmp/web_preview_bench prewarm   # about:blank first, then Google twice
//   /tmp/web_preview_bench prewarm-hidden # warm while the view is hidden
//   /tmp/web_preview_bench js        # same page with JavaScript disabled
//   /tmp/web_preview_bench nojs-html # JavaScript-off engine with an HTML endpoint
//   /tmp/web_preview_bench nonpersistent
//   /tmp/web_preview_bench reopen    # hide/re-show with and without a reload
//   /tmp/web_preview_bench hiddencpu # idle CPU of a hidden page
//   /tmp/web_preview_bench kill      # release the view, then recreate it
//
// It mirrors crates/launcher/src/macos/web_preview.rs: a frame-zero WKWebView
// in a plain NSWindow, Safari user agent, and the same hide/release calls. RSS
// comes from `ps`, sampling only the WebKit XPC processes this run started.

import AppKit
import WebKit
import Darwin

// --- helpers -------------------------------------------------------------
func now() -> Double { Double(DispatchTime.now().uptimeNanoseconds) / 1e9 }
struct Proc { let pid: Int; let rss: Double; let name: String }
func wkProcs() -> [Proc] {
    let p = Process(); p.executableURL = URL(fileURLWithPath: "/bin/bash")
    p.arguments = ["-c", "ps -Ao pid,rss,comm | grep -i 'WebKit.*xpc' | grep -v grep"]
    let pipe = Pipe(); p.standardOutput = pipe; try? p.run(); p.waitUntilExit()
    let d = pipe.fileHandleForReading.readDataToEndOfFile()
    return (String(data: d, encoding: .utf8) ?? "").split(separator: "\n").compactMap { line in
        let f = line.split(separator: " ", omittingEmptySubsequences: true)
        guard f.count >= 3, let pid = Int(f[0]), let kb = Int(f[1]) else { return nil }
        return Proc(pid: pid, rss: Double(kb)/1024.0,
                    name: (f[2] as NSString).lastPathComponent.replacingOccurrences(of: "com.apple.WebKit.", with: ""))
    }
}
var baseline: Set<Int> = []
func ours() -> [Proc] { wkProcs().filter { !baseline.contains($0.pid) } }
func total(_ ps: [Proc]) -> Double { ps.reduce(0) { $0 + $1.rss } }
func hasWebContent() -> Bool { ours().contains { $0.name == "WebContent" } }
func namelist() -> String {
    let ps = ours()
    return "\(Int(total(ps)))MB [" + ps.map { "\($0.name)=\(Int($0.rss))" }.joined(separator: " ") + "]"
}

final class Nav: NSObject, WKNavigationDelegate {
    var commit: Double? = nil
    var finish: Double? = nil
    var failed: String? = nil
    func webView(_ w: WKWebView, didCommit n: WKNavigation!) { if commit == nil { commit = now() } }
    func webView(_ w: WKWebView, didFinish n: WKNavigation!) { if finish == nil { finish = now() } }
    func webView(_ w: WKWebView, didFail n: WKNavigation!, withError e: Error) { failed = e.localizedDescription }
    func webView(_ w: WKWebView, didFailProvisionalNavigation n: WKNavigation!, withError e: Error) { failed = e.localizedDescription }
}
func spin(_ s: Double) { RunLoop.main.run(until: Date().addingTimeInterval(s)) }
func waitUntil(_ limit: Double, _ done: () -> Bool) {
    let end = now() + limit
    while !done() && now() < end { spin(0.02) }
}

func makeWeb(js: Bool, persistent: Bool, ua: Bool) -> (WKWebView, Double) {
    let t = now()
    let cfg = WKWebViewConfiguration()
    if !persistent { cfg.websiteDataStore = WKWebsiteDataStore.nonPersistent() }
    if !js { cfg.defaultWebpagePreferences.allowsContentJavaScript = false }
    if ua {
        let v = (Bundle(path: "/Applications/Safari.app")?.infoDictionary?["CFBundleShortVersionString"] as? String) ?? "26.0"
        cfg.applicationNameForUserAgent = "Version/\(v) Safari/605.1.15"
    }
    let w = WKWebView(frame: NSRect(x: 0, y: 0, width: 720, height: 440), configuration: cfg)
    w.isHidden = false
    return (w, now() - t)
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 720, height: 520),
                      styleMask: [.titled], backing: .buffered, defer: false)
let content = NSView(frame: NSRect(x: 0, y: 0, width: 720, height: 520))
window.contentView = content
window.orderFrontRegardless()
spin(1.0)
baseline = Set(wkProcs().map { $0.pid })

let mode = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "cold"
let url = URL(string: "https://www.google.com/search?q=rust+programming")!

func loadGoogle(_ web: WKWebView, _ label: String, _ seq: Int) {
    let nav = Nav(); web.navigationDelegate = nav
    var peak = 0.0
    let t = now()
    web.load(URLRequest(url: url))
    waitUntil(25) { nav.finish != nil || nav.failed != nil }
    let finish = now()
    for _ in 0..<40 { spin(0.1); peak = max(peak, total(ours())) }
    let commit = nav.commit.map { ($0 - t) * 1000 } ?? -1
    print(String(format: "  %@#%d commit %5.0f ms | finish %5.0f ms | peak %5.0f MB | settled %@%@",
                 label, seq, commit, (finish - t) * 1000, peak, namelist(),
                 nav.failed.map { "  FAILED: \($0)" } ?? ""))
}

switch mode {
case "cold":
    let (web, initMs) = makeWeb(js: true, persistent: true, ua: true)
    print(String(format: "WKWebView init returned in %.1f ms (blocking); procs: %@", initMs, namelist()))
    content.addSubview(web)
    var tProcs = now()
    waitUntil(3) { ours().count >= 3 }
    tProcs = (now() - tProcs) * 1000
    print(String(format: "  GPU+Networking+WebContent up %.0f ms after init: %@", tProcs, namelist()))
    loadGoogle(web, "cold google", 1)

case "prewarm":
    let (web, initMs) = makeWeb(js: true, persistent: true, ua: true)
    content.addSubview(web)
    let nav = Nav(); web.navigationDelegate = nav
    let t0 = now()
    web.load(URLRequest(url: URL(string: "about:blank")!))
    waitUntil(3) { nav.finish != nil }
    print(String(format: "prewarm: init %.1f ms, about:blank finished in %.0f ms, %@", initMs, (now() - t0) * 1000, namelist()))
    spin(0.5)
    loadGoogle(web, "warm google", 1)
    loadGoogle(web, "warm google", 2)

case "js":
    let (web, _) = makeWeb(js: false, persistent: true, ua: true)
    content.addSubview(web)
    loadGoogle(web, "js-off google", 1)

case "nojs-html":
    let (web, _) = makeWeb(js: false, persistent: true, ua: true)
    content.addSubview(web)
    let nav = Nav(); web.navigationDelegate = nav
    let t = now()
    web.load(URLRequest(url: URL(string: "https://html.duckduckgo.com/html/?q=rust+programming")!))
    waitUntil(25) { nav.finish != nil || nav.failed != nil }
    print(String(format: "  nojs html.duckduckgo commit %5.0f ms finish %5.0f ms %@",
                 (nav.commit.map { ($0 - t) * 1000 } ?? -1), (now() - t) * 1000, namelist()))

case "nonpersistent":
    let (web, _) = makeWeb(js: true, persistent: false, ua: true)
    content.addSubview(web)
    loadGoogle(web, "nonpersistent google", 1)

case "kill":
    var web: WKWebView? = makeWeb(js: true, persistent: true, ua: true).0
    content.addSubview(web!)
    loadGoogle(web!, "before-kill google", 1)
    let wc = ours().first { $0.name == "WebContent" }?.pid
    web!.stopLoading(); web!.loadHTMLString("", baseURL: nil)
    spin(1.0)
    web!.removeFromSuperview(); web = nil
    let tDrop = now()
    // wait for WebContent pid to disappear
    var gone = false
    let end = now() + 60
    while now() < end {
        if let wc, wkProcs().allSatisfy({ $0.pid != wc }) { gone = true; break }
        spin(0.25)
    }
    print(String(format: "  WebContent pid %@ after %.1f s; procs now %@", gone ? "reaped" : "still alive", now() - tDrop, namelist()))
    let (web2, init2) = makeWeb(js: true, persistent: true, ua: true)
    content.addSubview(web2)
    print(String(format: "  recreate init %.1f ms", init2))
    loadGoogle(web2, "after-kill google", 2)

case "reopen":
    let (web, _) = makeWeb(js: true, persistent: true, ua: true)
    content.addSubview(web)
    loadGoogle(web, "first open", 1)
    // path A: current maccer hide (blank the document)
    web.stopLoading(); web.loadHTMLString("", baseURL: nil); web.isHidden = true
    spin(1.5)
    web.isHidden = false
    loadGoogle(web, "reopen after blank", 2)
    // path B: hide only, keep the document
    web.isHidden = true
    spin(1.0)
    let c0 = ours().reduce(0.0) { $0 + $1.rss }
    spin(8.0)
    let c1 = ours().reduce(0.0) { $0 + $1.rss }
    print(String(format: "  kept-document hidden: %.0f MB -> %.0f MB over 8 s", c0, c1))
    web.isHidden = false
    let tShow = now()
    spin(0.3)
    print(String(format: "  re-show with document kept: %.0f ms, %@", (now() - tShow) * 1000, namelist()))

case "hiddencpu":
    let (web, _) = makeWeb(js: true, persistent: true, ua: true)
    content.addSubview(web)
    loadGoogle(web, "loaded", 1)
    func cpuSecs() -> Double {
        let p = Process(); p.executableURL = URL(fileURLWithPath: "/bin/bash")
        p.arguments = ["-c", "ps -Ao pid,time,comm | grep -i 'WebKit.*xpc' | grep -v grep"]
        let pipe = Pipe(); p.standardOutput = pipe; try? p.run(); p.waitUntilExit()
        let out = String(data: pipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
        var t = 0.0
        for l in out.split(separator: "\n") {
            let f = l.split(separator: " ", omittingEmptySubsequences: true)
            guard f.count >= 3, baseline.contains(Int(f[0]) ?? -1) == false else { continue }
            let parts = f[1].split(separator: ":")
            if parts.count == 3, let h = Double(parts[0]), let m = Double(parts[1]), let s = Double(parts[2]) { t += h*3600 + m*60 + s }
            else if parts.count == 2, let m = Double(parts[0]), let s = Double(parts[1]) { t += m*60 + s }
        }
        return t
    }
    web.isHidden = true
    spin(1.0)
    let a = cpuSecs(); spin(10.0); let b = cpuSecs()
    print(String(format: "  hidden with page live: %.2f CPU-s over 10 s (%.1f%% of one core)", b - a, (b - a) * 10))
    web.stopLoading(); web.loadHTMLString("", baseURL: nil)
    spin(1.0)
    let c = cpuSecs(); spin(10.0); let d = cpuSecs()
    print(String(format: "  hidden with blank doc: %.2f CPU-s over 10 s (%.1f%% of one core)", d - c, (d - c) * 10))


case "prewarm-hidden":
    // maccer keeps the view hidden until a page is shown. Confirm the warm
    // load still starts WebContent while hidden.
    let (web, _) = makeWeb(js: true, persistent: true, ua: true)
    web.isHidden = true
    content.addSubview(web)
    let nav = Nav(); web.navigationDelegate = nav
    let t0 = now()
    web.load(URLRequest(url: URL(string: "about:blank")!))
    waitUntil(3) { nav.finish != nil }
    print(String(format: "hidden warm: about:blank finished in %.0f ms, %@", (now() - t0) * 1000, namelist()))
    spin(0.5)
    web.isHidden = false
    loadGoogle(web, "hidden-warm google", 1)

default: print("unknown mode")
}
