#!/bin/bash
# Gate hosts on AWS, driven from a workstation with the aws CLI. The hosts hold no
# AWS credentials: they download through presigned addresses, and what they upload
# streams through this workstation.
#
#   tools/gate-aws.sh image aarch64|armv7 REVISION [INPUT]...
#       build the gate image of a target: a host from the base image set up and
#       prepared by tools/gate-host.sh at REVISION, with the reference builds
#       restored from the archive where it holds their packs and the packs of
#       the others uploaded; then the image, tagged Name=triergon-gate-TARGET,
#       replacing the earlier ones and their snapshots. INPUT is a media inputs
#       archive (.tar.gz, repository-relative paths): a local file or s3:KEY in
#       the evidence archive
#   tools/gate-aws.sh run aarch64|armv7 SHARDS REVISION OUT [--full] [--items NAME,...]
#       the full gate of REVISION on SHARDS hosts from the target's image, in
#       parallel: each takes the references of REVISION from its image or the
#       archive, gets ready, and runs its shard (tools/gate-host.sh); the
#       shards' outputs are archived under qualification/gate-runs/, their logs
#       and records copied to OUT, and joined by `cargo xtask gate summary`
#       into OUT/summary.json. Each host is terminated as soon as its shard's
#       outputs are archived and checked. It launches nothing unless this
#       checkout is REVISION, unchanged, and `cargo xtask gate check` passes in it
#       for SHARDS shards of the target. The RSS module review is the one REVISION commits
#       for its module source and the image's kernel. --items runs those items of
#       the plan alone
#
# Environment: AWS_PROFILE and AWS_REGION; TRIERGON_SUBNET, the subnet hosts
# start in; TRIERGON_SSH_CIDR, the one address (/32) SSH is open to, which must be
# this workstation's current public address or nothing is launched;
# TRIERGON_EVIDENCE_ARCHIVE, the s3:// evidence archive; optionally
# TRIERGON_BASE_IMAGE (default: the current Ubuntu 24.04 arm64 image) and
# TRIERGON_VOLUME_GB (default 200). Hosts are c6g.4xlarge, the class the gates
# qualify on for both targets: Graviton2, which runs 32-bit user spaces, with the
# 32 GiB kernel-bound memory needs. Every
# resource is tagged Project=triergon; hosts terminate when they shut down, and a
# timer on disk shuts each down 72 hours after it boots, whatever happens. Each
# command removes its hosts, security group and key pair when it ends, and exits
# non-zero when a shard did not finish or the summary fails.
set -euo pipefail

: "${AWS_PROFILE:?}" "${AWS_REGION:?}" "${TRIERGON_SUBNET:?}" "${TRIERGON_SSH_CIDR:?}"
: "${TRIERGON_EVIDENCE_ARCHIVE:?}"
case $TRIERGON_SSH_CIDR in
*/32) ;;
*) echo "TRIERGON_SSH_CIDR must be one address (/32): $TRIERGON_SSH_CIDR" >&2; exit 2 ;;
esac
TYPE=c6g.4xlarge
VOLUME=${TRIERGON_VOLUME_GB:-200}
HERE=$(cd "$(dirname "$0")" && pwd)
ARCHIVE=${TRIERGON_EVIDENCE_ARCHIVE%/}
BUCKET=${ARCHIVE#s3://}; BUCKET=${BUCKET%%/*}
PREFIX=${ARCHIVE#s3://"$BUCKET"}; PREFIX=${PREFIX#/}; PREFIX=${PREFIX:+$PREFIX/}
USERLAND=evidence/armv7-userland/f93de19128a5bc101c125a4a1f7c61f42062675186cfc86f627d220d1d079533.tar.gz
# First-boot user data: a timer, kept on disk and so in an image made from the host,
# that powers the host off (and so terminates it) 72 hours after each boot.
EXPIRY='#!/bin/sh
cat > /etc/systemd/system/triergon-expire.service <<EOF
[Unit]
Description=Power off a gate host
[Service]
Type=oneshot
ExecStart=/usr/bin/systemctl poweroff
EOF
cat > /etc/systemd/system/triergon-expire.timer <<EOF
[Unit]
Description=Power off a gate host 72 hours after it boots
[Timer]
OnBootSec=72h
[Install]
WantedBy=timers.target
EOF
systemctl daemon-reload
systemctl enable --now triergon-expire.timer
'
# Seconds between polls of a host; a run polls rarely, and on CPU 0, so as not to
# disturb what it times on CPU 1.
POLL=60
STATE=
RUN_OBJECT=
NAME=
SG=
INSTANCES=()

say() { echo "$(date -u +%FT%TZ) $*"; }
tags() { echo "ResourceType=$1,Tags=[{Key=Project,Value=triergon},{Key=Name,Value=$2}]"; }
presign() { aws s3 presign "$ARCHIVE/$1" --expires-in 86400; }
archived() { aws s3 ls "$ARCHIVE/$1" >/dev/null 2>&1 < /dev/null; }

# A key pair and a security group for this session, removed on exit.
session() {
    NAME=$1
    STATE=$(mktemp -d)
    aws ec2 create-key-pair --key-name "$NAME" --key-type ed25519 \
        --tag-specifications "$(tags key-pair "$NAME")" \
        --query KeyMaterial --output text > "$STATE/key"
    chmod 600 "$STATE/key"
    local vpc
    vpc=$(aws ec2 describe-subnets --subnet-ids "$TRIERGON_SUBNET" --query 'Subnets[0].VpcId' --output text)
    SG=$(aws ec2 create-security-group --group-name "$NAME" --description "gate hosts" \
        --vpc-id "$vpc" --tag-specifications "$(tags security-group "$NAME")" \
        --query GroupId --output text)
    aws ec2 authorize-security-group-ingress --group-id "$SG" --protocol tcp --port 22 \
        --cidr "$TRIERGON_SSH_CIDR" >/dev/null
    trap teardown EXIT
    trap 'exit 130' INT TERM
    say "session $NAME"
}

teardown() {
    local removed=1 live
    if [ ${#INSTANCES[@]} -gt 0 ]; then
        # Hosts terminated earlier may no longer exist; only the others are named.
        live=$(aws ec2 describe-instances \
            --filters "Name=instance-id,Values=$(IFS=,; echo "${INSTANCES[*]}")" \
            "Name=instance-state-name,Values=pending,running,stopping,stopped,shutting-down" \
            --query 'Reservations[].Instances[].InstanceId' --output text) || removed=0
        # shellcheck disable=SC2086
        if [ "$removed" = 1 ] && { [ -z "$live" ] ||
            { aws ec2 terminate-instances --instance-ids $live >/dev/null &&
                aws ec2 wait instance-terminated --instance-ids $live; }; }; then
            say "terminated ${live:-no host left}"
        else
            removed=0
        fi
    fi
    [ -z "$SG" ] || aws ec2 delete-security-group --group-id "$SG" || removed=0
    [ -z "$NAME" ] || aws ec2 delete-key-pair --key-name "$NAME" || removed=0
    if [ "$removed" = 1 ]; then
        [ -z "$STATE" ] || rm -rf "$STATE"
    else
        say "could not remove ${INSTANCES[*]} $SG $NAME; their key and addresses are in $STATE"
    fi
}

# COUNT hosts from IMAGE, numbered from FIRST (default 1); adds them to INSTANCES
# and writes STATE/ip-I.
launch() {
    local image=$1 count=$2 first=${3:-1}
    local ids new
    ids=$(aws ec2 run-instances --image-id "$image" --instance-type "$TYPE" --count "$count" \
        --subnet-id "$TRIERGON_SUBNET" --security-group-ids "$SG" --key-name "$NAME" \
        --associate-public-ip-address --instance-initiated-shutdown-behavior terminate \
        --block-device-mappings "DeviceName=/dev/sda1,Ebs={VolumeSize=$VOLUME,VolumeType=gp3,DeleteOnTermination=true}" \
        --user-data "$EXPIRY" \
        --tag-specifications "$(tags instance "$NAME")" "$(tags volume "$NAME")" \
        --query 'Instances[].InstanceId' --output text)
    read -r -a new <<< "$ids"
    INSTANCES+=("${new[@]}")
    aws ec2 wait instance-running --instance-ids "${new[@]}"
    local i=$first id
    for id in "${new[@]}"; do
        aws ec2 describe-instances --instance-ids "$id" \
            --query 'Reservations[0].Instances[0].PublicIpAddress' --output text > "$STATE/ip-$i"
        aws ec2 describe-instances --instance-ids "$id" \
            --query 'Reservations[0].Instances[0].LaunchTime' --output text > "$STATE/launched-$i"
        i=$((i + 1))
    done
}

options() { echo -i "$STATE/key" -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new \
    -o UserKnownHostsFile="$STATE/known_hosts" -o ConnectTimeout=15 -o ServerAliveInterval=30; }
on() { local i=$1; shift; # shellcheck disable=SC2046
    ssh $(options) "ubuntu@$(cat "$STATE/ip-$i")" "$@"; }
copy() { local i=$1 from=$2 to=$3; # shellcheck disable=SC2046
    scp -q $(options) "$from" "ubuntu@$(cat "$STATE/ip-$i"):$to"; }

# Wait until host I answers, giving up after about five minutes.
reachable() {
    local i=$1 tries=0
    until on "$i" true 2>/dev/null; do
        tries=$((tries + 1))
        [ "$tries" -lt 20 ] || { say "host $i unreachable"; return 1; }
        sleep 15
    done
}

# This workstation's public address, as AWS sees it; empty when it cannot be read.
workstation() { curl -fsS --max-time 10 https://checkip.amazonaws.com 2>/dev/null | tr -d '[:space:]'; }

# Refuse to launch unless TRIERGON_SSH_CIDR is this workstation's current address
# as a /32: the hosts must be reachable from here, and from nowhere else.
require_address() {
    local address
    address=$(workstation)
    echo "workstation address ${address:-unknown}, TRIERGON_SSH_CIDR $TRIERGON_SSH_CIDR"
    if [ -z "$address" ] || [ "$address/32" != "$TRIERGON_SSH_CIDR" ]; then
        echo "TRIERGON_SSH_CIDR must be this workstation's address as a /32: launching nothing" >&2
        return 2
    fi
}

# Whether the session's security group lets ADDRESS reach the hosts by SSH.
admitted() {
    aws ec2 describe-security-groups --group-ids "$SG" \
        --query 'SecurityGroups[0].IpPermissions[].IpRanges[].CidrIp' --output text < /dev/null |
        tr '\t' '\n' | grep -qx "$1/32"
}

# Run tools/gate-host.sh ARGS on host I, detached, and wait until its log records
# MARKER; give up after five failed polls in a row. A poll that fails because this
# workstation's address left the security group's one /32 is not a failure: the
# hosts work on, so it says so once and waits until the group admits the new address,
# which it never changes itself.
detached() {
    local i=$1 marker=$2; shift 2
    on "$i" "setsid nohup env $(printf '%q ' "${ENVIRONMENT[@]}") bash ~/gate-host.sh $* > ~/gate-host.out 2>&1 < /dev/null &"
    local failures=0 state moved=
    while sleep "$POLL"; do
        if state=$(on "$i" "taskset -c 0 tail -1 ~/gate-host.log; taskset -c 0 pgrep -f '[g]ate-host.sh' >/dev/null && echo running; true" 2>/dev/null); then
            failures=0
            case $state in *"$marker"*) return 0;; esac
            case $state in *running*) ;; *) say "host $i stopped"; on "$i" "tail -n 20 ~/gate-host.log ~/gate-host.out"; return 1;; esac
        else
            local address
            address=$(workstation)
            if [ -n "$address" ] && ! admitted "$address"; then
                [ "$address" = "${moved:-}" ] || say "host $i: unreachable, because this \
workstation's address is now $address and $SG admits SSH from another; polls resume \
once $SG admits $address/32"
                moved=$address failures=0
                continue
            fi
            failures=$((failures + 1))
            [ "$failures" -lt 5 ] || { say "host $i: no answer to five polls"; return 1; }
        fi
    done
}
ENVIRONMENT=(PATH=/usr/local/bin:/usr/bin:/bin)

# The reference packs host I lacks for its checkout, from the archive.
fetch_packs() {
    local i=$1 repository=$2 name key state
    while read -r name key state; do
        [ "$state" = missing ] || continue
        local object="qualification/reference-builds/$name/$key.tar.zst"
        archived "$object" || { say "host $i: $name $key not archived"; continue; }
        on "$i" "mkdir -p $repository/.research/reference-cache/$name && curl -fsS --retry 3 -o $repository/.research/reference-cache/$name/$key.tar.zst '$(presign "$object")'" < /dev/null
        say "host $i: $name $key fetched"
    done
}

# Upload the pack of every current reference build on host I that the archive lacks.
upload_packs() {
    local i=$1 inside=$2 repository=$3 name key state
    on "$i" "$inside 'cd ~/triergon && . ~/.cargo/env && cargo xtask references keys'" < /dev/null |
        while read -r name key state; do
            local object="qualification/reference-builds/$name/$key.tar.zst"
            [ "$state" = current ] || continue
            archived "$object" && continue
            on "$i" "$inside 'cd ~/triergon && . ~/.cargo/env && cargo xtask references pack $name'" < /dev/null > /dev/null
            on "$i" "cat $repository/.research/reference-cache/$name/$key.tar.zst" < /dev/null |
                aws s3 cp --only-show-errors - "$ARCHIVE/$object"
            say "uploaded $object"
        done
}

# The command that runs a shell command in the measurement environment of a host.
inside_command() {
    if [ "$1" = armv7 ]; then
        echo "sudo chroot /srv/armhf setarch linux32 sudo -u ubuntu -H bash -lc"
    else
        echo "bash -lc"
    fi
}
repository_path() { if [ "$1" = armv7 ]; then echo /srv/armhf/home/ubuntu/triergon; else echo /home/ubuntu/triergon; fi; }

image() {
    local target=$1 revision=$2; shift 2
    require_address
    local base=${TRIERGON_BASE_IMAGE:-$(aws ssm get-parameter --name \
        /aws/service/canonical/ubuntu/server/24.04/stable/current/arm64/hvm/ebs-gp3/ami-id \
        --query Parameter.Value --output text)}
    local stamp; stamp=$(date -u +%Y%m%d-%H%M%S)
    session "triergon-gate-image-$target-$stamp"
    launch "$base" 1
    reachable 1
    copy 1 "$HERE/gate-host.sh" gate-host.sh
    local inputs=() input
    for input in "$@"; do
        case $input in
        s3:*) inputs+=("$(presign "${input#s3:}")") ;;
        *) copy 1 "$input" "$(basename "$input")"; inputs+=("/home/ubuntu/$(basename "$input")") ;;
        esac
    done
    ENVIRONMENT+=("INPUTS=${inputs[*]}")
    [ "$target" != armv7 ] || ENVIRONMENT+=("USERLAND_URL=$(presign "$USERLAND")")
    say "setup of $target at $revision on ${INSTANCES[0]}"
    detached 1 "SETUP DONE" setup "$target" "$revision"
    local inside repository
    inside=$(inside_command "$target"); repository=$(repository_path "$target")
    on 1 "$inside 'cd ~/triergon && . ~/.cargo/env && cargo xtask references keys'" | fetch_packs 1 "$repository"
    say "prepare"
    detached 1 PREPARED prepare
    upload_packs 1 "$inside" "$repository"
    on 1 "rm -rf $repository/.research/reference-cache ~/*.tar.gz ~/gate-host.out; mv ~/gate-host.log ~/image.log; sudo apt-get clean"
    local name="triergon-gate-$target"
    say "image $name-$stamp"
    local ami
    ami=$(aws ec2 create-image --instance-id "${INSTANCES[0]}" --name "$name-$stamp" \
        --description "Gate host for $target at $revision" \
        --tag-specifications "$(tags image "$name")" "$(tags snapshot "$name")" \
        --query ImageId --output text)
    local state
    while state=$(aws ec2 describe-images --image-ids "$ami" --query 'Images[0].State' --output text); do
        case $state in available) break ;; pending) sleep 60 ;; *) say "image $ami $state"; return 1 ;; esac
    done
    say "image $ami available"
    # A host from the image boots the reviewed kernel, with the kernel packages held
    # and no unattended upgrades; otherwise the image is removed again.
    local kernel verdict
    kernel=$(on 1 "cat ~/reviewed-kernel")
    launch "$ami" 1 2
    reachable 2
    verdict=$(on 2 "uname -r; apt-mark showhold | grep -c '^linux-'; systemctl is-enabled unattended-upgrades.service apt-daily.timer apt-daily-upgrade.timer 2>&1 | sort -u | tr '\n' ' '")
    say "host from $ami: $(echo "$verdict" | tr '\n' ' ')(reviewed kernel $kernel)"
    if [ "$(echo "$verdict" | sed -n 1p)" != "$kernel" ] || [ "$(echo "$verdict" | sed -n 2p)" -lt 1 ] ||
        [ "$(echo "$verdict" | sed -n 3p)" != "masked " ]; then
        say "image $ami does not boot the reviewed kernel held: removing it"
        local snapshots
        snapshots=$(aws ec2 describe-images --image-ids "$ami" \
            --query 'Images[0].BlockDeviceMappings[].Ebs.SnapshotId' --output text)
        aws ec2 deregister-image --image-id "$ami"
        for snapshot in $snapshots; do aws ec2 delete-snapshot --snapshot-id "$snapshot"; done
        return 1
    fi
    local old snapshots
    for old in $(aws ec2 describe-images --owners self \
        --filters "Name=tag:Name,Values=$name" "Name=tag:Project,Values=triergon" \
        --query 'Images[].ImageId' --output text); do
        [ "$old" != "$ami" ] || continue
        snapshots=$(aws ec2 describe-images --image-ids "$old" \
            --query 'Images[0].BlockDeviceMappings[].Ebs.SnapshotId' --output text)
        aws ec2 deregister-image --image-id "$old"
        for snapshot in $snapshots; do aws ec2 delete-snapshot --snapshot-id "$snapshot"; done
        say "removed $old and $snapshots"
    done
    echo "$ami"
}

# Get host I ready for shard I of COUNT and run it. However it ends, collect what
# the host has (see collect) and release it (see release).
shard() {
    local i=$1 count=$2 target=$3 revision=$4 out=$5 full=$6
    mkdir -p "$out/shard-$i"
    # The values now: the trap can run after this function's locals are gone.
    # shellcheck disable=SC2064
    trap "$(printf 'collect %q %q %q %q; release %q %q %q' "$i" "$target" "$revision" "$out" "$i" "$count" "$out")" EXIT
    reachable "$i"
    copy "$i" "$HERE/gate-host.sh" gate-host.sh
    on "$i" "bash ~/gate-host.sh checkout $revision" | tail -n +2 |
        fetch_packs "$i" "$(repository_path "$target")"
    detached "$i" "READY" ready "$i/$count"
    detached "$i" "DONE" run "$i/$count" ".research/gate/$revision" "$full"
    touch "$out/shard-$i/finished"
}

# Collect what host I holds of its shard: its outputs archived whole as
# RUN_OBJECT/shard-I.tar.zst and checked by downloading them back on the host
# (OUT/shard-I/verified on a match), their logs and records copied to OUT/shard-I,
# and the packs of reference builds it made uploaded. Best effort: a host that
# failed or hangs gives what it can.
collect() {
    local i=$1 target=$2 revision=$3 out=$4
    local inside repository run=".research/gate/$revision" object="$RUN_OBJECT/shard-$i.tar.zst"
    set +e
    inside=$(inside_command "$target"); repository=$(repository_path "$target")
    on "$i" "cat ~/gate-host.log" > "$out/shard-$i/gate-host.log" < /dev/null
    local packed
    packed=$(on "$i" "cd $repository/$run && tar -cf - . | zstd -q -T0 > ~/shard.tar.zst && sha256sum < ~/shard.tar.zst | cut -c1-64" < /dev/null)
    if [ -n "$packed" ] &&
        on "$i" "cat ~/shard.tar.zst" < /dev/null | aws s3 cp --only-show-errors - "$ARCHIVE/$object" &&
        [ "$(on "$i" "curl -fsS --retry 3 '$(presign "$object")' | sha256sum | cut -c1-64" < /dev/null)" = "$packed" ]; then
        echo "$packed" > "$out/shard-$i/verified"
    else
        say "host $i: its outputs are not archived"
    fi
    on "$i" "cd $repository/$run && tar -cf - shard.json identity-*.json run.log steps logs 2>/dev/null" < /dev/null |
        tar -xf - -C "$out/shard-$i"
    upload_packs "$i" "$inside" "$repository"
}

# Release host I once its shard has ended: the last shard to end keeps its host for
# the summary (OUT/summary-host) when every shard finished; any other host is
# terminated, unless its outputs are not archived, when it stays for the session's
# teardown.
release() {
    local i=$1 count=$2 out=$3
    until mkdir "$out/.release" 2>/dev/null; do sleep 1; done
    echo "$i" >> "$out/ended"
    if [ "$(wc -l < "$out/ended")" -eq "$count" ] &&
        [ "$(find "$out" -path "$out/shard-*/finished" | wc -l)" -eq "$count" ]; then
        echo "$i" > "$out/summary-host"
        rmdir "$out/.release"
        say "host $i kept for the summary"
    else
        rmdir "$out/.release"
        if [ -f "$out/shard-$i/verified" ]; then
            terminate "$i" "$out"
        else
            say "host $i kept until the session ends: its outputs are not archived"
        fi
    fi
}

# Terminate host I, recording when in OUT/terminated-I.
terminate() {
    local i=$1 out=$2
    aws ec2 terminate-instances --instance-ids "${INSTANCES[$((i - 1))]}" >/dev/null &&
        date -u +%FT%TZ > "$out/terminated-$i" && say "host $i collected and terminated"
}

# Join every shard's records on host I into OUT/summary.json (archived beside the
# shards' outputs) with `cargo xtask gate summary`, then terminate the host.
summarise() {
    local host=$1 target=$2 count=$3 out=$4
    local i repository inside shards="" status=0
    repository=$(repository_path "$target"); inside=$(inside_command "$target")
    for i in $(seq 1 "$count"); do
        [ -f "$out/shard-$i/shard.json" ] || continue
        (cd "$out" && COPYFILE_DISABLE=1 tar -cf - "shard-$i"/shard.json "shard-$i"/steps "shard-$i"/identity-*.json) |
            on "$host" "mkdir -p $repository/.research/gate/shards && tar -xf - -C $repository/.research/gate/shards"
        shards="$shards .research/gate/shards/shard-$i"
    done
    if ! on "$host" "$inside 'cd ~/triergon && . ~/.cargo/env && cargo xtask gate summary --output .research/gate/summary.json$shards'" \
        > "$out/summary.txt"; then
        say "the summary fails: $out/summary.txt"
        status=1
    fi
    on "$host" "cat $repository/.research/gate/summary.json" > "$out/summary.json" || true
    aws s3 cp --only-show-errors "$out/summary.json" "$ARCHIVE/$RUN_OBJECT/summary.json" || true
    terminate "$host" "$out"
    return "$status"
}

run() {
    local target=$1 count=$2 revision=$3 out=$4 full="" items=""
    shift 4
    while [ $# -gt 0 ]; do
        case $1 in
        --full) full=--full ;;
        --items) items=$2; shift ;;
        *) echo "unknown option $1" >&2; return 2 ;;
        esac
        shift
    done
    [ -z "$items" ] || ENVIRONMENT+=("ITEMS=$items")
    require_address
    local name="triergon-gate-$target" ami stamp checkout
    # Before any host: every input each shard's preflight reads is one a prepared
    # host has. The check reads this checkout, so it must be REVISION, unchanged.
    checkout=$(cd "$HERE/.." && pwd)
    # Hosts fetch the revision by its full name.
    revision=$(git -C "$checkout" rev-parse "$revision^{commit}")
    if [ "$revision" != "$(git -C "$checkout" rev-parse HEAD)" ] ||
        ! git -C "$checkout" diff --quiet HEAD; then
        echo "check out $revision here, unchanged: the gate check reads this checkout" >&2
        return 2
    fi
    local triple
    case $target in
    aarch64) triple=aarch64-unknown-linux-gnu ;;
    armv7) triple=thumbv7neon-unknown-linux-gnueabihf ;;
    *) echo "unknown target $target" >&2; return 2 ;;
    esac
    (cd "$checkout" && CARGO_INCREMENTAL=0 cargo xtask gate check --shards "$count" --target "$triple" ${items:+--items "$items"}) ||
        { echo "the gate check fails: no host launched" >&2; return 1; }
    stamp=$(date -u +%Y%m%d-%H%M%S)
    ami=$(aws ec2 describe-images --owners self \
        --filters "Name=tag:Name,Values=$name" "Name=tag:Project,Values=triergon" \
        --query 'sort_by(Images,&CreationDate)[-1].ImageId' --output text)
    session "triergon-gate-run-$target-$stamp"
    POLL=300
    RUN_OBJECT="qualification/gate-runs/$revision-$target-$stamp"
    mkdir -p "$out"
    rm -f "$out/ended" "$out/summary-host" "$out"/terminated-* "$out"/shard-*/finished "$out"/shard-*/verified
    rmdir "$out/.release" 2>/dev/null || true
    launch "$ami" "$count"
    say "$count hosts from $ami: ${INSTANCES[*]}"
    local i pids=()
    for i in $(seq 1 "$count"); do
        shard "$i" "$count" "$target" "$revision" "$out" "$full" > "$out/shard-$i.log" 2>&1 &
        pids+=($!)
    done
    local failed=0
    for i in "${!pids[@]}"; do
        wait "${pids[$i]}" || { say "shard $((i + 1)) did not finish"; failed=1; }
    done
    if [ -f "$out/summary-host" ]; then
        summarise "$(cat "$out/summary-host")" "$target" "$count" "$out" || failed=1
    else
        say "no summary: not every shard finished"
    fi
    for i in $(seq 1 "$count"); do
        echo "host $i: launched $(cat "$STATE/launched-$i")" \
            "terminated $(cat "$out/terminated-$i" 2>/dev/null || echo "with the session")" \
            "$(grep -m1 READY "$out/shard-$i/gate-host.log" 2>/dev/null)" \
            "$(grep DONE "$out/shard-$i/gate-host.log" 2>/dev/null | tail -1)"
    done | tee "$out/hosts.txt"
    say "outputs archived under $RUN_OBJECT/"
    return "$failed"
}

command=${1:?usage: tools/gate-aws.sh image|run ...}
shift
case $command in
image) image "$@" ;;
run) run "$@" ;;
*) echo "unknown command $command" >&2; exit 2 ;;
esac
