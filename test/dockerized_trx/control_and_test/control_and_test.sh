#!/bin/sh -e

cd /shared

# the devices start together with this controller and run for $DURATION seconds from now
start=$(date +%s)

# Two Inferno devices on one IP (aplayrec3p + aplayrec3c in one container) need netaudio support for
# multiple devices per host: netaudio 0.4 lists and subscribes them, but reports their control address as
# ambiguous and skips their status probes. Enable with INFERNO_TEST_SAME_HOST_DEVICES=1.
routes="jack2:jack1 jack1:jack2 pipewire1:aplayrec1c aplayrec1p:aplayrec2c aplayrec1p:pipewire1 jack1:i2pipe1"
aplayrec_devices=4
aplayrec_sub_lines=6
subscribed=12
recorders="jack1 jack2 aplayrec1 aplayrec2 pipewire1 i2pipe1"
if test "$INFERNO_TEST_SAME_HOST_DEVICES" = 1; then
    echo '▶ same-host test enabled: two devices on one IP (aplayrec3p -> aplayrec3c)'
    routes="$routes aplayrec3p:aplayrec3c"
    aplayrec_devices=6
    aplayrec_sub_lines=$((aplayrec_sub_lines+2))
    subscribed=$((subscribed+2))
    recorders="$recorders aplayrec3"
else
    echo '⏸ PENDING: two devices on one IP (needs netaudio multi-device-per-host support)'
fi

# wait until all devices are discovered (one netaudio 0.4 pass takes ~15-20 s)
discovery_deadline=$((start+DURATION/2))
until netaudio device list > devlist && \
    test `grep jack < devlist | wc -l` -eq 2 && \
    test `grep aplayrec < devlist | wc -l` -eq $aplayrec_devices && \
    test `grep pipewire < devlist | wc -l` -eq 1 && \
    test `grep i2pipe < devlist | wc -l` -eq 1
do
    if test `date +%s` -ge $discovery_deadline; then
        cat devlist
        echo '❌ not all devices discovered in time'
        exit 1
    fi
    sleep 1
done


echo "✅ device list test passed ($((`date +%s`-start)) s after start)"

substart=$(date +%s)

# subscribe "TX n" -> "RX n" for all channels (netaudio bulk mode), in parallel
pids=""
for route in $routes; do
    netaudio subscription add --tx ${route%:*} --rx ${route#*:} &
    pids="$pids $!"
done
for pid in $pids; do
    wait $pid || { echo '❌ subscription add failed'; exit 1; }
done
subend=$(date +%s)

# audio can only be recorded after the subscriptions are made, so count discovery time too
signal_min_seconds=$((DURATION-8-subend+start))
if test $signal_min_seconds -lt $((DURATION/4)); then
    echo '❌ wasted too much time'
    exit 1
fi

echo "✅ after subscribe ($((subend-substart)) s, $((subend-start)) s after start)"

netaudio subscription list > sublist
test `grep jack sublist | wc -l` -eq 6
test `grep aplayrec sublist | wc -l` -eq $aplayrec_sub_lines
test `grep pipewire sublist | wc -l` -eq 4
test `grep i2pipe sublist | wc -l` -eq 2
test `grep Subscribed sublist | wc -l` -eq $subscribed

echo '✅ subscription test passed, waiting for recorded audio'


# devices finish $DURATION seconds after they started, allow a generous margin
wait_deadline=$((substart+DURATION+60))
function wait_for {
    while test -n "$1"; do
        while ! test -e "done-$1"; do
            if test `date +%s` -ge $wait_deadline; then
                echo "❌ $1 didn't finish recording in time"
                exit 1
            fi
            sleep 1
        done
        shift
    done
}

wait_for $recorders

samples_expected=$((DURATION*INFERNO_SAMPLE_RATE))
tolerance=$INFERNO_SAMPLE_RATE

echo "Minimal signal duration: $signal_min_seconds seconds"
function test_recording {
    infile="rec-$1.wav"
    echo "Testing file $infile"

    samples=$((`stat -c %s $infile`/4))
    test $samples -ge $((signal_min_seconds*INFERNO_SAMPLE_RATE))
    test $samples -le $((samples_expected+tolerance))
    echo '  ✅ recording length test passed'

    # a recording may outlast its source, and the silence after the source stopped can't match (jack-stdin
    # never plays its last period): trim trailing silence, like the leading silence is trimmed below
    sox rec-$1.wav rect-$1.wav reverse silence 1 0.1 1% reverse
    infile=rect-$1.wav
    samples=$((`stat -c %s $infile`/4))

    seconds_nonsilent="$(sox $infile recns-$1.wav silence 1 0.1 1% stat 2>&1 | sed -E 's/^Length \(seconds\):\s+([0-9]+).*$/\1/; t; d')"
    echo "  seconds non-silent: $seconds_nonsilent"
    test $seconds_nonsilent -ge $signal_min_seconds
    echo '  ✅ signal presence test passed'

    seconds_both_channels="$(sox $infile -t null null remix 2 silence 1 0.1 1% stat 2>&1 | sed -E 's/^Length \(seconds\):\s+([0-9]+).*$/\1/; t; d')"
    echo "  seconds non-silent both channels: $seconds_both_channels"
    test $seconds_both_channels -ge $signal_min_seconds
    echo '  ✅ signal presence test passed (both channels)'

    (/diff_tools/wd -align$samples_expected -ll play-$2.wav recns-$1.wav || true) | tee report-$1.txt
    grep '|MATCH' report-$1.txt
    echo '  ✅ longer channel recording matching'
    test `sed -E 's/^Compared:\s+([0-9]+).*$/\1/; t; d' < report-$1.txt` -ge $signal_min_seconds
    echo '  ✅ longer channel recording long enough'

    sox $infile recnsbc-$1.wav trim $((samples-seconds_both_channels*INFERNO_SAMPLE_RATE))s
    /diff_tools/wd -align$samples_expected -ll play-$2.wav recnsbc-$1.wav | tee reportbc-$1.txt
    echo '  ✅ shorter channel recording matching'
    test `sed -E 's/^Compared:\s+([0-9]+).*$/\1/; t; d' < reportbc-$1.txt` -ge $((seconds_both_channels*INFERNO_SAMPLE_RATE*8/10))
    echo '  ✅ shorter channel recording long enough'

    dd if=/dev/zero of=recnsbc-$1.wav bs=1 seek=100000 count=2 conv=notrunc
    ! /diff_tools/wd -align$samples_expected -ll play-$2.wav recnsbc-$1.wav
    echo '  ✅ wd tool correctly detects damaged data'

    echo "✅ rec-$1.wav all OK"
}

function raw_to_wav {
    sox -t raw -e signed-integer -b 16 -c 2 -r $INFERNO_SAMPLE_RATE /shared/$1.raw /shared/$1.wav
    rm /shared/$1.raw
}

raw_to_wav play-jack1
raw_to_wav play-jack2
raw_to_wav rec-jack1
raw_to_wav rec-jack2
sox --no-dither -t raw -e signed-integer -b 32 -c 2 -r $INFERNO_SAMPLE_RATE /shared/rec-i2pipe1.raw -b 16 /shared/rec-i2pipe1.wav
rm /shared/rec-i2pipe1.raw

# recorder:source pairs; test them all, then fail if any failed
pairs="aplayrec1:pipewire1 aplayrec2:aplayrec1 pipewire1:aplayrec1 i2pipe1:jack1 jack1:jack2 jack2:jack1"
if test "$INFERNO_TEST_SAME_HOST_DEVICES" = 1; then
    pairs="$pairs aplayrec3:aplayrec3"
fi
failed=""
set +e
for pair in $pairs; do
    ( set -e; test_recording ${pair%:*} ${pair#*:} )
    test $? -eq 0 || { echo "❌ rec-${pair%:*}.wav failed"; failed="$failed ${pair%:*}"; }
done
set -e
if test -n "$failed"; then
    echo "❌ recordings failed:$failed"
    exit 1
fi
echo '✅ all recordings OK'

rm -f /shared/rec* /shared/play*
