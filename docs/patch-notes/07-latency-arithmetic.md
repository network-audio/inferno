# Avoid overflow when converting latency to samples

## Before

The intermediate latency_ns multiplied by sample_rate can overflow usize before division.

## Change

Multiply and divide using u128, then convert the final sample count to usize.

## Verification

A regression test uses an intermediate product larger than usize while the final count fits.

## Limits

This addresses intermediate overflow. It does not add a policy limit for configured or advertised latency.

Ported from the DrEVILish/inferno fork onto network-audio/inferno dev at 4aad648.
