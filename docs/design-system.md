# The Metalcraft design system

*Cold steel on warm paper.*

This is the visual and behavioural language every Metalcraft surface is built
from — the pod's own web UI, metalcraft-front, the iOS client, the marketing
site, and any app that wants to look like it belongs to the same product.

It is derived from one thing: **the hero of metalcraftai.com**. That hero is not
a picture of the product, it *is* the product — a real agent window, signed in
or scripted, in the identical frame either way. Everything below is that window
written down, so a second surface can be built without looking at it.

---

## 1. The idea

Three commitments, in the order they matter.

**The work is the interface.** The hero has no headline. The first thing on the
page is the agent window itself, and every decision below serves the content
inside it: transcripts, tool calls, plans, files. Chrome is hairlines and
whitespace, never colour. If a screen has a decorative element competing with
what the agent said, the decoration is wrong.

**Cold steel on warm paper.** The palette is a slate-grey machine sitting on a
warm off-white page. There is exactly one accent — Steel — and it is spent on
what is *active*: the live node, the link, the focused field, the running tool.
Colour is a signal, and a surface that tints six things has told you nothing.

**Show the machine.** Tool calls, elapsed seconds, plan steps, the reset line,
the pod's own error wording — none of it is hidden behind a spinner. Latency is
content. A tool the agent ran is a fact about your data and gets a chip; a turn
that took 40 seconds says so. The aesthetic is *legible mechanism*, which is why
the type pairs a humanist grotesk with a mono: prose for what it said, mono for
what it did.

---

## 2. Colour

### 2.1 Core

| Token | Hex | What it is |
| --- | --- | --- |
| `ink` | `#1c2330` | Near-black slate. Headings, dark panels, the user's own words. |
| `night` | `#161c24` | The deepest panel — footers, dark page ground. |
| `night-2` | `#20272f` | One step up from Night. Raised surfaces on dark. |
| `steel` (accent) | `#4d6a9c` | The one accent. Links, active nodes, focus, running work. |
| `steel-light` | `#8fa9d6` | Steel that survives a dark background. |
| `page` | `#eef0f2` | The warm off-white ground everything sits on. |
| `surface` | `#ffffff` | Cards, panels, the window itself. |

### 2.2 Neutral and signal

| Token | Hex | What it is |
| --- | --- | --- |
| `slate` | `#54606e` | Body copy. |
| `slate-2` | `#5a6572` | Secondary body, chip text. |
| `prose` | `#33415a` | What the agent says. Warmer than body — it is speech. |
| `faint` | `#7f8b98` | Dim labels on dark. |
| `faint-2` | `#9aa6b2` | Dim labels on light. Placeholders, counts. |
| `line` | `#dcdfe3` | Hairline borders on light. |
| `line-2` | `#e2e5e9` | Card and panel borders — a shade softer than `line`. |
| `line-dark` | `#323b48` | Borders inside dark panels. |
| `code` | `#cdd5de` | Code text on dark. |
| `signal` | `#8fbf9f` | String literals in code. Nothing else. |
| `live` | `#6f9d7c` | Something is really running. |
| `idle` | `#aab4c0` | Something is not. |

### 2.3 Alarm

Never pure red. The error tone is a clay that belongs to the same palette.

| Token | Hex | Use |
| --- | --- | --- |
| `alarm-text` | `#8f4a34` | Error wording. |
| `alarm-bg` | `#fbf3f0` | Error panel ground. |
| `alarm-line` | `#e0c3b8` | Error panel border. |
| `alarm-strong` | `#c0555a` | "Don't" markers, destructive verbs. |

### 2.4 App hues

The tile row under the hero is the only place saturated colour is allowed, and
each app owns exactly one hue. A tile is a two-stop vertical gradient with a
matching soft shadow (`ring`) and a white glyph on top.

| Hue | From | To | Ring |
| --- | --- | --- | --- |
| steel | `#5f81bd` | `#3f5b8c` | `rgba(77,106,156,0.35)` |
| sage | `#84a98c` | `#5d8168` | `rgba(120,158,127,0.35)` |
| indigo | `#7b83b8` | `#565e93` | `rgba(107,116,168,0.35)` |
| clay | `#bb8f66` | `#946c48` | `rgba(168,130,91,0.35)` |
| plum | `#9a7bab` | `#755787` | `rgba(138,107,156,0.35)` |
| teal | `#5fa0ab` | `#3d7a85` | `rgba(77,143,156,0.35)` |
| graphite | `#79828e` | `#525a66` | `rgba(90,99,110,0.35)` |
| amber | `#c39a4e` | `#9a7530` | `rgba(170,133,60,0.35)` |

They are desaturated on purpose. A row of six reads as a set of apps rather than
a row of highlighter pens, and none of them out-shouts Steel.

### 2.5 Dark

The web is light-only. Native clients are not — a phone at 11pm is a dark-mode
phone — so dark is a defined mode, not an inversion. **Roles hold, values move.**

**Dark is opt-in, not inherited.** A client that offers both defaults to *light*
and puts Light / Dark / System behind a setting. The system is drawn light — cold
steel on warm paper is the whole premise — and a phone that happens to be in dark
mode should not decide on somebody's behalf that this is a dark product.

| Role | Light | Dark |
| --- | --- | --- |
| page ground | `#eef0f2` | `#12161d` |
| surface / card | `#ffffff` | `#1a212b` |
| raised surface | `#fafbfc` | `#20272f` |
| hairline | `#e2e5e9` | `#2b3441` |
| strong line | `#dcdfe3` | `#323b48` |
| primary text | `#1c2330` | `#eef2f6` |
| body text | `#54606e` | `#c3ccd6` |
| agent prose | `#33415a` | `#ccd6e4` |
| dim label | `#9aa6b2` | `#8b97a5` |
| accent | `#4d6a9c` | `#8fa9d6` |
| solid accent fill | `#4d6a9c` | `#4d6a9c` |

Two rules make dark mode not look like a different product:

1. **Steel lightens for text and holds for fills.** `#4d6a9c` on `#12161d` fails
   contrast as a label and is perfect as a button.
2. **The user's bubble is the darkest thing on a light page and the bluest thing
   on a dark one.** Ink on white; Steel on night. It stays the one filled
   element in the transcript either way.

---

## 3. Type

Two families do everything.

**Hanken Grotesk** — display and UI. Weights 400 / 500 / 600 / 700 / 800.
**Space Mono** — code, labels, data, anything the machine produced. 400 / 700.

Tracking is the tell. Display type is tight; mono labels are wide.

| Role | Family | Size / weight | Tracking |
| --- | --- | --- | --- |
| Page title | Hanken 800 | 36–52px, line-height 1.03 | `-0.035em` |
| Section title | Hanken 800 | 26–32px, 1.1 | `-0.025em` |
| Wordmark | Hanken 800 | matches mark cap height | `-0.025em` |
| Card title | Hanken 700 | 14–15px | `-0.01em` |
| Body | Hanken 400 | 15–17px, 1.55 | `0` |
| Agent prose | Hanken 400 | 14px, 1.62 | `0` |
| Eyebrow / label | Space Mono 400 | 10–12px, UPPERCASE | `0.14–0.16em` |
| Chip / meta | Space Mono 400 | 10.5–13px | `0` |
| Code | Space Mono 400 | 13–13.5px | `0` |

**The eyebrow is the system's signature.** A small uppercase mono kicker with
wide tracking sits above section headings, over the agent rail, over the tile
row ("WHAT IT CAN REACH"), and inside every panel that needs a name. It is how a
surface says "this is Metalcraft" without a logo.

Mono is not decoration. It marks **machine facts**: tool names, versions, pod
slugs, ids, durations, cron expressions, counts, hexes, file paths. If a person
wrote it, it is set in Hanken.

---

## 4. Geometry

### 4.1 Radii

A deliberate ladder, largest outside, smallest inside.

| Radius | Applies to |
| --- | --- |
| 20 | The window / primary panel |
| 15 | Composer field, chat bubble (tail corner 5) |
| 13 | Tile |
| 12 | Secondary panel, plan card |
| 11 | Icon tile, alarm note |
| 10 | Button |
| 9 | Rail row, small input |
| pill | Status, chips, counts |

All continuous (`style: .continuous` on iOS, which is what CSS `border-radius`
looks like at these sizes).

### 4.2 Lines and shadows

Structure is carried by **1px hairlines**, not by shadows. Shadows exist only to
say "this floats above the page", and they are wide, soft, and offset down —
never a dark halo.

```
window        0 34px 80px -40px rgba(28,35,48,0.45)
tile (hover)  0 10px 24px -14px rgba(28,35,48,0.40)
composer      0 2px 10px -6px  rgba(28,35,48,0.35)
composer:focus 0 4px 18px -10px rgba(77,106,156,0.60)
rail row      0 1px 3px        rgba(28,35,48,0.09)
icon tile     drop-shadow 0 4px 10px <hue.ring>
```

Note the negative spread everywhere: the shadow is smaller than the element, so
it reads as lift rather than as an outline.

### 4.3 Spacing

A 4px grid. Panel padding 14–16, section rhythm 16 / 20 / 24, page gutters
24 (mobile) and 40 (desktop), content shell max 1180px.

---

## 5. The mark

A single graph node wired to four neighbours — the atom of a Metalcraft graph.
Four spokes in a diamond, one vertical axis in Steel, four Ink nodes, and a
larger Steel node in the centre.

```
viewBox 0 0 48 48, stroke-width 1.6
spokes  (24,8)→(40,24)→(24,40)→(8,24)→(24,8)   stroke #8b97a3
axis    (24,8)→(24,40)                          stroke #4d6a9c
nodes   r 3.4 at each of the four points        fill  #1c2330
centre  r 4.6 at (24,24)                        fill  #4d6a9c
```

Reversed on dark: spokes `#5a6572`, nodes `#cdd5de`, axis and centre unchanged.

Rules: clear space of one node diameter on every side; never recolour, rotate or
stretch it; never place the dark mark on a dark ground; lock it up with the
wordmark at equal cap height, mark leading.

The mark does double duty as **the agent's avatar** — it is what sits beside
everything the agent says, at 15px inside a 26px white circle with a hairline.

---

## 6. Components

### 6.1 The window

The primary container: `radius 20`, `1px line-2` border, white, window shadow.
Three bands.

- **Top bar.** The mark at 20px on the left, a status pill pushed to the right,
  a `180deg #fbfcfd → #f5f7f8` gradient ground, hairline underneath. Nothing
  private goes here — the pod's hostname is an address, not a title.
- **Body.** A *definite* height, never a minimum: the scroll region inside only
  scrolls against one, and a growing panel pushes everything below it off the
  screen. A 196px agent rail on the left where there is room; on a phone the
  rail is a screen, not a column.
- **Tile strip.** The apps this agent can reach, on the raised `#fafbfc` ground
  behind an eyebrow.

### 6.2 Status pill

Capsule, hairline border, `page/70` ground, 6px dot, mono uppercase 10px at
`0.13em`. Three tones and nothing else: **live** `#6f9d7c`, **idle** `#aab4c0`,
**working** Steel *and pulsing*. Motion is the tone — a working dot that does
not move is an idle dot in a different colour.

### 6.3 Rail row

9-radius, 2.5/2 padding, a 6px dot, a 13px name, a 10px mono sub-line. Selected
is a **white card with the rail shadow**, not a coloured fill: the row you are
on has come forward, it has not been highlighted.

The "+ New agent" affordance is a *dashed* hairline button — the one dashed
border in the system, reserved for "this does not exist yet".

### 6.4 Transcript

The single most important rule in this document:

> **The user gets a bubble. The agent does not.**

The user's message is a filled Ink bubble, right-aligned, max 86% width, radius
15 with the bottom-right corner at 5. The agent's reply is **prose set beside
its mark** — no bubble, no ground, `#33415a` at 14px / 1.62. Agents write
paragraphs, code and lists; a wall of chat bubbles reads as a toy and makes long
answers unreadable.

Between them sit the machine rows:

- **Tool chips.** A wrapped row of capsules indented to the prose column
  (34px), hairline, `#f7f8fa` ground, mono 10.5px: `✓` in `live` green when
  done, a pulsing Steel `•` while running, then the tool name.
- **Thinking.** Three 5px Steel dots bouncing at 140ms offsets, and the label,
  in the prose column. Where a wait can be measured, the elapsed time is
  rendered in mono beside it.
- **Plan.** A hairline card, mono eyebrow `PLAN · 3/5`, one row per step with a
  12px marker column. Done steps are struck through and dimmed; the current one
  is full-strength. It is the only structured piece of an agent's reasoning, so
  it is the only piece worth drawing.
- **Queued.** The user's bubble shape, dashed and dimmed. It *is* their message
  and is about to be exactly that; anything more decorative reads as an error.
- **Reset divider.** A hairline rule across the column with a small centred
  Steel label — `reset · 2:14 PM`. Not a bubble: nobody said it.
- **Error.** The alarm panel, radius 12, carrying the pod's own wording, a mono
  code chip, and a link to what actually happened. Never a modal.

The transcript **bottom-aligns when short** and scrolls its own container, never
the page.

### 6.5 Composer

A 15-radius hairline box on `surface` with the composer shadow, growing 1–5
lines. On focus the border goes `steel/55` and the shadow becomes the Steel one
— the only glow in the system.

The send control is a **30px filled Ink circle** with an upward arrow, disabled
to `#d6dade` / `#9aa6b2`. While a turn runs it becomes a stop square, and send
rejoins it the moment there is something to queue — the button under the thumb
never changes meaning.

Enter sends, shift-enter newlines, and **the draft survives a failed send.**

### 6.6 Tiles

13-radius hairline card, hue icon tile (11-radius, two-stop gradient, white
1.7px stroked glyph, a `0 40 20` white-to-transparent top highlight for
dimensionality), a 13.5px semibold name and an 11.5px dim tagline. Hover lifts
1px, borders to `steel/35`, takes the tile shadow.

A tile is a **door, not a control**. Tapping it shows you the thing. It does not
install, commission, or configure.

### 6.7 Buttons

| Kind | Look |
| --- | --- |
| Primary | Ink fill, `#eef2f6` text, radius 10, bold 15px, hover to 90% opacity |
| Secondary | `surface` on a `#cfd4d9` hairline, Ink text, hover border and text to Steel |
| Quiet | Text only in `slate-2`, hover to Steel |
| Destructive | `alarm-strong` text; a fill only when the action is the page's point |

Focus is always a 2px Steel ring with a 2px offset. Never remove it.

### 6.8 Command / data strip

The `$ cargo add metalcraft` affordance: 9-radius hairline box, mono 13.5px,
a Steel `$`, a hairline divider, a dim version. Any place a copyable machine
fact needs to sit in a row of buttons uses this shape.

---

## 7. Motion

Motion reports state; it does not decorate.

- 150–200ms on colour and border. 200ms on a 1px hover lift.
- Pulse belongs to exactly one thing: work in progress.
- Bounce belongs to exactly one thing: the model is thinking.
- Scroll-to-bottom is animated; **arriving at a screen is not**. Never animate
  a transcript into place — somebody is reading it.
- Nothing moves for longer than the thing it describes.

---

## 8. Voice

The copy is part of the design, and it has a specific register: plain,
unhedged, second person, and it says what actually happened.

- **Name the mechanism.** "Nothing answered `POST {base}/responses`", not
  "Something went wrong."
- **The pod's words beat ours.** Show the refusal as the pod worded it — it
  knows which credential is missing and which automation still fires.
- **Quiet is not empty.** "Nothing active in the last few days" and a pointer to
  where the rest are, never a blank screen.
- **Say the size of the thing.** "3 more in Agents, quiet for over three days."
- **No exclamation marks. No "Oops". No emoji in product copy.**
- Sentence case everywhere except mono eyebrows, which are uppercase.

---

## 9. Platform mapping

The system is defined in web tokens because that is where it was drawn. Native
clients map, they do not re-invent.

| Web | SwiftUI |
| --- | --- |
| `bg-surface` | `Theme.surface` (dynamic `UIColor`) |
| `border-line-2` | `Theme.hairline`, 1px `strokeBorder` |
| `rounded-[20px]` | `RoundedRectangle(cornerRadius: 20, style: .continuous)` |
| `font-sans` | `Font.brand(_:_:)` → Hanken Grotesk |
| `font-mono uppercase tracking-[0.15em]` | `Font.mono(...)` + `.tracking(1.4)` + `.textCase(.uppercase)` |
| window shadow | `.shadow(color:radius:x:y:)` with the same soft, offset-down values |
| hover lift | there is no hover on a phone — use the pressed state instead |
| the agent rail | the root screen |
| the inspector | a sheet off the conversation's toolbar |

Two things are **not** portable and should not be faked: hover, and the desktop
three-column frame. Resolve them by size class rather than imitating them.

---

## 10. Do and don't

**Do**

- Put the work first and let chrome be hairlines.
- Spend Steel on exactly one thing per screen.
- Set every machine fact in mono.
- Show tool calls, plans, durations and the pod's own errors.
- Give the agent prose and the person a bubble.
- Keep the mark as shipped.

**Don't**

- Tint a surface to make it interesting.
- Put the agent's replies in bubbles.
- Use pure red, pure black, or a saturated blue.
- Hide latency behind an indeterminate spinner.
- Animate anything that isn't reporting state.
- Recolour, rotate, or restyle the mark.

---

*The reference implementation of every rule above is the hero at
metalcraftai.com (`metalcraft-ai-web/frontend/src/components/hero/`), and the
brand kit page at `/brand`.*
