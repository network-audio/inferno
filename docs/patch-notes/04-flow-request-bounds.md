# Parse flow requests before reading counts and offsets

## Before

Network-supplied flow counts, offsets and truncated update packets can panic the flow-control server.

## Change

Parse request and update payloads with checked lengths and offsets. Refuse out-of-range channel IDs and unavailable transmitters before acting.

## Verification

Nine flow-control tests pass, including malformed requests and mutation coverage.

## Limits

This PR handles parsing. TX layout and packet-size validation are separate. Upstream socket ownership and listener lifetime stay intact.

Ported from the DrEVILish/inferno fork onto network-audio/inferno dev at 4aad648.
