# kanade — Oracle Cloud (Always Free ARM64) validation environment

Stand up a free Ampere A1 VM on Oracle Cloud to run a production-like
kanade deployment (see `../linux/`). Everything here is **idempotent**:
re-running reuses what exists and only creates what's missing.

## What "Always Free" gives you

- **ARM (Ampere A1):** a total budget of **4 OCPU / 24 GB**, as one VM or
  split across several. So two `2 OCPU / 12 GB` boxes, or four
  `1 OCPU / 6 GB`, etc. — pick per `OCPUS`/`MEMGB` below.
- 2 AMD micro VMs (`E2.1.Micro`, 1/8 OCPU, 1 GB) — a weak x86 fallback.
- 200 GB block storage total; the boot volume counts against it.

The catch: **A1 capacity is scarce in popular regions** (Phoenix, Ashburn).
`Out of host capacity` on launch is normal — the script retries across all
availability domains for a while; capacity frees up intermittently.

## Prerequisites (one-time)

1. An Oracle Cloud account (sign-up needs a card for identity verification;
   Always Free resources are not charged). The **home region is permanent**,
   and Always Free ARM lives there — so use the home region.

2. Install the OCI CLI and authenticate:

   ```bash
   scoop install oci-cli          # Windows; or the official installer / pip
   oci session authenticate --region <home-region> --profile-name kanade
   ```

   `session authenticate` opens a browser, you log in, and it writes a
   short-lived **security-token** profile to `~/.oci/config`. The scripts
   use `--profile kanade --auth security_token`. When the token expires
   (~1 h), refresh it:

   ```bash
   oci session refresh --profile kanade
   ```

   Region identifiers: US West (Phoenix) = `us-phoenix-1`, Tokyo =
   `ap-tokyo-1`, Osaka = `ap-osaka-1`, Ashburn = `us-ashburn-1`.

3. **For a long unattended capacity hunt, use API-key auth, not the session
   token.** The `session authenticate` token expires ~hourly and cannot be
   refreshed once expired, so a multi-hour hunt dies. Set up a non-expiring
   api-key profile once (while a session is valid):

   ```bash
   openssl genrsa -out ~/.oci/kanade_api.pem 2048 && chmod 600 ~/.oci/kanade_api.pem
   openssl rsa -pubout -in ~/.oci/kanade_api.pem -out ~/.oci/kanade_api_public.pem
   USER=$(oci iam user list -c <tenancy> --query "data[?contains(name,'@')].id | [0]" --raw-output)
   oci iam user api-key upload --user-id "$USER" --key-file ~/.oci/kanade_api_public.pem \
     --query 'data.fingerprint' --raw-output
   ```

   Then add a `[kanade-api]` profile to `~/.oci/config` (`user`, `fingerprint`,
   `tenancy`, `region`, `key_file`) and run the scripts with
   `OCI_PROFILE=kanade-api OCI_AUTH=api_key`.

## Provision

```bash
./provision-oci.sh                    # default: 2 OCPU / 12 GB, name "kanade"
OCPUS=1 MEMGB=6 ./provision-oci.sh     # smaller — grabs scarce capacity more easily
NAME=kanade2 ./provision-oci.sh        # a second box (stays within the 4/24 ARM budget)
```

It creates (or reuses): a VCN, an internet gateway, a default route to it,
ingress rules for **22 / 80 / 443**, a public subnet, an SSH keypair
(`~/.ssh/kanade_oci`), finds the latest Ubuntu 24.04 aarch64 image, and
launches the instance — retrying across availability domains until ARM
capacity appears. It also passes a cloud-init script that opens the
**instance-local** iptables for 80/443 (Oracle's Ubuntu images block by
default), so both firewall layers are handled.

On success it prints the public IP and the SSH command.

### Ownership boundary

These scripts operate in the **tenancy-root compartment** and select by
display-name, so they assume a **dedicated single-tenant account** — do not
run them against a shared or production tenancy. As a guard, every resource
`provision-oci.sh` creates is stamped with the freeform tag
`managed-by=kanade-provision-oci`, and `teardown-oci.sh` refuses to delete
any instance or VCN that lacks it — so a same-named resource it did not
create cannot be torn down by accident.

Env knobs: `OCPUS`, `MEMGB`, `NAME`, `OCI_PROFILE`, `SSH_KEY`,
`RETRY_ROUNDS` (default 40), `RETRY_SLEEP` (default 75 s).

## Deploy kanade onto it

Once the instance is up, follow `../linux/README.md`: assemble the offline
bundle (`bundle.sh` / `bundle.ps1`), copy it over, and install:

```bash
scp -i ~/.ssh/kanade_oci kanade-linux-arm64-bundle.tar.gz  ubuntu@<IP>:
ssh -i ~/.ssh/kanade_oci ubuntu@<IP>
tar -xzf kanade-linux-arm64-bundle.tar.gz && cd kanade-linux-arm64-bundle
sudo KANADE_DOMAIN=kanade.example.com bash ./setup.sh
```

Point DNS (`kanade.<domain>` and `nats.<domain>`) at the public IP first,
so Caddy can issue certs.

## Tear down

```bash
./teardown-oci.sh            # NAME=kanade
NAME=kanade2 ./teardown-oci.sh
```

Terminates the instance, then deletes the subnet, gateway and VCN (the
default route table and security list go with the VCN). The SSH key is left
in place.

## Gotchas learned the hard way

- **`file://` for `--ingress-security-rules` needs a Windows path on
  Windows.** A Git Bash path (`/c/Users/...`) silently no-ops and the rules
  don't apply — so `provision-oci.sh` passes the JSON **inline** instead.
  Always verify the security list actually has 80/443 after applying.
- **`Get-Acl` warning** ("Permissions on ~/.oci/config are too open"): the
  OCI CLI's Windows permission check misfires; it's harmless. Silenced via
  `OCI_CLI_SUPPRESS_FILE_PERMISSIONS_WARNING=True`.
- **Two firewall layers.** The cloud Security List *and* the instance's own
  iptables must both allow 80/443. The Security List is set here; the
  cloud-init handles iptables. Miss either and Caddy can't get a cert.
- **Capacity, not config, is the blocker.** If every AD is out for a long
  time, drop to `OCPUS=1 MEMGB=6`, try a different home region at signup, or
  fall back to an AMD micro (x86 — which then needs an x86_64 backend build,
  not the aarch64 bundle).
