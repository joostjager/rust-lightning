# Recent `OnchainTxHandler` Bugs And Fixes

This note records the three `OnchainTxHandler` bugs that were fixed while
hardening the `chanmon_consistency` force-close corpus.

Both bugs lived in `lightning/src/chain/onchaintx.rs`. Both were real
logic issues, not harness-only artifacts. Both now pass in targeted
reruns and in the full `chanmon_consistency` corpus sweep.

Current green reference runs:

- Targeted duplicate-claim rerun:
  `fuzz/artifacts/chanmon_runner/run-1776537725/summary.txt`
- Targeted contentious-claim rerun:
  `fuzz/artifacts/chanmon_runner/run-1776538115/summary.txt`
- Targeted duplicate pending-claim-event rerun:
  `fuzz/artifacts/chanmon_runner/run-1776586956/summary.txt`
- Full corpus rerun:
  `fuzz/artifacts/chanmon_runner/run-1776587008/summary.txt`

Full-corpus result:

- `392 ok / 0 failed / 0 timed_out / 0 spawn_errors`

## 1. Duplicate pending claim request after force-close

### Repro cases

- `fc_duplicate_pending_claim_request_after_force_close_39b47f`
  - bytes: `0fd37373d0b2ffd3`
- `fc_duplicate_pending_claim_request_after_force_close_ed278d`
  - bytes: `08d37373d0b2ffd3`

### What went wrong

The failing shape was:

1. A force-close created two single-outpoint claim requests.
2. Those requests were merged into one delayed package because their
   timelock was still in the future.
3. A later replay of `update_claims_view_from_requests` at the same
   logical state recreated the same two single-outpoint requests.
4. The old dedupe logic only rejected a duplicate delayed claim if the
   outpoint sets were exactly equal.
5. Because the existing delayed claim had already been merged into a
   two-outpoint package, the new single-outpoint requests were not seen
   as duplicates.
6. At the timelock height, the same aggregated delayed package was
   restored twice and tried to register the same `ClaimId` twice.

The crash was the debug assertion in `OnchainTxHandler`:

- `assertion failed: self.pending_claim_requests.get(&claim_id).is_none()`

Representative evidence from
`fuzz/artifacts/chanmon_runner/run-1776537612/logs/fc_duplicate_pending_claim_request_after_force_close_ed278d.log`:

- line `1829`: `Updating claims view at height 61 with 2 claim requests`
- line `1830`: delayed until timelock `361`
- line `2077`: the same `2 claim requests` appear again
- line `17163`: delayed package restored at timelock `361`
- lines `17164` and `17167`: the same two-outpoint event is yielded twice
- line `17169`: assertion failure

The same pattern appears in the sibling repro
`fc_duplicate_pending_claim_request_after_force_close_39b47f`.

### Why the old logic was wrong

Before the fix, delayed-claim dedupe effectively asked:

- "Do I already have a delayed package with exactly the same outpoint
  set as this new request?"

That was too strict. Once two single-outpoint requests had already been
merged into one delayed package, replaying either single-outpoint
request should have been considered duplicate as well.

The correct question is:

- "Is every outpoint in this new request already covered by an existing
  delayed package?"

### The fix

In `OnchainTxHandler::update_claims_view_from_requests`, the delayed
claim dedupe was changed from exact package equality to covering-package
detection.

Relevant code:

- `lightning/src/chain/onchaintx.rs`, `timelocked_covering_package`
- log line for this path:
  `Ignoring second claim for outpoint ..., we already have one which
  we're waiting on a timelock at ...`

In practical terms:

- a fresh single-outpoint request is now ignored if a delayed package
  already contains that outpoint
- replaying the same logical claim state no longer creates duplicate
  delayed packages
- the delayed package is restored only once at the timelock height

### Why this fix is correct

This does not suppress any legitimate new claim. It only rejects a
request whose entire outpoint set is already represented in pending
delayed state. If a request introduces a truly new outpoint, it still
passes through.

### Verification

Targeted rerun:

- `fuzz/artifacts/chanmon_runner/run-1776537725/summary.txt`
- result: `2 ok / 0 failed / 0 timed_out`

## 2. Contentious claim reused an already resolved outpoint

### Repro cases

- `fc_contentious_claim_stuck_after_force_close_218996`
  - bytes: `89ffde3d3dc0d3ff`
- `fc_contentious_claim_stuck_after_force_close_36a22e`
  - bytes: `2cffde3d3dc0d3ff`
- `fc_contentious_claim_stuck_after_force_close_d7793e`
  - bytes: `76ffde3d3dc0d1ff`

### What went wrong

The failing shape was:

1. An HTLC output was claimed on-chain by a single-outpoint claim.
2. That claim matured past `ANTI_REORG_DELAY`.
3. `OnchainTxHandler` removed the pending claim tracking for that
   outpoint.
4. A later preimage update arrived and built a fresh two-outpoint claim
   that included the already-resolved outpoint again.
5. That new claim could never confirm, because one of its inputs had
   already been definitively spent.
6. The handler kept RBF-bumping that impossible claim forever, leaving a
   claimed payment stuck pending in the harness.

Representative evidence from
`fuzz/artifacts/chanmon_runner/run-1776537816/logs/fc_contentious_claim_stuck_after_force_close_d7793e.log`:

- line `3173`: `Updating claims view at height 60 with 1 claim requests`
- line `3175`: registers claim for
  `cc0e...:2`
- line `3282`: removes tracking for `cc0e...:2` after the claim package
  matured
- line `3424`: `Updating claims view at height 66 with 2 claim requests`
- line `3425`: yields a new event spending
  `cc0e...:1` and `cc0e...:2`
- lines `3426` and `3427`: registers both outpoints again
- lines `4438`, `5380`, `6322`, and many later lines: keeps yielding
  RBF-bumped events for that same impossible two-input claim
- line `21640`: final harness failure,
  `Node 2 has 1 stuck pending payments after settling all state`

The same family reproduced in the other two named cases.

### Why the old logic was wrong

Removing an outpoint from `claimable_outpoints` after its claim matured
was not enough. That only said:

- "we no longer need to actively track this pending request"

It did not preserve the stronger fact:

- "this outpoint is definitively spent and must never be re-claimed"

Without that second fact, a later preimage could cause
`update_claims_view_from_requests` to resurrect an already-resolved
outpoint into a new claim package.

### The fix

`OnchainTxHandler` now maintains a restart-safe
`irrevocably_spent_outpoints: HashSet<BitcoinOutPoint>`.

Relevant code paths:

- field definition:
  `lightning/src/chain/onchaintx.rs`
- serialization and deserialization:
  the new optional TLV field in `write` and `read`
- request filtering:
  `Ignoring claim for outpoint ..., it was already irrevocably spent by
  a confirmed claim transaction`
- maturation handling:
  outpoints are inserted into `irrevocably_spent_outpoints` when a claim
  or contentious outpoint reaches the anti-reorg threshold

This matters for restarts as well. The spent-outpoint memory is part of
the serialized `OnchainTxHandler` state, so a monitor reload does not
forget that the output was already definitively resolved.

### Why this fix is correct

Once a claim tx for an outpoint has reached `ANTI_REORG_DELAY`, the
handler should never generate a new claim for that same outpoint unless
the chain reorgs deep enough to invalidate the confirmation. That is
exactly the invariant the new set captures.

The fix is intentionally narrow:

- it does not suppress still-live outpoints
- it does not interfere with normal package splitting or merging
- it only blocks claim generation for outpoints that were already
  irreversibly resolved

### Verification

Targeted rerun:

- `fuzz/artifacts/chanmon_runner/run-1776538115/summary.txt`
- result: `3 ok / 0 failed / 0 timed_out`

## 3. Duplicate pending claim event after force-close

### Repro cases

- `fc_duplicate_pending_claim_event_after_force_close`
  - bytes: `2934ff3dc0d1b6ff`
- `fc_duplicate_pending_claim_event_after_force_close_zero_fee_commitments`
  - bytes: `3f34ff3dc0d1b6ff`

### What went wrong

The failing shape was:

1. A force-close path yielded an `OnchainClaim::Event`.
2. `OnchainTxHandler` inserted that event into `pending_claim_events`
   under its `ClaimId`.
3. Before the original pending event was drained, the same logical claim
   was rebuilt and yielded again.
4. The initial insertion path still assumed that duplicate `ClaimId`
   entries could never happen there.
5. That path pushed a second entry with the same `ClaimId` and hit the
   debug assertion that the count had to be zero.

The crash was the debug assertion in `OnchainTxHandler`:

- `debug_assert_eq!(self.pending_claim_events.iter().filter(|entry| entry.0 == claim_id).count(), 0);`

Representative evidence from
`fuzz/artifacts/chanmon_runner/run-1776584834/logs/crash-4b5e6aabf5bc0467bcd2163cced7d60241d24f17.log`:

- line `3544`: yields an on-chain event spending the commitment output
- line `3545`: registers the associated claim request
- line `3679`: later rebuilds claims view with one fresh claim request
- line `3680`: yields another on-chain event for HTLC output
  `513872...:2`
- line `3681`: assertion failure while inserting the second event with
  the same `ClaimId`

The sibling zero-fee-commitments repro follows the same shape in
`crash-a83289388ca2b4f52279218f3a70e0f1f0661a92.log`, with the same
panic at `onchaintx.rs:944`.

### Why the old logic was wrong

`pending_claim_events` was already being treated like a keyed queue in
other parts of `OnchainTxHandler`:

- rebroadcast logic replaced existing entries by `ClaimId`
- bump logic replaced existing entries by `ClaimId`
- reorg logic replaced existing entries by `ClaimId`

Only the initial insertion path still assumed uniqueness and pushed
blindly. That left the structure with inconsistent semantics depending
on which path happened to enqueue the event.

The correct invariant is:

- there is at most one pending event per `ClaimId`
- re-enqueuing the same logical claim should replace the older entry,
  not panic

### The fix

In `OnchainTxHandler::update_claims_view_from_requests`, the initial
`OnchainClaim::Event` insertion now matches the other paths:

- under debug builds it asserts the existing count is `0` or `1`
- it removes any existing `pending_claim_events` entry for that
  `ClaimId`
- it then pushes the newest event

This preserves insertion order for distinct claim ids while making
duplicate requeues idempotent.

### Why this fix is correct

This does not hide a real conflict between distinct claims. Two
different claim packages should not share a `ClaimId`. If they do, they
represent the same logical event as far as the queue is concerned, and
the newest version should replace the old one.

This also makes the queue semantics internally consistent. Every path
that mutates `pending_claim_events` now treats it as keyed by
`ClaimId`, rather than having one path act like a multimap.

### Verification

Targeted rerun:

- `fuzz/artifacts/chanmon_runner/run-1776586956/summary.txt`
- result: `2 ok / 0 failed / 0 timed_out`

## Final verification

After all three fixes landed, the default corpus sweep passed:

- `fuzz/artifacts/chanmon_runner/run-1776587008/summary.txt`
- result: `392 ok / 0 failed / 0 timed_out / 0 spawn_errors`

This is the reference run showing that:

- the duplicate delayed-claim family is fixed
- the contentious reused-outpoint family is fixed
- the duplicate pending-claim-event family is fixed
- neither change regressed the previously fixed dust, restart, or
  sender-terminal-event invariants
