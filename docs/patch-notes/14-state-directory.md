# Allow a host to pin its saved-state directory

## Before

Saved state follows the factory device ID, which can change with network identity. A host cannot choose a stable storage path.

## Change

Add STATE_DIR and INFERNO_STATE_DIR while preserving the existing state_dir helper and default per-device path.

## Verification

The regression changes the device ID and checks that an explicitly chosen directory stays fixed.

## Limits

Empty values keep upstream's unset-value behavior and use the default directory. Use a separate directory for each device. This does not migrate existing state automatically.

Ported from the DrEVILish/inferno fork onto network-audio/inferno dev at 4aad648.
