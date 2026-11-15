# Bound remote audio formats and receive flow slots

## Before

Remote TXT values can select zero channels, unsupported encodings, excessive latency or too many receive flow slots.

## Change

Validate advertised format and FPP bounds, cap flow-table growth at MAX_FLOWS, and check multicast channel positions before use.

## Verification

The serial library run passes 162 tests, including format bounds and flow-slot allocation.

## Limits

The port preserves upstream shared multicast sockets and removed-flow disconnect commands. It does not claim arbitrary disconnect API arguments are fully validated.

Ported from the DrEVILish/inferno fork onto network-audio/inferno dev at 4aad648.
