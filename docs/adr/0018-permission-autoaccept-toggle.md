# Permission cards carry an "开启自动授权" toggle

A user mid-task who hits a permission request most often just wants the AI to
keep going, so they type `/autoaccept on` — but that posts a separate text
message that becomes the conversation's latest message and stays there, burying
the live streaming card until the turn ends. We decided to put the switch on the
permission card itself: both the inline section of a streaming card and the
standalone permission card gain an "开启自动授权" button. Clicking it turns on
the session's Auto-Accept (`auto_accept` flag, identical to `/autoaccept on`) and
simultaneously approves the current pending permission (reusing
`approve_pending_for_session`), all without posting a new message — the pending
permission section just re-renders away and a Toast confirms it.

The toggle is session-wide and only turns the mode ON, because turning it on is
the frequent, mid-task action that was forcing an extra message. Turning it OFF
still uses `/autoaccept off` (rare, and once Auto-Accept is on no permission
cards appear to host an off-switch). The toggle is distinct from the backend's
per-type "Always Allow": "Always" is a server-side rule scoped to one permission
type that makes the backend skip asking, whereas this toggle is cola's
session-wide blanket that answers every permission with "once".
