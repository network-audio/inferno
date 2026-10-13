#!/bin/sh -e

# TMPDIR change is needed as a workaround for datagram sockets not working correctly across containers
mkdir -p /shared/tmp-$INFERNO_NAME
export TMPDIR=/shared/tmp-$INFERNO_NAME

rm -rf $HOME/.local/state/inferno_aoip || true

rm $TMPDIR/fifo || true
mkfifo $TMPDIR/fifo
inferno2pipe -c 2 -o $TMPDIR/fifo &
# inferno2pipe only outputs samples while a flow is alive, so a sample count alone never completes when the
# subscription comes late; also stop after $DURATION seconds, like the other recorders
timeout $DURATION head -c$((INFERNO_SAMPLE_RATE * 2 * 4 * (DURATION-7))) < $TMPDIR/fifo > /shared/rec-$INFERNO_NAME.raw || true

echo done > /shared/done-$INFERNO_NAME
