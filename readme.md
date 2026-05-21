Rust hook binary replacing Python/uv hook script

Drop-in replacement for claude-atoll-state.py that starts in <1ms vs 50-150ms for uv+Python. Connects to claude-atoll.sock with fallback to claude-island.sock for the legacy Claude Island app. All event types supported: SessionStart/End, UserPromptSubmit, PostToolUse, PermissionRequest (blocking approve/deny), Stop, SubagentStop, PreCompact, Notification.
