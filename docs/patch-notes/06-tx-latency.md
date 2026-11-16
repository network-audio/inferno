# Advertise TX latency in channel and bundle mDNS records

## Before

Transmit advertisements report RX_LATENCY_NS, so changing receive latency also changes what receivers are told to buffer.

## Change

Pass TX_LATENCY_NS to the responder and use it in both channel and multicast-bundle TXT records.

## Verification

A live loopback mDNS test uses 10 ms RX and 1 ms TX, then reads 1 ms from both advertisements.

## Limits

Defaults remain 10 ms. Choosing a lower transmit latency requires a host and network that can meet that deadline.

Ported from the DrEVILish/inferno fork onto network-audio/inferno dev at 4aad648.
