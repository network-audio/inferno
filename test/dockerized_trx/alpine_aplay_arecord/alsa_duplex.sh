#!/bin/sh -e

# TMPDIR change is needed as a workaround for datagram sockets not working correctly across containers
mkdir -p /shared/tmp-$INFERNO_NAME
export TMPDIR=/shared/tmp-$INFERNO_NAME

rm -rf $HOME/.local/state/inferno_aoip || true

sox -t null null -b 16 /shared/play-$INFERNO_NAME.wav synth $DURATION whitenoise whitenoise
sox /shared/play-$INFERNO_NAME.wav -e signed-integer -b 32 /shared/play-native-$INFERNO_NAME.wav

# Two Inferno devices run in this container: ${INFERNO_NAME}p (aplay) and ${INFERNO_NAME}c (arecord),
# distinguished by PROCESS_ID and ALT_PORT. If CAPTURE_IP (address/prefix) is set, it is added to eth0
# and the capture device binds to it, so each device has its own IP. Without it, both devices share
# the container's IP (netaudio 0.4 doesn't fully support that yet, see README).
PLAYBACK_IP=$(hostname -i)
if [ -n "$CAPTURE_IP" ]; then
    ip addr add "$CAPTURE_IP" dev eth0
    CAPTURE_BIND_IP=${CAPTURE_IP%/*}
else
    CAPTURE_BIND_IP=$PLAYBACK_IP
fi
echo "${INFERNO_NAME}p on $PLAYBACK_IP, ${INFERNO_NAME}c on $CAPTURE_BIND_IP"

INFERNO_BIND_IP=$PLAYBACK_IP INFERNO_PROCESS_ID=1 INFERNO_NAME=${INFERNO_NAME}p INFERNO_ALT_PORT=10100 aplay -D inferno /shared/play-native-$INFERNO_NAME.wav &
INFERNO_BIND_IP=$CAPTURE_BIND_IP INFERNO_PROCESS_ID=2 INFERNO_NAME=${INFERNO_NAME}c INFERNO_ALT_PORT=10200 arecord -D inferno -d $DURATION -c 2 -r $INFERNO_SAMPLE_RATE -f S32_LE /shared/rec-native-$INFERNO_NAME.wav

sox --no-dither /shared/rec-native-$INFERNO_NAME.wav -e signed-integer -b 16 /shared/rec-$INFERNO_NAME.wav

rm /shared/rec-native-$INFERNO_NAME.wav /shared/play-native-$INFERNO_NAME.wav

echo done > /shared/done-$INFERNO_NAME
