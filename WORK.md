# ChainMonitor update_channel InProgress override problem

## The problem

When `ChannelMonitor::update_monitor` returns `Err(())` inside
`ChainMonitor::update_channel`, the return value is overridden to
`ChannelMonitorUpdateStatus::InProgress` regardless of what the persister
returned (chainmonitor.rs:1417-1421):

```rust
if update_res.is_err() {
    ChannelMonitorUpdateStatus::InProgress
} else {
    persist_res
}
```

For a sync persister (one that only ever returns `Completed`), this creates a
permanently stuck `InProgress` entry in `ChannelManager`'s
`in_flight_monitor_updates`. Nobody ever calls `channel_monitor_updated` for it
because the persister already completed its work. The channel is frozen forever.

### How update_monitor can fail in production

The `update_monitor` inner function (channelmonitor.rs:4127+) has several
non-panic error paths reachable in release builds:

- **Post-close update rejection (line 4319-4322):** After all update steps are
  processed, if `no_further_updates_allowed()` is true (i.e.
  `funding_spend_seen || lockdown_from_offchain || holder_tx_signed`) and the
  update contains pre-close steps (commitment updates, secrets, etc.), the
  function returns `Err(())`. This is the most likely production trigger: a
  counterparty commitment tx confirms on chain (setting `funding_spend_seen`),
  then a commitment_signed message arrives before ChannelManager processes the
  chain event.
- `verify_matching_commitment_transactions` fails due to commitment tx count
  mismatch with `pending_funding` (splice-related race)
- `provide_secret` with an invalid commitment secret (only `debug_assert`, so
  returns `Err` in release)
- `renegotiated_funding` or `promote_funding` failures

### The preimage collision

Once a channel is frozen, preimage claims can still arrive. The
`claim_mpp_part` path (channelmanager.rs:9232+) generates a new
`ChannelMonitorUpdate` with a `PaymentPreimage` step and sends it through
`handle_new_monitor_update`, which calls `chain_monitor.update_channel()`.

The preimage `update_monitor` call succeeds (since `provide_payment_preimage`
is infallible). The sync persister returns `Completed`. `ChainMonitor` returns
`Completed`. But the old frozen `InProgress` update is still sitting in
`in_flight_monitor_updates`.

This triggers the per-channel assertion at channelmanager.rs:10085-10086:

```
Watch::update_channel returned Completed while prior updates are still InProgress
```

## The solution

Three coordinated changes handle the stuck InProgress case:

### 1. ChainMonitor: force-close for non-closing channels (chainmonitor.rs)

When `update_monitor` fails and the persister returned `Completed`, and the
monitor does NOT have `no_further_updates_allowed()` (i.e., no chain-initiated
close is pending), push a `HolderForceClosedWithInfo` event. This handles
splice/secret failures on channels that have no other close event pending.

When `no_further_updates_allowed()` is true, the monitor already has a pending
close event (e.g., `CommitmentTxConfirmed`) that will cause ChannelManager to
force-close the channel. No additional event is needed.

### 2. ChannelManager: drain stuck in-flight entries (channelmanager.rs:10085)

Replace the panic at the assertion point with a drain of prior stuck entries.
When `update_idx > 0` and the update completes, prior entries at indices
`0..update_idx` are stuck (the Watch returned Completed for a newer update
while older ones are still InProgress). Drain them so the pending close event
can proceed.

This also handles the restart case described in the GitHub comment: after
restart, in-flight updates are replayed sequentially starting at index 0. A
completion at index 0 with later updates still queued is expected (they haven't
been replayed yet) and does not trigger the drain.

### 3. Return InProgress to freeze ChannelManager (unchanged)

ChainMonitor still returns `InProgress` when `update_monitor` fails, which
freezes the channel in ChannelManager. This prevents the channel from sending
potentially invalid messages based on stale state.

### How it works: production scenario (funding_spend_seen)

1. Counterparty commitment tx confirms on B's ChainMonitor, setting
   `funding_spend_seen` and pushing `CommitmentTxConfirmed`.
2. A sends commitment_signed to B. `update_monitor` fails
   (`no_further_updates_allowed`). Persister returns Completed. ChainMonitor
   returns InProgress. Stuck entry at index 0 in `in_flight_monitor_updates`.
3. Since `no_further_updates_allowed()` is true, no `HolderForceClosedWithInfo`
   is pushed.
4. On next event processing, `CommitmentTxConfirmed` is picked up. The handler
   calls `locked_handle_force_close`, which generates a `ChannelForceClosed`
   update at index 1.
5. The force-close update completes (Completed). `update_idx = 1 > 0`, so the
   stuck entry at index 0 is drained. `in_flight` is now empty. Channel closes
   successfully.
6. Preimage claims go through the closed-channel path.

### How it works: splice/secret failure

1. `update_monitor` fails for a splice or secret reason. `no_further_updates_allowed()`
   is false (channel is still open). Persister returns Completed.
2. ChainMonitor pushes `HolderForceClosedWithInfo` and returns InProgress.
3. On next event processing, `HolderForceClosedWithInfo` handler force-closes
   the channel, generating a `ChannelForceClosed` update at index 1.
4. The force-close update completes. `update_idx = 1 > 0`, stuck entry drained.
   Channel closes.

### How it works: restart replay

After restart, in-flight updates are replayed via
`MonitorUpdateRegeneratedOnStartup`. They are replayed in order (index 0 first).
When the first update completes (`update_idx = 0`), the drain condition
(`update_idx > 0`) is false, so nothing is drained. Later updates are replayed
and complete normally.

## Test

`test_monitor_update_fail_after_funding_spend` in `chanmon_update_fail_tests.rs`
reproduces the production scenario and verifies the fix:

1. A-B channel with a fully committed payment that B can claim
2. A's commitment tx is confirmed on B's chain_monitor only (not ChannelManager),
   setting `funding_spend_seen = true` in the monitor
3. A sends a second payment's commitment_signed to B
4. B's monitor processes the update steps successfully, but `update_monitor`
   returns `Err(())` at line 4319 because `no_further_updates_allowed()` is true
5. ChainMonitor returns `InProgress`, creating a stuck entry
6. On the next event processing round, `CommitmentTxConfirmed` force-closes the
   channel. The force-close update completes, draining the stuck entry.
7. B claims payment 1; the preimage claim goes through the closed-channel path
   without hitting any assertion
