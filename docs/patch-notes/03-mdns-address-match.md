# Ignore unrelated answers during mDNS address checks

## Before

An mDNS response containing only other names makes an unused multicast address appear occupied.

## Change

Return true only when an answer matches the queried DNS name, without case sensitivity.

## Verification

The regression test covers no answers, an unrelated answer and a case-insensitive match.

## Limits

This preserves existing timeout and error handling. It does not change A-record type filtering.

Ported from the DrEVILish/inferno fork onto network-audio/inferno dev at 4aad648.
