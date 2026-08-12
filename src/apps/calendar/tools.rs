//! The 9 core `mcal_*` native tools, returning the `{status, data}` envelope
//! (matching `HttpApiTool`) so the calendar contract is byte-identical.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::store::{CalendarStore, EventInput};
use super::{tz, CalError};

pub fn register(reg: metalcraft::ToolRegistry, store: CalendarStore) -> metalcraft::ToolRegistry {
    reg.register(Whoami(store.clone()))
        .register(Now) // pure clock — no store
        .register(ListCalendars(store.clone()))
        .register(CreateCalendar(store.clone()))
        .register(ListEvents(store.clone()))
        .register(GetEvent(store.clone()))
        .register(CreateEvent(store.clone()))
        .register(UpdateEvent(store.clone()))
        .register(DeleteEvent(store.clone()))
        .register(AddGuests(store.clone()))
        .register(RemoveGuest(store))
}

fn ok(status: u16, data: Value) -> Value {
    json!({ "status": status, "data": data })
}
fn err(e: CalError) -> Value {
    json!({ "status": e.status, "data": { "error": e.message } })
}
async fn ready(s: &CalendarStore) -> Result<(), Value> {
    s.ensure_ready().await.map_err(err)
}
fn sa<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(|v| v.as_str())
}

// ── mcal_whoami ──────────────────────────────────────────────────────────────
pub struct Whoami(CalendarStore);
#[async_trait]
impl metalcraft::Tool for Whoami {
    fn name(&self) -> &str { "mcal_whoami" }
    fn description(&self) -> &str {
        "Validate access and see identity/scope. Returns { sub, email, scopes } (null scopes = full-access owner). Takes no parameters."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn call(&self, _a: Value) -> metalcraft::Result<Value> {
        Ok(ok(200, self.0.whoami()))
    }
}

// ── mcal_now ─────────────────────────────────────────────────────────────────
pub struct Now;
#[async_trait]
impl metalcraft::Tool for Now {
    fn name(&self) -> &str { "mcal_now" }
    fn description(&self) -> &str {
        "Current time to ground relative dates. Returns { utc, timezone, local, date, weekday, tomorrow, yesterday } — date/tomorrow/yesterday are LOCAL dates ready to pass to mcal_list_events?day=. Pass the target calendar's IANA timezone; omit for UTC."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "tz": { "type": "string", "description": "IANA timezone to localize into (e.g. 'America/New_York'). Omit for UTC." } }
        })
    }
    async fn call(&self, a: Value) -> metalcraft::Result<Value> {
        // Pure clock — no DB, never touches storage.
        match tz::now_response(sa(&a, "tz")) {
            Ok(v) => Ok(ok(200, v)),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mcal_list_calendars ──────────────────────────────────────────────────────
pub struct ListCalendars(CalendarStore);
#[async_trait]
impl metalcraft::Tool for ListCalendars {
    fn name(&self) -> &str { "mcal_list_calendars" }
    fn description(&self) -> &str {
        "List the account's calendars. Each has id, name, slug (used to address events), timezone, is_default, created_at. The default 'personal' calendar (UTC) exists until you create tz'd ones."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn call(&self, _a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        match self.0.list_calendars().await {
            Ok(v) => Ok(ok(200, serde_json::to_value(v).unwrap_or(Value::Null))),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mcal_create_calendar ─────────────────────────────────────────────────────
pub struct CreateCalendar(CalendarStore);
#[async_trait]
impl metalcraft::Tool for CreateCalendar {
    fn name(&self) -> &str { "mcal_create_calendar" }
    fn description(&self) -> &str {
        "Create a calendar. `name` is the display name; `timezone` is a REQUIRED IANA name (e.g. 'America/New_York') — ask the user, don't guess. `slug` optional (derived from name). Returns the created calendar."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Human-readable calendar name (e.g. 'Family')." },
                "timezone": { "type": "string", "description": "REQUIRED IANA timezone, e.g. 'America/New_York'. Ask the user if unknown — do not guess." },
                "slug": { "type": "string", "description": "Optional URL-safe id; derived from name if omitted." }
            },
            "required": ["name", "timezone"]
        })
    }
    async fn call(&self, a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        match self.0.create_calendar(sa(&a, "name").unwrap_or(""), sa(&a, "timezone").unwrap_or(""), sa(&a, "slug")).await {
            Ok(v) => Ok(ok(200, serde_json::to_value(v).unwrap_or(Value::Null))),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mcal_list_events ─────────────────────────────────────────────────────────
pub struct ListEvents(CalendarStore);
#[async_trait]
impl metalcraft::Tool for ListEvents {
    fn name(&self) -> &str { "mcal_list_events" }
    fn description(&self) -> &str {
        "List a calendar's events. `calendar` is the slug. Optional `day` ('today'/'tomorrow'/'yesterday'/'YYYY-MM-DD') resolved in the calendar's timezone (preferred for day questions; overrides from/to). Optional `from`/`to` are UTC ISO-8601 bounds."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "calendar": { "type": "string", "description": "Calendar slug (from mcal_list_calendars)." },
                "day": { "type": "string", "description": "Optional single day resolved in the calendar's tz: today/tomorrow/yesterday/YYYY-MM-DD. Overrides from/to." },
                "from": { "type": "string", "description": "Optional lower bound, UTC ISO-8601." },
                "to": { "type": "string", "description": "Optional upper bound, UTC ISO-8601." }
            },
            "required": ["calendar"]
        })
    }
    async fn call(&self, a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        let Some(cal) = sa(&a, "calendar") else {
            return Ok(err(CalError::bad_request("calendar is required")));
        };
        match self.0.list_events(cal, sa(&a, "day"), sa(&a, "from"), sa(&a, "to")).await {
            Ok(v) => Ok(ok(200, serde_json::to_value(v).unwrap_or(Value::Null))),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mcal_get_event ───────────────────────────────────────────────────────────
pub struct GetEvent(CalendarStore);
#[async_trait]
impl metalcraft::Tool for GetEvent {
    fn name(&self) -> &str { "mcal_get_event" }
    fn description(&self) -> &str {
        "Get one event by calendar slug + event id."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "calendar": { "type": "string", "description": "Calendar slug." },
                "id": { "type": "string", "description": "Event id (from mcal_list_events)." }
            },
            "required": ["calendar", "id"]
        })
    }
    async fn call(&self, a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        match (sa(&a, "calendar"), sa(&a, "id")) {
            (Some(c), Some(id)) => match self.0.event_with_guests(c, id).await {
                Ok(v) => Ok(ok(200, v)),
                Err(e) => Ok(err(e)),
            },
            _ => Ok(err(CalError::bad_request("calendar and id are required"))),
        }
    }
}

/// Extract guest emails from a `guests` param (array of `{email,name?}` objects
/// or plain email strings).
fn guest_emails(a: &Value) -> Vec<String> {
    a.get("guests")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|g| {
                    g.as_str()
                        .map(String::from)
                        .or_else(|| g.get("email").and_then(|e| e.as_str()).map(String::from))
                })
                .collect()
        })
        .unwrap_or_default()
}

// ── mcal_add_guests ──────────────────────────────────────────────────────────
pub struct AddGuests(CalendarStore);
#[async_trait]
impl metalcraft::Tool for AddGuests {
    fn name(&self) -> &str { "mcal_add_guests" }
    fn description(&self) -> &str {
        "Invite external guests to an event by email. They receive an emailed RSVP link (via the coordinator); their responses appear as each guest's `rsvp` in mcal_get_event. Requires a configured coordinator. Returns the guest list."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "calendar": { "type": "string", "description": "Calendar slug." },
                "id": { "type": "string", "description": "Event id to add guests to." },
                "guests": {
                    "type": "array",
                    "description": "Guests to invite.",
                    "items": { "type": "object", "properties": {
                        "email": { "type": "string", "description": "Guest email address." },
                        "name": { "type": "string", "description": "Optional display name." }
                    }, "required": ["email"] }
                }
            },
            "required": ["calendar", "id", "guests"]
        })
    }
    async fn call(&self, a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        let (Some(c), Some(id)) = (sa(&a, "calendar"), sa(&a, "id")) else {
            return Ok(err(CalError::bad_request("calendar and id are required")));
        };
        match self.0.add_guests(c, id, &guest_emails(&a)).await {
            Ok(v) => Ok(ok(200, serde_json::to_value(v).unwrap_or(Value::Null))),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mcal_remove_guest ────────────────────────────────────────────────────────
pub struct RemoveGuest(CalendarStore);
#[async_trait]
impl metalcraft::Tool for RemoveGuest {
    fn name(&self) -> &str { "mcal_remove_guest" }
    fn description(&self) -> &str {
        "Remove a guest from an event by email (drops the local invite mirror)."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "calendar": { "type": "string", "description": "Calendar slug." },
                "id": { "type": "string", "description": "Event id." },
                "email": { "type": "string", "description": "Email of the guest to remove." }
            },
            "required": ["calendar", "id", "email"]
        })
    }
    async fn call(&self, a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        match (sa(&a, "calendar"), sa(&a, "id"), sa(&a, "email")) {
            (Some(c), Some(id), Some(email)) => match self.0.remove_guest(c, id, email).await {
                Ok(()) => Ok(ok(204, json!({ "removed": true }))),
                Err(e) => Ok(err(e)),
            },
            _ => Ok(err(CalError::bad_request("calendar, id and email are required"))),
        }
    }
}

fn event_input<'a>(a: &'a Value) -> EventInput<'a> {
    EventInput {
        title: a.get("title").and_then(|v| v.as_str()).unwrap_or(""),
        starts_at: a.get("starts_at").and_then(|v| v.as_str()).unwrap_or(""),
        ends_at: a.get("ends_at").and_then(|v| v.as_str()).unwrap_or(""),
        all_day: a.get("all_day").and_then(|v| v.as_bool()).unwrap_or(false),
        description: a.get("description").and_then(|v| v.as_str()),
        location: a.get("location").and_then(|v| v.as_str()),
    }
}

fn event_props() -> Value {
    json!({
        "title": { "type": "string", "description": "Event title." },
        "starts_at": { "type": "string", "description": "Start time, UTC ISO-8601." },
        "ends_at": { "type": "string", "description": "End time, UTC ISO-8601." },
        "all_day": { "type": "boolean", "description": "All-day flag (default false)." },
        "description": { "type": "string", "description": "Optional description / notes." },
        "location": { "type": "string", "description": "Optional location." }
    })
}

// ── mcal_create_event ────────────────────────────────────────────────────────
pub struct CreateEvent(CalendarStore);
#[async_trait]
impl metalcraft::Tool for CreateEvent {
    fn name(&self) -> &str { "mcal_create_event" }
    fn description(&self) -> &str {
        "Create an event in a calendar. `calendar` (slug), `title`, `starts_at`, `ends_at` (UTC ISO-8601) are required; `all_day`, `description`, `location` optional. Returns the created event."
    }
    fn parameters_schema(&self) -> Value {
        let mut props = event_props();
        props["calendar"] = json!({ "type": "string", "description": "Calendar slug to create the event in." });
        json!({ "type": "object", "properties": props, "required": ["calendar", "title", "starts_at", "ends_at"] })
    }
    async fn call(&self, a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        let Some(cal) = sa(&a, "calendar") else {
            return Ok(err(CalError::bad_request("calendar is required")));
        };
        match self.0.create_event(cal, event_input(&a)).await {
            Ok(v) => Ok(ok(200, serde_json::to_value(v).unwrap_or(Value::Null))),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mcal_update_event ────────────────────────────────────────────────────────
pub struct UpdateEvent(CalendarStore);
#[async_trait]
impl metalcraft::Tool for UpdateEvent {
    fn name(&self) -> &str { "mcal_update_event" }
    fn description(&self) -> &str {
        "Update an event (full replace). `calendar`, `id`, `title`, `starts_at`, `ends_at` required (resend title/times to keep them); `all_day`, `description`, `location` optional. Returns the updated event."
    }
    fn parameters_schema(&self) -> Value {
        let mut props = event_props();
        props["calendar"] = json!({ "type": "string", "description": "Calendar slug." });
        props["id"] = json!({ "type": "string", "description": "Event id to update." });
        json!({ "type": "object", "properties": props, "required": ["calendar", "id", "title", "starts_at", "ends_at"] })
    }
    async fn call(&self, a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        match (sa(&a, "calendar"), sa(&a, "id")) {
            (Some(c), Some(id)) => match self.0.update_event(c, id, event_input(&a)).await {
                Ok(v) => Ok(ok(200, serde_json::to_value(v).unwrap_or(Value::Null))),
                Err(e) => Ok(err(e)),
            },
            _ => Ok(err(CalError::bad_request("calendar and id are required"))),
        }
    }
}

// ── mcal_delete_event ────────────────────────────────────────────────────────
pub struct DeleteEvent(CalendarStore);
#[async_trait]
impl metalcraft::Tool for DeleteEvent {
    fn name(&self) -> &str { "mcal_delete_event" }
    fn description(&self) -> &str {
        "Delete an event by calendar slug + event id. Irreversible — confirm with the user first."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "calendar": { "type": "string", "description": "Calendar slug." },
                "id": { "type": "string", "description": "Event id to delete." }
            },
            "required": ["calendar", "id"]
        })
    }
    async fn call(&self, a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        match (sa(&a, "calendar"), sa(&a, "id")) {
            (Some(c), Some(id)) => match self.0.delete_event(c, id).await {
                Ok(()) => Ok(ok(204, json!({ "deleted": true }))),
                Err(e) => Ok(err(e)),
            },
            _ => Ok(err(CalError::bad_request("calendar and id are required"))),
        }
    }
}
