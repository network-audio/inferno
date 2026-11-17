# Announce a configurable product version

## Before

Controllers have no distinct product version when a host application needs to identify its own release.

## Change

Add PRODUCT_VERSION, populate the product-info field and preserve the crate version in the existing firmware-version field.

## Verification

Parser tests cover major.minor, major.minor.patch and malformed or out-of-range components. A loopback UDP regression checks the serialized product-info packet, including the default, an override with a two-byte patch, and preservation of all other fields.

## Limits

Invalid PRODUCT_VERSION values follow upstream SettingsError handling. The existing firmware field at content offset 0x1c remains crate-valued. The software field at 0x18 and other product-info bytes are preserved; this change does not assign new meanings to those fields.

Ported from the DrEVILish/inferno fork onto network-audio/inferno dev at 4aad648.
