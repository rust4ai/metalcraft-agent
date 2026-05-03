---
description: YouTube video script structure and writing methodology
---

# Video Scripting

## Script Structure

Every video script should follow this structure:

### 1. HOOK (0:00 - 0:15)
- Open with a compelling question, bold claim, or demo preview
- Show the end result first to create curiosity
- Keep it under 15 seconds

### 2. INTRO (0:15 - 0:45)
- Brief intro of what the video covers
- Why it matters to the viewer
- "By the end of this video, you'll know how to..."

### 3. BODY (main content)
- Break into 3-5 clear segments
- Each segment: concept → demo → recap
- Use transitions between segments
- Include chapter markers: `## [02:30] Segment Title`

### 4. OUTRO (last 30-60s)
- Summarize key takeaways (3 bullet points max)
- Call to action: like, subscribe, comment
- Tease next video if applicable

## Script Formatting

Use these markers throughout the script:

### Visual Cues
- `[SCREEN]` — Screen recording (code editor, terminal, browser)
- `[CAMERA]` — Face cam / talking head
- `[SLIDE]` — Full-screen graphic or slide
- `[SPLIT]` — Split screen (face cam + screen)
- `[B-ROLL]` — Supplementary footage or animation

### Editing Cues
- `[CUT]` — Hard cut
- `[TRANSITION: type]` — Transition (fade, swipe, zoom)
- `[ZOOM: target]` — Zoom into specific area
- `[HIGHLIGHT: target]` — Highlight/callout on screen
- `[LOWER-THIRD: text]` — Lower third text overlay
- `[SFX: description]` — Sound effect

### Narration
- Write narration as plain text (what you say out loud)
- Use **(pause)** for dramatic pauses
- Use **[EMPHASIS]** before words to stress
- Keep sentences short — you need to breathe

## Tech Tutorial Tips

When scripting code walkthroughs:
1. Research the codebase FIRST using explore-codebase skill
2. Identify the 3-5 most important files/concepts to show
3. Plan a logical order: setup → core concept → build → result
4. For each code segment, note which file and lines to show on screen
5. Simplify explanations — if you can't explain it simply, research more
6. Add `[ZOOM: filename:line]` cues so the editor knows where to focus
