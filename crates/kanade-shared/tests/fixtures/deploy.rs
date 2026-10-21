use kanade_shared::{
    artifact::{ArtifactIntegrity, ArtifactSha256},
    manifest::Manifest,
    typed_operation::ExpectedArtifact,
    wire::{TypedCommand, TypedCommandRequest},
};
const DEFINITION: &str = include_str!("typed-deploy.json");
pub fn command() -> TypedCommand {
    let m: Manifest = serde_json::from_str(DEFINITION).unwrap();
    let kanade_shared::typed_operation::ManifestExecute::Typed(t) = &m.execute else {
        panic!()
    };
    TypedCommand::from_manifest(
        &m,
        TypedCommandRequest {
            request_id: "r".into(),
            exec_id: None,
            deadline_at: None,
            job_manifest_digest: ArtifactSha256::of(DEFINITION.as_bytes()),
            expected: ExpectedArtifact {
                identity: t.operation.artifact_identity(),
                integrity: &ArtifactIntegrity::of(b"backend package bytes"),
                manifest_sha256: t.operation.manifest_sha256(),
            },
        },
    )
    .unwrap()
}
