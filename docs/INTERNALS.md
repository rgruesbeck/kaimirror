# How kaimirror works

The reverse-engineering behind the mirror: why scrcpy's approach is
unavailable on KaiOS, what b2g offers instead, and what every number in
the README was measured against.  For using the tool, see the
[README](../README.md).

## Why scrcpy itself cannot work here

KaiOS runs Gecko (`b2g`) directly on the Android HAL. The device has an
Android 10 (SDK 29) base — `init`, binder, `dalvik.*` props, apexd — but
**no Android framework and no SurfaceFlinger**:

```
$ adb shell service list        # 35 services, no "SurfaceFlinger", no "window"
$ adb shell ps -A | grep -i surfaceflinger   # nothing; b2g composes via hwcomposer directly
```

scrcpy's server needs `android.jar`, `SurfaceControl.createDisplay()` and a
`MediaCodec` encoder bound to a virtual display. None of that exists.

Things that look promising but are dead ends:

| Approach | Result |
|---|---|
| `screencap` / `screenrecord` | Ship on the device but **hang forever** — they block on a SurfaceFlinger binder that never registers. |
| `/dev/graphics/fb0` | Exists (240x640 virtual, RGB565, stride 512) but `read()` returns `ENODEV` — MDSS composites through overlay planes, the fb is not the scanout source. |
| tmpfs for frame staging | `b2g` is SELinux-confined (`Enforcing`) and can only write under `/data/local/tmp`. |
| Gecko DevTools socket | `/data/local/firefox-debugger-socket` is live (`devtools.debugger.remote-enabled=true`, no connection prompt) — not a capture route, but the way to read the screen as *text* rather than pixels: see [Reading the screen as text](#reading-the-screen-as-text). |

## What actually works

`/system/bin/gfxdebugger` asks `b2g` over `/dev/socket/gfxdebugger-ipc` to dump
the composited display to a file — a PNG, or an uncompressed RGB565 frame if
the path ends in anything else:

```
gfxdebugger -c screencap [-d 0|1] -p /data/local/tmp/frame.png
gfxdebugger -c screencap [-d 0|1] -p /data/local/tmp/frame.raw
```

`-d 0` = primary 240x320 panel, `-d 1` = 128x128 cover display (`st7735s`).
Input injection goes through `sendevent` on the raw `/dev/input` nodes.
Together these are the capture + control primitives scrcpy would otherwise get
from the framework.

`gfxdebugger` is only a thin client for that socket, though, and starting it
costs more than the capture does. The device-side pump speaks the socket
directly instead — see [the protocol](#the-gfxdebugger-ipc-protocol).

## Performance

**~29 fps streaming.** That is a usable live mirror rather than the slideshow
this started as. The device-side pump is no longer the bottleneck — b2g's CPU
budget is — so the rate is capped by choice, not by capability.

| Pump | fps | notes |
|---|---|---|
| `kaipump`, IPC, capped at 30 (default) | **28.7** | b2g at 16% CPU |
| `kaipump`, IPC, uncapped | 66 | b2g at 74% CPU, 22 MB/s to flash |
| `kaipump`, `exec` backend | ~20 | spawns `gfxdebugger` per frame |
| the original shell pump (removed) | 6.1 | four forks per frame |

Uncapped is measured but not recommended: it re-captures a screen that is not
changing that fast, and pays most of b2g's CPU and 22 MB/s of flash writes to
do it.

`--fps` is one knob: it caps what the device captures **and** is what the sink
is told. Those used to be separate (`--max-fps` and `--fps`), which meant any
gap between them silently produced a wrong-speed video — 6s recorded at 10 fps
but declared as 30 came out as a 1.7s file playing at 3.5x. There is no way to
express that mismatch now.

Uncapped is no longer reachable from the CLI, because a variable rate cannot
be declared honestly to a container. The pump still does it directly, for
benchmarking:

```sh
adb exec-out /data/local/tmp/kaipump 5000 0 raw 1 150 ipc 0   # 0 = uncapped
```

There is also no `--poll-delay` any more. It set how long the device-side
guard slept between checks for a complete frame; swept from 200us to 5ms it
made no measurable difference, because the pump spends its time waiting on b2g
rather than on the poll, and going much lower only competes for CPU with the
process producing the frames. It is a constant now.

Other paths, all with the IPC pump:

| Path | fps | notes |
|---|---|---|
| `--display 1` (128x128 cover), uncapped | 123 | geometry read from the frame header |
| `--format png` | 12.4 | encode-bound: the cap makes no difference |

### Where the time actually goes

The bottleneck was **process startup**, not capture, not encoding, not flash.
Every external command on this device costs ~34 ms:

| | ms/call |
|---|---|
| shell builtin (`true`) | 1.4 |
| `stat` / `od` / `tail` (toybox) | 33.7 / 35.8 / 33.9 |
| `gfxdebugger`, usage only (no capture) | 34.5 |
| `gfxdebugger -c screencap` | 75.0 |

The shell pump this replaced spent three or four of those per frame — ~136 ms of overhead —
and `gfxdebugger` itself is a process like any other. Replacing the loop with
one long-lived binary that speaks b2g's socket directly removes every one of
them.

**An earlier version of this section claimed the remaining 41 ms of that 75 ms
was b2g's capture, and predicted a ~24 fps ceiling. That was wrong.** The IPC
pump sustains 150 captures/second with no process involved, so b2g composites
in single-digit milliseconds; essentially all of the 75 ms was `gfxdebugger`
starting up and dynamically linking `libbinder`, `libutils` and `libc++`. The
ceiling was never the capture.

### The gfxdebugger IPC protocol

`gfxdebugger` is a thin client for `/dev/socket/gfxdebugger-ipc`. The whole
exchange is one `connect()` and one `write()` — it never reads a reply, it
just closes:

```
u32   0x04          constant
u32   0x01          constant
u32   0x02          command: screencap
u32   display       0 = primary, 1 = cover
cstr  path          NUL-terminated, zero-padded to a 4-byte boundary
```

Recovered by tracing the real binary (`strace` ships on the device), not by
disassembling it, and each field pinned by varying the inputs: `-d 1` flips
word 4 alone, and a 28-character path yields a 48-byte message against 40 for
a 21-character one, matching the padding rule exactly.

The "reply is a 4-byte parcel" claim in earlier notes is not something the
client ever waits on — file completeness is the only real signal.

### The two output formats

b2g picks the format from the file extension, and it offers exactly two
choices: a path ending in `.png` gets a PNG, and **any** other extension gets
an uncompressed RGB565 dump. There is no JPEG encoder anywhere on this path —
`.jpg`, `.jpeg`, `.bmp` and `.webp` all produce the same raw bytes.

```
.png                        -> 89504e47   PNG
.jpg .jpeg .bmp .webp .raw  -> f0000000   16-byte header + w*h*2

header: f0 00 00 00  40 01 00 00  04 00 00 00  01 00 00 00
           w=240        h=320       format=4     planes=1
```

Streaming defaults to raw because it skips the PNG encode entirely, which is
now the dominant cost on that path — PNG sits at 12.4 fps whether capped or
not, while raw runs to 66. The header also doubles as a sync marker and
reports geometry, so the host splits frames for free and the cover display
needs no special casing.

The costs: **~12x the bandwidth** (~4.4 MB/s at 29 fps, fine over USB where
adb does 9, painful over adb-on-wifi — use `--format png` there), and RGB565
colour. Raw is *not* pixel-identical to the PNG: only 0.2% of pixels match
exactly, mean error ~0.5–6 per channel, PSNR 35.5 dB. Indistinguishable on
flat UI, but `shot` always asks for PNG, where fidelity matters and throughput
does not.

### The race this design defends against

b2g writes the frame file **asynchronously**. The capture request returns as
soon as it is accepted, and measured back to back, **23 of 40 PNG frames were
still incomplete** at that point. So the pump waits for the frame to be
complete — `IEND` for PNG, the expected size for raw — then renames it aside
before sending, so a late writer can never truncate the copy in flight.

This guard used to cost a 34 ms fork and was worth making optional. It is now
a single `stat` syscall costing ~9%, so it is unconditional and
`--no-device-guard` is gone: skipping it produced intermittent duplicate
frames (4 in one 150-frame run, 0 in the next) for no meaningful gain.

### Verifying the frame rate is real

A pump that re-ships a staged frame would inflate its rate while showing a
stale screen, and on a static UI that is invisible in the output. So the
numbers above were checked rather than trusted: the pump counts captures
against frames shipped (`/data/local/tmp/kaipump.stats`), every frame is
verified to start on a header with no trailing remainder, and IPC output is
byte-identical to `exec` output on a static screen.

### Driving the device

`--control` forwards terminal keystrokes to the phone while the mirror
runs (the map is in the README).

Keys go over a **persistent channel**, which is the whole point: a one-shot
`kaimirror key` costs ~140 ms, of which ~104 ms is just the `adb shell` round
trip and the process spawns. Holding one `adb shell` open for the session
turns that into a ~0.3 ms pipe write, and the pump writes `struct input_event`
straight to `/dev/input/eventN` instead of forking `sendevent` twice per key.

It needs its own connection rather than riding the frame stream: the stream
uses `adb exec-out`, because `adb shell` mangles binary output, and
**`exec-out` does not forward stdin at all**.

Note that keystrokes are read from the *terminal*, not the mirror window —
ffplay keeps its own key handling and offers no way to forward them.

End-to-end, a keypress reaches the screen in **~240 ms**, and that is now
dominated by b2g's own repaint plus the capture pipeline rather than by the
transport; varying the key hold between 5 ms and 50 ms does not move it
outside the noise.

### Typing on a keypad

A flip phone's keypad cannot carry letters, and not for want of scancodes:
`event1` *has* `KEY_A` (30), but the device's keylayout binds it to the **left
soft key**. Every letter code on that node is already spoken for. Injecting
"a" there presses a soft key.

So typing goes on the keypad the phone already has, by multi-tap — the way
its own user types. The section after this one is how; this one is the
obvious alternative, and why it is not here.

#### The keyboard that was not read

A second input device would have carried letters exactly: the pump created
one through `/dev/uinput` — a plain US keyboard named `kaimirror-keyboard`,
which b2g has no `.kl` for and therefore reads through Android's `Generic.kl`
and `Generic.kcm`, shift included. It was built, measured on a T435SP, and
**removed**. Every piece worked except the one that matters:

- `/dev/uinput` is there, and the SELinux domain `adb root` lands in can open
  it. Creating a real keyboard succeeds.
- **b2g never sees it.** Gecko's `EventHub` enumerates `/dev/input` once at
  startup and holds *no inotify watch* — `/proc/<b2g>/fd` has no inotify
  descriptor at all — so a device created afterwards is never opened. Its
  keystrokes go nowhere.
- Given the keyboard *before* it starts (create it, then restart b2g),
  EventHub picks it up exactly as designed:
  `New device: ... name='kaimirror-keyboard', keyLayout='/system/usr/keylayout/Generic.kl', keyCharacterMap='/system/usr/keychars/Generic.kcm'`,
  and Gecko then dispatches real letters —
  `KeyEventDispatcher::Dispatch ... mDOMKeyCode = :65 ... scanCode = :30` for
  `KEY_A`. The keylayout half of the idea was sound.
- Even then, **nothing lands in a text field.** KaiOS's IME owns text entry,
  and a physical keyboard's characters do not become text the way keypad
  presses do.

That last point is what settles it: even the version of this that requires
restarting b2g does not type. Nothing short of a build change would make it
work, so the `uinput` code, the scancode table, the `u CODE SHIFT` line in
the control protocol and the probe that chose between the two are all gone.
What is left is one typing path, which is also the one that works.

### Multi-tap

"c" as three presses of `2` inside the tap timeout, and a QWERTY keyboard on
the host end of it: the user types a letter, and the host works out which key
it sits on and how many taps in.

It runs **entirely on the host**. Multi-tap is nothing but ordinary keypad
presses with timing between them, so it reuses the existing `NODE CODE`
channel and the device half is unchanged. Host-side sleeps are accurate
enough for this because the pipe write is ~0.3 ms against a tap timeout near
700 ms.

Every number below was measured on the phone, and every one of them was wrong
in the first draft. `hello` came out as `hEko`, which is what a plan looks
like when three separate assumptions are each off by a little:

| What | Assumed | Measured |
|---|---|---|
| `#` presses per mode change | one per mode | **one wake press, then one per mode** — the first press of a burst only raises the banner |
| The mode ring | `Abc → abc → ABC → 123` | **`abc → ABC → 123 → symbols → abc`**; `Abc` is a start state the first switch leaves and nothing returns to |
| After a switch | type immediately | **wait ~1.8 s** — the mode banner sits over the field and *eats* keys rather than queueing them |
| Gap between taps of one key | 80 ms | **≥120 ms**; at 80 ms the IME drops taps, and three taps land as the second letter |
| Gap that commits a character | 900 ms | **~1 s** — 900 ms merges two taps into one character, 1200 ms does not |
| The `1` key's cycle | 33 guessed symbols | **13 measured**: `. , ? ! 1 ; : / @ - + _ =`, wrapping after thirteen |

The last one is the one that would have been quietly wrong forever: quotes,
apostrophe, brackets, `%`, `&` and `*` are not on that key at all — they live
behind the symbols mode, which is a picker rather than a cycle. They are now
reported as untypeable instead of typed as whatever sits at that position.

With those corrected, `Kai 42, ok? (yes)` types as `Kai 42, ok? yes` with the
parentheses named in a note, and `Hello world` types clean. It costs about
9 s for 11 characters — the waits, not the taps.

The hard part is not the tapping, it is the IME's mode -- and after three
failed attempts to model it, it is now **read off the screen** instead.

Modelling it failed because every fact a model needs turned out to be false
somewhere:

- The first `#` of a burst sometimes only raises the mode banner and changes
  nothing. Four presses on a T435SP move three modes.
- The cycle is not the same everywhere, and it has a fifth entry (symbols)
  that the first version did not know about.
- **The IME moves on its own.** Type `. ` and the indicator returns to `Ab`
  without anyone pressing anything, so a mode that was right two characters
  ago is wrong now.

So `multitap` no longer counts presses. A plan emits `Step::Mode(Case)` -- a
request -- and the executor satisfies it by grabbing a frame, reading the
indicator KaiOS already draws in the status bar, pressing `#`, and looking
again. `imemode` calibrates once by walking the cycle and keeping each mode's
pixels, then matches later readings against them; it finds lowercase among
them by the one property that separates it from every other mode, which is
that its first character is x-height and so starts *lower* than its second.
No glyph recognition, no font assumptions, and a build that reorders its cycle
changes nothing.

Four device behaviours make this fussier than it sounds, all found the hard
way:

- **A blanked panel still composites — and freezes.** It does not go black:
  every grab returns the *same* frame the panel last showed, field and status
  bar and a clock stopped at the minute it blanked. That reads as a perfectly
  good indicator which simply never changes, so the walk presses `#` until it
  gives up.

  Lighting it once before each switch is **not enough**, and that took a second
  round to learn. A walk is a press-and-look loop that can run the better part
  of a minute — calibration, then up to a full lap of the ring, each round a
  frame grab plus the banner wait — and the phone's screen timeout fires
  *inside* it. So the panel is lit at the frame grab, the one place a picture
  is actually taken, and only if it is dark, because power *toggles*.

  The signature is unmistakable once seen, which is why the walk now reports
  what it read on the way: `saw Lower -> Lower -> Lower -> Lower -> Lower ->
  Lower` is a frozen panel, not a mode cycle that will not turn.
- **`#` reaches the dialer when nothing is focused** — which is how one walk
  typed `2###########` into a phone number with CALL one key away. The walk
  stops after a *single* press that changes nothing, rather than after twelve;
  two dead presses is the whole budget, and a session that has seen one stops
  trying until the user navigates.

  It was believed for a while that an *empty* field did this too, and a switch
  therefore happened inside a throwaway character deleted afterwards. **That
  was wrong, and the throwaway did real damage.** Pressing `#` by hand into an
  empty focused Note, reading the indicator after each press, walks the whole
  cycle — `Ab → ab → AB → 12 → symbols → Ab` — with the field empty
  throughout and no dialer anywhere. The original diagnosis had come from runs
  where *no field was focused at all*, and an empty field was blamed for it.

  What the throwaway cost: its `DEL` was unconditional, so wherever the
  character had already gone it deleted the user's own text instead, and on an
  empty field it is not a delete at all — the Note editor reads it as "go
  back" and closes. It emptied an SMS draft and lost the focus in the middle
  of nearly every typing run before it was traced. Both keypresses are gone,
  and a mode switch is two presses cheaper for it.
- **The status bar is not always dark.** KaiOS draws it dark green over
  Contacts and cream over the Note editor, so thresholding on "text is the
  brightest thing in the bar" reads a light bar as solid ink and throws it
  away as unreadable. The cut is the midpoint of the crop's own range, and
  the polarity is chosen by which class is the *minority* — glyphs are, on
  either kind of bar. A crop whose range is under 60 (of ~250) has no text
  in it at all and says so, which is what keeps the flat mode banner from
  being recorded as a mode of its own.
- **The banner outlasts the wait.** The pause after each `#` was 900 ms
  against a banner that measures ~1.8 s, so every read landed on a blank bar
  and retried: a single mode switch cost tens of seconds of frame grabs.
  Waiting 1.9 s up front costs one second and saves six. Calibration then
  runs in ~7 s on a T435SP, end to end.

One more, and it is about the *terminal* rather than the phone: a failed mode
switch used to drop the session back into nav mode. Everything the user had
already typed was still sitting in the terminal buffer, and nav mode replays
it as **navigation** -- `Hi 42.` typed `4`, `2` and the right soft key on the
phone after the case failed. A typing error now throws the queue away and
stays in text mode.

Two tables (`KEYPAD` and `PUNCT`) are the whole device-specific
surface — everything else follows from them.

#### Testing it without a phone

A simulator carries the measured device — the commit timeout at the slower of
the two phones, the 120 ms tap floor, the sentence case the IME asserts for
itself, and a mode switch as the three real keypresses it is (`2`, the `#`
walk, `DEL`) rather than as a free assertion. Plans are run through it and
read back into text, and the text has to match what was asked for:

- **every ordered pair** of the 75 typeable characters — 5,625 of them;
- **every ordered triple** — 421,875, which is the shortest shape that can
  catch a mode switch landing between two characters that share a key;
- **2,000 seeded random lines** of up to 200 characters over the whole
  alphabet, and 4,000 more over a cramped one (`abcABCmno. `) where key
  collisions and case flips are the norm rather than the exception;
- runs of one key, a settle in the middle (the user leaving text mode and
  coming back), and a handful of realistic lines.

Plus a price: `hello world` must cost exactly one mode switch, `hi. there
friend` exactly two. A plan that is correct but asks the screen a question
per character is a plan nobody would wait for.

Modelling the switch as something that happens on the device, rather than as
a free assertion, is what earned the simulator its keep. It first found the
bug in the throwaway era: that `2` was a tap like any other, so a character
still mid-cycle on key 2 was *extended* by it and the `DEL` then deleted both
— `aB` lost its `a`.

The throwaway is gone, and the same hazard survived it in a subtler form. A
switch presses no keypad key at all now, only `#`, so it commits nothing by
being a keypress — it commits only by taking time. A walk of several `#`
presses takes seconds and commits whatever was pending on its own, but the
look-first check can return after a single frame grab without pressing
anything, and that is not long enough. So the model prices a switch at its
*fastest*, and a plan still has to emit a wait before every mode request.
Taking that wait back out fails nine tests.

What none of this can test is whether the model matches the phone; `KEYPAD`
and `PUNCT`, and the four measured timings, are where a disagreement would
live.

The cost is the wait, not the tapping: on the phone `Hello world` takes ~9 s.
A same-key pair costs 1.4 s and a case change ~2 s on top of its presses.

### Reading the screen as text

An agent driving the phone through kaimirror has to do it by looking at
pictures: `shot`, then read the PNG, then guess where the focus ring is.
That is the slowest and least reliable way to answer "what is on the
screen", and it is why `imemode` ended up doing pixel forensics on a
40-pixel crop of the status bar just to learn which input mode the IME is
in. What an agent wants instead is what agent-browser's `snapshot` returns:
a text tree of roles, names and focus, cheap enough to take before every
action.

**That works on these devices.** The homescreen reads back as

```
url: http://launcher.localhost/index.html#mainView
focus: div "PMSundayAug 30"

  div "PMSundayAug 30" [FOCUSED]
    menuitem "homescreen 2:59 PM, Sunday August 30"
  button "Notifications"
  button "Contacts"
```

against a screenshot of the same moment showing 2:59 PM, SUNDAY AUG 30 and
the two soft keys, and the app grid reads back with `menuitem "E-Mail"
[FOCUSED]` for the icon the screen has selected. It costs **~50 ms** where
`shot` costs **2.3 s**, and the whole exchange is host-side: `kaipump` is
untouched, and nothing new goes on the phone.

`tools/devtools-probe.py` is that snapshot, and the investigation it came
from.

#### The route

b2g is Gecko, and the dead-end table above notes that Gecko's remote
debugging server is listening:

```
$ adb shell ls -l /data/local/firefox-debugger-socket
srw-rw-rw- 1 root root 0 /data/local/firefox-debugger-socket
```

`adb forward tcp:6080 localfilesystem:/data/local/firefox-debugger-socket`
maps it to a host port and the rest is JSON over TCP. Framing is
`<byte-length>:<json>`, the server greets first, and events (`frameUpdate`,
`tabListChanged`, `consoleAPICall`) arrive interleaved with replies — an
event carries a `type` and a reply does not, which is the whole rule for
telling them apart. The root actor's traits place this at a modern devtools
server, `allowChromeProcess` included:

```
allowChromeProcess, bulk, heapSnapshots, networkMonitor, perfActorVersion,
sources, storageInspector, watchpoints, workerConsoleApiMessagesDispatched...
```

There is **no `webapps` actor** — the Firefox OS route through
`getWebapps` / `getAppActor` is gone. It is not needed, because `listTabs`
already returns one target per running app, each with a console, an
inspector and an accessibility actor:

```
http://launcher.localhost/index.html#appList     Launcher
http://callscreen.localhost/index.html#&timestamp=...
http://notes.localhost/index.html#/list          Notes
http://camera.localhost/index.html#/             Camera
http://keyboard.localhost/index.html#{"isFocus":false,...}
about:blank
```

The system UI is not in that list, and it is not an iframe inside any of
them either. It is the **parent process** — `chrome://b2g/content/shell.html`,
reached through `getProcess` with id 0, which is where a chrome-privileged
console lives.

Three details cost time, and none of them announce themselves:

- **A target must be attached before its console answers.** An
  `evaluateJSAsync` sent to a freshly-listed target's `consoleActor` gets
  no reply and no error — it simply hangs until the socket times out. One
  `attach` on the target actor first, and the same request answers in
  milliseconds.
- **`document.hidden` lies.** The launcher reports `hidden=true` while it is
  the thing on the panel. `document.hasFocus()` is the honest signal, with
  one wrinkle: b2g's shell claims focus as well, so a focused *app* has to
  win over `chrome://`.
- **A reply larger than ~10k is a `longString` grip**, an actor to call
  `substring` on rather than the string itself. A snapshot of a busy screen
  crosses that line.

#### Which of the three routes

| Route | Verdict |
|---|---|
| `evaluateJSAsync` on the console actor | **This one.** One round trip returns the whole tree in whatever shape we choose. ~50 ms. |
| accessibility walker | Works, but only after starting an engine that is off, and it is one round trip *per node*. |
| inspector's DOM walker | Same per-node round trips, without the computed names. Not pursued. |

The accessibility route is the closest analogue to a browser snapshot, so it
was taken as far as it goes. Gecko's a11y engine **is** compiled into this
build — `@mozilla.org/accessibilityService;1` is registered and
`accessibility.force_disabled` sits at 0 — but the service is not running,
and the devtools accessibility actor here accepts only

```
getTraits, bootstrap, getWalker, getSimulator
```

with no `enable`: the `ParentAccessibilityActor` that Firefox's own panel
uses to turn the engine on does not exist in this server. Starting it from
the chrome console does work —

```js
Cc['@mozilla.org/accessibilityService;1'].getService(Ci.nsIAccessibilityService)
// Services.appinfo.accessibilityEnabled -> true
```

— and the walker then answers: `document "Launcher"` → `section` → seven
children, a request per node. (`accessibility.force_disabled = 1` shuts it
back down; the probe restores the pref it found.)

So the engine is reachable, and it is still the wrong tool: turning on
a device-wide a11y engine and then paying a round trip per node, to get
*less* than a script that already returns everything in one. What the
script can carry and a generic a11y tree cannot is the part that matters
here — which element has focus (d-pad navigation is the whole interaction
model), what the two soft keys currently say, and position within a list.

#### What it would change

`imemode` reads the input-mode indicator by thresholding a 40-pixel crop of
the status bar, after a ~7 s calibration walk, defended against four device
behaviours that each broke it once. That exists only because the mode is
currently readable *only* as pixels. Reading it as text would delete the
whole apparatus.

The shape it would take is one subcommand alongside `shot`:

```sh
kaimirror snapshot            # text tree of the foreground app
kaimirror snapshot --target N # a specific target, or b2g's own shell
```

The one loose end in the prototype is the visibility test: elements are
filtered by a viewport-intersection check, and on a transform-scrolled grid
that keeps a few rows more than the panel actually shows.

### Dead ends, all tested on the device

- **JPEG**: does not exist on this path. `gfxdebugger` has no format strings
  at all; b2g gives you PNG or raw.
- **Let b2g write straight into a FIFO**, skipping the file: rejected
  synchronously (`result: 1`, zero bytes). b2g requires a regular file.
- **Stage frames on tmpfs** instead of flash: `mount` is denied even as root
  under SELinux, and it would not have helped — flash was never the cost.

### The shell pump that started this

The first version of the pump was a shell loop on the device, and it is gone:
at 6 fps against 29 it was not worth carrying a second implementation of the
frame protocol to keep it working. `kaipump` keeps an `exec` backend that
spawns `gfxdebugger` per frame, though, so the claim that speaking the socket
directly is what makes it fast stays falsifiable.
