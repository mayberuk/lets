cat > c.py <<'PY'
v01 = 0
v02 = 0
PY

( lets edit c.py --old 'v01 = 0' --new 'v01 = 1' --check 'touch held; sleep 3' >/dev/null 2>/dev/null ) &
until [ -f held ]; do sleep 0.05; done
lets edit c.py --old 'v02 = 0' --new 'v02 = 1'
code=$?
wait
exit "$code"

lets show c.py
