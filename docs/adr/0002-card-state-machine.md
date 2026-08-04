# Card state machine with inline errors

We compared two approaches for handling step failures in the Feishu message card: a dedicated `[error]` state that replaces the card content, or keeping errors inline within the `[done]` state.

## Decision

Errors are appended inline to the `[done]` state text. There is no separate error state in the card state machine.

## Why

Adding an `[error]` state requires handling transitions from every other state (loading, reasoning, streaming, any mid-tool-flight), which significantly increases the state machine's complexity. More importantly, replacing the card with a dedicated error view discards the context the user needs to understand what went wrong — the streaming text, tool outputs, and reasoning that led to the error.

An inline error preserves the full execution trail: the user sees what the AI said and did before the failure. On mobile Feishu where reading back through context is essential, this is the better UX.
