# Type Along

This command line application enables the user to practice typing using existing files.
For example, Markdown documentation or even code.

## Overall requirements

- User will provider one or more files as command line arguments. e.g. type-along DOCS.md code.rs
- The application will take over the terminal for the interface
- The file will be output to the screen using syntax highlighting, but the coloring should be slightly darker (almost grayed out)
- There will be a blinking square cursor over the the letter that the user should be typing.
- If the user types the letter correctly then the color returns to normal color instead of the grayed out version
- If the user types incorrectly then the charaction should show up as red
- When the user types the cursor is advanced even if they don't type the right character
- When the user begins they will be on line 1, but once they get halfway down the screen they will stay on the middle line until there is no more file to scroll.

> **Note (implementation divergence):** the shipped behavior is three-strikes,
> not immediate advance — a wrong character is counted, and the cursor only
> advances past it (marking it red) after the third consecutive miss at that
> position. Advancing on the very first typo would race ahead of a user who
> simply fumbled a key; three strikes reserves the skip for characters that
> are genuinely untypeable.

## Status Line Requirments
- The last line in the terminal will be a status line.
- It should have a background color to set it apart from the typing interface
- The bar should show the current filename, the number of the file (e.g. 2/5), and the percentage through the current file

## Reporting Requirements
- As soon as the user begins typing a timer should start so that we can calculate "words per minutes" and "characters per second"
- The report of WPM and CPS will be shown when the user completes typing
- We should also keep track of the mistakes a user makes. So a mapping of missed character to the count of misses. This will help the user understand their weaknesses.

> **Note (implementation divergence):** the shipped one-line end-of-run
> report, printed after the terminal is restored on both completion and
> early quit, now shows total keys pressed, total mistakes, accuracy, and
> WPM. CPS and the per-character mistake map above remain unimplemented.

## Technology

- This CLI will be written in rust
