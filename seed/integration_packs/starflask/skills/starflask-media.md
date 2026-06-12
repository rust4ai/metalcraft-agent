---
description: How to generate media (images, video, 3D, speech) with the Starflask API — the job lifecycle, payload shapes, and model reference
---

# Starflask Media Generation

Starflask (starflask.com) is an AI media-generation platform. You create media by
submitting **jobs** and collecting their results. Everything is authenticated by a
single `STARFLASK_API_KEY` bound to one account — you never pass an account id; each
tool already carries the key.

## The job lifecycle (always follow this)

Every generation is **asynchronous**:

1. **Create.** Call the right generator tool. It returns *only* a job id —
   `{"data": {"id": "<uuid>"}}`. Nothing is generated yet.
2. **Poll.** Call `starflask_get_job` with that id. `status` moves
   `pending -> processing -> completed | failed`. Wait a few seconds between polls;
   image jobs are usually quick, video/3D can take a minute or two.
3. **Deliver.** When `status` is `completed`, read the URL out of `result` and give
   it to the user. When `failed`, read `result`/`error` and report it plainly.

Never tell the user something is ready before a job reports `completed`.

## Editing an existing image (one of Starflask's best features)

Starflask doesn't just generate from scratch — it **edits images you already have**:
change a background, add or remove an object, restyle, fix a detail, extend a scene.
Use the dedicated **`starflask_edit_image`** tool: it takes the source image as
`image_url` and your change as `prompt`, and already defaults to an editing-capable
model (`nano-banana-edit`) so you don't have to pair one yourself.

End-to-end:

1. **Upload the source.** `starflask_upload_media` with the local `file_path` (must
   be inside the upload root). Take the returned `url`. (Skip this if the image is
   already a URL — e.g. `result.image_url` from a prior job.)
2. **Submit the edit.** `starflask_edit_image` with:
   - `image_url` = the uploaded url
   - `prompt` = the edit instruction, e.g. *"replace the sky with a sunset"*,
     *"remove the person on the left"*, *"make the jacket red"*
   - (optional) `model_key` to override the `nano-banana-edit` default with another
     editing model from `starflask_list_models`.
3. **Poll & deliver.** `starflask_get_job` until `completed`, then hand the user
   `result.image_url`.

Be specific about what changes and what stays. Offer to iterate — feed the returned
`result.image_url` straight back in as the next edit's `image_url` to refine.
(`starflask_edit_image` is just `starflask_generate_image` specialized for the
image-in/image-out case; you can still drive an edit through `starflask_generate_image`
directly by passing `image_url` + an editing `model_key`.)

For non-prompt transforms (upscale, remove background, vectorize, cartoonify,
resize, crop) see `starflask_create_job`; for pure format changes see
`starflask_convert_image` — both below.

Result URLs by job type:
- `image:generate` / `image:convert` -> `result.image_url` (or `result.svg_url` for svg)
- `video:generate` / animate -> `result.video_url`
- `mesh:generate` (3D mesh) -> `result.model_url` (plus `result.thumbnail_url`)
- `vector:generate` -> `result.svg_url`
- `video:convert` (format conversion — mp4 → animated webp / gif / mp4 / webm) -> `result.video_url`
- `audio:generate`, or `video:convert` with `audio_only` (audio extraction) -> the audio URL field in `result`

Result URLs are presigned and downloadable directly. If one expires, re-fetch fresh
metadata with `starflask_get_media`.

## Tools

- **`starflask_generate_image`** — text-to-image from a prompt. Headline tool.
- **`starflask_edit_image`** — **edit an existing image**: pass `image_url` + an
  edit `prompt`; defaults to the `nano-banana-edit` editing model (see "Editing an
  existing image" above).
- **`starflask_generate_video`** — text-to-video and image-to-video (animate a still).
- **`starflask_generate_3d`** — text-to-3D and image-to-3D meshes (job type `mesh:generate`).
- **`starflask_generate_speech`** — text-to-speech (job type `audio:generate`).
- **`starflask_convert_video`** — convert/transcode a video to another format
  (job type `video:convert`): **mp4 → animated WebP** (default), animated GIF, mp4, or webm.
- **`starflask_convert_image`** — convert an image to another format
  (job type `image:convert`): png, jpeg, webp (default), gif, or svg.
- **`starflask_create_job`** — anything else by raw type: `image:upscale`, `image:remove_bg`,
  `image:vectorize`, `image:cartoonify`, `image:resize`, `image:crop`,
  `audio:transcribe`, etc. The general-purpose escape hatch.
- **`starflask_get_job`** — poll a job's status/result. Use after every create.
- **`starflask_list_models`** — discover `model_key` values and each model's
  `input_schema` and `credits_cost`. Check here when unsure; don't invent keys.
- **`starflask_list_styles`** — image styles for `image_style_key`.
- **`starflask_upload_media`** — upload a local file (under the upload root) to get a
  URL to feed into image-to-video / image-to-3D / edit / upscale / remove-bg jobs.
- **`starflask_get_media`** — fresh metadata + presigned URL for a media id.
- **`starflask_account`** — `{plan, credits, ...}`. Check credits before big jobs.

## Tool parameters

The generator tools take **flat** parameters — you don't build a nested JSON
payload yourself; each tool assembles the Starflask job body for you. Every tool
has a sensible default `model_key`, so the only required field is the content.

- **`starflask_generate_image`**: `prompt` (required); optional `model_key`
  (default `ideogram-v3`), `aspect_ratio` (e.g. `"16:9"`, `"1:1"`, `"9:16"`),
  `image_style_key` (from `starflask_list_styles`), and `image_url` (to edit an
  existing image — pair with an editing model like `nano-banana-edit`).
- **`starflask_generate_video`**: `prompt` (required); optional `model_key`
  (default `kling-text2video`), `duration` (seconds), and `image_url` to animate a
  still (set `model_key` to `kling-img2video`).
- **`starflask_generate_3d`**: `prompt` *or* `image_url`; optional `model_key`
  (default `meshy-v6`), `topology`, `target_polycount`, `enable_pbr`.
- **`starflask_generate_speech`**: `text` (required); optional `voice`, `model_key`
  (default `chatterbox-tts`).
- **`starflask_convert_video`**: `video_url` (required) — the source video, from
  `starflask_upload_media` or a prior job's `result.video_url`; optional
  `target_format` (`webp` = animated WebP, the default; `gif`, `mp4`, `webm`), `fps`
  (default 15), `max_width` (default 640), `start_time`, `duration`. Read the
  converted file from `result.video_url`.
- **`starflask_convert_image`**: `image_url` (required) — the source image, from
  `starflask_upload_media` or a prior job's `result.image_url`; optional
  `target_format` (`webp` default, `png`, `jpeg`, `gif`, `svg`). Read the result
  from `result.image_url` (or `result.svg_url` for svg).
- **`starflask_create_job`**: `type` + `model_key` (required); optional `prompt`,
  `image_url`. The escape hatch for `image:upscale`, `image:remove_bg`, `image:vectorize`,
  `image:cartoonify`, `image:resize`, etc.

Always defer to a model's real `input_schema` from `starflask_list_models` when a
request is non-trivial — that's the source of truth for which params a model
accepts and the allowed values (aspect ratios, voices, topology, …).

## Common model keys (verify with starflask_list_models)

- Images: `ideogram-v3` (default, excellent typography), `gpt-image-2`,
  `gpt-image-2-hi` (higher quality, more credits), `nano-banana` /
  `nano-banana-edit` (editing).
- Video: `kling-text2video`, `kling-img2video`.
- 3D: `meshy-v6`.
- Speech / audio: `chatterbox-tts` (TTS), `elevenlabs-scribe-v2` (transcription).
- Utility: `creative-upscaler` (upscale), `birefnet` (background removal),
  `recraft-vectorize` (vectorize), `cartoonify`, `ffmpeg-convert` (video format
  conversion, used by `starflask_convert_video`). Image format conversion
  (`starflask_convert_image`, job type `image:convert`) needs no model_key.

## Working with source images

Image-to-video, image-to-3D, editing, upscaling, and background removal all need an
input the server can fetch. Workflow:

1. `starflask_upload_media` with a local `file_path` (must be inside the upload root).
2. Take the returned `url`.
3. Pass it as `image_url` (or `video_url`) inside the job's `params`.

You can also chain jobs: the `image_url` from a completed image job can feed straight
into an image-to-video or image-to-3D job.

## Credits & etiquette

- Media costs credits per run (see each model's `credits_cost`). Before expensive or
  bulk runs, check `starflask_account` and warn the user if the balance is low.
- Write specific prompts (subject, composition, lighting, style, aspect ratio) and
  offer to refine/regenerate rather than burning credits on vague requests.
- Never expose the API key or raw tool URLs.
