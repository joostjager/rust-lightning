# Force-close fuzzer LDK crashes

Minimized crash sequences found by the chanmon_consistency fuzzer with
force-close support. All crashes are `debug_assert` or `panic!` inside
LDK, not in the fuzzer harness. Byte 0 encodes monitor styles (bits
0-2) and channel type (bits 3-4: 0=Legacy, 1=KeyedAnchors).

## 1. channelmonitor.rs:2727 - HTLC input not found in transaction

```
debug_assert!(htlc_input_idx_opt.is_some());
```

When resolving an HTLC spend, the monitor searches for the HTLC
outpoint in the spending transaction's inputs but doesn't find it.
Falls back to index 0 in release mode, which would produce incorrect
tracking.

Minimized (17 bytes):
```
0x40 0xff 0xff 0xff 0xff 0xff 0xff 0xff 0xff 0xff 0xff 0xff 0xff 0xff 0xdc 0xde 0xff
```

Byte 0 = 0x40: Legacy channels, no async monitors. The sequence is
mostly 0xff (settlement) repeated, with height advances (0xdc, 0xde)
near the end. This suggests the crash happens during settlement when
processing on-chain HTLC spends after repeated settlement attempts.

## 2. onchaintx.rs:913 - Duplicate claim ID in pending requests

```
debug_assert!(self.pending_claim_requests.get(&claim_id).is_none());
```

The OnchainTxHandler registers a claim event with a claim_id that
already exists in the pending_claim_requests map.

Minimized (10 bytes):
```
0x08 0xd2 0x70 0x70 0x71 0x70 0x10 0x19 0xde 0xff
```

Byte 0 = 0x08: KeyedAnchors channels, no async monitors.
- 0xd2: B force-closes the A-B channel
- 0x70/0x71: disconnect/reconnect peers
- 0x10, 0x19: process messages on nodes A and B
- 0xde: advance chain 200 blocks
- 0xff: settle

B force-closes, peers disconnect and reconnect, messages are exchanged,
then height advances and settlement triggers the duplicate claim.

## 3. onchaintx.rs:1025 - Inconsistent internal maps

```
panic!("Inconsistencies between pending_claim_requests map and claimable_outpoints map");
```

The OnchainTxHandler detects that its `pending_claim_requests` and
`claimable_outpoints` maps are out of sync.

Minimized (14 bytes):
```
0x00 0x3c 0x11 0x19 0xd0 0xde 0xff 0xff 0x19 0x21 0x19 0xde 0x26 0xff
```

Byte 0 = 0x00: Legacy channels, all monitors completed.
- 0x3c: send hop payment A->B->C (1M msat)
- 0x11, 0x19: process messages to commit HTLC on A-B
- 0xd0: A force-closes A-B
- 0xde: advance 200 blocks
- 0xff: settle (first round)
- 0xff: settle again (second round, processes more messages)
- 0x19, 0x21, 0x19: continue processing B and C messages
- 0xde: advance 200 more blocks
- 0x26: process events on node C
- 0xff: settle (third round)

A hop payment partially committed, then A force-closes. Multiple
settlement rounds with continued message processing in between triggers
the internal map inconsistency.

## 4. test_channel_signer.rs:395 - Signing revoked commitment

```
panic!("can only sign the next two unrevoked commitment numbers, revoked={} vs requested={}")
```

The test channel signer is asked to sign an HTLC transaction for a
commitment number that has already been revoked.

Minimized (18 bytes):
```
0x22 0x71 0x71 0x71 0x71 0x71 0x71 0x71 0xff 0xff 0xff 0xff 0xff 0xff 0xde 0xde 0xb5 0xff
```

Byte 0 = 0x22: Legacy channels, async monitors on node B.
- 0x71: disconnect B-C peers (repeated, only first effective)
- 0xff: settle (repeated 6 times)
- 0xde 0xde: advance 400 blocks
- 0xb5: restart node B with alternate monitor state
- 0xff: settle

Async monitors on B with peer disconnection, repeated settlements,
height advances, and a node restart with a different monitor state.
The stale monitor combined with the restart puts B's signer in a state
where it's asked to sign for an already-revoked commitment.

## 5. channelmanager.rs:9836 - Payment blocker not found

```
debug_assert!(found_blocker);
```

During payment processing, the ChannelManager expects to find a
specific blocker entry for an in-flight payment but it's missing.

Minimized (13 bytes):
```
0x00 0x3c 0x11 0x19 0x11 0x1f 0x19 0x21 0x19 0x27 0x27 0xde 0xff
```

Byte 0 = 0x00: Legacy channels, all monitors completed.
- 0x3c: send hop A->B->C (1M msat)
- 0x11, 0x19, 0x11: commit HTLC on A-B
- 0x1f: B processes events (forwards HTLC to C)
- 0x19, 0x21, 0x19: commit HTLC on B-C
- 0x27, 0x27: C processes events (claims payment)
- 0xde: advance 200 blocks
- 0xff: settle

A straightforward A->B->C hop payment that completes normally (C
claims), followed by a height advance and settlement. No force-close
in this sequence, so the height advance before settlement may cause
HTLC timeout processing that conflicts with the claim path.

## 6. channelmanager.rs:19484 - Monitor update ID ordering violation

```
debug_assert!(update.update_id >= pending_update.update_id);
```

A ChannelMonitorUpdate has an update_id that is less than a pending
update's id, violating the expected monotonic ordering.

Minimized (10 bytes):
```
0x84 0x70 0x11 0x19 0x11 0x1f 0xd0 0x11 0x1f 0xba
```

Byte 0 = 0x84: Legacy channels, no async monitors, high bits set
(bits 3-4 = 0, bits 7 and 2 set).
- 0x70: disconnect A-B peers
- 0x11, 0x19, 0x11: process messages (likely reestablish after setup)
- 0x1f: process B events
- 0xd0: A force-closes A-B channel
- 0x11: process A messages
- 0x1f: process B events
- 0xba: restart node B with alternate monitor state

A force-close followed by continued message/event processing and a
node B restart triggers a monitor update with an out-of-order ID.
