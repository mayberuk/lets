# A misspelt selector names the values it saw instead of guessing.
lets transform deps/lock.json --set 'packages[name=gity].version=0.10'

# The corrected selector resolves to one entry; `0.10` stays the string "0.10" because the entry
# already held a string, and the footer names the index the selector resolved to.
lets transform deps/lock.json --set 'packages[name=gitty].version=0.10' --set version=1.5.0

cat deps/lock.json
