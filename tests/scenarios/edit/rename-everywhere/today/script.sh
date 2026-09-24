sed -i 's/usageCap/usageLimit/g' usage.ts

grep -n 'usageLimit' usage.ts
