#!/usr/bin/env bash
# Idempotent teardown of what provision-oci.sh created, for one NAME.
# Safe to re-run: it skips whatever is already gone. Order matters —
# a VCN cannot be deleted until its instances, subnets and gateways are.
#
#   ./teardown-oci.sh            # tears down NAME=kanade
#   NAME=kanade2 ./teardown-oci.sh
set -euo pipefail

PROFILE="${OCI_PROFILE:-kanade}"
NAME="${NAME:-kanade}"
export OCI_CLI_PROFILE="$PROFILE"
export OCI_CLI_AUTH="${OCI_AUTH:-security_token}"
export OCI_CLI_SUPPRESS_FILE_PERMISSIONS_WARNING=True
export SUPPRESS_LABEL_WARNING=True

TENANCY="$(awk -v p="[$PROFILE]" '
  $0==p {f=1; next} /^\[/{f=0} f && /^tenancy=/{sub(/^tenancy=/,""); print; exit}' "$HOME/.oci/config")"
# Without this, an empty compartment makes every `oci ... -c ""` a silent
# no-op and the script would still print "torn down" while deleting nothing.
[ -n "$TENANCY" ] || { echo "could not read tenancy for profile [$PROFILE] from ~/.oci/config" >&2; exit 1; }
C="$TENANCY"

# Only act on resources provision-oci.sh stamped with this ownership tag, so
# teardown can never delete a same-named resource it did not create.
OWNER='kanade-provision-oci'

id_of() { grep -v '^null$' <<<"$1" || true; }

echo "==> terminating instance(s) named $NAME (tagged managed-by=$OWNER)"
for i in $(oci compute instance list -c "$C" --display-name "$NAME" \
    --query "data[?\"lifecycle-state\"!=\`TERMINATED\` && \"freeform-tags\".\"managed-by\"=='$OWNER'].id" \
    --raw-output 2>/dev/null | tr -d '[],"'); do
  [ -n "$i" ] || continue
  echo "   $i"
  oci compute instance terminate --instance-id "$i" --force \
    --wait-for-state TERMINATED >/dev/null 2>&1 || true
done

VCN="$(id_of "$(oci network vcn list -c "$C" --display-name "$NAME-vcn" \
    --query "data[?\"freeform-tags\".\"managed-by\"=='$OWNER'].id | [0]" --raw-output 2>/dev/null)")"
if [ -n "$VCN" ]; then
  echo "==> deleting subnet(s)"
  for s in $(oci network subnet list -c "$C" --vcn-id "$VCN" --query 'data[].id' --raw-output 2>/dev/null | tr -d '[],"'); do
    [ -n "$s" ] || continue
    oci network subnet delete --subnet-id "$s" --force --wait-for-state TERMINATED >/dev/null 2>&1 || true
  done
  # An internet gateway that a route table still points at is "in use" and
  # refuses to delete (409), which then blocks the VCN delete. Clear the
  # default route table's rules first.
  echo "==> clearing route table rules (frees the internet gateway)"
  RT="$(oci network vcn get --vcn-id "$VCN" --query 'data."default-route-table-id"' --raw-output 2>/dev/null)"
  [ -n "$RT" ] && oci network route-table update --rt-id "$RT" --route-rules '[]' --force >/dev/null 2>&1 || true

  echo "==> deleting internet gateway(s)"
  for g in $(oci network internet-gateway list -c "$C" --vcn-id "$VCN" --query 'data[].id' --raw-output 2>/dev/null | tr -d '[],"'); do
    [ -n "$g" ] || continue
    oci network internet-gateway delete --ig-id "$g" --force --wait-for-state TERMINATED 2>&1 | grep -iE 'error|message' || true
  done
  echo "==> deleting VCN (takes its default route table + security list with it)"
  oci network vcn delete --vcn-id "$VCN" --force --wait-for-state TERMINATED 2>&1 | grep -iE 'error|message' || true
fi

# Verify, rather than trusting exit codes we deliberately softened for
# idempotency — the earlier version reported success while the IGW (and
# thus the VCN) was still there.
if [ -n "$(oci network vcn list -c "$C" --display-name "$NAME-vcn" \
    --query "data[?\"freeform-tags\".\"managed-by\"=='$OWNER'].id | [0]" --raw-output 2>/dev/null | grep -v '^null$')" ]; then
  echo "!! owned $NAME-vcn still present — a dependency is blocking deletion; re-run or check the console" >&2
  exit 1
fi
echo "==> torn down. (SSH key ~/.ssh/${NAME}_oci is left in place.)"
