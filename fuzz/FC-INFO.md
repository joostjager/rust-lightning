# Force-Close Fuzzing Notes

This file records the current contract for `chanmon_consistency` force-close
coverage. It is intentionally short. Keep branch history and one-off debugging
notes elsewhere.

## Goal

Force-close fuzzing here should:

- exercise realistic off-chain to on-chain transitions
- keep force-close from changing the eventual outcome of claimed payments
- only allow claimed-payment sender failures when force-close dust touched a
  used payment path
- allow unclaimed HTLCs to resolve by CLTV timeout
- drive the harness far enough that it observes real terminal outcomes
- avoid manufacturing timeout wins by starving message delivery or claim
  propagation

## Hard-Mode Invariant

The current hard mode is:

- once the harness calls `claim_funds`, that HTLC must eventually produce
  `PaymentClaimed` at the receiver
- after that claim, the sender must eventually produce a terminal outcome,
  `PaymentSent` or `PaymentFailed`
- if the sender produces `PaymentFailed` for a claimed payment, some used
  force-close path for that payment must have been dust-trimmed
- force-close dust on a used path is not, by itself, enough to require
  `PaymentFailed`; the payment may still end in `PaymentSent`
- if no used force-close path for the claimed payment was dust-trimmed, the
  sender must eventually produce `PaymentSent`
- going on-chain does not create any broader exception than that dust case
- unclaimed HTLCs may still fail by CLTV expiry
- CSV waits on force-close outputs are normal and expected; they are not
  payment outcome changes
- a payment disappearing from `list_recent_payments()` is not enough, the
  harness must observe or drive the terminal outcome directly

In this mode, the following are harness failures:

- `HTLCHandlingFailed::Receive` after we already chose to claim the HTLC
- a receiver-side claim without the receiver later getting `PaymentClaimed`
- a claimed HTLC without any sender-side terminal event
- a claimed HTLC getting `PaymentFailed` without any dust-trimmed used
  force-close path
- a claimed HTLC that should fulfill resolving by CLTV timeout instead
- cleanup stopping while live balances or other pending work still show that
  more progress is possible

## Timeouts

Do not conflate CSV and CLTV:

- CSV is normal force-close settlement latency
- CLTV expiry changes the HTLC outcome

The harness should keep driving through CSV waits. It should only protect
claimed HTLCs that should still fulfill from CLTV-expiry resolution.

## Harness Rules

The main rules for preserving the invariant are:

- advance large height jumps one block at a time, with bounded draining before
  and after each block
- process queued messages and events before confirming newly broadcast
  transactions, so preimages can propagate before timeout paths win
- keep sender-side payment bookkeeping independent of
  `list_recent_payments()`
- track which channels each payment actually used, and when force-closing,
  snapshot which used payment paths become dust-blocked on the closer's
  commitment
- keep driving while `ClaimableOnChannelClose`, HTLC-related claimable balances,
  queued messages, pending monitor updates, or pending broadcasts still show
  unresolved work
- only stop before a CLTV boundary when crossing it would let a claimed HTLC
  that has not yet reached a sender terminal event expire instead
- do not hide pending-payment state behind unrelated auto-driving before an
  explicit force-close opcode; a bounded pre-close drain is acceptable when it
  is only making already-queued work visible

## Review Checklist

When changing this harness, verify:

- claimed HTLCs still require `PaymentClaimed`
- claimed HTLCs still require a sender-side terminal event
- claimed HTLCs only allow `PaymentFailed` when some used force-close path was
  dust-trimmed
- claimed HTLCs without dust-trimmed used force-close paths still require
  `PaymentSent`
- unclaimed HTLCs may still time out on-chain
- force-close opcodes still act on the currently pending state
- large synthetic height jumps do not become blind timeout buttons again
- sender-side obligations are not reconciled away through local caches

## Verification

The standard check is:

```bash
~/repo/rl-tools/run_fuzz_runner.sh --timeout-secs 20
```

Re-run the full corpus after any meaningful force-close harness change.
