#!/usr/bin/env python3
"""Prints timestamped add/remove events for PipeWire links and for FreeRDP and
broadcast-linux nodes. Link ids are reused, so a stream re-created in place shows
up as a remove plus an add."""
import datetime
import json
import subprocess
import time


def objects():
    found = {}
    for obj in json.loads(subprocess.run(["pw-dump"], capture_output=True, text=True).stdout):
        kind = obj["type"].rsplit(":", 1)[-1]
        info = obj.get("info") or {}
        props = info.get("props") or {}
        if kind == "Link":
            found[obj["id"]] = f"link {info.get('output-node-id')}->{info.get('input-node-id')}"
        elif kind == "Node" and (props.get("application.name") == "FreeRDP"
                                 or "broadcast" in str(props.get("node.name", ""))):
            found[obj["id"]] = f"node {props.get('node.name')} {props.get('media.class')}"
    return found


previous = {}
while True:
    current = objects()
    stamp = datetime.datetime.now().strftime("%H:%M:%S.%f")[:-3]
    for key in current.keys() - previous.keys():
        print(stamp, "+", key, current[key], flush=True)
    for key in previous.keys() - current.keys():
        print(stamp, "-", key, previous[key], flush=True)
    previous = current
    time.sleep(0.02)
