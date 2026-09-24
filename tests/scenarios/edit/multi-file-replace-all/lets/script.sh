lets edit usage.ts app.js --old 'const cap = 10' --new 'const cap = 20' --all

grep -n 'cap' usage.ts app.js
