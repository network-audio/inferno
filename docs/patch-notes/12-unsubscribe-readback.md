# Clear subscription readback before asynchronous teardown

## Before

A controller can query immediately after removal and still see the old subscription until the worker handles its command.

## Change

Clear the public subscription slot before queuing the existing worker removal command.

## Verification

The regression asserts the slot is empty while the removal command is still waiting in the worker queue.

## Limits

Flow teardown remains asynchronous. The change aligns readback with the accepted removal request.

Ported from the DrEVILish/inferno fork onto network-audio/inferno dev at 4aad648.
