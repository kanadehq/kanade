//! Offline `package.manifest.json` operations for `kanade app`
//! (SPEC §2.13.4.3, stage 2a): sign a manifest for a local artifact, verify it
//! against independently supplied trust input, and export the public trust
//! record for a publisher key.
//!
//! Everything here is local: no backend, no NATS, no Object Store, no
//! auto-update check. A successful verification is **signature + integrity
//! only** — it is not publisher authorization, freshness or revocation
//! (stage 2b), and never makes an artifact deploy-ready.
//!
//! # Private key handling
//!
//! The publisher private key (padded base64 of the 32-byte Ed25519 seed) is
//! read from a hidden terminal prompt, or from stdin with `--secret-stdin`.
//! It is never accepted via argv or an environment variable, never printed,
//! and never written into a manifest, trust record, log or error. If it is
//! piped from a file, that file must be readable only by its owner (mode
//! 0600 / owner-only ACL) and kept off shared or synced storage. A
//! package-publisher key must not be the command-signing key.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use kanade_shared::artifact::{ArtifactIdentity, ArtifactIntegrity, ArtifactSha256};
use kanade_shared::package_manifest::{
    ManifestClaims, PublisherSigningKey, TrustedPublisherKey, sign_manifest, verify_offline,
};
use sha2::{Digest, Sha256};

/// Identity / manifest / trust files are small JSON documents.
const MAX_INPUT_BYTES: u64 = 64 * 1024;

#[derive(Args, Debug)]
pub struct SignArgs {
    /// Explicit artifact identity JSON (same file format as `publish-artifact`).
    #[arg(long)]
    pub identity: PathBuf,
    /// Reverse-DNS style package id, e.g. `com.kanade.client`.
    #[arg(long)]
    pub package_id: String,
    /// Minimum OS version the package requires, e.g. `14.0`.
    #[arg(long)]
    pub min_os_version: String,
    /// Required agent capability; repeatable. Sorted by the CLI; duplicates rejected.
    #[arg(long = "capability")]
    pub capabilities: Vec<String>,
    /// Where to write the manifest. Never overwrites an existing file.
    #[arg(long)]
    pub out: PathBuf,
    /// Read the publisher private key from one stdin line instead of a hidden prompt.
    #[arg(long)]
    pub secret_stdin: bool,
    /// Local artifact file to describe.
    pub artifact: PathBuf,
}

#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// Trusted publisher key record obtained independently of the package
    /// (see `publisher-trust`). A key embedded in the manifest is never used.
    #[arg(long)]
    pub trusted_key: PathBuf,
    /// Identity you expect the manifest to bind (same file format as `publish-artifact`).
    #[arg(long)]
    pub identity: PathBuf,
    pub manifest: PathBuf,
    pub artifact: PathBuf,
}

#[derive(Args, Debug)]
pub struct TrustArgs {
    /// Read the publisher private key from one stdin line instead of a hidden prompt.
    #[arg(long)]
    pub secret_stdin: bool,
}

fn read_small(path: &Path) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("read {}", path.display()))?;
    if buf.len() as u64 > MAX_INPUT_BYTES {
        bail!("{} exceeds {MAX_INPUT_BYTES} bytes", path.display());
    }
    Ok(buf)
}

fn read_identity(path: &Path) -> Result<ArtifactIdentity> {
    serde_json::from_slice(&read_small(path)?).context("parse explicit artifact identity")
}

/// Stream the artifact once; the empty file is rejected like `publish-artifact`.
fn measure(path: &Path) -> Result<ArtifactIntegrity> {
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0; 128 * 1024];
    loop {
        let n = file.read(&mut buffer).context("read artifact")?;
        if n == 0 {
            break;
        }
        size = size
            .checked_add(n as u64)
            .context("artifact size overflow")?;
        hash.update(&buffer[..n]);
    }
    if size == 0 {
        bail!("artifact file is empty");
    }
    Ok(ArtifactIntegrity {
        sha256: ArtifactSha256::from_bytes(hash.finalize().into()),
        size_bytes: size,
    })
}

fn read_signing_key(from_stdin: bool) -> Result<PublisherSigningKey> {
    let mut secret =
        super::login::read_password("Publisher private key (input hidden): ", from_stdin)?
            .into_bytes();
    let parsed = std::str::from_utf8(&secret)
        .map_err(|_| anyhow::anyhow!("private key must be ASCII base64"))
        .and_then(|s| PublisherSigningKey::from_seed_base64(s).map_err(Into::into));
    // Best effort: the seed text should not linger in this buffer.
    secret.fill(0);
    parsed
}

pub fn sign(args: SignArgs) -> Result<()> {
    let identity = read_identity(&args.identity)?;
    let mut caps = args.capabilities;
    caps.sort();
    let integrity = measure(&args.artifact)?;
    let claims = ManifestClaims::new(
        identity,
        args.package_id,
        args.min_os_version,
        caps,
        integrity,
    )?;
    let key = read_signing_key(args.secret_stdin)?;
    write_signed(&args.out, claims, &key)
}

fn write_signed(out_path: &Path, claims: ManifestClaims, key: &PublisherSigningKey) -> Result<()> {
    let signed = sign_manifest(claims, key);
    let bytes = signed.to_canonical_bytes();

    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out_path)
        .with_context(|| format!("create {} (refusing to overwrite)", out_path.display()))?;
    if let Err(e) = out.write_all(&bytes).and_then(|()| out.sync_all()) {
        drop(out);
        let _ = std::fs::remove_file(out_path);
        return Err(e).with_context(|| format!("write {}", out_path.display()));
    }
    println!("Wrote signed manifest {}", out_path.display());
    println!("publisher key_id: {}", signed.key_id().as_str());
    println!(
        "NOTE: signing does not publish anything and does not make the artifact trusted. \
         Verifiers must hold this key's trust record independently (`app publisher-trust`)."
    );
    Ok(())
}

pub fn verify(args: VerifyArgs) -> Result<()> {
    let trusted = TrustedPublisherKey::from_trust_json(&read_small(&args.trusted_key)?)?;
    let expected = read_identity(&args.identity)?;
    let manifest = read_small(&args.manifest)?;
    let observed = measure(&args.artifact)?;
    let ok = verify_offline(&manifest, &trusted, &expected, &observed)
        .context("offline manifest verification FAILED")?;
    let c = ok.claims();
    println!("OFFLINE VERIFIED (signature + integrity only)");
    println!("publisher key_id: {}", ok.key_id().as_str());
    println!("package_id:       {}", c.package_id());
    println!("min_os_version:   {}", c.min_os_version());
    println!(
        "capabilities:     {}",
        c.required_agent_capabilities().join(", ")
    );
    println!("size_bytes:       {}", c.integrity().size_bytes);
    println!(
        "NOT CHECKED: current publisher authorization, freshness, revocation. \
         This result is not deploy-ready status and grants no install privilege."
    );
    Ok(())
}

pub fn publisher_trust(args: TrustArgs) -> Result<()> {
    let key = read_signing_key(args.secret_stdin)?;
    std::io::stdout().write_all(&key.trusted_key().to_trust_json())?;
    Ok(())
}
