# Trajectory view

The conversation stays a chat. The **trajectory** (轨迹) is a separate
inspector for what the agent actually did, inspired by the deepseek-harness
session log.

Open it from the timeline icon between Share and the inbox bell, or with the
`/trajectory` slash command. Escape or the modal close button returns to the
chat; the transcript is not replaced.

## What it shows

A session-level Gantt (Duration / Turns / Calls) sits above a split view:
the event list on the left, a detail inspector on the right. Close the
inspector with its × to give the list the full width; click a row or a
Gantt segment to open it again. Clicking a Gantt segment also scrolls that
event to the top of the list. The list uses colored **USER / ASSISTANT /
TOOL** badges when it is wide, and compact icons when the inspector makes
the list narrow.

Events are grouped into turns — a turn starts at each of your messages and
covers everything the agent did in response. Each row has a kind marker and a
one-line summary:

- **USER** — your message that opened the turn.
- **ASSISTANT** — one assistant reply.
- **TOOL** — one tool call, shown as `name {args} → result`. Failed calls
  are highlighted in red, and finished calls show their wall-clock duration on
  the right.
- **USAGE** — one model round: input/output/cached tokens for that round.

Click a row (or a Gantt segment) to inspect it. The inspector has four tabs:

- **Summary** — source, status, duration, and a short preview.
- **Preview** — full arguments and result for tools, or the full text for
  messages.
- **Raw** — the stored cell as JSON.
- **Source** — the original payload (message text, tool arguments, or usage
  record).

A search box at the top filters rows by their summary and detail text.

The footer line aggregates the whole session:
turns · steps | LLM time · tool time | output tokens/sec | cache hit rate |
total input/output tokens.

## Where the data comes from

The trajectory is folded from the persisted message log (full tool arguments
and results, which the live chat view truncates for display) and the persisted
UI-event stream (per-round token usage and tool durations). While a turn is
still running, the modal shows lightweight live rows with client-side
timestamps; when the turn finishes, the exact backend snapshot replaces them.

Timestamps come from the `created_at` column on `session_ui_events`; events
persisted before this column existed simply render without timing.
