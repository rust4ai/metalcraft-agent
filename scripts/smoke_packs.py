#!/usr/bin/env python3
"""Read-only smoke test for the Railway and Render integration packs.

Loads each tool's real URL / GraphQL query straight from its
`seed/integration_packs/<pack>/api_tools/*.json` file and runs it against the
live API, so a query that drifts from what the provider actually accepts (the
class of bug where railway_list_projects silently returned 0 projects) fails
here instead of in production.

Only READ tools are exercised — nothing is created, redeployed, or verified.

Usage:
    RAILWAY_API_TOKEN=... RENDER_API_KEY=... python3 scripts/smoke_packs.py
    RAILWAY_API_TOKEN=...                    python3 scripts/smoke_packs.py  # railway only
    RENDER_API_KEY=...                       python3 scripts/smoke_packs.py  # render only

A pack section is skipped when its token env var is unset. Exit code is
non-zero if any executed check fails, so this is CI/pre-release friendly.
"""
import json
import os
import re
import sys
import urllib.request
import urllib.error

ROOT = os.path.join(os.path.dirname(__file__), os.pardir)
PACKS = os.path.join(ROOT, "seed", "integration_packs")

# ── tiny output helpers ──────────────────────────────────────────────────────
PASS, FAIL = 0, 0


def ok(msg):
    global PASS
    PASS += 1
    print(f"  \033[32mPASS\033[0m {msg}")


def bad(msg):
    global FAIL
    FAIL += 1
    print(f"  \033[31mFAIL\033[0m {msg}")


def tool(pack, name):
    """Load a tool's JSON definition from the pack it ships in."""
    with open(os.path.join(PACKS, pack, "api_tools", f"{name}.json")) as f:
        return json.load(f)


def http(method, url, headers, body=None):
    data = json.dumps(body).encode() if body is not None else None
    # Railway's edge 403s the default Python-urllib User-Agent, so set an
    # explicit one. (The agent's real HTTP client sends its own UA and is
    # unaffected — this is a quirk of the test harness, not the packs.)
    headers = {"User-Agent": "metalcraft-smoke/1.0", **headers}
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read().decode())
        except Exception:
            return e.code, None


def fill_url(url, params):
    """Mirror http_api.rs: substitute provided {params}, then strip any query
    segment that still contains an unfilled {placeholder}."""
    for k, v in params.items():
        if v not in (None, ""):
            url = url.replace("{" + k + "}", str(v))
    if "?" in url:
        base, query = url.split("?", 1)
        kept = [seg for seg in query.split("&") if "{" not in seg]
        url = base + ("?" + "&".join(kept) if kept else "")
    return url


# ── Railway (GraphQL) ────────────────────────────────────────────────────────
def gql(token, query, variables=None):
    body = {"query": query}
    if variables:
        body["variables"] = variables
    status, data = http(
        "POST",
        "https://backboard.railway.com/graphql/v2",
        {"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
        body,
    )
    errs = (data or {}).get("errors")
    return status, data, errs


def smoke_railway(token):
    print("\n\033[1mRailway\033[0m")

    # whoami
    q = tool("railway", "railway_whoami")["body_defaults"]["query"]
    st, data, errs = gql(token, q)
    if st == 200 and not errs and data["data"]["me"].get("email"):
        ok(f"whoami -> {data['data']['me']['email']}")
    else:
        bad(f"whoami (status={st}, errors={errs})")
        return  # nothing else will work without a valid token

    # list_projects — the exact query the tool ships
    q = tool("railway", "railway_list_projects")["body_defaults"]["query"]
    st, data, errs = gql(token, q)
    projects = []
    if st == 200 and not errs:
        for ws in data["data"]["me"]["workspaces"]:
            for e in ws["projects"]["edges"]:
                projects.append(e["node"])
        if projects:
            ok(f"list_projects -> {len(projects)} project(s) across "
               f"{len(data['data']['me']['workspaces'])} workspace(s)")
        else:
            bad("list_projects returned 0 projects — query may have drifted, "
                "or this token's workspaces are empty")
    else:
        bad(f"list_projects (status={st}, errors={errs})")

    if not projects:
        return
    pid = projects[0]["id"]

    # get_project -> services + environments
    t = tool("railway", "railway_get_project")
    st, data, errs = gql(token, t["body_defaults"]["query"], {"id": pid})
    if st == 200 and not errs and data["data"].get("project"):
        p = data["data"]["project"]
        svcs = [e["node"] for e in p["services"]["edges"]]
        envs = [e["node"] for e in p["environments"]["edges"]]
        ok(f"get_project '{p['name']}' -> {len(svcs)} service(s), {len(envs)} env(s)")
        if svcs and envs:
            sid, eid = svcs[0]["id"], envs[0]["id"]
            # deployments
            t = tool("railway", "railway_list_deployments")
            st, data, errs = gql(token, t["body_defaults"]["query"],
                                 {"input": {"serviceId": sid, "environmentId": eid, "projectId": pid}})
            (ok if st == 200 and not errs else bad)(f"list_deployments (status={st}, errors={errs})")
            # variables
            t = tool("railway", "railway_list_variables")
            st, data, errs = gql(token, t["body_defaults"]["query"],
                                 {"projectId": pid, "environmentId": eid, "serviceId": sid})
            (ok if st == 200 and not errs else bad)(f"list_variables (status={st}, errors={errs})")
            # domains
            t = tool("railway", "railway_list_domains")
            st, data, errs = gql(token, t["body_defaults"]["query"],
                                 {"projectId": pid, "environmentId": eid, "serviceId": sid})
            (ok if st == 200 and not errs else bad)(f"list_domains (status={st}, errors={errs})")
        else:
            print("  (project has no service/env to chain deployment checks)")
    else:
        bad(f"get_project (status={st}, errors={errs})")


# ── Render (REST) ────────────────────────────────────────────────────────────
def smoke_render(key):
    print("\n\033[1mRender\033[0m")
    headers = {"Authorization": f"Bearer {key}", "Accept": "application/json"}

    # list_owners
    url = tool("render", "render_list_owners")["url"]
    st, data = http("GET", url, headers)
    if st == 200 and isinstance(data, list) and data:
        oid = data[0]["owner"]["id"]
        ok(f"list_owners -> {len(data)} workspace(s), first '{data[0]['owner']['name']}'")
    else:
        bad(f"list_owners (status={st})")
        return

    # list_services
    url = fill_url(tool("render", "render_list_services")["url"], {"ownerId": oid, "limit": 100})
    st, data = http("GET", url, headers)
    if st == 200 and isinstance(data, list):
        ok(f"list_services -> {len(data)} service(s)")
    else:
        bad(f"list_services (status={st})")
        return

    if not data:
        print("  (workspace has no services — custom-domain tools not reachable to test)")
        return
    sid = data[0]["service"]["id"]

    # list_custom_domains on the first service
    url = fill_url(tool("render", "render_list_custom_domains")["url"], {"serviceId": sid})
    st, data = http("GET", url, headers)
    (ok if st == 200 else bad)(f"list_custom_domains on {sid} (status={st})")


# ── main ─────────────────────────────────────────────────────────────────────
def main():
    rw = os.environ.get("RAILWAY_API_TOKEN")
    rn = os.environ.get("RENDER_API_KEY")
    if not rw and not rn:
        print(
            "Set RAILWAY_API_TOKEN and/or RENDER_API_KEY to run.",
            file=sys.stderr,
        )
        return 2
    if rw:
        smoke_railway(rw)
    else:
        print("\n\033[1mRailway\033[0m\n  skipped (RAILWAY_API_TOKEN unset)")
    if rn:
        smoke_render(rn)
    else:
        print("\n\033[1mRender\033[0m\n  skipped (RENDER_API_KEY unset)")

    print(f"\n{PASS} passed, {FAIL} failed")
    return 1 if FAIL else 0


if __name__ == "__main__":
    sys.exit(main())
