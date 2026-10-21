use kanade_shared::artifact::ArtifactIntegrity;
use kanade_shared::{
    manifest::Manifest,
    typed_operation::ExpectedArtifact,
    wire::{Command, CommandEnvelope, TypedCommand},
};
use serde_json::{Value, json};
const DEFINITION: &str = include_str!("fixtures/typed-deploy.json");
fn fixture() -> Value {
    serde_json::from_str(DEFINITION).unwrap()
}
mod deploy_fixture {
    include!("fixtures/deploy.rs");
}
use deploy_fixture::command;

#[test]
fn shared_fixture_round_trips_and_never_enters_shell_decoder() {
    let m: Manifest = serde_json::from_str(DEFINITION).unwrap();
    m.validate().unwrap();
    assert!(m.execute.shell().is_none());
    let c = command();
    let bytes = serde_json::to_vec(&c).unwrap();
    assert_eq!(serde_json::from_slice::<TypedCommand>(&bytes).unwrap(), c);
    assert!(serde_json::from_slice::<Command>(&bytes).is_err());
}
#[test]
fn malformed_binding_fields_and_versions_fail_closed() {
    for (pointer, values) in [
        (
            "/execute/payload_version",
            vec![Value::Null, json!(0), json!(2), json!("1"), json!(1.0)],
        ),
        (
            "/execute/operation/spec/artifact_size_bytes",
            vec![json!(0), json!(-1), Value::Null],
        ),
        (
            "/execute/operation/spec/package_id",
            vec![json!("../pkg"), json!(""), Value::Null],
        ),
        (
            "/execute/operation/spec/manifest_sha256",
            vec![json!("AB"), Value::Null],
        ),
        ("/execute/kind", vec![json!("future"), Value::Null]),
    ] {
        for value in values {
            let mut raw = fixture();
            *raw.pointer_mut(pointer).unwrap() = value;
            assert!(
                match serde_json::from_value::<Manifest>(raw) {
                    Err(_) => true,
                    Ok(m) => m.validate().is_err(),
                },
                "{pointer}"
            );
        }
    }
}
#[test]
fn repeated_keys_fail_even_in_unknown_fields_and_nested_buffers() {
    for (from, to) in [
        ("\"id\":", "\"future\":1,\"future\":2,\"id\":"),
        (
            "\"preserve_config\": true",
            "\"preserve_config\":false,\"preserve_config\":true",
        ),
        (
            "\"type\": \"deploy_backend\"",
            "\"type\":\"unknown\",\"type\":\"deploy_backend\"",
        ),
    ] {
        let raw = DEFINITION.replace(from, to);
        assert_ne!(raw, DEFINITION);
        assert!(serde_json::from_str::<Manifest>(&raw).is_err());
    }
    let shell = r#"{"id":"j","version":"1","request_id":"r","shell":"sh","script":"x","timeout_secs":5,"future":1,"future":2}"#;
    assert!(serde_json::from_str::<Command>(shell).is_err());
    let envelope = format!(
        r#"{{"kind":"kanade.command.v2","target_pc_id":"PC","expires_at":"2026-10-22T00:00:00Z","command":{shell}}}"#
    );
    assert!(serde_json::from_str::<CommandEnvelope>(&envelope).is_err());
    let yaml = "id: j\nversion: 1\nexecute:\n  shell: sh\n  script: x\n  timeout: 5s\nfuture: 1\nfuture: 2\n";
    assert!(serde_yaml::from_str::<Manifest>(yaml).is_err());
}
#[test]
fn size_only_mismatch_fails_conversion() {
    let m: Manifest = serde_json::from_str(DEFINITION).unwrap();
    let kanade_shared::typed_operation::ManifestExecute::Typed(t) = &m.execute else {
        panic!()
    };
    let mut integrity = ArtifactIntegrity::of(b"backend package bytes");
    integrity.size_bytes += 1;
    assert!(
        t.operation
            .verify_binding(&ExpectedArtifact {
                identity: t.operation.artifact_identity(),
                integrity: &integrity,
                manifest_sha256: t.operation.manifest_sha256()
            })
            .is_err()
    );
}

#[test]
fn signed_duplicate_envelopes_are_rejected_after_valid_signature_check() {
    use kanade_shared::{
        signing::{KeyPolicy, KeyRing, Signer, generate_keypair},
        wire::{sign_typed_envelope, verify_typed_envelope},
    };
    let signer = Signer::new(generate_keypair().unwrap(), "backend-1");
    let mut ring = KeyRing::new();
    ring.insert(
        signer.kid(),
        signer.verifying_key(),
        KeyPolicy::backend("backend"),
    );
    let now = chrono::Utc::now();
    let (body, headers) = sign_typed_envelope(
        &signer,
        "PC1",
        now + chrono::Duration::minutes(10),
        command(),
        now,
    )
    .unwrap();
    verify_typed_envelope(&ring, &body, &headers, "PC1", now).unwrap();
    let text = String::from_utf8(body).unwrap();
    for (from, to) in [
        (
            "\"target_pc_id\":\"PC1\"",
            "\"target_pc_id\":\"other\",\"target_pc_id\":\"PC1\"",
        ),
        (
            "\"command_kind\":\"typed_operation\"",
            "\"command_kind\":\"shell\",\"command_kind\":\"typed_operation\"",
        ),
        (
            "\"artifact_size_bytes\":21",
            "\"artifact_size_bytes\":22,\"artifact_size_bytes\":21",
        ),
    ] {
        let duplicate = text.replace(from, to);
        assert_ne!(duplicate, text);
        let headers = signer.headers(duplicate.as_bytes(), now.timestamp_millis());
        assert!(verify_typed_envelope(&ring, duplicate.as_bytes(), &headers, "PC1", now).is_err());
    }
}

#[test]
fn historical_shell_defaults_aliases_and_exact_serialization_survive_strict_maps() {
    let old = r#"{"id":"echo","version":"1","request_id":"r","job_id":"execution","shell":"sh","script":"echo hi","timeout_secs":30,"future":true}"#;
    let c: Command = serde_json::from_str(old).unwrap();
    c.ensure_execution_enabled().unwrap();
    assert_eq!(c.exec_id.as_deref(), Some("execution"));
    let golden = r#"{"id":"echo","version":"1","request_id":"r","exec_id":"execution","shell":"sh","script":"echo hi","timeout_secs":30,"jitter_secs":null,"run_as":"system","staleness":{"mode":"cached"}}"#;
    assert_eq!(serde_json::to_string(&c).unwrap(), golden);
    let yaml =
        "id: echo\nversion: 1\nexecute:\n  shell: sh\n  script: x\n  timeout: 5s\n  future: true\n";
    let m: Manifest = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(m.version, "1");
    m.validate().unwrap();
    assert!(kanade_shared::strict::from_yaml_str::<Manifest>(yaml).is_err());
}
