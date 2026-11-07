#!/bin/sh -e

# TMPDIR change is needed as a workaround for datagram sockets not working correctly across containers
mkdir -p /shared/tmp-$INFERNO_NAME
export TMPDIR=/shared/tmp-$INFERNO_NAME

rm -rf $HOME/.local/state/inferno_aoip || true

sox -t null null -b 16 /shared/play-$INFERNO_NAME.wav synth $DURATION whitenoise whitenoise

# the Inferno sink and source come from /etc/pipewire/pipewire.conf.d/50-inferno.conf
export INFERNO_RX_CHANNELS=6 INFERNO_TX_CHANNELS=10
/usr/bin/pipewire &
/usr/bin/wireplumber &
# pw-play and sox use the default sink and source: wait until WirePlumber has chosen them (a fixed 2 s wasn't
# always enough, and then the Inferno device never appeared)
deadline=$(($(date +%s)+30))
until wpctl inspect @DEFAULT_AUDIO_SINK@ > /dev/null 2>&1 && wpctl inspect @DEFAULT_AUDIO_SOURCE@ > /dev/null 2>&1; do
    if test $(date +%s) -ge $deadline; then
        echo "PipeWire's default sink and source didn't appear"
        wpctl status || true
        exit 1
    fi
    sleep 0.2
done

pw-play --rate=$INFERNO_SAMPLE_RATE --channels=2 --format=s16 /shared/play-$INFERNO_NAME.wav &
sox --no-dither -c 2 -b 16 -e signed-integer -r $INFERNO_SAMPLE_RATE -t alsa pipewire -b 16 /shared/rec-$INFERNO_NAME.wav trim 0 $DURATION

echo done > /shared/done-$INFERNO_NAME
