# Metalcraft Email pack

Read-only access to a Metalcraft user's email, backed by **Metalcraft Email**
(`https://email.metalcraftai.com`) — an IMAP mailbox cache in the Metalcraft ecosystem.

## Auth
One credential: `METALCRAFT_TOKEN` (a Metalcraft ID `mck_` PAT), sent as
`Authorization: Bearer` on every tool. The same token works across every ecosystem app;
the account is implied by the token. On a managed pod the control plane injects it.

## Tools (all read-only)
| Tool | Purpose |
|------|---------|
| `memail_whoami` | Validate the token, see its scopes. |
| `memail_accounts` | List connected mailboxes + sync status. |
| `memail_list` | Recent messages, newest first (account/unread/mailbox filters, `before` paging). |
| `memail_search` | Full-text search over subject/sender/body. |
| `memail_get` | One message in full (bodies, recipients, attachment refs). |
| `memail_thread` | A whole conversation, oldest first. |

There are deliberately **no** send/reply/delete/modify tools — this integration only
reads. Attachment bytes are not retrievable (metadata only).

## Auto-enable
Tagged `metalcraft-ecosystem`, so managed pods with `ENABLE_METALCRAFT_PACKS` enable it
automatically on first boot alongside notes/calendar/contacts/drive.

See the `metalcraft-email` skill for the whoami → search/list → get/thread workflow.
