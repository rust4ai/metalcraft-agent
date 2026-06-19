#!/usr/bin/env python3
"""Trigger a fresh micro-blog-server build, poll to completion, verify /health is
publicly reachable, then tear down the sprite. Reads creds from crate-root .env."""
import urllib.request, json, time, ssl, os

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
env = {}
for line in open(os.path.join(HERE, ".env")):
    line = line.strip()
    if not line or line.startswith("#") or "=" not in line:
        continue
    k, v = line.split("=", 1)
    env[k] = v.strip().strip('"').strip("'")
BASE = env["SPRITE_BUILDER_BASE_URL"].rstrip("/")
KEY = env["SPRITE_BUILDER_API_KEY"]
PROJECT = "8e963c0a-b77b-4428-88f6-aa4361b40105"


def api(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    hdrs = {"Authorization": "Bearer " + KEY}
    if data:
        hdrs["Content-Type"] = "application/json"
    r = urllib.request.Request(BASE + path, data=data, method=method, headers=hdrs)
    try:
        with urllib.request.urlopen(r, timeout=30) as resp:
            return resp.status, resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")[:400]
    except Exception as e:
        return None, f"{type(e).__name__}: {e}"


def get_url(u):
    try:
        with urllib.request.urlopen(u, timeout=15, context=ssl.create_default_context()) as r:
            return r.status, r.read(300).decode("utf-8", "replace").replace("\n", " ")
    except urllib.error.HTTPError as e:
        return e.code, e.read(300).decode("utf-8", "replace").replace("\n", " ")
    except Exception as e:
        return None, f"{type(e).__name__}: {e}"


print("== triggering fresh build of micro-blog-server (HEAD) ==", flush=True)
s, b = api("POST", f"/api/projects/{PROJECT}/builds", {})
if s not in (200, 201):
    print("FAILED to trigger build:", s, b)
    raise SystemExit(1)
build = json.loads(b)
bid = build["id"]
print(f"build id: {bid}  status: {build['status']}", flush=True)

sprite_name = None
url = None
deadline = time.time() + 20 * 60
final = None
while time.time() < deadline:
    time.sleep(12)
    s, b = api("GET", f"/api/builds/{bid}")
    if s != 200:
        print("  poll error:", s, b)
        continue
    d = json.loads(b)
    sprite_name = d.get("sprite_name") or sprite_name
    url = d.get("url") or url
    print(f"  [{int(time.time())%100000}] status={d['status']} sprite={sprite_name}", flush=True)
    if d["status"] in ("succeeded", "failed"):
        final = d
        break

print("== build finished ==", flush=True)
if final is None:
    print("TIMED OUT after 20 min; last sprite:", sprite_name)
else:
    print("final status:", final["status"], "| url:", final.get("url"))
    if final["status"] == "succeeded" and final.get("url"):
        u = final["url"].rstrip("/")
        for path in ["/health", "/"]:
            st, body = get_url(u + path)
            print(f"  GET {path} -> {st} | {body[:160]}", flush=True)
    elif final["status"] == "failed":
        print("  error:", (final.get("error") or "")[:300])
        print("  logs tail:", (final.get("logs") or "")[-400:])

# Teardown: delete the sprite (build row will remain — no delete-build endpoint).
if sprite_name:
    st, body = api("DELETE", f"/api/admin/sprites/{sprite_name}")
    print(f"== teardown: DELETE sprite {sprite_name} -> {st} {body[:120]} ==", flush=True)
else:
    print("== teardown: no sprite to delete ==")
print(f"NOTE: build row {bid} remains (no delete-build endpoint).", flush=True)
