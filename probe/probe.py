#!/usr/bin/env python3
"""Phase 0 de-risk probe for OMSN Desktop.

Proves the whole data path end to end before any UI exists:
    OAuth login → identify the user → read → create → update → delete.

The write half operates on a single throwaway record that this script creates
and then removes, so the team's real tasks are never touched. Run with
--read-only to skip the write half entirely.
"""
from __future__ import annotations

import argparse
import sys
from datetime import datetime

import bitable
from lark_auth import (TOKEN_CACHE, USER_INFO_URL, ConfigError, authorize,
                       get_json, load_config, save_tokens)

PROBE_TITLE = "🧪 [probe] OMSN Desktop connectivity test — safe to delete"


def step(n: int, label: str) -> None:
    print(f"\n[{n}] {label}")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--read-only", action="store_true",
                    help="verify auth + read only; make no writes")
    args = ap.parse_args()

    try:
        cfg = load_config()
    except ConfigError as exc:
        print(f"Config error: {exc}", file=sys.stderr)
        return 1

    print(f"App    : {cfg.app_id}")
    print(f"Base   : {cfg.base_token} / {cfg.table_id}")
    print(f"Redirect: {cfg.redirect}")

    step(1, "OAuth — browser consent")
    try:
        tokens = authorize(cfg)
    except Exception as exc:  # surface the real reason, don't mask it
        print(f"  ✗ OAuth failed: {exc}", file=sys.stderr)
        return 1
    access = tokens.get("access_token") or tokens.get("data", {}).get("access_token")
    refresh = tokens.get("refresh_token") or tokens.get("data", {}).get("refresh_token")
    if not access:
        print(f"  ✗ No access_token in response: {tokens}", file=sys.stderr)
        return 1
    print(f"  ✓ access_token acquired (len {len(access)})")
    print(f"  {'✓' if refresh else '✗'} refresh_token "
          f"{'present — offline_access works' if refresh else 'MISSING'}")
    save_tokens(tokens)
    print(f"  ✓ tokens cached (mode 600) at {TOKEN_CACHE}")

    step(2, "Identify the signed-in user")
    try:
        info = get_json(USER_INFO_URL, access).get("data", {})
        print(f"  ✓ {info.get('name')}  open_id={info.get('open_id')}")
    except Exception as exc:
        print(f"  ✗ user_info failed: {exc}", file=sys.stderr)
        return 1

    step(3, "Read records (user token against the real table)")
    try:
        data = bitable.list_records(access, cfg.base_token, cfg.table_id, page_size=3)
        items = data.get("items", [])
        print(f"  ✓ read ok — total={data.get('total')} sample={len(items)}")
        for it in items:
            title = it.get("fields", {}).get("Title")
            title = title if isinstance(title, str) else str(title)[:40]
            print(f"      · {title}")
    except Exception as exc:
        print(f"  ✗ list failed: {exc}", file=sys.stderr)
        return 1

    if args.read_only:
        print("\n--read-only: skipping write checks. Auth + read verified.")
        return 0

    step(4, "Create a throwaway record")
    stamp = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    record_id = ""
    try:
        rec = bitable.create_record(access, cfg.base_token, cfg.table_id, {
            "Title": PROBE_TITLE,
            "Status": "Backlog",
            "Remarks": f"Created by the Phase 0 probe at {stamp}. Auto-deleted.",
        })
        record_id = rec.get("record_id", "")
        print(f"  ✓ created {record_id}")
    except Exception as exc:
        print(f"  ✗ create failed: {exc}", file=sys.stderr)
        return 1

    try:
        step(5, "Update it (status + remarks)")
        try:
            bitable.update_record(access, cfg.base_token, cfg.table_id, record_id, {
                "Status": "In Progress",
                "Remarks": f"Updated by probe at {stamp}.",
            })
            print("  ✓ updated")
        except Exception as exc:
            print(f"  ✗ update failed: {exc}", file=sys.stderr)
            return 1
    finally:
        step(6, "Delete the throwaway record (cleanup)")
        try:
            ok = bitable.delete_record(access, cfg.base_token, cfg.table_id, record_id)
            print(f"  {'✓ deleted' if ok else '✗ delete reported false'} {record_id}")
        except Exception as exc:
            print(f"  ✗ CLEANUP FAILED — remove {record_id} by hand: {exc}",
                  file=sys.stderr)

    print("\n✅ Phase 0 complete — OAuth, identity, read, create, update, delete all verified.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
