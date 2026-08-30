//! Argument parsing.
//!
//! Hand-rolled rather than pulled from a crate, because the one behaviour
//! that matters here is awkward to get from a derive-style parser: capture
//! options work on *either side* of the subcommand name, so
//! `kaimirror --display 1 shot x.png` and `kaimirror shot --display 1 x.png`
//! are the same command.  Walking the arguments once, accepting options
//! wherever they appear, gets that for free.

use crate::keys;

/// Read from Cargo.toml rather than repeated here: a hardcoded copy drifts
/// the moment the package version moves, and quietly ships a binary that
/// misreports itself.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_FPS: u32 = 30;

pub struct Args {
    pub cmd: Option<String>,
    pub display: u32,
    pub no_wake: bool,
    pub format: String,
    pub fps: u32,
    pub scale: f64,
    pub control: bool,
    pub list: bool,
    pub positionals: Vec<String>,
}

const COMMANDS: &[&str] = &["view", "record", "shot", "key", "type", "mode", "wake"];

fn bad(msg: &str) -> ! {
    eprintln!("error: {msg}");
    eprintln!("try `kaimirror --help`");
    std::process::exit(2);
}

pub fn parse(argv: Vec<String>) -> Args {
    let mut a = Args {
        cmd: None, display: 0, no_wake: false, format: "raw".into(),
        fps: DEFAULT_FPS, scale: 2.0, control: false, list: false,
        positionals: Vec::new(),
    };
    let mut it = argv.into_iter().peekable();

    while let Some(arg) = it.next() {
        // Accept both `--opt value` and `--opt=value`.
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |what: &str| -> String {
            inline.clone().or_else(|| it.next())
                .unwrap_or_else(|| bad(&format!("{what} needs a value")))
        };

        match name.as_str() {
            "-h" | "--help" => {
                print!("{}", help(a.cmd.as_deref()));
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("kaimirror {VERSION}");
                std::process::exit(0);
            }
            "--display" => {
                a.display = match value("--display").as_str() {
                    "0" => 0,
                    "1" => 1,
                    v => bad(&format!("--display must be 0 or 1, got {v:?}")),
                }
            }
            "--no-wake" => a.no_wake = true,
            "--control" => a.control = true,
            "--list" => a.list = true,
            "--format" => {
                a.format = match value("--format").as_str() {
                    v @ ("raw" | "png") => v.to_string(),
                    v => bad(&format!("--format must be raw or png, got {v:?}")),
                }
            }
            "--fps" => {
                let v = value("--fps");
                a.fps = v.parse().ok().filter(|&n| n > 0)
                    .unwrap_or_else(|| bad(&format!("--fps must be a positive integer, got {v:?}")));
            }
            "--scale" => {
                let v = value("--scale");
                a.scale = v.parse().ok().filter(|&f: &f64| f > 0.0)
                    .unwrap_or_else(|| bad(&format!("--scale must be a positive number, got {v:?}")));
            }
            other if other.starts_with('-') && other.len() > 1 => {
                bad(&format!("unknown option {other:?}"))
            }
            other => {
                if a.cmd.is_none() && COMMANDS.contains(&other) {
                    a.cmd = Some(other.to_string());
                } else {
                    a.positionals.push(other.to_string());
                }
            }
        }
    }
    a
}

const CAPTURE_OPTS: &str = "\
capture options (usable on either side of the command):
  --display {0,1}    0=primary panel, 1=external/cover display (default: 0);
                     geometry is read from the frame
  --no-wake          do not tap power before capturing; a blanked panel
                     captures as solid black
";

const STREAM_OPTS: &str = "\
  --format {raw,png} raw RGB565 (default, content-independent rate) or png
                     (12x less bandwidth, slower)
  --fps N            frame rate: caps the device-side capture and is what the
                     sink is told (default: 30).  These are one knob because
                     any gap between them silently produces a wrong-speed
                     video
";

const TYPING_NOTE: &str = "\
typing (`type`, and text mode in `view --control`):
  Text goes onto the phone's own keypad by multi-tap -- 2=abc, 7=pqrs, and
  so on -- because that is the only input path a KaiOS device actually
  reads.  Case is not declared and not counted: it is read off the phone's
  own input-mode indicator in a captured frame, so the panel must be lit
  and a text field focused before typing starts.
";

pub fn help(cmd: Option<&str>) -> String {
    match cmd {
        Some("view") => format!(
            "usage: kaimirror view [options]\n\n\
             Live mirror in an ffplay window.\n\n\
             options:\n\
             \x20 --control          forward terminal keystrokes to the device over a\n\
             \x20                    persistent channel (~0.3ms per key against ~140ms for\n\
             \x20                    `kaimirror key`); needs a TTY.  Type in the terminal:\n\
             \x20                    the mirror window keeps its own keystrokes.  TAB\n\
             \x20                    switches to text mode, where letters, digits and\n\
             \x20                    symbols type on the phone; tab or Esc switches\n\
             \x20                    back\n\
             \x20 --scale F          window magnification, nearest-neighbour (default: 2)\n\
             {STREAM_OPTS}\n{TYPING_NOTE}\n{CAPTURE_OPTS}"),
        Some("record") => format!(
            "usage: kaimirror record [options] OUTPUT\n\n\
             Record the screen to a video file.  Ctrl-C finalizes it; an\n\
             existing file is overwritten.  The extension picks the container.\n\n\
             options:\n{STREAM_OPTS}\n{CAPTURE_OPTS}"),
        Some("shot") => format!(
            "usage: kaimirror shot [options] [OUTPUT]\n\n\
             Save a single screenshot (default: kaishot.png).  Always captured\n\
             as PNG: one frame is not worth optimising, and PNG keeps more\n\
             colour precision than the RGB565 raw dump.\n\n\
             options:\n{CAPTURE_OPTS}"),
        Some("key") => {
            let names = keys::names();
            let listed = names.chunks(8).map(|c| format!("  {}", c.join(", ")))
                .collect::<Vec<_>>().join("\n");
            format!(
                "usage: kaimirror key [--list] KEY [KEY ...]\n\n\
                 Inject key presses on the raw /dev/input nodes.\n\n\
                 options:\n\x20 --list             print the known key names and exit\n\n\
                 key names:\n{listed}\n")
        }
        Some("type") => format!(
            "usage: kaimirror type [TEXT ...]\n\n\
             Type on the device from a QWERTY keyboard: letters in both cases,\n\
             digits, space, enter and the punctuation the keypad can reach.\n\n\
             With no TEXT, the terminal becomes the phone's keyboard: what you\n\
             type here lands there, until Ctrl-C.  TAB switches to nav mode\n\
             (arrows, m=menu, q=quit) and back.  With TEXT, that text is typed\n\
             once and the command exits; arguments are joined with spaces.\n\n\
             Focus a text field on the phone first, and leave the panel lit --\n\
             the case comes off the phone's own input-mode indicator, read from\n\
             a captured frame.  Characters the keypad has no tap path for --\n\
             quotes, brackets, `%`, `&` and the rest of the symbol picker --\n\
             are reported and skipped rather than typed as something else.\n\n\
             examples:\n\
             \x20 kaimirror type                     type live from the terminal\n\
             \x20 kaimirror type \"hello world\"\n\
             \x20 kaimirror type 'Password1.'\n\n\
             {TYPING_NOTE}"),
        Some("wake") => "usage: kaimirror wake\n\n\
             Tap power to light the panel.  Power *toggles*: if the panel was\n\
             already lit, this turns it off.\n".to_string(),
        _ => format!(
            "usage: kaimirror [options] COMMAND [args]\n\n\
             scrcpy-style screen mirroring for KaiOS 3.x devices.\n\n\
             KaiOS runs Gecko (b2g) directly on the Android HAL with no\n\
             SurfaceFlinger and no Android framework, so scrcpy itself cannot\n\
             work.  What KaiOS does provide is b2g's own gfxdebugger-ipc\n\
             socket, which dumps the composited display to a file; the\n\
             device-side pump speaks it directly.\n\n\
             commands:\n\
             \x20 view               live mirror in an ffplay window\n\
             \x20 record OUTPUT      record the screen to a video file\n\
             \x20 shot [OUTPUT]      save a single screenshot\n\
             \x20 key KEY [KEY ...]  inject key presses (key --list for names)\n\
             \x20 type [TEXT]        type from the terminal, or type TEXT once\n\
             \x20 wake               tap power to light the panel\n\n\
             {CAPTURE_OPTS}\n\
             {STREAM_OPTS}\n\
             other options:\n\
             \x20 -h, --help         show help, or help for COMMAND\n\
             \x20 -V, --version      print the version\n\n\
             examples:\n\
             \x20 kaimirror view                     live mirror window (2x)\n\
             \x20 kaimirror view --scale 3\n\
             \x20 kaimirror view --control           drive the phone from the terminal\n\
             \x20 kaimirror record out.mp4           Ctrl-C to finalize\n\
             \x20 kaimirror record --fps 10 out.mp4  lighter on the device\n\
             \x20 kaimirror shot screen.png\n\
             \x20 kaimirror shot --display 1 cover.png\n\
             \x20 kaimirror key DOWN OK\n\
             \x20 kaimirror type                     type live from the terminal\n\
             \x20 kaimirror type \"hello world\"       type one line and exit\n"),
    }
}
