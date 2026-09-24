printf '{"session_id":"s","cwd":"%s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cat config/app.json | wc -l"}}' "$(pwd)" | lets hook classify
