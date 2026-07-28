#!/usr/bin/env bash
# Idempotent provisioning of an Oracle Cloud "Always Free" ARM64 VM for a
# kanade validation deployment: VCN + public subnet + firewall (22/80/443)
# + an Ampere A1.Flex instance, with capacity-retry across availability
# domains. Re-running reuses whatever already exists and only creates or
# launches what is missing.
#
# Prerequisites (one-time):
#   scoop install oci-cli          # or the official installer
#   oci session authenticate --region <home-region> --profile-name kanade
#     ^ opens a browser; writes a security-token profile to ~/.oci/config
#
# Usage:
#   ./provision-oci.sh                 # defaults: profile kanade, 2 OCPU / 12 GB
#   OCPUS=1 MEMGB=6 ./provision-oci.sh # smaller shape (grabs capacity easier)
#   NAME=kanade2 ./provision-oci.sh    # a second validation box (ARM free budget = 4 OCPU / 24 GB total)
#
# Runs anywhere the OCI CLI runs, including Git Bash on Windows.
set -euo pipefail

PROFILE="${OCI_PROFILE:-kanade}"
NAME="${NAME:-kanade}"
OCPUS="${OCPUS:-2}"
MEMGB="${MEMGB:-12}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/${NAME}_oci}"
RETRY_ROUNDS="${RETRY_ROUNDS:-40}"
RETRY_SLEEP="${RETRY_SLEEP:-75}"

# Ownership tag stamped on every resource this tool creates. teardown-oci.sh
# refuses to delete anything without it, so it cannot touch a same-named
# resource it did not create. (These scripts still assume a dedicated
# single-tenant validation account — see the README — but the tag bounds the
# blast radius within it.)
TAG='{"managed-by":"kanade-provision-oci"}'

export OCI_CLI_PROFILE="$PROFILE"
# security_token (from `oci session authenticate`) expires ~hourly and cannot
# be refreshed unattended — for a long capacity hunt use an api_key profile
# (OCI_AUTH=api_key OCI_PROFILE=kanade-api), which does not expire.
export OCI_CLI_AUTH="${OCI_AUTH:-security_token}"
export OCI_CLI_SUPPRESS_FILE_PERMISSIONS_WARNING=True   # silences the Get-Acl warning on Windows
export SUPPRESS_LABEL_WARNING=True

# Compartment = tenancy root, read from the profile in ~/.oci/config.
TENANCY="$(awk -v p="[$PROFILE]" '
  $0==p {f=1; next} /^\[/{f=0} f && /^tenancy=/{sub(/^tenancy=/,""); print; exit}' "$HOME/.oci/config")"
[ -n "$TENANCY" ] || { echo "could not read tenancy for profile [$PROFILE] from ~/.oci/config" >&2; exit 1; }
C="$TENANCY"
echo "compartment (tenancy root): $C"

# ---- helpers -------------------------------------------------------------

# Print the id of the first non-deleted resource with a given display-name,
# or empty. $1=list-subcommand-as-space-string, $2=display-name, rest=extra args.
first_id() {  # usage: first_id "network vcn list" "kanade-vcn" --vcn-id ...
  local sub="$1"; shift
  local name="$1"; shift
  # shellcheck disable=SC2086
  oci $sub -c "$C" --display-name "$name" "$@" \
    --query 'data[?"lifecycle-state"!=`TERMINATED` && "lifecycle-state"!=`TERMINATING`].id | [0]' \
    --raw-output 2>/dev/null | grep -v '^null$' || true
}

# ---- 1. VCN --------------------------------------------------------------
VCN="$(first_id "network vcn list" "$NAME-vcn")"
if [ -z "$VCN" ]; then
  echo "==> creating VCN $NAME-vcn"
  VCN="$(oci network vcn create -c "$C" --cidr-block 10.0.0.0/16 --display-name "$NAME-vcn" \
        --freeform-tags "$TAG" \
        --dns-label "${NAME//-/}" --wait-for-state AVAILABLE --query 'data.id' --raw-output)"
fi
echo "vcn: $VCN"
RT="$(oci network vcn get --vcn-id "$VCN" --query 'data."default-route-table-id"' --raw-output)"
SL="$(oci network vcn get --vcn-id "$VCN" --query 'data."default-security-list-id"' --raw-output)"

# ---- 2. Internet Gateway -------------------------------------------------
IGW="$(oci network internet-gateway list -c "$C" --vcn-id "$VCN" --display-name "$NAME-igw" \
      --query 'data[0].id' --raw-output 2>/dev/null | grep -v '^null$' || true)"
if [ -z "$IGW" ]; then
  echo "==> creating Internet Gateway"
  IGW="$(oci network internet-gateway create -c "$C" --vcn-id "$VCN" --is-enabled true \
        --display-name "$NAME-igw" --wait-for-state AVAILABLE --query 'data.id' --raw-output)"
fi
echo "igw: $IGW"

# ---- 3. default route 0.0.0.0/0 -> IGW -----------------------------------
echo "==> ensuring default route to IGW"
oci network route-table update --rt-id "$RT" --force \
  --route-rules "[{\"destination\":\"0.0.0.0/0\",\"destinationType\":\"CIDR_BLOCK\",\"networkEntityId\":\"$IGW\"}]" \
  >/dev/null

# ---- 4. default security list: ingress 22/80/443 -------------------------
# Inline JSON (not file://): the file:// form needs a Windows-style path on
# Windows and silently no-ops with a Git Bash path — inline avoids that.
echo "==> ensuring ingress 22/80/443"
oci network security-list update --security-list-id "$SL" --force \
  --ingress-security-rules '[
    {"source":"0.0.0.0/0","protocol":"6","isStateless":false,"tcpOptions":{"destinationPortRange":{"min":22,"max":22}}},
    {"source":"0.0.0.0/0","protocol":"6","isStateless":false,"tcpOptions":{"destinationPortRange":{"min":80,"max":80}}},
    {"source":"0.0.0.0/0","protocol":"6","isStateless":false,"tcpOptions":{"destinationPortRange":{"min":443,"max":443}}}
  ]' \
  --egress-security-rules '[{"destination":"0.0.0.0/0","protocol":"all","isStateless":false}]' \
  >/dev/null

# ---- 5. public subnet ----------------------------------------------------
SUBNET="$(first_id "network subnet list" "$NAME-subnet" --vcn-id "$VCN")"
if [ -z "$SUBNET" ]; then
  echo "==> creating public subnet"
  SUBNET="$(oci network subnet create -c "$C" --vcn-id "$VCN" --cidr-block 10.0.1.0/24 \
        --display-name "$NAME-subnet" --dns-label "${NAME//-/}sub" --route-table-id "$RT" \
        --security-list-ids "[\"$SL\"]" --prohibit-public-ip-on-vnic false \
        --wait-for-state AVAILABLE --query 'data.id' --raw-output)"
fi
echo "subnet: $SUBNET"

# ---- 6. SSH keypair ------------------------------------------------------
if [ ! -f "$SSH_KEY" ]; then
  echo "==> generating SSH keypair $SSH_KEY"
  mkdir -p "$(dirname "$SSH_KEY")"
  ssh-keygen -t ed25519 -f "$SSH_KEY" -N "" -C "$NAME-oci" >/dev/null
fi
echo "ssh key: $SSH_KEY (private — keep it)"

# ---- 7. Ubuntu 24.04 aarch64 image --------------------------------------
IMAGE="$(oci compute image list -c "$C" --operating-system "Canonical Ubuntu" \
        --operating-system-version "24.04" --shape VM.Standard.A1.Flex \
        --sort-by TIMECREATED --sort-order DESC --query 'data[0].id' --raw-output)"
# Fail fast: an empty image id would be passed to every launch attempt and
# burn the whole retry budget while looking like a capacity failure.
[ -n "$IMAGE" ] && [ "$IMAGE" != "null" ] \
  || { echo "no Ubuntu 24.04 aarch64 image found for VM.Standard.A1.Flex" >&2; exit 1; }
echo "image: $IMAGE"

# ---- 8. instance (reuse if present, else launch with AD retry) ----------
INST="$(oci compute instance list -c "$C" --display-name "$NAME" \
       --query 'data[?"lifecycle-state"==`RUNNING` || "lifecycle-state"==`PROVISIONING` || "lifecycle-state"==`STARTING` || "lifecycle-state"==`STOPPED` || "lifecycle-state"==`STOPPING`].id | [0]' \
       --raw-output 2>/dev/null | grep -v '^null$' || true)"

if [ -n "$INST" ]; then
  # Reuse an existing instance rather than launching a duplicate. A stopped
  # one (e.g. a prior run's box that was shut down) gets started and waited on.
  state="$(oci compute instance get --instance-id "$INST" --query 'data."lifecycle-state"' --raw-output 2>/dev/null)"
  if [ "$state" = "STOPPED" ] || [ "$state" = "STOPPING" ]; then
    echo "==> reusing existing instance (was $state) — starting it"
    oci compute instance action --instance-id "$INST" --action START \
      --wait-for-state RUNNING >/dev/null 2>&1 || true
  fi
fi

if [ -z "$INST" ]; then
  # cloud-init: open the instance-local iptables (Oracle Ubuntu blocks by default)
  CLOUD_INIT="$(mktemp)"
  cat > "$CLOUD_INIT" <<'EOF'
#!/bin/bash
iptables -I INPUT 6 -p tcp --dport 80  -j ACCEPT
iptables -I INPUT 6 -p tcp --dport 443 -j ACCEPT
netfilter-persistent save
EOF
  mapfile -t ADS < <(oci iam availability-domain list -c "$C" --query 'data[].name' --raw-output 2>/dev/null | tr -d '[],"' | grep -v '^$')
  echo "==> launching $NAME (A1.Flex ${OCPUS} OCPU / ${MEMGB} GB); ARM capacity is scarce, retrying up to $RETRY_ROUNDS rounds"
  for i in $(seq 1 "$RETRY_ROUNDS"); do
    for AD in "${ADS[@]}"; do
      AD="$(echo "$AD" | xargs)"   # trim
      [ -n "$AD" ] || continue
      # `|| true`: a capacity failure returns non-zero, and without this the
      # `out=$(...)` assignment would trip `set -e` and abort the whole
      # retry loop on the first "Out of host capacity".
      out="$(oci compute instance launch -c "$C" --availability-domain "$AD" \
        --shape VM.Standard.A1.Flex --shape-config "{\"ocpus\":$OCPUS,\"memoryInGBs\":$MEMGB}" \
        --image-id "$IMAGE" --subnet-id "$SUBNET" --assign-public-ip true \
        --display-name "$NAME" --freeform-tags "$TAG" \
        --ssh-authorized-keys-file "$SSH_KEY.pub" \
        --user-data-file "$CLOUD_INIT" \
        --wait-for-state RUNNING --query 'data.id' --raw-output 2>&1)" || true
      if echo "$out" | grep -qE 'ocid1\.instance'; then
        INST="$(echo "$out" | grep -oE 'ocid1\.instance[a-z0-9._-]+' | head -1)"
        echo "$(date +%H:%M:%S) launched at $AD (round $i): $INST"
        break 2
      fi
    done
    echo "$(date +%H:%M:%S) round $i/$RETRY_ROUNDS: out of host capacity in all ADs, retrying in ${RETRY_SLEEP}s..."
    sleep "$RETRY_SLEEP"
  done
  rm -f "$CLOUD_INIT"
fi

[ -n "$INST" ] || { echo "!! could not obtain ARM capacity after $RETRY_ROUNDS rounds. Try OCPUS=1 MEMGB=6, or later." >&2; exit 1; }

# ---- 9. public IP + next steps ------------------------------------------
IP="$(oci compute instance list-vnics --instance-id "$INST" --query 'data[0]."public-ip"' --raw-output 2>/dev/null)"
echo
echo "==> Instance ready: $INST"
echo "    public IP: $IP"
echo "    ssh -i $SSH_KEY ubuntu@$IP"
echo
echo "    Next: point DNS (kanade.<domain> + nats.<domain>) at $IP, then deploy the"
echo "    kanade bundle (deploy/linux): scp the tarball, extract, and run"
echo "    sudo KANADE_DOMAIN=<domain> bash ./setup.sh"
