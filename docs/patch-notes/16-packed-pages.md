# Pack ARC channel pages without unused table slots

## Before

Short channel-list pages reserve unused table slots before their strings, and an entry rejected at the size limit can leave orphaned heap data.

## Change

Pack only emitted entries, shift their heap offsets, roll back rejected entry data, and limit RX pages to 16 entries while retaining 32 for TX.

## Verification

The serial library suite passes 165 tests, including short pages, full pages, packet-size cuts and RX page limits.

## Limits

The port preserves upstream listing RX channels when no receiver runs. It changes page layout, not channel IDs or the underlying subscription state.
