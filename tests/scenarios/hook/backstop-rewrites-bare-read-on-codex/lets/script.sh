printf '{"session_id":"s","turn_id":"t","cwd":"%s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cat src/usage.ts"}}' "$(pwd)" | lets hook classify
