# `inner` is a working tree of its own, so a target in the parent sandbox is outside the tree the
# command runs in.
git -c init.defaultBranch=main init --quiet inner

cd inner && lets transform ../config/app.json --set review.threads=3
