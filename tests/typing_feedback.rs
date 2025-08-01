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
