printf "{\"session_id\":\"s\",\"cwd\":\"%s\",\"hook_event_name\":\"PreToolUse\",\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"sed -i 's/line/row/g' src/a.ts notes.md\"}}" "$(pwd)" | lets hook classify

lets edit src/a.ts notes.md --old 'line' --new 'row' --all

cat src/a.ts notes.md
