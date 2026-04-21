# Open Issues

There are no currently open `chanmon_consistency` crash families in this
branch.

Latest green reference run:

- Full corpus rerun:
  [run-1776587008 summary](/Users/joost/repo/rust-lightning-fuzz-force-close/fuzz/artifacts/chanmon_runner/run-1776587008/summary.txt)
  with `392 ok / 0 failed / 0 timed_out / 0 spawn_errors`

Recently resolved:

- Manager reload failed with `DangerousValue`.
  Fixed in
  [fuzz/src/chanmon_consistency.rs](/Users/joost/repo/rust-lightning-fuzz-force-close/fuzz/src/chanmon_consistency.rs)
  by retiring every pending monitor blob at `<= completed_update_id`
  once a later monitor update is acknowledged complete.
  This prevents restart selectors from reloading a stale older monitor
  after the serialized `ChannelManager` has already dropped the
  corresponding blocked updates.
  Targeted verification is clean in
  [run-1776585235 summary](/Users/joost/repo/rust-lightning-fuzz-force-close/fuzz/artifacts/chanmon_runner/run-1776585235/summary.txt)
  with `8 ok / 0 failed / 0 timed_out`.

- `OnchainTxHandler` could enqueue the same pending claim event twice.
  Fixed in
  [lightning/src/chain/onchaintx.rs](/Users/joost/repo/rust-lightning-fuzz-force-close/lightning/src/chain/onchaintx.rs)
  by making the initial `pending_claim_events` insertion path replace an
  existing entry with the same `ClaimId`, matching the keyed behavior
  already used in the rebroadcast, bump, and reorg paths.
  Representative repro cases are:
  [fc_duplicate_pending_claim_event_after_force_close](/Users/joost/repo/rust-lightning-fuzz-force-close/fuzz/test_cases/chanmon_consistency/fc_duplicate_pending_claim_event_after_force_close)
  with bytes `2934ff3dc0d1b6ff`, and
  [fc_duplicate_pending_claim_event_after_force_close_zero_fee_commitments](/Users/joost/repo/rust-lightning-fuzz-force-close/fuzz/test_cases/chanmon_consistency/fc_duplicate_pending_claim_event_after_force_close_zero_fee_commitments)
  with bytes `3f34ff3dc0d1b6ff`.
  Targeted verification is clean in
  [run-1776586956 summary](/Users/joost/repo/rust-lightning-fuzz-force-close/fuzz/artifacts/chanmon_runner/run-1776586956/summary.txt)
  with `2 ok / 0 failed / 0 timed_out`.
