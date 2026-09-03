# Metalcraft Images pack

Generate and edit images with fal.ai through **Metalcraft Images**
(`https://images.metalcraftai.com`), billed in the user's **Metalcraft credits**.
Images runs every model call through Metalcraft Inference on the user's behalf, so
there is no second fal key and no second ledger.

## Auth
One credential: `METALCRAFT_TOKEN` (a Metalcraft ID `mck_` PAT with the `write`
scope), sent as `Authorization: Bearer` on every tool. The same token works across
every ecosystem app; the account is implied by the token. On a managed pod the
control plane injects it, so there is nothing to configure.

Generating, editing, and describing additionally require a **premium** account —
they spend credits. Reading history, sharing, and uploading do not.

## Tools
| Tool | Spends? | Purpose |
|------|---------|---------|
| `mimg_list_models` | no | Models with `modes` (text-to-image / image-to-image) and per-image price. Call first. |
| `mimg_generate_image` | **yes** | Text → image, saved to the gallery. |
| `mimg_edit_image` | **yes** | Image → image, from a gallery image, an upload, or a URL. |
| `mimg_describe_image` | **yes** | What is actually in an image, optionally judged against an expectation. |
| `mimg_upload_image` | no | One-call upload of a local file, for use as an edit source. |
| `mimg_share_image` | no | A time-boxed link anyone can open. |
| `mimg_list_generations` | no | History, newest first, paged. Also the timeout-recovery path. |
| `mimg_get_generation` | no | One generation in full. |

## The two things worth knowing
**A timeout is not a failure.** Generation is synchronous and can outlast the
tool's timeout while the work completes. The recovery is `mimg_list_generations`,
never a second generate — that charges twice for one image.

**An image's `url` is owner-only.** It is proxied from a private bucket behind the
owner's own auth. To hand a link to anyone else, use `mimg_share_image`.

## Editing needs an edit-capable model
`mimg_edit_image` requires a model whose inference catalog row declares
`image-to-image` (see `metalcraft-inference` migration `007_image_modes.sql`).
Asking a text-to-image model to edit is refused before any credits are
authorized, so the refusal is cheap and final — pick a different model rather
than retrying.
