lets edit --from - <<'LETS'
@@ usage.ts
<<<<<<< old
const cap = 10
======= new
const cap = 20
>>>>>>>
@@ app.js
<<<<<<< old
const cap = 10
======= new
const cap = 20
>>>>>>>
@@ lib.rs insert-before #usage
======= new
/// Returns the running total for id.
>>>>>>>
LETS

lets edit --from - <<'LETS'
@@ usage.ts
<<<<<<< old
const cap = 20
======= new
const cap = 40
>>>>>>>
@@ app.js
<<<<<<< old
cap
======= new
limit
>>>>>>>
LETS

grep -n 'cap' usage.ts app.js
