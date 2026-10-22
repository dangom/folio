//! folio: render Markdown or Org as ANSI-styled text for `less -R`.
//!
//! Zero dependencies and line-oriented: each input line is rendered on its
//! own, prose word-wrapped to an 80-column measure, while code, tables and
//! metadata stay one line each.
//!
//! The look: greys only, weight carrying the hierarchy, headings flush with
//! the text and an H1, H2… marker hanging in the left margin, booktabs tables,
//! and colour kept for warnings and errors.

use std::collections::HashMap;
use std::io::{self, IsTerminal, Read, StdoutLock, Write};
use std::path::Path;
use std::process::ExitCode;
use std::{env, fs};

const USAGE: &str = "\
usage: folio [--md | --org] [FILE | -]

Render Markdown or Org as ANSI text for `less -R`. Files with other extensions
produce no output (exit 1), so less falls back to showing them as is:

    export LESSOPEN='|folio -- %s'

Standard input is rendered when it reads as Markdown or Org (--md or --org to
insist), and passed through unchanged otherwise.

The palette follows the terminal background (COLORFGBG); set
FOLIO_LIGHT=day, night or plain to choose one.
";

/// Width of the left margin, where heading markers (H1, H2, ...) hang.
const MARGIN: usize = 4;
/// The text column: prose wraps and rules stop there.
const MEASURE: usize = 80;

/// The roles that carry a colour, as SGR parameters. Plain text is in the
/// terminal's own foreground.
struct Palette {
    heading: &'static str, // strong, bold
    done: &'static str,    // a DONE heading: faded, bold
    grey: &'static str,    // code and verbatim
    quote: &'static str,   // grey, italic
    faded: &'static str,   // markup, metadata, dates, tags, rules
}

// Greys for a light (day) and a dark (night) background.
static DAY: Palette = Palette {
    heading: "1;38;2;10;10;10",
    done: "1;38;2;101;102;102",
    grey: "38;2;69;70;69",
    quote: "3;38;2;69;70;69",
    faded: "38;2;101;102;102",
};
static NIGHT: Palette = Palette {
    heading: "1;38;2;245;244;243",
    done: "1;38;2;173;171;167",
    grey: "38;2;218;217;214",
    quote: "3;38;2;218;217;214",
    faded: "38;2;173;171;167",
};
/// For terminals without truecolor: weight and dim only.
static PLAIN: Palette = Palette { heading: "1", done: "1;2", grey: "2", quote: "3;2", faded: "2" };

const BOLD: &str = "1";
const ITALIC: &str = "3";
const BOLD_ITALIC: &str = "1;3";
const UNDERLINE: &str = "4";
const STRIKE: &str = "9";
const LINK: &str = "4";
const WARNING: &str = "1;33";
const ERROR: &str = "1;31";

const TODO_WORDS: [&[u8]; 11] =
    [b"TODO", b"NEXT", b"PROJ", b"LOOP", b"STRT", b"STARTED", b"WAIT", b"WAITING", b"HOLD", b"IDEA", b"SOMEDAY"];
const DONE_WORDS: [&[u8]; 4] = [b"DONE", b"KILL", b"CANCELLED", b"CANCELED"];
const PLANNING: [&[u8]; 3] = [b"SCHEDULED:", b"DEADLINE:", b"CLOSED:"];

/// Input past this size is left to less itself, which pages anything lazily.
const MAX_INPUT: usize = 256 << 20;
/// Inline nesting deeper than this is no document: the rest is shown as is.
const MAX_DEPTH: usize = 16;
/// Failed scans for a closing bracket or backtick run allowed per line, before
/// those are taken literally; keeps pathological lines from going quadratic.
const MAX_MISSES: usize = 32;
/// The longest code span looked for, in bytes.
const MAX_SPAN: usize = 4096;
/// How much of piped input to read before guessing its format.
const SNIFF: usize = 64 << 10;
/// Words that open lines of code far more often than lines of prose.
const CODE_WORDS: [&[u8]; 44] = [
    b"import", b"from", b"def", b"class", b"return", b"fn", b"pub", b"use", b"let", b"const", b"var",
    b"function", b"struct", b"impl", b"enum", b"package", b"func", b"if", b"elif", b"else", b"fi", b"then",
    b"for", b"while", b"do", b"done", b"case", b"esac", b"export", b"local", b"echo", b"alias", b"source",
    b"set", b"unset", b"print", b"printf", b"try", b"catch", b"end", b"#include", b"#define", b"#ifdef",
    b"#endif",
];

static MD_SPECIAL: [bool; 256] = byte_set(b"\\`*_~![<&:");
static ORG_SPECIAL: [bool; 256] = byte_set(b"*/_=~+[<:");

#[derive(Clone, Copy)]
enum Format {
    Md,
    Org,
}

fn main() -> ExitCode {
    let (mut format, mut path, mut options) = (None, None, true);
    for arg in env::args_os().skip(1) {
        match arg.to_str().filter(|_| options) {
            Some("--") => options = false, // what follows is a file name, even `--org`
            Some("--md" | "--markdown") => format = Some(Format::Md),
            Some("--org") => format = Some(Format::Org),
            Some("-h" | "--help") => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ => path = Some(arg),
        }
    }
    let src = match path.filter(|p| p.to_str() != Some("-")) {
        Some(p) => {
            // Not ours to render: print nothing so less shows the file as is.
            let Some(f) = format.or_else(|| detect(Path::new(&p))) else {
                return ExitCode::FAILURE;
            };
            format = Some(f);
            // Only regular files of sane size: less pages devices, pipes and giants itself.
            if !fs::metadata(&p).is_ok_and(|m| m.is_file() && m.len() <= MAX_INPUT as u64) {
                return ExitCode::FAILURE;
            }
            match fs::read(&p) {
                Ok(s) => s,
                Err(_) => return ExitCode::FAILURE,
            }
        }
        None => {
            if io::stdin().is_terminal() {
                eprint!("{USAGE}");
                return ExitCode::from(2);
            }
            let mut stdin = io::stdin().lock();
            let mut s = Vec::new();
            if stdin.by_ref().take(SNIFF as u64).read_to_end(&mut s).is_err() {
                return ExitCode::FAILURE;
            }
            // Piped input has no name to go by: guess, and pass it through unless sure.
            let Some(f) = format.or_else(|| sniff(&s)) else {
                return pass(&s, &mut stdin);
            };
            format = Some(f);
            if stdin.by_ref().take((MAX_INPUT + 1 - s.len()) as u64).read_to_end(&mut s).is_err() {
                return ExitCode::FAILURE;
            }
            if s.len() > MAX_INPUT {
                return pass(&s, &mut stdin);
            }
            s
        }
    };
    let src = defang(src);
    let mut ink = Ink {
        out: Vec::with_capacity(1 << 17),
        stack: Vec::with_capacity(8),
        sink: io::stdout().lock(),
        p: palette(),
        bol: true,
        width: term_width().min(MARGIN + MEASURE),
        line: 0,
        hang: None,
        widest: 0,
        depth: 0,
        wrap: Wrap::default(),
    };
    match format.unwrap_or(Format::Md) {
        Format::Md => markdown(&src, &mut ink),
        Format::Org => org(&src, &mut ink),
    }
    ink.flush();
    ExitCode::SUCCESS
}

/// Copy input through untouched, as cat would.
fn pass(head: &[u8], rest: &mut impl Read) -> ExitCode {
    let mut out = io::stdout().lock();
    let _ = out.write_all(head).and_then(|()| io::copy(rest, &mut out));
    ExitCode::SUCCESS
}

/// Make control characters visible, so a document can't drive the terminal:
/// no escape sequences, no carriage return overprinting a line, no C1
/// controls, and no bidi overrides displaying text other than as it reads.
fn defang(src: Vec<u8>) -> Vec<u8> {
    let (mut out, mut done, mut k) = (None::<Vec<u8>>, 0, 0);
    while k < src.len() {
        let b = src[k];
        let len = match b {
            b'\t' | b'\n' => 0,
            b'\r' if src.get(k + 1) == Some(&b'\n') => 0,
            0..=0x1f | 0x7f => 1,
            0xC2 if matches!(src.get(k + 1), Some(0x80..=0x9f)) => 2,
            0xE2 if matches!(src.get(k + 1..k + 3), Some([0x80, 0xAA..=0xAE] | [0x81, 0xA6..=0xA9])) => 3,
            _ => 0,
        };
        if len == 0 {
            k += 1;
            continue;
        }
        let o = out.get_or_insert_with(|| Vec::with_capacity(src.len() + 64));
        o.extend_from_slice(&src[done..k]);
        if len == 1 {
            o.extend_from_slice(&[b'^', b ^ 0x40]); // ESC shows as ^[, as in less
        } else {
            o.extend_from_slice("\u{FFFD}".as_bytes());
        }
        k += len;
        done = k;
    }
    match out {
        Some(mut o) => {
            o.extend_from_slice(&src[done..]);
            o
        }
        None => src,
    }
}

fn detect(path: &Path) -> Option<Format> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "md" | "markdown" | "mdown" | "mkd" | "mkdn" | "mdx" => Some(Format::Md),
        "org" => Some(Format::Org),
        _ => None,
    }
}

/// Guess from its opening lines whether piped input is Markdown or Org, by
/// weighing their marks against those of code and config. Anything unclear is
/// None, to be passed through as is: a false guess mangles, a missed one doesn't.
fn sniff(head: &[u8]) -> Option<Format> {
    let start = head.trim_ascii_start();
    if head.starts_with(b"#!")
        || head.contains(&0)
        || [&b"<!doctype"[..], b"<html", b"<?xml"].iter().any(|p| starts_with_ic(start, p))
    {
        return None;
    }
    let (mut md, mut org, mut code) = (0, 0, 0);
    // Inside a code fence or an Org block, only the delimiters count.
    let (mut fenced, mut block, mut comment) = (false, false, false);
    let (mut prev_blank, mut prev_text, mut prev_hash) = (true, false, false);
    // Front matter is YAML or TOML, and says nothing about what follows.
    let mut head = head;
    for fence in [&b"---\n"[..], b"+++\n"] {
        if let Some(rest) = head.strip_prefix(fence)
            && let Some(end) = find(rest, &[b"\n", fence].concat())
        {
            head = &rest[end + fence.len() + 1..];
        }
    }
    for line in head.split(|&b| b == b'\n').take(500) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let (ind, t) = indent(line);
        let t = t.trim_ascii_end();
        if t.is_empty() {
            (prev_blank, prev_text, prev_hash) = (true, false, false);
            continue;
        }
        if fence_open(t).is_some() {
            fenced = !fenced;
            md += 2;
            continue;
        }
        if starts_with_ic(t, b"#+begin_") || starts_with_ic(t, b"#+end_") {
            block = starts_with_ic(t, b"#+begin_");
            org += 3;
            continue;
        }
        if fenced || block {
            continue;
        }
        if t.starts_with(b"diff --git") || t.starts_with(b"@@ -") {
            return None;
        }
        // Minified code: long lines with next to no spaces.
        if t.len() > 500 && t.iter().filter(|&&b| b == b' ').count() * 20 < t.len() {
            code += 10;
            continue;
        }
        // HTML turns up in both, and weighs for neither.
        if t[0] == b'<' && t.ends_with(b">") {
            continue;
        }
        // A C block comment, whose doc text may well be Markdown, or a run of
        // `#` lines, which Markdown headings never form, is code.
        let hash = t[0] == b'#' && !t.starts_with(b"#+");
        if comment || t.starts_with(b"/*") || (hash && prev_hash) {
            comment = (comment || t.starts_with(b"/*")) && find(t, b"*/").is_none();
            code += 1;
            (prev_blank, prev_text, prev_hash) = (false, true, hash);
            continue;
        }

        // Org
        let key = t.strip_prefix(b"#+").map(|k| k.iter().take_while(|b| b.is_ascii_alphabetic()).count());
        if key.is_some_and(|n| n > 0 && t.get(n + 2) == Some(&b':')) {
            org += 3;
        } else if is_drawer(t) || t.eq_ignore_ascii_case(b":END:") {
            org += 2;
        }
        if ind == 0 && line[0] == b'*' {
            let stars = run(line, 0, b'*');
            if line.get(stars) == Some(&b' ') {
                org += stars.min(2); // a lone `* ` may be a Markdown bullet
            }
        }
        if let Some(p) = find(t, b"[[")
            && let Some(q) = find(&t[p..], b"]]")
        {
            let inner = &t[p + 2..p + q];
            if p == 0 && p + q + 2 == t.len() && inner.iter().all(|&b| b.is_ascii_alphanumeric() || b"_-.".contains(&b)) {
                code += 1; // a TOML array of tables
            } else if inner.first().is_some_and(|&b| !is_ws(b)) && inner.last().is_some_and(|&b| !is_ws(b)) {
                org += 2; // a link; shell tests read `[[ -f x ]]`
            }
        }

        // Markdown; a `# heading` counts little, as it reads as a comment too.
        if ind == 0
            && prev_blank
            && let Some((level, text)) = atx(t)
            && !text.is_empty()
        {
            md += if level > 1 { 2 } else { 1 };
        }
        if prev_text && setext(line) == Some(1) {
            md += 2;
        }
        if t.contains(&b'[') && find(t, b"](").is_some() {
            md += 2;
        }
        if t.contains(&b'|') && table_delim(line).is_some() {
            md += 2;
        }
        let item = list_marker(t, true).filter(|_| ind != 1); // ` * ` continues a C comment
        let body = item.map_or(t, |m| t[m..].trim_ascii_start());
        let word = &body[..body.iter().position(|&b| b == b' ' || b == b'(').unwrap_or(body.len())];
        let keyed = word.len() > 1
            && word.ends_with(b":")
            && !word[0].is_ascii_uppercase() // prose says `Note:`, config `name:`
            && word[..word.len() - 1].iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        // YAML lists hold words; Markdown ones, mostly prose.
        let prose = body.split(|&b| b == b' ').filter(|w| !w.is_empty()).count() >= 3;
        if (item.is_some() && !keyed && prose) || t.starts_with(b"> ") {
            md += 1;
        }
        let bold = find(t, b"**").is_some_and(|p| {
            let r = &t[p + 2..];
            r.first().is_some_and(|&b| b != b'*' && !is_ws(b)) && find(r, b"**").is_some()
        });
        if bold || t.iter().filter(|&&b| b == b'`').count() >= 2 {
            md += 1;
        }

        // Code and config
        let assign = word.iter().position(|&b| b == b'=').is_some_and(|p| {
            p > 0 && word[..p].iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_')
        });
        if keyed
            || assign
            || CODE_WORDS.contains(&word)
            || t[0] == b'('
            || matches!(t[t.len() - 1], b';' | b'{' | b'}' | b'\\')
            || [&b"//"[..], b"/*", b"*/", b"$(", b"${", b"&&", b"||", b" = "].iter().any(|p| find(t, p).is_some())
        {
            code += 1;
        }
        (prev_blank, prev_text, prev_hash) = (false, true, hash);
    }
    if org >= 3 && org > md && org > 2 * code {
        Some(Format::Org)
    } else if md >= 4 && 2 * md > 3 * code {
        Some(Format::Md)
    } else {
        None
    }
}

fn palette() -> &'static Palette {
    match env::var("FOLIO_LIGHT").as_deref() {
        Ok("day") => return &DAY,
        Ok("night") => return &NIGHT,
        Ok("plain") => return &PLAIN,
        _ => {}
    }
    if !matches!(env::var("COLORTERM").as_deref(), Ok("truecolor" | "24bit")) {
        return &PLAIN;
    }
    // COLORFGBG is "fg;bg" (or "fg;default;bg"); ANSI 0-6 and 8 are dark.
    let dark = env::var("COLORFGBG")
        .ok()
        .and_then(|v| v.rsplit(';').next()?.parse::<u8>().ok())
        .is_some_and(|bg| bg < 7 || bg == 8);
    if dark { &NIGHT } else { &DAY }
}

fn term_width() -> usize {
    if let Some(n) = env::var("COLUMNS").ok().and_then(|v| v.parse::<usize>().ok()).filter(|&n| n > 0) {
        return n;
    }
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        const TIOCGWINSZ: std::ffi::c_ulong = 0x5413;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        const TIOCGWINSZ: std::ffi::c_ulong = 0x4008_7468;
        // Under LESSOPEN stdout is a pipe, but stderr is usually still the terminal.
        for fd in [2, 0, 1] {
            let mut ws = [0u16; 4]; // rows, cols, xpixel, ypixel
            if unsafe { ioctl(fd, TIOCGWINSZ, ws.as_mut_ptr()) } == 0 && ws[1] > 0 {
                return ws[1] as usize;
            }
        }
    }
    80
}

// ---------------------------------------------------------------- output

/// Output buffer plus the stack of active SGR styles, so a nested span can
/// close without clobbering the styles of its parents.
struct Ink {
    out: Vec<u8>,
    stack: Vec<&'static str>,
    sink: StdoutLock<'static>,
    p: &'static Palette,
    bol: bool,           // at the beginning of a line, before its margin
    width: usize,        // columns prose wraps at, margin included
    line: usize,         // where the current line starts in `out`
    hang: Option<usize>, // where in `out` wrapped prose continues from, if it may wrap
    widest: usize,       // columns of the widest row the last line wrapped into, or 0
    depth: usize,        // how deeply inline spans are nested
    wrap: Wrap,
}

/// Buffers `Ink::wrap` reuses from line to line.
#[derive(Default)]
struct Wrap {
    body: Vec<u8>,               // the text being wrapped
    cont: Vec<u8>,               // what a continuation row starts with
    active: Vec<(usize, usize)>, // styles open since the last reset, as ranges of `body`
    inner: Vec<(usize, usize)>,  // styles changing inside the current word
}

impl Ink {
    /// Let the current line wrap, its continuations lining up with what comes next.
    fn hang(&mut self) {
        debug_assert!(self.stack.is_empty());
        self.begin();
        self.hang = Some(self.out.len());
    }

    /// Open the current line with a blank margin, unless that's done already.
    fn begin(&mut self) {
        if self.bol {
            self.bol = false;
            self.out.extend_from_slice(&[b' '; MARGIN]);
        }
    }

    /// Open the current line with a heading's level marker in the margin.
    fn marker(&mut self, level: usize) {
        self.bol = false;
        let label = format!("{:>w$} ", format!("H{level}"), w = MARGIN - 1);
        self.styled(self.p.faded, label);
    }

    fn raw(&mut self, b: impl AsRef<[u8]>) {
        let b = b.as_ref();
        if !b.is_empty() {
            self.begin();
            self.out.extend_from_slice(b);
        }
    }

    fn pad(&mut self, n: usize) {
        if n > 0 {
            self.begin();
            self.out.resize(self.out.len() + n, b' ');
        }
    }

    fn push(&mut self, style: &'static str) {
        self.begin();
        sgr(&mut self.out, style);
        self.stack.push(style);
    }

    fn pop(&mut self) {
        self.stack.pop();
        self.out.extend_from_slice(b"\x1b[0m");
        for s in &self.stack {
            sgr(&mut self.out, s);
        }
    }

    fn styled(&mut self, style: &'static str, text: impl AsRef<[u8]>) {
        self.push(style);
        self.raw(text);
        self.pop();
    }

    fn nl(&mut self) {
        debug_assert!(self.stack.is_empty());
        self.widest = 0;
        // Bytes bound columns from above, so short lines skip the measuring.
        if let Some(hang) = self.hang.take()
            && self.out.len() - self.line > self.width
        {
            self.wrap(hang);
        }
        self.out.push(b'\n');
        self.bol = true;
        // Stream in chunks so less can paint the first screen of a huge file at once.
        if self.out.len() >= 1 << 16 {
            self.flush();
        }
        self.line = self.out.len();
    }

    /// Word-wrap the current line to `self.width`. Each continuation starts
    /// under the column at `hang`, below a copy of any `│` gutters left of it.
    fn wrap(&mut self, hang: usize) {
        let mut buf = std::mem::take(&mut self.wrap);
        let Wrap { body, cont, active, inner } = &mut buf;
        body.clear();
        body.extend_from_slice(&self.out[hang..]);
        self.out.truncate(hang);

        cont.clear();
        let mut indent = 0;
        let prefix = &self.out[self.line..];
        let mut k = 0;
        while k < prefix.len() {
            let (len, w) = unit(prefix, k);
            if w == 0 || &prefix[k..k + len] == "│".as_bytes() {
                cont.extend_from_slice(&prefix[k..k + len]);
            } else {
                cont.resize(cont.len() + w, b' ');
            }
            indent += w;
            k += len;
        }
        if self.width < indent + 20 {
            self.out.extend_from_slice(body); // too narrow to be worth it
            self.wrap = buf;
            return;
        }

        active.clear();
        let track = |active: &mut Vec<(usize, usize)>, (a, e): (usize, usize)| {
            if &body[a..e] == b"\x1b[0m" {
                active.clear();
            } else {
                active.push((a, e));
            }
        };
        // Copy through in runs: only a break interrupts the text.
        let (mut col, mut k, mut done) = (indent, 0, 0);
        while k < body.len() {
            // The gap before the next word: spaces, and styles opening or closing.
            let gap = k;
            let mut gap_w = 0;
            while k < body.len() {
                match body[k] {
                    b' ' => {
                        gap_w += 1;
                        k += 1;
                    }
                    0x1b => {
                        let e = k + unit(body, k).0;
                        track(active, (k, e));
                        k = e;
                    }
                    _ => break,
                }
            }
            let word = k;
            let mut word_w = 0;
            inner.clear(); // styles changing inside the word, applied once it's placed
            while k < body.len() {
                match body[k] {
                    b' ' => break,
                    0x1b => {
                        let e = k + unit(body, k).0;
                        inner.push((k, e));
                        k = e;
                    }
                    b if b < 0x80 => {
                        word_w += 1;
                        k += 1;
                    }
                    _ => {
                        let (len, w) = unit(body, k);
                        word_w += w;
                        k += len;
                    }
                }
            }
            if word_w > 0 && col > indent && col + gap_w + word_w > self.width {
                // The gap's spaces go; its styles live on in `active`.
                self.widest = self.widest.max(col - indent);
                self.out.extend_from_slice(&body[done..gap]);
                self.out.extend_from_slice(b"\x1b[0m\n");
                self.out.extend_from_slice(cont);
                for &(a, e) in active.iter() {
                    self.out.extend_from_slice(&body[a..e]);
                }
                done = word;
                col = indent;
            } else {
                col += gap_w;
            }
            col += word_w;
            for &r in inner.iter() {
                track(active, r);
            }
        }
        self.widest = self.widest.max(col - indent);
        self.out.extend_from_slice(&body[done..]);
        self.wrap = buf;
    }

    fn flush(&mut self) {
        // A failed write means the reader (less) quit: nothing left to do.
        if self.sink.write_all(&self.out).and_then(|()| self.sink.flush()).is_err() {
            std::process::exit(0);
        }
        self.out.clear();
    }
}

fn sgr(out: &mut Vec<u8>, style: &str) {
    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(style.as_bytes());
    out.push(b'm');
}

/// A faded rule `cols` wide, or across the measure.
fn rule(ink: &mut Ink, cols: Option<usize>) {
    let cols = cols.unwrap_or(ink.width.saturating_sub(MARGIN));
    ink.push(ink.p.faded);
    for _ in 0..cols {
        ink.raw("─");
    }
    ink.pop();
}

/// Rule off the title across the measure, or an H1 as wide as its text: the
/// terminal's stand-in for a larger size, which it cannot show.
fn rule_under(ink: &mut Ink, title: bool) {
    let text = display_width(&ink.out[ink.hang.unwrap_or(ink.out.len())..]);
    ink.nl();
    let cols = if ink.widest > 0 { ink.widest } else { text }; // a wrapped H1: its widest row
    rule(ink, (!title).then_some(cols));
}

// ---------------------------------------------------------------- helpers

const fn byte_set(bytes: &[u8]) -> [bool; 256] {
    let mut set = [false; 256];
    let mut k = 0;
    while k < bytes.len() {
        set[bytes[k] as usize] = true;
        k += 1;
    }
    set
}

fn split_lines(src: &[u8]) -> Vec<&[u8]> {
    let src = src.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(src);
    if src.is_empty() {
        return Vec::new();
    }
    let src = src.strip_suffix(b"\n").unwrap_or(src);
    src.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l)).collect()
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

/// Length of the run of `c` starting at `i`.
fn run(s: &[u8], i: usize, c: u8) -> usize {
    s[i..].iter().take_while(|&&b| b == c).count()
}

fn find(s: &[u8], pat: &[u8]) -> Option<usize> {
    s.windows(pat.len()).position(|w| w == pat)
}

fn starts_with_ic(s: &[u8], prefix: &[u8]) -> bool {
    s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Indentation width (tabs stop every 4 columns) and the rest of the line.
fn indent(line: &[u8]) -> (usize, &[u8]) {
    let mut w = 0;
    for (k, &b) in line.iter().enumerate() {
        match b {
            b' ' => w += 1,
            b'\t' => w += 4 - w % 4,
            _ => return (w, &line[k..]),
        }
    }
    (w, &line[line.len()..])
}

/// Terminal columns taken by rendered bytes.
fn display_width(b: &[u8]) -> usize {
    let (mut k, mut width) = (0, 0);
    while k < b.len() {
        let (len, w) = unit(b, k);
        width += w;
        k += len;
    }
    width
}

/// Length and display width of the SGR sequence or UTF-8 character at `k`.
fn unit(s: &[u8], k: usize) -> (usize, usize) {
    let b = s[k];
    if b == 0x1b {
        return (s[k..].iter().position(|&c| c == b'm').map_or(s.len() - k, |p| p + 1), 0);
    }
    let len = match b {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => return (1, 1),
    };
    match s.get(k..k + len).and_then(|c| std::str::from_utf8(c).ok()).and_then(|c| c.chars().next()) {
        Some(c) => (len, char_width(c)),
        None => (1, 1),
    }
}

fn char_width(c: char) -> usize {
    match c as u32 {
        0x0300..=0x036F | 0x200B..=0x200F | 0x20D0..=0x20FF | 0xFE00..=0xFE0F => 0,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

/// A bare http(s) URL whose "://" is at `i`, provided it doesn't begin before `lit`.
fn bare_url(s: &[u8], i: usize, lit: usize) -> Option<(usize, usize)> {
    let start = if s[..i].ends_with(b"https") {
        i - 5
    } else if s[..i].ends_with(b"http") {
        i - 4
    } else {
        return None;
    };
    if start < lit || (start > 0 && s[start - 1].is_ascii_alphanumeric()) || !s[i..].starts_with(b"://") {
        return None;
    }
    let from = i + 3;
    let mut end = from + s[from..].iter().position(|&b| is_ws(b) || b"<>]".contains(&b)).unwrap_or(s.len() - from);
    while end > from && b".,:;!?'\"*_~".contains(&s[end - 1]) {
        end -= 1;
    }
    let url = &s[start..end];
    if url.ends_with(b")") && url.iter().filter(|&&b| b == b'(').count() < url.iter().filter(|&&b| b == b')').count() {
        end -= 1;
    }
    (end > from).then_some((start, end))
}

fn url_note(ink: &mut Ink, url: &[u8]) {
    ink.raw(" ");
    ink.push(ink.p.faded);
    ink.raw("(");
    ink.raw(url);
    ink.raw(")");
    ink.pop();
}

/// A list marker (`-`, `+`, `*` if `star`, `1.`, `1)`) at the start of `t`: its length.
fn list_marker(t: &[u8], star: bool) -> Option<usize> {
    let n = match *t.first()? {
        b'-' | b'+' => 1,
        b'*' if star => 1,
        _ => {
            let d = t.iter().take_while(|b| b.is_ascii_digit()).count();
            if d == 0 || d > 9 || !matches!(t.get(d), Some(b'.' | b')')) {
                return None;
            }
            d + 1
        }
    };
    matches!(t.get(n), None | Some(b' ' | b'\t')).then_some(n)
}

type Inline = fn(&mut Ink, &[u8]);

/// A list item, with typographic bullets and check boxes.
fn item(ink: &mut Ink, t: &[u8], marker: usize, inline: Inline, org: bool) {
    ink.raw(match t[0] {
        b'-' => "–".as_bytes(),
        b'+' => "◦".as_bytes(),
        b'*' => "•".as_bytes(),
        _ => &t[..marker],
    });
    ink.raw(" ");
    let mut body = t[marker..].trim_ascii_start();
    let check = match body {
        [b'[', c, b']', rest @ ..] if rest.first().is_none_or(|&b| b == b' ') => match *c {
            b' ' => Some("□"),
            b'x' | b'X' => Some("☑"),
            b'-' => Some("◫"),
            _ => None,
        },
        _ => None,
    };
    if let Some(glyph) = check {
        ink.raw(glyph);
        ink.raw(" ");
        body = body[3..].trim_ascii_start();
    }
    ink.hang();
    if org && let Some(p) = find(body, b" :: ") {
        ink.push(BOLD);
        inline(ink, &body[..p]);
        ink.pop();
        ink.styled(ink.p.faded, " ::");
        body = &body[p + 3..];
    }
    inline(ink, body);
}

/// Carry the paragraph begun by `lines[*i - 1]` on with each line that
/// continues it, on the same output line so the whole of it wraps as one.
fn reflow(ink: &mut Ink, lines: &[&[u8]], i: &mut usize, inline: Inline, continues: fn(&[&[u8]], usize) -> bool) {
    while *i < lines.len() && continues(lines, *i) {
        ink.raw(" ");
        inline(ink, lines[*i].trim_ascii());
        *i += 1;
    }
}

// ---------------------------------------------------------------- tables

#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

enum Row<'a> {
    Cells(Vec<&'a [u8]>),
    Rule,
}

/// Split a `| a | b |` row into trimmed cells; `\|` does not split.
fn split_row(t: &[u8]) -> Vec<&[u8]> {
    let mut t = t.trim_ascii();
    t = t.strip_prefix(b"|").unwrap_or(t);
    if t.ends_with(b"|") && !t.ends_with(b"\\|") {
        t = &t[..t.len() - 1];
    }
    let mut cells = Vec::new();
    let (mut start, mut k) = (0, 0);
    while k < t.len() {
        match t[k] {
            b'\\' => k += 1,
            b'|' => {
                cells.push(t[start..k].trim_ascii());
                start = k + 1;
            }
            _ => {}
        }
        k += 1;
    }
    cells.push(t[start.min(t.len())..].trim_ascii());
    cells
}

/// Render rows aligned into columns, booktabs style: no
/// vertical rules, and rule rows drawn as thin lines. One line per row.
fn table(ink: &mut Ink, lead: &[u8], rows: &[Row], aligns: &[Align], inline: Inline) {
    const GAP: usize = 3; // where " | " stood
    ink.begin(); // the margin goes before the cells rendered below
    let mut widths = vec![0; aligns.len()];
    let mut rendered = Vec::with_capacity(rows.len());
    for row in rows {
        let Row::Cells(cells) = row else {
            rendered.push(None);
            continue;
        };
        let mut out = Vec::with_capacity(cells.len());
        for (c, cell) in cells.iter().enumerate() {
            let start = ink.out.len();
            inline(ink, cell);
            let text = ink.out.split_off(start);
            let w = display_width(&text);
            if c == widths.len() {
                widths.push(0);
            }
            widths[c] = widths[c].max(w);
            out.push((text, w));
        }
        rendered.push(Some(out));
    }
    let total = widths.iter().sum::<usize>() + GAP * widths.len().saturating_sub(1);
    // Too wide to align, and padding every row out to it could bloat the output
    // without bound: leave such a table ragged, each row as long as its cells.
    let ragged = total > 4 * MEASURE;
    for row in &rendered {
        ink.raw(lead);
        match row {
            None => {
                ink.push(ink.p.faded);
                for _ in 0..if ragged { MEASURE } else { total } {
                    ink.raw("─");
                }
                ink.pop();
            }
            Some(cells) => {
                let cols = if ragged { cells.len() } else { widths.len() };
                for c in 0..cols {
                    if c > 0 {
                        ink.pad(GAP);
                    }
                    let (text, tw) = cells.get(c).map_or((&b""[..], 0), |(t, w)| (t.as_slice(), *w));
                    let gap = widths.get(c).filter(|_| !ragged).map_or(0, |&w| w - tw);
                    let left = match aligns.get(c) {
                        Some(Align::Right) => gap,
                        Some(Align::Center) => gap / 2,
                        _ => 0,
                    };
                    ink.pad(left);
                    ink.raw(text);
                    if c + 1 < cols {
                        ink.pad(gap - left);
                    }
                }
            }
        }
        ink.nl();
    }
}

// ---------------------------------------------------------------- markdown

fn markdown(src: &[u8], ink: &mut Ink) {
    let lines = split_lines(src);
    let mut i = 0;
    let mut fence: Option<(u8, usize)> = None; // open code fence: char and length
    let (mut in_comment, mut in_icode, mut in_list, mut prev_blank) = (false, false, false, true);

    // YAML (---) or TOML (+++) front matter.
    if let Some(first) = lines.first().map(|l| l.trim_ascii_end())
        && (first == b"---" || first == b"+++")
        && let Some(end) = lines[1..].iter().position(|l| l.trim_ascii_end() == first)
    {
        for l in &lines[..end + 2] {
            ink.styled(ink.p.faded, l);
            ink.nl();
        }
        i = end + 2;
    }

    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let (ind, t) = indent(line);
        let lead = &line[..line.len() - t.len()];

        if let Some((fc, flen)) = fence {
            if let Some((c, n, info)) = fence_open(t)
                && c == fc
                && n >= flen
                && info.is_empty()
            {
                ink.raw(lead);
                ink.styled(ink.p.faded, t);
                fence = None;
            } else {
                ink.raw(line);
            }
            ink.nl();
            continue;
        }
        if in_comment {
            ink.styled(ink.p.faded, line);
            ink.nl();
            in_comment = find(line, b"-->").is_none();
            continue;
        }
        if t.is_empty() {
            ink.nl();
            prev_blank = true;
            continue;
        }
        let was_blank = std::mem::replace(&mut prev_blank, false);
        if was_blank && ind < 2 && list_marker(t, true).is_none() {
            in_list = false;
        }
        in_icode = ind >= 4 && (in_icode || (was_blank && !in_list));
        if in_icode {
            ink.raw(line);
            ink.nl();
            continue;
        }
        if ind >= 4 && !in_list {
            // Lazy paragraph continuation.
            ink.raw(lead);
            ink.hang();
            md_inline(ink, t);
            ink.nl();
            continue;
        }

        if let Some((c, n, _)) = fence_open(t) {
            fence = Some((c, n));
            ink.raw(lead);
            ink.styled(ink.p.faded, t);
        } else if let Some((level, text)) = atx(t) {
            ink.marker(level);
            ink.hang();
            ink.push(ink.p.heading);
            md_inline(ink, text);
            ink.pop();
            if level == 1 {
                rule_under(ink, false);
            }
        } else if is_hr(t) {
            rule(ink, None);
        } else if t.starts_with(b"<!--") {
            ink.styled(ink.p.faded, line);
            in_comment = find(&t[4..], b"-->").is_none();
        } else if t[0] == b'>' {
            ink.raw(lead);
            md_quote(ink, &lines, &mut i);
        } else if let Some(m) = list_marker(t, true) {
            in_list = true;
            ink.raw(lead);
            item(ink, t.trim_ascii_end(), m, md_inline, false);
            reflow(ink, &lines, &mut i, md_inline, |l, k| md_continues(l[k - 1], l[k], l.get(k + 1).copied()));
        } else if t[0] == b'[' && find(t, b"]:").is_some_and(|p| p > 1) {
            ink.styled(ink.p.faded, line); // link reference or footnote definition
        } else if t.contains(&b'|')
            && let Some(aligns) = lines.get(i).and_then(|l| table_delim(l))
            && split_row(t).len() == aligns.len()
        {
            let start = i - 1;
            i += 1;
            while i < lines.len() && lines[i].contains(&b'|') && !lines[i].trim_ascii().is_empty() {
                i += 1;
            }
            let rows: Vec<Row> =
                (start..i).map(|k| if k == start + 1 { Row::Rule } else { Row::Cells(split_row(lines[k])) }).collect();
            table(ink, lead, &rows, &aligns, md_inline);
            continue;
        } else if let Some(level) = lines.get(i).and_then(|l| setext(l)) {
            i += 1; // the underline is markup, drawn as for ATX headings
            ink.marker(level);
            ink.hang();
            ink.push(ink.p.heading);
            md_inline(ink, t);
            ink.pop();
            if level == 1 {
                rule_under(ink, false);
            }
        } else {
            ink.raw(lead);
            ink.hang();
            md_inline(ink, t.trim_ascii_end());
            reflow(ink, &lines, &mut i, md_inline, |l, k| md_continues(l[k - 1], l[k], l.get(k + 1).copied()));
        }
        ink.nl();
    }
}

/// Whether `lines[k]` goes on with the paragraph above it, rather than
/// after a hard line break or as the start of another block.
fn md_continues(prev: &[u8], line: &[u8], next: Option<&[u8]>) -> bool {
    let t = line.trim_ascii();
    !(t.is_empty()
        || prev.trim_ascii().is_empty()
        || prev.ends_with(b"  ")
        || prev.ends_with(b"\\")
        || fence_open(t).is_some()
        || atx(t).is_some()
        || is_hr(t)
        || t.starts_with(b"<!--")
        || t[0] == b'>'
        || list_marker(t, true).is_some()
        || next.is_some_and(|l| setext(l).is_some() || table_delim(l).is_some()))
}

/// A code fence (3+ backticks or tildes): its char, length and info string.
fn fence_open(t: &[u8]) -> Option<(u8, usize, &[u8])> {
    let c = *t.first()?;
    if c != b'`' && c != b'~' {
        return None;
    }
    let n = run(t, 0, c);
    let info = t[n..].trim_ascii();
    (n >= 3 && !(c == b'`' && info.contains(&b'`'))).then_some((c, n, info))
}

/// An ATX heading (`## Title ##`): its level and text.
fn atx(t: &[u8]) -> Option<(usize, &[u8])> {
    let level = run(t, 0, b'#');
    if level == 0 || level > 6 || !matches!(t.get(level), None | Some(b' ' | b'\t')) {
        return None;
    }
    let text = t[level..].trim_ascii();
    // Drop a closing `###` sequence, which must follow a space.
    let e = text.len() - text.iter().rev().take_while(|&&b| b == b'#').count();
    Some((
        level,
        match e {
            0 => &text[..0],
            _ if is_ws(text[e - 1]) => text[..e].trim_ascii_end(),
            _ => text,
        },
    ))
}

fn is_hr(t: &[u8]) -> bool {
    let c = t[0];
    matches!(c, b'-' | b'*' | b'_')
        && t.iter().all(|&b| b == c || b == b' ' || b == b'\t')
        && t.iter().filter(|&&b| b == c).count() >= 3
}

/// A setext underline (`===` or `---`) turning the previous line into a heading.
fn setext(l: &[u8]) -> Option<usize> {
    let (ind, t) = indent(l);
    let t = t.trim_ascii_end();
    if ind >= 4 || t.is_empty() {
        None
    } else if t.iter().all(|&b| b == b'=') {
        Some(1)
    } else if t.iter().all(|&b| b == b'-') {
        Some(2)
    } else {
        None
    }
}

/// A table delimiter row (`|:--|--:|`): the column alignments.
fn table_delim(l: &[u8]) -> Option<Vec<Align>> {
    let (ind, t) = indent(l);
    if ind >= 4 || !t.contains(&b'|') || !t.contains(&b'-') || !t.iter().all(|b| b" \t|:-".contains(b)) {
        return None;
    }
    split_row(t)
        .into_iter()
        .map(|c| {
            let d = c.strip_prefix(b":").unwrap_or(c);
            let d = d.strip_suffix(b":").unwrap_or(d);
            if d.is_empty() || !d.iter().all(|&b| b == b'-') {
                return None;
            }
            Some(match (c[0] == b':', c[c.len() - 1] == b':') {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            })
        })
        .collect()
}

/// The quote line `lines[*i - 1]`, and the lines after it that continue its paragraph.
fn md_quote(ink: &mut Ink, lines: &[&[u8]], i: &mut usize) {
    let (depth, s) = quoted(lines[*i - 1]);
    ink.push(ink.p.faded);
    for _ in 0..depth {
        ink.raw("│ ");
    }
    ink.pop();
    let (_, body) = indent(s);
    let lead = &s[..s.len() - body.len()];
    if let Some((_, text)) = atx(body) {
        ink.hang();
        ink.push(ink.p.heading);
        md_inline(ink, text);
        ink.pop();
    } else if let Some(word) = alert(body) {
        // Colour only where it carries meaning.
        let style = match word {
            b"WARNING" => WARNING,
            b"CAUTION" => ERROR,
            _ => ink.p.heading,
        };
        ink.styled(style, word);
    } else if let Some(m) = list_marker(body, true) {
        ink.raw(lead);
        item(ink, body, m, md_inline, false);
    } else {
        ink.raw(lead);
        ink.hang();
        ink.push(ink.p.quote);
        md_inline(ink, body.trim_ascii_end());
        while *i < lines.len() {
            let (d, next) = quoted(lines[*i]);
            let after = lines.get(*i + 1).map(|l| quoted(l).1);
            if d != depth || !md_continues(quoted(lines[*i - 1]).1, next, after) {
                break;
            }
            ink.raw(" ");
            md_inline(ink, next.trim_ascii());
            *i += 1;
        }
        ink.pop();
    }
}

/// The depth of a quote line's `>` markers, and the text after them.
fn quoted(line: &[u8]) -> (usize, &[u8]) {
    let mut depth = 0;
    let mut s = indent(line).1;
    while let Some(r) = s.strip_prefix(b">") {
        depth += 1;
        s = r.strip_prefix(b" ").unwrap_or(r);
        let (_, rest) = indent(s);
        if rest.starts_with(b">") {
            s = rest;
        }
    }
    (depth, s)
}

/// The word of a GitHub alert marker such as `> [!NOTE]`.
fn alert(t: &[u8]) -> Option<&[u8]> {
    let word = t.trim_ascii_end().strip_prefix(b"[!")?.strip_suffix(b"]")?;
    [&b"NOTE"[..], b"TIP", b"IMPORTANT", b"WARNING", b"CAUTION"].contains(&word).then_some(word)
}

fn md_inline(ink: &mut Ink, s: &[u8]) {
    if ink.depth == MAX_DEPTH {
        ink.raw(s);
        return;
    }
    ink.depth += 1;
    let (mut i, mut lit, mut misses) = (0, 0, 0);
    let mut dead = [false; 7]; // delimiter runs already known to have no closer
    while i < s.len() {
        let c = s[i];
        if !MD_SPECIAL[c as usize] {
            i += 1;
            continue;
        }
        if c == b':' {
            if let Some((start, end)) = bare_url(s, i, lit) {
                ink.raw(&s[lit..start]);
                ink.styled(LINK, &s[start..end]);
                (i, lit) = (end, end);
            } else {
                i += 1;
            }
            continue;
        }
        ink.raw(&s[lit..i]);
        lit = i;
        let costly = c == b'`' || c == b'[' || (c == b'!' && s.get(i + 1) == Some(&b'['));
        let end = match c {
            _ if costly && misses == MAX_MISSES => None,
            b'\\' if s.get(i + 1).is_some_and(u8::is_ascii_punctuation) => {
                ink.raw(&s[i + 1..i + 2]);
                Some(i + 2)
            }
            b'`' => code_span(ink, s, i),
            b'*' | b'_' | b'~' => md_emphasis(ink, s, i, &mut dead),
            b'!' if s.get(i + 1) == Some(&b'[') => md_link(ink, s, i + 1, true),
            b'[' => md_link(ink, s, i, false),
            b'<' => angle(ink, s, i),
            b'&' => entity(ink, s, i),
            _ => None,
        };
        if costly && end.is_none() {
            misses = (misses + 1).min(MAX_MISSES);
        }
        match end {
            Some(e) => (i, lit) = (e, e),
            None => i += if matches!(c, b'`' | b'*' | b'_' | b'~') { run(s, i, c) } else { 1 },
        }
    }
    ink.raw(&s[lit..]);
    ink.depth -= 1;
}

fn code_span(ink: &mut Ink, s: &[u8], i: usize) -> Option<usize> {
    let n = run(s, i, b'`');
    let j = find_run(s, i + n, b'`', n)?;
    let mut code = &s[i + n..j];
    if code.len() >= 2 && code[0] == b' ' && code[code.len() - 1] == b' ' && code.iter().any(|&b| b != b' ') {
        code = &code[1..code.len() - 1];
    }
    ink.styled(ink.p.grey, code);
    Some(j + n)
}

/// Start of the next run of exactly `n` copies of `c` at or after `from`,
/// within `MAX_SPAN` bytes.
fn find_run(s: &[u8], from: usize, c: u8, n: usize) -> Option<usize> {
    let mut j = from;
    while j < s.len().min(from + MAX_SPAN) {
        if s[j] == c {
            let k = run(s, j, c);
            if k == n {
                return Some(j);
            }
            j += k;
        } else {
            j += 1;
        }
    }
    None
}

fn md_emphasis(ink: &mut Ink, s: &[u8], i: usize, dead: &mut [bool; 7]) -> Option<usize> {
    let c = s[i];
    let n = run(s, i, c);
    let (slot, style) = match (c, n) {
        (b'~', 2) => (6, STRIKE),
        (b'*' | b'_', 1..=3) => ((c == b'_') as usize * 3 + n - 1, [ITALIC, BOLD, BOLD_ITALIC][n - 1]),
        _ => return None,
    };
    if dead[slot] || !can_open(s, i, n, c) {
        return None;
    }
    let Some(j) = find_closer(s, i + n, c, n) else {
        dead[slot] = true;
        return None;
    };
    ink.push(style);
    md_inline(ink, &s[i + n..j]);
    ink.pop();
    Some(j + n)
}

// Simplified CommonMark flanking rules; `_` never opens or closes inside a word.
fn can_open(s: &[u8], i: usize, n: usize, c: u8) -> bool {
    let Some(&next) = s.get(i + n) else { return false };
    let prev = if i > 0 { s[i - 1] } else { b' ' };
    let prev_open = is_ws(prev) || prev.is_ascii_punctuation();
    !is_ws(next) && (prev_open || !next.is_ascii_punctuation()) && (c != b'_' || prev_open)
}

fn can_close(s: &[u8], j: usize, n: usize, c: u8) -> bool {
    let prev = s[j - 1];
    let next = s.get(j + n).copied().unwrap_or(b' ');
    let next_open = is_ws(next) || next.is_ascii_punctuation();
    !is_ws(prev) && (next_open || !prev.is_ascii_punctuation()) && (c != b'_' || next_open)
}

fn find_closer(s: &[u8], from: usize, c: u8, n: usize) -> Option<usize> {
    let mut j = from;
    while j < s.len() {
        match s[j] {
            b'\\' => j += 2,
            b'`' => {
                let k = run(s, j, b'`');
                j = find_run(s, j + k, b'`', k).map_or(j + k, |e| e + k);
            }
            b if b == c => {
                let k = run(s, j, c);
                if k == n && j > from && can_close(s, j, k, c) {
                    return Some(j);
                }
                j += k;
            }
            _ => j += 1,
        }
    }
    None
}

/// `[text](url)`, `[text][ref]`, `![alt](src)` or a `[^note]` at `i` (the `[`).
fn md_link(ink: &mut Ink, s: &[u8], i: usize, image: bool) -> Option<usize> {
    let close = matching(s, i, b'[', b']')?;
    let text = &s[i + 1..close];
    let (url, end) = match s.get(close + 1) {
        Some(b'(') => {
            let e = matching(s, close + 1, b'(', b')')?;
            (Some(destination(&s[close + 2..e])), e + 1)
        }
        Some(b'[') => (None, close + 3 + s[close + 2..].iter().position(|&b| b == b']')?),
        _ if !image && text.starts_with(b"^") => {
            ink.styled(ink.p.faded, &s[i..=close]);
            return Some(close + 1);
        }
        _ => return None,
    };
    if image {
        ink.push(ink.p.faded);
        ink.raw("▣ ");
        ink.raw(text);
        ink.pop();
    } else {
        ink.push(LINK);
        md_inline(ink, text);
        ink.pop();
        if let Some(u) = url.filter(|u| !u.is_empty() && u[0] != b'#') {
            url_note(ink, u);
        }
    }
    Some(end)
}

/// Index of the bracket closing the one at `i`, skipping escapes and code spans.
fn matching(s: &[u8], i: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0;
    let mut j = i;
    while j < s.len() {
        match s[j] {
            b'\\' => j += 1,
            b'`' => {
                let n = run(s, j, b'`');
                j = find_run(s, j + n, b'`', n).map_or(j + n, |e| e + n) - 1;
            }
            b if b == open => depth += 1,
            b if b == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

fn destination(d: &[u8]) -> &[u8] {
    let d = d.trim_ascii();
    if let Some(r) = d.strip_prefix(b"<") {
        return &r[..r.iter().position(|&b| b == b'>').unwrap_or(r.len())];
    }
    &d[..d.iter().position(|&b| is_ws(b)).unwrap_or(d.len())]
}

/// `<https://autolink>`, `<a@b.c>`, or an inline HTML tag (faded).
fn angle(ink: &mut Ink, s: &[u8], i: usize) -> Option<usize> {
    let rest = &s[i + 1..];
    let e = rest.iter().position(|&b| b == b'>' || b == b'<')?;
    if rest[e] != b'>' || e == 0 {
        return None;
    }
    let inner = &rest[..e];
    let f = inner[0];
    if f.is_ascii_alphabetic() && !inner.iter().any(|&b| is_ws(b)) && inner.iter().any(|&b| b == b':' || b == b'@') {
        ink.styled(LINK, inner);
    } else if f.is_ascii_alphabetic() || f == b'/' || f == b'!' {
        ink.styled(ink.p.faded, &s[i..i + e + 2]);
    } else {
        return None;
    }
    Some(i + e + 2)
}

fn entity(ink: &mut Ink, s: &[u8], i: usize) -> Option<usize> {
    let rest = &s[i + 1..];
    let e = rest.iter().take(32).position(|&b| b == b';')?;
    let name = &rest[..e];
    let text = match name {
        b"amp" => "&",
        b"lt" => "<",
        b"gt" => ">",
        b"quot" => "\"",
        b"apos" => "'",
        b"nbsp" => " ",
        b"mdash" => "—",
        b"ndash" => "–",
        b"hellip" => "…",
        b"copy" => "©",
        b"reg" => "®",
        b"trade" => "™",
        b"larr" => "←",
        b"rarr" => "→",
        [b'#', num @ ..] => {
            let num = std::str::from_utf8(num).ok()?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16),
                None => num.parse(),
            };
            let ch = char::from_u32(code.ok()?)?;
            ink.raw(ch.encode_utf8(&mut [0; 4]));
            return Some(i + e + 2);
        }
        _ => return None,
    };
    ink.raw(text);
    Some(i + e + 2)
}

// ---------------------------------------------------------------- org

#[derive(Clone, Copy)]
enum Block {
    Code,
    Quiet,
    Quote,
    Text,
}

fn org(src: &[u8], ink: &mut Ink) {
    let lines = split_lines(src);
    // Where blocks and drawers close, found in one pass: one never closed is plain text.
    let mut block_ends = HashMap::new(); // the last `#+end_NAME` of each name
    let mut drawer_end = None; // the last `:END:`
    for (k, l) in lines.iter().enumerate() {
        let t = l.trim_ascii();
        if starts_with_ic(t, b"#+end_") {
            block_ends.insert(t[6..].to_ascii_lowercase(), k);
        } else if t.eq_ignore_ascii_case(b":END:") {
            drawer_end = Some(k);
        }
    }
    let mut block: Option<(Block, &[u8])> = None; // open #+begin_ block: kind and name
    let mut drawer = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let (ind, t) = indent(line);
        let lead = &line[..line.len() - t.len()];

        if let Some((kind, name)) = block {
            if block_end(t, name) {
                // The #+end_ prefix is hidden.
                ink.raw(lead);
                ink.styled(ink.p.faded, &t[6..]);
                block = None;
            } else {
                match kind {
                    Block::Code => ink.raw(line),
                    Block::Quiet => ink.styled(ink.p.faded, line),
                    Block::Quote => {
                        ink.raw(lead);
                        ink.hang();
                        ink.push(ink.p.quote);
                        org_inline(ink, t.trim_ascii_end());
                        reflow(ink, &lines, &mut i, org_inline, org_continues);
                        ink.pop();
                    }
                    Block::Text => {
                        ink.raw(lead);
                        ink.hang();
                        org_inline(ink, t);
                    }
                }
            }
            ink.nl();
            continue;
        }
        if drawer {
            ink.styled(ink.p.faded, line);
            drawer = !t.trim_ascii_end().eq_ignore_ascii_case(b":END:");
            ink.nl();
            continue;
        }
        if t.is_empty() {
            ink.nl();
            continue;
        }
        if line[0] == b'*' && org_heading(ink, line) {
            ink.nl();
            continue;
        }
        if starts_with_ic(t, b"#+begin_") {
            let spec = &t[8..];
            let name = &spec[..spec.iter().position(|&b| is_ws(b)).unwrap_or(spec.len())];
            if block_ends.get(&name.to_ascii_lowercase()).is_some_and(|&e| e >= i) {
                let is = |k: &[u8]| name.eq_ignore_ascii_case(k);
                let kind = if is(b"src") || is(b"example") {
                    Block::Code
                } else if is(b"export") || is(b"comment") {
                    Block::Quiet
                } else if is(b"quote") {
                    Block::Quote
                } else {
                    Block::Text
                };
                // The #+begin_ prefix is hidden.
                ink.raw(lead);
                ink.styled(ink.p.faded, spec);
                ink.nl();
                block = Some((kind, name));
                continue;
            }
        }

        if t.starts_with(b"#+") {
            // The #+ prefix hidden, and the title shown unlabelled.
            if starts_with_ic(t, b"#+title:") {
                ink.hang();
                ink.push(ink.p.heading);
                org_inline(ink, t[8..].trim_ascii());
                ink.pop();
                rule_under(ink, true);
            } else {
                ink.raw(lead);
                ink.hang();
                ink.styled(ink.p.faded, &t[2..]);
            }
        } else if t == b"#" || t.starts_with(b"# ") {
            ink.raw(lead);
            ink.hang();
            ink.styled(ink.p.faded, t);
        } else if t == b":" || t.starts_with(b": ") {
            ink.raw(lead);
            ink.styled(ink.p.grey, t);
        } else if is_drawer(t) && drawer_end.is_some_and(|e| e >= i) {
            ink.styled(ink.p.faded, line);
            drawer = true;
        } else if PLANNING.iter().any(|k| t.starts_with(k)) {
            ink.raw(lead);
            ink.hang();
            ink.push(ink.p.faded);
            org_inline(ink, t);
            ink.pop();
        } else if t.len() >= 5 && t.iter().all(|&b| b == b'-') {
            rule(ink, None);
        } else if t[0] == b'|' {
            let start = i - 1;
            while i < lines.len() && indent(lines[i]).1.starts_with(b"|") {
                i += 1;
            }
            org_table(ink, lead, &lines[start..i]);
            continue;
        } else if let Some(m) = list_marker(t, ind > 0) {
            ink.raw(lead);
            item(ink, t.trim_ascii_end(), m, org_inline, true);
            reflow(ink, &lines, &mut i, org_inline, org_continues);
        } else {
            ink.raw(lead);
            ink.hang();
            org_inline(ink, t.trim_ascii_end());
            reflow(ink, &lines, &mut i, org_inline, org_continues);
        }
        ink.nl();
    }
}

/// Whether `lines[k]` goes on with the paragraph above it, rather than
/// after a `\\` line break or as the start of another element.
fn org_continues(lines: &[&[u8]], k: usize) -> bool {
    let l = lines[k];
    let (ind, t) = indent(l);
    !(t.is_empty()
        || lines[k - 1].trim_ascii_end().ends_with(b"\\\\")
        || (l[0] == b'*' && matches!(l.get(run(l, 0, b'*')), Some(b' ' | b'\t')))
        || t[0] == b'#'
        || t[0] == b':'
        || t[0] == b'|'
        || PLANNING.iter().any(|p| t.starts_with(p))
        || (t.len() >= 5 && t.iter().all(|&b| b == b'-'))
        || list_marker(t, ind > 0).is_some())
}

fn block_end(t: &[u8], name: &[u8]) -> bool {
    starts_with_ic(t, b"#+end_") && t[6..].trim_ascii_end().eq_ignore_ascii_case(name)
}

fn is_drawer(t: &[u8]) -> bool {
    let t = t.trim_ascii_end();
    t.len() > 2
        && t[0] == b':'
        && t[t.len() - 1] == b':'
        && t[1..t.len() - 1].iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !t.eq_ignore_ascii_case(b":END:")
}

/// Render `line` if it is an Org headline; otherwise write nothing and return false.
fn org_heading(ink: &mut Ink, line: &[u8]) -> bool {
    let level = run(line, 0, b'*');
    if !matches!(line.get(level), Some(b' ' | b'\t')) {
        return false;
    }
    let mut t = line[level..].trim_ascii();
    ink.marker(level);
    ink.hang();

    let mut style = ink.p.heading;
    let word = &t[..t.iter().position(|&b| b == b' ').unwrap_or(t.len())];
    let keyword = if TODO_WORDS.contains(&word) {
        Some(ink.p.heading)
    } else if DONE_WORDS.contains(&word) {
        style = ink.p.done;
        Some(ink.p.faded)
    } else {
        None
    };
    if let Some(k) = keyword {
        ink.styled(k, word);
        ink.raw(" ");
        t = t[word.len()..].trim_ascii_start();
    }
    if let [b'[', b'#', _, b']', ..] = t {
        ink.styled(ink.p.faded, &t[..4]);
        ink.raw(" ");
        t = t[4..].trim_ascii_start();
    }
    let mut tags: &[u8] = &[];
    if t.ends_with(b":")
        && let Some(p) = t.iter().rposition(|&b| b == b' ' || b == b'\t')
    {
        let cand = &t[p + 1..];
        if cand.len() > 2 && cand[0] == b':' && cand.iter().all(|&b| b.is_ascii_alphanumeric() || b"_@#%:".contains(&b)) {
            tags = cand;
            t = t[..p].trim_ascii_end();
        }
    }
    ink.push(style);
    org_inline(ink, t);
    ink.pop();
    if !tags.is_empty() {
        ink.raw("  ");
        ink.styled(ink.p.faded, tags);
    }
    if level == 1 {
        rule_under(ink, false);
    }
    true
}

fn org_table(ink: &mut Ink, lead: &[u8], lines: &[&[u8]]) {
    let rows: Vec<Row> = lines
        .iter()
        .map(|l| {
            let t = l.trim_ascii();
            if t.starts_with(b"|-") { Row::Rule } else { Row::Cells(split_row(t)) }
        })
        .collect();
    // As Org does, right-align mostly-numeric columns, not counting a header
    // (the rows above the first rule).
    let head = rows.iter().position(|r| matches!(r, Row::Rule)).filter(|&p| p + 1 < rows.len()).unwrap_or(0);
    let ncols = rows.iter().map(|r| if let Row::Cells(c) = r { c.len() } else { 0 }).max().unwrap_or(0);
    let mut counts = vec![(0, 0); ncols]; // filled and numeric cells per column
    for row in &rows[head..] {
        if let Row::Cells(cells) = row {
            for (c, v) in cells.iter().enumerate().filter(|(_, v)| !v.is_empty()) {
                counts[c].0 += 1;
                counts[c].1 += is_number(v) as usize;
            }
        }
    }
    let aligns: Vec<Align> =
        counts.iter().map(|&(filled, numeric)| if numeric * 2 > filled { Align::Right } else { Align::Left }).collect();
    table(ink, lead, &rows, &aligns, org_inline);
}

fn is_number(v: &[u8]) -> bool {
    let v = v.strip_prefix(b"-").or_else(|| v.strip_prefix(b"+")).unwrap_or(v);
    v.first().is_some_and(u8::is_ascii_digit) && v.iter().all(|&b| b.is_ascii_digit() || b".,%eE".contains(&b))
}

fn org_inline(ink: &mut Ink, s: &[u8]) {
    if ink.depth == MAX_DEPTH {
        ink.raw(s);
        return;
    }
    ink.depth += 1;
    let (mut i, mut lit, mut misses) = (0, 0, 0);
    let mut dead = [false; 6]; // markers already known to have no closer
    while i < s.len() {
        let c = s[i];
        if !ORG_SPECIAL[c as usize] {
            i += 1;
            continue;
        }
        if c == b':' {
            if let Some((start, end)) = bare_url(s, i, lit) {
                ink.raw(&s[lit..start]);
                ink.styled(LINK, &s[start..end]);
                (i, lit) = (end, end);
            } else {
                i += 1;
            }
            continue;
        }
        ink.raw(&s[lit..i]);
        lit = i;
        let costly = c == b'[' || c == b'<';
        let end = match c {
            _ if costly && misses == MAX_MISSES => None,
            b'[' => org_bracket(ink, s, i),
            b'<' => org_angle(ink, s, i),
            _ => org_emphasis(ink, s, i, &mut dead),
        };
        if costly && end.is_none() {
            misses = (misses + 1).min(MAX_MISSES);
        }
        match end {
            Some(e) => (i, lit) = (e, e),
            None => i += 1,
        }
    }
    ink.raw(&s[lit..]);
    ink.depth -= 1;
}

/// `*bold*`, `/italic/`, `_underline_`, `+strike+`, `=verbatim=` or `~code~` at `i`.
fn org_emphasis(ink: &mut Ink, s: &[u8], i: usize, dead: &mut [bool; 6]) -> Option<usize> {
    let m = s[i];
    let slot = b"*/_=~+".iter().position(|&b| b == m)?;
    if dead[slot] || (i > 0 && !b" \t-({'\"".contains(&s[i - 1])) || s.get(i + 1).is_none_or(|&b| is_ws(b)) {
        return None;
    }
    let mut j = i + 2;
    let close = loop {
        let Some(p) = s.get(j..).and_then(|r| r.iter().position(|&b| b == m)) else {
            dead[slot] = true;
            return None;
        };
        let k = j + p;
        if !is_ws(s[k - 1]) && s.get(k + 1).is_none_or(|b| b" \t-.,;:!?')}[\"\\".contains(b)) {
            break k;
        }
        j = k + 1;
    };
    let body = &s[i + 1..close];
    if m == b'=' || m == b'~' {
        ink.styled(ink.p.grey, body);
    } else {
        ink.push(match m {
            b'*' => BOLD,
            b'/' => ITALIC,
            b'_' => UNDERLINE,
            _ => STRIKE,
        });
        org_inline(ink, body);
        ink.pop();
    }
    Some(close + 1)
}

/// `[[link][desc]]`, `[fn:x]`, `[2024-01-01 Mon]` or a `[2/5]` cookie at `i`.
fn org_bracket(ink: &mut Ink, s: &[u8], i: usize) -> Option<usize> {
    let r = &s[i..];
    if r.starts_with(b"[[") {
        let e = find(r, b"]]")?;
        let inner = &r[2..e];
        let (target, desc) = match find(inner, b"][") {
            Some(p) => (&inner[..p], Some(&inner[p + 2..])),
            None => (inner, None),
        };
        ink.push(LINK);
        match desc {
            Some(d) => org_inline(ink, d),
            None => ink.raw(target),
        }
        ink.pop();
        if desc.is_some() && !target.starts_with(b"*") && !target.starts_with(b"#") && !target.starts_with(b"id:") {
            url_note(ink, target);
        }
        return Some(i + e + 2);
    }
    let e = r.iter().position(|&b| b == b']')?;
    let inner = &r[1..e];
    let style = if inner.starts_with(b"fn:") || is_date(inner) {
        ink.p.faded
    } else if !inner.is_empty() && inner.iter().all(|&b| b.is_ascii_digit() || b == b'/' || b == b'%') {
        // A statistics cookie: strong while there's work left, as org-todo.
        let done = match inner.iter().position(|&b| b == b'/') {
            Some(p) => p > 0 && inner[..p] == inner[p + 1..],
            None => inner == b"100%",
        };
        if done { ink.p.faded } else { ink.p.heading }
    } else {
        return None;
    };
    ink.styled(style, &r[..=e]);
    Some(i + e + 1)
}

/// An active timestamp `<2024-01-01 Mon 10:00>` at `i`.
fn org_angle(ink: &mut Ink, s: &[u8], i: usize) -> Option<usize> {
    if !is_date(&s[i + 1..]) {
        return None;
    }
    let e = s[i..].iter().position(|&b| b == b'>')?;
    ink.styled(ink.p.faded, &s[i..=i + e]);
    Some(i + e + 1)
}

fn is_date(t: &[u8]) -> bool {
    t.len() >= 10
        && t[..4].iter().all(u8::is_ascii_digit)
        && t[4] == b'-'
        && t[5..7].iter().all(u8::is_ascii_digit)
        && t[7] == b'-'
        && t[8..10].iter().all(u8::is_ascii_digit)
}
