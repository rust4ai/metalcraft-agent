# Coordinator — the cross-tenant relay (Plan B, Phase 6)

The pod-native apps are single-user and backend-only, so three flows can't live
purely in a pod because they cross tenants or face the public internet:

1. **Notes public sharing across pods** — a shareable link on a neutral domain
   (`/p/{token}`) that resolves to *whichever* pod owns the note.
2. **Calendar external-guest invites/RSVP** — inviting people who have no pod;
   emailing them; a public RSVP page; syncing their answer back to the
   organizer's pod.
3. *(future)* Drive public file links.

The **Coordinator** is the small, mostly-stateless cloud service that owns
exactly this cross-tenant routing — and *only* this. The user's actual notes,
events, and files stay in their pod; the Coordinator holds routing rows
(share-token → pod) and invite/RSVP rows, never content. It is the **only
always-on cloud DB** in the whole design, and a deliberately tiny one.

Status: **design.** The pod-side halves that don't need it are already built (see
"Already built" below); the cloud service itself awaits the decisions in §6.

---

## What's already built (pod side)

- **Notes sharing is pod-local for the owner's own shares.** A note's
  `share`/`unshare` set a `public_token`, and the pod serves its own public
  `/p/{token}` render page (comrak + ammonia, unauthenticated). For a *single
  pod* this already works end-to-end — no Coordinator needed. The Coordinator
  only adds **cross-pod token routing** so a *neutral* domain can find the right
  pod (§3).
- **Calendar events + reminders are pod-local.** What's missing is purely the
  invite/RSVP/email plane (§4), which is inherently multi-party.

So the Coordinator's job shrinks to: (a) a token→pod index + public passthrough
for shares, and (b) the calendar invite/RSVP/email subsystem.

---

## 2. Shape

- A small Rust/Axum service (mirrors the other ecosystem subapps), on a neutral
  domain (e.g. `share.metalcraftai.com` / `rsvp.metalcraftai.com`, or one
  `coordinator.metalcraftai.com`).
- **Small DB** (Neon/Postgres or even SQLite/D1): two logical tables —
  `shares(token → pod_slug, kind, ref)` and `invites(token, event_snapshot,
  organizer_pod, guest_email, rsvp, …)`. No user content.
- Talks to pods over their existing ingress (`<slug>.pods.metalcraftai.com`) with
  a **connection token** (the same aud-scoped `mck_` token clients already use);
  pods expose a couple of coordinator-facing endpoints.
- Reuses the ecosystem's **metalcraft-id** for any human auth and **Resend** for
  email (the cloud calendar already uses Resend).

---

## 3. Notes sharing across pods

**Register:** when a pod shares a note, it POSTs `{token, note_slug}` to the
Coordinator, which stores `token → (pod_slug, "note", note_slug)`. Unshare
deletes the row.

**Serve:** `GET coordinator/p/{token}` looks up the pod, fetches the rendered
page from that pod's `/apps/metalcraft-notes/p/{token}` (pod→pod, over ingress),
and returns it. Bytes/HTML never persist in the Coordinator — it's a router.

This is the notes-r2 "D1 shares index" idea, generalized to pods. It's small and
low-risk; it's the recommended **first Coordinator slice**.

---

## 4. Calendar external invites / RSVP (the hard part)

The genuinely multi-party flow. Sequence:

1. **Outbound:** organizer's pod (via `mcal_add_guests`) POSTs to the Coordinator
   `{event snapshot (title/time/location), organizer_pod, guest_emails}`. The
   Coordinator persists invite rows + tokens and **emails each guest** (Resend)
   with a public `/rsvp/{token}` link.
2. **RSVP:** guest opens `coordinator/rsvp/{token}` (public, no account), picks
   accept/decline. The Coordinator records it and **notifies the organizer's
   pod** (POST to the pod ingress, connection-token authed — reuse the gateway's
   push-via-k3 route) so the pod updates its local `event_guests` status. Pod can
   also poll as a fallback.
3. **Inbound (the user is a guest):** the Coordinator is the user's invite
   **mailbox** — matched by their verified email; `mcal_list_invites` /
   `mcal_respond_invite` read/write it. Accepting can place a read-only mirror on
   a local calendar.

Pod side needs: an `event_guests` table + `mcal_add_guests`/`_list_invites`/
`_respond_invite` wired to the Coordinator (currently these tools fall through to
the pack's old cloud HTTP defs), plus a coordinator-facing "RSVP updated" webhook.

---

## 5. Phasing

- **C1 — Notes cross-pod sharing** (smallest, testable): the token→pod index +
  `/p/{token}` passthrough; pod emits register/unregister on share/unshare.
- **C2 — Calendar invite outbound + RSVP page + email:** the invite table,
  Resend send, public `/rsvp/{token}`.
- **C3 — RSVP sync back to the pod** + the invite mailbox (`mcal_list_invites`).
- **C4 — Drive public links** (same shape as C1).

---

## 6. Decisions to lock before building the cloud service

These are why the Coordinator is *designed* here rather than blind-built — each
changes the implementation materially:

- **Hosting + DB:** Railway/DO + Neon (matches the ecosystem), or Cloudflare
  Workers + D1 (matches the notes-r2 direction, near-zero idle). The whole point
  is a *tiny* always-on footprint, so idle cost matters.
- **Email provider:** Resend (the cloud calendar already uses it) vs another.
  Deliverability/domain setup is real work.
- **Domain(s):** one `coordinator.metalcraftai.com` vs split
  `share.` / `rsvp.`. Affects link branding + TLS.
- **Pod addressability from the Coordinator:** confirm pods are reachable at
  `<slug>.pods.metalcraftai.com` from the Coordinator and mint it a
  connection/service token per pod (or a shared service secret like the
  gateway's `X-Metalcraft-Service-Secret`).
- **Auth for the RSVP page:** public token only (like the cloud today), and
  whether logged-in ecosystem users get an in-app inbox too.
- **Retention:** how long invite/RSVP rows live after the event; share-token
  lifetime.

---

## 7. Why this preserves the win

Even with the Coordinator, the "no always-on DB of record" goal holds: the
Coordinator's DB holds only **routing rows and invite/RSVP state**, which is tiny
and event-driven (written when someone shares or invites, not polled). All real
user data lives in pods. The Coordinator is the seam that lets single-user
backend-only pods still do the few things that are irreducibly multi-party.
