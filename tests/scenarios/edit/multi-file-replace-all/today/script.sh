sed -i 's/const cap = 10/const cap = 20/g' usage.ts app.js

grep -n 'cap' usage.ts app.js
