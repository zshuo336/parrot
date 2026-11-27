#!/bin/busybox sh
# SPDX-License-Identifier: AGPL-3.0-only
set -eu
export PATH=/bin
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
exec </dev/console >/dev/console 2>&1
mount -t ext4 /dev/vda /runlab
umask 077
export PARROT_HANDOFF_RUN=/runlab PARROT_HANDOFF_BIND=0.0.0.0:8317
ip link set lo up
ip link set eth0 up
ip addr add 10.0.2.15/24 dev eth0
echo "PARROT_BOOT $(cat /proc/sys/kernel/random/boot_id)"
mkdir /tmp/health
cp /proc/sys/kernel/random/boot_id /tmp/health/boot
httpd -p 8318 -h /tmp/health

old_receiver() {
    /receiver --ignored --exact receiver_handoff::original_host --nocapture
}
external() {
    grep -qw parrot_external /proc/cmdline
}
if [ -f /runlab/enrolled ]; then
    test ! -s /runlab/proof
    if old_receiver >/tmp/rejoin 2>&1; then exit 1; fi
    grep -Fq 'receiver sealed for handoff' /tmp/rejoin
    echo PARROT_REFUSED_OLD_RECEIVER
    mv /runlab/state/ordered.state /runlab/saved.state
    if /handoff prepare /runlab/state recovery-contract recovery-target >/tmp/invite 2>/tmp/reason; then exit 1; fi
    test ! -s /tmp/invite
    grep -Fq 'state is missing' /tmp/reason
    test ! -e /runlab/state/ordered.state
    mv /runlab/saved.state /runlab/state/ordered.state
    echo PARROT_REFUSED_MISSING_STATE
    if external; then
        sync
        poweroff -f
    fi
    if /handoff witness /runlab/witness </runlab/invitation >/tmp/proof 2>/tmp/reason; then exit 1; fi
    test ! -s /tmp/proof
    grep -Fq 'original boot' /tmp/reason
    echo PARROT_REFUSED_OLD_BOOT
    # The original volume has become available again. Only release the seal
    # over the separate administration serial port after all checks succeed.
    stty -F /dev/ttyS1 raw -echo
    /handoff seal /runlab/state recovery-contract recovery-target </runlab/invitation >/dev/ttyS1
    echo PARROT_OFFLINE_SEAL
    sync
    poweroff -f
fi

mkdir -m 700 /runlab/state /runlab/witness
cp /tls/* /runlab/
export PARROT_HANDOFF_DISCOVER=1
old_receiver >/runlab/original.log 2>&1 &
owner=$!
until test -s /runlab/receiver-id; do kill -0 "$owner" || { cat /runlab/original.log; exit 1; }; sleep .05; done
stty -F /dev/ttyS1 raw -echo
echo "PARROT_SOURCE $(cat /runlab/receiver-id)"
IFS= read -r invitation </dev/ttyS1
printf '%s\n' "$invitation" >/runlab/invitation
digest=$(sha256sum /runlab/invitation | cut -d' ' -f1)
if external; then
    /handoff enroll-guest /dev/ttyS1 </runlab/invitation >/runlab/proof 2>/runlab/witness.log &
else
    /handoff witness /runlab/witness </runlab/invitation >/runlab/proof 2>/runlab/witness.log &
fi
witness=$!
until grep -Fq "parrot.standby.$digest" /proc/net/unix; do
    kill -0 "$witness" || { cat /runlab/witness.log; exit 1; }
    sleep .05
done
echo PARROT_LISTENING
echo "$digest" >/runlab/invite-digest.next
mv /runlab/invite-digest.next /runlab/invite-digest
until test -s /runlab/address; do kill -0 "$owner" || { cat /runlab/original.log /runlab/witness.log; exit 1; }; sleep .05; done
touch /runlab/enrolled
sync
echo PARROT_ENROLLED
until test -f /runlab/entered; do sleep .05; done
touch /runlab/stop
until test -f /runlab/dropping; do sleep .05; done
if external; then wait "$witness"; else kill -0 "$witness"; fi
test ! -s /runlab/proof
sync
echo PARROT_DELIVERY_BLOCKED
while :; do sleep 10; done
