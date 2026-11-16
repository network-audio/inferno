# Drop oversized responses before packet construction

## Before

An oversized response can panic while slicing the fixed send buffer.

## Change

Check the payload against the buffer space left after the header, log the failure and keep the connection running.

## Verification

A loopback UDP regression drops an oversized response and receives the next valid response at the exact buffer limit.

## Limits

An oversized response is dropped. Valid lists still need correct pagination; this guard does not paginate them.

Ported from the DrEVILish/inferno fork onto network-audio/inferno dev at 4aad648.
