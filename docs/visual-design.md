# Visual design: colour, type, and the cell grid

The specification for how caocli looks. Two front ends paint the same session
(`src/ui/theme.rs` is the one place they agree), so this is written as tokens
and rules rather than as screenshots. Everything described here is implemented
in `src/ui/`, with the test envelope that proves each one; the design document
is the prose that the tests stand in for.

## 1. The thesis

**The transcript is a document; the machine's activity is marginalia.**

A coding agent's screen is read, not watched: a long answer, a diff, a command,
a verdict. So the type is quiet and the colour is scarce. There are five colours
in the whole palette, every one of them is a meaning rather than a decoration,
and two of them — the green and the red — appear only at a verdict.

Three rules follow, and the rest of this document is their consequences:

1. **Colour never carries meaning alone.** Every coloured thing has a glyph, a
   column, or a word that says the same thing. This survives `NO_COLOR`, a
   monochrome terminal, red-green colour blindness, and a screenshot printed on
   a laser printer.
2. **De-emphasis is a colour, never a modifier.** `SGR 2` is rendered as
   anything from "slightly grey" to "invisible" to "nothing at all" depending on
   the terminal, and it stacks with the colour underneath unpredictably. Secondary
   text is painted in a secondary colour.
3. **Hierarchy comes from columns and blank rows, not from height.** A terminal
   cell is one size. There is no point size to vary, so the vertical rhythm and
   the two-column gutter are the typography.

## 2. Colour

### 2.1 The five colours of meaning

The palette is not a list of nice colours; it is a list of **jobs**. The six
`Style` variants in `src/ui/cell/mod.rs` map onto five tokens, because `Dim` and
`Reasoning` are deliberately the same colour (what tells thinking from a tool
result is the rule in its gutter, not its hue).

| token | job | `Style` | where it appears |
|---|---|---|---|
| `fg` | the answer | `Plain` | the answer's body, the user's own words, a question's option label |
| `muted` | everything the machine says about itself | `Dim`, `Reasoning` | thinking, tool results, step children, notices, fold lines, the queue, inline code and fenced blocks, a picker row's detail, the banner's separators |
| `signal` | in flight, or waiting on you | `Yellow` | the `▸` of a running step, the `▸` of a question or an approval, an in-progress task, the tool verb, `strong` in markdown, `⏹ interrupted` |
| `ok` | a change that landed | `Green` | the `✔` of a step that finished, a `+` line in a diff, a completed task |
| `bad` | a change that did not | `Red` | the `✘` of a failure or a refusal, a `-` line in a diff, an `error:` line, a failing step's children |

Everything in that table is what the code paints today, with two exceptions,
both of which are currently left in the terminal's default foreground and are
`muted` in this design: the status line, and the working indicator's title.

One more token is structural rather than semantic:

| token | job |
|---|---|
| `rule` | a border, a separator, a scrollbar: geometry, never text. It is the only colour allowed below 3:1, because no one reads it |

There is deliberately **no selection colour**. §2.6 is why: a highlighted row
needs a background, this application does not know the terminal's background,
and a hard-coded one is wrong on half the terminals it lands on.

### 2.2 The two themes

The application does not paint a background (the frame is drawn on the
terminal's own), so a palette has to be calibrated against a *reference*
background rather than against its own. Two themes are enough, and they are a
pair: **ink** is text on a dark screen, **paper** is text on a light one.

#### ink (default)

Calibrated against `#16161E`, and required to hold on the dark backgrounds a
terminal is actually likely to be set to.

| token | hex | on `#16161E` | on `#000000` | on `#282A36` | on `#0D1117` |
|---|---|---|---|---|---|
| `fg` | `#D7DAE4` | 12.9:1 | 15.0:1 | 10.2:1 | 13.6:1 |
| `muted` | `#9AA1B5` | 7.0:1 | 8.1:1 | 5.5:1 | 7.3:1 |
| `signal` | `#E2B15C` | 9.1:1 | 10.7:1 | 7.2:1 | 9.6:1 |
| `ok` | `#8FD9A3` | 10.8:1 | 12.6:1 | 8.6:1 | 11.4:1 |
| `bad` | `#E9707E` | 6.1:1 | 7.1:1 | 4.8:1 | 6.4:1 |
| `rule` | `#3A3F52` | 1.7:1 | 2.0:1 | 1.4:1 | 1.8:1 |

Every text token clears **4.5:1 on every one of the four backgrounds** — the
worst case in the table is `bad` on Dracula's lighter `#282A36`, at 4.8:1. That
is the property worth having: the user's own background is not ours to know.

#### paper (opt-in)

Calibrated against `#FFFFFF`, and it must also hold on tinted paper.

| token | hex | on `#FFFFFF` | on `#F7F7F5` | on `#E8E8E4` |
|---|---|---|---|---|
| `fg` | `#23252E` | 15.3:1 | 14.2:1 | 12.4:1 |
| `muted` | `#5C6172` | 6.2:1 | 5.7:1 | 5.0:1 |
| `signal` | `#8A5C06` | 5.8:1 | 5.4:1 | 4.7:1 |
| `ok` | `#1A7245` | 5.9:1 | 5.5:1 | 4.8:1 |
| `bad` | `#7A1124` | 10.9:1 | 10.1:1 | 8.8:1 |
| `rule` | `#C9CCD6` | 1.6:1 | — | — |

`signal` deepened from `#8F6008` to `#8A5C06` (4.7:1 on the dimmest light
background, above the 4.5:1 floor) and `ok`/`bad` shifted to a forest green
`#1A7245` and a deep oxblood `#7A1124` to clear the same floor across the
whole family of light backgrounds — a themed editor's light grey is darker
than white, and every accent has to clear 4.5:1 there too.

### 2.3 Why not the theme that is there now

`theme.rs` ships Dracula today. Measured, it has two problems, and both are
about *reading* rather than about taste:

- **The comment colour is too quiet for prose.** `#6272A4` on `#282A36` is
  **3.0:1**. That is below the 4.5:1 floor, and it is the colour of the thinking
  block and of every tool result — the two longest runs of text on the screen
  after the answer itself. An answer whose reasoning is unreadable is an answer
  nobody audits. The replacement is 7.0:1: still unmistakably secondary, but
  legible.
- **Dracula's foreground does not survive a light terminal.** `#F8F8F2` on
  white is 1.1:1. There is no way to run this tool on a light background today.
  Hence a second theme rather than a cleverer single one.

What is *kept* from Dracula is the shape of the thing: a cool near-white body, a
muted blue-grey for marginalia, one warm attention colour, and green and red
reserved for verdicts. The hues are recognisable; the lightnesses are re-tuned
so the contrast holds.

### 2.4 Attention versus failure

`signal` is amber and `bad` is red, and they are deliberately far apart in
lightness (9.1:1 versus 6.1:1) as well as in hue. The pair people confuse is not
these two — it is `ok` and `bad`, and the palette separates them on the axis
that colour blindness does not touch:

**`ok` and `bad` differ by 1.78:1 in relative luminance.** A reader with
deuteranopia — around 6% of men — sees no green-red axis at all; what survives
for them is that an added line is *lighter* than a removed line. That is the
reason the green is bright and the red is deep, and it is a deliberate departure
from the usual "red is the loud one".

Measured with the Machado (2009) severity-1.0 matrices, every pair of text
tokens stays at least ΔE 14 apart under deuteranopia and at least ΔE 19 apart
under protanopia, in normal vision at least ΔE 21. The one pair that comes
closest is `ok`/`bad` under deuteranopia, and that pair is the one whose meaning
is also spelled out by `+`/`-` and by `✔`/`✘`.

### 2.5 The same palette in lesser terminals

Truecolor is not universal and `NO_COLOR` exists. There are three tiers, and
each one gives up something specific. The tier is decided from the environment
(`COLORTERM` first, then `TERM`); an uninformative environment gets truecolour
on purpose, because a modern embedder that sets neither variable is far more
likely than a VT100.

**Tier 1 — truecolor.** `38;2;r;g;b`. Both themes available.

**Tier 2 — 256 colours.** Rounded to the nearest xterm-256 entry by CIELAB
distance. What is lost is chroma, not the relationships: `muted` comes out
neutral grey, which is no loss for secondary text, and the `ok`/`bad` tone split
— the thing colour blindness relies on — comes out *wider* than in truecolor
because the 6×6×6 cube rounds toward the extremes.

A naive rounding by nearest entry would have flattened the split: ink's `bad`
at `#E9707E` rounds up to `#FF8787`, losing two lightness steps to gain ΔE 1.4
of hue. So `ok` and `bad` are rounded with a *bound*: `ok` may only come out
lighter than its truecolour source, `bad` only darker. The split is therefore
2.31:1 in ink (against 1.78:1 in truecolor) and 3.12:1 in paper (against
1.83:1).

| token | ink | paper |
|---|---|---|
| `fg` | 253 `#DADADA` | 235 `#262626` |
| `muted` | 247 `#9E9E9E` | 241 `#626262` |
| `signal` | 179 `#D7AF5F` | 94 `#875F00` |
| `ok` | 151 `#AFD7AF` | 29 `#00875F` |
| `bad` | 167 `#D75F5F` | 52 `#5F0000` |
| `rule` | 238 `#444444` | 252 `#D0D0D0` |

**Tier 3 — 16 colours.** Here the palette stops meaning anything, because the
terminal owns both the background and all sixteen entries. The correct move is
to stop guessing and map onto the slots a colour scheme conventionally fills,
and to write the escapes as `30..=37` / `90..=97` rather than as `38;5;n`, for
the small but real class of terminals that does not have the extended palette
at all:

| token | ANSI | `ratatui` name |
|---|---|---|
| `fg` | the terminal's default foreground (no SGR colour at all) | `Color::Reset` |
| `muted` | bright black (8) | `Color::DarkGray` |
| `signal` | yellow (3) | `Color::Yellow` |
| `ok` | green (2) | `Color::Green` |
| `bad` | red (1) | `Color::Red` |
| `rule` | bright black (8) | `Color::DarkGray` |

There is exactly one 16-colour mapping, not one per theme: the terminal already
decided light or dark, and second-guessing it is how a light terminal gets dark
text. **At this tier the bold modifier must be dropped from the coloured
styles** — in a 16-colour terminal `SGR 1` usually *brightens* the foreground
rather than thickening it, so a bold green is a different colour than the one
the table asked for.

**Tier 0 — `NO_COLOR`.** No SGR colour at all. Everything still works, because
of §1.1: what tells a running step from a finished one is `▸` versus `✔`, and
what tells an added line from a removed one is `+` versus `-`.

### 2.6 Selection and the rule

Two places need more than a foreground.

**The picker's highlighted row** is drawn `REVERSED`, and stays that way: reverse
video is the one highlight that works in every terminal without the application
knowing the background. Three rules go with it:

- The row carries a `❯` cursor in `signal` on its left as well, so the highlight
  is not the only thing marking the row. A whole-row inversion is a strong
  signal on a terminal that renders it well and an invisible one on a terminal
  that renders it faintly; the mark is a colour *as well as* a shape.
- **The detail column drops its dim-ness while the row is reversed.** Painting
  `muted` on an inverted background is the classic unreadable combination, and
  how a terminal renders `DIM` *under* reverse video is undefined. On the
  selected row the hierarchy is carried by position alone.
- The cursor's own colour is the foreground of the inverted cell — which is
  what makes it read as a mark rather than as a highlight of its own — so the
  cursor is `signal` on the selected row and empty on the others.

**The rule** — the box's top and bottom border, separators, the scrollbar — is
`rule`, and it is the one token allowed to sit under 3:1, because it is not
text. Today the box border is painted in `rule`; the `Modifier::DIM` that used
to do this job is the one §1.2 said had to go.

**The working indicator's title** (the spinner, the verb, the elapsed seconds,
the tokens-per-second estimate) sits on the box's top rule and is painted in
`signal`. A signal-on-geometry arrangement is what reads as "the spinner is on
fire" without saying it with a colour the user has to learn.

## 3. Type

### 3.1 What a terminal application can actually decide

Honest framing, because it determines what the rest of this section is about:

| | a GUI decides | caocli decides |
|---|---|---|
| typeface | yes | **no** — the terminal owns it (§3.4 says what to ask it for) |
| point size | yes | **no** — the user's zoom is the only lever |
| weight | yes | partly: `SGR 1` |
| italic | yes | **avoid** — see §3.5 |
| leading | yes | no — but the app *spends* rows, which is the same lever (§3.6) |
| measure | yes | yes — the wrap width |
| colour | yes | yes (§2) |
| glyphs | yes | yes, and this is a real decision (§3.3) |

So "font size" for caocli means three separate things: **the glyphs it emits**
(a contract the typeface must satisfy), **the weight and colour it applies**
(the scale in §3.2), and **the rows and columns it spends** (the rhythm in
§3.6). The point size is the user's, and the design's job is to remain excellent
at every one.

### 3.2 The scale: five levels, one cell tall

There is no vertical room to make a heading bigger, so the levels are
distinguished by column, weight and colour. This is the whole of the app's
typography.

| level | role | column | weight | token | marked by |
|---|---|---|---|---|---|
| **L0 Body** | the answer | 0 | regular | `fg` | the only text at the left edge |
| **L1 Marginalia** | thinking, tool results, notices, fold lines, queue | 2 | regular | `muted` | the two blank columns |
| **L2 Signal** | a tool's name, a question, the working indicator | 2 | bold | `signal` | `▸` |
| **L3 Verdict** | finished, failed, refused | 2 | bold | `ok`/`bad` | `✔` / `✘` |
| **L4 Chrome** | the box rule, the status line, a picker's detail column, the banner's separators | varies | regular | `muted` / `rule` | it is not the transcript |

Two rules the table is really saying:

- **The left edge belongs to the answer.** One thing on the screen starts in
  column 0, and it is the thing being read. Everything else is set in two
  columns. This is doing the work a heading's size would do in a GUI.
- **Bold means "acting on something", never "important".** Three levels carry
  it, and they are exactly the three that are about something happening. The
  answer is never bold: a paragraph of bold text is a paragraph nobody reads.

Markdown inside an answer inherits this, and today's renderer is already close:
`strong` is `signal`, inline code and fenced blocks are `muted`, a link is its
text with the URL beside it.

The one place the scale has to compromise is a heading, and the compromise is
the right one: **a heading keeps its `#` markers.** There is no point size to
raise, so `###` is the size bump — the marks are the type. To colour a heading
as `signal` + bold would say "something is happening here", which is L2's
meaning and not a heading's; a heading is `fg`, like the text under it, one cell
taller by virtue of its own characters. A block quote opens with `> `, for the
same reason: the convention is a convention because it fits in a cell.

### 3.3 The glyph contract

The application emits a small, closed vocabulary of non-ASCII characters. Every
one of them is a decision, because the typeface has to have it and the terminal
has to give it exactly one column.

**The gutter markers — one per kind of line, and six of the eight are
non-ASCII:**

```
›   user                          U+203A   N
▸   running, asking, approving    U+25B8   N
✔   finished, completed           U+2714   N
✘   failed, denied                U+2718   N
☐   a task not started            U+2610   N
≡   usage                         U+2261   A   <- the one at risk
*   a notice                      ASCII
    thinking, a failure, a stop   two blanks
```

**The marks that are not in a gutter:**

```
⏹   interrupted                  U+23F9   N   inline, in the yellow line
⋮   rows the window cut           U+22EE   N   the lead of a count line
❯ ✓ the picker's cursor, chosen    U+276F U+2713   N
─   the box's rule                U+2500   A
◐ ◓ ◑ ◒  the working spinner       U+25D0–U+25D3   A, N, A, N
·   a separator between segments  U+00B7   A
…   an ellipsis                  U+2026   A
—   an em dash                   U+2014   A
•   the secret prompt's mask     U+2022   A
```

#### The East Asian Ambiguous trap

Checked against `UnicodeData.txt`'s `East_Asian_Width`: **almost every glyph
above is class `A` (ambiguous) or class `N` (neutral), and the split does not
follow the gutters — it follows the Unicode block.** The punctuation and
symbols (middle dot, ellipsis, em dash, bullet, box drawing, geometric shapes)
are `A`; the dingbats and arrows (`✔ ✘ ✓ ❯ › ▸ ☐ ⋮ ⏹`) are `N`.

`text::width` resolves `A` as one column, which is what most terminals do too.
Where they disagree, the arithmetic is off by one column per glyph — and two of
these are not merely off, they are wrong about a line that is measured:

- **The spinner is the worst of it.** `◐` is `A`, `◓` is `N`, `◑` is `A`, `◒` is
  `N`: in a CJK-width terminal **two of the four frames take two columns and two
  take one**, so the input box's top border shifts by a column twice per
  revolution — at 12 revolutions a second, for as long as a turn runs.
- **`≡` is the one marker that is `A`**, and it sits in a two-column gutter:
  every usage line shifts by a column.
- A `·` in the status line overflows the right margin `text::width` measured for
  it; a `─` in a full-width rule wraps the frame.

Three fixes, in order of how much they buy:

1. **Replace the spinner with a class-`N` set.** `⣾⣽⣻⢿⡿⣟⣯⣷` (braille, all
   `N`) is the modern convention and every coding typeface has it. This removes
   the only defect in the list that is *visible on a normal screen*.
2. **Replace `⏹` with `▪`** (`U+25AA`, class `N`). `⏹` is also the one glyph in
   the whole vocabulary with genuinely thin typeface coverage — it sits in
   Miscellaneous Technical, a block most monospace fonts skip.
3. **Move `≡` out of the gutter**, or accept it and let the ASCII mode cover the
   case: of the six non-ASCII gutter markers, five are `N` and this is the sixth.

#### The ASCII mode

Whatever the terminal does, there must be a mode that cannot go wrong. One
setting — `CAOCLI_GLYPHS=ascii`, or a key in `settings.json` — that makes every
glyph in this document ASCII, and the default stays `unicode`:

| | unicode | ascii |
|---|---|---|
| user | `› ` | `> ` |
| running / in progress | `▸ ` | `> ` |
| done / completed | `✔ ` | `+ ` |
| failed / denied | `✘ ` | `x ` |
| pending | `☐ ` | `- ` |
| more hidden | `⋮` | `:` |
| usage | `≡ ` | `= ` |
| interrupted | `⏹` | `!` |
| picker cursor | `❯ ` | `> ` |
| picker chosen | `✓ ` | `* ` |
| separator | ` · ` | ` \| ` |
| ellipsis | `…` | `...` |
| em dash | `—` | `-` |
| spinner | `⣾⣽⣻⢿⡿⣟⣯⣷` | `\|/-\` |
| box rule | `─` | `-` |
| secret mask | `•` | `*` |

**Every substitution but one is width-preserving**, and that is the property
worth designing for: the ascii column has the same columns as the unicode one
when the terminal resolves ambiguous as narrow, so switching modes does not
re-flow a single line. The exception is the ellipsis — `…` is one column and
`...` is three — which is why it is the one glyph whose ascii form the code has
to treat as a different string rather than a different character.

### 3.4 The typeface

The application cannot pick one, so this is a set of requirements and the fonts
that meet them. It belongs in the README as a recommendation, not in the code.

**Requirements, in order:**

1. **Monospace, with the nine markers present and drawn inside one cell.** The
   failure mode is not a missing glyph (terminals substitute) — it is a glyph
   drawn *wider than its cell*, which collides with the column beside it. Watch
   `⋮` and `≡` in particular.
2. **Ambiguous characters resolved narrow.** A terminal setting or a typeface
   property; either is fine, both is better.
3. **Ligatures off.** See §3.5.
4. **A CJK fallback on the same grid.** If the machine is CJK, the fallback's
   ideograph must be exactly two cells of the primary font, or every line with
   Chinese in it drifts.

**Recommended, roughly in order of how well they fit:**

- **Sarasa Term SC / 等距更纱黑体** — the single-font answer. Built on Iosevka
  for Latin, with CJK, and the `Term` variants exist precisely to make
  ambiguous-width glyphs narrow. Strict two-to-one grid.
- **Iosevka Term** — the widest symbol coverage of any coding typeface, so the
  nine markers are safe, and the Term variant is the narrow-ambiguous one.
  Latin-only, so pair it with a CJK fallback (or use Sarasa).
- **UDEV Gothic / HackGen** — the same "ambiguous is narrow" design goal, built
  from a Latin mono plus a CJK gothic. Good second opinion if Sarasa's Latin is
  not to taste.
- **JetBrains Mono** — the safe mainstream choice: good hinting, good CJK
  pairing. Check `⏹`, `⋮` and `≡` before committing to it; those are its thin
  spots, and the reason §3.3 recommends replacing `⏹` anyway.
- **Berkeley Mono, Commit Mono, Geist Mono** — the most refined Latin-only
  options. No CJK, so a fallback is mandatory rather than optional.

### 3.5 Three modifier rules

- **No italic, ever.** A monospace typeface that has no italic is *synthesised*
  in oblique by the terminal by slapping a slant on the roman, which is ugly at
  small sizes and worse at CJK. Nothing in the design needs it: emphasis is
  `signal` and secondary text is `muted`. Reserved and unused.
- **Bold is a weight, and it is spent on three things** — §3.2's L2 and L3. It
  is not used to make a paragraph stand out, because in a terminal a bold
  paragraph is a smear.
- **Underline is reserved for links.** Nothing underlines today; if the file-path
  and URL detection people want later gets written, `SGR 4` is what it gets.
  Until then, no underline — it is a second line of ink inside one row.

### 3.6 The rhythm: rows are the leading

The horizontal rhythm is fixed by the grid — a gutter is two columns, a
continuation line repeats the gutter, a nested list adds two more. Nothing to
design there. The vertical rhythm is the design:

| gap | between |
|---|---|
| 2 blank rows | the answer and what follows it |
| 1 blank row | one tool call and the next |
| 1 blank row | the standing task list and the transcript above it |
| 0 | inside a call: the header, its children, its verdict are one object |
| 0 | a thinking block's lines, and its fold line |

The rule underneath: **a blank row means "a new kind of thing starts here".**
Zero gaps inside a call is what makes `▸ Bash ls` and its four lines of output
read as one object rather than five lines that happen to be adjacent. Two blank
rows around an answer is what stops the answer from being read as another tool
result.

This is also why the app should recommend a **terminal line height of 1.15–1.25**
rather than the default. In a GUI the leading is inside the type; here it is
between the rows, and at 1.0 a two-column indent and a blank row are the only
things left saying "this is subordinate", which they cannot say at that density.

### 3.7 The measure, and what degrades

The answer is the one cell with no gutter, so it is as wide as the terminal.
That is one decision worth revisiting: 45–75 characters is a comfortable measure
for prose, 80–100 for technical writing, and a maximised 200-column terminal
gives the answer 200. **Recommended operating window: 100–120 columns**, at
which the answer gets 98–118 and the gutter still has room for the markers.

The app must stay excellent without it, so the degradation order is fixed — what
goes first is what the reader can most afford to lose, and nothing is ever cut
mid-word or mid-number. Steps 1, 2 and 3 are already the code's behaviour; 4 and
5 are the target, and the reason the order is written down is that a new segment
has to be inserted into it rather than placed wherever it fits:

1. At **< 120 columns**: banner segments drop from the right, in the order the
   banner already names (hint, then model, then count). A count is the most
   expendable thing on the screen.
2. At **< 100 columns**: the status line drops its raw hit/miss counts and keeps
   the hit rate — already so, in `Status::line`.
3. At **< 80 columns**: the working indicator drops its tokens-per-second
   estimate, then itself; a clipped spinner says less than no spinner.
4. At **< 60 columns**: the picker's detail column is dropped whole, because a
   label plus half a sentence is worse than a label. Today the detail is left to
   the row's own clipping, which is exactly the "half a sentence" this rules out.
5. At **< 40 columns**: everything but the transcript and the box goes.

Two things never degrade: the gutter (a wrapped line that loses its indent
stops reading as a continuation) and the markers (a step with no glyph is a step
with no status).

### 3.8 The zoom test

A terminal's font size is a keystroke away, so the design is really "how does
this look at 200%". At 200% a 120-column window becomes 60, which lands at §3.7's
fourth step. That is the case to keep checking: the transcript and the box, no
decorations, nothing truncated mid-token. If it reads well at 60 columns, it
reads well at any point size, and the font size is genuinely the user's to pick.

## 4. Terminal setup, in short

The README recommendation, once the design is settled:

- **Typeface**: Sarasa Term SC, or Iosevka Term plus a CJK fallback on the same
  two-to-one grid.
- **Ligatures**: off.
- **Line height**: 1.15–1.25.
- **Font size**: the largest at which the window is at least 100 columns. A
  rough starting point is 14 px on a HiDPI laptop, 16 px on a 27" display, and
  20 px or more when the screen is being shared.
- **Window**: at least 100×30. Below that the pinned regions start taking rows
  from the transcript.
- **Theme**: `auto` is right for almost every terminal. `--theme paper` on a
  light terminal whose background query doesn't answer; `--theme ink` on the
  rare embedder that lies.
- **Glyphs**: leave at `unicode` unless the working indicator visibly
  twitches, the box rule wraps, or a marker is missing from the font — in
  which case `--glyphs ascii` is the answer.

## 5. What lives where

This section is the index, not the changelog — the *change* log is the commit
history. What is here is which file holds which decision, so a reader fixing
something has a map rather than a treasure hunt.

**The palette and its tests** — `src/ui/theme.rs` and `src/ui/theme/tests.rs`.
The two themes are constants (`INK`, `PAPER`); the four tiers and the
glyph-resolution arithmetic live in `Theme`. The tests are *the* floor on what
is legible: contrast, CVD, tone split, and the three tiers' own spellings.

**The glyph set** — `src/ui/glyphs.rs`. Two constants (`UNICODE`, `ASCII`),
installed once by `startup::install_visuals` before any front end paints. The
test envelope in the same file enforces the width contract the screen assumes.

**The front ends' agreement** — every cell goes out as one of six `Style`
variants (`src/ui/cell/mod.rs`); both front ends spell those into colour the
same way, through `theme::style_code` (the plain front end's bytes) and
`theme::style_of` (the TUI's `ratatui` data). A style a caller picks is a
style both backends agree on; the theme is the agreement.

**The terminal background query** — `src/ui/terminal.rs::RealTerminal::background`.
Round trip `OSC 11 ; ?` with `ICANON`/`ECHO` off, polled with a 60 ms deadline,
and parsed by `parse_osc11` — which is split out so the spellings (`rgb:ff/ff/ff`,
`#ffffff`, `#fff`, one- to four-digit channels) are a test rather than a
runtime surprise.

**The picker cursor** — `src/ui/tui/picker.rs::cursor_style`. The picker's
highlight is `REVERSED` plus a `❯` in `signal`; the detail on the selected row
is *not* muted (the mod-and-colour combination is undefined under reverse video);
the cursor's colour is the foreground of the inverted cell.

**The box border** — `src/ui/tui/layout.rs::box_border_set`. Returns
`ratatui`'s plain set for unicode, a `+`/`-` set for ascii. Set once on the
screen's `Block`, not on the cell layer, because there is exactly one box.

**The install path** — `src/startup.rs::install_visuals`. Called by `main`
before the first turn runs. Reads the flags and the settings file, decides
the tier from the environment, asks the terminal for its background only when
neither of the other two has answered, and reports names nothing answers to.
