#!/bin/sh -e

# TMPDIR change is needed as a workaround for datagram sockets not working correctly across containers
mkdir -p /shared/tmp-$INFERNO_NAME
export TMPDIR=/shared/tmp-$INFERNO_NAME

rm -rf $HOME/.local/state/inferno_aoip || true

rm $TMPDIR/fifo || true
mkfifo $TMPDIR/fifo
inferno2pipe -c 2 -o $TMPDIR/fifo &
# inferno2pipe records along the media clock, silence before the subscription. It stops after $DURATION seconds,
# like the other recorders (at most $DURATION seconds of audio, also without a clock); stopping earlier left it
# less signal than the controller requires once discovery is quick
timeout $DURATION head -c$((INFERNO_SAMPLE_RATE * 2 * 4 * DURATION)) < $TMPDIR/fifo > /shared/rec-$INFERNO_NAME.raw || true

echo done > /shared/done-$INFERNO_NAME
