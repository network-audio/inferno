# Announce a configurable product version

## Before

Controllers have no distinct product version when a host application needs to identify its own release.

## Change

Add PRODUCT_VERSION, populate the product-info field and keep the crate version in the software-version field.

## Verification

Parser tests cover major.minor, major.minor.patch and malformed or out-of-range components.

## Limits

Invalid PRODUCT_VERSION values follow upstream SettingsError handling. Other product-info fields are preserved.

Ported from the DrEVILish/inferno fork onto network-audio/inferno dev at 4aad648.
