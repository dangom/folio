# folio

A simple Markdown and Org file parser designed to be used for quick pagination  in `less`.

It's designed to be fast. On a Macbook air it reaches  ~250 MB/s so this README.md for example would render in less than 1ms.

## Installation

```sh
cargo build --release
install -m 755 target/release/folio ~/.local/bin/
```

## Usage

Either directly, but more useful to  hook it into `less` from your shell rc:

```sh
export LESSOPEN="|$HOME/.local/bin/folio -- %s"
export LESS=-R   # let less pass the styling through
```

Then `less README.org` or `less README.md` renders the file.
Since piped input has no file extension, however, folio  guesses that a file is markdown or org from the content of the text (it tries to identify programming languages and leave them alone). With that said `--md` or `--org` can be used as flags to force rendering in case of a false negative.

```sh
folio README.md | less -R
curl -s https://example.com/README.md | folio | less -R
cat script.sh | folio   # unchanged
```

## Aesthetics

Meant to look like ink on paper.

- greys only, with weight carrying the hierarchy; colour is kept for warnings
- headings flush with the text, their level (`H1`, `H2`, …) in the margin
- a rule under the title across the page, and under each H1 as wide as its text
- prose wrapped at 80 columns, lists and quotes hanging under their text
- markup and metadata faded: drawers, keywords, dates, tags, comments, URLs
- typographic bullets and check boxes, and booktabs tables

Code and tables never wrapped though.

The palette follows the terminal background (`COLORFGBG`). Set
`FOLIO_LIGHT` to `day`, `night` or `plain` to override.`plain` will use
bold and dim only, for terminals without truecolor.
