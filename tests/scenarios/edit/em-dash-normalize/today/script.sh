sed -n '5,7p' notes.md

sed -i 's/the agent - not the user - decides/the agent decides/' notes.md

grep -n 'decides' notes.md
