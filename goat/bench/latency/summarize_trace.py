#!/usr/bin/env python3
"""Usage: python3 ws13-summarize-trace.py FILE.stderr DIRECT|BATCH|COLD"""
import collections
import json
import math
import statistics
import sys

path, mode = sys.argv[1:]
assert mode in {'DIRECT', 'BATCH', 'COLD'}
rows = []
with open(path) as stream:
    for line in stream:
        if line.startswith('cua_goat_latency '):
            fields = dict(item.split('=', 1) for item in line.split()[1:])
            if fields['tool'] == 'click':
                rows.append(fields)
warmup = {'DIRECT': 5, 'BATCH': 15, 'COLD': 0}[mode]
rows = rows[warmup:]
expected = 150 if mode == 'BATCH' else 50
assert len(rows) == expected, f'Expected {expected} measured clicks, got {len(rows)}'
assert all(row['outcome'] == 'ok' for row in rows)
assert len({row['request_id'] for row in rows}) == len(rows)
for row in rows:
    assert int(row['measured_us']) + int(row['other_us']) == int(row['total_us'])

def summary(group):
    result = {}
    for key in group[0]:
        if key.endswith('_us') or key == 'detector_enumerations':
            divisor = 1000 if key.endswith('_us') else 1
            values = sorted(int(row[key])/divisor for row in group)
            result[key.replace('_us', '_ms')] = {
                'median': statistics.median(values),
                'p95': values[math.ceil(.95*len(values))-1],
            }
    result['exit_counts'] = dict(collections.Counter(row['detector_exit'] for row in group))
    result['tail_counts'] = dict(collections.Counter(row['tail'] for row in group))
    return result

out = {'n_clicks': len(rows), 'all': summary(rows)}
if mode == 'BATCH':
    out['position'] = {str(i+1): summary(rows[i::3]) for i in range(3)}
print(json.dumps(out, indent=2))
