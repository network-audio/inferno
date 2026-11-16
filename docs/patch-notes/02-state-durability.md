# Write and sync saved state before renaming

## Before

A successful write can be partial. Renaming an unsynced temporary file can expose empty or truncated state after power loss.

## Change

Use write_all and sync_all before the atomic rename. Sync the parent directory afterward on a best-effort basis.

## Verification

Five state tests pass, including a long-document round trip and atomic-rename checks.

## Limits

Tests check file contents and replacement behavior. They do not simulate a power cut; directory sync remains best effort.

Ported from the DrEVILish/inferno fork onto network-audio/inferno dev at 4aad648.
