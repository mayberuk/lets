grep -n 'export function usage' usage.ts

sed -i '3i /** Returns the running total for id. */' usage.ts

sed -n '1,5p' usage.ts
