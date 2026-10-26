#!/usr/bin/env python3
"""Read Railway env JSON from stdin, override Jito endpoint for NY, write KEY=VALUE
lines to stdout. Pipe via:
  railway variables --json | python scripts/export-env.py > env-for-vps.txt
"""
import sys, json

v = json.load(sys.stdin)
# Override Jito endpoint to NY (closer to us-east-1)
v['JITO_BLOCK_ENGINE_URL'] = 'https://ny.mainnet.block-engine.jito.wtf'
for k, val in v.items():
    print(f'{k}={val}')
