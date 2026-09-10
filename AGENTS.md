# Agent rules for chatgpt-use

## Never test against the live ChatGPT

Do **not** run anything that reaches the real chatgpt.com to verify a change. That means:
- live `chatgpt-use ask / run / serve / work / resume / cancel` runs;
- probe scripts;
- `chrome-use site chatgpt/*` calls;
- opening chatgpt.com under throwaway `--session` names.

It drives the user's own signed-in account, and chatgpt.com throttles that account by **request count**. A full page load is about 45 backend requests. One afternoon of scripted live checks (roughly 23 full loads, mostly through fresh session names) tripped "Too many requests" on the account the user works in.

Verify offline instead:
- `cargo test`, which is fully offline and never calls chrome-use;
- pure functions for anything the browser decides (see `judge_record`, `classify`, `receipt::live_state`, `structured::evaluate`), tested with fixtures.

If something truly can only be confirmed live, say so and leave it marked unverified. Never run it yourself.

## Build

Heavy compiles go to the build box (`leo@192.168.0.190`) when it's reachable and not loaded. Anything that touches the user's browser cannot move there, and per the rule above it shouldn't be run at all.
