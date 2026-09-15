//! Integration/E2E tests for the three-strikes-then-skip and dim-contrast
//! features (see the Keys section of README.md).
//!
//! `TypingSession`'s methods call `terminal::size()` and write raw escape
//! sequences, so they can't be driven headless via plain `#[test]` calls
//! into the crate (it's a binary crate with no lib target, and the PRD
//! forbids restructuring it to add one). Instead these tests spawn the real
//! `type-along` binary under a pseudo-terminal, feed it keystrokes, and
//! assert on the literal ANSI byte stream it emits - the same bytes a real
//! terminal would render.
//!
//! Fixture files are written at runtime under `CARGO_TARGET_TMPDIR` (the
//! directory Cargo provides specifically for integration-test scratch
//! output) rather than checked into the repo or pointed at this session's
//! scratchpad - the suite must still pass on a clean checkout, on another
//! machine, and after this session's scratchpad is gone.

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_type-along");
const WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Fixture contents, written to disk once (guarded by `OnceLock` so
/// parallel test threads can't race each other or observe a partial
/// write) rather than checked into the repo.
const FIXTURE_FILES: &[(&str, &str)] = &[
    ("basic.txt", "ab"),
    ("abc.txt", "abc"),
    ("newline.txt", "hi\nok"),
    ("ws.txt", "a   b"),
    ("shift.txt", "Hi"),
    ("final_strike.txt", "ab"),
    ("emdash.txt", "a\u{2014}b"),
    (
        "dim.rs",
        "// a comment\nfn my_function(x: i32) -> i32 { let s = \"a string\"; return x; }",
    ),
    ("tiny.txt", "a"),
    ("leading_ws.txt", "   a"),
];

fn fixtures_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("typing_feedback_fixtures");
        std::fs::create_dir_all(&dir).expect("create fixtures dir");
        for (name, content) in FIXTURE_FILES {
            std::fs::write(dir.join(name), content)
                .unwrap_or_else(|e| panic!("write fixture {name}: {e}"));
        }
        dir
    })
}

fn fixture(name: &str) -> String {
    fixtures_dir()
        .join(name)
        .to_str()
        .expect("fixture path is valid UTF-8")
        .to_string()
}

/// Named crossterm ANSI SGR codes we look for in the raw output stream.
/// (Confirmed empirically and against crossterm 0.27's `Colored::Display`
/// impl: `Color::Red` -> "38;5;9", `ResetColor` -> "0m".)
const RED_FG: &str = "\x1b[38;5;9m";
const RESET: &str = "\x1b[0m";
const GREEN_FG: &str = "\x1b[38;5;10m"; // completion message color

/// `cursor::MoveTo(col, row)` -> `ESC[{row+1};{col+1}H` (1-indexed).
fn move_to(col: u16, row: u16) -> String {
    format!("\x1b[{};{}H", row + 1, col + 1)
}

struct Session {
    writer: Box<dyn Write + Send>,
    buf: Arc<Mutex<Vec<u8>>>,
    child: Box<dyn Child + Send + Sync>,
    // Keeping the master alive for the session's lifetime; dropping it
    // closes the pty.
    _master: Box<dyn MasterPty + Send>,
}

impl Session {
    fn spawn(fixture_path: &str) -> Self {
        Self::spawn_with_size(fixture_path, 24, 80)
    }

    fn spawn_with_size(fixture_path: &str, rows: u16, cols: u16) -> Self {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let mut cmd = CommandBuilder::new(BIN);
        cmd.arg(fixture_path);
        // Make sure a NO_COLOR set in the test-runner's environment can't
        // suppress the SGR sequences these tests assert on.
        cmd.env_remove("NO_COLOR");

        let child = pair.slave.spawn_command(cmd).expect("spawn type-along");
        // We don't need the slave side once the child owns it.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
        let writer = pair.master.take_writer().expect("take pty writer");

        let buf = Arc::new(Mutex::new(Vec::new()));
        let reader_buf = buf.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => reader_buf.lock().unwrap().extend_from_slice(&chunk[..n]),
                }
            }
        });

        Self {
            writer,
            buf,
            child,
            _master: pair.master,
        }
    }

    fn output(&self) -> Vec<u8> {
        self.buf.lock().unwrap().clone()
    }

    /// Polls the accumulated output until `needle` appears, or panics with
    /// the full captured output on timeout. Polling a real condition
    /// instead of sleeping a fixed duration - the app only writes on
    /// keypress, so there's no fixed delay that's both fast and reliable.
    fn wait_for(&self, needle: &str) -> Vec<u8> {
        let start = Instant::now();
        loop {
            let out = self.output();
            if contains(&out, needle.as_bytes()) {
                return out;
            }
            if start.elapsed() > WAIT_TIMEOUT {
                panic!(
                    "timed out after {:?} waiting for {:?}\n--- captured output ---\n{}",
                    WAIT_TIMEOUT,
                    needle,
                    String::from_utf8_lossy(&out)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn send_str(&mut self, s: &str) {
        self.send(s.as_bytes());
    }

    /// Waits (polling, no fixed sleep) for the child to exit, and returns
    /// whether it exited successfully.
    fn wait_exit_success(&mut self, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status.success();
            }
            if start.elapsed() > timeout {
                panic!("process did not exit within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return needle.is_empty();
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------
// UC-1 / UC-2: two misses then the correct character - no strike-out,
// counter resets, position advances by exactly 1.
// ---------------------------------------------------------------------
#[test]
fn two_misses_then_correct_no_strike_and_resets_counter() {
    // fixture: "ab" (no trailing newline)
    let mut s = Session::spawn(&fixture("basic.txt"));
    s.wait_for("(0/2)"); // initial render done

    s.send_str("z");
    s.wait_for("[miss 1/3]");
    s.send_str("z");
    s.wait_for("[miss 2/3]");

    // Third attempt is the CORRECT character - must not strike out.
    s.send_str("a");
    let out = s.wait_for("(1/2)");

    // Progress advanced by exactly 1 (50%, 1/2) and the miss segment is
    // gone (counter reset), and the character was redrawn in its normal
    // (non-red) syntax color, not red.
    assert!(
        String::from_utf8_lossy(&out).contains("50% (1/2)"),
        "expected exactly one position of advance"
    );
    let last_status_at = find_last(&out, b" basic.txt - 50% (1/2)");
    let status_line = &out[last_status_at.unwrap()..];
    assert!(
        !contains(status_line, b"[miss"),
        "miss counter should have reset after the correct keypress"
    );
    // The redraw of 'a' at (0,0) must not use the red SGR code.
    let redraw = format!("{}{RED_FG}a", move_to(0, 0));
    assert!(
        !contains(&out, redraw.as_bytes()),
        "position that matched on the 3rd attempt must not render red"
    );

    // Finish the file normally.
    s.send_str("b");
    let out3 = s.wait_for("Congratulations! You've completed the file.");
    assert!(
        contains(&out3, format!("{GREEN_FG}Congratulations").as_bytes()),
        "completion message should still be printed in green"
    );
    assert!(
        !contains(&out3, b"Press any key"),
        "message must not claim it waits for a keypress - nothing does"
    );
    assert_completion_after_restore(&out3);
    assert!(
        s.wait_exit_success(Duration::from_secs(3)),
        "process should exit cleanly (status 0) after completion"
    );
}

fn find_last(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .rev()
        .find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// Confirms the completion message shows up in the byte stream only after
/// the alternate-screen restore sequence - i.e. it was printed to the
/// user's normal scrollback, not into the alternate screen that run() tears
/// down immediately afterward (where it would be invisible).
fn assert_completion_after_restore(out: &[u8]) {
    let restore_idx = find_last(out, b"\x1b[?1049l")
        .expect("expected the alternate-screen restore sequence in the output");
    let message_idx = find_last(out, b"Congratulations! You've completed the file.")
        .expect("expected the completion message in the output");
    assert!(
        message_idx > restore_idx,
        "completion message must be printed after the alternate-screen restore, not before"
    );

    // run()'s unconditional restore must fire exactly once on a normal
    // exit - never zero (the bug this guards against) and never twice.
    let restore_count = count_occurrences(out, b"\x1b[?1049l");
    assert_eq!(
        restore_count, 1,
        "expected exactly one alternate-screen restore sequence on a normal exit, found {restore_count}"
    );
}

fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

// ---------------------------------------------------------------------
// UC-3: third consecutive miss strikes out the position (red), advances
// current_position by exactly 1, and resets the counter. The three misses
// need not be the same wrong character.
// ---------------------------------------------------------------------
#[test]
fn three_distinct_misses_strike_out_and_advance_by_one() {
    // fixture: "abc"
    let mut s = Session::spawn(&fixture("abc.txt"));
    s.wait_for("(0/3)");

    s.send_str("x");
    s.wait_for("[miss 1/3]");
    s.send_str("y"); // different wrong character than attempt 1
    s.wait_for("[miss 2/3]");
    s.send_str("z"); // different again - strike-out on the 3rd

    let out = s.wait_for("33% (1/3)");

    // The struck-out 'a' at (0,0) is redrawn in red.
    let struck = format!("{}{RED_FG}a{RESET}", move_to(0, 0));
    assert!(
        contains(&out, struck.as_bytes()),
        "expected struck-out red redraw of 'a' at (0,0)"
    );

    // Position advanced by exactly 1 (33%, 1/3 - not skipping further),
    // and the miss counter dropped from the status bar (reset to 0).
    let last_status = find_last(&out, b" abc.txt - 33% (1/3)").unwrap();
    assert!(!contains(&out[last_status..], b"[miss"));

    // Counter reset is also observable behaviorally: the very next wrong
    // keypress at the new position must report [miss 1/3], not 3/3 or
    // continue counting.
    s.send_str("q"); // wrong for expected 'b'
    let out2 = s.wait_for("[miss 1/3]");
    assert!(
        !contains(&out2[out2.len().saturating_sub(200)..], b"[miss 2/3]")
            && !contains(&out2, b"[miss 3/3]"),
        "counter must have reset to 0 after strike-out, not continued from 3"
    );

    // Finish the file.
    s.send_str("b");
    s.send_str("c");
    let out3 = s.wait_for("Congratulations! You've completed the file.");
    assert_completion_after_restore(&out3);
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// Coverage note tested here: a struck-out position is never repainted.
// The app has no scroll/resize-triggered redraw path (out of scope per
// the PRD), so within a single run there is exactly one event that could
// ever draw over position 0 again - and we assert it never happens for
// the remainder of the captured stream.
// ---------------------------------------------------------------------
#[test]
fn struck_out_position_is_never_repainted_for_rest_of_session() {
    let mut s = Session::spawn(&fixture("abc.txt"));
    s.wait_for("(0/3)");
    s.send_str("x");
    s.wait_for("[miss 1/3]");
    s.send_str("y");
    s.wait_for("[miss 2/3]");
    s.send_str("z");
    let out = s.wait_for("33% (1/3)");
    let strike_idx = find_last(&out, format!("{}{RED_FG}a", move_to(0, 0)).as_bytes())
        .expect("strike-out draw not found");

    s.send_str("b");
    s.send_str("c");
    let out3 = s.wait_for("Congratulations! You've completed the file.");
    assert_completion_after_restore(&out3);
    assert!(s.wait_exit_success(Duration::from_secs(3)));

    let full = s.output();
    let after_strike = &full[strike_idx + move_to(0, 0).len()..];
    let target_move = move_to(0, 0);
    assert!(
        !contains(after_strike, target_move.as_bytes()),
        "position (0,0) must never be targeted for redraw again after strike-out"
    );
}

// ---------------------------------------------------------------------
// UC-5: Enter pressed when expected char is not a newline counts as a
// miss (not a silent no-op).
// ---------------------------------------------------------------------
#[test]
fn enter_against_non_newline_expected_char_counts_as_miss() {
    let mut s = Session::spawn(&fixture("basic.txt")); // "ab"
    s.wait_for("(0/2)");

    s.send(b"\r"); // Enter, in raw mode -> KeyCode::Enter
    s.wait_for("[miss 1/3]");
    s.send(b"\r");
    s.wait_for("[miss 2/3]");

    // Confirm it wasn't silently ignored and didn't strike out early:
    // typing the correct char now should complete UC-1/UC-2 normally.
    s.send_str("a");
    let out = s.wait_for("50% (1/2)");
    let redraw = format!("{}{RED_FG}a", move_to(0, 0));
    assert!(!contains(&out, redraw.as_bytes()));

    s.send_str("b");
    let out3 = s.wait_for("Congratulations! You've completed the file.");
    assert_completion_after_restore(&out3);
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// UC-4: a printable char typed when the expected char IS '\n' counts as
// a miss; the 3rd such miss draws a red '$' one column past end-of-line
// and advances past the newline, resuming at column 0 of the next line.
// ---------------------------------------------------------------------
#[test]
fn three_misses_against_newline_draws_dollar_and_advances_past_it() {
    // fixture: "hi\nok" - position 2 is the newline after "hi"
    let mut s = Session::spawn(&fixture("newline.txt"));
    s.wait_for("(0/5)");

    s.send_str("h");
    s.wait_for("20% (1/5)");
    s.send_str("i");
    s.wait_for("40% (2/5)");

    s.send_str("z");
    s.wait_for("[miss 1/3]");
    s.send_str("z");
    s.wait_for("[miss 2/3]");
    s.send_str("z");
    let out = s.wait_for("60% (3/5)");

    // Red '$' one column past end-of-line ("hi" is 2 chars -> col 2, row 0).
    let dollar = format!("{}{RED_FG}${RESET}", move_to(2, 0));
    assert!(
        contains(&out, dollar.as_bytes()),
        "expected a red '$' at col 2, row 0 marking the struck-out newline"
    );

    // Typing resumes at column 0 of the next line: 'o' then 'k' complete
    // the file normally, at row 1.
    s.send_str("o");
    let out2 = s.wait_for("80% (4/5)");
    assert!(contains(
        &out2,
        format!("{}{}o", move_to(0, 1), "\x1b[38;2;192;197;206m").as_bytes()
    ));
    s.send_str("k");
    let out3 = s.wait_for("Congratulations! You've completed the file.");
    assert_completion_after_restore(&out3);
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// UC-6: Tab on a non-whitespace character is a no-op - it must not
// increment or reset the miss counter.
// ---------------------------------------------------------------------
#[test]
fn tab_on_non_whitespace_does_not_touch_counter() {
    let mut s = Session::spawn(&fixture("basic.txt")); // "ab", 'a' is not whitespace
    s.wait_for("(0/2)");

    s.send(b"\t");
    // Give the (expected no-op) Tab a moment to have not done anything,
    // then prove the counter is still at 0 by checking the very next
    // miss reports 1/3, not 2/3 (which it would if Tab had counted).
    s.send_str("z");
    let out = s.wait_for("[miss 1/3]");
    assert!(
        !contains(&out, b"[miss 2/3]"),
        "Tab on a non-whitespace character must not have incremented the counter"
    );
}

// ---------------------------------------------------------------------
// UC-7: Tab whitespace-skip advances past the run and resets the counter
// (even if a miss had already been recorded at the whitespace position).
// ---------------------------------------------------------------------
#[test]
fn tab_whitespace_skip_advances_and_resets_counter() {
    // fixture: "a   b" - 'a', three spaces, 'b'
    let mut s = Session::spawn(&fixture("ws.txt"));
    s.wait_for("(0/5)");

    s.send_str("a");
    s.wait_for("20% (1/5)");

    // Record a miss against the space at position 1 first.
    s.send_str("z");
    s.wait_for("[miss 1/3]");

    s.send(b"\t");
    let out = s.wait_for("80% (4/5)"); // skipped positions 1-3, now at 'b' (pos 4)
    let last_status = find_last(&out, b"80% (4/5)").unwrap();
    assert!(
        !contains(&out[last_status..], b"[miss"),
        "counter must reset once the whitespace skip completes"
    );

    // Confirm the reset behaviorally: next miss at 'b' reports 1/3.
    s.send_str("z");
    let out2 = s.wait_for("[miss 1/3]");
    assert!(!contains(&out2, b"[miss 2/3]"));
}

// ---------------------------------------------------------------------
// UC-9: arrow keys and a bare Ctrl+letter chord never count as attempts
// (no counter change, no advance).
// ---------------------------------------------------------------------
#[test]
fn arrows_and_ctrl_letter_do_not_count_as_attempts() {
    let mut s = Session::spawn(&fixture("basic.txt")); // "ab"
    s.wait_for("(0/2)");

    s.send(b"\x1b[C"); // Right arrow
    s.send(b"\x1b[B"); // Down arrow
    s.send(b"\x01"); // Ctrl-A

    // If any of those had counted as a miss, this would show [miss 2/3]
    // or later; instead it must be the very first miss.
    s.send_str("z");
    let out = s.wait_for("[miss 1/3]");
    assert!(!contains(&out, b"[miss 2/3]"));

    // And they must not have advanced the cursor either: the expected
    // character at position 0 is still 'a'.
    s.send_str("a");
    let out2 = s.wait_for("50% (1/2)");
    assert!(!contains(
        &out2,
        format!("{}{RED_FG}a", move_to(0, 0)).as_bytes()
    ));
}

// ---------------------------------------------------------------------
// Alt+letter is likewise a no-op chord (guard arm covers CONTROL | ALT).
// ---------------------------------------------------------------------
#[test]
fn alt_letter_does_not_count_as_attempt() {
    let mut s = Session::spawn(&fixture("basic.txt")); // "ab"
    s.wait_for("(0/2)");

    // ESC immediately followed by 'a' in the same write, so crossterm's
    // reader sees both bytes available together and parses it as
    // Alt+'a' rather than a bare Esc keypress (which would quit the app).
    s.send(b"\x1ba");
    s.send_str("z");
    let out = s.wait_for("[miss 1/3]");
    assert!(
        !contains(&out, b"[miss 2/3]"),
        "Alt+'a' must not have counted as an attempt (and Esc must not have quit the app)"
    );
}

// ---------------------------------------------------------------------
// Esc quits immediately and cleanly, without firing the completion
// message. The `completed` flag is only ever set on a real finish, so an
// early quit here must never print it.
// ---------------------------------------------------------------------
#[test]
fn esc_quits_without_completion_message() {
    let mut s = Session::spawn(&fixture("basic.txt"));
    s.wait_for("(0/2)");

    s.send(b"\x1b"); // Esc, sent alone (no trailing bytes, unlike the Alt+'a' test)
    assert!(
        s.wait_exit_success(Duration::from_secs(3)),
        "Esc should exit the process cleanly (status 0)"
    );
    let out = s.output();
    assert!(!contains(&out, b"Congratulations"));
}

// ---------------------------------------------------------------------
// Ctrl-X quits immediately and cleanly, without firing the completion
// message or counting as an attempt.
// ---------------------------------------------------------------------
#[test]
fn ctrl_x_quits_cleanly() {
    let mut s = Session::spawn(&fixture("basic.txt"));
    s.wait_for("(0/2)");

    s.send(b"\x18"); // Ctrl-X
    assert!(
        s.wait_exit_success(Duration::from_secs(3)),
        "Ctrl-X should exit the process cleanly (status 0)"
    );
    let out = s.output();
    assert!(!contains(&out, b"Congratulations"));
    assert!(!contains(&out, b"[miss"));
}

// ---------------------------------------------------------------------
// Ctrl-C must quit: in raw mode ISIG is disabled, so the terminal never
// turns Ctrl-C into SIGINT - without a dedicated arm it just falls into
// the Ctrl/Alt no-op guard and there's no way to quit that isn't Esc or
// Ctrl-X. Also confirms the alternate-screen restore sequence is actually
// emitted before exit, not just that the process exit code is 0.
// ---------------------------------------------------------------------
#[test]
fn ctrl_c_quits_cleanly() {
    let mut s = Session::spawn(&fixture("basic.txt"));
    s.wait_for("(0/2)");

    s.send(b"\x03"); // Ctrl-C
    assert!(
        s.wait_exit_success(Duration::from_secs(3)),
        "Ctrl-C should exit the process cleanly (status 0)"
    );
    let out = s.output();
    assert!(!contains(&out, b"Congratulations"));
    assert!(!contains(&out, b"[miss"));
    assert!(
        contains(&out, b"\x1b[?1049l"),
        "expected the alternate-screen restore sequence (LeaveAlternateScreen) on exit"
    );
}

// ---------------------------------------------------------------------
// Regression: show_completion_message used to compute `height - 2` with
// no guard, which underflowed and panicked (exit 101) in a 1-row
// terminal. Completing a file there must exit cleanly instead.
// ---------------------------------------------------------------------
#[test]
fn completion_in_one_row_terminal_does_not_panic() {
    let mut s = Session::spawn_with_size(&fixture("tiny.txt"), 1, 80); // "a"
    s.wait_for("(0/1)");

    s.send_str("a");
    let out = s.wait_for("Congratulations! You've completed the file.");
    assert_completion_after_restore(&out);
    assert!(
        s.wait_exit_success(Duration::from_secs(3)),
        "completing a file in a 1-row terminal must exit cleanly, not panic"
    );
}

// ---------------------------------------------------------------------
// Regression: an error from event_loop() (as opposed to a panic) used to
// skip the restore block entirely, since it was reached via early `?`
// returns. Point the binary at a directory instead of a file: main()'s
// `args.file.exists()` check is true for directories too, so the app
// proceeds past it, enters the alternate screen and raw mode, and only
// then does `load_file()`'s `fs::read_to_string` fail - deterministically,
// no timing/race needed, since a directory can never be read as UTF-8
// text. This is the same shape of failure the reviewer flagged (file
// deleted/replaced out from under a running session), just triggered
// without a race.
// ---------------------------------------------------------------------
#[test]
fn error_reading_file_after_raw_mode_still_restores_terminal() {
    let dir_path = fixtures_dir().join("a_directory");
    std::fs::create_dir_all(&dir_path).expect("create directory fixture");

    let mut s = Session::spawn(dir_path.to_str().expect("fixture path is valid UTF-8"));

    // load_file() fails before display_content() ever runs, so there's no
    // status-bar text to wait for - just wait for the process to exit.
    let exited_success = s.wait_exit_success(Duration::from_secs(3));
    assert!(
        !exited_success,
        "reading a directory as the practice file should be a reported error, not a clean exit"
    );

    let out = s.output();
    assert!(
        contains(&out, b"\x1b[?1049l"),
        "expected the alternate-screen restore sequence even though the run errored"
    );
    assert!(
        !contains(&out, b"Congratulations"),
        "an error must not trigger the completion message"
    );
    assert!(
        !contains(&out, b"Keys pressed"),
        "an error must not trigger the end-of-run stats report either"
    );
    assert!(
        !contains(&out, b"Accuracy"),
        "an error must not print the accuracy field either (PRD-accuracy-and-wpm.md)"
    );
    assert!(
        !contains(&out, b"WPM"),
        "an error must not print the WPM field either (PRD-accuracy-and-wpm.md)"
    );
}

// ---------------------------------------------------------------------
// Shift+letter (a capital letter byte) is a real typing attempt, not
// filtered by the Ctrl/Alt no-op guard.
// ---------------------------------------------------------------------
#[test]
fn shift_letter_counts_as_real_attempt() {
    let mut s = Session::spawn(&fixture("shift.txt")); // "Hi"
    s.wait_for("(0/2)");

    s.send_str("H"); // capital H -> crossterm attaches KeyModifiers::SHIFT
    let out = s.wait_for("50% (1/2)");
    // It must have matched (advanced), not been swallowed as a no-op chord.
    assert!(!contains(&out, b"[miss"));

    s.send_str("i");
    let out2 = s.wait_for("Congratulations! You've completed the file.");
    assert_completion_after_restore(&out2);
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// Striking out the final character of a file still fires the completion
// message and exits cleanly.
// ---------------------------------------------------------------------
#[test]
fn strike_out_on_final_character_still_completes() {
    // fixture: "ab" - strike out the final 'b'
    let mut s = Session::spawn(&fixture("final_strike.txt"));
    s.wait_for("(0/2)");
    s.send_str("a");
    s.wait_for("50% (1/2)");

    s.send_str("x");
    s.wait_for("[miss 1/3]");
    s.send_str("y");
    s.wait_for("[miss 2/3]");
    s.send_str("z");

    let out = s.wait_for("100% (2/2)");
    let struck = format!("{}{RED_FG}b{RESET}", move_to(1, 0));
    assert!(
        contains(&out, struck.as_bytes()),
        "final character should be struck out and drawn red"
    );
    let out2 = s.wait_for("Congratulations! You've completed the file.");
    assert_completion_after_restore(&out2);
    assert!(
        s.wait_exit_success(Duration::from_secs(3)),
        "app must exit cleanly even when the file ends on a strike-out"
    );
}

// ---------------------------------------------------------------------
// The character that motivated this feature: an em dash (U+2014), which
// can't be typed without a compose key. Three wrong ASCII attempts
// strike it out and draw the real multi-byte glyph in red.
// ---------------------------------------------------------------------
#[test]
fn em_dash_strikes_out_with_real_glyph_in_red() {
    // fixture: "a\u{2014}b"
    let mut s = Session::spawn(&fixture("emdash.txt"));
    s.wait_for("(0/3)");
    s.send_str("a");
    s.wait_for("33% (1/3)");

    s.send_str("x");
    s.wait_for("[miss 1/3]");
    s.send_str("y");
    s.wait_for("[miss 2/3]");
    s.send_str("z");

    let out = s.wait_for("66% (2/3)");
    let struck = format!("{}{RED_FG}\u{2014}{RESET}", move_to(1, 0));
    assert!(
        contains(&out, struck.as_bytes()),
        "expected the real em-dash glyph struck out in red"
    );

    s.send_str("b");
    let out2 = s.wait_for("Congratulations! You've completed the file.");
    assert_completion_after_restore(&out2);
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// Dim rendering: the original `c/2 + 40` per-channel formula for
// not-yet-reached text, restored after the desaturate/darken/floor
// experiment was reverted. Cross-checks the app's *actual* emitted RGB
// values for real base16-ocean.dark syntax-highlighted spans.
// ---------------------------------------------------------------------
#[test]
fn dim_rendering_matches_c_over_2_plus_40_formula() {
    // fixture: same sample used previously for the dim-comparison doc:
    //   // a comment
    //   fn my_function(x: i32) -> i32 { let s = "a string"; return x; }
    let s = Session::spawn(&fixture("dim.rs"));
    let out = s.wait_for("(0/76)");
    let text = String::from_utf8_lossy(&out);

    // (scope, input RGB for real base16-ocean.dark syntax-highlighted spans)
    let cases: &[(&str, (u8, u8, u8))] = &[
        ("comment", (101, 115, 126)),
        ("keyword `fn`", (180, 142, 173)),
        ("function-name", (143, 161, 179)),
        ("string", (163, 190, 140)),
        ("default foreground / punctuation", (192, 197, 206)),
    ];

    for (scope, (r, g, b)) in cases {
        let (er, eg, eb) = (r / 2 + 40, g / 2 + 40, b / 2 + 40);
        let needle = format!("\x1b[38;2;{er};{eg};{eb}m");
        assert!(
            text.contains(&needle),
            "{scope}: expected dimmed RGB ({er},{eg},{eb}) from c/2+40 not found in output"
        );
    }
}

// =====================================================================
// End-of-run report (see docs/PRD-end-of-run-report.md, Test Plan).
//
// Added here rather than a new `tests/end_of_run_report.rs` file: the
// `Session` harness, fixture helpers, and byte-stream assertion helpers
// above (`wait_for`, `contains`, `find_last`, `assert_completion_after_restore`,
// etc.) are private items in this file's own test binary. Each file under
// `tests/` compiles as an independent crate, so sharing them with a new
// file would require extracting a `tests/common/mod.rs` module - a bigger
// structural change than "add tests" calls for, and the crate deliberately
// has no lib target for tests to depend on (see the file-level doc comment
// above). Reusing the harness in place, in this file, is the smaller,
// correct change.
// =====================================================================

/// Finds the exact stats line and asserts it comes after `after` (an
/// already-located byte offset), mirroring the ordering check
/// `assert_completion_after_restore` does for the alt-screen restore.
fn assert_stats_line_after(out: &[u8], expected: &str, after: usize, after_desc: &str) {
    let stats_idx = find_last(out, expected.as_bytes()).unwrap_or_else(|| {
        panic!(
            "expected stats line {expected:?} not found in output\n--- captured output ---\n{}",
            String::from_utf8_lossy(out)
        )
    });
    assert!(
        stats_idx > after,
        "stats line {expected:?} must appear after {after_desc}"
    );
}

// ---------------------------------------------------------------------
// PRD UC-2 / Test Plan #2, and the ordering half of Test Plan #1: on a
// clean completion with zero mistakes, the exact stats line prints,
// `Mistakes: 0` is never omitted, and it lands after both the
// alternate-screen restore and the green congratulations line.
// ---------------------------------------------------------------------
#[test]
fn report_after_completion_with_zero_mistakes_and_ordering() {
    // fixture: "ab" - two correct keypresses, no mistakes.
    let mut s = Session::spawn(&fixture("basic.txt"));
    s.wait_for("(0/2)");

    s.send_str("a");
    s.wait_for("50% (1/2)");
    s.send_str("b");
    let out = s.wait_for("Keys pressed: 2, Mistakes: 0");

    assert_completion_after_restore(&out); // restore happens, then congrats
    let restore_idx = find_last(&out, b"\x1b[?1049l").unwrap();
    let congrats_idx = find_last(&out, b"Congratulations! You've completed the file.").unwrap();
    assert_stats_line_after(
        &out,
        "Keys pressed: 2, Mistakes: 0",
        restore_idx,
        "the alternate-screen restore sequence",
    );
    assert_stats_line_after(
        &out,
        "Keys pressed: 2, Mistakes: 0",
        congrats_idx,
        "the congratulations line",
    );
    assert!(
        s.wait_exit_success(Duration::from_secs(3)),
        "process should exit cleanly after completion"
    );
}

// ---------------------------------------------------------------------
// PRD Test Plan #1: two misses then the correct character, completing a
// one-character file. `keys_pressed` counts all three attempts (2 wrong +
// 1 right); `mistakes` counts just the two wrong ones.
// ---------------------------------------------------------------------
#[test]
fn wrong_keypresses_before_correct_count_as_mistakes_and_keys() {
    // fixture: "a" - a single character, so two misses then the correct
    // keypress both records the mistakes and finishes the file.
    let mut s = Session::spawn(&fixture("tiny.txt"));
    s.wait_for("(0/1)");

    s.send_str("z");
    s.wait_for("[miss 1/3]");
    s.send_str("z");
    s.wait_for("[miss 2/3]");
    s.send_str("a");

    let out = s.wait_for("Keys pressed: 3, Mistakes: 2");
    assert_completion_after_restore(&out);
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// A full three-strike sequence (strike-out on the 3rd miss) contributes
// exactly 3 to both counters - every miss counts, not just the strike.
// ---------------------------------------------------------------------
#[test]
fn full_strike_out_sequence_counts_three_mistakes_and_three_keys() {
    // fixture: "abc"
    let mut s = Session::spawn(&fixture("abc.txt"));
    s.wait_for("(0/3)");

    s.send_str("x");
    s.wait_for("[miss 1/3]");
    s.send_str("y");
    s.wait_for("[miss 2/3]");
    s.send_str("z");
    s.wait_for("33% (1/3)"); // strike-out on 'a', advanced to 'b'

    s.send(b"\x1b"); // Esc: quit early rather than complete the file
    let out = s.wait_for("Keys pressed: 3, Mistakes: 3");
    assert!(!contains(&out, b"Congratulations"));
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// PRD Test Plan #7: two full three-miss strike-out sequences (6 wrong
// keypresses total) both contribute to `mistakes` - not just the two
// positions that actually got struck out.
// ---------------------------------------------------------------------
#[test]
fn two_strike_out_sequences_count_every_miss() {
    // fixture: "abc"
    let mut s = Session::spawn(&fixture("abc.txt"));
    s.wait_for("(0/3)");

    // Strike out 'a'.
    s.send_str("x");
    s.wait_for("[miss 1/3]");
    s.send_str("y");
    s.wait_for("[miss 2/3]");
    s.send_str("z");
    s.wait_for("33% (1/3)");

    // Strike out 'b'.
    s.send_str("x");
    s.wait_for("[miss 1/3]");
    s.send_str("y");
    s.wait_for("[miss 2/3]");
    s.send_str("z");
    s.wait_for("66% (2/3)");

    s.send(b"\x1b"); // Esc: quit rather than type the remaining 'c'
    let out = s.wait_for("Keys pressed: 6, Mistakes: 6");
    assert!(!contains(&out, b"Congratulations"));
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// PRD Test Plan #6: a Tab that skips a run of whitespace counts once
// toward `keys_pressed`, no matter how many characters it skips.
// ---------------------------------------------------------------------
#[test]
fn tab_whitespace_skip_counts_as_one_key_regardless_of_spaces_skipped() {
    // fixture: "a   b" - 'a', three spaces, 'b'
    let mut s = Session::spawn(&fixture("ws.txt"));
    s.wait_for("(0/5)");

    s.send_str("a");
    s.wait_for("20% (1/5)");
    s.send(b"\t"); // skips all three spaces in one press
    s.wait_for("80% (4/5)");

    s.send(b"\x1b"); // Esc: quit before typing the final 'b'
    let out = s.wait_for("Keys pressed: 2, Mistakes: 0");
    assert!(!contains(&out, b"Congratulations"));
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// A Tab pressed on a non-whitespace character is the existing no-op case
// (UC-6) and must not add to `keys_pressed` either.
// ---------------------------------------------------------------------
#[test]
fn tab_on_non_whitespace_does_not_count_as_key() {
    let mut s = Session::spawn(&fixture("basic.txt")); // "ab", 'a' is not whitespace
    s.wait_for("(0/2)");

    s.send(b"\t");
    s.send(b"\x1b"); // Esc
    let out = s.wait_for("Keys pressed: 0, Mistakes: 0");
    assert!(!contains(&out, b"Congratulations"));
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// PRD Test Plan #5: arrow keys and a Ctrl-letter chord never touch either
// counter.
// ---------------------------------------------------------------------
#[test]
fn ignored_keys_do_not_affect_report_counters() {
    let mut s = Session::spawn(&fixture("basic.txt")); // "ab"
    s.wait_for("(0/2)");

    s.send(b"\x1b[C"); // Right arrow
    s.send(b"\x1b[B"); // Down arrow
    s.send(b"\x01"); // Ctrl-A
    s.send(b"\x1b"); // Esc: quit without any real typing attempt

    let out = s.wait_for("Keys pressed: 0, Mistakes: 0");
    assert!(!contains(&out, b"Congratulations"));
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// PRD Test Plan #4: quitting immediately, before any keypress that could
// count, still prints the unconditional report showing all zeros.
// ---------------------------------------------------------------------
#[test]
fn esc_quit_with_no_keys_pressed_reports_zeros() {
    let mut s = Session::spawn(&fixture("basic.txt"));
    s.wait_for("(0/2)");

    s.send(b"\x1b"); // Esc, immediately
    let out = s.wait_for("Keys pressed: 0, Mistakes: 0");
    assert!(!contains(&out, b"Congratulations"));
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// PRD Test Plan #3 / UC-3: quitting early after a mistake still reports
// the partial counts, and the congratulations line is absent.
// ---------------------------------------------------------------------
#[test]
fn esc_quit_after_mistake_reports_partial_counts_without_congratulations() {
    let mut s = Session::spawn(&fixture("basic.txt")); // "ab"
    s.wait_for("(0/2)");

    s.send_str("z"); // wrong keypress against 'a'
    s.wait_for("[miss 1/3]");
    s.send(b"\x1b"); // Esc

    let out = s.wait_for("Keys pressed: 1, Mistakes: 1");
    assert!(
        !contains(&out, b"Congratulations"),
        "early quit must not print the completion message"
    );
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// UC-3, via Ctrl-C instead of Esc: same partial-report behavior on the
// other quit path.
// ---------------------------------------------------------------------
#[test]
fn ctrl_c_quit_after_mistake_reports_partial_counts_without_congratulations() {
    let mut s = Session::spawn(&fixture("basic.txt")); // "ab"
    s.wait_for("(0/2)");

    s.send_str("z"); // wrong keypress against 'a'
    s.wait_for("[miss 1/3]");
    s.send(b"\x03"); // Ctrl-C

    let out = s.wait_for("Keys pressed: 1, Mistakes: 1");
    assert!(
        !contains(&out, b"Congratulations"),
        "early quit must not print the completion message"
    );
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// UC-5-adjacent: a wrong Enter (against a non-newline expected char) is a
// real typing attempt like any other miss - it counts toward both
// `keys_pressed` and `mistakes`, not just `mistakes`.
// ---------------------------------------------------------------------
#[test]
fn wrong_enter_counts_as_mistake_and_key() {
    let mut s = Session::spawn(&fixture("basic.txt")); // "ab", expected char is 'a'
    s.wait_for("(0/2)");

    s.send(b"\r"); // Enter, wrong against 'a'
    s.wait_for("[miss 1/3]");
    s.send(b"\x1b"); // Esc

    let out = s.wait_for("Keys pressed: 1, Mistakes: 1");
    assert!(!contains(&out, b"Congratulations"));
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// =====================================================================
// Accuracy and WPM (see docs/PRD-accuracy-and-wpm.md, Test Plan). Reuses
// the same harness and helpers above for the reasons documented at the
// top of the end-of-run-report section - no new test file, no lib target.
// =====================================================================

/// Parses the number immediately following `prefix` (through the next
/// character that isn't a digit or '.') out of the last occurrence of
/// `prefix` in the captured output. Plain string split/parse, per the
/// PRD's Test Plan - no regex crate needed.
fn parse_number_after(out: &[u8], prefix: &str) -> f64 {
    let text = String::from_utf8_lossy(out);
    let idx = text.rfind(prefix).unwrap_or_else(|| {
        panic!("expected {prefix:?} in output\n--- captured output ---\n{text}")
    });
    let rest = &text[idx + prefix.len()..];
    let num_str: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    num_str
        .parse::<f64>()
        .unwrap_or_else(|e| panic!("failed to parse {num_str:?} after {prefix:?}: {e}"))
}

// ---------------------------------------------------------------------
// PRD Test Plan #1 / #6: two misses then the correct character on a
// one-character file (same setup as
// `wrong_keypresses_before_correct_count_as_mistakes_and_keys`:
// keys_pressed = 3, mistakes = 2). Accuracy is deterministic from the
// counters alone and doesn't round evenly (33.33...% -> 33.3%), so assert
// the exact substring.
// ---------------------------------------------------------------------
#[test]
fn completion_with_mistakes_reports_exact_rounded_accuracy() {
    // fixture: "a"
    let mut s = Session::spawn(&fixture("tiny.txt"));
    s.wait_for("(0/1)");

    s.send_str("z");
    s.wait_for("[miss 1/3]");
    s.send_str("z");
    s.wait_for("[miss 2/3]");
    s.send_str("a");

    let out = s.wait_for("Keys pressed: 3, Mistakes: 2");
    assert!(
        contains(&out, b"Accuracy: 33.3%"),
        "expected (3-2)/3 = 33.3% exactly, got: {}",
        String::from_utf8_lossy(&out)
    );
    assert_completion_after_restore(&out);
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// PRD Test Plan #2 / UC-2: completing a small file with zero mistakes
// reports exactly 100.0% accuracy and a strictly positive WPM (some real
// time elapsed and at least one correct Char attempt occurred). WPM is
// timing-dependent in a PTY test, so only its sign is asserted, never a
// specific value.
// ---------------------------------------------------------------------
#[test]
fn completion_with_zero_mistakes_reports_full_accuracy_and_positive_wpm() {
    // fixture: "ab" - two correct keypresses, no mistakes.
    let mut s = Session::spawn(&fixture("basic.txt"));
    s.wait_for("(0/2)");

    s.send_str("a");
    s.wait_for("50% (1/2)");
    s.send_str("b");

    let out = s.wait_for("Keys pressed: 2, Mistakes: 0");
    assert!(
        contains(&out, b"Accuracy: 100.0%"),
        "expected exactly 100.0% accuracy with zero mistakes"
    );
    let wpm = parse_number_after(&out, "WPM: ");
    assert!(
        wpm > 0.0,
        "expected a strictly positive WPM after a completed run with correct keystrokes, got {wpm}"
    );
    assert_completion_after_restore(&out);
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// PRD Test Plan #3 / UC-4: quitting immediately, before any keypress that
// could start the timer, reports deterministic 100.0% accuracy (no
// mistakes possible with zero keys_pressed) and exactly 0.0 WPM (the
// timer never started) - despite being a PTY test, both fields here are
// deterministic, so assert the exact substring.
// ---------------------------------------------------------------------
#[test]
fn immediate_quit_reports_full_accuracy_and_zero_wpm() {
    let mut s = Session::spawn(&fixture("basic.txt"));
    s.wait_for("(0/2)");

    s.send(b"\x1b"); // Esc, immediately - no typing attempt of any kind
    let out = s.wait_for("Keys pressed: 0, Mistakes: 0");
    assert!(
        contains(&out, b"Accuracy: 100.0%, WPM: 0.0"),
        "expected the exact deterministic tail for an immediate quit, got: {}",
        String::from_utf8_lossy(&out)
    );
    assert!(!contains(&out, b"Congratulations"));
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// PRD Test Plan #4 / UC-5: the user's only input is a Tab whitespace-skip
// over *leading* whitespace - it starts the timer (real time elapses
// before the quit keypress) but contributes nothing to correct_chars, so
// WPM must still read exactly 0.0. This is the one case worth a dedicated
// test per the PRD: a running timer alone doesn't guarantee a nonzero
// WPM. `basic.txt`/`ws.txt` don't start with whitespace (their first
// character must be typed before a Tab there would be a real
// whitespace-skip rather than the UC-6 no-op), so this uses a dedicated
// `leading_ws.txt` fixture ("   a") whose very first character is
// whitespace, matching the PRD's "leading whitespace" description
// literally.
// ---------------------------------------------------------------------
#[test]
fn tab_only_whitespace_skip_starts_timer_but_wpm_stays_zero() {
    // fixture: "   a" - three leading spaces, then 'a'.
    let mut s = Session::spawn(&fixture("leading_ws.txt"));
    s.wait_for("(0/4)");

    s.send(b"\t"); // whitespace-skip over all three leading spaces
    s.wait_for("75% (3/4)");

    s.send(b"\x1b"); // Esc: quit without ever typing a character
    let out = s.wait_for("Keys pressed: 1, Mistakes: 0");
    assert!(
        contains(&out, b"Accuracy: 100.0%"),
        "one keys_pressed, zero mistakes -> 100.0% accuracy"
    );
    assert!(
        contains(&out, b"WPM: 0.0"),
        "correct_chars is 0 (Tab doesn't count), so WPM must be exactly 0.0 \
         even though the whitespace-skip started the timer, got: {}",
        String::from_utf8_lossy(&out)
    );
    assert!(!contains(&out, b"Congratulations"));
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// PRD Test Plan #5 / UC-3: quitting early after a single wrong keypress
// reports partial, exact accuracy (0.0%, since the lone keys_pressed was
// also the lone mistake) and exactly 0.0 WPM - a single wrong keypress
// contributes nothing to correct_chars, so the numerator is
// deterministically zero regardless of elapsed time.
// ---------------------------------------------------------------------
#[test]
fn early_quit_after_mistake_reports_zero_accuracy_and_zero_wpm() {
    let mut s = Session::spawn(&fixture("basic.txt")); // "ab"
    s.wait_for("(0/2)");

    s.send_str("z"); // wrong keypress against 'a'
    s.wait_for("[miss 1/3]");
    s.send(b"\x1b"); // Esc

    let out = s.wait_for("Keys pressed: 1, Mistakes: 1");
    assert!(
        contains(&out, b"Accuracy: 0.0%"),
        "expected (1-1)/1 = 0.0% exactly, got: {}",
        String::from_utf8_lossy(&out)
    );
    assert!(
        contains(&out, b"WPM: 0.0"),
        "a single wrong keypress contributes nothing to correct_chars, so \
         WPM must be exactly 0.0, got: {}",
        String::from_utf8_lossy(&out)
    );
    assert!(!contains(&out, b"Congratulations"));
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}

// ---------------------------------------------------------------------
// A full three-strike sequence (strike-out on the 3rd miss) is a real
// typing attempt like any other, so it still contributes to accuracy the
// same way ordinary misses do: 3 keys_pressed, 3 mistakes -> 0.0%
// accuracy, and the struck-out advance is not a "correct" attempt, so it
// contributes nothing to correct_chars either.
// ---------------------------------------------------------------------
#[test]
fn full_strike_out_sequence_reports_zero_accuracy_and_zero_wpm() {
    // fixture: "abc"
    let mut s = Session::spawn(&fixture("abc.txt"));
    s.wait_for("(0/3)");

    s.send_str("x");
    s.wait_for("[miss 1/3]");
    s.send_str("y");
    s.wait_for("[miss 2/3]");
    s.send_str("z");
    s.wait_for("33% (1/3)"); // strike-out on 'a', advanced to 'b'

    s.send(b"\x1b"); // Esc: quit early rather than complete the file
    let out = s.wait_for("Keys pressed: 3, Mistakes: 3");
    assert!(
        contains(&out, b"Accuracy: 0.0%"),
        "expected (3-3)/3 = 0.0% exactly, got: {}",
        String::from_utf8_lossy(&out)
    );
    assert!(
        contains(&out, b"WPM: 0.0"),
        "a strike-out advance is not a correct attempt, so correct_chars \
         stays 0 and WPM must be exactly 0.0, got: {}",
        String::from_utf8_lossy(&out)
    );
    assert!(!contains(&out, b"Congratulations"));
    assert!(s.wait_exit_success(Duration::from_secs(3)));
}
