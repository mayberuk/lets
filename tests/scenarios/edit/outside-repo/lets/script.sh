# `inner` is a working tree of its own, so a target in the parent is outside the tree the
# command runs in — the sandbox's HOME lives under that parent and is not outside it. The branch
# name is pinned because git only suppresses its default-branch hint when that config is set.
git -c init.defaultBranch=main init --quiet inner

cd inner && lets edit ../usage.ts --old 'const cap = 10' --new 'const cap = 20'

grep -n 'const cap' usage.ts

cd inner && lets edit ../usage.ts --old 'const cap = 10' --new 'const cap = 20' --allow-outside
