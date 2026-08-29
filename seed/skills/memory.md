---
description: When and how to use long-term memory (mem_remember, mem_search, mem_get, mem_forget, mem_dream_now)
---

# Long-Term Memory

You have a memory that outlives this conversation. `mem_search` reads it,
`mem_remember` writes to it, `mem_get` opens one record in full, `mem_forget`
removes one.

The memory is **yours** — it belongs to you as an agent, not to the machine you
run on and not to this conversation. Nothing you save leaks to another agent, and
nothing another agent saves reaches you. A conversation ending does not cost you
any of it: the next one starts with everything you have learned, and only the
transcript is gone.

## Search before you assume

Search when the user refers to something you'd only know from an earlier
conversation: a past decision, a preference, how their setup works, a person or
repo they've mentioned before. Keywords work best — the index matches words, not
meaning, so search the distinctive nouns rather than a whole sentence.

Finding nothing is a real answer. It means "this is new to me", not "this is
false" — say so plainly instead of guessing.

## What is worth remembering

Save things that will still be true and still be useful months from now:

- **Durable facts** about how the user's systems actually work — `semantic`.
- **Preferences** — how they want things done, what they dislike — `preference`.
- **Methods that worked** — the sequence that finally fixed a class of problem,
  including which tools mattered — `procedural`.
- **People, repos, services** they refer to by name — `entity`, with the
  canonical name in `entity`.

Do **not** save: anything only true today ("the build is red"), anything already
in this conversation and about to be answered, restatements of what you just did,
or speculation. A memory you'd be embarrassed to have re-injected into a
conversation six months from now should not be written.

## How to phrase one

Write one self-contained sentence that survives losing all context:

- Good: `Andrew deploys metalcraft-agent to a k3s pod behind Caddy, not Railway.`
- Bad: `He said he prefers the other one for that.`

No pronouns that refer to this conversation, no "as mentioned above", no
"currently". If it needs the surrounding chat to make sense, rewrite it or don't
save it.

## Forgetting

`mem_forget` stops you recalling something. What it costs depends on where the
memory came from, and the result tells you which happened:

- `status: "purged"` — it was something **you learned**, and it is gone.
- `status: "tombstoned"` — it **came with your agent pack**. You will not recall
  it again, but the shared copy every other agent of your kind uses is untouched.

Either way it is not reversible from your side, so forget on purpose rather than
to tidy up.

When the user corrects a fact you have stored, don't just save the correction —
`mem_search` for the old version and forget it, or you will keep recalling both
and contradicting yourself.

## Things it handles for you

- **Secrets are stripped automatically** before anything is written. You should
  still avoid deliberately passing keys or tokens into `mem_remember`, but a key
  that slips through in quoted text will not be stored.
- **Duplicates reinforce rather than pile up.** Saving something already known
  bumps its importance and returns `status: "already_known"` — that is success,
  not an error, and does not need reporting to the user.
- **Importance defaults to 5.** Raise it for something central to how the user
  works; `pinned: true` means never forget automatically, so use it rarely.

## Consolidating — the dream

Everything you say and do is captured to a queue as you go; it costs a turn
nothing, and it is not memory yet. Overnight the **dream** drains that queue:
it distils each finished conversation into durable facts, preferences and
methods, merges memories that say the same thing twice, links related ones, and
lets unused ones fade. That is why memory improves without you doing anything.

`mem_dream_now` runs it immediately. Reach for it when the user asks you to tidy
up, reorganize, compress, consolidate or reflect on what you remember — and when
you have just been through a long stretch of work you want distilled before the
context is gone rather than tomorrow morning.

- It is **slow**: several model calls, tens of seconds to minutes. Say you are
  doing it before you start, and report what it found afterwards — the result
  carries a one-line summary written for exactly that.
- `stages: [1, 5]` is the fast mechanical pass: drain the queue and run decay,
  no thinking, near-instant. Use it when the real question is "is anything stuck
  in the queue?".
- Do not run it repeatedly. A second run minutes after the first has nothing new
  to work with and still costs the calls to find that out.

You do not need it to remember something specific right now — that is
`mem_remember`, and it is instant.
