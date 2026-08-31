#!/usr/bin/env python3
"""Read the KaiOS screen as text, over Gecko's remote debugging socket.

kaimirror otherwise sees the phone as pixels: `shot`, then look at the PNG,
then guess where the focus ring is.  An agent driving the phone would rather
read the screen, the way agent-browser's `snapshot` returns an accessibility
tree instead of an image.  b2g *is* Gecko, and INTERNALS.md notes that its
remote debugging socket is live on these devices:

    /data/local/firefox-debugger-socket   (devtools.debugger.remote-enabled)

This script is the investigation behind
[Reading the screen as text](../docs/INTERNALS.md#reading-the-screen-as-text),
and it doubles as a working prototype: it prints a text tree of whatever the
phone is showing, with the focused element marked.  Everything happens on the
host over `adb forward` -- no device pump, no new binary on the phone.

    tools/devtools-probe.py              # snapshot the foreground app
    tools/devtools-probe.py --all        # every target, not just the focused one
    tools/devtools-probe.py --a11y       # also test Gecko's accessibility walker
    tools/devtools-probe.py --raw        # dump every RDP packet exchanged
"""

import argparse
import json
import re
import socket
import subprocess
import sys
import textwrap
import time

SOCKET = "/data/local/firefox-debugger-socket"

def adb(*args, check=True):
    p = subprocess.run(("adb",) + args, capture_output=True, text=True)
    if check and p.returncode != 0:
        die(f"adb {' '.join(args)} failed: {p.stderr.strip() or p.stdout.strip()}")
    return p.stdout

def die(msg):
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)

# ---------------------------------------------------------------- transport

class RDP:
    """A Remote Debugging Protocol connection.

    Framing is `<byte-length>:<json>`.  The server greets first, and pushes
    events (`tabListChanged`, `frameUpdate`, `consoleAPICall`) interleaved with
    replies whenever it likes -- an event carries a `type` and a reply does
    not, which is the whole rule for telling them apart.
    """

    def __init__(self, port, raw=False, timeout=15):
        self.raw = raw
        self.buf = b""
        self.parked = []
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        self.hello = self.recv()

    def recv(self):
        while True:
            m = re.match(rb"(\d+):", self.buf)
            if m and len(self.buf) >= m.end() + int(m.group(1)):
                n, start = int(m.group(1)), m.end()
                pkt = json.loads(self.buf[start:start + n])
                self.buf = self.buf[start + n:]
                if self.raw:
                    print(f"  <- {json.dumps(pkt)[:400]}")
                return pkt
            chunk = self.sock.recv(65536)
            if not chunk:
                die("devtools socket closed")
            self.buf += chunk

    def send(self, **packet):
        body = json.dumps(packet).encode()
        if self.raw:
            print(f"  -> {json.dumps(packet)[:400]}")
        self.sock.sendall(str(len(body)).encode() + b":" + body)

    def ask(self, to, type, want_type=None, **kw):
        """Send one request and return the matching reply.

        `want_type` waits for a *later* packet of that type from the same
        actor, which is how evaluateJSAsync answers: an immediate ack carrying
        a resultID, then the evaluationResult once the script has run.
        """
        self.send(to=to, type=type, **kw)
        for pkt in list(self.parked):
            if self._matches(pkt, to, want_type):
                self.parked.remove(pkt)
                return pkt
        while True:
            pkt = self.recv()
            if self._matches(pkt, to, want_type):
                return pkt
            self.parked.append(pkt)

    @staticmethod
    def _matches(pkt, to, want_type):
        if pkt.get("from") != to:
            return False
        return pkt.get("type") == want_type if want_type else "type" not in pkt

    def string(self, value):
        """Resolve a long-string grip: anything over ~10k arrives as an actor."""
        if isinstance(value, dict) and value.get("type") == "longString":
            got = self.ask(value["actor"], "substring", start=0, end=value["length"])
            return got.get("substring", "")
        return value

    def eval(self, console, js):
        reply = self.ask(console, "evaluateJSAsync", want_type="evaluationResult",
                         text=js.strip())
        if reply.get("hasException"):
            return f"<threw: {reply.get('exceptionMessage') or reply.get('exception')}>"
        return self.string(reply.get("result"))

def err(pkt):
    if isinstance(pkt, dict) and "error" in pkt:
        return f"{pkt['error']}: {pkt.get('message', '')}".strip(": ")
    return None

# ------------------------------------------------------------------ payload

# Runs inside the app's own window.  Deliberately ES5 and dependency-free, and
# it has to survive whatever markup a KaiOS app happens to use.
SNAPSHOT_JS = r"""
(function () {
  var out = [], n = 0;
  function name(el) {
    var s = el.getAttribute('aria-label') ||
            (el.labels && el.labels[0] && el.labels[0].textContent) ||
            (el.tagName === 'INPUT' ? (el.value || el.placeholder || '') : '') ||
            el.textContent || '';
    return s.replace(/\s+/g, ' ').trim().slice(0, 60);
  }
  function role(el) { return el.getAttribute('role') || el.tagName.toLowerCase(); }
  function onscreen(el) {
    var cs = getComputedStyle(el);
    if (cs.display === 'none' || cs.visibility === 'hidden' || cs.opacity === '0') return false;
    var r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0 &&
           r.bottom > 0 && r.right > 0 &&
           r.top < innerHeight && r.left < innerWidth;
  }
  function walk(el, depth) {
    if (n++ > 400 || depth > 14) return;
    if (!onscreen(el)) return;
    var kids = el.children, label = name(el);
    var mark = (el === document.activeElement) ? ' [FOCUSED]' : '';
    // Print what an agent would act on -- anything named, focusable, or
    // carrying its own text -- and let pure layout boxes pass through.
    var worth = mark || el.getAttribute('role') || el.getAttribute('aria-label') ||
                el.tabIndex >= 0 ||
                /^(A|BUTTON|INPUT|SELECT|TEXTAREA|LI|H1|H2|H3)$/.test(el.tagName) ||
                (kids.length === 0 && label);
    if (worth) out.push('  '.repeat(depth) + role(el) + (label ? ' "' + label + '"' : '') + mark);
    // Indent by depth in the *printed* tree, not in the DOM: the layout
    // wrappers a KaiOS app nests eight deep say nothing and should cost
    // nothing to read.
    for (var i = 0; i < kids.length; i++) walk(kids[i], worth ? depth + 1 : depth);
  }
  walk(document.body, 0);
  var a = document.activeElement;
  return ['url: ' + location.href,
          'focus: ' + (a ? role(a) + ' "' + name(a) + '"' : 'none'),
          ''].concat(out).join('\n');
})()
"""

# document.hidden lies here -- the launcher reports hidden while it is the
# thing on the panel -- but exactly one target answers true to hasFocus().
FOREGROUND_JS = "document.hasFocus() + '|' + document.hidden + '|' + document.title"

# -------------------------------------------------------------------- probe

def head(title):
    print(f"\n=== {title} " + "=" * max(0, 58 - len(title)))

def targets(rdp, verbose=True):
    """Every debuggable window: one target per running app, plus b2g's shell."""
    found = []
    tabs = rdp.ask("root", "listTabs")
    if e := err(tabs):
        die(f"listTabs: {e}")
    for entry in tabs.get("tabs", []):
        got = rdp.ask(entry["actor"], "getTarget")
        form = got.get("frame") or got.get("form") or entry
        # The console will not answer until the target is attached: an
        # evaluateJSAsync sent before this simply never gets a reply.
        rdp.ask(form["actor"], "attach")
        found.append(form)
    # The system UI is not a tab.  It is the parent process, chrome://b2g.
    proc = rdp.ask("root", "getProcess", id=0)
    if not err(proc):
        got = rdp.ask(proc["processDescriptor"]["actor"], "getTarget")
        form = got.get("process")
        if form:
            rdp.ask(form["actor"], "attach")
            found.append(form)
    for form in found:
        focus, hidden, title = (rdp.eval(form["consoleActor"], FOREGROUND_JS) or "||").split("|")
        form["_focused"] = focus == "true"
        if verbose:
            flag = "  <- foreground" if form["_focused"] else ""
            print(f"  {form.get('url', '?')[:52]:54} hidden={hidden:5} {title[:16]:18}{flag}")
    return found

def check_a11y(rdp, form, enable):
    """Gecko's own accessibility walker: roles and computed names, per node.

    Present on this build but *not running*, and the devtools actor here has
    no `enable` -- the parent-process ParentAccessibilityActor that Firefox
    uses for that does not exist in this server.  The engine can still be
    started by instantiating the service from the chrome console, which is a
    device-wide change, so it only happens when asked for.
    """
    a11y = form.get("accessibilityActor")
    if not a11y:
        print("  no accessibilityActor on this target")
        return
    print(f"  actor accepts: {rdp.ask(a11y, 'requestTypes').get('requestTypes')}")
    print(f"  bootstrap: {json.dumps(rdp.ask(a11y, 'bootstrap').get('state'))}")
    if not enable:
        print("  engine not started (pass --a11y to start it and walk the tree)")
        return
    proc = rdp.ask("root", "getProcess", id=0)
    got = rdp.ask(proc["processDescriptor"]["actor"], "getTarget")["process"]
    rdp.ask(got["actor"], "attach")
    chrome = got["consoleActor"]
    print("  starting the engine:", rdp.eval(chrome, """
        (function () { try {
          Cc['@mozilla.org/accessibilityService;1'].getService(Ci.nsIAccessibilityService);
          return 'running=' + Services.appinfo.accessibilityEnabled;
        } catch (e) { return 'failed: ' + e; } })()"""))
    time.sleep(2)
    try:
        walker = rdp.ask(a11y, "getWalker")["walker"]["actor"]
        roots = rdp.ask(walker, "children").get("children", [])
        # One round trip per node is the cost of this route; two levels is
        # enough to show it answers.
        for node in roots:
            print(f"  {node['role']} \"{node['name']}\" ({node['childCount']} children)")
            for kid in rdp.ask(node["actor"], "children").get("children", []):
                print(f"    {kid['role']} \"{kid['name']}\" ({kid['childCount']} children)")
    finally:
        # Put the phone back: force_disabled=1 shuts the service down, then
        # the pref goes back to the 0 (auto) it was found at.
        print("  stopping the engine:", rdp.eval(chrome,
            "(function(){Services.prefs.setIntPref('accessibility.force_disabled',1);"
            "var s='running='+Services.appinfo.accessibilityEnabled;"
            "Services.prefs.setIntPref('accessibility.force_disabled',0);return s})()"))

def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=6080, help="host port for the forward")
    ap.add_argument("--all", action="store_true", help="snapshot every target")
    ap.add_argument("--a11y", action="store_true",
                    help="start Gecko's accessibility engine and walk its tree")
    ap.add_argument("--raw", action="store_true", help="dump every RDP packet")
    args = ap.parse_args()

    head("device")
    if "uid=0" not in adb("shell", "id", check=False):
        adb("root", check=False)
        adb("wait-for-device")
        if "uid=0" not in adb("shell", "id", check=False):
            die("need adb root (userdebug build with ro.debuggable=1)")
    listing = adb("shell", f"ls -l {SOCKET} 2>&1", check=False).strip()
    print(f"  {listing}")
    if "No such file" in listing:
        die("no debugger socket; check devtools.debugger.remote-enabled")
    adb("forward", "--remove", f"tcp:{args.port}", check=False)
    adb("forward", f"tcp:{args.port}", f"localfilesystem:{SOCKET}")
    print(f"  forward tcp:{args.port} -> {SOCKET}")

    try:
        rdp = RDP(args.port, raw=args.raw)
    except OSError as e:
        die(f"could not connect through the forward: {e}")
    print(f"  {rdp.hello.get('applicationType')}, traits: "
          f"{', '.join(sorted(rdp.hello.get('traits', {})))}")

    head("targets")
    forms = targets(rdp)
    # b2g's shell always claims focus too, so a focused *app* wins over it.
    front = next((f for f in forms if f["_focused"]
                  and not f.get("url", "").startswith("chrome://")),
                 next((f for f in forms if f["_focused"]), forms[0]))

    head("accessibility walker")
    check_a11y(rdp, front, args.a11y)

    for form in (forms if args.all else [front]):
        head(f"snapshot: {form.get('url', '?')[:40]}")
        started = time.time()
        text = rdp.eval(form["consoleActor"], SNAPSHOT_JS)
        elapsed = (time.time() - started) * 1000
        print(textwrap.indent(str(text), "  "))
        print(f"\n  ({elapsed:.0f} ms)")

if __name__ == "__main__":
    main()
