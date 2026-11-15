# Bounds-check ARC item arrays

## Before

A malformed ARC payload shorter than two bytes, or a count extending past the body, can panic the request task.

## Change

Count only complete items after the two-byte header. Treat a short header as an empty list.

## Verification

Five focused tests cover short headers, partial items, excessive counts and valid arrays.

## Limits

The helper still ignores trailing partial records, matching the previous handling of incomplete entries.

Ported from the DrEVILish/inferno fork onto network-audio/inferno dev at 4aad648.
