lets transform --check 'chmod 500 batch/readonly' --from - <<'JSONL'
{"file":"batch/a.json","set":{"version":"2.0.0"}}
{"file":"batch/readonly/b.json","set":{"version":"2.0.0"}}
JSONL

chmod 700 batch/readonly
