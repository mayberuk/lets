cat > c.py <<'PY'
v01 = 0
v02 = 0
v03 = 0
v04 = 0
v05 = 0
v06 = 0
v07 = 0
v08 = 0
v09 = 0
v10 = 0
v11 = 0
v12 = 0
v13 = 0
v14 = 0
v15 = 0
v16 = 0
v17 = 0
v18 = 0
v19 = 0
v20 = 0
PY

: > codes
for n in $(seq -w 1 20); do
  ( lets edit c.py --old "v$n = 0" --new "v$n = 1" >/dev/null 2>/dev/null; echo $? >> codes ) &
done
wait
sort codes | uniq -c | awk '{print $1, $2}'

lets show c.py
