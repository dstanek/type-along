# PRD: End-of-Run Report (Keys Pressed / Mistakes)

## Summary

Add a one-line report, printed after the terminal is restored, showing how
many keys the user pressed and how many mistakes they made during the
session. This is a small first step toward the "Reporting Requirements"
section of `REQUIREMENTS.md`; it does **not** implement the WPM/CPS timer or
the per-character mistake map described there — those remain future work.

Two new counters are added to `TypingSession` and incremented alongside the
existing keypress-handling logic; no new files, no lib target, no change to
the single-binary-crate structure the existing tests depend on.

## Use-Cases

**UC-1: User completes the file with some mistakes.**
The user types the whole file, making some wrong keypresses along the way
(some of which strike out). After the terminal is restored, the existing
green "Congratulations!" line prints, followed immediately by the stats
line reporting the total keys pressed and total mistakes made.

**UC-2: User completes the file with zero mistakes.**
Same as UC-1, but the user never types a wrong character. The stats line
still prints and explicitly shows `Mistakes: 0` — it is never omitted or
suppressed just because the count is zero.

**UC-3: User quits early (Esc / Ctrl-X / Ctrl-C) after making some mistakes.**
The user quits partway through. The congratulations line is (as today)
skipped, but the stats line still prints, reflecting whatever partial
progress was made. This surfaces useful feedback even for an aborted
session and avoids a special case where the report silently depends on
`completed`. (See Open Question 3 — this is the recommended behavior, not
yet confirmed.)

**UC-4: User quits immediately without pressing any typing key.**
The user launches the app and immediately presses Esc. The stats line still
prints, showing `Keys pressed: 0, Mistakes: 0` — the report is unconditional
on any successful run, not gated on activity.

**Exception flow (all use-cases): `event_loop()` returns an `Err`.**
E.g. the practice file disappears out from under the run, or (per the
existing regression test) the target path is a directory. The terminal is
still restored unconditionally as today, but neither the congratulations
line nor the new stats line is printed — matching the existing rule that an
error must not trigger the completion message.

## Data Model Changes

Add two fields to `TypingSession`, both initialized to `0` in `new()`:

| Field | Type | Incremented when |
| --- | --- | --- |
| `keys_pressed` | `usize` | Once per real typing attempt: every call to `handle_expected_char_attempt` (covers both a `Char` match/mismatch and an `Enter` match/mismatch), and once per `Tab` press that actually performs a whitespace skip (i.e. inside `skip_whitespace`, once per call — not once per character skipped). |
| `mistakes` | `usize` | Once per call to `record_incorrect_attempt` — i.e. every wrong `Char` or wrong `Enter`, including all three misses of a strike-out sequence, not just the one that triggers the strike-out. |

Not incremented by either counter: arrow/function keys, Ctrl/Alt chords,
quit keys (Esc/Ctrl-X/Ctrl-C), and a `Tab` pressed when the current
character is not whitespace (the existing no-op case, UC-6 in
`tests/typing_feedback.rs`).

No changes to `incorrect_attempts` or `struck_out_positions` — those keep
their existing per-position semantics untouched.

## Output Format

A new method, e.g. `show_stats_report`, prints exactly:

```
Keys pressed: {keys_pressed}, Mistakes: {mistakes}
```

- Plain default terminal color (no `SetForegroundColor`) — it's a data
  line, not the celebratory message, so it stays visually distinct from the
  green congratulations text.
- No pluralization logic (`1 key` vs `2 keys`) — the labels are fixed
  strings, not sentences, so this doesn't arise. Keeps the change small.
- Called from `run()` after the unconditional terminal restore, guarded
  only on `result` being `Ok(())` (i.e. `event_loop()` didn't error) —
  independent of `self.completed`. On completion it prints on the line
  after the congratulations message; on early quit it's the only line
  printed.

Example, on completion:
```
Congratulations! You've completed the file.
Keys pressed: 42, Mistakes: 5
```

Example, on early quit:
```
Keys pressed: 10, Mistakes: 3
```

## Test Plan

Extend `tests/typing_feedback.rs` (PTY-based, asserting on raw ANSI bytes,
consistent with the existing suite):

1. **Completion with mistakes**: reuse a two-misses-then-correct flow (like
   `two_misses_then_correct_no_strike_and_resets_counter`); after finishing
   the file, assert `"Keys pressed: 3, Mistakes: 2"` appears after the
   alternate-screen restore (same `assert_completion_after_restore`-style
   ordering check, extended to also anchor the stats line).
2. **Zero mistakes**: complete a file (e.g. `basic.txt`) without any wrong
   keypresses; assert the literal substring `"Mistakes: 0"` is present —
   proves it's never omitted at zero.
3. **Early quit with prior mistakes**: make one miss, then send Esc; assert
   the stats line appears with the correct partial counts and that
   `"Congratulations"` is absent.
4. **Early quit, no keys pressed**: spawn and immediately send Esc; assert
   `"Keys pressed: 0, Mistakes: 0"`.
5. **Ignored keys don't inflate the count**: send arrows + a Ctrl-letter
   chord (reusing the `arrows_and_ctrl_letter_do_not_count_as_attempts`
   fixture/sequence), then quit; assert `"Keys pressed: 0"`.
6. **Tab counts once per press, not once per character skipped**: on
   `ws.txt` (`"a   b"`), type `a`, then press Tab once to skip the run of
   three spaces, then quit; assert `"Keys pressed: 2"` (the `a` plus the one
   Tab), not 4.
7. **Strike-out sequence contributes every miss, not just the strike**: on
   `abc.txt`, drive two full three-miss strike-out sequences (6 wrong
   keypresses total, as in `three_distinct_misses_strike_out_and_advance_by_one`
   run twice), then quit or complete; assert `"Mistakes: 6"`.
8. **Error path prints neither line**: extend
   `error_reading_file_after_raw_mode_still_restores_terminal` to also
   assert the output does not contain `"Keys pressed"`.

## Task Breakdown

**backend-engineer**
- [ ] Add `keys_pressed: usize` and `mistakes: usize` fields to
      `TypingSession`, initialized to `0` in `new()`.
- [ ] Increment `keys_pressed` in `handle_expected_char_attempt` (covers
      `Char` and `Enter`) and once per call in `skip_whitespace`.
- [ ] Increment `mistakes` in `record_incorrect_attempt`, once per call
      (all three misses of a strike-out sequence, not just the strike).
- [ ] Add `show_stats_report(&self) -> Result<()>` printing the exact format
      above in the default terminal color.
- [ ] Call `show_stats_report()` from `run()` after the unconditional
      restore, gated on `result` being `Ok(())`, independent of
      `self.completed`.

**integration-tester**
- [ ] Implement the 8 test-plan scenarios above in
      `tests/typing_feedback.rs`.
- [ ] Confirm no existing test's byte-stream assertions (e.g. exact
      completion-message ordering checks) break due to the new trailing
      line.

**documentation-writer**
- [ ] Update README.md's description of the completion/quit behavior to
      mention the new stats line, its exact format, and that (per this
      PRD) it prints on both completion and early quit but not on error
      exit.
- [ ] Add a short note under REQUIREMENTS.md's "Reporting Requirements"
      section recording that the keys-pressed/mistakes counts are
      implemented, while the WPM/CPS timer and per-character mistake map
      remain future work — without rewriting that section's original
      language.

## Open Questions

1. **What counts as a "key pressed"?**
   Recommendation: only real typing attempts — `Char`/`Enter` evaluated
   against the expected character, plus one count per `Tab` that performs a
   whitespace skip. Arrow/function keys, Ctrl/Alt chords, a no-op `Tab`, and
   the quit keys themselves are excluded. Counting swallowed no-ops or the
   keystroke that ends the session would inflate the number without
   reflecting any actual typing practice.

2. **Does "mistakes" mean total wrong keypresses, the number of struck-out
   positions, or both?**
   Recommendation: total wrong keypresses (every call to
   `record_incorrect_attempt`), not just the count of struck-out positions.
   "How many mistakes were made" naturally reads as every wrong attempt;
   counting only strikes would silently report `0` for a session full of
   two-misses-then-correct recoveries, which is misleading. Reporting both
   numbers is left for a later iteration alongside the per-character
   mistake map already scoped in `REQUIREMENTS.md`, to keep this change
   small.

3. **Should the report also print on early quit, or only on completion?**
   Recommendation: print on both. The counters are meaningful regardless of
   whether the user finished, and gating the whole report on `completed`
   would hide feedback for the (very common) case of a user quitting
   partway through practice. The existing "no message on early quit" rule
   is specific to the congratulations text, not this new data line.

4. **Should the counters appear live in the status bar too, or only in the
   final report?**
   Recommendation: final report only, for now. The status bar already shows
   position, percent, and the per-position miss counter; a live
   session-wide total duplicates ground the future WPM/CPS live timer is
   meant to cover, and adding it now would expand this change beyond a
   ~30-line diff.
