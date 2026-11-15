# Remove every channel in a bulk unsubscribe request

## Before

ARC 0x3014 reads only the first channel and indexes short requests without checking their length.

## Change

Parse the complete ID list, filter local channel indices and queue removal for every valid channel.

## Verification

The focused parser regression covers multiple channels, a single channel, short headers and exaggerated counts.

## Limits

The newer upstream receive-buffer teardown remains intact. A separate PR fixes immediate subscription readback.

Ported from the DrEVILish/inferno fork onto network-audio/inferno dev at 4aad648.
