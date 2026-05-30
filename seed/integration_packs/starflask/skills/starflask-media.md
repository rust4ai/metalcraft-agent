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

Result URLs by job type:
- image -> `result.image_url`
- video / animate -> `result.video_url`
- 3D mesh -> `result.model_url` (plus `result.thumbnail_url`)
- vector -> `result.svg_url`
- audio / clip conversion -> the URL field in `result`

Result URLs are presigned and downloadable directly. If one expires, re-fetch fresh
metadata with `starflask_get_media`.

## Tools

- **`starflask_generate_image`** — text-to-image and image editing. Headline tool.
- **`starflask_generate_video`** — text-to-video and image-to-video (animate a still).
- **`starflask_generate_3d`** — text-to-3D and image-to-3D meshes (job type `mesh`).
- **`starflask_generate_speech`** — text-to-speech (job type `audio`).
- **`starflask_create_job`** — anything else by raw type: `upscale`, `remove_bg`,
  `vectorize`, `cartoonify`, `clip` (media convert), `image:resize`, `image:crop`,
  `audio:transcribe`, etc. The general-purpose escape hatch.
- **`starflask_get_job`** — poll a job's status/result. Use after every create.
- **`starflask_list_models`** — discover `model_key` values and each model's
  `input_schema` and `credits_cost`. Check here when unsure; don't invent keys.
- **`starflask_list_styles`** — image styles for `image_style_key`.
- **`starflask_upload_media`** — upload a local file (under the upload root) to get a
  URL to feed into image-to-video / image-to-3D / edit / upscale / remove-bg jobs.
- **`starflask_get_media`** — fresh metadata + presigned URL for a media id.
- **`starflask_account`** — `{plan, credits, ...}`. Check credits before big jobs.

## Payload shapes

Generation requests use a `payload` object. Two forms for images:

- Quick: `{"prompt": "a tiny red fox sitting on a mushroom"}` — default model + style.
- Structured (to choose a model / params / style):
  ```json
  {
    "structured_data": {
      "model_key": "gpt-image-2",
      "params": { "prompt": "...", "aspect_ratio": "16:9" }
    },
    "image_style_key": "cinematic"
  }
  ```

Video (`starflask_generate_video`):
```json
{ "structured_data": { "model_key": "kling-text2video", "params": { "prompt": "...", "duration": 5 } } }
```
Image-to-video: `model_key` `"kling-img2video"`, params `{ "image_url": "<uploaded url>", "prompt": "...", "duration": 5 }`.

3D (`starflask_generate_3d`, sent as type `mesh`):
```json
{ "structured_data": { "model_key": "meshy-v6", "params": { "prompt": "a low-poly treasure chest", "topology": "triangle", "target_polycount": 30000, "enable_pbr": true } } }
```
Image-to-3D: use params `{ "image_url": "<uploaded url>" }`.

Speech (`starflask_generate_speech`, type `audio`):
```json
{ "structured_data": { "model_key": "chatterbox-tts", "params": { "text": "Hello there", "voice": "<voice-id>" } } }
```

These are starting points — always defer to a model's real `input_schema` from
`starflask_list_models` when a request is non-trivial.

## Common model keys (verify with starflask_list_models)

- Images: `ideogram-v3` (default, excellent typography), `gpt-image-2`,
  `gpt-image-2-hi` (higher quality, more credits), `nano-banana` /
  `nano-banana-edit` (editing).
- Video: `kling-text2video`, `kling-img2video`.
- 3D: `meshy-v6`.
- Speech / audio: `chatterbox-tts` (TTS), `elevenlabs-scribe-v2` (transcription).
- Utility: `creative-upscaler` (upscale), `birefnet` (background removal),
  `recraft-vectorize` (vectorize), `cartoonify`.

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
