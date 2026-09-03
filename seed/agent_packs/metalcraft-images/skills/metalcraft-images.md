---
description: Generate, edit, and check images with Metalcraft Images — pick a model, spend credits deliberately, look at the result, share or remix it
version: 1.0.0
---

# Metalcraft Images — generate, edit, and check

Generate and edit images through **Metalcraft Images**
(`https://images.metalcraftai.com`), which runs fal.ai models via Metalcraft
Inference and bills the user's **Metalcraft credits**. Auth is the ecosystem token
`METALCRAFT_TOKEN` (Bearer). Generating, editing, and describing are
**premium-only** and each spends real credits at the moment of the call.

## The loop

You cannot see. That single fact shapes this whole workflow: the tool that
generates an image tells you only that *an* image exists, and every prompt you
have ever written looked correct to you at the time. So the loop is not
generate-and-report — it is generate, **look**, then report.

1. **Pick a model** — `mimg_list_models`. It gives every model's `modes` and its
   `per_unit_credits` (micro-credits; ÷1000 for credits). Filter on `modes`:
   `text-to-image` to create, `image-to-image` to edit. Quote the cost when it
   matters: price × `num_images`.
2. **Get a source, if editing** — an image already in the gallery needs nothing:
   pass its id as `source_image_id`. Only reach for `mimg_upload_image` when the
   source is a local file, or `image_url` when it is a public URL.
3. **Generate or edit** — `mimg_generate_image` / `mimg_edit_image`. Both save to
   the user's gallery and return per-image `id`, `url`, `seed`, and the `credits`
   charged.
4. **Look at it** — `mimg_describe_image` with `expect` set to what the user
   asked for, in their words. Read the `evidence`, not just the `result`.
5. **Share or remix** — `mimg_share_image` for a link anyone can open;
   reuse a `seed` to vary a prompt around the same composition.

## Checking your work

`mimg_describe_image` is the only honest answer to "is this right?". Use it when
the request had a checkable requirement — text that must be spelled correctly, a
specific number of subjects, a named composition, "no people in frame" — and on
anything you are about to present as finished.

Read it properly:

- `evidence` is what the model can actually see, written before it judged.
  A `pass` with thin evidence is worth less than a `fail` with specific evidence.
- `result: "unsure"` means the image does not settle the question. It is not a
  pass. Say so plainly.
- `result: "unavailable"` means no verdict could be produced, and carries a
  `reason` (out of credits, not premium, the model went off-script). It is never
  a pass, and it is not a fact about the image.

On a `fail`, you may correct **once** without asking: re-roll with the seed
reused and the prompt adjusted to attack exactly what the evidence described. If
the second attempt also fails, stop. Report what the model saw both times and
let the user decide — do not keep spending their credits hunting for a pass.

And never discard an image on a verdict alone. The user paid for it, the verdict
can be wrong, and a `fail` you quietly hide is a charge with nothing to show for
it. Show what you made, say what the check found.

## Spending

Every generate, edit, and describe spends real money.

- Confirm before calling when the model, prompt, or count is ambiguous, or when
  `num_images` is large. Never generate in a speculative loop.
- Quote the cost when it is more than one image, and count the describe call in
  the total when you plan to check.
- A `402` means the account is not premium or is out of credits. Relay it
  plainly and **do not retry**.

## When a call times out

A generation runs synchronously: fal renders, the server downloads every image
and stores it, then answers. A big or slow run can outlast the tool's timeout
while the work itself keeps going and completes.

So a timeout is **not** a failure, and the recovery is never to call again:

1. `mimg_list_generations` — the newest row is almost certainly yours.
2. If it is `complete`, carry on with its images. If `pending`, wait and list
   again. If `error`, the credits were refunded and you may retry.

Re-running a timed-out generation charges the user twice for one image. This is
the most expensive mistake available in this pack.

## Notes

- The `url` on a generation's images is owner-authenticated — it opens only for
  the signed-in owner. To give a link to anyone else, use `mimg_share_image`, and
  say when it expires.
- Editing needs a model whose `modes` include `image-to-image`. Asking a
  text-to-image model to edit is refused before anything is billed, so trust the
  refusal rather than retrying with the same model.
- Never reveal `METALCRAFT_TOKEN` or raw tool URLs.
