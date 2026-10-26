#!/usr/bin/env python3
import re
counts = {}
with open("/var/log/arbbot.log") as f:
    for line in f:
        m = re.search(r"n_hops=(\d+)", line)
        if m:
            v = int(m.group(1))
            counts[v] = counts.get(v, 0) + 1
for k in sorted(counts):
    print("hops=%d: %d" % (k, counts[k]))
