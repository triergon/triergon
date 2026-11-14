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
#   tools/gate-aws.sh run aarch64|armv7 SHARDS REVISION REVIEW OUT [--full]
#       the full gate of REVISION on SHARDS hosts from the target's image, in
#       parallel: each takes the references of REVISION from its image or the
#       archive, gets ready, and runs its shard (tools/gate-host.sh); the
#       shards' outputs are archived under qualification/gate-runs/, their logs
#       and records copied to OUT, and joined by `cargo xtask gate summary`
#       into OUT/summary.json. REVIEW is the RSS module review (a local file)
#
# Environment: AWS_PROFILE and AWS_REGION; TRIERGON_SUBNET, the subnet hosts
# start in; TRIERGON_SSH_CIDR, the only address range SSH is open to;
# TRIERGON_EVIDENCE_ARCHIVE, the s3:// evidence archive; optionally
# TRIERGON_BASE_IMAGE (default: the current Ubuntu 24.04 arm64 image),
# TRIERGON_INSTANCE_TYPE (default c6g.4xlarge, Graviton2, which runs 32-bit user
# spaces) and TRIERGON_VOLUME_GB (default 200). Every resource is tagged
# Project=triergon; hosts terminate when they shut down and shut down after 72
# hours whatever happens. Each command removes its hosts, security group and key
# pair when it ends.
set -euo pipefail

: "${AWS_PROFILE:?}" "${AWS_REGION:?}" "${TRIERGON_SUBNET:?}" "${TRIERGON_SSH_CIDR:?}"
: "${TRIERGON_EVIDENCE_ARCHIVE:?}"
TYPE=${TRIERGON_INSTANCE_TYPE:-c6g.4xlarge}
VOLUME=${TRIERGON_VOLUME_GB:-200}
HERE=$(cd "$(dirname "$0")" && pwd)
ARCHIVE=${TRIERGON_EVIDENCE_ARCHIVE%/}
BUCKET=${ARCHIVE#s3://}; BUCKET=${BUCKET%%/*}
PREFIX=${ARCHIVE#s3://"$BUCKET"}; PREFIX=${PREFIX#/}; PREFIX=${PREFIX:+$PREFIX/}
USERLAND=evidence/armv7-userland/f93de19128a5bc101c125a4a1f7c61f42062675186cfc86f627d220d1d079533.tar.gz
STATE=
ARCHIVE_RUN=
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
    if [ ${#INSTANCES[@]} -gt 0 ]; then
        aws ec2 terminate-instances --instance-ids "${INSTANCES[@]}" >/dev/null || true
        aws ec2 wait instance-terminated --instance-ids "${INSTANCES[@]}" || true
        say "terminated ${INSTANCES[*]}"
    fi
    [ -z "$SG" ] || aws ec2 delete-security-group --group-id "$SG" || true
    [ -z "$NAME" ] || aws ec2 delete-key-pair --key-name "$NAME" || true
    [ -z "$STATE" ] || rm -rf "$STATE"
}

# COUNT hosts from IMAGE; sets INSTANCES and writes STATE/ip-I.
launch() {
    local image=$1 count=$2
    local ids
    ids=$(aws ec2 run-instances --image-id "$image" --instance-type "$TYPE" --count "$count" \
        --subnet-id "$TRIERGON_SUBNET" --security-group-ids "$SG" --key-name "$NAME" \
        --associate-public-ip-address --instance-initiated-shutdown-behavior terminate \
        --block-device-mappings "DeviceName=/dev/sda1,Ebs={VolumeSize=$VOLUME,VolumeType=gp3,DeleteOnTermination=true}" \
        --user-data "$(printf '#!/bin/sh\nshutdown -h +4320\n')" \
        --tag-specifications "$(tags instance "$NAME")" "$(tags volume "$NAME")" \
        --query 'Instances[].InstanceId' --output text)
    read -r -a INSTANCES <<< "$ids"
    aws ec2 wait instance-running --instance-ids "${INSTANCES[@]}"
    local i=1 id
    for id in "${INSTANCES[@]}"; do
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

# Run tools/gate-host.sh ARGS on host I, detached, and wait until its log records
# MARKER; give up after five failed polls in a row.
detached() {
    local i=$1 marker=$2; shift 2
    on "$i" "setsid nohup env $(printf '%q ' "${ENVIRONMENT[@]}") bash ~/gate-host.sh $* > ~/gate-host.out 2>&1 < /dev/null &"
    local failures=0 state
    while sleep 60; do
        if state=$(on "$i" "tail -1 ~/gate-host.log; pgrep -f '[g]ate-host.sh' >/dev/null && echo running; true" 2>/dev/null); then
            failures=0
            case $state in *"$marker"*) return 0;; esac
            case $state in *running*) ;; *) say "host $i stopped"; on "$i" "tail -n 20 ~/gate-host.log ~/gate-host.out"; return 1;; esac
        else
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

# Get host I ready for shard I of COUNT, run it, and collect its outputs: archived
# whole under ARCHIVE_RUN, their logs and records copied to OUT/shard-I. Host 1 stays
# for the summary; every other host is terminated once its outputs are collected.
shard() {
    local i=$1 count=$2 target=$3 revision=$4 out=$5 full=$6
    local inside repository run=".research/gate/$revision"
    inside=$(inside_command "$target"); repository=$(repository_path "$target")
    reachable "$i"
    copy "$i" "$HERE/gate-host.sh" gate-host.sh
    copy "$i" "$STATE/review" "$repository/.research/RSS-REVIEW.json"
    on "$i" "bash ~/gate-host.sh checkout $revision" | tail -n +2 | fetch_packs "$i" "$repository"
    detached "$i" "READY" ready "$i/$count" .research/RSS-REVIEW.json
    detached "$i" "DONE" run "$i/$count" .research/RSS-REVIEW.json "$run" "$full"
    mkdir -p "$out/shard-$i"
    on "$i" "cat ~/gate-host.log" > "$out/shard-$i/gate-host.log"
    on "$i" "cd $repository/$run && tar -cf - . | zstd -q -T0" |
        aws s3 cp --only-show-errors - "$ARCHIVE_RUN/shard-$i.tar.zst"
    on "$i" "cd $repository/$run && tar -cf - shard.json identity-*.json run.log steps logs" |
        tar -xf - -C "$out/shard-$i"
    upload_packs "$i" "$inside" "$repository"
    if [ "$i" != 1 ]; then
        aws ec2 terminate-instances --instance-ids "${INSTANCES[$((i - 1))]}" >/dev/null
        say "host $i collected and terminated"
    fi
}

run() {
    local target=$1 count=$2 revision=$3 review=$4 out=$5 full=${6:-}
    local name="triergon-gate-$target" ami stamp
    stamp=$(date -u +%Y%m%d-%H%M%S)
    ami=$(aws ec2 describe-images --owners self \
        --filters "Name=tag:Name,Values=$name" "Name=tag:Project,Values=triergon" \
        --query 'sort_by(Images,&CreationDate)[-1].ImageId' --output text)
    session "triergon-gate-run-$target-$stamp"
    ARCHIVE_RUN="$ARCHIVE/qualification/gate-runs/$revision-$target-$stamp"
    cp "$review" "$STATE/review"
    mkdir -p "$out"
    launch "$ami" "$count"
    say "$count hosts from $ami: ${INSTANCES[*]}"
    local i pids=()
    for i in $(seq 1 "$count"); do
        shard "$i" "$count" "$target" "$revision" "$out" "$full" > "$out/shard-$i.log" 2>&1 &
        pids+=($!)
    done
    for i in "${!pids[@]}"; do wait "${pids[$i]}" || say "shard $((i + 1)) did not finish"; done
    local repository inside shards=""
    repository=$(repository_path "$target"); inside=$(inside_command "$target")
    for i in $(seq 1 "$count"); do
        [ -f "$out/shard-$i/shard.json" ] || continue
        (cd "$out" && COPYFILE_DISABLE=1 tar -cf - "shard-$i"/shard.json "shard-$i"/steps "shard-$i"/identity-*.json) |
            on 1 "mkdir -p $repository/.research/gate/shards && tar -xf - -C $repository/.research/gate/shards"
        shards="$shards .research/gate/shards/shard-$i"
    done
    on 1 "$inside 'cd ~/triergon && . ~/.cargo/env && cargo xtask gate summary --output .research/gate/summary.json$shards'" \
        > "$out/summary.txt" || true
    on 1 "cat $repository/.research/gate/summary.json" > "$out/summary.json" || true
    aws s3 cp --only-show-errors "$out/summary.json" "$ARCHIVE_RUN/summary.json" || true
    for i in $(seq 1 "$count"); do
        echo "host $i: launched $(cat "$STATE/launched-$i")" \
            "$(grep -m1 READY "$out/shard-$i/gate-host.log" 2>/dev/null)" \
            "$(grep DONE "$out/shard-$i/gate-host.log" 2>/dev/null | tail -1)"
    done | tee "$out/hosts.txt"
    say "outputs archived under ${ARCHIVE_RUN#"$ARCHIVE"/}/"
}

command=${1:?usage: tools/gate-aws.sh image|run ...}
shift
case $command in
image) image "$@" ;;
run) run "$@" ;;
*) echo "unknown command $command" >&2; exit 2 ;;
esac
