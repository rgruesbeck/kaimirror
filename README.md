# kaimirror — scrcpy-style screen mirroring for KaiOS 3.x

Live mirroring, recording, screenshots and key injection for a KaiOS device
over adb. Tested on two TCL flip phones running KaiOS 3.x: the **Flip 3**
(`T435SP` / `Gflip7_VZW`) and the **4056S** (`Gflip5_VZW`), which is the one
in the demo below.

scrcpy itself cannot work here: KaiOS runs Gecko (`b2g`) directly on the
Android HAL, with no Android framework and no SurfaceFlinger for scrcpy's
server to bind an encoder to. kaimirror captures through b2g's own
`gfxdebugger` socket instead, and injects keys on the raw `/dev/input`
nodes — see [docs/INTERNALS.md](docs/INTERNALS.md) for how that was found
and what it costs.

![kaimirror mirroring a TCL flip phone's launcher](docs/demo.gif)

*`kaimirror record --fps 10`, captured over adb at the phone's native
240x320 and driven by hand. Two cuts from one 29-second take, with the
game's load screen dropped.*

## Install

kaimirror runs on Linux and macOS. You need `adb` with root on the device
(KaiOS ships `userdebug` with `ro.debuggable=1`, so `adb root` works) and
`ffmpeg`/`ffplay` on the host — the viewer is an ffplay window. On macOS both
come from Homebrew:

```sh
brew install ffmpeg android-platform-tools
```

### Download a binary

The release is one file per platform. It **embeds the device pump**, so it
installs its own other half on first use — no NDK, no Rust, no second
download.

On Linux, that file is statically linked against musl, so it runs on any
x86-64 machine without matching a glibc:

```sh
curl -LO https://github.com/rgruesbeck/kaimirror/releases/latest/download/kaimirror-0.2.0-x86_64-linux.tar.gz
tar xzf kaimirror-0.2.0-x86_64-linux.tar.gz
./kaimirror --version
```

On macOS it is a universal binary, so the same download covers Apple silicon
and Intel:

```sh
curl -LO https://github.com/rgruesbeck/kaimirror/releases/latest/download/kaimirror-0.2.0-universal-macos.tar.gz
tar xzf kaimirror-0.2.0-universal-macos.tar.gz
./kaimirror --version
```

The binary is unsigned. `curl` leaves it alone, but a browser download picks
up a quarantine flag that Gatekeeper refuses to run; clear it with
`xattr -d com.apple.quarantine kaimirror`.

Put it on your `PATH` if you want it everywhere — `~/.local/bin` on Linux,
`/usr/local/bin` on macOS:

```sh
install -Dm755 kaimirror ~/.local/bin/kaimirror   # Linux
install -m755 kaimirror /usr/local/bin/kaimirror  # macOS
```

The release carries one `SHA256SUMS` covering both tarballs, so check the
line for the one you took:

```sh
curl -LO https://github.com/rgruesbeck/kaimirror/releases/latest/download/SHA256SUMS
grep x86_64-linux SHA256SUMS | sha256sum -c -        # Linux
grep universal-macos SHA256SUMS | shasum -a 256 -c - # macOS
```

### Build from source

Both halves are Rust, built together, on either host:

```sh
./build.sh                # device pump + host CLI
./build.sh --push         # ...and install the pump on the device
./build.sh --dist         # ...and link the host half for release, packaging dist/
```

Cross-compiling the device half needs the `armv7-linux-androideabi` target
and an NDK, found at `$ANDROID_NDK_HOME` or in the usual places — the
standalone `~/Android/android-ndk-*`, or Android Studio's
`~/Library/Android/sdk/ndk/*` on macOS. The device pump is the same ARM32
binary on both hosts; only the host half differs, and only for `--dist`,
which links against musl on Linux and `lipo`s the two Apple targets into one
universal binary on macOS:

```sh
rustup target add armv7-linux-androideabi
rustup target add x86_64-unknown-linux-musl                 # --dist on Linux
rustup target add aarch64-apple-darwin x86_64-apple-darwin  # --dist on macOS
```

The binary lands at `target/release/kaimirror`, or with `--dist` under
`target/x86_64-unknown-linux-musl/release/` (Linux) or at
`target/kaimirror-universal` (macOS), which is what a release ships alongside
the tarball and checksum it writes to `dist/`. Neither tarball can be built
on the other's host, so `--dist` leaves any tarball already in `dist/` alone
and rewrites `SHA256SUMS` over everything there: build on both machines,
collect the two files, and the checksum file covers both.

Building without the NDK works for editing the host half; the pump is then
absent and the binary says so rather than pushing a stub.

## Usage

`kaimirror --help` lists everything; each command has its own `--help`.

```sh
kaimirror view                     # live mirror window (2x, nearest-neighbour)
kaimirror view --scale 3
kaimirror record out.mp4           # Ctrl-C to finalize
kaimirror shot screen.png
kaimirror snapshot                 # ...or read the screen as text, not pixels
kaimirror key DOWN OK              # inject key presses
kaimirror type                     # terminal becomes the phone's keyboard
kaimirror type "hello world"       # ...or type one line and exit
kaimirror wake                     # tap power so the panel is lit
kaimirror shot --display 1 cover.png
kaimirror view --control           # drive the phone from the terminal
kaimirror view --format png        # PNG stream: 12x less bandwidth, slower
kaimirror record --fps 10 out.mp4  # lighter on the device, correctly timed
```

The capture options (`--display`, `--no-wake`, and for `view`/`record` also
`--format` and `--fps`; `--control` and `--scale` are `view` only) work on
either side of the command name, so the `kaimirror --display 1 shot cover.png`
form works too.

## Keys

`kaimirror key --list` prints them: digits `0`–`9`, `UP` `DOWN` `LEFT`
`RIGHT`, `OK`/`CENTER`, `BACK`, `MENU`, `HELP`, `CALL`/`SEND`, `STAR`,
`POUND`, `SOFT_LEFT`, `SOFT_RIGHT`, `POWER`, `VOLUMEUP`, `VOLUMEDOWN`,
`CAMERA`.

With `view --control`, terminal keystrokes are forwarded to the phone as you
type:

| Terminal | Device |
|---|---|
| arrows | UP / DOWN / LEFT / RIGHT |
| enter | OK |
| backspace | back (48 — also the delete key) |
| Ins | left soft key |
| PgUp | right soft key |
| Del | green / call |
| PgDn | red (116, on the power node) |
| digits, `*`, `#` | the keypad keys themselves |
| `m` | MENU |
| `-` / `+` | volume down / up |
| tab | switch between nav and text mode |
| Esc, Ctrl-C | quit |

A digit in nav mode is the phone's **keypad key**, which is a menu shortcut in
a list and a *multi-tap* in a text field — `5` there types `j`, not `5`, since
that key carries `jkl5`. Use text mode to type a digit: it taps four times to
reach the end of the cycle and lands a real `5`.

The phone keys with no keyboard equivalent sit on the **navigation cluster**
rather than on punctuation, so `,` `.` and `c` stay typeable and the cluster
keys — which mean nothing to a phone text field — keep working in text mode
too, the way the arrows do.

Two of those were settled by watching `getevent` while the handset's own keys
were pressed, rather than by reading a table. **Back is code 48**, the one the
keylayout calls `DEL`: back and delete are one physical key, deleting a
character where there is one and going back where there is not. Code 158 —
whose *kernel* name is `KEY_BACK`, which is how it got mistaken for one — is
the right soft key and nothing else. **Red is `KEY_POWER` on the power node**,
the same switch as the power button, so the phone decides from press length
whether it means "go back" or "blank the screen"; a tap backs out of an app.

### Typing

The keypad has no letters, so `--control` has a **text mode**: press `tab`,
and everything you type — lower and upper case, digits, and the printable
ASCII symbols — goes to the phone as typed characters. `tab` again (or `Esc`)
returns to nav mode, `Ctrl-C` still quits, and the arrow keys keep working in
both, moving the caret inside a field.

```
tab                    switch to text mode
Hello, world! $9.99    typed on the phone
tab                    back to nav mode
```

`kaimirror type` gives you the same text mode without a mirror window: the
terminal becomes the phone's keyboard from the first keystroke until Ctrl-C.
`kaimirror type "hello world"` types one line and exits, for scripts. Either
way, point the phone at a text field first — keystrokes go wherever the focus
is.

```sh
kaimirror type            # type here, it lands there; Ctrl-C to stop
```

Text goes onto the phone by the keypad's own **multi-tap** — `c` as three
presses of `2` — because that is the only input path a KaiOS device reads.
The keypad node cannot carry letters (`KEY_A` there *is* the left soft key),
and the obvious way around that, a virtual QWERTY keyboard on `/dev/uinput`,
was built and then removed: b2g enumerates `/dev/input` once at startup and
never rescans, so a keyboard created afterwards is never opened and every
keystroke on it is discarded. See
[docs/INTERNALS.md](docs/INTERNALS.md#the-keyboard-that-was-not-read) for what
that took to establish.

So typing is slow but real: about 9 seconds for `Hello world`, since two
characters on one key must wait out the ~1 s commit timeout and a change of
case costs a mode switch plus the ~1.8 s the mode banner spends covering the
field.

Case is the hard part, and the tool no longer guesses at it. KaiOS shows the
current input mode in the status bar (`Ab`, `ab`, `AB`, `123`, symbols), so
multi-tap **reads that indicator off a captured frame**, presses `#`, and
looks again until the phone says what was asked for. Nothing is counted or
assumed, which matters because all three assumptions a model would need are
wrong somewhere: the first `#` of a burst sometimes only raises the mode
banner, the cycle differs between builds, and the IME switches itself back to
sentence mode after a sentence ends.

That has two requirements worth knowing. The panel must be **lit** — a blanked
one does not stop compositing, it freezes, and every grab of it returns the
same plausible frame with a stopped clock, so the indicator never changes
however many times `#` is pressed. The tool lights it at every frame grab
(a walk can outlast the screen timeout on its own), and only if it is
actually dark, since power *toggles*. And something must be
**focused**: with no text field listening, `#` goes to the **dialer**, so a
switch types a phone number rather than changing mode. The tool gives up after
a single `#` that changes nothing rather than walking a whole cycle into one.
An *empty* focused field is fine — that was tested by hand, one `#` at a time,
and the cycle runs `Ab → ab → AB → 12 → symbols → Ab` with the field empty
throughout.

Not every character is reachable. The `1` key's cycle on the tested phones is
`. , ? ! 1 ; : / @ - + _ =`; quotes, apostrophe, brackets, `%`, `&` and `*`
live behind a symbols picker the tool does not drive, so they are **reported
rather than typed wrong**:

```
$ kaimirror type "Kai 42, ok? (yes)"
note: no way to type '(', ')' on this device -- skipped
```

See [docs/INTERNALS.md](docs/INTERNALS.md#typing-on-a-keypad) for how each of
those numbers was measured.

Type in the **terminal**, not the mirror window — ffplay keeps its own key
handling and offers no way to forward keystrokes. A keypress reaches the
screen in ~240 ms.

## Reading the screen as text

`kaimirror snapshot` prints what is on the screen as a tree of roles, names
and focus, instead of a picture of it:

```
$ kaimirror snapshot --target 2
target: Notes (http://notes.localhost/index.html#/list)
focus: li "Hello world 2000!"

application "Note Hello world 2000! Date editedDate createdTitleConfirmat"
  div "Note"
  li "Hello world 2000!" [FOCUSED]
    span "Hello world 2000!"
  button "New"
  button "Select"
  button "Options"
```

That is the form anything scripted wants — an agent especially, which
otherwise has to take a screenshot and work out where the focus ring is. It
costs **~50 ms** against the ~2.3 s of a `shot`, and it presses nothing to
take it.

It is the one command that never touches the device pump. b2g is Gecko, so
it asks Gecko: the phone's remote debugging socket is forwarded to a local
port and the app is asked for its own DOM. Bare `kaimirror snapshot` reads
the app in front; every running app is debuggable, though, and so is b2g's
own shell:

```sh
kaimirror snapshot                 # the app in front
kaimirror snapshot --list          # the debuggable targets, foreground marked
kaimirror snapshot --target 2      # read one by index, foreground or not
```

A backgrounded app still answers, which is how the Notes list above was read
while the phone was showing something else. What no snapshot can do is
invent a repaint: a sleeping phone stops updating, and its DOM goes stale the
same way its framebuffer does — a launcher clock four hours behind the device
clock is what that looks like. `snapshot` says so when the panel is dark;
`kaimirror wake` first for a live read.

## Notes

The mirror runs at **~29 fps**, capped by `--fps`; the cap is a choice, not a
limit, and exists so the rate can be declared honestly to a video container.

`view` and `record` stream uncompressed RGB565 by default, which costs ~12x
the bandwidth of PNG (~4.4 MB/s) but is far cheaper on the device. That is
fine over USB, painful over adb-on-wifi — use `--format png` there. `shot`
always captures PNG, where colour fidelity matters and throughput does not.

If the panel is blanked, captures come back solid black; every command taps
power first unless you pass `--no-wake`. `snapshot` is the exception: it
reads without pressing anything, and warns instead.

## Files

- `kaimirror/` — host-side CLI, frame-stream reader and control channel
- `kaipump/` — device-side frame pump, statically linked for ARM32; the host
  binary embeds a copy and installs it when needed
- `build.sh` — builds both halves on Linux or macOS; `--push` also installs
  the pump, `--dist` builds the release host binary (musl static, or a
  universal binary) and packages `dist/`
- `tools/devtools-probe.py` — the investigation behind `snapshot`, kept for
  what the subcommand deliberately does not do: raw protocol dumps, and
  Gecko's own accessibility walker
- `docs/INTERNALS.md` — how capture works, the protocol, and the numbers
