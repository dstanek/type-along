# type-along

A terminal typing-practice tool that uses your own files as the source text —
Markdown docs, source code, anything readable. Instead of drilling on random
word lists, you retype real material and get the muscle memory for the syntax
and vocabulary you actually work with.

The file is rendered with syntax highlighting, dimmed. As you type each
character correctly it lights up to its normal color, so the screen fills in
behind you.

## Requirements

Rust (edition 2024) and Cargo.

## Build

```sh
cargo build --release
```

## Usage

```sh
type-along FILE
```

For example:

```sh
cargo run -- src/main.rs
cargo run -- README.md
```

The application takes over the terminal (alternate screen), so your shell
scrollback is left untouched when you exit.

## Keys

| Key | Action |
| --- | --- |
| correct character | Advances the cursor and lights the character up to its normal color |
| wrong character | Counts as a miss — see below; does not advance the cursor by itself |
| `Enter` | Types the end of a line; against anything else it counts as a miss, same as a wrong character |
| `Tab` | Skips the run of spaces/tabs under the cursor (handy for indented code); never counts as a miss |
| `Esc`, `Ctrl-X`, or `Ctrl-C` | Quit |

Arrow/function keys and other Ctrl/Alt chords (e.g. Ctrl-A) are ignored and
never count as a miss. Shift+letter is typed normally, as the resulting
capital letter.

A wrong keypress is never a silent no-op. Three consecutive wrong keypresses
at the same position mark that character red and force the cursor past it,
so a single untypeable character (an em dash with no compose key, for
example) can't block you forever. Struck-out characters count toward
completion the same as typed ones. While a strike is pending, the status bar
shows `[miss 1/3]` or `[miss 2/3]`; the counter resets on any advance,
whether from a correct character, a Tab whitespace-skip, or a strike-out. If
the character being struck out is the newline you'd type with `Enter`, a red
`$` is drawn one column past the end of the line instead, since there's no
glyph there to color.

The bottom line is a status bar showing the filename, percent complete,
character position, and the miss counter when one is pending.

Finishing the whole file prints `Congratulations! You've completed the
file.` after the terminal is restored, so it lands in your normal scrollback
rather than the alternate screen. Quitting early (`Esc`, `Ctrl-X`, or
`Ctrl-C`) skips this message.

## Current state

This is a work in progress against `REQUIREMENTS.md`. Not yet implemented:

- Multiple files in one session (`type-along a.md b.rs`)
- Scrolling — only the lines that fit on screen are shown
- WPM / CPS timing and the per-character mistake report

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
