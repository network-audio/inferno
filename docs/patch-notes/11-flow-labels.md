# Answer TX flow label queries with an empty page

## Before

Controllers polling ARC 0x2204 receive no reply and must wait for a timeout.

## Change

Return a successful empty page because Inferno stores no per-flow labels.

## Verification

A running-device UDP integration test checks that the query returns the two-byte empty page.

## Limits

This adds an empty response, not storage or editing of flow labels.

Ported from the DrEVILish/inferno fork onto network-audio/inferno dev at 4aad648.
