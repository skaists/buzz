# 2026-09-13 — buzz-acp + buzz-cli canonical signing on alias roads (zCode)

**Founder report**: bFUzZ's supervisor log — every start since 2026-09-08 dies on
`wss://relay2.skaists.dev` with `Auth failed: auth-required: verification failed`
(50 failed starts over 5 days for bFUzZ alone). bAstra/bLuNa logs identical.

## Diagnosis

- Every seat runs one buzz-acp leg per workspace relay. The two ALIAS roads fail
  for every seat; the two CANONICAL roads (`skaists.buzz`, `beehivenature.buzz`)
  are clean. Hash-map proof from `agents/logs/`: `7d1ad015`=relay2.skaists.dev
  and `5762ee2a`=relay.skaists.dev carry all `verification failed` hits; the
  canonical-road logs carry zero.
- `/info` truth (fetched live): `relay2.skaists.dev` → `push.origin
  wss://beehivenature.buzz`; `relay.skaists.dev` → `wss://skaists.buzz`.
- buzz-acp's `send_auth_response` signed the NIP-42 `relay` tag with the
  TRANSPORT URL; the relay verifies against the canonical origin → rejected.
  Same defect in buzz-cli's NIP-98 `u` tags (CLI probe against relay2 returned
  `401 URL mismatch: event has https://relay2.skaists.dev/query, expected
  https://beehivenature.buzz/query`).
- This is the exact "next candidate fix" left open by the 2026-09-08 desktop
  canonical-claim incident (desktop WS/HTTP clients were fixed at 579103f;
  buzz-acp, an independent signer, was not covered).

## Fix (branch `zcode/buzz-acp-canonical-auth` from e103567)

- **buzz-acp** (78b6005): `send_auth_response` resolves the canonical WS
  identity via `GET {origin}/info` → `push.origin` (strict structural
  validation; cached per road). `HarnessRelay` stores `signing_ws_url`;
  `RestClient` gains `signing_base_url` so `bridge_post` signs
  `{canonical}{path}` while riding `{transport}{path}`. FAIL-OPEN by design:
  unreadable `/info` (e.g. the communities.buzz.xyz tenant) or absent/invalid
  `push.origin` keeps the supplied URL verbatim — no working road can regress;
  the resolved value (including fallbacks) is cached per road per process.
- **buzz-cli** (same commit): same law, REACTIVE — requests sign verbatim
  first; on a 401 the client probes `/info` once per process and the immediate
  retry signs the canonical base (`with_retry_body` + the moderation loop).
  Canonical roads and roads without `/info` never pay a probe (the eager-probe
  draft broke mock-count retry tests and would stall 5s/process on
  /info-less roads — rejected).

## Receipts

- Tests: buzz-acp **780 passed / 3 failed** — the 3 (`acp_steer_request…`,
  `goose_transport_wins…`, `keepalive_resets_idle…`) fail IDENTICALLY on
  pristine e103567 (verified by running them in the untouched buzz-src
  checkout) — pre-existing, unrelated. +7 new canonical tests, including an
  end-to-end `send_auth_response` over a real socket pair proving the AUTH
  event's `relay` tag names the advertised origin while the socket rides the
  transport.
- buzz-cli **353/0** (+6 new, incl. `alias_road_401_flips_to_canonical_signing`:
  401 → probe → retry, exactly 3 requests). clippy 0 warnings, fmt clean.
- **LIVE PROOF (344d511)**: ignored test `live_alias_road_auth_succeeds_with_
  canonical_signing` — `do_connect("wss://relay2.skaists.dev")` with a real
  seat key + NIP-OA tag: **ok in 0.83s** (pre-fix binary: "verification
  failed"). Without the tag the relay answers `restricted: not a relay member`
  — proving signature + canonical URL already verified at that point.
- **CLI LIVE**: new buzz.exe, bSpark env, `BUZZ_RELAY_URL=https://relay2.skaists.dev`
  → `channels list` returns beehivenature's channels, EXIT=0.

## Deployment (this Windows box)

- `AppData\Local\Buzz\buzz-acp.exe` + `buzz.exe` swapped (rename-safe; running
  supervisors keep the old image), originals kept as `*.bak-zcode-canonical`.
- Nest copy `~/.buzz/buzz.exe` refreshed (sandboxed codex seats execute the
  nest copy — see the bSpark CLI-PATH fix in the same session; user-AppData
  paths are denied in codex's Windows sandbox).
- **Founder gesture remaining**: restart the Buzz desktop (agent state rides
  the relay — seamless). Expected: relay2 + relay.skaists.dev legs log
  `NIP-42 authentication successful` instead of the verification-failed
  terminal error.
- Rollback: rename `.bak-zcode-canonical` back over the swapped binaries and
  restart.

## Same-session context (seat ops, zCode)

bSpark's original "buzz command not found" was TWO breaks: (1) `buzz` never on
PATH → shims at `~/.local/bin`; (2) codex sandbox denies executing
user-AppData binaries → binary copied into the nest and shims repointed;
proven inside a real codex workspace-write sandbox (exit 0). Seat env contract
verified live: `BUZZ_PRIVATE_KEY` + `BUZZ_RELAY_URL` + `BUZZ_AUTH_TAG`
(missing tag ⇒ 403 `relay_membership_required`; the tag must be passed
verbatim).
