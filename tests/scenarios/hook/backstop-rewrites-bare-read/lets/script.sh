printf '{"session_id":"s","cwd":"%s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cat src/usage.ts"}}' "$(pwd)" | lets hook classify
# ---
lets show src/usage.ts --all --no-header --no-numbers
# ---
cat src/usage.ts
