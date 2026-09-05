# Failure-semantics corpus for ChatGPT-Web-as-backend transports

A repository-neutral set of cases for anything that drives an authenticated
ChatGPT Web conversation on behalf of a coding agent. Each case is specified by
**observable browser state and a required outcome**, not by API shape, so it
can be run against any implementation regardless of architecture.

The last column says what chatgpt-use does today. "fail" entries are real and
kept on purpose: a corpus that only lists passes is marketing.

Terms: *conversation id* = the `/c/<id>` segment once the first turn is
persisted; *pinned* = the implementation has recorded that id for the session.

## 1. Conversation identity

| # | Precondition (browser state) | Action | Required outcome | Forbidden outcome | chatgpt-use |
|---|---|---|---|---|---|
| 1.1 | Fresh chat, no `/c/<id>` yet | Send turn 1 | Turn lands; id recorded after persistence | Erroring because there is no id yet | pass (unpinned `None` always passes) |
| 1.2 | Pinned `/c/A`; tab now shows `/c/B` (sidebar click) | Send turn N | Steer back to `/c/A` once, then send | Sending into `/c/B` | pass (`verify_convo` → `reopen_pinned`, then fail-closed) |
| 1.3 | Pinned `/c/A`; steer-back also lands on `/c/B` | Send turn N | Hard error naming the drift | Answering from `/c/B` | pass (fails closed) |
| 1.4 | Pinned `/c/A`; the conversation was deleted server-side | Send turn N | Error, no new chat created silently | Silently starting `/c/C` and continuing | pass (reopen fails → error) |

## 2. Reconnect during generation

| # | Precondition | Action | Required outcome | Forbidden outcome | chatgpt-use |
|---|---|---|---|---|---|
| 2.1 | Pinned `/c/A`, assistant is mid-generation, tab closed by another process | Wait for the turn | Reopen `/c/A`, keep observing, return the completed reply | Reporting a bare timeout | pass (`reopen_pinned`; generation lives server-side) |
| 2.2 | No id yet (turn 1 never persisted), tab closed | Wait for the turn | Explicit "nothing to reconnect to — rerun" | Guessing a conversation | pass |
| 2.3 | Single poll fails while the page hydrates | Keep observing | No reattach on one miss | Reattaching on a single failed poll | pass (`LOST_POLLS_BEFORE_REATTACH = 3`) |

## 3. Ambiguous submit

| # | Precondition | Action | Required outcome | Forbidden outcome | chatgpt-use |
|---|---|---|---|---|---|
| 3.1 | Prompt filled; Enter pressed; page unresponsive after | Caller retries "the send" | Failure is typed *ambiguous*; caller must not resend | A generic error that reads as retryable | pass (`SubmitFailure::Ambiguous`) |
| 3.2 | Enter swallowed; composer NOT cleared; no new user turn | Fallback | Click send button once; confirm by a new user turn | Second Enter | pass |
| 3.3 | Enter landed; generation started (send button became stop) | Fallback fires anyway | Fallback must be inert | A duplicate prompt | pass (selector matches nothing once generating) |
| 3.4 | Submit already happened; tab dies before any receipt | Recovery | Reattach to `/c/A`; never re-send | `reopen_fresh` + resend | pass (`reopen_fresh` is pre-pin & pre-submit only) |

## 4. Completion observation

| # | Precondition | Action | Required outcome | Forbidden outcome | chatgpt-use |
|---|---|---|---|---|---|
| 4.1 | Enter swallowed, composer already cleared, previous turn present | Observe | Must NOT return the previous turn's text | Scraping the previous turn as this turn's answer | pass (evidence = new user turn rendered, not "composer empty") |
| 4.2 | Slow React clear after a successful submit | Observe | Must NOT count as a failed submit | Duplicate send | pass |
| 4.3 | Reply streams for minutes with no DOM change | Observe | Heartbeat, no false timeout | Timing out on silence | pass |

## 5. Cross-process contention (not in the original list; routine, not edge)

| # | Precondition | Action | Required outcome | Forbidden outcome | chatgpt-use |
|---|---|---|---|---|---|
| 5.1 | Two agent processes share one browser session | Both send | Turns serialize; neither clobbers the other's tab | Interleaved prompts in one chat | pass (turns serialized across processes) |
| 5.2 | Another process closes the browser before your turn 1 | Send turn 1 | Open a fresh chat and continue | Erroring | pass (`reopen_fresh`) |
| 5.3 | Another process closes the browser after your submit | Wait | Reattach to `/c/A` | Resend | pass |

## 6. Out of scope for chatgpt-use (kept so the corpus is honest)

| # | Case | chatgpt-use |
|---|---|---|
| 6.1 | Attachment ordering across a multi-file upload | n/a — no attachment path exists |
| 6.2 | Byte integrity of uploaded/downloaded files | n/a |
| 6.3 | Persistent message identity across transports | n/a — text in, text out; no envelope |

## Running it

Every case above is a browser-state precondition plus a required outcome. To
run one against an implementation: put the browser into the precondition by
hand (close the tab, click the sidebar, delete the conversation, press Enter
twice), trigger the action, and check the outcome against the two columns.
Cases 3.x and 4.x are the ones worth automating first — their failure modes
are silent and return plausible wrong output.
