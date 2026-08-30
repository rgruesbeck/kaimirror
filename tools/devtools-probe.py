#!/usr/bin/env python3
"""Probe the KaiOS device's Gecko DevTools socket for a text snapshot route.

kaimirror sees the phone as pixels.  An agent driving it would rather read
the screen than look at it, the way agent-browser's `snapshot` returns an
accessibility tree instead of a PNG.  b2g *is* Gecko, and INTERNALS.md notes
that its remote debugging socket is live on these devices:

    /data/local/firefox-debugger-socket   (devtools.debugger.remote-enabled)

That socket speaks the Remote Debugging Protocol, which already knows how to
answer "what is on the screen" in three different ways -- the accessibility
walker (roles and computed names, the closest analogue to a browser
snapshot), the inspector's DOM walker, and plain JS evaluation in the app's
own window.  This script finds out which of the three this build actually
offers, and prints a sample snapshot from the best one it reaches.

Nothing here writes to the device or presses a key: it opens the socket,
asks questions, and prints the answers.  Run it with a phone attached:

    tools/devtools-probe.py            # probe, then snapshot the foreground app
    tools/devtools-probe.py --raw      # ...and dump every RDP packet exchanged
    tools/devtools-probe.py --port N   # host port for the forward (default 6080)
"""

import argparse
import json
import re
import socket
import subprocess
import sys
import textwrap

# ---------------------------------------------------------------- transport

def adb(*args, check=True):
    p = subprocess.run(("adb",) + args, capture_output=True, text=True)
    if check and p.returncode != 0:
        die(f"adb {' '.join(args)} failed: {p.stderr.strip() or p.stdout.strip()}")
    return p.stdout

def die(msg):
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)

class RDP:
    """A Remote Debugging Protocol connection.

    Framing is `<byte-length>:<json>`.  Replies are not the only thing that
    arrives -- the server pushes events (tabListChanged, consoleAPICall,
    resource-available-form) whenever it likes -- so a request reads until it
    sees a packet from the actor it asked, and parks everything else.
    """

    def __init__(self, port, raw=False):
        self.raw = raw
        self.buf = b""
        self.parked = []
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=15)
        self.hello = self.recv()          # the server greets first

    def recv(self):
        while True:
            m = re.match(rb"(\d+):", self.buf)
            if m:
                n = int(m.group(1))
                start = m.end()
                if len(self.buf) >= start + n:
                    body = self.buf[start:start + n]
                    self.buf = self.buf[start + n:]
                    pkt = json.loads(body)
                    if self.raw:
                        print(f"  <- {json.dumps(pkt)[:400]}")
                    return pkt
            chunk = self.sock.recv(65536)
            if not chunk:
                die("devtools socket closed (b2g may have dropped the connection)")
            self.buf += chunk

    def send(self, **packet):
        body = json.dumps(packet).encode()
        if self.raw:
            print(f"  -> {json.dumps(packet)[:400]}")
        self.sock.sendall(str(len(body)).encode() + b":" + body)

    def ask(self, to, type, want_type=None, **kw):
        """Send one request and return the matching reply.

        `want_type` waits for a *later* packet of that type from the same
        actor, which is how evaluateJSAsync answers: an immediate ack with a
        resultID, then the evaluationResult once it has run.
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
        """Is this packet the reply we are waiting for?

        An event carries a `type` and a reply does not, which is what keeps a
        root-actor event -- tabListChanged fires constantly -- from being read
        as the answer to a root-actor request.  The exception is a reply we
        asked for by type, which is how evaluateJSAsync delivers its result.
        """
        if pkt.get("from") != to:
            return False
        return pkt.get("type") == want_type if want_type else "type" not in pkt

def err(pkt):
    """The protocol's error shape, or None if the reply is a real answer."""
    if isinstance(pkt, dict) and "error" in pkt:
        return f"{pkt['error']}: {pkt.get('message', '')}".strip(": ")
    return None

# ------------------------------------------------------------------ payload

# Run inside the app's own window.  Deliberately dependency-free and defensive:
# this is the fallback that works even if the accessibility engine is off, and
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
  function role(el) {
    return el.getAttribute('role') || el.tagName.toLowerCase();
  }
  function interesting(el) {
    var cs = getComputedStyle(el);
    if (cs.display === 'none' || cs.visibility === 'hidden') return false;
    var r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  }
  function walk(el, depth) {
    if (n++ > 400 || depth > 12) return;
    if (!interesting(el)) return;
    var kids = el.children;
    var label = name(el);
    var mark = (el === document.activeElement) ? ' [FOCUSED]' : '';
    // Print the elements an agent would act on -- anything named, focusable,
    // or carrying its own text -- and let pure layout boxes pass through.
    var worth = mark ||
                el.getAttribute('role') || el.getAttribute('aria-label') ||
                el.tabIndex >= 0 ||
                /^(A|BUTTON|INPUT|SELECT|TEXTAREA|LI|H1|H2|H3)$/.test(el.tagName) ||
                (kids.length === 0 && label);
    if (worth) {
      out.push('  '.repeat(depth) + role(el) + (label ? ' "' + label + '"' : '') + mark);
    }
    for (var i = 0; i < kids.length; i++) walk(kids[i], depth + 1);
  }
  walk(document.body, 0);
  return ['url: ' + location.href,
          'hidden: ' + document.hidden,
          'focus: ' + (document.activeElement ? role(document.activeElement) + ' "' + name(document.activeElement) + '"' : 'none'),
          ''].concat(out).join('\n');
})()
"""

# ------------------------------------------------------------------- phases

def head(title):
    print(f"\n=== {title} " + "=" * max(0, 60 - len(title)))

def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=6080, help="host port for the forward")
    ap.add_argument("--raw", action="store_true", help="dump every RDP packet")
    args = ap.parse_args()

    head("device")
    if "uid=0" not in adb("shell", "id", check=False):
        adb("root", check=False)
        adb("wait-for-device")
        if "uid=0" not in adb("shell", "id", check=False):
            die("need adb root (userdebug build with ro.debuggable=1)")
    sock_path = "/data/local/firefox-debugger-socket"
    listing = adb("shell", f"ls -l {sock_path} 2>&1", check=False).strip()
    print(f"  {listing}")
    if "No such file" in listing:
        die("no debugger socket; check devtools.debugger.remote-enabled on the device")
    prefs = adb("shell",
                "grep -h 'devtools\\|accessibility' /data/b2g/mozilla/*.default/prefs.js "
                "/system/b2g/defaults/pref/*.js 2>/dev/null | sort -u", check=False)
    for line in prefs.splitlines():
        print(f"  pref {line.strip()}")

    head("forward")
    adb("forward", "--remove", f"tcp:{args.port}", check=False)
    adb("forward", f"tcp:{args.port}", f"localfilesystem:{sock_path}")
    print(f"  tcp:{args.port} -> {sock_path}")

    try:
        rdp = RDP(args.port, raw=args.raw)
    except OSError as e:
        die(f"could not connect through the forward: {e}")

    head("root actor")
    print(f"  applicationType: {rdp.hello.get('applicationType')}")
    traits = rdp.hello.get("traits", {})
    print(f"  traits: {', '.join(sorted(traits)) or '(none)'}")

    head("targets")
    targets = []            # (label, target-form)
    tabs = rdp.ask("root", "listTabs")
    if e := err(tabs):
        print(f"  listTabs: {e}")
    for entry in tabs.get("tabs", []):
        actor = entry.get("actor")
        form = entry
        # Since Firefox ~75 listTabs returns *descriptors*; the target with the
        # console and accessibility actors on it comes from getTarget.
        got = rdp.ask(actor, "getTarget")
        if not err(got):
            form = got.get("frame", got.get("form", form))
        targets.append((entry.get("url") or entry.get("title") or actor, form))
    # b2g used to expose apps through a webapps actor rather than as tabs; if
    # listTabs came back thin, that is where the running apps will be.
    webapps = rdp.ask("root", "getWebapps")
    if not err(webapps):
        wa = webapps.get("webappsActor")
        print(f"  webapps actor: {wa}")
        running = rdp.ask(wa, "listRunningApps")
        for manifest in running.get("apps", []):
            got = rdp.ask(wa, "getAppActor", manifestURL=manifest)
            if not err(got):
                targets.append((manifest, got.get("actor", {})))
    else:
        print("  webapps actor: absent (apps are plain tabs on this build)")

    if not targets:
        die("no targets: nothing to snapshot")
    for label, form in targets:
        kinds = [k for k in ("consoleActor", "accessibilityActor", "inspectorActor",
                             "walkerActor") if form.get(k)]
        print(f"  target {label}\n    actors: {', '.join(kinds) or '(bare descriptor)'}")

    head("accessibility actor")
    # The closest analogue to agent-browser's snapshot: roles and computed
    # names straight from Gecko's a11y engine.  It is also the piece most
    # likely to be missing, since a build can be made without it.
    a11y_ok = False
    for label, form in targets:
        a11y = form.get("accessibilityActor")
        if not a11y:
            continue
        boot = rdp.ask(a11y, "bootstrap")
        if e := err(boot):
            print(f"  {label}: bootstrap -> {e}")
            continue
        print(f"  {label}: bootstrap -> {json.dumps(boot)[:200]}")
        for attempt in ("enable", "getWalker"):
            reply = rdp.ask(a11y, attempt)
            print(f"  {label}: {attempt} -> {json.dumps(reply)[:200]}")
            if attempt == "getWalker" and not err(reply):
                a11y_ok = True
    if not any(f.get("accessibilityActor") for _, f in targets):
        print("  no accessibilityActor on any target (this build ships devtools without it)")

    head("snapshot via JS evaluation")
    # The fallback that needs nothing but a console actor.  Whatever the
    # accessibility answer above, this is what tells us a text snapshot is
    # reachable at all.
    printed = False
    for label, form in targets:
        console = form.get("consoleActor")
        if not console:
            continue
        reply = rdp.ask(console, "evaluateJSAsync", want_type="evaluationResult",
                        text=SNAPSHOT_JS.strip())
        if e := err(reply):
            print(f"  {label}: {e}")
            continue
        result = reply.get("result")
        if isinstance(result, dict):
            print(f"  {label}: non-string result {json.dumps(result)[:200]}")
            continue
        exc = reply.get("exceptionMessage") or reply.get("exception")
        if exc:
            print(f"  {label}: threw {exc}")
            continue
        print(f"\n--- {label} ---")
        print(textwrap.indent(str(result), "  "))
        printed = True

    head("verdict")
    print(f"  accessibility walker reachable: {'yes' if a11y_ok else 'no'}")
    print(f"  DOM snapshot over the console:  {'yes' if printed else 'no'}")
    print("  (a `yes` on either line means `kaimirror snapshot` is buildable)")

if __name__ == "__main__":
    main()
