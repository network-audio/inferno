# Validate TX flow layouts before starting transmission

## Before

Invalid channel layouts, sample widths, packet sizes and frame counts can reach fixed transmitter buffers.

## Change

Validate new and updated flow layouts before queuing work. Check requested flow indices before using them.

## Verification

Three focused tests cover normal layouts, rejected layouts and the exact packet-size boundary.

## Limits

The current portable transmitter and real-time scheduling remain intact. Flow-request parsing is a separate PR.

Ported from the DrEVILish/inferno fork onto network-audio/inferno dev at 4aad648.
