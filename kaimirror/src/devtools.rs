//! Reading the screen as text, over Gecko's remote debugging socket.
//!
//! Everything else here treats the phone as pixels.  This does not: b2g *is*
//! Gecko, and Gecko's debugging server is listening on
//! `/data/local/firefox-debugger-socket`, which is enough to ask the running
//! app what is on the screen and get roles, names and focus back as text.
//! A snapshot costs ~50ms against the ~2.3s a `shot` costs, and it takes it
//! without pressing anything -- though a phone that has stopped repainting
//! serves a stale DOM the same way it serves a stale frame.
//!
//! It is also the one path here that needs nothing on the device: `adb
//! forward` maps the socket to a local port and the whole conversation is
//! host-side.  No pump, no /dev/input, no gfxdebugger.
//!
//! See docs/INTERNALS.md, "Reading the screen as text", for how this was
//! found and what else was tried.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::adb::{adb, fail};
use crate::json::{self, Json};

const SOCKET: &str = "/data/local/firefox-debugger-socket";

/// The server answers in milliseconds or not at all -- a request it does not
/// like is usually met with silence rather than an error, so this bound is
/// what turns a hang into a diagnosis.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);

/// Runs inside the app's own window.  ES5 and dependency-free, because it has
/// to survive whatever markup a KaiOS app happens to use, and it returns the
/// tree as text so the whole screen costs one round trip.
const SNAPSHOT_JS: &str = r#"
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
           r.bottom > 0 && r.right > 0 && r.top < innerHeight && r.left < innerWidth;
  }
  function walk(el, depth) {
    if (n++ > 400 || depth > 14) return;
    if (!onscreen(el)) return;
    var kids = el.children, label = name(el);
    var mark = (el === document.activeElement) ? ' [FOCUSED]' : '';
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
  return ['focus: ' + (a ? role(a) + ' "' + name(a) + '"' : 'none'), '']
         .concat(out).join('\n');
})()
"#;

/// document.hidden is not the answer: the launcher reports hidden while it is
/// the thing on the panel.  hasFocus() is.
const FOREGROUND_JS: &str = "document.hasFocus() + '|' + document.title";

/// One debuggable window: a running app, or b2g's own shell.
pub struct Target {
    pub actor: String,
    pub console: String,
    pub url: String,
    pub title: String,
    pub focused: bool,
}

impl Target {
    /// The app's own name, as something short enough to print in a list.
    pub fn label(&self) -> String {
        if !self.title.is_empty() {
            return self.title.clone();
        }
        // App URLs are http://<app>.localhost/index.html#<state>, and the
        // state can be a whole percent-encoded JSON blob.  The host is the
        // part worth showing.
        self.url
            .split("://").nth(1).unwrap_or(&self.url)
            .split('/').next().unwrap_or(&self.url)
            .to_string()
    }
}

pub struct Conn {
    sock: TcpStream,
    buf: Vec<u8>,
    /// Events, and replies that arrived before the request that wanted them
    /// got around to asking.  Requests can be pipelined, so out-of-order is
    /// normal rather than exceptional.
    parked: Vec<Json>,
}

impl Conn {
    /// Forward the device socket to a local port and connect to it.
    pub fn open(port: u16) -> Conn {
        // A stale forward from an earlier run points at the same place, but
        // removing it first keeps `adb forward` from complaining about a port
        // it already owns.
        adb(&["forward", "--remove", &format!("tcp:{port}")]);
        let out = adb(&[
            "forward",
            &format!("tcp:{port}"),
            &format!("localfilesystem:{SOCKET}"),
        ]);
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr);
            fail(&format!(
                "could not forward the debugging socket: {}\n\
                 (does {SOCKET} exist?  it needs devtools.debugger.remote-enabled)",
                why.trim()
            ));
        }
        let sock = TcpStream::connect(("127.0.0.1", port)).unwrap_or_else(|e| {
            fail(&format!("could not connect to the forwarded socket: {e}"))
        });
        let _ = sock.set_read_timeout(Some(REPLY_TIMEOUT));
        let mut conn = Conn { sock, buf: Vec::new(), parked: Vec::new() };
        conn.recv(); // the server greets first
        conn
    }

    fn recv(&mut self) -> Json {
        loop {
            if let Some(body) = split_packet(&mut self.buf) {
                let Some(pkt) = json::parse(&body) else {
                    fail(&format!("could not read a reply from the debugging server: {body}"));
                };
                return pkt;
            }
            let mut chunk = [0u8; 65536];
            match self.sock.read(&mut chunk) {
                Ok(0) => fail("the debugging server closed the connection"),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) => fail(&format!("the debugging server stopped answering: {e}")),
            }
        }
    }

    /// Send one request.  `extra` is a rendered fragment of JSON fields,
    /// each with its leading comma, or "".
    fn send(&mut self, to: &str, kind: &str, extra: &str) {
        let body = format!(
            "{{\"to\":{},\"type\":{}{}}}",
            json::quote(to),
            json::quote(kind),
            extra
        );
        let framed = format!("{}:{}", body.len(), body);
        if self.sock.write_all(framed.as_bytes()).is_err() {
            fail("could not write to the debugging server");
        }
    }

    /// Wait for a reply from `to`, parking everything else.
    fn wait(&mut self, to: &str, want: Option<&str>) -> Json {
        if let Some(i) = self.parked.iter().position(|p| is_reply(p, to, want)) {
            return self.parked.remove(i);
        }
        loop {
            let pkt = self.recv();
            if is_reply(&pkt, to, want) {
                return pkt;
            }
            self.parked.push(pkt);
        }
    }

    fn ask(&mut self, to: &str, kind: &str, extra: &str) -> Json {
        self.send(to, kind, extra);
        self.wait(to, None)
    }

    /// Evaluate JS in a target's window and return what it produced.
    ///
    /// Two packets come back: an immediate ack carrying a resultID, then the
    /// evaluationResult once the script has run.
    fn eval_result(&mut self, console: &str) -> String {
        self.wait(console, None); // the ack
        let reply = self.wait(console, Some("evaluationResult"));
        if reply.is_true("hasException") {
            let why = reply.text("exceptionMessage");
            return format!("<the page threw: {}>", if why.is_empty() { "?" } else { why });
        }
        match reply.get("result") {
            Some(Json::Str(s)) => s.clone(),
            // Anything past ~10k arrives as an actor to call substring on
            // rather than as the string itself, and a busy screen crosses
            // that line.
            Some(grip) if grip.text("type") == "longString" => {
                let actor = grip.text("actor").to_string();
                let end = grip.num("length").unwrap_or(0.0) as u64;
                let got = self.ask(&actor, "substring", &format!(",\"start\":0,\"end\":{end}"));
                got.text("substring").to_string()
            }
            _ => String::new(),
        }
    }

    fn send_eval(&mut self, console: &str, js: &str) {
        self.send(console, "evaluateJSAsync", &format!(",\"text\":{}", json::quote(js)));
    }

    /// Every debuggable window on the phone, foreground marked.
    ///
    /// Requests are pipelined a stage at a time -- every getTarget, then every
    /// attach, then every focus check -- because each stage needs the one
    /// before it but not itself.  Sequentially this is seven round trips per
    /// stage against one.
    pub fn targets(&mut self) -> Vec<Target> {
        let tabs = self.ask("root", "listTabs", "");
        let descriptors: Vec<String> = tabs
            .list("tabs")
            .iter()
            .map(|t| t.text("actor").to_string())
            .collect();
        for actor in &descriptors {
            self.send(actor, "getTarget", "");
        }
        let mut forms: Vec<Json> = Vec::new();
        for actor in &descriptors {
            let reply = self.wait(actor, None);
            if let Some(frame) = reply.get("frame") {
                forms.push(frame.clone());
            }
        }
        // The system UI is not a tab.  It is the parent process --
        // chrome://b2g/content/shell.html -- and it is where the status bar
        // and the IME's own indicator live.
        let proc = self.ask("root", "getProcess", ",\"id\":0");
        if let Some(desc) = proc.get("processDescriptor") {
            let reply = self.ask(desc.text("actor"), "getTarget", "");
            if let Some(form) = reply.get("process") {
                forms.push(form.clone());
            }
        }

        let mut targets: Vec<Target> = forms
            .iter()
            .map(|f| Target {
                actor: f.text("actor").to_string(),
                console: f.text("consoleActor").to_string(),
                url: f.text("url").to_string(),
                title: f.text("title").to_string(),
                focused: false,
            })
            .collect();

        // A target's console never answers until the target is attached: an
        // evaluateJSAsync sent before this gets no reply and no error, it just
        // hangs until the read times out.
        for t in &targets {
            self.send(&t.actor, "attach", "");
        }
        let actors: Vec<String> = targets.iter().map(|t| t.actor.clone()).collect();
        for actor in &actors {
            self.wait(actor, None);
        }

        let consoles: Vec<String> = targets.iter().map(|t| t.console.clone()).collect();
        for console in &consoles {
            self.send_eval(console, FOREGROUND_JS);
        }
        for (i, console) in consoles.iter().enumerate() {
            let answer = self.eval_result(console);
            let (focus, title) = answer.split_once('|').unwrap_or(("false", ""));
            targets[i].focused = focus == "true";
            if targets[i].title.is_empty() {
                targets[i].title = title.to_string();
            }
        }
        targets
    }

    /// The text tree of one target's screen.
    pub fn snapshot(&mut self, target: &Target) -> String {
        self.send_eval(&target.console, SNAPSHOT_JS);
        self.eval_result(&target.console)
    }
}

/// Take one `<byte-length>:<json>` frame off the front of the buffer.
///
/// The length is in *bytes*, not characters, which matters the moment an app
/// title or a snapshot carries anything outside ASCII.
fn split_packet(buf: &mut Vec<u8>) -> Option<String> {
    let colon = buf.iter().position(|&b| b == b':')?;
    let header = std::str::from_utf8(&buf[..colon]).ok();
    let Some(len) = header.and_then(|h| h.parse::<usize>().ok()) else {
        fail("the debugging server sent something that is not RDP framing");
    };
    let start = colon + 1;
    if buf.len() < start + len {
        return None; // the rest is still on the wire
    }
    let body = String::from_utf8_lossy(&buf[start..start + len]).into_owned();
    buf.drain(..start + len);
    Some(body)
}

/// Is this packet the reply we are waiting for?
///
/// An event carries a `type` and a reply does not, which is the whole rule --
/// without it a `tabListChanged` gets read as the answer to whatever was
/// asked of the root actor.  `want` names the exception: a reply that *is*
/// delivered as a typed event, which is how evaluateJSAsync returns its
/// result.
fn is_reply(pkt: &Json, to: &str, want: Option<&str>) -> bool {
    pkt.text("from") == to
        && match want {
            Some(kind) => pkt.text("type") == kind,
            None => pkt.get("type").is_none(),
        }
}

/// The app the phone is actually showing.
///
/// b2g's shell claims focus alongside the app in front of it, so a focused
/// *app* wins over chrome://; if nothing claims focus at all -- which is what
/// a target list taken mid-transition looks like -- the first one is a better
/// answer than none.
pub fn foreground(targets: &[Target]) -> Option<&Target> {
    targets
        .iter()
        .find(|t| t.focused && !t.url.starts_with("chrome://"))
        .or_else(|| targets.iter().find(|t| t.focused))
        .or_else(|| targets.first())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(body: &str) -> Vec<u8> {
        format!("{}:{}", body.len(), body).into_bytes()
    }

    fn target(url: &str, focused: bool) -> Target {
        Target {
            actor: "a".into(), console: "c".into(),
            url: url.into(), title: String::new(), focused,
        }
    }

    #[test]
    fn frames_arrive_split_and_batched() {
        // Nothing guarantees a packet lands in one read: the server writes
        // several at once and TCP cuts them wherever it likes.
        let mut buf = Vec::new();
        buf.extend_from_slice(b"15:{\"from\":\"ro");
        assert_eq!(split_packet(&mut buf), None);
        buf.extend_from_slice(b"ot\"}");
        assert_eq!(split_packet(&mut buf).as_deref(), Some(r#"{"from":"root"}"#));

        let mut both = framed(r#"{"from":"a"}"#);
        both.extend(framed(r#"{"from":"b"}"#));
        assert_eq!(split_packet(&mut both).as_deref(), Some(r#"{"from":"a"}"#));
        assert_eq!(split_packet(&mut both).as_deref(), Some(r#"{"from":"b"}"#));
        assert!(both.is_empty());
    }

    #[test]
    fn the_length_is_bytes_not_characters() {
        // App titles and snapshots carry more than ASCII, and counting
        // characters here would cut every later packet in the stream.
        let body = r#"{"from":"a","title":"✓ 😀"}"#;
        let mut buf = framed(body);
        assert_eq!(split_packet(&mut buf).as_deref(), Some(body));
        assert!(buf.is_empty());
    }

    #[test]
    fn events_are_not_mistaken_for_replies() {
        let event = json::parse(r#"{"from":"root","type":"tabListChanged"}"#).unwrap();
        let reply = json::parse(r#"{"from":"root","tabs":[]}"#).unwrap();
        let result = json::parse(r#"{"from":"c1","type":"evaluationResult","result":"x"}"#).unwrap();

        assert!(!is_reply(&event, "root", None));
        assert!(is_reply(&reply, "root", None));
        assert!(!is_reply(&reply, "other", None));
        // ...except the one reply that is delivered as an event.
        assert!(is_reply(&result, "c1", Some("evaluationResult")));
        assert!(!is_reply(&result, "c1", None));
    }

    #[test]
    fn the_foreground_app_wins_over_the_shell() {
        // b2g's shell claims focus alongside whatever app is in front of it.
        let both = [
            target("http://launcher.localhost/index.html", true),
            target("chrome://b2g/content/shell.html", true),
        ];
        assert_eq!(foreground(&both).map(|t| t.url.as_str()),
                   Some("http://launcher.localhost/index.html"));

        // Only the shell focused: it is still the right answer.
        let shell_only = [
            target("http://notes.localhost/index.html", false),
            target("chrome://b2g/content/shell.html", true),
        ];
        assert_eq!(foreground(&shell_only).map(|t| t.url.as_str()),
                   Some("chrome://b2g/content/shell.html"));

        // Mid-transition nothing claims focus, and the first target beats
        // refusing to answer.
        let none = [target("http://notes.localhost/index.html", false)];
        assert!(foreground(&none).is_some());
        assert!(foreground(&[]).is_none());
    }

    #[test]
    fn a_target_names_itself_from_its_url_when_it_has_no_title() {
        // The hash of a KaiOS app URL can be a whole percent-encoded JSON
        // blob, which is not a name.
        let t = target("http://keyboard.localhost/index.html#%7B%22isFocus%22%3Afalse%7D", false);
        assert_eq!(t.label(), "keyboard.localhost");
        let mut titled = target("http://notes.localhost/index.html", false);
        titled.title = "Notes".into();
        assert_eq!(titled.label(), "Notes");
    }
}
