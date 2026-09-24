git -c init.defaultBranch=main init --quiet inner

cd inner && sed -i 's/const cap = 10/const cap = 20/' ../usage.ts

grep -n 'const cap' usage.ts
