use anyhow::Result;
use clap::Parser;
use crossterm::{
    cursor::{self, DisableBlinking, EnableBlinking},
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::{Color, ResetColor, SetBackgroundColor, SetForegroundColor},
    terminal::{self, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    collections::HashSet,
    fs,
    io::{Write, stdout},
    path::PathBuf,
};
use syntect::{
    easy::HighlightLines,
    highlighting::{Style, ThemeSet},
    parsing::SyntaxSet,
};

#[derive(Parser)]
#[command(name = "type-along")]
#[command(about = "A typing practice application using an existing file")]
struct Args {
    #[arg(help = "File to practice typing with")]
    file: PathBuf,
}

struct TypingSession {
    file: PathBuf,
    current_content: String,
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
    current_position: usize,
    incorrect_attempts: usize, // consecutive wrong keypresses at current_position
    struck_out_positions: HashSet<usize>, // positions force-advanced past; render red forever
    completed: bool, // set when the file was finished; printed after the terminal is restored
    keys_pressed: usize, // real typing attempts only: Char/Enter attempts plus whitespace-skipping Tab presses
    mistakes: usize, // every wrong Char/Enter attempt, including all misses in a strike-out sequence
}

impl TypingSession {
    fn new(file: PathBuf) -> Result<Self> {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();

        Ok(Self {
            file,
            current_content: String::new(),
            syntax_set,
            theme_set,
            current_position: 0,
            incorrect_attempts: 0,
            struck_out_positions: HashSet::new(),
            completed: false,
            keys_pressed: 0,
            mistakes: 0,
        })
    }

    fn load_file(&mut self) -> Result<()> {
        self.current_content = fs::read_to_string(&self.file)?;
        Ok(())
    }

    fn get_file_extension(&self) -> &str {
        self.file
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("txt")
    }

    fn get_cursor_screen_position(&self) -> (u16, u16) {
        let mut line = 0;
        let mut col = 0;
        let chars: Vec<char> = self.current_content.chars().collect();

        for (i, &ch) in chars.iter().enumerate() {
            if i >= self.current_position {
                break;
            }
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }

        (col as u16, line as u16)
    }

    fn get_current_char(&self) -> Option<char> {
        self.current_content.chars().nth(self.current_position)
    }

    fn update_just_typed_character(&self) -> Result<()> {
        // Get the character that was just typed (current_position - 1)
        let typed_pos = self.current_position - 1;

        if let Some(ch) = self.current_content.chars().nth(typed_pos) {
            // Calculate screen position for the typed character
            let (col, row) = self.get_screen_position_for_char(typed_pos);
            let (_width, height) = terminal::size()?;
            let content_height = height - 1;

            // Only update if visible on screen
            if row < content_height {
                // Red if struck out, otherwise the normal (non-dimmed) color
                let color = self.color_for_typed_position(typed_pos)?;

                // Move to the character position and redraw it with normal color
                execute!(
                    stdout(),
                    cursor::MoveTo(col, row),
                    SetForegroundColor(color)
                )?;
                print!("{ch}");
                execute!(stdout(), ResetColor)?;
            }
        }

        Ok(())
    }

    fn get_screen_position_for_char(&self, char_pos: usize) -> (u16, u16) {
        let mut line = 0;
        let mut col = 0;
        let chars: Vec<char> = self.current_content.chars().collect();

        for (i, &ch) in chars.iter().enumerate() {
            if i >= char_pos {
                break;
            }
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }

        (col as u16, line as u16)
    }

    fn get_normal_color_for_position(&self, char_pos: usize) -> Result<Color> {
        // Find which line contains this character position
        let lines: Vec<&str> = self.current_content.lines().collect();
        let mut current_pos = 0;

        for line in &lines {
            let line_char_count = line.chars().count();
            let line_end_pos = current_pos + line_char_count;

            if char_pos >= current_pos && char_pos < line_end_pos {
                // Character is in this line - get syntax highlighting for the line
                let extension = self.get_file_extension();
                let syntax = self
                    .syntax_set
                    .find_syntax_by_extension(extension)
                    .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

                let theme = &self.theme_set.themes["base16-ocean.dark"];
                let mut highlighter = HighlightLines::new(syntax, theme);
                let ranges = highlighter.highlight_line(line, &self.syntax_set)?;

                // Find which syntax range contains our character
                let char_in_line = char_pos - current_pos;
                let mut range_char_pos = 0;

                for (style, text) in ranges {
                    let range_len = text.chars().count();
                    if char_in_line >= range_char_pos && char_in_line < range_char_pos + range_len {
                        let Style { foreground, .. } = style;
                        return Ok(Color::Rgb {
                            r: foreground.r,
                            g: foreground.g,
                            b: foreground.b,
                        });
                    }
                    range_char_pos += range_len;
                }
                break;
            }

            current_pos = line_end_pos + 1; // +1 for the newline character
        }

        // Default to white if we can't determine the color
        Ok(Color::White)
    }

    // Single place that decides red-vs-normal for an already-passed position.
    // update_character_at_position and update_just_typed_character both go
    // through here so a struck-out position can never be repainted back to
    // its normal syntax color by a later redraw.
    fn color_for_typed_position(&self, char_pos: usize) -> Result<Color> {
        if self.struck_out_positions.contains(&char_pos) {
            Ok(Color::Red)
        } else {
            self.get_normal_color_for_position(char_pos)
        }
    }

    fn update_status_and_cursor(&self) -> Result<()> {
        let (_width, height) = terminal::size()?;
        let content_height = height - 1;

        // Update status bar
        self.display_status_bar(height - 1)?;

        // Position cursor at current character
        let (col, row) = self.get_cursor_screen_position();
        if row < content_height {
            execute!(stdout(), cursor::MoveTo(col, row))?;
        }

        stdout().flush()?;
        Ok(())
    }

    fn skip_whitespace(&mut self) -> Result<()> {
        // Called only when a whitespace-skip is actually happening (the
        // caller already checked the current char is whitespace), so this
        // counts once per Tab press regardless of how many characters it skips.
        self.keys_pressed += 1;

        let chars: Vec<char> = self.current_content.chars().collect();
        let start_position = self.current_position;

        // Skip forward through whitespace characters (space, tab)
        while self.current_position < chars.len() {
            let ch = chars[self.current_position];
            if ch != ' ' && ch != '\t' {
                break;
            }
            self.current_position += 1;
        }

        // Update all skipped characters to normal color
        for pos in start_position..self.current_position {
            self.update_character_at_position(pos)?;
        }

        // Position advanced - reset the miss counter like any other advance
        self.incorrect_attempts = 0;

        // Update status bar and cursor position
        self.update_status_and_cursor()?;

        Ok(())
    }

    fn update_character_at_position(&self, char_pos: usize) -> Result<()> {
        if let Some(ch) = self.current_content.chars().nth(char_pos) {
            // Get the screen position for this character
            let (col, row) = self.get_screen_position_for_char(char_pos);
            let (_width, height) = terminal::size()?;
            let content_height = height - 1;

            // Only update if the character is visible on screen
            if row < content_height {
                // Red if struck out, otherwise the correct syntax-highlighting color
                let color = self.color_for_typed_position(char_pos)?;

                // Move to character position and print with normal color
                execute!(
                    stdout(),
                    cursor::MoveTo(col, row),
                    SetForegroundColor(color)
                )?;
                print!("{ch}");
                execute!(stdout(), ResetColor)?;
            }
        }
        Ok(())
    }

    fn display_content(&self) -> Result<()> {
        let (_width, height) = terminal::size()?;
        let content_height = height - 1; // Reserve bottom line for status bar

        // Clear screen and move to top
        execute!(
            stdout(),
            terminal::Clear(ClearType::All),
            cursor::MoveTo(0, 0)
        )?;

        // Get syntax highlighting setup
        let extension = self.get_file_extension();
        let syntax = self
            .syntax_set
            .find_syntax_by_extension(extension)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let theme = &self.theme_set.themes["base16-ocean.dark"];
        let mut highlighter = HighlightLines::new(syntax, theme);

        // Display only as many lines as will fit on screen
        let lines: Vec<&str> = self.current_content.lines().collect();
        let lines_to_show = std::cmp::min(content_height as usize, lines.len());

        for (row, line) in lines.iter().take(lines_to_show).enumerate() {
            execute!(stdout(), cursor::MoveTo(0, row as u16))?;

            let ranges = highlighter.highlight_line(line, &self.syntax_set)?;

            for (style, text) in ranges {
                let Style { foreground, .. } = style;
                let dimmed_color = Color::Rgb {
                    r: foreground.r / 2 + 40,
                    g: foreground.g / 2 + 40,
                    b: foreground.b / 2 + 40,
                };

                execute!(stdout(), SetForegroundColor(dimmed_color))?;
                print!("{text}");
            }

            execute!(stdout(), ResetColor)?;
        }

        // Display status bar
        self.display_status_bar(height - 1)?;

        // Position cursor at current character
        let (col, row) = self.get_cursor_screen_position();
        if row < content_height {
            execute!(stdout(), cursor::MoveTo(col, row))?;
        }

        stdout().flush()?;
        Ok(())
    }

    fn display_status_bar(&self, row: u16) -> Result<()> {
        let (width, _) = terminal::size()?;

        // Move to status bar row and set background
        execute!(
            stdout(),
            cursor::MoveTo(0, row),
            SetBackgroundColor(Color::Blue),
            SetForegroundColor(Color::White)
        )?;

        // Get filename from current file
        let filename = self
            .file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        // Create status bar content with progress
        let total_chars = self.current_content.chars().count();
        let progress_percent = if total_chars > 0 {
            (self.current_position * 100) / total_chars
        } else {
            0
        };
        let mut status_info = format!(
            " {} - {}% ({}/{}) ",
            filename, progress_percent, self.current_position, total_chars
        );

        // Surface the miss count instead of a terminal bell (bell audible/
        // flash/suppressed behavior is outside the app's control). ASCII
        // only - status_info.len() below is a byte count used for padding.
        if self.incorrect_attempts > 0 {
            status_info.push_str(&format!("[miss {}/3] ", self.incorrect_attempts));
        }

        // Print status info and fill the rest of the line
        print!("{status_info}");
        for _ in status_info.len()..(width as usize) {
            print!(" ");
        }

        execute!(stdout(), ResetColor)?;
        Ok(())
    }

    // Shared match/mismatch handling for a keypress evaluated against the
    // expected character at current_position (used by both Char and Enter).
    fn handle_expected_char_attempt(&mut self, matches: bool) -> Result<bool> {
        // Every call here is a real typing attempt (Char or Enter evaluated
        // against the expected character), win or lose.
        self.keys_pressed += 1;

        if !matches {
            return self.record_incorrect_attempt();
        }

        self.current_position += 1;
        self.incorrect_attempts = 0;

        // Update just the typed character to normal color
        self.update_just_typed_character()?;

        // Update status bar and cursor position
        self.update_status_and_cursor()?;

        // Check if we've reached the end of the file
        if self.current_position >= self.current_content.chars().count() {
            self.completed = true;
            return Ok(false);
        }

        Ok(true)
    }

    // Records a wrong keypress at the current position. Misses are routine
    // in a typing trainer, so feedback here is visual only (a status-bar
    // strike counter) - terminal bell behavior (audible / visual flash /
    // suppressed) is outside the app's control, so no bell is rung.
    fn record_incorrect_attempt(&mut self) -> Result<bool> {
        self.incorrect_attempts += 1;
        self.mistakes += 1;

        if self.incorrect_attempts < 3 {
            // No advance, no redraw - just surface the updated miss count.
            self.update_status_and_cursor()?;
            return Ok(true);
        }

        // Third consecutive miss: strike out this position and force-advance
        // past it so no character can ever permanently block completion.
        let pos = self.current_position;
        let expected_char = self.get_current_char();
        self.struck_out_positions.insert(pos);

        if expected_char == Some('\n') {
            // Mark the end of the line with a red '$' one column past the
            // last character, skipped if that column is off-screen.
            let (col, row) = self.get_screen_position_for_char(pos);
            let (width, height) = terminal::size()?;
            let content_height = height - 1;
            if row < content_height && col < width {
                execute!(
                    stdout(),
                    cursor::MoveTo(col, row),
                    SetForegroundColor(Color::Red)
                )?;
                print!("$");
                execute!(stdout(), ResetColor)?;
            }
        } else {
            // Redraw the real glyph; color_for_typed_position sees the
            // struck-out entry just inserted above and renders it red.
            self.update_character_at_position(pos)?;
        }

        self.current_position += 1;
        self.incorrect_attempts = 0;
        self.update_status_and_cursor()?;

        if self.current_position >= self.current_content.chars().count() {
            self.completed = true;
            return Ok(false);
        }

        Ok(true)
    }

    fn handle_keypress(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc => {
                return Ok(false);
            }
            KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(false);
            }
            // Raw mode disables ISIG, so the terminal never turns Ctrl-C
            // into SIGINT here - without this arm it's just another
            // swallowed control chord and there's no other way to quit.
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(false);
            }
            // Other Ctrl/Alt + letter chords (e.g. Ctrl-A) are terminal
            // control input - ignore them like any other non-character
            // key. Shift is deliberately not filtered here: Shift+letter is
            // a real typing attempt (capital letters) and must still count
            // as a match or a miss.
            KeyCode::Char(_)
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {}
            // Char and Enter share one expected-character comparison path:
            // on a match, advance as before; on a mismatch, Enter is
            // evaluated as a wrong "character" against the expected one
            // exactly like any other Char (UC-5) rather than a silent no-op.
            KeyCode::Char(ch) => {
                if let Some(expected_char) = self.get_current_char() {
                    return self.handle_expected_char_attempt(ch == expected_char);
                }
            }
            KeyCode::Enter => {
                if let Some(expected_char) = self.get_current_char() {
                    return self.handle_expected_char_attempt(expected_char == '\n');
                }
            }
            KeyCode::Tab => {
                // Skip all whitespace (spaces and tabs) until next non-whitespace character
                if let Some(current_char) = self.get_current_char() {
                    if current_char == ' ' || current_char == '\t' {
                        self.skip_whitespace()?;

                        // Check if we've reached the end of the file
                        if self.current_position >= self.current_content.chars().count() {
                            self.completed = true;
                            return Ok(false);
                        }
                    }
                }
            }
            _ => {
                // Ignore other keys
            }
        }
        Ok(true)
    }

    // Printed once, after run() has restored the terminal (left the
    // alternate screen and disabled raw mode) - printing it before that
    // made it invisible, since run() immediately tore the alternate screen
    // down again microseconds later. No terminal geometry needed here
    // anymore, so no cursor positioning against `terminal::size()`.
    fn show_completion_message(&self) -> Result<()> {
        execute!(stdout(), SetForegroundColor(Color::Green))?;
        println!("Congratulations! You've completed the file.");
        execute!(stdout(), ResetColor)?;
        stdout().flush()?;
        Ok(())
    }

    // Printed once, after run() has restored the terminal, alongside (or in
    // place of) show_completion_message - see the call site in run() for the
    // exact conditions. Plain default color: this is a data line, not the
    // celebratory message, so it stays visually distinct from the green
    // congratulations text.
    fn show_stats_report(&self) -> Result<()> {
        println!(
            "Keys pressed: {}, Mistakes: {}",
            self.keys_pressed, self.mistakes
        );
        stdout().flush()?;
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        // If we panic once the alternate screen / raw mode is active, the
        // normal restore below never runs - the terminal is left in raw
        // mode on the alternate screen with no cooked-mode shell, and raw
        // mode has disabled Ctrl-C too, so it looks like a hang. Restore
        // terminal state before the default panic message prints, then
        // chain to the previous hook so the message still reaches the user.
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = execute!(stdout(), DisableBlinking);
            let _ = terminal::disable_raw_mode();
            let _ = execute!(stdout(), LeaveAlternateScreen);
            previous_hook(info);
        }));

        let result = self.event_loop();

        // Restore terminal state unconditionally - on a normal finish, an
        // early quit, AND an error (e.g. the practice file disappearing out
        // from under load_file() after the alternate screen/raw mode are
        // already on). Without this, any `?` in event_loop() would return
        // straight past the restore and leave the terminal wedged in raw
        // mode with Ctrl-C disabled - the panic hook above doesn't cover
        // this, since nothing panicked. Errors from restoring itself are
        // discarded so they can't mask the real failure below.
        let _ = execute!(stdout(), DisableBlinking);
        let _ = terminal::disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);

        result?;

        // Only after the restore above, so it lands in the user's normal
        // scrollback instead of the alternate screen it would otherwise be
        // torn down with. Never printed on an early quit (Esc/Ctrl-X/Ctrl-C)
        // or when event_loop() returned an error.
        if self.completed {
            self.show_completion_message()?;
        }

        // Printed on both completion and early quit (Esc/Ctrl-X/Ctrl-C),
        // independent of self.completed - only suppressed when event_loop()
        // returned an error, matching the guard on show_completion_message
        // above but without the `self.completed` condition.
        self.show_stats_report()?;

        Ok(())
    }

    // The fallible part of a session: enter the alternate screen, enable
    // raw mode, load and display the file, then run the keypress loop.
    // Isolated from run() so run() can restore the terminal exactly once,
    // unconditionally, regardless of how this returns.
    fn event_loop(&mut self) -> Result<()> {
        // Enter alternate screen to preserve original terminal contents
        execute!(stdout(), EnterAlternateScreen)?;

        // Enable raw mode to capture keypresses
        terminal::enable_raw_mode()?;

        // Enable blinking cursor
        execute!(stdout(), EnableBlinking)?;

        self.load_file()?;
        self.display_content()?;

        // Event loop to capture keypresses
        loop {
            if let Event::Key(key) = event::read()? {
                if !self.handle_keypress(key)? {
                    break;
                }
            }
        }

        Ok(())
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    if !args.file.exists() {
        eprintln!("Error: File '{}' does not exist", args.file.display());
        std::process::exit(1);
    }

    let mut session = TypingSession::new(args.file)?;
    session.run()?;

    Ok(())
}
