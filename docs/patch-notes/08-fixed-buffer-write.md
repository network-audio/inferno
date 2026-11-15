# Clamp string writes to the destination buffer

## Before

A string write beyond the destination or with an out-of-range offset panics while building a packet.

## Change

Limit copied bytes to the available destination space. Make writes at or beyond the end a no-op.

## Verification

The regression covers a short destination and offsets at the end, past the end and usize::MAX.

## Limits

This helper writes raw bytes. It does not promise UTF-8-safe truncation or change wire-offset serialization.

Ported from the DrEVILish/inferno fork onto network-audio/inferno dev at 4aad648.
