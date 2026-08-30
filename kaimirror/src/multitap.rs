//! Typing on the keypad itself, by multi-tap.
//!
//! This is how text gets onto the phone, and the only way that does.  The
//! keypad cannot carry letter scancodes -- `matrix-keypad.kl` maps `KEY_A`
//! (30) to a soft key -- and a virtual `/dev/uinput` keyboard, which would
//! have carried them, is not read: b2g's EventHub enumerates `/dev/input`
//! once at startup and never rescans, so a device created later is never
//! opened and every keystroke on it goes into the void.  That path was tried
//! on hardware and removed.  What is left is what the phone's own user has:
//! 2=abc, 7=pqrs, and the timing rules below.
//!
//! Multi-tap is guesswork by nature, and the guessing was all about the
//! IME's mode -- which cannot be computed, only observed.  Three separate
//! attempts to model it each failed on hardware: press counts are off by a
//! wake press, the ring differs between builds, and the IME moves on its own
//! (type `. ` and it returns to sentence mode).  So the model is gone.  A
//! plan now *asks* for a case with `Step::Mode`, and the executor reaches it
//! by reading the phone's own mode indicator -- see `imemode`.
//!
//! What stays here is everything the host really can decide: which key a
//! character sits on, how many taps in, and the waits that keep two
//! characters on one key from merging.  Digits still ride the last tap of
//! their letter key rather than switching to `123`, because a mode change is
//! now a screen round trip and worth avoiding.

use std::time::Duration;

use crate::imemode::Case;

/// Milliseconds between taps of the same key.
///
/// Bounded on both sides, and both bounds were measured on a T435SP rather
/// than guessed: below ~120ms the IME drops taps outright -- three taps at
/// 80ms produce the *second* letter, which is how `Hello` first came out as
/// `Geko` -- and it has to stay well under the commit timeout below.
const TAP_MS: u64 = 150;
/// Milliseconds to wait for the character to commit when the next one is on
/// the same key.
///
/// This is per *device*, and the two tested disagree by half a second: on a
/// T435SP 900ms merges and 1200ms commits, while on a 4056S 1400ms still
/// merges -- `remove` typed as `re6ve`, m and o being one key -- and 1700ms
/// commits.  So the value covers the slower of them with margin, because too
/// short does not lose one character, it merges two and shifts the rest.
const ADVANCE_MS: u64 = 1900;

/// The `1` key's punctuation cycle, in tap order, measured on a T435SP by
/// tapping it n times for n = 1..14 and reading off what landed.
///
/// It wraps after thirteen, and it is shorter than it looks: quotes,
/// apostrophe, brackets, `%`, `&`, `*` and the rest are not on this key at
/// all -- they live behind the symbols mode, which is a picker rather than a
/// cycle.  Characters missing from here are reported as untypeable rather
/// than typed as whatever sits at that position, because a silently wrong
/// character is the worst thing this can do.
const PUNCT: &[char] = &[
    '.', ',', '?', '!', '1', ';', ':', '/', '@', '-', '+', '_', '=',
];

/// Each key's tap cycle.  The digit last, which is how the keypad itself
/// orders them, and why a digit costs four taps.
const KEYPAD: &[(&str, &[char])] = &[
    ("2", &['a', 'b', 'c', '2']),
    ("3", &['d', 'e', 'f', '3']),
    ("4", &['g', 'h', 'i', '4']),
    ("5", &['j', 'k', 'l', '5']),
    ("6", &['m', 'n', 'o', '6']),
    ("7", &['p', 'q', 'r', 's', '7']),
    ("8", &['t', 'u', 'v', '8']),
    ("9", &['w', 'x', 'y', 'z', '9']),
    ("0", &[' ', '0']),
];

/// One thing to do.
///
/// `Mode` is a *request*, not a keypress: the executor decides how many `#`
/// presses it takes by looking at the screen, because that is the one thing
/// the host cannot work out in advance.
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum Step {
    Key(&'static str),
    Wait(Duration),
    Mode(Case),
}

/// What the host can still keep track of: where the last tap landed, and
/// which case it last asked for.
pub struct MultiTap {
    /// The key the last tap landed on, because that is what decides whether
    /// the next character needs the timeout to elapse first.
    last: Option<&'static str>,
    /// The case last requested, so a run of one case asks once rather than
    /// once per character -- each request costs a frame grab.
    asked: Option<Case>,
    /// The IME returns to sentence mode by itself after a sentence ends, so
    /// the next letter has to ask again even if the case has not changed.
    /// Measured: `. ` alone flips the indicator back to `Ab`.
    resets_next: bool,
}

impl Default for MultiTap {
    fn default() -> Self {
        MultiTap { last: None, asked: None, resets_next: false }
    }
}

impl MultiTap {
    pub fn new() -> Self {
        Self::default()
    }

    /// The keypresses that type `ch`, or None if the keypad cannot reach it.
    ///
    /// Takes `&mut self` because typing is a walk through modes and taps: the
    /// plan for a character depends on every character before it.
    pub fn plan(&mut self, ch: char) -> Option<Vec<Step>> {
        // Enter and space are keys in their own right, and neither disturbs
        // the tap position of what follows.
        if ch == '\n' || ch == '\r' {
            self.last = None;
            return Some(vec![Step::Key("OK")]);
        }
        let (key, taps) = find(ch)?;
        let mut steps = Vec::new();

        // Only letters care about the mode.  Everything else types the same
        // in either case mode, so it rides whatever is already set -- which
        // saves a frame grab per punctuation mark.
        if ch.is_ascii_alphabetic() {
            let want = if ch.is_ascii_uppercase() { Case::Upper } else { Case::Lower };
            if self.asked != Some(want) || self.resets_next {
                // A mode switch is not free of the tap cycle: `#` only
                // reaches the IME while the field holds a character, so the
                // executor types a throwaway one on key 2 and deletes it
                // afterwards.  That tap obeys the same timeout as any other
                // -- if what came before is still mid-cycle it extends *that*
                // character, and the delete then takes both away.
                if self.last.is_some() {
                    steps.push(Step::Wait(Duration::from_millis(ADVANCE_MS)));
                }
                steps.push(Step::Mode(want));
                self.asked = Some(want);
                // Switching moves the cursor on, so what follows is never a
                // continuation of the character before it.
                self.last = None;
            }
            self.resets_next = false;
        } else if matches!(ch, '.' | '!' | '?') {
            // The IME reasserts sentence mode after this, so whatever case
            // was set no longer holds.
            self.resets_next = true;
        }

        // Same key as the character before: the cursor has to time out first,
        // or these taps extend that character instead of starting this one.
        if self.last == Some(key) {
            steps.push(Step::Wait(Duration::from_millis(ADVANCE_MS)));
        }
        for i in 0..taps {
            if i > 0 {
                steps.push(Step::Wait(Duration::from_millis(TAP_MS)));
            }
            steps.push(Step::Key(key));
        }
        self.last = Some(key);
        Some(steps)
    }

    /// Wait out the last character before anything else reads the field --
    /// otherwise the final letter is still sitting mid-cycle, and the next
    /// keypress from anywhere edits it.
    pub fn settle(&mut self) -> Vec<Step> {
        let steps = match self.last {
            Some(_) => vec![Step::Wait(Duration::from_millis(ADVANCE_MS))],
            None => Vec::new(),
        };
        self.last = None;
        // Whatever happens next is out of this model's sight -- a `backspace`,
        // a switch to nav mode, a different field.  Any of those can move the
        // IME (deleting back to an empty field puts it in sentence mode), so
        // the case is no longer known and the next letter has to look again.
        self.asked = None;
        self.resets_next = false;
        steps
    }

}

/// Where a character lives on the keypad: which key, and how many taps.
fn find(ch: char) -> Option<(&'static str, usize)> {
    let lower = ch.to_ascii_lowercase();
    for (key, cycle) in KEYPAD {
        if let Some(i) = cycle.iter().position(|&c| c == lower) {
            return Some((key, i + 1));
        }
    }
    PUNCT.iter().position(|&c| c == ch).map(|i| ("1", i + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(steps: &[Step]) -> Vec<&'static str> {
        steps.iter().filter_map(|s| match s {
            Step::Key(k) => Some(*k),
            _ => None,
        }).collect()
    }

    fn modes(steps: &[Step]) -> Vec<Case> {
        steps.iter().filter_map(|s| match s {
            Step::Mode(c) => Some(*c),
            _ => None,
        }).collect()
    }

    fn plan_all(text: &str) -> Vec<Step> {
        let mut t = MultiTap::new();
        let mut out = Vec::new();
        for ch in text.chars() {
            out.extend(t.plan(ch).unwrap_or_else(|| panic!("no plan for {ch:?}")));
        }
        out
    }

    #[test]
    fn a_letter_is_its_position_in_the_key_cycle() {
        let mut t = MultiTap::new();
        assert_eq!(keys(&t.plan('a').unwrap()), ["2"]);
        assert_eq!(keys(&t.plan('c').unwrap()), ["2", "2", "2"]);
        assert_eq!(keys(&t.plan('s').unwrap()), ["7", "7", "7", "7"]);
    }

    /// The whole point of the timeout: without it "on" types as one letter.
    #[test]
    fn two_characters_on_one_key_wait_between_them() {
        let steps = plan_all("on");
        assert_eq!(keys(&steps), ["6", "6", "6", "6", "6"]);
        let waits: Vec<&Step> = steps.iter()
            .filter(|s| matches!(s, Step::Wait(d) if d.as_millis() >= ADVANCE_MS as u128))
            .collect();
        assert_eq!(waits.len(), 1, "exactly one advance wait, between o and n");
    }

    #[test]
    fn different_keys_need_no_wait() {
        let steps = plan_all("he");
        assert_eq!(keys(&steps), ["4", "4", "3", "3"]);
        assert!(!steps.iter().any(|s| matches!(s, Step::Wait(d) if *d == Duration::from_millis(ADVANCE_MS))),
                "h and e are on different keys, so nothing has to time out");
    }

    /// Case is a request the executor resolves against the screen, not a
    /// count of `#` presses -- counting them is what kept going wrong.
    #[test]
    fn case_is_requested_not_counted() {
        let steps = plan_all("aB");
        assert_eq!(modes(&steps), [Case::Lower, Case::Upper]);
        assert!(!keys(&steps).contains(&"POUND"), "no plan presses # itself");
    }

    /// One request per run of a case, not one per character: each costs a
    /// frame grab.
    #[test]
    fn a_run_of_one_case_asks_once() {
        assert_eq!(modes(&plan_all("hello")), [Case::Lower]);
        assert_eq!(modes(&plan_all("HELLO")), [Case::Upper]);
    }

    /// Measured on the phone: after a sentence ends the IME returns to
    /// sentence mode by itself, so the next letter cannot assume anything.
    #[test]
    fn a_sentence_end_forces_the_next_letter_to_ask_again() {
        assert_eq!(modes(&plan_all("hi there")), [Case::Lower]);
        assert_eq!(modes(&plan_all("hi. there")), [Case::Lower, Case::Lower],
                   "the period resets the mode, so `there` has to ask again");
    }

    /// Digits ride their own letter key rather than switching to 123, which
    /// now also saves a screen round trip.
    #[test]
    fn digits_ask_for_no_mode() {
        let steps = plan_all("5");
        assert_eq!(keys(&steps), ["5", "5", "5", "5"]);
        assert!(modes(&steps).is_empty());
    }

    #[test]
    fn space_and_enter_are_single_keys() {
        let mut t = MultiTap::new();
        assert_eq!(keys(&t.plan(' ').unwrap()), ["0"]);
        assert_eq!(keys(&t.plan('\n').unwrap()), ["OK"]);
    }

    #[test]
    fn punctuation_comes_off_the_one_key() {
        let mut t = MultiTap::new();
        assert_eq!(keys(&t.plan('.').unwrap()), ["1"]);
        assert_eq!(keys(&t.plan(',').unwrap()).len(), 2);
        assert_eq!(t.plan('\u{20ac}'), None, "a euro sign has no ASCII tap path");
    }

    /// The characters the keypad cannot reach must stay unreachable rather
    /// than quietly becoming whatever sits at that tap position.
    #[test]
    fn unreachable_characters_have_no_plan() {
        let mut t = MultiTap::new();
        for ch in ['\'', '"', '(', ')', '%', '&', '*', '#', '$', '<', '>', '[', ']', '{', '}', '\\', '|', '~', '^', '`'] {
            assert_eq!(t.plan(ch), None, "{ch:?} is not on the keypad but got a plan");
        }
    }

    #[test]
    fn every_key_the_plan_names_is_a_real_key() {
        let mut t = MultiTap::new();
        for ch in ' '..='~' {
            for step in t.plan(ch).into_iter().flatten() {
                if let Step::Key(name) = step {
                    assert!(crate::keys::lookup(name).is_some(), "{name} is not a key");
                }
            }
        }
    }
}


/// A model of the keypad on the other end, used to read plans back.
///
/// It shares the character tables with the planner, so it cannot catch a table
/// that is wrong about the *device* -- only a planner that disagrees with its
/// own model of one.  That is still where nearly everything goes wrong here:
/// tap counts, the waits that keep two characters apart, and the keypresses
/// the executor makes on the planner's behalf around a mode switch.
///
/// What it models that the planner does not get to assume:
///
/// * the commit timeout, at the slower of the two measured devices;
/// * the minimum gap between taps, below which the IME drops them;
/// * a mode switch as time rather than as a free assertion -- and at the
///   fastest it can go, since that is when it leaves a character mid-cycle;
/// * the IME putting *itself* back into sentence case after `.`, `!` or `?`.
///
/// The last two are the ones that found bugs.
#[cfg(test)]
mod sim {
    use super::*;

    /// The slower of the two measured devices, since a plan has to work on
    /// both: taps of one key inside this window advance the same character,
    /// and anything longer commits it.  A 4056S merges at 1400ms and commits
    /// at 1700ms; a T435SP merges at 900ms and commits at 1200ms.
    const DEVICE_TIMEOUT_MS: u64 = 1700;
    /// Taps of one key closer together than this are dropped by the IME.
    /// Measured: three taps 80ms apart yield the second letter, 120ms and up
    /// yield the third.
    const DEVICE_MIN_TAP_MS: u64 = 120;
    /// The pump holds each key down this long, which is real time between one
    /// press and the next.
    const DEVICE_HOLD_MS: u64 = 50;
    /// The *fastest* a mode switch can be: the executor's look-first check
    /// finds the phone already in the mode being asked for, and returns after
    /// one frame grab without pressing anything.
    ///
    /// The fast case is the one worth modelling, because it is the one that
    /// can leave a character mid-cycle.  A switch that presses `#` a few times
    /// takes seconds and commits whatever was pending on its own; this one
    /// does not, and a plan that only works when the switch is slow breaks the
    /// first time it is quick.
    const DEVICE_MODE_MS: u64 = 1000;

    pub struct Keypad {
        upper: bool,
        out: String,
        pending: Option<(&'static str, usize)>,
        since_tap: u64,
        /// Simulated wall clock, so a plan can be priced as well as checked:
        /// a correct plan nobody would wait for is still a bad plan.
        elapsed: u64,
        switches: u32,
    }

    impl Keypad {
        pub fn new() -> Self {
            Keypad {
                upper: false, out: String::new(), pending: None,
                since_tap: u64::MAX, elapsed: 0, switches: 0,
            }
        }

        fn cycle_of(key: &str) -> &'static [char] {
            if key == "1" {
                PUNCT
            } else {
                KEYPAD.iter().find(|(k, _)| *k == key).map(|(_, c)| *c).expect("real key")
            }
        }

        fn commit(&mut self) {
            let Some((key, i)) = self.pending.take() else { return };
            let ch = Self::cycle_of(key)[i];
            self.out.push(if self.upper { ch.to_ascii_uppercase() } else { ch });
            // The IME asserts sentence case for itself once a sentence ends.
            // Modelled as a sticky uppercase because that is enough to catch
            // the mistake it causes: a plan that assumes its last request
            // still holds types the next letter in the wrong case.
            if matches!(ch, '.' | '!' | '?') {
                self.upper = true;
            }
        }

        /// One physical press of a keypad key, with the merge rule and the
        /// dropped-tap rule the real IME applies to it.
        fn tap(&mut self, key: &'static str) {
            if self.pending.map(|(k, _)| k) == Some(key) {
                assert!(
                    self.since_tap >= DEVICE_MIN_TAP_MS,
                    "tapped {key} again {}ms later -- the IME drops that",
                    self.since_tap
                );
            }
            let n = Self::cycle_of(key).len();
            self.pending = match self.pending {
                Some((k, i)) if k == key && self.since_tap < DEVICE_TIMEOUT_MS => {
                    Some((k, (i + 1) % n))
                }
                _ => {
                    self.commit();
                    Some((key, 0))
                }
            };
            self.since_tap = DEVICE_HOLD_MS;
            self.elapsed += DEVICE_HOLD_MS;
        }

        pub fn step(&mut self, step: Step) {
            match step {
                Step::Wait(d) => {
                    let ms = d.as_millis() as u64;
                    self.since_tap = self.since_tap.saturating_add(ms);
                    self.elapsed += ms;
                    if self.since_tap >= DEVICE_TIMEOUT_MS {
                        self.commit();
                    }
                }
                // A switch presses no keypad key at all -- only `#`, which
                // the IME takes for itself.  So it does not commit anything by
                // being a keypress; it commits only by taking time, and at its
                // fastest it does not take enough.  That is what makes the
                // wait a plan has to emit before a switch load-bearing rather
                // than decorative.
                Step::Mode(case) => {
                    self.since_tap = self.since_tap.saturating_add(DEVICE_MODE_MS);
                    if self.since_tap >= DEVICE_TIMEOUT_MS {
                        self.commit();
                    }
                    self.upper = case == Case::Upper;
                    self.elapsed += DEVICE_MODE_MS;
                    self.switches += 1;
                }
                Step::Key("OK") => {
                    self.commit();
                    self.out.push('\n');
                    self.since_tap = u64::MAX;
                    self.elapsed += DEVICE_HOLD_MS;
                }
                Step::Key(key) => self.tap(key),
            }
        }

        pub fn finish(mut self) -> String {
            self.commit();
            self.out
        }
    }

    /// Plan `text`, run the plan through the model, and report what the field
    /// would hold.  This is the whole test harness; everything below feeds it.
    fn round_trip(text: &str) -> String {
        run(text).0
    }

    /// The same, keeping what the run cost: simulated milliseconds and mode
    /// switches.
    fn run(text: &str) -> (String, u64, u32) {
        let mut tap = MultiTap::new();
        let mut pad = Keypad::new();
        let mut steps = Vec::new();
        for ch in text.chars() {
            steps.extend(tap.plan(ch).unwrap_or_else(|| panic!("no plan for {ch:?}")));
        }
        steps.extend(tap.settle());
        for step in steps {
            pad.step(step);
        }
        let (ms, switches) = (pad.elapsed, pad.switches);
        (pad.finish(), ms, switches)
    }

    fn typeable() -> Vec<char> {
        (' '..='~').filter(|&c| find(c).is_some()).collect()
    }

    /// Deterministic noise, so a failure is reproducible from the seed alone
    /// rather than from whatever the machine's RNG felt like that morning.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            // xorshift64*, which is short enough to read and more than good
            // enough to pick characters out of a table.
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn pick<T: Copy>(&mut self, from: &[T]) -> T {
            from[(self.next() % from.len() as u64) as usize]
        }
    }

    /// Every ordered pair of typeable characters.  Pairs are the smallest
    /// unit that can break: nearly every failure here needs one character to
    /// be followed by a particular other one.
    #[test]
    fn every_pair_of_characters_survives_the_round_trip() {
        let typeable = typeable();
        let mut checked = 0;
        for &a in &typeable {
            for &b in &typeable {
                let text: String = [a, b].iter().collect();
                assert_eq!(round_trip(&text), text);
                checked += 1;
            }
        }
        // 75 typeable characters, so 5,625 ordered pairs.
        assert!(checked > 5_000, "only checked {checked} pairs");
    }

    /// Every ordered *triple*, which is where a mode switch can be caught
    /// between two characters that share a key -- the shape that pairs alone
    /// cannot produce.
    #[test]
    fn every_triple_of_characters_survives_the_round_trip() {
        let typeable = typeable();
        for &a in &typeable {
            for &b in &typeable {
                for &c in &typeable {
                    let text: String = [a, b, c].iter().collect();
                    assert_eq!(round_trip(&text), text, "triple {text:?}");
                }
            }
        }
    }

    /// Long random lines over the whole typeable alphabet: the pair and
    /// triple sweeps are exhaustive but short, and this is what exercises
    /// state that survives further than three characters -- the remembered
    /// case, and the sentence reset that invalidates it.
    #[test]
    fn random_lines_survive_the_round_trip() {
        let typeable = typeable();
        let mut rng = Rng(0x5EED_1234_ABCD_EF01);
        for _ in 0..2_000 {
            let len = 1 + (rng.next() % 200) as usize;
            let text: String = (0..len).map(|_| rng.pick(&typeable)).collect();
            assert_eq!(round_trip(&text), text, "seeded line {text:?}");
        }
    }

    /// The same, drawn from a small alphabet so collisions on one key and
    /// case flips happen far more often than chance would give them.
    #[test]
    fn random_lines_over_a_cramped_alphabet_survive() {
        // All of a, b and c share key 2 with the mode switch's own throwaway
        // letter; m, n and o share key 6; `.` ends a sentence and moves the
        // IME by itself.
        let cramped: Vec<char> = "abcABCmno. ".chars().collect();
        let mut rng = Rng(0x0BAD_C0FF_EE12_3456);
        for _ in 0..4_000 {
            let len = 1 + (rng.next() % 60) as usize;
            let text: String = (0..len).map(|_| rng.pick(&cramped)).collect();
            assert_eq!(round_trip(&text), text, "seeded line {text:?}");
        }
    }

    /// Runs of one key, which is the worst case for the commit timeout: every
    /// character has to wait out the one before it.
    #[test]
    fn long_runs_of_one_key_survive() {
        for text in ["aaaaaaaa", "mmmmmmmm", "sssss", "zzzzzz", "        ", "........"] {
            assert_eq!(round_trip(text), text);
        }
        // And the same characters with the case alternating, so a mode switch
        // lands between two taps of one key every time.
        for text in ["aAaAaAaA", "mMmMmM", "sSsS"] {
            assert_eq!(round_trip(text), text);
        }
    }

    /// Lines of the kind someone actually types into a phone.
    #[test]
    fn realistic_lines_survive() {
        for text in [
            "Hello, world! kaimirror 0.2.0 test@example.com 99",
            "Meet me at 7:30, do not be late.",
            "wifi-password_2024",
            "Call Bob?\nYes.\n",
            "search: multi-tap timing",
            "1600 Pennsylvania Ave; Washington",
            "a.b.c.d.e.f",
            "THE QUICK BROWN FOX jumps over 13 lazy dogs.",
        ] {
            assert_eq!(round_trip(text), text, "{text:?}");
        }
    }

    /// A settle in the middle is what happens when the user leaves text mode,
    /// does something on the phone, and comes back.  Both halves have to land
    /// whole, and the second must not inherit a case it can no longer vouch
    /// for.
    #[test]
    fn text_typed_either_side_of_a_settle_survives() {
        for (first, second) in [("hello", "world"), ("abc", "abc"), ("Hi", "hi"),
                                ("end.", "next"), ("mno", "mno")] {
            let mut tap = MultiTap::new();
            let mut pad = Keypad::new();
            for ch in first.chars().chain(Some('\u{0}')).chain(second.chars()) {
                let steps = if ch == '\u{0}' { tap.settle() } else { tap.plan(ch).unwrap() };
                for step in steps {
                    pad.step(step);
                }
            }
            for step in tap.settle() {
                pad.step(step);
            }
            assert_eq!(pad.finish(), format!("{first}{second}"));
        }
    }

    /// A plan that is correct but glacial is still unusable, and the cost is
    /// all in mode switches: each is a couple of seconds of frame grabs.  So
    /// the count is asserted, not just the text.
    #[test]
    fn a_line_of_one_case_pays_for_one_mode_switch() {
        let (out, _, switches) = run("hello world");
        assert_eq!(out, "hello world");
        assert_eq!(switches, 1, "one lowercase run should ask once");

        // A sentence end makes the phone move the mode itself, so the letter
        // after it has to ask again -- but only once, not per character.
        let (_, _, switches) = run("hi. there friend");
        assert_eq!(switches, 2);

        // The pathological case, priced rather than forbidden: alternating
        // case cannot avoid a switch per letter.
        let (out, _, switches) = run("aAaA");
        assert_eq!(out, "aAaA");
        assert_eq!(switches, 4);
    }

    /// A rough budget for the common case, in simulated milliseconds.  It is
    /// generous on purpose: this exists to catch a change that makes typing
    /// several times slower, not to pin the current number.
    #[test]
    fn a_short_message_types_in_a_sane_time() {
        let (out, ms, _) = run("hello world");
        assert_eq!(out, "hello world");
        assert!(ms < 20_000, "11 characters should not take {ms}ms");
    }
}
