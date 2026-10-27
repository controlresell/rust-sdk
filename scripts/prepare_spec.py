#!/usr/bin/env python3
"""Turns the OpenAPI 3.1 document of the public API into the 3.0 document Progenitor reads.

  prepare_spec.py <in.json> <out.json>

Progenitor (openapiv3 crate) only models OpenAPI 3.0.x. The document served by the API is 3.1.1 and uses four 3.1
constructs, rewritten here to their 3.0 form:
  - `type: [T, "null"]`                  -> `type: T` + `nullable: true`
  - `anyOf: [X, {type: "null"}]`         -> `allOf: [X]` + `nullable: true`
and two things Progenitor cannot take:
  - the `Idempotency-Key` header parameter is dropped (the client sets it on every POST, see src/retry.rs);
  - the `default` response is dropped: Progenitor asserts one response type per operation and counts `default`
    next to the 2xx one. Every error status is also described explicitly, so nothing is lost; `4XX` and `5XX`
    ranges are added so an undocumented error status (502, 504) still reads as an API error.
Anything else 3.1-only makes the script stop instead of silently producing a wrong client.
"""
import json
import sys

UNSUPPORTED = ("const", "prefixItems", "$defs", "contentEncoding", "contentMediaType", "unevaluatedProperties", "patternProperties")


def fix(node, path=""):
    if isinstance(node, list):
        return [fix(x, f"{path}[]") for x in node]
    if not isinstance(node, dict):
        return node
    for key in UNSUPPORTED:
        if key in node and not path.endswith("/properties"):
            sys.exit(f"OpenAPI 3.1 keyword '{key}' at {path or '/'} has no 3.0 form here: extend scripts/prepare_spec.py")
    out = {k: fix(v, f"{path}/{k}") for k, v in node.items()}
    t = out.get("type")
    if isinstance(t, list):
        rest = [x for x in t if x != "null"]
        if len(rest) != len(t):
            out["nullable"] = True
        if len(rest) != 1:
            sys.exit(f"type {t} at {path} cannot be expressed in 3.0")
        out["type"] = rest[0]
    for union in ("anyOf", "oneOf"):
        members = out.get(union)
        if isinstance(members, list):
            rest = [m for m in members if m != {"type": "null"}]
            if len(rest) != len(members):
                if len(rest) != 1:
                    sys.exit(f"{union} at {path} has several members next to null: extend scripts/prepare_spec.py")
                del out[union]
                out["allOf"] = rest
                out["nullable"] = True
    return out


def main(src, dst):
    with open(src) as f:
        doc = json.load(f)
    if not str(doc.get("openapi", "")).startswith(("3.0", "3.1")):
        sys.exit(f"unexpected openapi version {doc.get('openapi')}")
    for item in doc["paths"].values():
        for operation in item.values():
            if not isinstance(operation, dict):
                continue
            if "parameters" in operation:
                operation["parameters"] = [p for p in operation["parameters"] if p.get("name") != "Idempotency-Key"]
            responses = operation["responses"]
            error = responses.pop("default", None)
            if error is not None:
                responses.setdefault("4XX", error)
                responses.setdefault("5XX", error)
    doc = fix(doc)
    doc["openapi"] = "3.0.3"
    with open(dst, "w") as f:
        json.dump(doc, f, indent=1, sort_keys=True)
        f.write("\n")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
