grep -n '^import' usage.ts | tail -1

sed -i "2a import { total } from './total'" usage.ts

sed -n '1,4p' usage.ts
