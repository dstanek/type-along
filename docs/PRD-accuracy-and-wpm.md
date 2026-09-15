# PRD: Accuracy and WPM in the End-of-Run Report

## Summary

Extend the existing end-of-run report line (`Keys pressed: {n}, Mistakes:
{m}`, added in `docs/PRD-end-of-run-report.md` and printed after the
terminal is restored on any clean exit) with two more numbers: accuracy and
words per minute. Scope is exactly these two metrics — no per-character
mistake map and no persistence of stats across runs; both remain separate
future features per `REQUIREMENTS.md`'s Reporting Requirements section.

Three fields are added to `TypingSession` (a start `Instant`, an end
`Instant`, and a correct-character counter) alongside the existing
`keys_pressed`/`mistakes` counters; no new files, no lib target, no change
to the single-binary-crate structure the existing tests depend on.

## Use-Cases

**UC-1: User completes the file with some mistakes.**
The user types the whole file, making some wrong keypresses along the way.
The report line shows `Accuracy` below 100% (computed from the final
`keys_pressed`/`mistakes` counts) and a `WPM` greater than 0 (some time
elapsed and at least one correct `Char`/`Enter` attempt occurred).

**UC-2: User completes the file with zero mistakes.**
Same as UC-1, but the user never types a wrong character. `Accuracy` reads
exactly `100.0%`.

**UC-3: User quits early (Esc / Ctrl-X / Ctrl-C) after some keystrokes.**
The report still prints (per the existing rule that it is unconditional on
any non-error exit), reflecting whatever partial progress was made.
`Accuracy` and `WPM` are computed from the same partial `keys_pressed`,
`mistakes`, and `correct_chars` counts, and from elapsed time up to the quit
keypress — not from a "would-have-finished" projection.

**UC-4: User quits immediately without pressing any typing key.**
The user launches the app and immediately presses Esc. The timer never
started (see Data Model Changes), so `WPM` reads `0.0`. `keys_pressed` is
`0`, so per the business rule below `Accuracy` reads `100.0%` rather than
dividing by zero.

**UC-5: User's only input is a Tab whitespace-skip, then quits.**
The user presses Tab once (skipping a run of leading whitespace) and then
quits without typing any character. This starts the timer (a whitespace-skip
counts as the "first typing attempt" that starts it) but contributes nothing
to `correct_chars`, since only correct `Char`/`Enter` attempts count toward
WPM. Elapsed time is therefore nonzero, but `WPM` still reads `0.0` — the
numerator is 0. `Accuracy` reads `100.0%` (one `keys_pressed`, zero
mistakes). This is the one case worth a dedicated test: a running timer does
not by itself guarantee a nonzero WPM.

**Exception flow (all use-cases): `event_loop()` returns an `Err`.**
Same as the predecessor PRD: the terminal is still restored, but no report
line — with or without accuracy/WPM — is printed at all.

## Data Model Changes

Add three fields to `TypingSession`:

| Field | Type | Initial value | Set/incremented when |
| --- | --- | --- | --- |
| `start_time` | `Option<Instant>` | `None` | Set once, the first time it is still `None`, at the same two call sites that already increment `keys_pressed`: the top of `handle_expected_char_attempt` (covers the first `Char`/`Enter` attempt, correct or not) and the top of `skip_whitespace` (covers the first Tab whitespace-skip). Whichever of those two happens first in the session sets it. |
| `end_time` | `Option<Instant>` | `None` | Set once, in `event_loop()`, immediately after `handle_keypress` returns `Ok(false)` and *before* the loop's `break` — i.e. before returning control to `run()`, which does the terminal restore and printing. This is the one call site that fires for every way a session ends: the three quit keys (Esc/Ctrl-X/Ctrl-C) and both ways `completed` gets set to `true` (finishing on a correct attempt or finishing via a forced strike-out advance), since all of them return `Ok(false)` up through `handle_keypress`. Capturing it here, not later in `run()`, keeps terminal-restore and I/O-flush time out of the elapsed duration — the quit key's own (negligible) match-arm handling is the only thing that happens before the snapshot, not disabling raw mode or leaving the alternate screen. |
| `correct_chars` | `usize` | `0` | Incremented once per correct `Char`/`Enter` attempt: in `handle_expected_char_attempt`, in the `matches == true` branch, alongside `current_position += 1`. **Not** incremented by `skip_whitespace` (whitespace-skips don't count toward WPM's "characters typed correctly") and **not** incremented by the forced-advance branch of `record_incorrect_attempt` (a strike-out is not a correct attempt). |

No changes to `keys_pressed`, `mistakes`, `incorrect_attempts`, or
`struck_out_positions` — those keep their existing semantics from the
predecessor PRD untouched. Note the existing invariant that `mistakes` can
never exceed `keys_pressed` (every miss is also counted as a `keys_pressed`
attempt in `handle_expected_char_attempt`), so the accuracy formula below
can never go negative.

Requires adding `std::time::Instant` to the `use std::{...}` import block in
`src/main.rs` (not currently imported).

## Output Format

Extend `show_stats_report` to print exactly:

```
Keys pressed: {keys_pressed}, Mistakes: {mistakes}, Accuracy: {accuracy:.1}%, WPM: {wpm:.1}
```

Example, on completion:
```
Congratulations! You've completed the file.
Keys pressed: 42, Mistakes: 3, Accuracy: 92.9%, WPM: 48.2
```

Example, on immediate quit:
```
Keys pressed: 0, Mistakes: 0, Accuracy: 100.0%, WPM: 0.0
```

- Still a single line, still plain default terminal color (no
  `SetForegroundColor`) — same rationale as the predecessor PRD.
- One decimal place for both numbers via Rust's `{:.1}` formatting; no
  other rounding logic.
- Nothing added to the status bar (`display_status_bar`) — final report
  only, matching the predecessor PRD's Open Question 4 resolution.

**Business rules:**

- **Accuracy** = `(keys_pressed - mistakes) / keys_pressed * 100`, one
  decimal place. When `keys_pressed == 0`, accuracy is `100.0` rather than
  a division by zero or an `undefined`/`N/A` string — nothing was typed
  incorrectly, so 100% is the most defensible reading, and it keeps the
  field's type a plain `f64` with no formatting special-case at the print
  site. Note `keys_pressed` here includes Tab whitespace-skips (per the
  predecessor PRD's existing definition), which are never mistakes, so a
  Tab-only session also reads 100%; this is a direct consequence of reusing
  the existing `keys_pressed` counter, not a new definition.
- **WPM** = `(correct_chars / 5) / elapsed_minutes`, one decimal place,
  where `elapsed_minutes = end_time.duration_since(start_time).as_secs_f64()
  / 60.0`. If `start_time` is `None` (timer never started — no typing
  attempt of any kind occurred) or the elapsed duration is zero, WPM is
  `0.0` rather than dividing by zero.
- The timer starts at the first typing attempt (first `Char`/`Enter`
  attempt or first Tab whitespace-skip), not at process launch, and stops
  at the moment the session-ending keypress is recognized, not when the
  terminal is later restored or the report is printed.

## Test Plan

Extend `tests/typing_feedback.rs` (same PTY-based harness as the
predecessor report tests, reused for the reasons documented at the top of
that file). WPM is timing-dependent in a PTY test — the interval between a
test's `send_str` calls includes real scheduler and I/O latency — so tests
must not assert an exact WPM value tied to wall-clock timing. Add a small
helper that parses the `WPM: {n}` (and `Accuracy: {n}%`) values out of a
captured line with a simple split/parse (no need for a regex crate), then:

1. **Completion with mistakes, exact accuracy**: drive a deterministic
   keys/mistakes ratio (e.g. `tiny.txt`, two misses then the correct
   character — matches the existing `wrong_keypresses_before_correct_...`
   test's setup: 3 keys, 2 mistakes). Assert the exact substring
   `"Accuracy: 33.3%"` (`(3-2)/3 = 33.33...%`, rounds to one decimal).
   Accuracy is deterministic from the counters alone, so assert it exactly.
2. **Completion with zero mistakes, WPM is nonzero**: complete a small file
   (e.g. `basic.txt`) with all-correct keypresses. Assert the exact
   substring `"Accuracy: 100.0%"`; parse the WPM value out of the line and
   assert it is `> 0.0` (some real time elapsed and `correct_chars > 0`) —
   do not assert a specific number.
3. **Immediate quit, no keys pressed**: spawn and send Esc immediately.
   Assert the exact substring `"Accuracy: 100.0%, WPM: 0.0"` — this is
   deterministic (`keys_pressed == 0` and the timer never started) despite
   being a PTY test.
4. **Tab-only interaction, timer runs but WPM stays zero (UC-5)**: on
   `ws.txt` (`"a   b"`), press Tab once (a whitespace-skip, no character
   ever typed), then quit. Assert the exact substring `"WPM: 0.0"` even
   though real time has elapsed — proves a running timer alone doesn't
   produce a nonzero WPM.
5. **Early quit after mistakes reports partial, exact accuracy**: reuse the
   existing `esc_quit_after_mistake_reports_partial_counts_...` setup (one
   miss against `basic.txt`'s `'a'`, then Esc: `keys_pressed = 1`,
   `mistakes = 1`). Assert the exact substring `"Accuracy: 0.0%"`
   (`(1-1)/1 = 0%`); parse WPM and assert it is `>= 0.0` (a single wrong
   keypress contributes nothing to `correct_chars`, so `0.0` is expected
   here too — assert it exactly, since the numerator is deterministically
   zero regardless of elapsed time).
6. **Rounding**: pick a ratio that doesn't round evenly (the 33.3% case in
   #1 already covers this) to confirm one-decimal rounding is exercised,
   not just whole-number cases like 100.0% and 0.0%.
7. **Error path prints neither new field**: extend the existing
   `error_reading_file_after_raw_mode_still_restores_terminal` test's
   assertions to also check the output does not contain `"Accuracy"` or
   `"WPM"`.
8. **Existing predecessor-PRD tests keep passing unmodified**: none of the
   existing `wait_for`/`assert_stats_line_after`/`contains` calls in
   `tests/typing_feedback.rs` anchor on end-of-line or a trailing newline —
   they all check a `"Keys pressed: N, Mistakes: M"` *prefix* substring
   (see below). Confirm this by running the full existing suite unchanged
   after the format-string change; no existing assertion should need
   editing.

**Which existing tests assert on the full line vs. a substring:** every
existing assertion in `tests/typing_feedback.rs` checks a prefix substring
like `"Keys pressed: 2, Mistakes: 0"` or `"Keys pressed: 0, Mistakes: 0"`
(via `wait_for`, `contains`, or `assert_stats_line_after`), never the
complete line including a trailing newline or a length/equality check on
the whole string. Appending `, Accuracy: {a}%, WPM: {w}` after that prefix
does not break any of them — they remain valid substring matches against
the new, longer line. **No existing test needs to change** for the format
extension itself; the additions above are new assertions for the new
fields.

## Task Breakdown

**backend-engineer**
- [ ] Add `use std::time::Instant;` to `src/main.rs`'s import block.
- [ ] Add `start_time: Option<Instant>`, `end_time: Option<Instant>`, and
      `correct_chars: usize` fields to `TypingSession`, initialized to
      `None`, `None`, and `0` respectively in `new()`.
- [ ] In `handle_expected_char_attempt`, set `start_time` (if still `None`)
      alongside the existing `keys_pressed += 1` at the top of the method.
- [ ] In `skip_whitespace`, set `start_time` (if still `None`) alongside
      the existing `keys_pressed += 1` at the top of the method.
- [ ] In `handle_expected_char_attempt`'s `matches == true` branch,
      increment `correct_chars` alongside `current_position += 1`.
- [ ] In `event_loop()`'s loop, set `end_time` (if still `None`)
      immediately after `handle_keypress(key)?` returns `false`, before the
      `break` — not later in `run()`.
- [ ] Add `fn accuracy_percent(&self) -> f64` and `fn wpm(&self) -> f64`
      implementing the formulas in this PRD's Business Rules, including
      the `keys_pressed == 0` and no-timer/zero-elapsed guards.
- [ ] Update `show_stats_report` to the new format string, calling the two
      new helper methods.

**integration-tester**
- [ ] Add a small helper to `tests/typing_feedback.rs` to parse the
      `Accuracy: {n}%` and `WPM: {n}` values out of a captured stats line
      (plain string split/parse — no new dependency needed).
- [ ] Implement the 8 test-plan scenarios above.
- [ ] Confirm the full existing suite in `tests/typing_feedback.rs` passes
      unmodified against the new, longer report line (per point 8/the
      substring-vs-full-line note above) — flag to backend-engineer via the
      debugger flow if any assertion turns out to be anchored more strictly
      than believed.

**documentation-writer**
- [ ] Update `README.md`'s description of the end-of-run report line to
      show the new `Accuracy`/`WPM` fields, their exact format, and the
      accuracy/WPM formulas and zero-guards from this PRD's Business Rules.
- [ ] Update `README.md`'s "Current state" bullet that currently reads
      "WPM / CPS timing and the per-character mistake report ... currently
      shows only total keys pressed and total mistakes" — WPM is now
      implemented; CPS and the per-character mistake map remain future
      work.
- [ ] Update `REQUIREMENTS.md`'s existing "implementation divergence" note
      under Reporting Requirements to record that WPM (not CPS) is now
      implemented alongside keys-pressed/mistakes/accuracy, without
      rewriting the original requirement language.

## Open Questions

None. Every design decision this feature needed (the accuracy formula and
its zero-`keys_pressed` case, the WPM formula and its zero-timer/zero-
elapsed case, when the timer starts and stops, and that nothing is added to
the status bar) was specified up front; see the Business Rules under
Output Format and the Data Model Changes table above for the concrete
rules.
